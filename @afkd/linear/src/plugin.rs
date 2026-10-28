//! The plugin's state across calls, and one handler per call: the thin layer between afkd's
//! wire ([`crate::wire`]) and the `issue` kind's vendor half ([`crate::issue`]).
//!
//! **afkd runs the slots.** `on_claim` after a `poll` hands a unit over, and `on_done`,
//! `on_park` or `on_fail` after its `finish`; each action a slot calls is one `call`, naming
//! the issue it acts on by its handle — the id and key of a unit this plugin handed over,
//! never an implied claim — which this plugin does on that unit's issue
//! ([`crate::action`]). A finished unit's issue is kept a while for that
//! ([`FINISHED_MAX`]), since its post-run slot's calls arrive after `finish`.
//!
//! What the wire forces, as for `@afkd/trello`:
//!
//! - **Who is asking arrives in `hello`**, and a `hello` without it is refused, since the
//!   park owner cannot work without a service name.
//! - **Every reply fits one line.** afkd caps a line at 64 KiB. A `poll` whose brief would
//!   overflow it is cut to fit ([`fit_poll`]); a `comments` reply sends only what afkd has
//!   not been told yet, and if that still overflows, the newest that fit.
//! - **An undelivered finish is `held`**: `finish` answers `{"ok":true,"held":true}` and
//!   afkd asks `release` for the key on a later beat, which replays what is owed.
//! - **The lines go to stderr**, which afkd files under `[@afkd/linear:err]`.
//! - **Every call answers within 45 seconds**: each armed call runs under [`CALL_BUDGET`],
//!   and a Linear too slow for it reads as a Linear that is down.
//! - **A write Linear could not take is retried after the reply**, by the outbox's worker
//!   thread ([`crate::outbox`]), each issue's writes in order, for up to an hour.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use serde_json::{json, Map, Value};

use crate::action::{from_call, Call, Op};
use crate::auth::Auth;
use crate::client::LinearClient;
use crate::common::{Clock, Diag, StderrDiag, SystemClock, CALL_BUDGET};
use crate::issue::{is_afkd, Identity, LinearUnits, SelfIds, Unit};
use crate::outbox::Outbox;
use crate::rfc3339::format_utc;
use crate::settings::{issue_config, IssueConfig};
use crate::tracker::{Issue, Tracker};
use crate::wire::{
    fire_line, fit_comments, fit_poll, Facts, Request, UnitOutcome, WireComment, MAX_REPLY, PROTO,
};

/// The one kind this plugin provides, its main one: `service(linear)` in a config.
pub(crate) const LINEAR_KIND: &str = "issue";

/// How many finished units' issues a post-run slot's `call`s can still reach. afkd sends
/// them straight after `finish`, one unit at a time, so a handful would do; the rest is
/// headroom, and the oldest goes first.
const FINISHED_MAX: usize = 16;

/// The optional calls the kind answers, exactly as `hello` lists them: every one afkd has.
const CALLS: &[&str] = &["release", "renew", "comments", "attempt_failed", "classify"];

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
/// in the tests — over the credential the service and its outbox worker share. `Send`,
/// since the worker gets one of its own.
pub(crate) type Connect = Box<dyn Fn(&IssueConfig, Arc<Auth>) -> Box<dyn Tracker + Send>>;

/// The plugin across its whole life: unarmed until `hello`, then armed.
pub(crate) struct Plugin {
    connect: Connect,
    clock: Box<dyn Clock>,
    diag: Box<dyn Diag>,
    armed: Option<Armed>,
    /// Whether arming starts the outbox's worker thread. The tests deliver the outbox
    /// themselves, on their own clock.
    worker: bool,
}

/// The armed service: the kind's vendor half, and the units afkd is running.
struct Armed {
    units: LinearUnits,
    /// The units handed over and not yet finished or released, by key.
    live: BTreeMap<String, LiveUnit>,
    /// The issues of the units most recently finished, by key and oldest first, for the
    /// post-run slot's `call`s; at most [`FINISHED_MAX`].
    finished: VecDeque<(String, Held)>,
}

/// What a `call` on a finished unit's issue needs of it: the issue, and the ids afkd's own
/// comments carry on it.
struct Held {
    issue: Issue,
    me: SelfIds,
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
            Box::new(LinearClient::new(&cfg.base_url, auth)) as Box<dyn Tracker + Send>
        });
        Self {
            worker: true,
            ..Self::with_connect(connect, clock, diag)
        }
    }

    /// A plugin that arms through `connect`, and starts no worker: what its outbox owes
    /// waits for the caller to deliver it.
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
            worker: false,
        }
    }

    /// Say what the outbox still owes: afkd has closed the pipe, and the process ends.
    pub(crate) fn close(&self) {
        if let Some(armed) = &self.armed {
            armed.units.outbox().abandon(&*self.diag);
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
            } => {
                let id = Identity {
                    owner,
                    service,
                    roster,
                };
                let arming = arm(&self.connect, proto, &kind, &settings, id)
                    .map_err(|problem| diag.err(&problem))
                    .ok();
                *armed = arming.map(|(armed, cfg, auth)| {
                    if self.worker {
                        start_worker(&self.connect, Arc::clone(armed.units.outbox()), &cfg, auth);
                    }
                    armed
                });
                Answer::Reply(
                    match armed {
                        Some(_) => json!({"ok": true, "proto": PROTO, "calls": CALLS}),
                        None => json!({"ok": false, "proto": PROTO}),
                    }
                    .to_string(),
                )
            }
            Request::Poll => on_armed(armed, clock, |a| a.poll(clock, diag)),
            Request::Release { key } => {
                on_armed(armed, clock, |a| a.release(&key, clock.now(), diag))
            }
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
                a.attempt_failed(&key, n, max, reason.as_deref(), clock.now(), diag)
            }),
            Request::Finish {
                key,
                outcome,
                facts,
            } => on_armed(armed, clock, |a| {
                a.finish(&key, outcome, &facts, clock.now(), diag)
            }),
            Request::Call { action, args } => {
                on_armed(armed, clock, |a| a.call(&action, &args, clock.now(), diag))
            }
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
        None => Answer::Fatal("afkd sent a call before a `hello` this plugin accepted".into()),
    }
}

/// Arm the kind `hello` names over its settings, as `id`, or say why not; the settings and
/// the credential it armed with come back too, for the outbox's worker. afkd has already
/// held the block to the manifest, so what can be wrong here is the protocol, the kind, a
/// missing identity, or a rule the manifest cannot express. Nothing is sent to Linear: an
/// outage when the service arms is a poll's idle beat, not a refused service.
fn arm(
    connect: &Connect,
    proto: u32,
    kind: &str,
    settings: &serde_json::Value,
    id: Identity,
) -> Result<(Armed, IssueConfig, Arc<Auth>), String> {
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
    let cfg = issue_config(settings).map_err(|e| format!("trigger {kind}: {e}"))?;
    let auth = Arc::new(Auth::new(&cfg.auth));
    let armed = Armed {
        units: LinearUnits::new(connect(&cfg, Arc::clone(&auth)), &cfg, id),
        live: BTreeMap::new(),
        finished: VecDeque::new(),
    };
    Ok((armed, cfg, auth))
}

/// Start the worker that delivers `outbox` for the life of the process, on a tracker client
/// of its own, which no call's deadline reaches, sharing the service's credential.
fn start_worker(connect: &Connect, outbox: Arc<Outbox>, cfg: &IssueConfig, auth: Arc<Auth>) {
    let tracker = connect(cfg, auth);
    std::thread::spawn(move || outbox.run(&*tracker, &SystemClock, &StderrDiag));
}

/// The `poll` reply that hands nothing over.
fn idle() -> Answer {
    Answer::Reply(json!({"fire": false}).to_string())
}

impl Armed {
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
        let wire = self.units.wire_unit(&unit);
        let whole = fire_line(&wire);
        let line = if whole.len() <= MAX_REPLY {
            whole
        } else if let Some(cut) = fit_poll(&wire) {
            diag.err(&format_args!(
                "the brief for {} was cut to fit afkd's 64 KiB plugin line",
                unit.identifier()
            ));
            cut
        } else {
            // Only a unit whose fields besides the brief overflow the line reaches this:
            // nothing can be handed over, so the claim is taken back before the service ends.
            self.units.release_stale(&wire.key, clock.now(), diag);
            return Answer::Fatal(format!(
                "{} cannot be handed to afkd: even with its brief cut away, the unit is over \
                 afkd's 64 KiB plugin line",
                unit.identifier()
            ));
        };
        let reported = wire.seen.iter().cloned().collect();
        self.live.insert(wire.key, LiveUnit { unit, reported });
        Answer::Reply(line)
    }

    /// Release a journal key — a crashed run's, a `held` finish's, a unit afkd handed
    /// straight back, or one whose `on_claim` failed — and forget it.
    fn release(&mut self, key: &str, now: Instant, diag: &dyn Diag) -> Answer {
        self.live.remove(key);
        self.finished.retain(|(finished, _)| finished != key);
        let released = self.units.release_stale(key, now, diag);
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
        now: Instant,
        diag: &dyn Diag,
    ) -> Answer {
        let ok = match self.live.get(key) {
            Some(live) => {
                self.units
                    .attempt_failed(&live.unit, n, max, reason, now, diag);
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
        now: Instant,
        diag: &dyn Diag,
    ) -> Answer {
        let Some(live) = self.live.remove(key) else {
            diag.err(&format_args!(
                "finish for {key}, which this plugin holds no claim on"
            ));
            return Answer::Reply(json!({"ok": true}).to_string());
        };
        let reply = if self.units.finish(&live.unit, outcome, facts, now, diag) {
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
        let held = Held {
            issue: live.unit.issue().clone(),
            me: live.unit.me().clone(),
        };
        self.finished.push_back((key.to_string(), held));
        Answer::Reply(reply.to_string())
    }

    /// Do one action a slot called, on the issue of the unit its handle names — a live one,
    /// or one finished since. `{"ok":false}` with the sentence, diagnosed too, for an action
    /// afkd would not have bound, a handle naming no issue, a read that could not be written,
    /// or a Linear that refused it. A write Linear could not be reached for is taken for
    /// delivery and `ok`: the outbox retries it, and the slot's next write queues behind it.
    /// A read is done now or refused, never queued.
    fn call(
        &self,
        action: &str,
        args: &Map<String, Value>,
        now: Instant,
        diag: &dyn Diag,
    ) -> Answer {
        let refuse = |error: String| {
            diag.err(&error);
            Answer::Reply(json!({"ok": false, "error": error}).to_string())
        };
        let Call { key, op } = match from_call(action, args) {
            Ok(call) => call,
            Err(problem) => return refuse(problem),
        };
        let (issue, me) = match self.live.get(&key) {
            Some(live) => (live.unit.issue(), live.unit.me()),
            None => match self.finished.iter().find(|(finished, _)| *finished == key) {
                Some((_, held)) => (&held.issue, &held.me),
                None => {
                    return refuse(format!(
                        "{action} for {key}, which this plugin holds no issue for"
                    ))
                }
            },
        };
        let done = match op {
            Op::Act(act) => self
                .units
                .act(issue, act, now, diag)
                .map_err(|e| e.to_string()),
            Op::Read { read, to } => self.units.read(issue, me, read, &to, diag),
        };
        match done {
            Ok(()) => Answer::Reply(json!({"ok": true}).to_string()),
            Err(problem) => refuse(problem),
        }
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
    use crate::tracker::{Action, LinearError, MockTracker, SELF_ID};
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
                Box::new(move |_, _| Box::new(Arc::clone(&shared)) as Box<dyn Tracker + Send>);
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

        /// The same, armed as the `issue` kind over `settings`.
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

        /// Call `action` on the issue whose key is `key`, its handle first, as afkd binds it.
        fn act(&mut self, action: &str, args: Value, key: &Value) -> Value {
            let mut bound = json!({"issue": {"id": "ENG-1", "key": key}});
            for (k, v) in args.as_object().unwrap() {
                bound[k] = v.clone();
            }
            self.call(json!({"call": "call", "action": action, "args": bound}))
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
        json!({"team": "ENG", "api_key": "lin_api_k", "pick_from": "Todo"})
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
                "trigger issue: setting `api_key`: write api_key or client_id and \
                 client_secret, not both"
                    .into(),
            ),
            (
                with(&|r| {
                    r["settings"].as_object_mut().unwrap().remove("api_key");
                }),
                "trigger issue: setting `api_key`: needs api_key, or client_id and \
                 client_secret"
                    .into(),
            ),
            (
                with(&|r| {
                    let s = r["settings"].as_object_mut().unwrap();
                    s.remove("api_key");
                    s.insert("client_secret".into(), json!("s"));
                }),
                "trigger issue: setting `client_secret`: client_secret needs client_id".into(),
            ),
            (
                with(&|r| {
                    r["settings"].as_object_mut().unwrap().remove("team");
                }),
                "trigger issue: setting `team`: `team` is required: name the team by its key"
                    .into(),
            ),
            (
                with(&|r| {
                    r["settings"].as_object_mut().unwrap().remove("pick_from");
                }),
                "trigger issue: setting `pick_from`: `pick_from` is required: name the \
                 workflow state or states new issues are claimed from"
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
    /// not declare, a handle that is no handle, a key it holds no issue for, and a Linear
    /// that refused it. The next request is still answered.
    #[test]
    fn a_call_the_plugin_cannot_do_answers_its_sentence() {
        let mut f = Fixture::armed(settings());
        let key = f.poll()["unit"]["key"].clone();
        let mut expect = Vec::new();
        for (action, args, error) in [
            (
                "assign",
                json!({"issue": {"id": "ENG-1", "key": key}, "who": "me"}),
                "no action `assign`".to_string(),
            ),
            (
                "comment",
                json!({"issue": "ENG-1", "text": "done 🚀"}),
                "comment: parameter issue must be an item handle".to_string(),
            ),
            (
                "comment",
                json!({"issue": {"id": "ENG-9", "key": "ENG-9#c9"}, "text": "done 🚀"}),
                "comment for ENG-9#c9, which this plugin holds no issue for".to_string(),
            ),
            (
                "move_to",
                json!({"issue": {"id": "ENG-1", "key": key}, "state": "Nirgendwo 🚫"}),
                "linear resolve state: no workflow state named 'Nirgendwo 🚫' in team ENG"
                    .to_string(),
            ),
        ] {
            assert_eq!(
                f.call(json!({"call": "call", "action": action, "args": args})),
                json!({"ok": false, "error": error})
            );
            expect.push(error);
        }
        assert_eq!(f.diag.errs(), expect);
        assert_eq!(f.tracker.issue("ENG-1").state.name, "Todo");
        assert_eq!(
            f.call(json!({"call": "renew", "key": key, "renewal": 1})),
            json!({"ok": true})
        );
    }

    /// A slot's action on the live issue lands and is narrated; a read writes its file.
    #[test]
    fn a_call_on_the_live_issue_is_done_and_narrated() {
        let mut f = Fixture::armed(settings());
        let key = f.poll()["unit"]["key"].clone();
        assert_eq!(
            f.act("move_to", json!({"state": "In Progress"}), &key),
            json!({"ok": true})
        );
        assert_eq!(f.tracker.issue("ENG-1").state.name, "In Progress");
        let dir = TempDir::new("call");
        let to = dir.path().join("description.md");
        assert_eq!(
            f.act("read_description", json!({"to": to}), &key),
            json!({"ok": true})
        );
        assert_eq!(std::fs::read_to_string(&to).unwrap(), "Body");
        assert_eq!(
            f.diag.narrated()[1..],
            [
                format!("[linear] moving issue ENG-1 \"{TITLE}\" to state \"In Progress\""),
                format!(
                    "[linear] writing the description of issue ENG-1 \"{TITLE}\" to {}",
                    to.display()
                ),
            ]
        );
        assert!(f.diag.errs().is_empty(), "{:?}", f.diag.errs());
    }

    /// `poll` and `finish` move nothing themselves, and the post-run slot's calls, which
    /// afkd sends after `finish`, still reach the finished issue — a `held` one's too — until
    /// afkd releases the key.
    #[test]
    fn a_post_run_call_reaches_a_finished_issue_until_it_is_released() {
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
            f.tracker.issue("ENG-1").state.name,
            "Todo",
            "finish moves nothing"
        );

        assert_eq!(
            f.act("move_to", json!({"state": "In Progress"}), &key),
            json!({"ok": true})
        );
        assert_eq!(f.tracker.issue("ENG-1").state.name, "In Progress");
        let dir = TempDir::new("finished");
        let to = dir.path().join("thread.md");
        assert_eq!(
            f.act("list_comments", json!({"to": to}), &key),
            json!({"ok": true}),
            "a read reaches it too"
        );
        f.tracker.clear_failure();
        assert_eq!(
            f.call(json!({"call": "release", "key": key})),
            json!({"released": true})
        );
        let released = f.diag.errs().len();
        assert_eq!(
            f.act("comment", json!({"text": "late"}), &key),
            json!({"ok": false, "error": format!(
                "comment for {}, which this plugin holds no issue for",
                key.as_str().unwrap()
            )})
        );
        assert_eq!(
            f.diag.errs().len(),
            released + 1,
            "the refusal is diagnosed"
        );
    }

    /// The finished issues a post-run call reaches are bounded: past [`FINISHED_MAX`] the
    /// oldest is forgotten, and the newer ones still answer.
    #[test]
    fn the_finished_issues_a_call_reaches_are_bounded() {
        let mut f = Fixture::armed(settings());
        for n in 2..=FINISHED_MAX + 1 {
            f.tracker
                .add_issue("Todo", &format!("ENG-{n}"), &format!("課題 {n} 🚧"), "");
        }
        let mut keys = Vec::new();
        for _ in 0..=FINISHED_MAX {
            let key = f.poll()["unit"]["key"].clone();
            assert_eq!(f.finish(&key, "clean"), json!({"ok": true}));
            // The `on_done` afkd runs next, so the next poll claims the next issue.
            assert_eq!(
                f.act("move_to", json!({"state": "In Progress"}), &key),
                json!({"ok": true})
            );
            keys.push(key);
        }
        let comment = json!({"text": "see the run log"});
        assert_eq!(f.act("comment", comment.clone(), &keys[0])["ok"], false);
        for key in &keys[1..] {
            assert_eq!(f.act("comment", comment.clone(), key), json!({"ok": true}));
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
        let request = serde_json::from_value(json!({"call": "call", "action": "archive"})).unwrap();
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
                "call" => f.call(json!({"call": "call", "action": "comment"})),
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

    /// An oversized brief is cut to fit one line, with one diagnostic naming the issue.
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
        assert_eq!(
            f.diag.errs(),
            ["the brief for ENG-1 was cut to fit afkd's 64 KiB plugin line"]
        );
    }

    /// afkd closing the pipe with writes still owed names each.
    #[test]
    fn closing_with_writes_owed_names_them() {
        let mut f = Fixture::armed(settings());
        let key = f.poll()["unit"]["key"].clone();
        f.tracker.go_down();
        f.call(
            json!({"call": "attempt_failed", "key": key, "n": 1, "max": 1,
                      "reason": "ran out of time\nsee the log"}),
        );
        let before = f.diag.errs().len();
        f.plugin.close();
        assert_eq!(
            f.diag.errs()[before..],
            [format!(
                "afkd ended the plugin with the comment \"[afkd-attempt] 1/1: ran out of time…\" \
                 for issue ENG-1 \"{TITLE}\" still owed"
            )]
        );
    }
}
