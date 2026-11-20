//! The plugin's state across calls, and one handler per call: the thin layer between afkd's
//! wire ([`crate::wire`]) and the `intake` kind's vendor half ([`crate::issue`]).
//!
//! **afkd runs the slots.** `on_claim` after a `poll` hands a unit over, and `on_done`,
//! `on_park` or `on_fail` after its `finish`; each is passed the `linear.Workspace` the
//! service's credential settings build, beside the issue, and each action a slot calls is
//! one `call`. A finished unit's issue is kept a while ([`FINISHED_MAX`]), since its post-run
//! slot's calls arrive after `finish` and may pass it.
//!
//! **Every action reaches a workspace, not the service** ([`crate::reads`],
//! [`crate::writes`]). It comes as a `call`: to the trigger child from a `service(linear)`'s
//! slot, which may pass it the issue the service claimed, and from anywhere else to a child
//! afkd starts for that one call, which gets no `hello` and reads its `proto` and the
//! plugin's directory off the call itself.
//!
//! What the wire forces, as for `@afkd/trello`:
//!
//! - **Who is asking arrives in `hello`**, and a `hello` without it is refused, since the
//!   park owner cannot work without a service name.
//! - **Every reply fits one line.** afkd caps a line at 64 KiB, and a call's reply at 8 MiB.
//!   A `poll` whose issue's description and brief would overflow it is cut to fit
//!   ([`fit_poll`]); a `comments` reply sends only what afkd has not been told yet, and if
//!   that still overflows, the newest that fit.
//! - **An undelivered finish is `held`**: `finish` answers `{"ok":true,"held":true}` and
//!   afkd asks `release` for the key on a later beat, which replays what is owed.
//! - **The lines go to stderr**, which afkd files under `[@afkd/linear:err]`.
//! - **Every call answers within 45 seconds**: each armed call runs under [`CALL_BUDGET`],
//!   and a Linear too slow for it reads as a Linear that is down.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use serde_json::{json, Map, Value};

use crate::auth::Auth;
use crate::client::LinearClient;
use crate::common::{file_safe, Clock, Diag, CALL_BUDGET};
use crate::issue::{is_afkd, Identity, LinearUnits, Unit};
use crate::reads::{self, Claimed, ReadContext, READS};
use crate::rfc3339::format_utc;
use crate::settings::{intake_config, IntakeConfig};
use crate::tracker::{Issue, Tracker};
use crate::wire::{
    fit_comments, fit_poll, Facts, Fitted, Request, UnitOutcome, WireComment, PROTO,
};
use crate::writes::{self, WRITES};

/// The one kind this plugin provides, its main one: `service(linear)` in a config.
pub(crate) const LINEAR_KIND: &str = "intake";

/// How many finished units' issues a post-run slot's `call`s can still reach. afkd sends
/// them straight after `finish`, one unit at a time, so a handful would do; the rest is
/// headroom, and the oldest goes first.
const FINISHED_MAX: usize = 16;

/// The optional calls the kind answers, exactly as `hello` lists them: every one afkd has.
const CALLS: &[&str] = &["release", "renew", "comments", "attempt_failed", "classify"];

/// The sentence the process ends on when a request that is no call for a child of its own
/// comes before a `hello` this plugin accepted: afkd is out of step.
const BEFORE_HELLO: &str = "afkd sent a call before a `hello` this plugin accepted";

/// What the process does with one request.
#[derive(Debug, PartialEq)]
pub(crate) enum Answer {
    /// Write this line to stdout.
    Reply(String),
    /// Write this sentence to stderr and exit non-zero: afkd faults the service, and the
    /// sentence is the last thing in its log.
    Fatal(String),
}

/// Builds the tracker a `hello` arms with — the real client in the running plugin, a mock
/// in the tests — over the service's credential.
pub(crate) type Connect = Box<dyn Fn(&IntakeConfig, Arc<Auth>) -> Box<dyn Tracker>>;

/// The plugin across its whole life: unarmed until `hello`, then armed.
pub(crate) struct Plugin {
    connect: Connect,
    clock: Box<dyn Clock>,
    diag: Box<dyn Diag>,
    armed: Option<Armed>,
}

/// The armed service: the kind's vendor half, and the units afkd is running.
struct Armed {
    units: LinearUnits,
    /// The plugin's own directory, as `hello` named it.
    state_dir: Option<PathBuf>,
    /// The units handed over and not yet finished or released, by key.
    live: BTreeMap<String, LiveUnit>,
    /// The issues of the units most recently finished, by key and oldest first, for the
    /// post-run slot's `call`s; at most [`FINISHED_MAX`].
    finished: VecDeque<(String, Issue)>,
}

/// A unit afkd is running.
struct LiveUnit {
    unit: Unit,
    /// The comment ids afkd has been told about: the unit's `seen`, then everything a
    /// `comments` reply carried or left out.
    reported: HashSet<String>,
}

impl Plugin {
    /// A plugin that arms against the real Linear, at the block's `base_url`.
    pub(crate) fn new(clock: Box<dyn Clock>, diag: Box<dyn Diag>) -> Self {
        let connect: Connect = Box::new(|cfg, auth| {
            Box::new(LinearClient::new(&cfg.base_url, auth)) as Box<dyn Tracker>
        });
        Self::with_connect(connect, clock, diag)
    }

    /// A plugin that arms through `connect`.
    pub(crate) fn with_connect(
        connect: Connect,
        clock: Box<dyn Clock>,
        diag: Box<dyn Diag>,
    ) -> Self {
        Self {
            connect,
            clock,
            diag,
            armed: None,
        }
    }

    /// Answer one request.
    pub(crate) fn answer(&mut self, request: Request) -> Answer {
        let (clock, diag, armed) = (&*self.clock, &*self.diag, &mut self.armed);
        match request {
            Request::Hello {
                proto,
                kind,
                settings,
                service,
                roster,
                owner,
                state_dir,
            } => {
                let id = Identity {
                    owner,
                    service,
                    roster,
                };
                let state_dir = state_dir.map(PathBuf::from);
                *armed = arm(&self.connect, proto, &kind, &settings, id, state_dir)
                    .map_err(|problem| diag.err(&problem))
                    .ok();
                Answer::Reply(
                    match armed {
                        Some(_) => json!({"ok": true, "proto": PROTO, "calls": CALLS}),
                        None => json!({"ok": false, "proto": PROTO}),
                    }
                    .to_string(),
                )
            }
            Request::Poll => on_armed(armed, clock, |a| a.poll(clock, diag)),
            Request::Release { key, handed_back } => on_armed(armed, clock, |a| {
                a.release(&key, handed_back, clock.now(), diag)
            }),
            Request::Renew { key, renewal } => {
                on_armed(armed, clock, |a| a.renew(&key, renewal, diag))
            }
            Request::Comments { key } => on_armed(armed, clock, |a| a.comments(&key, diag)),
            Request::Classify { scratch, outcome } => on_armed(armed, clock, |_| {
                let outcome = LinearUnits::classify(Path::new(&scratch), outcome);
                Answer::Reply(json!({ "outcome": outcome }).to_string())
            }),
            Request::AttemptFailed {
                key,
                n,
                max,
                reason,
            } => on_armed(armed, clock, |a| {
                a.attempt_failed(&key, n, max, reason.as_deref(), diag)
            }),
            Request::Finish {
                key,
                outcome,
                facts,
            } => on_armed(armed, clock, |a| {
                a.finish(&key, outcome, &facts, clock, diag)
            }),
            Request::Call {
                action,
                args,
                proto,
                state_dir,
            } => match armed {
                Some(_) => on_armed(armed, clock, |a| a.call(&action, &args, clock, diag)),
                None => once(&action, &args, proto, state_dir.as_deref(), clock, diag),
            },
            Request::Unknown => {
                diag.err(&"afkd sent a call this plugin did not list in its `hello` reply");
                Answer::Reply(json!({"ok": false}).to_string())
            }
        }
    }
}

/// Run `f` on the armed service under the call's [`CALL_BUDGET`], cleared once the reply
/// is built so no call inherits another's deadline — or end the process, since afkd sends
/// nothing but `hello` before a `hello` it has seen accepted.
fn on_armed(
    armed: &mut Option<Armed>,
    clock: &dyn Clock,
    f: impl FnOnce(&mut Armed) -> Answer,
) -> Answer {
    match armed {
        Some(armed) => {
            armed
                .units
                .set_call_deadline(Some(clock.now() + CALL_BUDGET));
            let answer = f(armed);
            armed.units.set_call_deadline(None);
            answer
        }
        None => Answer::Fatal(BEFORE_HELLO.into()),
    }
}

/// Answer the one `call` a child afkd started for it gets, with no `hello` before it: a read or
/// a write, with the plugin's directory the call names and the call's own budget. An action no
/// one declared is refused, as is a protocol this plugin does not speak. A call with no `proto`
/// at all is no such call: afkd is out of step, as for any request before a `hello`.
fn once(
    action: &str,
    args: &Map<String, Value>,
    proto: Option<u32>,
    state_dir: Option<&str>,
    clock: &dyn Clock,
    diag: &dyn Diag,
) -> Answer {
    let Some(proto) = proto else {
        return Answer::Fatal(BEFORE_HELLO.into());
    };
    let refuse = |error: String| {
        diag.err(&error);
        Answer::Reply(json!({"ok": false, "error": error}).to_string())
    };
    if proto != PROTO {
        return refuse(format!(
            "afkd speaks plugin protocol {proto}, and this plugin speaks {PROTO}"
        ));
    }
    let ctx = ReadContext {
        state_dir: state_dir.map(Path::new),
        deadline: clock.now() + CALL_BUDGET,
        claimed: &|_| None,
    };
    answer_action(action, args, &ctx, diag).unwrap_or_else(|| refuse(no_action(action)))
}

/// The sentence refusing an action the manifest does not declare.
fn no_action(action: &str) -> String {
    format!("no action `{action}`")
}

/// The reply to the read or the write `action` with `args`, in `ctx`, or `None` for an
/// action that is neither.
fn answer_action(
    action: &str,
    args: &Map<String, Value>,
    ctx: &ReadContext<'_>,
    diag: &dyn Diag,
) -> Option<Answer> {
    if READS.contains(&action) {
        Some(Answer::Reply(reads::answer(action, args, ctx, diag)))
    } else if WRITES.contains(&action) {
        Some(Answer::Reply(writes::answer(action, args, ctx, diag)))
    } else {
        None
    }
}

/// Arm the kind `hello` names over its settings, as `id`, keeping the plugin's directory
/// `state_dir`, or say why not. afkd has already held the block to the manifest, so what can
/// be wrong here is the protocol, the kind, a missing identity, or a rule the manifest cannot
/// express. Nothing is sent to Linear: an outage when the service arms is a poll's idle beat,
/// not a refused service.
fn arm(
    connect: &Connect,
    proto: u32,
    kind: &str,
    settings: &serde_json::Value,
    id: Identity,
    state_dir: Option<PathBuf>,
) -> Result<Armed, String> {
    if proto != PROTO {
        return Err(format!(
            "afkd speaks plugin protocol {proto}, and this plugin speaks {PROTO}"
        ));
    }
    if kind != LINEAR_KIND {
        return Err(format!("kind `{kind}` is not provided by @afkd/linear"));
    }
    if id.service.is_empty() || id.owner.is_empty() {
        return Err(
            "afkd did not say which service this is; @afkd/linear needs an afkd whose `hello` \
             carries `service`, `roster` and `owner`"
                .to_string(),
        );
    }
    let cfg = intake_config(settings).map_err(|e| format!("trigger {kind}: {e}"))?;
    let auth = Arc::new(Auth::new(&cfg.auth));
    Ok(Armed {
        units: LinearUnits::new(connect(&cfg, auth), &cfg, id),
        state_dir,
        live: BTreeMap::new(),
        finished: VecDeque::new(),
    })
}

/// The `poll` reply that hands nothing over.
fn idle() -> Answer {
    Answer::Reply(json!({"fire": false}).to_string())
}

/// Where the files the unit under `key` links are downloaded: `<state_dir>/task-files/`
/// and the key written as one file name ([`file_safe`]). Afkd copies them in from there on
/// each attempt, so they stay until the unit is finished or released, and a child restarted
/// in between finds the folder by the key alone.
pub(crate) fn task_files_dir(state_dir: &Path, key: &str) -> PathBuf {
    state_dir.join("task-files").join(file_safe(key))
}

impl Armed {
    /// Where the files of the unit under `key` are downloaded, when the plugin has a
    /// directory of its own.
    fn task_files_dir(&self, key: &str) -> Option<PathBuf> {
        self.state_dir
            .as_deref()
            .map(|state_dir| task_files_dir(state_dir, key))
    }

    /// Delete the folder of the files the unit under `key` was handed over with, if there is
    /// one. One that cannot be deleted is said and left.
    fn remove_task_files(&self, key: &str, diag: &dyn Diag) {
        let Some(dir) = self.task_files_dir(key) else {
            return;
        };
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => diag.err(&format_args!("cannot delete {}: {e}", dir.display())),
        }
    }

    /// One beat: the parked sweep, then the claim race. A tracker failure is an idle beat,
    /// diagnosed.
    fn poll(&mut self, clock: &dyn Clock, diag: &dyn Diag) -> Answer {
        let unit = match self.units.try_claim_next(diag, clock) {
            Ok(Some(unit)) => unit,
            Ok(None) => return idle(),
            Err(e) => {
                diag.err(&e);
                return idle();
            }
        };
        // The files are downloaded once the claim is won, under the plugin's own directory,
        // which outlives this child; with none, every link stays Linear's.
        let fetched = match self.task_files_dir(&unit.key()) {
            Some(dir) => self.units.fetch_files(&unit, &dir, diag),
            None => Vec::new(),
        };
        let wire = self.units.wire_unit(&unit, &fetched);
        let line = if let Some(Fitted {
            line,
            description_cut,
            brief_cut,
        }) = fit_poll(&wire)
        {
            if description_cut {
                diag.err(&format_args!(
                    "the description field of {} was cut to fit afkd's 64 KiB plugin line",
                    unit.identifier()
                ));
            }
            if brief_cut {
                diag.err(&format_args!(
                    "the brief for {} was cut to fit afkd's 64 KiB plugin line",
                    unit.identifier()
                ));
            }
            line
        } else {
            // Only a unit whose fields besides the brief and the description overflow the
            // line reaches this: nothing can be handed over, so the claim is taken back
            // before the service ends.
            self.units.release_stale(&wire.key, clock.now(), diag);
            self.remove_task_files(&wire.key, diag);
            return Answer::Fatal(format!(
                "{} cannot be handed to afkd: even with its description and brief cut away, \
                 the unit is over afkd's 64 KiB plugin line",
                unit.identifier()
            ));
        };
        let reported = wire.seen.iter().cloned().collect();
        self.live.insert(wire.key, LiveUnit { unit, reported });
        Answer::Reply(line)
    }

    /// Release a journal key — a crashed run's, a `held` finish's, a unit afkd handed
    /// straight back, or one whose `on_claim` failed — and forget it.
    ///
    /// A unit afkd marks `handed_back` while this plugin still holds it live keeps its place:
    /// nothing ran on it, so only its claim is pruned. Every other key goes through
    /// [`LinearUnits::release_stale`], to the bottom of its state.
    fn release(&mut self, key: &str, handed_back: bool, now: Instant, diag: &dyn Diag) -> Answer {
        let live = self.live.remove(key).is_some();
        self.finished.retain(|(finished, _)| finished != key);
        self.remove_task_files(key, diag);
        let released = if handed_back && live {
            self.units.hand_back(key, diag)
        } else {
            self.units.release_stale(key, now, diag)
        };
        Answer::Reply(json!({ "released": released }).to_string())
    }

    /// Renew a live unit's claim marker.
    fn renew(&self, key: &str, renewal: u64, diag: &dyn Diag) -> Answer {
        let ok = match self.live.get(key) {
            Some(live) => {
                self.units.renew(&live.unit, renewal, diag);
                true
            }
            None => {
                diag.err(&format_args!(
                    "renew for {key}, which this plugin holds no claim on"
                ));
                false
            }
        };
        Answer::Reply(json!({ "ok": ok }).to_string())
    }

    /// Report a live unit's comments afkd has not been told about yet, oldest-first.
    ///
    /// afkd's watch drops the ids in its cursor and afkd's own comments anyway, so leaving
    /// out those — and every control marker, which only afkd writes — changes no delivery;
    /// it is what keeps a long thread inside one line. `null` when Linear could not be read.
    fn comments(&mut self, key: &str, diag: &dyn Diag) -> Answer {
        let null = || Answer::Reply(json!({ "comments": null }).to_string());
        let Some(live) = self.live.get_mut(key) else {
            diag.err(&format_args!(
                "comments for {key}, which this plugin holds no claim on"
            ));
            return null();
        };
        let all = match self.units.comments(&live.unit) {
            Ok(all) => all,
            Err(e) => {
                diag.err(&e);
                return null();
            }
        };
        let me = live.unit.me();
        let mut fresh: Vec<_> = all
            .into_iter()
            .filter(|c| !live.reported.contains(&c.id) && !is_afkd(c, me))
            .collect();
        fresh.sort_by(|a, b| (a.posted_at, &a.id).cmp(&(b.posted_at, &b.id)));
        let fresh: Vec<WireComment> = fresh
            .into_iter()
            .map(|c| WireComment {
                id: c.id,
                author: c.author,
                author_name: c.author_name,
                body: c.text,
                at: format_utc(c.posted_at),
            })
            .collect();
        let (line, dropped) = fit_comments(&fresh);
        if dropped > 0 {
            let ids: Vec<&str> = fresh[..dropped].iter().map(|c| c.id.as_str()).collect();
            diag.err(&format_args!(
                "{} comments on {} did not fit afkd's 64 KiB plugin line and were not \
                 delivered: {}",
                dropped,
                live.unit.identifier(),
                ids.join(", ")
            ));
        }
        live.reported.extend(fresh.into_iter().map(|c| c.id));
        Answer::Reply(line)
    }

    /// Mark a live unit's faulted attempt on its issue.
    fn attempt_failed(
        &self,
        key: &str,
        n: u32,
        max: u32,
        reason: Option<&str>,
        diag: &dyn Diag,
    ) -> Answer {
        let ok = match self.live.get(key) {
            Some(live) => {
                self.units.attempt_failed(&live.unit, n, max, reason, diag);
                true
            }
            None => {
                diag.err(&format_args!(
                    "attempt_failed for {key}, which this plugin holds no claim on"
                ));
                false
            }
        };
        Answer::Reply(json!({ "ok": ok }).to_string())
    }

    /// Finish a unit, and keep its issue for the post-run slot's `call`s. A finish that did
    /// not land is `held`: afkd keeps the claim and sends `release` for the key on a later
    /// beat, which replays what is owed.
    fn finish(
        &mut self,
        key: &str,
        outcome: UnitOutcome,
        facts: &Facts,
        clock: &dyn Clock,
        diag: &dyn Diag,
    ) -> Answer {
        // The run is over, and afkd copies no more of its files in.
        self.remove_task_files(key, diag);
        let Some(live) = self.live.remove(key) else {
            diag.err(&format_args!(
                "finish for {key}, which this plugin holds no claim on"
            ));
            return Answer::Reply(json!({"ok": true}).to_string());
        };
        let reply = if self.units.finish(&live.unit, outcome, facts, clock, diag) {
            json!({"ok": true})
        } else {
            diag.err(&format_args!(
                "could not deliver the finish for {key}; afkd holds the claim and releases it \
                 on a later beat"
            ));
            json!({"ok": true, "held": true})
        };
        if self.finished.len() == FINISHED_MAX {
            self.finished.pop_front();
        }
        self.finished
            .push_back((key.to_string(), live.unit.issue().clone()));
        Answer::Reply(reply.to_string())
    }

    /// The issue a unit this plugin handed over under `key` holds, live or finished since,
    /// as a read passed its handle needs it.
    fn claimed(&self, key: &str) -> Option<Claimed> {
        let issue = match self.live.get(key) {
            Some(live) => live.unit.issue(),
            None => &self.finished.iter().find(|(held, _)| held == key)?.1,
        };
        Some(Claimed {
            uuid: issue.id.clone(),
            organization: self.units.organization()?,
        })
    }

    /// Do one read or write a slot called, through the workspace it is passed, the issues
    /// this service claimed — live, or finished since — among what it may be passed.
    /// `{"ok":false}` with the sentence, diagnosed too, for an action the manifest does not
    /// declare.
    fn call(
        &self,
        action: &str,
        args: &Map<String, Value>,
        clock: &dyn Clock,
        diag: &dyn Diag,
    ) -> Answer {
        let ctx = ReadContext {
            state_dir: self.state_dir.as_deref(),
            deadline: self
                .units
                .call_deadline()
                .unwrap_or_else(|| clock.now() + CALL_BUDGET),
            claimed: &|key| self.claimed(key),
        };
        answer_action(action, args, &ctx, diag).unwrap_or_else(|| {
            let error = no_action(action);
            diag.err(&error);
            Answer::Reply(json!({"ok": false, "error": error}).to_string())
        })
    }
}

#[cfg(test)]
mod tests {
    //! The handlers over a mock tracker: what each call answers, and the state the plugin
    //! keeps between them. The wire itself — a real child, real JSON lines, a real HTTP
    //! Linear — is `tests/wire.rs`.

    use super::*;
    use crate::claim::{claim_text, is_claim};
    use crate::common::{CaptureDiag, FakeClock, TempDir};
    use crate::issue::RETRY_FOR;
    use crate::tracker::{Action, LinearError, MockTracker, MOCK_UPLOADS, SELF_ID};
    use crate::wire::MAX_REPLY;
    use serde_json::Value;
    use std::time::{Duration, SystemTime};

    /// A [`Clock`] the test keeps a handle on after the plugin owns it.
    struct SharedClock(Arc<FakeClock>);

    impl Clock for SharedClock {
        fn sleep(&self, d: std::time::Duration) {
            self.0.sleep(d);
        }

        fn now(&self) -> Instant {
            self.0.now()
        }
    }

    /// A [`Diag`] the test keeps a handle on after the plugin owns it.
    struct Shared(Arc<CaptureDiag>);

    impl Diag for Shared {
        fn err(&self, err: &dyn std::fmt::Display) {
            self.0.err(err);
        }

        fn narrate(&self, line: &str) {
            self.0.narrate(line);
        }
    }

    struct Fixture {
        plugin: Plugin,
        tracker: Arc<MockTracker>,
        diag: Arc<CaptureDiag>,
        clock: Arc<FakeClock>,
    }

    const SERVICE: &str = "afkd::develop";

    /// The polled issue's title: the delimiter a success line wraps it in, wide CJK, an
    /// emoji and an em dash.
    const TITLE: &str = "Fix \"the\" café — 修复 🚨";

    impl Fixture {
        /// A plugin over a mock tracker holding `ENG-1` in `Todo`, not yet armed.
        fn new() -> Self {
            let tracker = Arc::new(MockTracker::new());
            tracker.add_state("Todo", "unstarted");
            tracker.add_state("In Progress", "started");
            tracker.add_issue("Todo", "ENG-1", TITLE, "Body");
            let now = SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();
            tracker.set_clock(now - 600);
            let diag = Arc::new(CaptureDiag::default());
            let clock = Arc::new(FakeClock::new());
            let shared = Arc::clone(&tracker);
            let connect: Connect =
                Box::new(move |_, _| Box::new(Arc::clone(&shared)) as Box<dyn Tracker>);
            let plugin = Plugin::with_connect(
                connect,
                Box::new(SharedClock(Arc::clone(&clock))),
                Box::new(Shared(Arc::clone(&diag))),
            );
            Self {
                plugin,
                tracker,
                diag,
                clock,
            }
        }

        /// The same, armed as the `intake` kind over `settings`.
        fn armed(settings: Value) -> Self {
            let mut f = Self::new();
            let hello = f.call(hello(settings));
            assert_eq!(hello["ok"], true, "{:?}", f.diag.errs());
            f
        }

        /// Send one request as afkd writes it, and read the reply line back as JSON.
        fn call(&mut self, request: Value) -> Value {
            let request = serde_json::from_value(request).expect("an afkd request");
            match self.plugin.answer(request) {
                Answer::Reply(line) => {
                    assert!(line.len() <= MAX_REPLY && !line.contains('\n'), "{line}");
                    serde_json::from_str(&line).expect("a JSON reply")
                }
                Answer::Fatal(reason) => panic!("unexpected fatal: {reason}"),
            }
        }

        fn poll(&mut self) -> Value {
            self.call(json!({"call": "poll"}))
        }

        fn finish(&mut self, key: &Value, outcome: &str) -> Value {
            self.call(
                json!({"call": "finish", "id": "ENG-1", "key": key, "outcome": outcome,
                             "facts": {"signal": "proceed", "reason": null, "duration_ms": 3,
                                       "cost": 0.0, "turns": null, "tokens": null,
                                       "run_name": "261027-100400-unit-ENG-1-1"}}),
            )
        }

        /// The issue a slot passing the handle keyed `key` reaches, as a read or a write
        /// resolves it.
        fn claimed(&self, key: &Value) -> Option<Claimed> {
            self.plugin
                .armed
                .as_ref()
                .expect("an armed plugin")
                .claimed(key.as_str().expect("a key"))
        }

        fn claims_on(&self, id: &str) -> usize {
            self.tracker
                .comments_on(id)
                .iter()
                .filter(|c| is_claim(&c.text))
                .count()
        }
    }

    /// The `hello` afkd writes, carrying `settings`.
    fn hello(settings: Value) -> Value {
        json!({"call": "hello", "proto": 2, "kind": LINEAR_KIND, "service": SERVICE,
               "roster": [SERVICE, "afkd::discuss"], "owner": "afkd-4242",
               "settings": settings})
    }

    fn settings() -> Value {
        json!({"team": ["ENG"], "api_key": "lin_api_k", "claim_from": ["Todo"]})
    }

    /// The accepted `hello` lists every optional call, and touches no tracker: an outage
    /// when the service arms is a poll's business.
    #[test]
    fn hello_lists_every_optional_call_and_reaches_no_tracker() {
        let mut f = Fixture::new();
        assert_eq!(
            f.call(hello(settings())),
            json!({"ok": true, "proto": 2,
                   "calls": ["release", "renew", "comments", "attempt_failed", "classify"]})
        );
        assert!(f.diag.errs().is_empty(), "{:?}", f.diag.errs());
        assert!(f.tracker.calls().is_empty());
    }

    /// Each refusal is `ok:false` with the problem on the diagnostic channel, and leaves the
    /// plugin unarmed.
    #[test]
    fn hello_refuses_what_the_kind_cannot_arm_with() {
        let with = |edit: &dyn Fn(&mut Value)| {
            let mut request = hello(settings());
            edit(&mut request);
            request
        };
        let identity = "afkd did not say which service this is; @afkd/linear needs an afkd \
                        whose `hello` carries `service`, `roster` and `owner`";
        for (request, problem) in [
            (
                with(&|r| {
                    for field in ["service", "roster", "owner"] {
                        r.as_object_mut().unwrap().remove(field);
                    }
                }),
                identity.to_string(),
            ),
            (with(&|r| r["owner"] = json!("")), identity.to_string()),
            (
                with(&|r| r["proto"] = json!(1)),
                "afkd speaks plugin protocol 1, and this plugin speaks 2".into(),
            ),
            (
                with(&|r| r["kind"] = json!("card")),
                "kind `card` is not provided by @afkd/linear".into(),
            ),
            (
                with(&|r| {
                    r["settings"]["client_id"] = json!("app-id");
                }),
                "trigger intake: setting `api_key`: write api_key or client_id and \
                 client_secret, not both"
                    .into(),
            ),
            (
                with(&|r| {
                    r["settings"].as_object_mut().unwrap().remove("api_key");
                }),
                "trigger intake: setting `api_key`: needs api_key, or client_id and \
                 client_secret"
                    .into(),
            ),
            (
                with(&|r| {
                    let s = r["settings"].as_object_mut().unwrap();
                    s.remove("api_key");
                    s.insert("client_secret".into(), json!("s"));
                }),
                "trigger intake: setting `client_secret`: client_secret needs client_id".into(),
            ),
            (
                with(&|r| {
                    r["settings"].as_object_mut().unwrap().remove("team");
                }),
                "trigger intake: setting `team`: `team` is required: name each team by its key"
                    .into(),
            ),
            (
                with(&|r| {
                    r["settings"].as_object_mut().unwrap().remove("claim_from");
                }),
                "trigger intake: setting `claim_from`: `claim_from` is required: name the \
                 workflow state or states new issues are claimed from"
                    .into(),
            ),
            (
                with(&|r| r["settings"]["only_cycle"] = json!(["zwölf"])),
                "trigger intake: setting `only_cycle`: `only_cycle` expects cycle numbers, not \
                 \"zwölf\""
                    .into(),
            ),
        ] {
            let mut f = Fixture::new();
            assert_eq!(f.call(request), json!({"ok": false, "proto": 2}));
            assert_eq!(f.diag.errs(), [problem]);
            assert!(matches!(
                f.plugin.answer(Request::Poll),
                Answer::Fatal(reason) if reason.contains("before a `hello`")
            ));
        }
    }

    /// A `call` the plugin cannot do answers `ok:false` with its sentence — which afkd fails
    /// the slot's call with — and says it on the diagnostic channel too: an action it does
    /// not declare, and a read or a write whose workspace is no `linear.Workspace`, refused
    /// before any request. No tracker is reached, and the next request is still answered.
    #[test]
    fn a_call_the_plugin_cannot_do_answers_its_sentence() {
        let mut f = Fixture::armed(settings());
        let key = f.poll()["unit"]["key"].clone();
        let calls = f.tracker.calls().len();
        let issue = json!({"id": "ENG-1", "key": key});
        let mut expect = Vec::new();
        for (action, args, error) in [
            (
                "assign",
                json!({"issue": issue, "who": "me"}),
                "no action `assign`",
            ),
            (
                "update_issue",
                json!({"ws": "lin_api_k", "issue": issue, "title": "修复 🚀"}),
                "update_issue: parameter ws must be a linear.Workspace",
            ),
            (
                "comments",
                json!({"ws": {"api_key": "", "base_url": "http://127.0.0.1:9"}, "issue": issue}),
                "comments: the linear.Workspace sets no credential; set api_key, or client_id \
                 and client_secret",
            ),
        ] {
            assert_eq!(
                f.call(json!({"call": "call", "action": action, "args": args})),
                json!({"ok": false, "error": error})
            );
            expect.push(error);
        }
        assert_eq!(f.diag.errs(), expect);
        assert_eq!(f.tracker.calls().len(), calls, "no tracker is reached");
        assert_eq!(
            f.call(json!({"call": "renew", "key": key, "renewal": 1})),
            json!({"ok": true})
        );
    }

    /// The unit a poll hands over passes every slot the `linear.Workspace` the service's
    /// credential settings build, in its `args`: an API key's, or an app's.
    #[test]
    fn a_unit_passes_every_slot_the_services_workspace() {
        let mut f = Fixture::armed(settings());
        assert_eq!(
            f.poll()["unit"]["args"],
            json!({"workspace": {"api_key": "lin_api_k", "client_id": null,
                                 "client_secret": null, "base_url": "https://api.linear.app"}})
        );
        let mut f = Fixture::armed(json!({
            "team": ["ENG"], "claim_from": ["Todo"], "api_key": "",
            "client_id": "app-id-7f3a", "client_secret": "app-secret/&=",
            "base_url": "http://127.0.0.1:9",
        }));
        assert_eq!(
            f.poll()["unit"]["args"],
            json!({"workspace": {"api_key": null, "client_id": "app-id-7f3a",
                                 "client_secret": "app-secret/&=",
                                 "base_url": "http://127.0.0.1:9"}})
        );
    }

    /// An armed fixture whose issue `ENG-1` links the file at `url` (holding `bytes`) from its
    /// description, in the plugin directory `state_dir` when there is one.
    fn armed_with_file(state_dir: Option<&Path>, url: &str, bytes: &[u8]) -> Fixture {
        let mut f = Fixture::new();
        f.tracker.seed_file(url, bytes);
        f.tracker.edit_issue("ENG-1", |i| {
            i.description = format!("Body ![shot.png]({url})")
        });
        let mut request = hello(settings());
        if let Some(dir) = state_dir {
            request["state_dir"] = json!(dir.to_str().unwrap());
        }
        assert_eq!(f.call(request)["ok"], true, "{:?}", f.diag.errs());
        f
    }

    /// A won issue's linked files are downloaded under the plugin's directory, in a folder
    /// named by the unit's key, and handed over as `from` entries beside the README whose
    /// links name them; the folder is there until the unit is finished or released — handed
    /// back or stale — and gone after. The handle's `description` is Linear's own text.
    #[test]
    fn a_units_files_are_kept_until_it_finishes_or_is_released() {
        let url = format!("{MOCK_UPLOADS}acme/1/shot");
        let bytes = b"\x89PNG\r\n\x1a\n\0\xff--png--";
        for end in ["finish", "release", "handed back"] {
            let state = TempDir::new("task-files");
            let mut f = armed_with_file(Some(state.path()), &url, bytes);
            let unit = f.poll()["unit"].clone();
            let key = unit["key"].clone();
            let dir = task_files_dir(state.path(), key.as_str().unwrap());
            assert_eq!(
                dir,
                state
                    .path()
                    .join("task-files")
                    .join(file_safe(key.as_str().unwrap()))
            );
            assert_eq!(std::fs::read(dir.join("shot.png")).unwrap(), bytes, "{end}");
            assert_eq!(
                unit["files"],
                json!([
                    {"path": "task/README.md",
                     "text": "Fix \"the\" café — 修复 🚨\n\nBody ![shot.png](<shot.png>)"},
                    {"path": "task/shot.png", "from": dir.join("shot.png").to_str().unwrap()},
                ]),
                "{end}"
            );
            assert_eq!(
                unit["fields"]["description"],
                format!("Body ![shot.png]({url})"),
                "Linear's own text"
            );
            assert!(f.diag.errs().is_empty(), "{:?}", f.diag.errs());
            match end {
                "finish" => {
                    f.finish(&key, "clean");
                }
                "release" => {
                    f.call(json!({"call": "release", "key": key}));
                }
                _ => {
                    f.call(json!({"call": "release", "key": key, "handed_back": true}));
                }
            }
            assert!(!dir.exists(), "{end}: the folder is deleted");
        }
    }

    /// A restarted child, which holds no unit, finds a leftover folder by its key alone: the
    /// `release` afkd sends for the key deletes it, and so does a `finish`.
    #[test]
    fn a_restarted_child_deletes_a_leftover_folder_by_its_key() {
        let url = format!("{MOCK_UPLOADS}acme/1/shot");
        let state = TempDir::new("task-files-left");
        let mut f = armed_with_file(Some(state.path()), &url, b"x");
        let key = "0f6c1d2e#5e2a9c71#";
        let dir = task_files_dir(state.path(), key);
        for end in ["release", "finish"] {
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("shot.png"), b"x").unwrap();
            if end == "release" {
                f.call(json!({"call": "release", "key": key}));
            } else {
                f.finish(&json!(key), "clean");
            }
            assert!(!dir.exists(), "{end}");
        }
    }

    /// A plugin with no directory of its own downloads nothing: the unit's brief keeps the
    /// Linear url, there is no `from` entry, and Linear is not asked for the file.
    #[test]
    fn with_no_state_dir_no_file_is_downloaded() {
        let url = format!("{MOCK_UPLOADS}acme/1/shot");
        let mut f = armed_with_file(None, &url, b"x");
        let unit = f.poll()["unit"].clone();
        assert_eq!(
            unit["files"],
            json!([{"path": "task/README.md",
                    "text": format!("Fix \"the\" café — 修复 🚨\n\nBody ![shot.png]({url})")}])
        );
        assert!(!f.tracker.calls().contains(&"fetch file"));
    }

    /// A won issue that cannot be handed over at all — its fields besides the brief overflow
    /// afkd's line — has its claim taken back and its downloaded folder deleted before the
    /// service ends.
    #[test]
    fn a_unit_that_cannot_be_handed_over_leaves_no_folder() {
        let url = format!("{MOCK_UPLOADS}acme/1/shot");
        let state = TempDir::new("task-files-fatal");
        let mut f = armed_with_file(Some(state.path()), &url, b"x");
        for n in 0..2_500 {
            f.tracker.seed_comment(
                "ENG-1",
                &format!("c3a1e8f0-6b2d-4e7a-9f1c-2d3e4f5a{n:04}"),
                "Chatter",
                1,
            );
        }
        let Answer::Fatal(reason) = f.plugin.answer(Request::Poll) else {
            panic!("the unit fits");
        };
        assert!(reason.contains("cannot be handed to afkd"), "{reason}");
        let left = std::fs::read_dir(state.path().join("task-files"))
            .map(|d| d.count())
            .unwrap_or(0);
        assert_eq!(left, 0, "no folder is left");
    }

    /// A slot's read or write reaches the issue the service claimed, by its handle's key, while
    /// it is live and once it is finished — a `held` one's too — until afkd releases the key;
    /// `poll` and `finish` move nothing themselves.
    #[test]
    fn a_slot_reaches_a_claimed_issue_until_it_is_released() {
        let mut f = Fixture::armed(settings());
        let key = f.poll()["unit"]["key"].clone();
        let claimed = Claimed {
            uuid: "ENG-1".into(),
            organization: crate::tracker::Organization {
                id: "mock-org".into(),
                url_key: "acme".into(),
            },
        };
        assert_eq!(f.claimed(&key), Some(claimed.clone()), "live");
        f.tracker.fail(
            "delete comment",
            LinearError::Transport {
                stage: "delete comment",
                reason: "mock failure".into(),
            },
        );
        assert_eq!(f.finish(&key, "clean"), json!({"ok": true, "held": true}));
        assert_eq!(
            f.tracker.issue("ENG-1").state.name,
            "Todo",
            "finish moves nothing"
        );
        assert_eq!(f.claimed(&key), Some(claimed), "finished");
        assert_eq!(
            f.claimed(&json!("ENG-9#c9")),
            None,
            "a key it never handed out"
        );
        f.tracker.clear_failure();
        assert_eq!(
            f.call(json!({"call": "release", "key": key})),
            json!({"released": true})
        );
        assert_eq!(f.claimed(&key), None, "released");
    }

    /// The finished issues a post-run call reaches are bounded: past [`FINISHED_MAX`] the
    /// oldest is forgotten, and the newer ones still answer.
    #[test]
    fn the_finished_issues_a_call_reaches_are_bounded() {
        let mut f = Fixture::armed(json!({"team": ["ENG"], "api_key": "lin_api_k",
                                          "claim_from": ["Todo"], "skip_labels": ["Done 🏁"]}));
        for n in 2..=FINISHED_MAX + 1 {
            f.tracker
                .add_issue("Todo", &format!("ENG-{n}"), &format!("課題 {n} 🚧"), "");
        }
        let mut keys = Vec::new();
        for _ in 0..=FINISHED_MAX {
            let unit = f.poll()["unit"].clone();
            assert_eq!(f.finish(&unit["key"], "clean"), json!({"ok": true}));
            // What an `on_done` would do, so the next poll claims the next issue.
            f.tracker
                .seed_label(unit["id"].as_str().unwrap(), "Done 🏁");
            keys.push(unit["key"].clone());
        }
        assert_eq!(f.claimed(&keys[0]), None);
        for key in &keys[1..] {
            assert!(f.claimed(key).is_some(), "{key}");
        }
    }

    /// A call from a later afkd is refused and the next request is still answered; one
    /// before an accepted `hello` is afkd out of step.
    #[test]
    fn an_unknown_call_is_refused_and_one_before_hello_is_fatal() {
        let mut f = Fixture::armed(settings());
        assert_eq!(
            f.call(json!({"call": "rewind", "key": "k"})),
            json!({"ok": false})
        );
        assert_eq!(
            f.diag.errs(),
            ["afkd sent a call this plugin did not list in its `hello` reply"]
        );
        assert_eq!(f.poll()["fire"], true);

        let mut f = Fixture::new();
        let request = serde_json::from_value(json!({"call": "call", "action": "issues"})).unwrap();
        assert!(matches!(
            f.plugin.answer(request),
            Answer::Fatal(reason) if reason.contains("before a `hello`")
        ));
    }

    /// Every armed call hands the tracker a deadline [`CALL_BUDGET`] from the clock's now
    /// before it runs, and clears it once the reply is built.
    #[test]
    fn every_armed_call_runs_under_the_call_budget_and_clears_it() {
        let mut f = Fixture::armed(settings());
        assert!(f.tracker.call_deadlines().is_empty(), "hello hands none");
        let scratch = TempDir::new("budget");
        let mut key = Value::Null;
        for call in [
            "poll",
            "renew",
            "comments",
            "attempt_failed",
            "classify",
            "finish",
            "call",
            "release",
        ] {
            f.clock.advance(Duration::from_secs(3));
            let now = f.clock.now();
            let reply = match call {
                "poll" => f.poll(),
                "renew" => f.call(json!({"call": "renew", "key": key, "renewal": 1})),
                "comments" => f.call(json!({"call": "comments", "key": key})),
                "attempt_failed" => f.call(json!({"call": "attempt_failed", "key": key, "n": 1,
                                                  "max": 2, "reason": "cargo test: 3 failed"})),
                "classify" => f.call(json!({"call": "classify", "key": key,
                                            "scratch": scratch.path(), "outcome": "clean"})),
                "finish" => f.finish(&key, "clean"),
                "call" => f.call(json!({"call": "call", "action": "issues"})),
                _ => f.call(json!({"call": "release", "key": key})),
            };
            if call == "poll" {
                assert_eq!(reply["fire"], true, "{:?}", f.diag.errs());
                key = reply["unit"]["key"].clone();
            }
            let handed = f.tracker.call_deadlines();
            assert_eq!(
                handed[handed.len() - 2..],
                [Some(now + CALL_BUDGET), None],
                "`{call}` runs under its own deadline and clears it"
            );
        }
        assert_eq!(
            f.tracker.call_deadlines().len(),
            16,
            "one set and one clear each"
        );
    }

    /// A tracker failure on `poll` is an idle beat, diagnosed; the next beat claims.
    #[test]
    fn a_poll_tracker_error_is_an_idle_beat_and_logged() {
        let mut f = Fixture::armed(settings());
        f.tracker.fail(
            "read issues",
            LinearError::Status {
                stage: "read issues",
                status: 500,
                retry_after: None,
            },
        );
        assert_eq!(f.poll(), json!({"fire": false}));
        assert_eq!(
            f.diag.errs(),
            ["linear read issues: Linear returned status 500"]
        );
        f.tracker.clear_failure();
        assert_eq!(f.poll()["fire"], true);
    }

    /// The won unit is handed over, and its success line goes to the narration channel.
    #[test]
    fn a_won_claim_narrates_and_hands_the_unit_over() {
        let mut f = Fixture::armed(settings());
        let unit = f.poll()["unit"].clone();
        let claim = &f.tracker.comments_on("ENG-1")[0];
        assert_eq!(unit["key"], format!("ENG-1#{}", claim.id));
        assert_eq!(unit["self"], SELF_ID);
        assert_eq!(unit["fields"]["state"], "Todo");
        assert_eq!(
            f.diag.narrated(),
            [format!("[linear] claimed issue ENG-1 \"{TITLE}\"")]
        );
        assert!(f.diag.errs().is_empty(), "{:?}", f.diag.errs());
    }

    /// The `comments` reply carries only what afkd has not been told — not what the brief
    /// carried, not afkd's own words, not a control marker — each with its author, name,
    /// body and time; a second read carries only what is newer; an unreadable Linear or an
    /// unknown key is `null`.
    #[test]
    fn a_comments_reply_sends_only_the_delta() {
        let mut f = Fixture::armed(settings());
        let at = |secs: u64| std::time::UNIX_EPOCH + Duration::from_secs(secs);
        f.tracker
            .push_comment("ENG-1", "h0", "the ask", "u-phil", at(1_793_000_000));
        let unit = f.poll()["unit"].clone();
        f.tracker.push_comment(
            "ENG-1",
            "h1",
            "看起来不对 🚨\n\n```rust\nlet x = 1;\n```",
            "u-chen",
            at(1_793_095_080),
        );
        f.tracker
            .push_comment("ENG-1", "self1", "my own reply", SELF_ID, at(1_793_095_081));
        f.tracker.push_comment(
            "ENG-1",
            "rival",
            &claim_text("afkd-9"),
            "u-other",
            at(1_793_095_082),
        );
        f.tracker.push_comment(
            "ENG-1",
            "park",
            r"\[afkd\-park\] service=afkd\:\:discuss",
            "u-other",
            at(1_793_095_083),
        );
        f.tracker
            .push_comment("ENG-1", "h2", "", "u-alvaro", at(1_793_095_120));
        assert_eq!(
            f.call(json!({"call": "comments", "key": unit["key"]})),
            json!({"comments": [
                {"id": "h1", "author": "u-chen", "author_name": "u-chen",
                 "body": "看起来不对 🚨\n\n```rust\nlet x = 1;\n```", "at": "2026-10-27T09:58:00Z"},
                {"id": "h2", "author": "u-alvaro", "author_name": "u-alvaro",
                 "body": "", "at": "2026-10-27T09:58:40Z"},
            ]})
        );
        f.tracker.push_comment(
            "ENG-1",
            "h3",
            "Also the flag.",
            "u-alvaro",
            at(1_793_095_200),
        );
        let again = f.call(json!({"call": "comments", "key": unit["key"]}));
        assert_eq!(again["comments"].as_array().unwrap().len(), 1);
        assert_eq!(again["comments"][0]["id"], "h3");

        f.tracker.go_down();
        assert_eq!(
            f.call(json!({"call": "comments", "key": unit["key"]})),
            json!({"comments": null})
        );
        assert_eq!(
            f.call(json!({"call": "comments", "key": "ENG-9#c9"})),
            json!({"comments": null})
        );
        assert_eq!(
            f.diag.errs(),
            [
                "linear read comments: no response (mock failure)",
                "comments for ENG-9#c9, which this plugin holds no claim on",
            ]
        );
    }

    /// A finish that did not land is `held`, and the `release` afkd sends later delivers
    /// it: `false` while Linear is still down, `true` once it lands.
    #[test]
    fn an_undelivered_finish_is_held_and_a_later_release_delivers_it() {
        let mut f = Fixture::armed(settings());
        let key = f.poll()["unit"]["key"].clone();
        f.tracker.fail(
            "delete comment",
            LinearError::Transport {
                stage: "delete comment",
                reason: "mock failure".into(),
            },
        );
        assert_eq!(f.finish(&key, "clean"), json!({"ok": true, "held": true}));
        assert_eq!(
            f.diag.errs(),
            [
                "linear delete comment: no response (mock failure)".to_string(),
                format!(
                    "could not deliver the finish for {}; afkd holds the claim and releases \
                     it on a later beat",
                    key.as_str().unwrap()
                ),
            ]
        );
        assert_eq!(
            f.call(json!({"call": "release", "key": key})),
            json!({"released": false})
        );
        assert_eq!(f.claims_on("ENG-1"), 1, "the lease is held");
        f.tracker.clear_failure();
        assert_eq!(
            f.call(json!({"call": "release", "key": key})),
            json!({"released": true})
        );
        assert_eq!(f.claims_on("ENG-1"), 0);
        assert!(
            !f.tracker
                .actions()
                .iter()
                .any(|a| matches!(a, Action::Move { .. })),
            "a finish moves nothing"
        );
    }

    /// A unit afkd hands back marked `handed_back` keeps its place at the head of its state,
    /// its claim pruned, and is the next poll's again; the plain `release` afkd sends after a
    /// failed `on_claim` puts it at the bottom.
    #[test]
    fn a_unit_handed_back_keeps_its_place_and_a_plain_release_goes_to_the_bottom() {
        let mut f = Fixture::armed(settings());
        f.tracker.add_issue("Todo", "ENG-2", "課題 2 🚧", "");
        let key = f.poll()["unit"]["key"].clone();
        assert_eq!(
            f.call(json!({"call": "release", "key": key, "handed_back": true})),
            json!({"released": true})
        );
        assert_eq!(f.claims_on("ENG-1"), 0);
        assert_eq!(f.tracker.in_state("Todo"), ["ENG-1", "ENG-2"]);

        let unit = f.poll()["unit"].clone();
        assert_eq!(unit["id"], "ENG-1", "the head is claimed again");
        assert_eq!(
            f.call(json!({"call": "release", "key": unit["key"]})),
            json!({"released": true})
        );
        assert_eq!(f.claims_on("ENG-1"), 0);
        assert_eq!(f.tracker.in_state("Todo"), ["ENG-2", "ENG-1"]);
    }

    /// A `held` finish that gives up goes to the bottom of its state, even on a `release`
    /// marked `handed_back`: the mark keeps the place only of a unit still live.
    #[test]
    fn a_held_finish_goes_to_the_bottom_even_marked_handed_back() {
        let mut f = Fixture::armed(settings());
        f.tracker.add_issue("Todo", "ENG-2", "課題 2 🚧", "");
        let key = f.poll()["unit"]["key"].clone();
        f.tracker.fail(
            "delete comment",
            LinearError::Transport {
                stage: "delete comment",
                reason: "mock failure".into(),
            },
        );
        assert_eq!(f.finish(&key, "clean"), json!({"ok": true, "held": true}));
        f.clock.advance(RETRY_FOR);
        assert_eq!(
            f.call(json!({"call": "release", "key": key, "handed_back": true})),
            json!({"released": true})
        );
        assert_eq!(f.tracker.in_state("Todo"), ["ENG-2", "ENG-1"]);
    }

    /// `renew` edits the live claim and refuses a key it holds no claim on; `release` of a
    /// live unit forgets it, and a key with no `#` names nothing.
    #[test]
    fn renew_and_release_answer_for_what_the_plugin_holds() {
        let mut f = Fixture::armed(settings());
        let key = f.poll()["unit"]["key"].clone();
        assert_eq!(
            f.call(json!({"call": "renew", "key": key, "renewal": 3})),
            json!({"ok": true})
        );
        assert_eq!(
            f.call(json!({"call": "renew", "key": "ENG-9#c9", "renewal": 1})),
            json!({"ok": false})
        );
        assert_eq!(
            f.call(json!({"call": "release", "key": key})),
            json!({"released": true})
        );
        assert_eq!(f.claims_on("ENG-1"), 0);
        assert_eq!(
            f.call(json!({"call": "renew", "key": key, "renewal": 4})),
            json!({"ok": false}),
            "a released unit is forgotten"
        );
        assert_eq!(
            f.call(json!({"call": "release", "key": "garbage"})),
            json!({"released": null})
        );
    }

    /// An oversized description is cut to fit one line, in the handle's field and, since the
    /// brief alone is over the line, in the brief too: one diagnostic for each, naming the
    /// issue.
    #[test]
    fn an_oversized_brief_is_cut_and_said_so() {
        let mut f = Fixture::armed(settings());
        f.tracker.edit_issue("ENG-1", |i| {
            i.description = "看起来不对 🚨 — the retry path.\n".repeat(2_600)
        });
        let reply = f.poll();
        let brief = reply["unit"]["files"][0]["text"].as_str().unwrap();
        assert!(
            brief.ends_with("read the whole issue in Linear]"),
            "{}",
            &brief[brief.len() - 120..]
        );
        let field = reply["unit"]["fields"]["description"].as_str().unwrap();
        assert!(field.contains("this description was cut at"), "{field}");
        assert_eq!(
            f.diag.errs(),
            [
                "the description field of ENG-1 was cut to fit afkd's 64 KiB plugin line",
                "the brief for ENG-1 was cut to fit afkd's 64 KiB plugin line",
            ]
        );
    }

    /// The one `call` a child afkd starts for it gets, with no `hello`: a read whose
    /// workspace holds no credential is refused before any request, a call in another
    /// protocol is refused naming both, and an action no one declared is refused. The plugin
    /// arms nothing, and no tracker is reached.
    #[test]
    fn a_child_for_one_call_refuses_what_it_cannot_do_and_reaches_no_tracker() {
        let mut f = Fixture::new();
        let state = TempDir::new("once");
        let once = |action: &str, proto: Value, args: Value| {
            json!({"call": "call", "proto": proto, "action": action, "args": args,
                   "settings": {}, "state_dir": state.path()})
        };
        for (request, error) in [
            (
                once(
                    "teams",
                    json!(2),
                    json!({"ws": {"api_key": "", "base_url": "http://127.0.0.1:9"}}),
                ),
                "teams: the linear.Workspace sets no credential; set api_key, or client_id and \
                 client_secret",
            ),
            (
                once("issues", json!(1), json!({"ws": {"api_key": "lin_api_k"}})),
                "afkd speaks plugin protocol 1, and this plugin speaks 2",
            ),
            (once("assign", json!(2), json!({})), "no action `assign`"),
        ] {
            assert_eq!(f.call(request), json!({"ok": false, "error": error}));
        }
        assert_eq!(f.diag.errs().len(), 3, "each refusal is diagnosed");
        assert!(f.tracker.calls().is_empty(), "no tracker is reached");
        assert!(
            std::fs::read_dir(state.path()).unwrap().next().is_none(),
            "nothing is kept"
        );
        assert!(matches!(
            f.plugin.answer(Request::Poll),
            Answer::Fatal(reason) if reason.contains("before a `hello`")
        ));
    }

    /// An attempt note Linear does not take is said in one line naming the issue and the
    /// note, and lost: nothing is owed afterwards, and the next note lands.
    #[test]
    fn an_attempt_note_linear_does_not_take_is_said_and_lost() {
        let mut f = Fixture::armed(settings());
        let key = f.poll()["unit"]["key"].clone();
        f.tracker.go_down();
        assert_eq!(
            f.call(
                json!({"call": "attempt_failed", "key": key, "n": 1, "max": 2,
                          "reason": "ran out of time\nsee the log"})
            ),
            json!({"ok": true})
        );
        assert_eq!(
            f.diag.errs(),
            [format!(
                "issue ENG-1 \"{TITLE}\": the comment \"[afkd-attempt] 1/2: ran out of time…\" is \
                 lost — linear post comment: no response (mock failure)"
            )]
        );
        f.tracker.clear_failure();
        f.call(
            json!({"call": "attempt_failed", "key": key, "n": 2, "max": 2,
                      "reason": "cargo test: 3 failed"}),
        );
        assert_eq!(
            f.tracker.comments_on("ENG-1").last().unwrap().text,
            "[afkd-attempt] 2/2: cargo test: 3 failed"
        );
    }
}
