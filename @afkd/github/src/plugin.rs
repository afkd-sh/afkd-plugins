//! The plugin's state across calls, and one handler per call: the thin layer between
//! afkd's wire ([`crate::wire`]) and the armed kind's vendor half ([`crate::issue`] or
//! [`crate::pr`], behind [`crate::kind`]). Everything here is written once, against the
//! seam, so both kinds share every rule below.
//!
//! **afkd runs the slots.** `on_claim` after a `poll` hands a unit over, and `on_done` or
//! `on_fail` after its `finish`; each action a slot calls is one `call`, naming the item it
//! acts on by its handle — the id and key of a unit this plugin handed over, never an
//! implied claim — which this plugin does on that unit's issue or pull request, under the
//! armed kind's names ([`Units::VOCABULARY`]). A finished unit is kept a while for that
//! ([`FINISHED_MAX`]), since its post-run slot's calls arrive after `finish`.
//!
//! Four things the wire forces that the built-in never had to do:
//!
//! - **Every reply fits one line.** afkd caps a line at 64 KiB. A `poll` whose brief would
//!   overflow it is cut to fit ([`fit_poll`]); a `comments` reply sends only what afkd has
//!   not been told yet, and if that still overflows, the newest that fit.
//! - **The identity is resolved at `hello`, and again by whichever call needs it if that
//!   failed.** afkd's reaper runs at the top of a beat, before its poll, so a fresh child's
//!   first call after `hello` can be `release` of a crashed run's key — and GitHub's
//!   release unassigns the bot by login. Without the identity, `release` answers `false`,
//!   and afkd asks again next beat: the built-in's reaper waits for the identity the same
//!   way.
//! - **Every call answers within 45 seconds.** afkd ends the service on a missed reply,
//!   so each armed call runs under [`CALL_BUDGET`], and a forge too slow for it reads as
//!   a forge that is down.
//! - **A write GitHub could not take is retried after the reply.** afkd retries no action
//!   and its deadline leaves no room for a retry inside the call, so an action or the
//!   claim marker's release that could not reach GitHub is queued ([`crate::outbox`]), the
//!   call answers `ok`, and a worker thread delivers it — each item's writes in order —
//!   once GitHub is back, for up to an hour.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Instant;

use serde_json::{json, Map, Value};

use crate::claim::is_claim;
use crate::client::{Github, GithubClient};
use crate::common::{Clock, Diag, StderrDiag, SystemClock, CALL_BUDGET};
use crate::issue::IssueUnits;
use crate::kind::{ClaimedUnit, Units};
use crate::lifecycle::{from_call, Call};
use crate::outbox::{Outbox, Write};
use crate::pr::PrUnits;
use crate::rfc3339::format_utc;
use crate::settings::{issue_config, pr_review_config, GithubConfig};
use crate::wire::{fire_line, fit_comments, fit_poll, Request, WireComment, MAX_REPLY, PROTO};

/// The issue kind, the plugin's main one: `service(github)` in a config.
pub(crate) const ISSUE_KIND: &str = "issue";

/// The pull-request review kind: `service(github.pr)` in a config.
pub(crate) const PR_KIND: &str = "pr";

/// How many finished units a post-run hook's `call`s can still reach. afkd sends them
/// straight after `finish`, one unit at a time, so a handful would do; the rest is
/// headroom, and the oldest goes first.
const FINISHED_MAX: usize = 16;

/// What the process does with one request.
#[derive(Debug, PartialEq)]
pub(crate) enum Answer {
    /// Write this line to stdout.
    Reply(String),
    /// Write this sentence to stderr and exit non-zero: afkd faults the service, and the
    /// sentence is the last thing in its log.
    Fatal(String),
}

/// Builds the forge client a `hello` arms with — the real one in the running plugin, a
/// mock in the tests. `Send`, since the outbox's worker gets one of its own.
pub(crate) type Connect = Box<dyn Fn(&GithubConfig) -> Box<dyn GithubClient + Send>>;

/// The plugin across its whole life: unarmed until `hello`, then one armed kind.
pub(crate) struct Plugin {
    connect: Connect,
    clock: Box<dyn Clock>,
    diag: Box<dyn Diag>,
    armed: Option<Box<dyn Service>>,
    /// Whether arming starts the outbox's worker thread. The tests deliver the outbox
    /// themselves, on their own clock.
    worker: bool,
}

/// One armed service, whichever kind it is: the calls afkd makes after `hello`.
trait Service {
    /// The optional calls the armed kind answers, as `hello` lists them.
    fn calls(&self) -> &'static [&'static str];
    /// Set (or, with `None`, clear) the current call's deadline, which the kind's scan
    /// and every forge request it makes until the next set run under.
    fn set_call_deadline(&self, deadline: Option<Instant>);
    /// The token's login, resolved once and kept for the claims it marks; `None`,
    /// diagnosed, while the forge cannot say.
    fn me(&mut self, diag: &dyn Diag) -> Option<String>;
    fn poll(&mut self, clock: &dyn Clock, diag: &dyn Diag) -> Answer;
    fn release(&mut self, key: &str, diag: &dyn Diag) -> Answer;
    fn renew(&self, key: &str, renewal: u64, diag: &dyn Diag) -> Answer;
    fn comments(&mut self, key: &str, diag: &dyn Diag) -> Answer;
    fn finish(&mut self, key: &str, now: Instant, diag: &dyn Diag) -> Answer;
    fn call(
        &self,
        action: &str,
        args: &Map<String, Value>,
        now: Instant,
        diag: &dyn Diag,
    ) -> Answer;
    /// The writes the armed kind owes the forge, for the worker that delivers them.
    fn outbox(&self) -> &Arc<Outbox>;
}

/// One armed service of kind `K`.
struct Armed<K: Units> {
    units: K,
    /// The token's login, resolved at `hello` — or, failing that, on the first call that
    /// needs it — and kept.
    me: Option<String>,
    /// The units handed over and not yet finished or released, by key.
    live: BTreeMap<String, LiveUnit<K::Unit>>,
    /// The units most recently finished, by key and oldest first, for the post-run hook's
    /// `call`s; at most [`FINISHED_MAX`].
    finished: VecDeque<(String, K::Unit)>,
}

/// A unit afkd is running.
struct LiveUnit<U> {
    unit: U,
    /// The comment ids afkd has been told about: the unit's `seen`, then everything a
    /// `comments` reply carried or left out.
    reported: HashSet<String>,
}

impl Plugin {
    /// A plugin that arms against the real GitHub.
    pub(crate) fn new(clock: Box<dyn Clock>, diag: Box<dyn Diag>) -> Self {
        let connect: Connect = Box::new(|cfg| {
            Box::new(Github::new(&cfg.host, &cfg.token)) as Box<dyn GithubClient + Send>
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
            armed.outbox().abandon(&*self.diag);
        }
    }

    /// Answer one request.
    pub(crate) fn answer(&mut self, request: Request) -> Answer {
        let (clock, diag, armed) = (&*self.clock, &*self.diag, &mut self.armed);
        // An optional call the armed kind never listed is refused as an unknown one is.
        // Before any `hello`, `on_armed` below ends the process instead.
        if let (Some(call), Some(service)) = (request.optional_call(), armed.as_deref()) {
            if !service.calls().contains(&call) {
                return unlisted(diag);
            }
        }
        match request {
            Request::Hello {
                proto,
                kind,
                settings,
            } => {
                let arming = arm(&self.connect, proto, &kind, &settings)
                    .map_err(|problem| diag.err(&problem))
                    .ok();
                *armed = arming.map(|(service, cfg)| {
                    if self.worker {
                        start_worker(&self.connect, Arc::clone(service.outbox()), &cfg);
                    }
                    service
                });
                let Some(service) = armed.as_deref_mut() else {
                    return Answer::Reply(json!({"ok": false, "proto": PROTO}).to_string());
                };
                // `me` is resolved here, under the call budget like any forge read, so a
                // token the forge refuses shows at start. A forge that cannot say yet does
                // not refuse the service over a blip: the first call that needs it asks again.
                service.set_call_deadline(Some(clock.now() + CALL_BUDGET));
                service.me(diag);
                service.set_call_deadline(None);
                let reply = json!({"ok": true, "proto": PROTO, "calls": service.calls()});
                Answer::Reply(reply.to_string())
            }
            Request::Poll => on_armed(armed, clock, |a| a.poll(clock, diag)),
            Request::Release { key } => on_armed(armed, clock, |a| a.release(&key, diag)),
            Request::Renew { key, renewal } => {
                on_armed(armed, clock, |a| a.renew(&key, renewal, diag))
            }
            Request::Comments { key } => on_armed(armed, clock, |a| a.comments(&key, diag)),
            Request::Finish { key } => {
                on_armed(armed, clock, |a| a.finish(&key, clock.now(), diag))
            }
            Request::Call { action, args } => {
                on_armed(armed, clock, |a| a.call(&action, &args, clock.now(), diag))
            }
            Request::Unknown => unlisted(diag),
        }
    }
}

/// The reply to a call this plugin did not list in its `hello` reply.
fn unlisted(diag: &dyn Diag) -> Answer {
    diag.err(&"afkd sent a call this plugin did not list in its `hello` reply");
    Answer::Reply(json!({"ok": false}).to_string())
}

/// Run `f` on the armed kind under the call's [`CALL_BUDGET`], cleared once the reply is
/// built so no call inherits another's deadline — or end the process, since afkd sends
/// nothing but `hello` before a `hello` it has seen accepted.
fn on_armed(
    armed: &mut Option<Box<dyn Service>>,
    clock: &dyn Clock,
    f: impl FnOnce(&mut dyn Service) -> Answer,
) -> Answer {
    match armed {
        Some(armed) => {
            armed.set_call_deadline(Some(clock.now() + CALL_BUDGET));
            let answer = f(armed.as_mut());
            armed.set_call_deadline(None);
            answer
        }
        None => Answer::Fatal("afkd sent a call before a `hello` this plugin accepted".into()),
    }
}

/// Arm the kind `hello` names over its settings, or say why not; the settings it armed
/// with come back too, for the outbox's worker. afkd has already held the block to the
/// manifest, so what can be wrong here is the protocol, the kind, or a rule the manifest
/// cannot express.
fn arm(
    connect: &Connect,
    proto: u32,
    kind: &str,
    settings: &serde_json::Value,
) -> Result<(Box<dyn Service>, GithubConfig), String> {
    if proto != PROTO {
        return Err(format!(
            "afkd speaks plugin protocol {proto}, and this plugin speaks {PROTO}"
        ));
    }
    let fault = |e| format!("trigger {kind}: {e}");
    Ok(match kind {
        ISSUE_KIND => {
            let cfg = issue_config(settings).map_err(fault)?;
            let service: Box<dyn Service> =
                Box::new(Armed::new(IssueUnits::new(connect(&cfg), &cfg)));
            (service, cfg)
        }
        PR_KIND => {
            let cfg = pr_review_config(settings).map_err(fault)?;
            let service: Box<dyn Service> = Box::new(Armed::new(PrUnits::new(connect(&cfg), &cfg)));
            (service, cfg)
        }
        _ => return Err(format!("kind `{kind}` is not provided by @afkd/github")),
    })
}

/// Start the worker that delivers `outbox` for the life of the process, on a forge client
/// of its own, which no call's deadline reaches.
fn start_worker(connect: &Connect, outbox: Arc<Outbox>, cfg: &GithubConfig) {
    let client = connect(cfg);
    std::thread::spawn(move || outbox.run(&*client, &SystemClock, &StderrDiag));
}

/// The `poll` reply that hands nothing over.
fn idle() -> Answer {
    Answer::Reply(json!({"fire": false}).to_string())
}

impl<K: Units> Armed<K> {
    fn new(units: K) -> Self {
        Self {
            units,
            me: None,
            live: BTreeMap::new(),
            finished: VecDeque::new(),
        }
    }

    /// The unit `key` names, live or finished since.
    fn unit(&self, key: &str) -> Option<&K::Unit> {
        match self.live.get(key) {
            Some(live) => Some(&live.unit),
            None => self
                .finished
                .iter()
                .find(|(finished, _)| finished == key)
                .map(|(_, unit)| unit),
        }
    }
}

impl<K: Units> Service for Armed<K> {
    fn calls(&self) -> &'static [&'static str] {
        K::CALLS
    }

    fn set_call_deadline(&self, deadline: Option<Instant>) {
        self.units.set_call_deadline(deadline);
    }

    /// The token's login: the one already resolved, or resolved now and kept. `None` —
    /// diagnosed — when the forge will not say; the caller answers as though nothing
    /// could be done this beat, and the next call that needs it asks again.
    fn me(&mut self, diag: &dyn Diag) -> Option<String> {
        if self.me.is_none() {
            match self.units.resolve_me() {
                Ok(me) => self.me = Some(me),
                Err(e) => diag.err(&e),
            }
        }
        self.me.clone()
    }

    /// One beat: resolve the identity if it is not yet known, then run the claim race. A
    /// forge failure is the built-in's idle beat, diagnosed.
    fn poll(&mut self, clock: &dyn Clock, diag: &dyn Diag) -> Answer {
        let Some(me) = self.me(diag) else {
            return idle();
        };
        let unit = match self.units.try_claim_next(&me, diag, clock) {
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
                unit.thread()
            ));
            cut
        } else {
            // Only a unit whose fields besides the brief overflow the line reaches this:
            // nothing can be handed over, so the claim is taken back before the service
            // ends.
            self.units.release(&unit, diag);
            return Answer::Fatal(format!(
                "{} cannot be handed to afkd: even with its brief cut away, the unit is over \
                 afkd's 64 KiB plugin line",
                unit.thread()
            ));
        };
        let reported = wire.seen.iter().cloned().collect();
        self.live.insert(wire.key, LiveUnit { unit, reported });
        Answer::Reply(line)
    }

    /// Release a leftover journal key — a crashed run's, a unit afkd handed straight back,
    /// or one whose `on_claim` failed — and forget it. Without the identity nothing is
    /// written and the key is kept (`false`), so a `/user` outage never drops an entry the
    /// release still owes.
    fn release(&mut self, key: &str, diag: &dyn Diag) -> Answer {
        self.live.remove(key);
        self.finished.retain(|(finished, _)| finished != key);
        let released = match self.me(diag) {
            Some(me) => self.units.release_stale(key, &me, diag),
            None => Some(false),
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
    /// afkd's watch drops the ids in its cursor, afkd's own comments and claim markers
    /// anyway, so leaving those out changes no delivery; it is what keeps a long thread
    /// inside one line. `null` when the forge could not be read.
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
        let me = live.unit.claimed_as();
        let mut fresh: Vec<_> = all
            .into_iter()
            .filter(|c| {
                !live.reported.contains(&c.id.to_string())
                    && c.user.login != me
                    && !is_claim(&c.body)
            })
            .collect();
        fresh.sort_by_key(|c| (c.created_at, c.id));
        let fresh: Vec<WireComment> = fresh
            .into_iter()
            .map(|c| WireComment {
                id: c.id.to_string(),
                author: c.user.login.clone(),
                author_name: c.user.login,
                body: c.body,
                at: format_utc(c.created_at),
            })
            .collect();
        let (line, dropped) = fit_comments(&fresh);
        if dropped > 0 {
            let ids: Vec<&str> = fresh[..dropped].iter().map(|c| c.id.as_str()).collect();
            diag.err(&format_args!(
                "{} comments on {} did not fit afkd's 64 KiB plugin line and were not \
                 delivered: {}",
                dropped,
                live.unit.thread(),
                ids.join(", ")
            ));
        }
        live.reported.extend(fresh.into_iter().map(|c| c.id));
        Answer::Reply(line)
    }

    /// Finish a unit — its claim marker released, or queued for release when GitHub could
    /// not be reached — and keep it for the post-run hook's `call`s. Always plain `ok`: the
    /// release is best-effort, so nothing is held.
    fn finish(&mut self, key: &str, now: Instant, diag: &dyn Diag) -> Answer {
        let ok = Answer::Reply(json!({"ok": true}).to_string());
        let Some(live) = self.live.remove(key) else {
            diag.err(&format_args!(
                "finish for {key}, which this plugin holds no claim on"
            ));
            return ok;
        };
        self.units.finish(&live.unit, now, diag);
        if self.finished.len() == FINISHED_MAX {
            self.finished.pop_front();
        }
        self.finished.push_back((key.to_string(), live.unit));
        ok
    }

    /// Do one action a slot called, on the unit its handle names — a live one, or one
    /// finished since. `{"ok":false}` with the sentence, diagnosed too, for an action afkd
    /// would not have bound under the armed kind's names, a handle naming no unit, or a
    /// forge that refused it. An action the forge could not be reached for is taken for
    /// delivery and `ok`: the outbox retries it, and the slot's next action on the item
    /// queues behind it.
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
        let Call { key, action: act } = match from_call(&K::VOCABULARY, action, args) {
            Ok(call) => call,
            Err(problem) => return refuse(problem),
        };
        let Some(unit) = self.unit(&key) else {
            return refuse(format!(
                "{action} for {key}, which this plugin holds no claim on"
            ));
        };
        match self.units.deliver(unit, Write::Act(act), now, diag) {
            Ok(_) => Answer::Reply(json!({"ok": true}).to_string()),
            Err(e) => refuse(e.to_string()),
        }
    }

    fn outbox(&self) -> &Arc<Outbox> {
        self.units.outbox()
    }
}

#[cfg(test)]
mod tests {
    //! The handlers over a mock forge: what each call answers, and the state the plugin
    //! keeps between them. The wire itself — a real child, real JSON lines, a real HTTP
    //! forge — is `tests/wire.rs`.

    use super::*;
    use crate::claim::{claim_key, claim_text};
    use crate::client::{Action, MockClient, Repo};
    use crate::common::{CaptureDiag, FakeClock};
    use crate::lifecycle::{Vocabulary, ISSUE_VOCABULARY, PR_VOCABULARY};
    use crate::outbox::RETRY_FIRST;
    use std::time::Duration;

    /// A [`Clock`] the test keeps a handle on after the plugin owns it.
    struct SharedClock(Arc<FakeClock>);

    impl Clock for SharedClock {
        fn sleep(&self, d: Duration) {
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
    }

    struct Fixture {
        plugin: Plugin,
        mock: Arc<MockClient>,
        diag: Arc<CaptureDiag>,
        clock: Arc<FakeClock>,
        /// The armed kind's names, which [`act`](Self::act) sends a call under.
        vocabulary: Vocabulary,
    }

    /// The token's login: non-ASCII and bracketed, as a GitHub App's bot login is.
    const ME: &str = "björn-öst[bot]";
    const REPO: &str = "acme/widgets";

    impl Fixture {
        /// A plugin over a mock forge whose token is `björn-öst[bot]`'s, not yet armed.
        fn new() -> Self {
            let mock = Arc::new(MockClient::new(ME));
            let diag = Arc::new(CaptureDiag::default());
            let clock = Arc::new(FakeClock::new());
            let forge = Arc::clone(&mock);
            let connect: Connect =
                Box::new(move |_| Box::new(Arc::clone(&forge)) as Box<dyn GithubClient + Send>);
            let plugin = Plugin::with_connect(
                connect,
                Box::new(SharedClock(Arc::clone(&clock))),
                Box::new(Shared(Arc::clone(&diag))),
            );
            Self {
                plugin,
                mock,
                diag,
                clock,
                vocabulary: ISSUE_VOCABULARY,
            }
        }

        /// The same, armed as the `issue` kind over `settings`.
        fn armed(settings: serde_json::Value) -> Self {
            Self::armed_as(ISSUE_KIND, settings)
        }

        /// The same, armed as `kind` over `settings`.
        fn armed_as(kind: &str, settings: serde_json::Value) -> Self {
            let mut f = Self::new();
            if kind == PR_KIND {
                f.vocabulary = PR_VOCABULARY;
            }
            let hello = f.call(hello(kind, settings));
            assert_eq!(hello["ok"], true, "{:?}", f.diag.lines());
            f
        }

        /// Send one request as afkd writes it, and read the reply line back as JSON.
        fn call(&mut self, request: serde_json::Value) -> serde_json::Value {
            let request = serde_json::from_value(request).expect("an afkd request");
            match self.plugin.answer(request) {
                Answer::Reply(line) => {
                    assert!(line.len() <= MAX_REPLY && !line.contains('\n'), "{line}");
                    serde_json::from_str(&line).expect("a JSON reply")
                }
                Answer::Fatal(reason) => panic!("unexpected fatal: {reason}"),
            }
        }

        fn poll(&mut self) -> serde_json::Value {
            self.call(json!({"call": "poll"}))
        }

        /// One pass of the outbox's worker, on the fixture's forge and clock: when the
        /// earliest write left is due.
        fn deliver(&self) -> Option<Instant> {
            let armed = self.plugin.armed.as_ref().expect("an armed plugin");
            armed
                .outbox()
                .deliver_due(&*self.mock, &*self.clock, &*self.diag)
        }

        /// Deliver until nothing is owed, the clock moved on to each next try.
        fn drain(&self) {
            while let Some(next) = self.deliver() {
                self.clock
                    .advance(next.saturating_duration_since(self.clock.now()));
            }
        }

        fn finish(&mut self, key: &serde_json::Value, outcome: &str) -> serde_json::Value {
            self.call(
                json!({"call": "finish", "id": "7", "key": key, "outcome": outcome,
                             "facts": {"signal": "proceed", "reason": null, "duration_ms": 3,
                                       "cost": 0.0, "turns": null, "tokens": null,
                                       "run_name": "260926-100400-unit-7-1"}}),
            )
        }

        /// One `call` of `action` with `args` on the item of the unit `key` names, as afkd
        /// sends a slot's action: the item's handle first, under the armed kind's names.
        fn act(
            &mut self,
            action: &str,
            args: serde_json::Value,
            key: &serde_json::Value,
        ) -> serde_json::Value {
            let mut bound = json!({ self.vocabulary.handle: handle(key) });
            bound
                .as_object_mut()
                .unwrap()
                .extend(args.as_object().expect("an args object").clone());
            self.call(json!({"call": "call", "action": action, "args": bound}))
        }
    }

    /// The handle afkd passes for the unit `key` names. The plugin reads only its key; the
    /// id is the item's number, as `poll` handed it over.
    fn handle(key: &serde_json::Value) -> serde_json::Value {
        let key = key.as_str().expect("a unit key");
        json!({"id": key.split('#').nth(1), "key": key})
    }

    /// The `hello` afkd writes for `kind` over `settings`.
    fn hello(kind: &str, settings: serde_json::Value) -> serde_json::Value {
        json!({"call": "hello", "proto": 2, "kind": kind, "service": "監視::triage",
               "roster": ["監視::triage"], "owner": "陳大文", "settings": settings})
    }

    fn settings() -> serde_json::Value {
        json!({"repo": REPO, "token": "PAT", "source_label": "afkd/ready"})
    }

    /// A `pr` block over the bot's own PRs, the `bool` lowered to its word.
    fn pr_settings() -> serde_json::Value {
        json!({"repo": REPO, "token": "PAT", "author_me": "true"})
    }

    /// A claimed issue #9 a crashed run left behind: the status label, the bot and a human
    /// assigned, and the bot's marker — the journal key names it.
    fn crashed_claim(mock: &MockClient) -> String {
        mock.add_issue_assigned(
            9,
            "Crashed mid-run",
            "",
            &["afkd/ready", "afkd/claimed"],
            &["陳大文", ME],
        );
        mock.add_comment_body(9, 6744, ME, &claim_text(ME), 900);
        claim_key(REPO, 9, 6744)
    }

    fn markers_on(mock: &MockClient, number: u64) -> usize {
        let repo = Repo::parse(REPO).unwrap();
        mock.list_issue_comments(&repo, number)
            .unwrap()
            .iter()
            .filter(|c| crate::claim::is_claim(&c.body))
            .count()
    }

    /// The accepted `hello` lists exactly the optional calls the kind answers — `call` is
    /// proto 2's own and is never listed — and reads the identity, the token's login
    /// read once, under the call budget.
    #[test]
    fn hello_lists_every_optional_call_and_reads_the_identity() {
        let mut f = Fixture::new();
        let reply = f.call(hello(ISSUE_KIND, settings()));
        assert_eq!(
            reply,
            json!({"ok": true, "proto": 2, "calls": ["release", "renew", "comments"]})
        );
        assert!(f.diag.lines().is_empty(), "{:?}", f.diag.lines());
        assert_eq!(f.mock.user_reads(), 1, "hello reads the identity");
        let now = f.clock.now();
        assert_eq!(f.mock.call_deadlines(), [Some(now + CALL_BUDGET), None]);
        assert!(f.mock.actions().is_empty(), "hello writes nothing");
    }
    /// The PR kind lists the same three calls — no `classify`, since a PR never parks —
    /// and refuses the one it does not list.
    #[test]
    fn pr_hello_lists_exactly_the_calls_the_kind_answers() {
        let mut f = Fixture::new();
        let reply = f.call(hello(PR_KIND, pr_settings()));
        assert_eq!(
            reply,
            json!({"ok": true, "proto": 2, "calls": ["release", "renew", "comments"]})
        );
        assert!(f.diag.lines().is_empty(), "{:?}", f.diag.lines());
        assert_eq!(
            f.call(json!({"call": "classify", "key": "k", "scratch": "/tmp/s",
                          "outcome": "failed"})),
            json!({"ok": false})
        );
    }

    /// Each refusal is `ok:false` with the problem, in the built-in's own words, on the
    /// diagnostic channel — and leaves the plugin unarmed.
    #[test]
    fn hello_refuses_what_the_kind_cannot_arm_with() {
        for (kind, proto, settings, problem) in [
            (
                "github_issues",
                2,
                settings(),
                "kind `github_issues` is not provided by @afkd/github",
            ),
            (
                "github_issue",
                2,
                settings(),
                "kind `github_issue` is not provided by @afkd/github",
            ),
            (
                ISSUE_KIND,
                1,
                settings(),
                "afkd speaks plugin protocol 1, and this plugin speaks 2",
            ),
            (
                ISSUE_KIND,
                2,
                json!({"repo": "", "token": "PAT"}),
                "trigger issue: setting `repo`: a github trigger needs a `repo` (`owner/name`)",
            ),
            (
                PR_KIND,
                2,
                json!({"repo": "", "token": "PAT", "author_me": "true"}),
                "trigger pr: setting `repo`: a github trigger needs a `repo` (`owner/name`)",
            ),
        ] {
            let mut f = Fixture::new();
            let mut request = hello(kind, settings);
            request["proto"] = json!(proto);
            assert_eq!(f.call(request), json!({"ok": false, "proto": 2}));
            assert_eq!(f.diag.lines(), [problem]);
            assert_eq!(f.mock.user_reads(), 0, "a refused hello reads no forge");
            assert!(matches!(
                f.plugin.answer(Request::Poll),
                Answer::Fatal(reason) if reason.contains("before a `hello`")
            ));
        }
    }

    /// A forge failure on `poll` is the built-in's idle beat, diagnosed; the next beat,
    /// with the forge back, claims.
    #[test]
    fn a_poll_forge_error_is_an_idle_beat_and_logged() {
        let mut f = Fixture::armed(settings());
        f.mock.add_issue(7, "Fix", "do it", &["afkd/ready"]);
        f.mock.fail("list issues");
        assert_eq!(f.poll(), json!({"fire": false}));
        assert_eq!(
            f.diag.lines(),
            ["github list issues: no response (mock failure)"]
        );
        f.mock.clear_failure();
        assert_eq!(f.poll()["fire"], true);
    }

    /// A forge that cannot say who the token is at `hello` does not refuse the service over
    /// a blip: the kind is armed, the reply is the same, and the failure is
    /// diagnosed. Each poll asks again, idle until it can, and once resolved the identity
    /// is kept: the forge is not asked again.
    #[test]
    fn a_hello_whose_identity_read_fails_is_still_accepted() {
        let mut f = Fixture::new();
        f.mock.add_issue(7, "Fix", "do it", &["afkd/ready"]);
        f.mock.add_issue(8, "Fix too", "do it", &["afkd/ready"]);
        f.mock.fail("current user");
        assert_eq!(
            f.call(hello(ISSUE_KIND, settings())),
            json!({"ok": true, "proto": 2, "calls": ["release", "renew", "comments"]})
        );
        assert_eq!(f.poll(), json!({"fire": false}));
        assert_eq!(
            f.diag.lines(),
            ["github current user: no response (mock failure)"; 2]
        );
        f.mock.clear_failure();
        assert_eq!(f.poll()["unit"]["self"], ME);
        assert_eq!(f.poll()["unit"]["id"], "8");
        assert_eq!(f.mock.user_reads(), 3, "two failed asks, one that stuck");
    }
    /// `classify` and `attempt_failed` are calls the kind does not list, so each is
    /// refused exactly as an unknown call is, and the next request is still answered.
    #[test]
    fn the_calls_it_never_listed_are_refused() {
        let mut f = Fixture::armed(settings());
        for request in [
            json!({"call": "classify", "key": "k", "scratch": "/tmp/s", "outcome": "failed"}),
            json!({"call": "attempt_failed", "key": "k", "n": 1, "max": 2, "reason": null}),
        ] {
            assert_eq!(f.call(request), json!({"ok": false}));
        }
        assert_eq!(
            f.diag.lines(),
            ["afkd sent a call this plugin did not list in its `hello` reply"; 2]
        );
        assert_eq!(f.poll(), json!({"fire": false}));
    }

    /// The `comments` reply maps each comment the way afkd's watch reads it — the decimal
    /// id, the login as both author fields, the body verbatim, and the creation stamp as
    /// RFC 3339 — and leaves out afkd's own comments and every claim marker.
    #[test]
    fn a_comments_reply_maps_id_author_body_and_created_at_and_omits_markers_and_self() {
        let mut f = Fixture::armed(settings());
        f.mock.add_issue(7, "Fix", "do it", &["afkd/ready"]);
        let key = f.poll()["unit"]["key"].clone();
        // Created 2026-09-25T09:58:00Z, edited later: `at` is the creation.
        f.mock.add_comment_edited(
            7,
            41,
            "陳大文",
            "看起来不对 🚨\n\n```rust\nlet x = 1;\n```",
            1_790_330_280,
            1_790_340_000,
        );
        f.mock
            .add_comment_body(7, 42, ME, "Working on it.", 1_790_330_300);
        f.mock.add_comment_body(
            7,
            43,
            "rival[bot]",
            &claim_text("rival[bot]"),
            1_790_330_310,
        );
        f.mock.add_comment_body(7, 44, "álvaro", "", 1_790_330_320);

        let reply = f.call(json!({"call": "comments", "key": key}));
        assert_eq!(
            reply,
            json!({"comments": [
                {"id": "41", "author": "陳大文", "author_name": "陳大文",
                 "body": "看起来不对 🚨\n\n```rust\nlet x = 1;\n```", "at": "2026-09-25T09:58:00Z"},
                {"id": "44", "author": "álvaro", "author_name": "álvaro",
                 "body": "", "at": "2026-09-25T09:58:40Z"},
            ]})
        );

        // A second read returns only what is newer than what was reported.
        f.mock
            .add_comment_body(7, 45, "álvaro", "Also the flag.", 1_790_330_400);
        let reply = f.call(json!({"call": "comments", "key": key}));
        assert_eq!(reply["comments"].as_array().unwrap().len(), 1);
        assert_eq!(reply["comments"][0]["id"], "45");
    }

    /// The issue claim reads no comments, so `seen` is empty and the first read hands
    /// afkd the whole thread — the baseline its watch records rather than delivers.
    #[test]
    fn an_unseeded_first_read_returns_the_whole_baseline() {
        let mut f = Fixture::armed(settings());
        f.mock.add_issue(7, "Fix", "do it", &["afkd/ready"]);
        f.mock
            .add_comment_body(7, 41, "álvaro", "the original ask", 100);
        let unit = f.poll()["unit"].clone();
        assert_eq!(unit["seen"], json!([]));
        let reply = f.call(json!({"call": "comments", "key": unit["key"]}));
        assert_eq!(reply["comments"][0]["id"], "41");
    }

    #[test]
    fn an_unreachable_forge_or_an_unknown_key_is_null() {
        let mut f = Fixture::armed(settings());
        f.mock.add_issue(7, "Fix", "do it", &["afkd/ready"]);
        let key = f.poll()["unit"]["key"].clone();
        f.mock.fail("list comments");
        assert_eq!(
            f.call(json!({"call": "comments", "key": key})),
            json!({"comments": null})
        );
        assert_eq!(
            f.call(json!({"call": "comments", "key": "acme/widgets#9#1"})),
            json!({"comments": null})
        );
        assert_eq!(
            f.diag.lines(),
            [
                "github list comments: no response (mock failure)",
                "comments for acme/widgets#9#1, which this plugin holds no claim on",
            ]
        );
    }

    /// A thread too long for one line keeps the newest comments that fit, and names the
    /// ones it left out; they are not offered again.
    #[test]
    fn an_overflowing_thread_keeps_the_newest_and_names_the_rest() {
        let mut f = Fixture::armed(settings());
        f.mock.add_issue(7, "Fix", "do it", &["afkd/ready"]);
        let key = f.poll()["unit"]["key"].clone();
        for id in 1..=400 {
            f.mock
                .add_comment_body(7, id, "álvaro", &"ø".repeat(150), 1_000 + id);
        }
        let reply = f.call(json!({"call": "comments", "key": key}));
        let kept = reply["comments"].as_array().unwrap();
        assert!(kept.len() < 400);
        assert_eq!(kept.last().unwrap()["id"], "400");
        let dropped = 400 - kept.len();
        let lines = f.diag.lines();
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].starts_with(&format!("{dropped} comments on acme/widgets#7 did not fit")),
            "{}",
            lines[0]
        );
        assert!(lines[0].ends_with(&format!(", {dropped}")), "{}", lines[0]);
        assert_eq!(
            f.call(json!({"call": "comments", "key": key})),
            json!({"comments": []})
        );
    }

    /// A `release` drops a live unit, so nothing later can act on it; a key that is not a
    /// claim key names nothing (`null`); and one whose repo half is not `owner/name` is
    /// kept for a later try (`false`).
    #[test]
    fn a_release_forgets_a_live_unit() {
        let mut f = Fixture::armed(settings());
        f.mock.add_issue(7, "Fix", "do it", &["afkd/ready"]);
        let key = f.poll()["unit"]["key"].clone();
        assert_eq!(
            f.call(json!({"call": "renew", "key": key, "renewal": 1})),
            json!({"ok": true})
        );
        assert_eq!(
            f.call(json!({"call": "release", "key": key})),
            json!({"released": true})
        );
        assert!(!f.mock.has_label(7, "afkd/claimed"));
        assert_eq!(markers_on(&f.mock, 7), 0);
        assert_eq!(
            f.call(json!({"call": "renew", "key": key, "renewal": 2})),
            json!({"ok": false})
        );
        for key in ["acme/widgets#9", "garbage"] {
            assert_eq!(
                f.call(json!({"call": "release", "key": key})),
                json!({"released": null})
            );
        }
        assert_eq!(
            f.call(json!({"call": "release", "key": "not-a-repo#9#1"})),
            json!({"released": false})
        );
    }

    /// afkd's reaper runs before its poll, so a fresh child's first call after `hello` can
    /// be `release` of a crashed run's key. The identity `hello` resolved serves it, and
    /// the release is the whole of the built-in's: the status label, the bot's assignment
    /// (and not the human's), and the marker. Nothing asks for the identity again.
    #[test]
    fn release_before_any_poll_releases_in_full_as_the_hello_identity() {
        let mut f = Fixture::armed(settings());
        let key = crashed_claim(&f.mock);

        assert_eq!(
            f.call(json!({"call": "release", "key": key})),
            json!({"released": true})
        );
        assert!(!f.mock.has_label(9, "afkd/claimed"));
        assert!(f.mock.has_label(9, "afkd/ready"));
        assert_eq!(f.mock.assignees_of(9), vec!["陳大文".to_string()]);
        assert_eq!(markers_on(&f.mock, 9), 0);
        assert_eq!(f.mock.user_reads(), 1);

        assert_eq!(f.poll()["unit"]["id"], "9", "released, so claimable again");
        assert_eq!(f.mock.user_reads(), 1, "the identity is kept");
    }

    /// With no identity, a `release` writes nothing and keeps the key (`false`) — even a
    /// key that names nothing, which is only judged once the identity is known. Healed,
    /// the same key releases.
    #[test]
    fn release_without_an_identity_keeps_the_key() {
        let mut f = Fixture::new();
        let key = crashed_claim(&f.mock);
        f.mock.fail("current user");
        assert_eq!(
            f.call(hello(ISSUE_KIND, settings()))["ok"],
            true,
            "the hello is accepted without an identity"
        );

        assert_eq!(
            f.call(json!({"call": "release", "key": key})),
            json!({"released": false})
        );
        assert_eq!(
            f.call(json!({"call": "release", "key": "garbage"})),
            json!({"released": false})
        );
        assert_eq!(
            f.diag.lines(),
            ["github current user: no response (mock failure)"; 3]
        );
        // No write at all: the release's first step is the label removal, and the mock
        // records every write.
        assert_eq!(f.mock.actions(), []);
        assert!(f.mock.has_label(9, "afkd/claimed"));

        f.mock.clear_failure();
        assert_eq!(
            f.call(json!({"call": "release", "key": key})),
            json!({"released": true})
        );
        assert_eq!(f.mock.assignees_of(9), vec!["陳大文".to_string()]);
    }

    /// Nothing a finish does can leave it undelivered, so nothing is `held`: a marker
    /// delete the forge refused is said lost, and the finish still answers plain `ok` —
    /// the marker ages out, and nothing is queued.
    #[test]
    fn a_finish_whose_marker_delete_fails_still_answers_plain_ok_diagnosed() {
        let mut f = Fixture::armed(settings());
        f.mock.add_issue(7, "Fix", "do it", &["afkd/ready"]);
        let key = f.poll()["unit"]["key"].clone();
        f.mock.refuse("delete comment", 403);
        assert_eq!(f.finish(&key, "clean"), json!({"ok": true}));
        assert_eq!(
            f.diag.lines(),
            [
                "issue \"Fix\" (acme/widgets#7): deleting the claim marker is lost — github \
                 delete comment: forge returned status 403"
            ]
        );
        assert_eq!(markers_on(&f.mock, 7), 1, "left to age out");
        assert_eq!(f.deliver(), None, "nothing queued");
    }
    /// A delivered finish is plain `ok`, and `finish` for a key the plugin never handed
    /// over is diagnosed and still `ok`, touching nothing.
    #[test]
    fn a_delivered_finish_is_ok_and_an_unknown_key_is_ok_and_diagnosed() {
        let mut f = Fixture::armed(settings());
        f.mock.add_issue(7, "Fix", "do it", &["afkd/ready"]);
        let key = f.poll()["unit"]["key"].clone();
        assert_eq!(f.finish(&key, "clean"), json!({"ok": true}));
        assert!(f.diag.lines().is_empty(), "{:?}", f.diag.lines());

        // The finished unit is no longer live: a second `finish` for it releases nothing
        // again.
        let before = f.mock.actions();
        assert_eq!(
            before
                .iter()
                .filter(|a| matches!(a, Action::DeleteComment { .. }))
                .count(),
            1
        );
        assert_eq!(f.finish(&key, "clean"), json!({"ok": true}));
        assert_eq!(f.mock.actions(), before);
        assert_eq!(
            f.diag.lines(),
            [format!(
                "finish for {}, which this plugin holds no claim on",
                key.as_str().unwrap()
            )]
        );
    }

    /// An oversized brief is cut to fit one line, with one diagnostic naming the thread.
    #[test]
    fn an_oversized_brief_is_cut_and_said_so() {
        let mut f = Fixture::armed(settings());
        f.mock.add_issue(
            7,
            "修复 the retry storm 🚨",
            &"看起来不对 🚨 — the retry path.\n".repeat(2_600),
            &["afkd/ready"],
        );
        let reply = f.poll();
        assert_eq!(reply["fire"], true);
        let brief = reply["unit"]["files"][0]["text"].as_str().unwrap();
        assert!(
            brief.ends_with("read the whole issue with the github skill]"),
            "{}",
            &brief[brief.len() - 120..]
        );
        assert_eq!(
            f.diag.lines(),
            ["the brief for acme/widgets#7 was cut to fit afkd's 64 KiB plugin line"]
        );
    }

    /// Every armed call hands the forge a deadline [`CALL_BUDGET`] from the clock's now
    /// before it runs, and clears it once the reply is built — `hello` too, for the read
    /// of `me`; an unknown call makes no forge request and hands it none. Each call runs at
    /// its own `now`, so a deadline inherited from the call before would show.
    #[test]
    fn every_armed_call_runs_under_the_call_budget_and_clears_it() {
        let mut f = Fixture::armed(settings());
        f.mock.add_issue(
            7,
            "Fix the 修复 path — 🚨",
            "Steps:\n1. run it\n2. ship it",
            &["afkd/ready"],
        );
        assert_eq!(f.mock.call_deadlines().len(), 2, "hello's read of `me`");

        let mut key = serde_json::Value::Null;
        for call in ["poll", "renew", "comments", "finish", "call", "release"] {
            f.clock.advance(Duration::from_secs(3));
            let now = f.clock.now();
            let reply = match call {
                "poll" => f.poll(),
                "renew" => f.call(json!({"call": "renew", "key": key, "renewal": 1})),
                "comments" => f.call(json!({"call": "comments", "key": key})),
                "finish" => f.finish(&key, "clean"),
                "call" => f.act("close", json!({}), &key),
                _ => f.call(json!({"call": "release", "key": key})),
            };
            if call == "poll" {
                assert_eq!(reply["fire"], true, "{:?}", f.diag.lines());
                key = reply["unit"]["key"].clone();
            }
            let handed = f.mock.call_deadlines();
            assert_eq!(
                handed[handed.len() - 2..],
                [Some(now + CALL_BUDGET), None],
                "`{call}` runs under its own deadline and clears it"
            );
        }
        assert_eq!(
            f.mock.call_deadlines().len(),
            14,
            "one set and one clear each"
        );

        f.call(json!({"call": "rewind", "key": "k"}));
        assert_eq!(
            f.mock.call_deadlines().len(),
            14,
            "an unknown call hands none"
        );
    }

    /// `poll` and `finish` perform no hook action: the claim writes only the marker and
    /// the status label, and the finish only releases the marker.
    #[test]
    fn poll_and_finish_perform_no_hook_actions() {
        let mut f = Fixture::armed(settings());
        f.mock.add_issue(7, "Fix", "do it", &["afkd/ready"]);
        let key = f.poll()["unit"]["key"].clone();
        let claimed = f.mock.actions();
        assert!(
            claimed.iter().all(|a| match a {
                Action::Comment { body, .. } => crate::claim::is_claim(body),
                Action::Label { name, .. } => name == "afkd/claimed",
                _ => false,
            }),
            "the claim ran no hook action: {claimed:?}"
        );
        assert_eq!(f.finish(&key, "clean"), json!({"ok": true}));
        assert!(matches!(
            f.mock.actions()[claimed.len()..],
            [Action::DeleteComment { .. }]
        ));
        assert!(f.diag.lines().is_empty(), "{:?}", f.diag.lines());
    }

    /// Each of the six actions, as afkd sends a hook's call of it on the live unit, does
    /// its one forge operation on the claimed issue as its claim identity and answers
    /// `ok` — a slashed emoji label byte for byte, and a comment's `#{run.x}` left as afkd
    /// sent it.
    #[test]
    fn every_action_over_call_acts_on_the_live_unit() {
        let mut f = Fixture::armed(settings());
        f.mock
            .add_issue_assigned(7, "Fix", "do it", &["afkd/ready"], &["陳大文"]);
        let key = f.poll()["unit"]["key"].clone();
        let before = f.mock.actions().len();
        let markdown = "## 完了 ✅\n\n- took 3m 12s\n- log: #{run.x}\n\nsee the run log.";
        for (action, args) in [
            ("assign_me", json!({})),
            ("label_add", json!({"label": "afkd/reviewed ✅"})),
            ("label_remove", json!({"label": "afkd/claimed"})),
            ("comment", json!({"text": markdown})),
            ("unassign", json!({})),
            ("close", json!({})),
        ] {
            assert_eq!(f.act(action, args, &key), json!({"ok": true}), "{action}");
        }
        assert!(f.diag.lines().is_empty(), "{:?}", f.diag.lines());
        let me = || vec![ME.to_string()];
        assert_eq!(
            f.mock.actions()[before..],
            [
                Action::AddAssignees {
                    index: 7,
                    assignees: me()
                },
                Action::Label {
                    index: 7,
                    name: "afkd/reviewed ✅".into()
                },
                Action::Unlabel {
                    index: 7,
                    name: "afkd/claimed".into()
                },
                Action::Comment {
                    index: 7,
                    body: markdown.into()
                },
                Action::RemoveAssignees {
                    index: 7,
                    assignees: me()
                },
                Action::State {
                    index: 7,
                    state: "closed".into()
                },
            ]
        );
        assert_eq!(f.mock.assignees_of(7), vec!["陳大文".to_string()]);
    }

    /// The post-run hook's calls, which afkd sends after `finish`, still reach the
    /// finished unit — until afkd releases the key, after which a call names no claim.
    #[test]
    fn a_post_run_call_acts_on_the_finished_unit_until_it_is_released() {
        let mut f = Fixture::armed(settings());
        f.mock.add_issue(7, "Fix", "do it", &["afkd/ready"]);
        let key = f.poll()["unit"]["key"].clone();
        assert_eq!(f.finish(&key, "clean"), json!({"ok": true}));
        let closed = |f: &Fixture| {
            f.mock
                .actions()
                .iter()
                .any(|a| matches!(a, Action::State { state, .. } if state == "closed"))
        };
        assert!(
            !closed(&f),
            "the finish closed nothing: that is `on_done`'s"
        );

        assert_eq!(f.act("close", json!({}), &key), json!({"ok": true}));
        assert!(closed(&f));

        assert_eq!(
            f.call(json!({"call": "release", "key": key})),
            json!({"released": true})
        );
        let released = f.diag.lines().len();
        assert_eq!(
            f.act("comment", json!({"text": "late"}), &key),
            json!({"ok": false, "error": format!(
                "comment for {}, which this plugin holds no claim on",
                key.as_str().unwrap()
            )})
        );
        assert_eq!(
            f.diag.lines().len(),
            released + 1,
            "the refusal is diagnosed"
        );
    }

    /// The finished units a post-run call reaches are bounded: past [`FINISHED_MAX`] the
    /// oldest is forgotten, and the newest still answer.
    #[test]
    fn the_finished_units_are_bounded_oldest_first() {
        let mut f = Fixture::armed(settings());
        let issues = FINISHED_MAX as u64 + 1;
        for n in 1..=issues {
            f.mock
                .add_issue(n, &format!("修复 {n}"), "do it", &["afkd/ready"]);
        }
        let keys: Vec<serde_json::Value> = (1..=issues)
            .map(|_| {
                let key = f.poll()["unit"]["key"].clone();
                assert_eq!(f.finish(&key, "clean"), json!({"ok": true}));
                key
            })
            .collect();
        let comment = json!({"text": "see the run log"});
        assert_eq!(f.act("comment", comment.clone(), &keys[0])["ok"], false);
        for key in &keys[1..] {
            assert_eq!(f.act("comment", comment.clone(), key), json!({"ok": true}));
        }
    }

    /// Each call the plugin cannot do answers `ok:false` with its sentence — which afkd
    /// fails the slot's `result` with — and says it on the diagnostic channel too: an
    /// action afkd would not bind, a handle naming no claim, and a forge that refused it.
    /// None of them is queued.
    #[test]
    fn a_call_the_plugin_cannot_do_answers_its_sentence() {
        let mut f = Fixture::armed(settings());
        f.mock.add_issue(7, "Fix", "do it", &["afkd/ready"]);
        let key = f.poll()["unit"]["key"].clone();
        let before = f.mock.actions().len();
        let mut expect = Vec::new();
        for (action, args, key, error) in [
            ("reopen", json!({}), key.clone(), "no action `reopen`"),
            (
                "label_remove",
                json!({}),
                key.clone(),
                "label_remove: parameter label is required",
            ),
            (
                "comment",
                json!({"text": ["a", "b"]}),
                key.clone(),
                "comment: parameter text must be a string",
            ),
            (
                "close",
                json!({}),
                json!("acme/widgets#9#1"),
                "close for acme/widgets#9#1, which this plugin holds no claim on",
            ),
        ] {
            assert_eq!(
                f.act(action, args, &key),
                json!({"ok": false, "error": error}),
                "{action}"
            );
            expect.push(error.to_string());
        }
        f.mock.refuse("set state", 403);
        assert_eq!(
            f.act("close", json!({}), &key),
            json!({"ok": false, "error": "github set state: forge returned status 403"})
        );
        expect.push("github set state: forge returned status 403".into());
        assert_eq!(f.diag.lines(), expect);
        assert_eq!(f.mock.actions().len(), before, "nothing landed");
        assert_eq!(f.deliver(), None, "nothing queued");
    }

    /// The PR kind answers `call` too, under its own `pr_` names, on the claimed pull request.
    #[test]
    fn a_pr_call_acts_on_the_claimed_pull_request() {
        let mut f = Fixture::armed_as(PR_KIND, pr_settings());
        f.mock.add_pull(7, ME, "feature/retry-backoff");
        f.mock.add_comment(7, 41, "陳大文", 1_790_330_000);
        let key = f.poll()["unit"]["key"].clone();
        assert_eq!(f.finish(&key, "clean"), json!({"ok": true}));
        assert_eq!(
            f.act("pr_comment", json!({"text": "round done ✅"}), &key),
            json!({"ok": true})
        );
        assert!(f
            .mock
            .actions()
            .iter()
            .any(|a| matches!(a, Action::Comment { index: 7, body } if body == "round done ✅")));
    }

    /// A call acts on the item its handle names, never on an implied claim: with one issue
    /// finished and the next one claimed and live, an `on_done`'s comment on the finished
    /// issue lands there — even beside a stray top-level `key` naming the live one, which
    /// the request no longer carries and the plugin does not read.
    #[test]
    fn a_call_acts_on_the_item_its_handle_names() {
        let mut f = Fixture::armed(settings());
        f.mock
            .add_issue(7, "修复 the retry storm 🚨", "do it", &["afkd/ready"]);
        f.mock
            .add_issue(8, "第二の課題 🚧", "and this", &["afkd/ready"]);
        let first = f.poll()["unit"]["key"].clone();
        assert_eq!(f.finish(&first, "clean"), json!({"ok": true}));
        let second = f.poll()["unit"]["key"].clone();
        assert_ne!(first, second, "the next issue is claimed");
        let before = f.mock.actions().len();

        let text = "afkd landed this issue in 3m 12s.\n\n— 完了 ✅";
        assert_eq!(
            f.call(json!({"call": "call", "action": "comment",
                          "args": {"issue": handle(&first), "text": text},
                          "key": second})),
            json!({"ok": true})
        );
        let commented: Vec<u64> = f.mock.actions()[before..]
            .iter()
            .filter_map(|a| match a {
                Action::Comment { index, body } if body == text => Some(*index),
                _ => None,
            })
            .collect();
        assert_eq!(commented, [7], "the finished issue, not the live one");
        assert!(f.diag.lines().is_empty(), "{:?}", f.diag.lines());
    }

    /// A call whose item handle is missing, is not a handle, names a key this plugin never
    /// handed over, or is the other kind's answers `ok:false` with its sentence, diagnosed,
    /// and touches nothing on the forge — even with a live claim it could have guessed at.
    #[test]
    fn a_call_without_a_usable_handle_is_refused() {
        let mut f = Fixture::armed(settings());
        f.mock.add_issue(7, "Fix", "do it", &["afkd/ready"]);
        let key = f.poll()["unit"]["key"].clone();
        let before = f.mock.actions().len();
        let mut expect = Vec::new();
        for (action, args, error) in [
            (
                "comment",
                json!({"text": "hej"}),
                "comment: parameter issue is required",
            ),
            (
                "comment",
                json!({"issue": key, "text": "hej"}),
                "comment: parameter issue must be an item handle",
            ),
            (
                "comment",
                json!({"issue": {"id": "7"}, "text": "hej"}),
                "comment: parameter issue must be an item handle",
            ),
            (
                "comment",
                json!({"issue": {"id": "9", "key": "acme/widgets#9#1"}, "text": "hej"}),
                "comment for acme/widgets#9#1, which this plugin holds no claim on",
            ),
            (
                "comment",
                json!({"pr": handle(&key), "text": "hej"}),
                "comment: parameter issue is required",
            ),
            (
                "pr_comment",
                json!({"pr": handle(&key), "text": "hej"}),
                "no action `pr_comment`",
            ),
        ] {
            assert_eq!(
                f.call(json!({"call": "call", "action": action, "args": args, "key": key})),
                json!({"ok": false, "error": error}),
                "{action} {args}"
            );
            expect.push(error.to_string());
        }
        assert_eq!(f.diag.lines(), expect);
        assert_eq!(f.mock.actions().len(), before, "nothing landed");
    }

    /// A `call` before an accepted `hello` is afkd out of step, like any other call.
    #[test]
    fn a_call_before_hello_is_fatal() {
        let mut f = Fixture::new();
        let request = serde_json::from_value(json!({"call": "call", "action": "close",
                   "args": {"issue": {"id": "7", "key": "acme/widgets#7#1"}}}))
        .unwrap();
        assert!(matches!(
            f.plugin.answer(request),
            Answer::Fatal(reason) if reason.contains("before a `hello`")
        ));
    }

    /// The issue the outage tests run: the delimiter a log line wraps it in, wide CJK, an
    /// emoji and an em dash.
    const TITLE: &str = "Fix \"the\" café — 修复 🚨";

    /// What an `on_fail` comments: multi-line, with a fence.
    const FAILED_NOTE: &str = "attempts spent: run did not complete\n```\nexit 128\n```";

    /// A plugin armed over issue 7, claimed by a poll and assigned to the bot by its
    /// `on_claim`, beside a human already assigned; and the unit's key.
    fn claimed_issue() -> (Fixture, serde_json::Value) {
        let mut f = Fixture::armed(settings());
        f.mock
            .add_issue_assigned(7, TITLE, "do it", &["afkd/ready"], &["陳大文"]);
        let key = f.poll()["unit"]["key"].clone();
        assert_eq!(f.act("assign_me", json!({}), &key), json!({"ok": true}));
        (f, key)
    }

    /// The claim marker the unit `key` names was won on.
    fn marker(key: &serde_json::Value) -> u64 {
        key.as_str()
            .unwrap()
            .rsplit('#')
            .next()
            .unwrap()
            .parse()
            .unwrap()
    }

    /// A call the forge cannot be reached for answers `ok`, so the slot's next action
    /// still runs; that one queues behind it, each said once; and both land, in order,
    /// once the forge is back.
    #[test]
    fn a_call_the_forge_cannot_be_reached_for_answers_ok_and_lands_later() {
        let (mut f, key) = claimed_issue();
        let before = f.mock.actions().len();
        f.mock.go_down();
        assert_eq!(
            f.act("label_add", json!({"label": "needs/human 🚧"}), &key),
            json!({"ok": true})
        );
        assert_eq!(
            f.act("comment", json!({"text": FAILED_NOTE}), &key),
            json!({"ok": true})
        );
        assert_eq!(
            f.diag.lines(),
            [
                format!(
                    "github is unreachable for issue \"{TITLE}\" (acme/widgets#7): github add \
                     label: no response (mock failure); adding the label \"needs/human 🚧\" is \
                     queued and retried for up to 1h"
                ),
                format!(
                    "github still owes issue \"{TITLE}\" (acme/widgets#7) earlier writes; the \
                     comment \"attempts spent: run did not complete…\" is queued behind them"
                ),
            ]
        );
        assert_eq!(f.mock.actions().len(), before, "nothing landed yet");

        f.clock.advance(RETRY_FIRST);
        assert!(f.deliver().is_some(), "still down: tried again later");
        f.mock.clear_failure();
        f.drain();
        assert_eq!(
            f.mock.actions()[before..],
            [
                Action::Label {
                    index: 7,
                    name: "needs/human 🚧".into(),
                },
                Action::Comment {
                    index: 7,
                    body: FAILED_NOTE.into(),
                },
            ]
        );
    }

    /// The run-end release and the whole `on_fail` slot through an outage: `finish` and
    /// every call answer `ok`, a few retries fail, and once the forge is back one pass
    /// lands the marker's delete and then the slot's actions in the order they were
    /// called. The issue ends in its `on_fail` state with the human still assigned, and
    /// nothing was given up.
    #[test]
    fn a_github_outage_through_the_failure_path_still_lands_the_failed_state() {
        let (mut f, key) = claimed_issue();
        let before = f.mock.actions().len();
        f.mock.go_down();
        assert_eq!(f.finish(&key, "failed"), json!({"ok": true}));
        for (action, args) in [
            ("label_remove", json!({"label": "afkd/ready"})),
            ("unassign", json!({})),
            ("label_add", json!({"label": "needs/human 🚧"})),
            ("comment", json!({"text": FAILED_NOTE})),
            ("label_remove", json!({"label": "afkd/claimed"})),
        ] {
            assert_eq!(f.act(action, args, &key), json!({"ok": true}), "{action}");
        }
        for _ in 0..3 {
            let next = f.deliver().expect("still owed");
            f.clock.advance(next - f.clock.now());
        }
        assert_eq!(f.mock.actions().len(), before, "nothing landed while down");

        f.mock.clear_failure();
        assert_eq!(f.deliver(), None, "one pass lands the whole backlog");
        assert_eq!(
            f.mock.actions()[before..],
            [
                Action::DeleteComment { id: marker(&key) },
                Action::Unlabel {
                    index: 7,
                    name: "afkd/ready".into(),
                },
                Action::RemoveAssignees {
                    index: 7,
                    assignees: vec![ME.into()],
                },
                Action::Label {
                    index: 7,
                    name: "needs/human 🚧".into(),
                },
                Action::Comment {
                    index: 7,
                    body: FAILED_NOTE.into(),
                },
                Action::Unlabel {
                    index: 7,
                    name: "afkd/claimed".into(),
                },
            ]
        );
        assert!(!f.mock.has_label(7, "afkd/ready"));
        assert!(!f.mock.has_label(7, "afkd/claimed"));
        assert!(f.mock.has_label(7, "needs/human 🚧"));
        assert_eq!(f.mock.assignees_of(7), ["陳大文"]);
        assert_eq!(markers_on(&f.mock, 7), 0);
        let lines = f.diag.lines();
        assert!(lines.iter().all(|l| !l.contains("gave up")), "{lines:?}");
        assert_eq!(
            lines
                .iter()
                .filter(|l| l.starts_with("github is back for"))
                .count(),
            6,
            "{lines:?}"
        );
    }

    /// GitHub out for the whole hour: every write the run's end and its `on_fail` owed is
    /// given up, one loud line each naming the issue and what is lost, in the order they
    /// were asked for — and nothing landed.
    #[test]
    fn an_outage_past_the_hour_gives_up_every_owed_write_loudly() {
        let (mut f, key) = claimed_issue();
        let before = f.mock.actions().len();
        f.mock.go_down();
        assert_eq!(f.finish(&key, "failed"), json!({"ok": true}));
        for (action, args) in [
            ("label_add", json!({"label": "needs/human 🚧"})),
            ("comment", json!({"text": FAILED_NOTE})),
            ("unassign", json!({})),
        ] {
            assert_eq!(f.act(action, args, &key), json!({"ok": true}), "{action}");
        }
        f.drain();
        let gave_up: Vec<String> = f
            .diag
            .lines()
            .into_iter()
            .filter(|l| l.starts_with("gave up"))
            .collect();
        let item = format!("issue \"{TITLE}\" (acme/widgets#7)");
        assert_eq!(
            gave_up,
            [
                format!(
                    "gave up on {item} after 1h: deleting the claim marker is lost — github \
                     delete comment: no response (mock failure)"
                ),
                format!(
                    "gave up on {item} after 1h: adding the label \"needs/human 🚧\" is lost \
                     — github add label: no response (mock failure)"
                ),
                format!(
                    "gave up on {item} after 1h: the comment \"attempts spent: run did not \
                     complete…\" is lost — github post comment: no response (mock failure)"
                ),
                format!(
                    "gave up on {item} after 1h: unassigning the bot is lost — github remove \
                     assignees: no response (mock failure)"
                ),
            ]
        );
        assert_eq!(f.mock.actions().len(), before, "nothing landed");
    }

    /// The PR kind's round end through an outage: the marker's delete is queued, the
    /// `on_done`'s calls queue behind it under the pull request's own name, and all of it
    /// lands in order once GitHub is back.
    #[test]
    fn a_pr_round_end_during_an_outage_lands_when_github_returns() {
        let mut f = Fixture::armed_as(PR_KIND, pr_settings());
        f.mock.add_pull(7, ME, "feature/retry-backoff");
        f.mock.add_comment(7, 41, "陳大文", 1_790_330_000);
        let key = f.poll()["unit"]["key"].clone();
        let before = f.mock.actions().len();
        f.mock.go_down();
        assert_eq!(f.finish(&key, "clean"), json!({"ok": true}));
        for (action, args) in [
            ("pr_comment", json!({"text": "round done ✅\n\n- 3m 12s"})),
            ("pr_label_add", json!({"label": "afkd/reviewed ✅"})),
        ] {
            assert_eq!(f.act(action, args, &key), json!({"ok": true}), "{action}");
        }
        assert_eq!(
            f.diag.lines(),
            [
                "github is unreachable for pull request \"PR 7\" (acme/widgets#7): github \
                 delete comment: no response (mock failure); deleting the claim marker is \
                 queued and retried for up to 1h",
                "github still owes pull request \"PR 7\" (acme/widgets#7) earlier writes; the \
                 comment \"round done ✅…\" is queued behind them",
                "github still owes pull request \"PR 7\" (acme/widgets#7) earlier writes; \
                 adding the label \"afkd/reviewed ✅\" is queued behind them",
            ]
        );
        f.mock.clear_failure();
        f.drain();
        assert_eq!(
            f.mock.actions()[before..],
            [
                Action::DeleteComment { id: marker(&key) },
                Action::Comment {
                    index: 7,
                    body: "round done ✅\n\n- 3m 12s".into(),
                },
                Action::Label {
                    index: 7,
                    name: "afkd/reviewed ✅".into(),
                },
            ]
        );
    }

    /// afkd closing the pipe with writes still owed: each is named, one line, on the way
    /// out.
    #[test]
    fn closing_with_writes_owed_names_them() {
        let (mut f, key) = claimed_issue();
        f.mock.go_down();
        assert_eq!(f.finish(&key, "clean"), json!({"ok": true}));
        assert_eq!(f.act("close", json!({}), &key), json!({"ok": true}));
        let said = f.diag.lines().len();
        f.plugin.close();
        assert_eq!(
            f.diag.lines()[said..],
            [
                format!(
                    "afkd ended the plugin with deleting the claim marker for issue \"{TITLE}\" \
                     (acme/widgets#7) still owed"
                ),
                format!(
                    "afkd ended the plugin with closing it for issue \"{TITLE}\" \
                     (acme/widgets#7) still owed"
                ),
            ]
        );
    }
}
