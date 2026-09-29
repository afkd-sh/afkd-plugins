//! The plugin's state across calls, and one handler per call: the thin layer between
//! afkd's wire ([`crate::wire`]) and the armed kind's vendor half ([`crate::issue`] or
//! [`crate::pr`], behind [`crate::kind`]). Everything here is written once and shared by
//! both kinds.
//!
//! **afkd runs the hooks.** `on_claim` after a `poll` hands a unit over, and `on_done`,
//! `on_fail` or the issue kind's `on_park` after its `finish`; each action a hook calls is
//! one `call`, naming the unit by its key, which this plugin does on that unit's issue or
//! PR. A finished unit is kept a while for that ([`FINISHED_MAX`]), since its post-run
//! hook's calls arrive after `finish`.
//!
//! Three things the wire forces that the built-in never had to do:
//!
//! - **Every reply fits one line.** afkd caps a line at 64 KiB. A `poll` whose brief would
//!   overflow it is cut to fit ([`fit_poll`]); a `comments` reply sends only what afkd has
//!   not been told yet, and if that still overflows, the newest that fit.
//! - **`finish` always answers `ok`.** afkd crashes the service on a refused `finish`. The
//!   built-in's answer to a park that did not land — hold the claim, release it on the
//!   next beat, and on a second failure for the same unit leave it for a human — is
//!   performed here, at once.
//! - **Every call answers within 45 seconds.** afkd ends the service on a missed reply,
//!   so each armed call runs under [`CALL_BUDGET`], and a forge too slow for it reads as
//!   a forge that is down.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::path::Path;
use std::time::Instant;

use serde_json::{json, Map, Value};

use crate::claim::is_claim;
use crate::client::{Gitea, GiteaClient};
use crate::common::{ClaimFault, Clock, Diag, CALL_BUDGET};
use crate::issue::IssueUnits;
use crate::kind::{ClaimedUnit, Units};
use crate::lifecycle::from_call;
use crate::pr::PrUnits;
use crate::rfc3339::format_utc;
use crate::settings::{issue_config, pr_config, GiteaConfig};
use crate::wire::{
    fire_line, fit_comments, fit_poll, Facts, Request, UnitOutcome, WireComment, MAX_REPLY, PROTO,
};

/// The issue kind, the plugin's main one: `service(gitea)` in a config.
pub(crate) const ISSUE_KIND: &str = "issue";

/// The pull-request kind: `service(gitea.pr)` in a config.
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
/// mock in the tests.
pub(crate) type Connect = Box<dyn Fn(&GiteaConfig) -> Box<dyn GiteaClient>>;

/// The plugin across its whole life: unarmed until `hello`, then one armed kind.
pub(crate) struct Plugin {
    connect: Connect,
    clock: Box<dyn Clock>,
    diag: Box<dyn Diag>,
    armed: Option<Box<dyn Service>>,
}

/// One armed service, whichever kind it is: the calls afkd makes after `hello`.
trait Service {
    /// The optional calls the armed kind answers, as `hello` lists them.
    fn calls(&self) -> &'static [&'static str];
    /// Set (or, with `None`, clear) the current call's deadline, which the kind's scan
    /// and every forge request it makes until the next set run under.
    fn set_call_deadline(&self, deadline: Option<Instant>);
    /// The token's login — the value `me` — resolved once and kept; `None`, diagnosed,
    /// while the forge cannot say.
    fn me(&mut self, diag: &dyn Diag) -> Option<String>;
    fn poll(&mut self, clock: &dyn Clock, diag: &dyn Diag) -> Answer;
    fn release(&mut self, key: &str, diag: &dyn Diag) -> Answer;
    fn renew(&self, key: &str, renewal: u64, diag: &dyn Diag) -> Answer;
    fn comments(&mut self, key: &str, diag: &dyn Diag) -> Answer;
    fn classify(&self, scratch: &Path, outcome: UnitOutcome) -> UnitOutcome;
    fn finish(&mut self, key: &str, outcome: UnitOutcome, facts: &Facts, diag: &dyn Diag)
        -> Answer;
    fn call(
        &self,
        action: &str,
        args: &Map<String, Value>,
        key: Option<&str>,
        diag: &dyn Diag,
    ) -> Answer;
}

/// One armed service of kind `K`.
struct Armed<K: Units> {
    units: K,
    /// The token's login, resolved at `hello` (or, failing that, on a `poll`) and kept.
    me: Option<String>,
    /// The units handed over and not yet finished or released, by key.
    live: BTreeMap<String, LiveUnit<K::Unit>>,
    /// The units most recently finished, by key and oldest first, for the post-run hook's
    /// `call`s; at most [`FINISHED_MAX`].
    finished: VecDeque<(String, K::Unit)>,
    /// The threads whose park has failed to land once.
    undelivered: HashSet<String>,
}

/// A unit afkd is running.
struct LiveUnit<U> {
    unit: U,
    /// The comment ids afkd has been told about: the unit's `seen`, then everything a
    /// `comments` reply carried or left out.
    reported: HashSet<String>,
}

impl Plugin {
    /// A plugin that arms against the real Gitea.
    pub(crate) fn new(clock: Box<dyn Clock>, diag: Box<dyn Diag>) -> Self {
        let connect: Connect =
            Box::new(|cfg| Box::new(Gitea::new(&cfg.base_url, &cfg.token)) as Box<dyn GiteaClient>);
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
                *armed = arm(&self.connect, proto, &kind, &settings)
                    .map_err(|problem| diag.err(&problem))
                    .ok();
                let Some(service) = armed.as_deref_mut() else {
                    return Answer::Reply(json!({"ok": false, "proto": PROTO}).to_string());
                };
                // `me` is resolved here, under the call budget like any forge read. A
                // forge that cannot say yet does not refuse the service over a blip: the
                // reply leaves `values` out, and the first `poll` resolves it again.
                service.set_call_deadline(Some(clock.now() + CALL_BUDGET));
                let me = service.me(diag);
                service.set_call_deadline(None);
                let mut reply = json!({"ok": true, "proto": PROTO, "calls": service.calls()});
                if let Some(me) = me {
                    reply["values"] = json!({ "me": me });
                }
                Answer::Reply(reply.to_string())
            }
            Request::Poll => on_armed(armed, clock, |a| a.poll(clock, diag)),
            Request::Release { key } => on_armed(armed, clock, |a| a.release(&key, diag)),
            Request::Renew { key, renewal } => {
                on_armed(armed, clock, |a| a.renew(&key, renewal, diag))
            }
            Request::Comments { key } => on_armed(armed, clock, |a| a.comments(&key, diag)),
            Request::Classify { scratch, outcome } => on_armed(armed, clock, |a| {
                let outcome = a.classify(Path::new(&scratch), outcome);
                Answer::Reply(json!({ "outcome": outcome }).to_string())
            }),
            Request::Finish {
                key,
                outcome,
                facts,
            } => on_armed(armed, clock, |a| a.finish(&key, outcome, &facts, diag)),
            Request::Call { action, args, key } => on_armed(armed, clock, |a| {
                a.call(&action, &args, key.as_deref(), diag)
            }),
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

/// Arm the kind `hello` names over its settings, or say why not. afkd has already held
/// the block to the manifest, so what can be wrong here is the protocol, the kind, or a
/// rule the manifest cannot express.
fn arm(
    connect: &Connect,
    proto: u32,
    kind: &str,
    settings: &serde_json::Value,
) -> Result<Box<dyn Service>, String> {
    if proto != PROTO {
        return Err(format!(
            "afkd speaks plugin protocol {proto}, and this plugin speaks {PROTO}"
        ));
    }
    let fault = |e| format!("trigger {kind}: {e}");
    Ok(match kind {
        ISSUE_KIND => {
            let cfg = issue_config(settings).map_err(fault)?;
            Box::new(Armed::new(IssueUnits::new(connect(&cfg), &cfg)))
        }
        PR_KIND => {
            let cfg = pr_config(settings).map_err(fault)?;
            Box::new(Armed::new(PrUnits::new(connect(&cfg), &cfg)))
        }
        _ => return Err(format!("kind `{kind}` is not provided by @afkd/gitea")),
    })
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
            undelivered: HashSet::new(),
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
    /// transient forge failure is the built-in's idle beat, diagnosed; a definite one
    /// ends the service with its sentence.
    fn poll(&mut self, clock: &dyn Clock, diag: &dyn Diag) -> Answer {
        let Some(me) = self.me(diag) else {
            return idle();
        };
        let unit = match self.units.try_claim_next(&me, diag, clock) {
            Ok(Some(unit)) => unit,
            Ok(None) => return idle(),
            Err(ClaimFault::Transient(e)) => {
                diag.err(&e);
                return idle();
            }
            Err(ClaimFault::Fatal(reason)) => return Answer::Fatal(reason),
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
            // Only a unit whose `seen` alone overflows the line reaches this: nothing can
            // be handed over, so the claim is taken back before the service ends.
            self.units.release(&unit, diag);
            return Answer::Fatal(format!(
                "{} cannot be handed to afkd: even with its brief cut away, the unit is over \
                 afkd's 64 KiB plugin line ({} comment ids in `seen`)",
                unit.thread(),
                wire.seen.len()
            ));
        };
        let reported = wire.seen.iter().cloned().collect();
        self.live.insert(wire.key, LiveUnit { unit, reported });
        Answer::Reply(line)
    }

    /// Release a leftover journal key — a unit afkd handed straight back, or one whose
    /// `on_claim` failed — and forget it.
    fn release(&mut self, key: &str, diag: &dyn Diag) -> Answer {
        self.live.remove(key);
        self.finished.retain(|(finished, _)| finished != key);
        Answer::Reply(json!({"released": self.units.release_stale(key, diag)}).to_string())
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

    fn classify(&self, scratch: &Path, outcome: UnitOutcome) -> UnitOutcome {
        K::classify(scratch, outcome)
    }

    /// Finish a unit, and keep it for the post-run hook's `call`s. Always `ok` (see the
    /// module doc): a park that did not land is released at once so the next poll retries
    /// the unit, and the second time the same unit fails that way it is left for a human.
    fn finish(
        &mut self,
        key: &str,
        outcome: UnitOutcome,
        facts: &Facts,
        diag: &dyn Diag,
    ) -> Answer {
        let ok = Answer::Reply(json!({"ok": true}).to_string());
        let Some(live) = self.live.remove(key) else {
            diag.err(&format_args!(
                "finish for {key}, which this plugin holds no claim on"
            ));
            return ok;
        };
        if !self.units.finish(&live.unit, outcome, facts, diag) {
            let thread = live.unit.thread();
            if self.undelivered.insert(thread.clone()) {
                self.units.release(&live.unit, diag);
                diag.err(&format_args!(
                    "could not deliver the park for {key}; releasing the claim so the next \
                     poll retries it"
                ));
            } else {
                diag.err(&format_args!(
                    "the park for {thread} failed twice; leaving the claim in place for a \
                     human"
                ));
            }
        }
        if self.finished.len() == FINISHED_MAX {
            self.finished.pop_front();
        }
        self.finished.push_back((key.to_string(), live.unit));
        ok
    }

    /// Do one action a hook called, on the unit `key` names — a live one, or one finished
    /// since. `{"ok":false}` with the sentence, diagnosed too, for an action afkd would not
    /// have bound, a key naming no unit, or a forge that refused it.
    fn call(
        &self,
        action: &str,
        args: &Map<String, Value>,
        key: Option<&str>,
        diag: &dyn Diag,
    ) -> Answer {
        let refuse = |error: String| {
            diag.err(&error);
            Answer::Reply(json!({"ok": false, "error": error}).to_string())
        };
        let decoded = match from_call(action, args) {
            Ok(decoded) => decoded,
            Err(problem) => return refuse(problem),
        };
        let Some(key) = key else {
            return refuse(format!(
                "{action} acts on a claimed unit, and this run has none"
            ));
        };
        let Some(unit) = self.unit(key) else {
            return refuse(format!(
                "{action} for {key}, which this plugin holds no claim on"
            ));
        };
        match self.units.act(unit, &decoded) {
            Ok(()) => Answer::Reply(json!({"ok": true}).to_string()),
            Err(e) => refuse(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    //! The handlers over a mock forge: what each call answers, and the state the plugin
    //! keeps between them. The wire itself — a real child, real JSON lines, a real HTTP
    //! forge — is `tests/wire.rs`.

    use super::*;
    use crate::claim::claim_text;
    use crate::client::{Action, MockClient};
    use crate::common::{CaptureDiag, FakeClock, TempDir};
    use std::sync::Arc;
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
    }

    impl Fixture {
        /// A plugin over a mock forge whose token is `björn-öst[bot]`'s, not yet armed.
        fn new() -> Self {
            let mock = Arc::new(MockClient::new("björn-öst[bot]"));
            let diag = Arc::new(CaptureDiag::default());
            let clock = Arc::new(FakeClock::new());
            let forge = Arc::clone(&mock);
            let connect: Connect =
                Box::new(move |_| Box::new(Arc::clone(&forge)) as Box<dyn GiteaClient>);
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
            }
        }

        /// The same, armed as the `issue` kind over `settings`.
        fn armed(settings: serde_json::Value) -> Self {
            Self::armed_as(ISSUE_KIND, settings)
        }

        /// The same, armed as `kind` over `settings`.
        fn armed_as(kind: &str, settings: serde_json::Value) -> Self {
            let mut f = Self::new();
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

        /// The `finish` call for `key`, ended `outcome`, with the facts of a run that
        /// proceeded.
        fn finish(&mut self, key: &serde_json::Value, outcome: &str) -> serde_json::Value {
            self.call(
                json!({"call": "finish", "id": "7", "key": key, "outcome": outcome,
                             "facts": {"signal": "proceed", "reason": null, "duration_ms": 3,
                                       "cost": 0.0, "turns": null, "tokens": null,
                                       "run_name": "260925-100400-unit-7-1"}}),
            )
        }

        /// One `call` of `action` with `args` on the unit `key` names, as afkd sends a
        /// hook's action.
        fn act(
            &mut self,
            action: &str,
            args: serde_json::Value,
            key: &serde_json::Value,
        ) -> serde_json::Value {
            self.call(json!({"call": "call", "action": action, "args": args, "key": key}))
        }
    }

    /// The `hello` afkd writes for `kind` over `settings`.
    fn hello(kind: &str, settings: serde_json::Value) -> serde_json::Value {
        json!({"call": "hello", "proto": 2, "kind": kind, "service": "afkd::develop",
               "settings": settings})
    }

    fn settings() -> serde_json::Value {
        json!({"repo": "acme/widgets", "token": "PAT", "source_label": "afkd/ready"})
    }

    /// A `pr` block over the bot's own PRs, the `bool` lowered to its word.
    fn pr_settings() -> serde_json::Value {
        json!({"repo": "acme/widgets", "token": "PAT", "author_me": "true"})
    }

    /// The accepted `hello` lists exactly the optional calls the kind answers — `call` is
    /// proto 2's own and is never listed — and supplies the value `me`, the token's login
    /// read under the call budget.
    #[test]
    fn hello_lists_every_optional_call_and_supplies_me() {
        let mut f = Fixture::new();
        let reply = f.call(hello(ISSUE_KIND, settings()));
        assert_eq!(
            reply,
            json!({"ok": true, "proto": 2, "calls": ["release", "renew", "comments", "classify"],
                   "values": {"me": "björn-öst[bot]"}})
        );
        assert!(f.diag.lines().is_empty(), "{:?}", f.diag.lines());
        let now = f.clock.now();
        assert_eq!(f.mock.call_deadlines(), [Some(now + CALL_BUDGET), None]);
        assert!(f.mock.actions().is_empty(), "hello writes nothing");
    }

    /// The PR kind lists what it answers: no `classify`, since the built-in keeps the
    /// spine's default and never parks a PR.
    #[test]
    fn a_pr_hello_lists_release_renew_comments() {
        let mut f = Fixture::new();
        let reply = f.call(hello(PR_KIND, pr_settings()));
        assert_eq!(
            reply,
            json!({"ok": true, "proto": 2, "calls": ["release", "renew", "comments"],
                   "values": {"me": "björn-öst[bot]"}})
        );
        assert!(f.diag.lines().is_empty(), "{:?}", f.diag.lines());
    }

    /// A forge that cannot say who the token is at `hello` does not refuse the service
    /// over a blip: the kind is armed, the reply leaves `values` out, the failure is
    /// diagnosed, and the first `poll` resolves the login and claims.
    #[test]
    fn a_hello_whose_identity_read_fails_is_accepted_without_me() {
        let mut f = Fixture::new();
        f.mock.add_issue(7, "Fix", "do it", &["afkd/ready"]);
        f.mock.fail("current user");
        assert_eq!(
            f.call(hello(ISSUE_KIND, settings())),
            json!({"ok": true, "proto": 2, "calls": ["release", "renew", "comments", "classify"]})
        );
        assert_eq!(
            f.diag.lines(),
            ["gitea current user: no response (mock failure)"]
        );
        f.mock.clear_failure();
        let unit = f.poll()["unit"].clone();
        assert_eq!(unit["self"], "björn-öst[bot]");
    }

    /// Each refusal is `ok:false` with the problem, in the built-in's own words, on the
    /// diagnostic channel — and leaves the plugin unarmed.
    #[test]
    fn hello_refuses_what_the_kind_cannot_arm_with() {
        for (kind, proto, settings, problem) in [
            (
                "gitlab_issue",
                2,
                settings(),
                "kind `gitlab_issue` is not provided by @afkd/gitea",
            ),
            (
                "gitea_issue",
                2,
                settings(),
                "kind `gitea_issue` is not provided by @afkd/gitea",
            ),
            (
                PR_KIND,
                2,
                json!({"repo": "acme/widgets", "org": "acme", "token": "PAT", "author_me": "true"}),
                "trigger pr: setting `org`: a gitea trigger takes exactly one of `repo` or \
                 `org` (both were set)",
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
                json!({"repo": "acme/widgets", "org": "acme", "token": "PAT"}),
                "trigger issue: setting `org`: a gitea trigger takes exactly one of `repo` or \
                 `org` (both were set)",
            ),
        ] {
            let mut f = Fixture::new();
            let mut request = hello(kind, settings);
            request["proto"] = json!(proto);
            assert_eq!(f.call(request), json!({"ok": false, "proto": 2}));
            assert_eq!(f.diag.lines(), [problem]);
            assert!(
                f.mock.call_deadlines().is_empty(),
                "a refused hello reads no forge"
            );
            assert!(matches!(
                f.plugin.answer(Request::Poll),
                Answer::Fatal(reason) if reason.contains("before a `hello`")
            ));
        }
    }

    /// A transient forge failure on `poll` is the built-in's idle beat, diagnosed; the
    /// next beat, with the forge back, claims.
    #[test]
    fn a_poll_forge_error_is_an_idle_beat_and_logged() {
        let mut f = Fixture::armed(settings());
        f.mock.add_issue(7, "Fix", "do it", &["afkd/ready"]);
        f.mock.fail("list issues");
        assert_eq!(f.poll(), json!({"fire": false}));
        assert_eq!(
            f.diag.lines(),
            ["gitea list issues: no response (mock failure)"]
        );
        f.mock.clear_failure();
        assert_eq!(f.poll()["fire"], true);
    }

    /// An identity neither `hello` nor the beat could resolve is an idle beat, retried on
    /// the next one; once resolved it is kept, so no later beat reads it again.
    #[test]
    fn an_identity_that_will_not_resolve_is_an_idle_beat_retried_next_poll() {
        let mut f = Fixture::new();
        f.mock.fail("current user");
        f.call(hello(ISSUE_KIND, settings()));
        f.mock.add_issue(7, "Fix", "do it", &["afkd/ready"]);
        assert_eq!(f.poll(), json!({"fire": false}));
        assert_eq!(
            f.diag.lines(),
            ["gitea current user: no response (mock failure)"; 2]
        );
        f.mock.clear_failure();
        assert_eq!(f.poll()["unit"]["self"], "björn-öst[bot]");
        f.mock.fail("current user");
        assert_eq!(f.poll(), json!({"fire": false}), "nothing more to claim");
        assert_eq!(f.diag.lines().len(), 2, "the kept login is not read again");
    }

    /// A definite verdict ends the process with the built-in's sentence.
    #[test]
    fn an_exclusive_claim_label_is_fatal() {
        let mut f = Fixture::armed(settings());
        f.mock.register_label("afkd/claimed", true);
        f.mock.add_issue(7, "Fix", "do it", &["afkd/ready"]);
        let Answer::Fatal(reason) = f.plugin.answer(Request::Poll) else {
            panic!("an exclusive gate label is fatal");
        };
        assert!(
            reason.starts_with("label “afkd/claimed” on acme/widgets is an exclusive scoped label"),
            "{reason}"
        );
    }

    /// The `comments` reply maps each comment the way afkd's watch reads it — the
    /// decimal id, the login as both author fields, the body verbatim, and the creation
    /// stamp as RFC 3339 — and leaves out afkd's own comments and every claim marker.
    #[test]
    fn a_comments_reply_maps_id_author_body_and_created_at_and_omits_markers_and_self() {
        let mut f = Fixture::armed(settings());
        f.mock.add_issue(7, "Fix", "do it", &["afkd/ready"]);
        let key = f.poll()["unit"]["key"].as_str().unwrap().to_string();
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
            .add_comment_body(7, 42, "björn-öst[bot]", "Working on it.", 1_790_330_300);
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
        let ids: Vec<&str> = reply["comments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["45"]);
    }

    /// With nothing seeded — the claim read no comments — the first read hands afkd the
    /// whole thread, the baseline its watch records rather than delivers. With
    /// `discuss_with`, the claim-time ids are the unit's `seen` and never come back.
    #[test]
    fn an_unseeded_first_read_returns_the_whole_baseline_and_a_seeded_one_does_not() {
        let mut f = Fixture::armed(settings());
        f.mock
            .add_issue_assigned(7, "Fix", "do it", &[], &["björn-öst[bot]"]);
        f.mock
            .add_comment_body(7, 41, "álvaro", "the original ask", 100);
        let unit = f.poll()["unit"].clone();
        assert_eq!(unit["seen"], json!([]));
        let reply = f.call(json!({"call": "comments", "key": unit["key"]}));
        assert_eq!(reply["comments"][0]["id"], "41");

        let mut settings = settings();
        settings["discuss_with"] = json!(["anyone"]);
        let mut f = Fixture::armed(settings);
        f.mock
            .add_issue_assigned(7, "Fix", "do it", &[], &["björn-öst[bot]"]);
        f.mock
            .add_comment_body(7, 41, "álvaro", "the original ask", 100);
        let unit = f.poll()["unit"].clone();
        assert_eq!(unit["seen"], json!(["41"]));
        let reply = f.call(json!({"call": "comments", "key": unit["key"]}));
        assert_eq!(reply, json!({"comments": []}));
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

    #[test]
    fn classify_answers_park_on_the_marker_and_echoes_afkd_otherwise() {
        let mut f = Fixture::armed(settings());
        let scratch = TempDir::new();
        let path = scratch.path().to_str().unwrap();
        let classify = |f: &mut Fixture, outcome: &str| {
            f.call(json!({"call": "classify", "key": "acme/widgets#7#1", "scratch": path, "outcome": outcome}))
        };
        assert_eq!(classify(&mut f, "clean"), json!({"outcome": "clean"}));
        assert_eq!(classify(&mut f, "failed"), json!({"outcome": "failed"}));
        std::fs::write(scratch.path().join("park"), b"").unwrap();
        assert_eq!(classify(&mut f, "failed"), json!({"outcome": "park"}));
    }

    /// A `release` drops a live unit, so nothing later can act on it.
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
        assert_eq!(
            f.call(json!({"call": "renew", "key": key, "renewal": 2})),
            json!({"ok": false})
        );
        // And the three shapes a journal key comes in.
        assert_eq!(
            f.call(json!({"call": "release", "key": "acme/widgets#9"})),
            json!({"released": null})
        );
        assert_eq!(
            f.call(json!({"call": "release", "key": "no-slash#9#1"})),
            json!({"released": false})
        );
    }

    /// A park that did not land still answers `ok` — afkd would crash the service
    /// otherwise. The first time, the claim is released so the next poll retries the
    /// issue; the second time for the same issue, it is left for a human.
    #[test]
    fn an_undelivered_park_releases_once_then_leaves_the_claim() {
        let mut f = Fixture::armed(settings());
        f.mock.add_issue(7, "Fix", "do it", &["afkd/ready"]);
        // The first poll's ensure defines both managed labels; the park's swap then
        // fails on the forge, so the awaiting label never lands.
        let key = f.poll()["unit"]["key"].clone();
        f.mock.fail("add label");
        assert_eq!(f.finish(&key, "park"), json!({"ok": true}));
        f.mock.clear_failure();
        assert!(!f.mock.has_label(7, "afkd/claimed"), "released for a retry");
        assert!(!f.mock.has_label(7, "afkd/awaiting-reply"));
        assert_eq!(
            f.diag.lines().last().unwrap(),
            &format!(
                "could not deliver the park for {}; releasing the claim so the next poll \
                 retries it",
                key.as_str().unwrap()
            )
        );

        let key = f.poll()["unit"]["key"].clone();
        f.mock.fail("add label");
        assert_eq!(f.finish(&key, "park"), json!({"ok": true}));
        f.mock.clear_failure();
        assert!(f.mock.has_label(7, "afkd/claimed"), "left in place");
        assert_eq!(
            f.diag.lines().last().unwrap(),
            "the park for acme/widgets#7 failed twice; leaving the claim in place for a human"
        );
        assert_eq!(f.poll(), json!({"fire": false}), "and not re-claimed");
    }

    /// A clean finish is plain `ok` and does only the plugin-owned write — the marker's
    /// release; a finish without a `discuss_with` reads nothing and writes nothing else.
    #[test]
    fn poll_and_finish_perform_no_hook_actions() {
        let mut f = Fixture::armed(settings());
        f.mock.add_issue(7, "Fix", "do it", &["afkd/ready"]);
        let key = f.poll()["unit"]["key"].clone();
        let claimed = f.mock.actions();
        assert!(
            claimed.iter().all(|a| match a {
                Action::Comment { body, .. } => is_claim(body),
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

    /// `finish` for a key the plugin never handed over is diagnosed and still `ok`.
    #[test]
    fn a_finish_for_an_unknown_key_is_ok_and_diagnosed() {
        let mut f = Fixture::armed(settings());
        let reply = f.call(
            json!({"call": "finish", "id": "7", "key": "acme/widgets#7#1", "outcome": "clean",
                                  "facts": {"signal": "proceed", "duration_ms": 0, "cost": 0.0}}),
        );
        assert_eq!(reply, json!({"ok": true}));
        assert!(f
            .mock
            .actions()
            .iter()
            .all(|a| !matches!(a, Action::State { .. })));
        assert_eq!(
            f.diag.lines(),
            ["finish for acme/widgets#7#1, which this plugin holds no claim on"]
        );
    }

    /// A unit whose claim-time thread alone overflows the line cannot be handed over at
    /// all: the claim is taken back, and the process ends saying why.
    #[test]
    fn a_seen_list_over_the_line_releases_the_claim_and_is_fatal() {
        let mut settings = settings();
        settings["discuss_with"] = json!(["anyone"]);
        let mut f = Fixture::armed(settings);
        f.mock
            .add_issue_assigned(7, "Fix", "do it", &[], &["björn-öst[bot]"]);
        for id in 1..=12_000 {
            f.mock
                .add_comment_body(7, 1_000_000 + id, "álvaro", "+1", id);
        }
        let Answer::Fatal(reason) = f.plugin.answer(Request::Poll) else {
            panic!("an unfittable unit is fatal");
        };
        assert!(
            reason.starts_with("acme/widgets#7 cannot be handed to afkd")
                && reason.ends_with("(12000 comment ids in `seen`)"),
            "{reason}"
        );
        assert!(
            !f.mock.has_label(7, "afkd/claimed"),
            "the claim was taken back"
        );
        let markers = f
            .mock
            .list_issue_comments(&crate::client::Repo::parse("acme/widgets").unwrap(), 7)
            .unwrap()
            .into_iter()
            .filter(|c| is_claim(&c.body))
            .count();
        assert_eq!(markers, 0);
    }

    /// `classify` is a call the PR kind does not list, so it is refused exactly as an
    /// unknown call is — even with a park marker in the scratch directory, which only the
    /// issue kind reads.
    #[test]
    fn a_pr_service_refuses_classify() {
        let mut f = Fixture::armed_as(PR_KIND, pr_settings());
        let scratch = TempDir::new();
        std::fs::write(scratch.path().join("park"), b"").unwrap();
        let reply = f.call(json!({"call": "classify", "key": "acme/widgets#7#1",
                                  "scratch": scratch.path().to_str().unwrap(), "outcome": "failed"}));
        assert_eq!(reply, json!({"ok": false}));
        assert_eq!(
            f.diag.lines(),
            ["afkd sent a call this plugin did not list in its `hello` reply"]
        );
    }

    /// A PR unit's `seen` is its claim-time thread, and the `comments` reply builds on
    /// it: the review feedback the brief already carried never comes back, and only
    /// what humans said after the claim does — the bot's own reply and markers aside.
    #[test]
    fn a_pr_comments_reply_leaves_out_the_claim_time_thread() {
        let mut f = Fixture::armed_as(PR_KIND, pr_settings());
        f.mock
            .add_pull(7, "björn-öst[bot]", "feature/retry-backoff");
        f.mock.add_comment_body(
            7,
            41,
            "陳大文",
            "看起来不对 🚨\n\nThe backoff never caps.",
            1_790_330_000,
        );
        f.mock.add_review(7, 51, "carol", 1_790_330_100);
        let unit = f.poll()["unit"].clone();
        assert_eq!(unit["seen"], json!(["41"]));
        let key = unit["key"].clone();

        f.mock
            .add_comment_body(7, 42, "björn-öst[bot]", "Pushed a cap.", 1_790_330_280);
        f.mock
            .add_comment_body(7, 43, "álvaro", "Also the jitter?", 1_790_330_300);
        let reply = f.call(json!({"call": "comments", "key": key}));
        assert_eq!(
            reply,
            json!({"comments": [
                {"id": "43", "author": "álvaro", "author_name": "álvaro",
                 "body": "Also the jitter?", "at": "2026-09-25T09:58:20Z"},
            ]})
        );
    }

    #[test]
    fn a_call_it_never_listed_is_refused() {
        let mut f = Fixture::armed(settings());
        let reply =
            f.call(json!({"call": "attempt_failed", "key": "k", "n": 1, "max": 2, "reason": null}));
        assert_eq!(reply, json!({"ok": false}));
    }

    /// Every armed call hands the forge a deadline [`CALL_BUDGET`] from the clock's now
    /// before it runs, and clears it once the reply is built — `hello` too, for the read
    /// of `me`; an unknown call makes no forge request and hands it none. Each call runs
    /// at its own `now`, so a deadline inherited from the call before would show.
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
        let scratch = TempDir::new();

        let facts = json!({"signal": "proceed", "reason": null, "duration_ms": 3, "cost": 0.0,
                           "turns": null, "tokens": null, "run_name": "260927-220459-unit-7-1"});
        let mut key = serde_json::Value::Null;
        for call in [
            "poll", "renew", "comments", "classify", "finish", "call", "release",
        ] {
            f.clock.advance(Duration::from_secs(3));
            let now = f.clock.now();
            let reply = match call {
                "poll" => f.poll(),
                "renew" => f.call(json!({"call": "renew", "key": key, "renewal": 1})),
                "comments" => f.call(json!({"call": "comments", "key": key})),
                "classify" => f.call(
                    json!({"call": "classify", "key": key, "scratch": scratch.path(),
                           "outcome": "clean"}),
                ),
                "finish" => f.call(json!({"call": "finish", "id": "7", "key": key,
                                          "outcome": "clean", "facts": facts})),
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
            16,
            "one set and one clear each"
        );

        f.call(json!({"call": "rewind", "key": "k"}));
        assert_eq!(
            f.mock.call_deadlines().len(),
            16,
            "an unknown call hands none"
        );
    }

    /// Each of the six actions, as afkd sends a hook's call of it on the live unit, does
    /// its one forge operation on the claimed issue as its claim identity and answers
    /// `ok` — a slashed emoji label byte for byte, and a comment's `#{run.x}` left as
    /// afkd sent it.
    #[test]
    fn every_action_over_call_acts_on_the_live_unit() {
        let mut f = Fixture::armed(settings());
        f.mock.register_label("afkd/reviewed ✅", false);
        f.mock
            .add_issue_assigned(7, "Fix", "do it", &["afkd/ready"], &["alice"]);
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
        let acted = &f.mock.actions()[before..];
        assert!(
            matches!(
                acted,
                [
                    Action::Assign { index: 7, assignees: added },
                    Action::Label { index: 7, name },
                    Action::Unlabel { index: 7, .. },
                    Action::Comment { index: 7, body },
                    Action::Assign { index: 7, assignees: kept },
                    Action::State { index: 7, state },
                ] if added == &["alice", "björn-öst[bot]"]
                    && name == "afkd/reviewed ✅"
                    && body == markdown
                    && kept == &["alice"]
                    && state == "closed"
            ),
            "{acted:?}"
        );
        assert!(!f.mock.has_label(7, "afkd/claimed"));
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

        assert_eq!(
            f.act("label_remove", json!({"label": "afkd/claimed"}), &key),
            json!({"ok": true})
        );
        assert_eq!(f.act("close", json!({}), &key), json!({"ok": true}));
        assert!(closed(&f));
        assert!(!f.mock.has_label(7, "afkd/claimed"));

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
    /// fails the hook's `result` with — and says it on the diagnostic channel too: an
    /// action afkd would not bind, a bare run's `null` key, a key naming no claim, a
    /// label the repository does not define, and a forge that never answered.
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
                "label_add",
                json!({}),
                key.clone(),
                "label_add: parameter label is required",
            ),
            (
                "comment",
                json!({"text": 7}),
                key.clone(),
                "comment: parameter text must be a string",
            ),
            (
                "close",
                json!({}),
                json!(null),
                "close acts on a claimed unit, and this run has none",
            ),
            (
                "close",
                json!({}),
                json!("acme/widgets#9#1"),
                "close for acme/widgets#9#1, which this plugin holds no claim on",
            ),
            (
                "label_add",
                json!({"label": "afkd/nirgendwo 🚫"}),
                key.clone(),
                "gitea add label: label “afkd/nirgendwo 🚫” was not applied (the repository \
                 defines no such label)",
            ),
        ] {
            assert_eq!(
                f.act(action, args, &key),
                json!({"ok": false, "error": error}),
                "{action}"
            );
            expect.push(error.to_string());
        }
        f.mock.fail("set state");
        assert_eq!(
            f.act("close", json!({}), &key),
            json!({"ok": false, "error": "gitea set state: no response (mock failure)"})
        );
        expect.push("gitea set state: no response (mock failure)".into());
        assert_eq!(f.diag.lines(), expect);
        assert_eq!(f.mock.actions().len(), before, "nothing landed");
    }

    /// The PR kind answers `call` too, on the claimed pull request.
    #[test]
    fn a_pr_call_acts_on_the_claimed_pull_request() {
        let mut f = Fixture::armed_as(PR_KIND, pr_settings());
        f.mock
            .add_pull(7, "björn-öst[bot]", "feature/retry-backoff");
        f.mock
            .add_comment_body(7, 41, "陳大文", "看起来不对 🚨", 1_790_330_000);
        let key = f.poll()["unit"]["key"].clone();
        assert_eq!(f.finish(&key, "clean"), json!({"ok": true}));
        assert_eq!(
            f.act("comment", json!({"text": "round done ✅"}), &key),
            json!({"ok": true})
        );
        assert!(f
            .mock
            .actions()
            .iter()
            .any(|a| matches!(a, Action::Comment { index: 7, body } if body == "round done ✅")));
    }

    /// A `call` before an accepted `hello` is afkd out of step, like any other call.
    #[test]
    fn a_call_before_hello_is_fatal() {
        let mut f = Fixture::new();
        let request = serde_json::from_value(
            json!({"call": "call", "action": "close", "args": {}, "key": "acme/widgets#7#1"}),
        )
        .unwrap();
        assert!(matches!(
            f.plugin.answer(request),
            Answer::Fatal(reason) if reason.contains("before a `hello`")
        ));
    }
}
