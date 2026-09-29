//! The plugin's state across calls, and one handler per call: the thin layer between
//! afkd's wire ([`crate::wire`]) and the `card` kind's vendor half ([`crate::card`]).
//!
//! **afkd runs the hooks.** `on_claim` after a `poll` hands a unit over, and `on_done`,
//! `on_park` or `on_fail` after its `finish`; each action a hook calls is one `call`,
//! naming the unit by its key, which this plugin does on that unit's card. A finished
//! unit's card is kept a while for that ([`FINISHED_MAX`]), since its post-run hook's
//! calls arrive after `finish`.
//!
//! Five things the wire forces that the built-in never had to do:
//!
//! - **Who is asking arrives in `hello`.** The built-in read its service, roster and
//!   claim owner off afkd's config; here `hello` carries them, and a `hello` without them
//!   is refused, since the park owner cannot work without a service name.
//! - **Every reply fits one line.** afkd caps a line at 64 KiB. A `poll` whose brief would
//!   overflow it is cut to fit ([`fit_poll`]); a `comments` reply sends only what afkd has
//!   not been told yet, and if that still overflows, the newest that fit.
//! - **An undelivered finish is `held`.** The built-in held the claim when its terminal
//!   moment did not land and its reaper finished it on a later beat; `finish` answers
//!   `{"ok":true,"held":true}` and afkd asks `release` for the key on a later beat, which
//!   replays what is owed — the park badge, the owner marker, the claim's release.
//! - **The lines go to stderr.** The success narration and the diagnostics alike, which
//!   afkd files under `[@afkd/trello:err]`.
//! - **Every call answers within 45 seconds.** afkd ends the service on a missed reply,
//!   so each armed call runs under [`CALL_BUDGET`], and a board too slow for it reads as
//!   a board that is down.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::path::Path;

use serde_json::{json, Map, Value};

use crate::board::{BoardClient, Card};
use crate::card::{is_afkd, Identity, TrelloUnits, Unit};
use crate::client::TrelloClient;
use crate::common::{Clock, Diag, CALL_BUDGET};
use crate::lifecycle::from_call;
use crate::rfc3339::format_utc;
use crate::settings::{board_config, BoardConfig, ME};
use crate::wire::{
    fire_line, fit_comments, fit_poll, Facts, Request, UnitOutcome, WireComment, MAX_REPLY, PROTO,
};

/// The one kind this plugin provides, its main one: `service(trello)` in a config.
pub(crate) const TRELLO_KIND: &str = "card";

/// How many finished units' cards a post-run hook's `call`s can still reach. afkd sends
/// them straight after `finish`, one unit at a time, so a handful would do; the rest is
/// headroom, and the oldest goes first.
const FINISHED_MAX: usize = 16;

/// The optional calls the kind answers, exactly as `hello` lists them: every one afkd
/// has, since the built-in used the whole spine.
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

/// Builds the board client a `hello` arms with — the real one in the running plugin, a
/// mock in the tests.
pub(crate) type Connect = Box<dyn Fn(&BoardConfig) -> Box<dyn BoardClient>>;

/// The plugin across its whole life: unarmed until `hello`, then armed.
pub(crate) struct Plugin {
    connect: Connect,
    clock: Box<dyn Clock>,
    diag: Box<dyn Diag>,
    armed: Option<Armed>,
}

/// The armed service: the kind's vendor half, and the units afkd is running.
struct Armed {
    units: TrelloUnits,
    /// The units handed over and not yet finished or released, by key.
    live: BTreeMap<String, LiveUnit>,
    /// The cards of the units most recently finished, by key and oldest first, for the
    /// post-run hook's `call`s; at most [`FINISHED_MAX`].
    finished: VecDeque<(String, Card)>,
}

/// A unit afkd is running.
struct LiveUnit {
    unit: Unit,
    /// The comment ids afkd has been told about: the unit's `seen`, then everything a
    /// `comments` reply carried or left out.
    reported: HashSet<String>,
}

impl Plugin {
    /// A plugin that arms against the real Trello, at the block's `base_url`.
    pub(crate) fn new(clock: Box<dyn Clock>, diag: Box<dyn Diag>) -> Self {
        let connect: Connect = Box::new(|cfg| {
            Box::new(TrelloClient::with_base(
                &cfg.base_url,
                &cfg.api_key,
                &cfg.token,
            )) as Box<dyn BoardClient>
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
            } => {
                let id = Identity {
                    owner,
                    service,
                    roster,
                };
                *armed = arm(&self.connect, proto, &kind, &settings, id)
                    .map_err(|problem| diag.err(&problem))
                    .ok();
                Answer::Reply(
                    match armed {
                        Some(_) => json!({"ok": true, "proto": PROTO, "calls": CALLS,
                                          "values": {"me": ME}}),
                        None => json!({"ok": false, "proto": PROTO}),
                    }
                    .to_string(),
                )
            }
            Request::Poll => on_armed(armed, clock, |a| a.poll(clock, diag)),
            Request::Release { key } => on_armed(armed, clock, |a| a.release(&key, diag)),
            Request::Renew { key, renewal } => {
                on_armed(armed, clock, |a| a.renew(&key, renewal, diag))
            }
            Request::Comments { key } => on_armed(armed, clock, |a| a.comments(&key, diag)),
            Request::Classify { scratch, outcome } => on_armed(armed, clock, |_| {
                let outcome = TrelloUnits::classify(Path::new(&scratch), outcome);
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
            } => on_armed(armed, clock, |a| a.finish(&key, outcome, &facts, diag)),
            Request::Call { action, args, key } => on_armed(armed, clock, |a| {
                a.call(&action, &args, key.as_deref(), diag)
            }),
            Request::Unknown => {
                diag.err(&"afkd sent a call this plugin did not list in its `hello` reply");
                Answer::Reply(json!({"ok": false}).to_string())
            }
        }
    }
}

/// Run `f` on the armed service under the call's [`CALL_BUDGET`], cleared once the reply
/// is built so no call inherits another's deadline — or end the process, since afkd
/// sends nothing but `hello` before a `hello` it has seen accepted.
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

/// Arm the kind `hello` names over its settings, as `id`, or say why not. afkd has
/// already held the block to the manifest, so what can be wrong here is the protocol,
/// the kind, a missing identity, or a rule the manifest cannot express.
fn arm(
    connect: &Connect,
    proto: u32,
    kind: &str,
    settings: &serde_json::Value,
    id: Identity,
) -> Result<Armed, String> {
    if proto != PROTO {
        return Err(format!(
            "afkd speaks plugin protocol {proto}, and this plugin speaks {PROTO}"
        ));
    }
    if kind != TRELLO_KIND {
        return Err(format!("kind `{kind}` is not provided by @afkd/trello"));
    }
    if id.service.is_empty() || id.owner.is_empty() {
        return Err(
            "afkd did not say which service this is; @afkd/trello needs an afkd whose \
                    `hello` carries `service`, `roster` and `owner`"
                .to_string(),
        );
    }
    let cfg = board_config(settings).map_err(|e| format!("trigger {kind}: {e}"))?;
    Ok(Armed {
        units: TrelloUnits::new(connect(&cfg), &cfg, id),
        live: BTreeMap::new(),
        finished: VecDeque::new(),
    })
}

/// The `poll` reply that hands nothing over.
fn idle() -> Answer {
    Answer::Reply(json!({"fire": false}).to_string())
}

impl Armed {
    /// One beat: the parked sweep, then the claim race. A board failure is the
    /// built-in's idle beat, diagnosed.
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
                unit.thread()
            ));
            cut
        } else {
            // Only a unit whose fields besides the brief overflow the line reaches this:
            // nothing can be handed over, so the claim is taken back before the service
            // ends.
            self.units.release_stale(&wire.key, diag);
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

    /// Release a journal key — a crashed run's, a `held` finish's, a unit afkd handed
    /// straight back, or one whose `on_claim` failed — and forget it.
    fn release(&mut self, key: &str, diag: &dyn Diag) -> Answer {
        self.live.remove(key);
        self.finished.retain(|(finished, _)| finished != key);
        let released = self.units.release_stale(key, diag);
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
    /// afkd's watch drops the ids in its cursor and afkd's own comments anyway, so
    /// leaving out those — and every control marker, which only afkd writes — changes no
    /// delivery; it is what keeps a long thread inside one line. `null` when the board
    /// could not be read.
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
        let me = live.unit.self_author();
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
                live.unit.thread(),
                ids.join(", ")
            ));
        }
        live.reported.extend(fresh.into_iter().map(|c| c.id));
        Answer::Reply(line)
    }

    /// Mark a live unit's faulted attempt on its card.
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

    /// Finish a unit, and keep its card for the post-run hook's `call`s. A finish that did
    /// not land is `held`: afkd keeps the claim and sends `release` for the key on a later
    /// beat, which replays what is owed.
    fn finish(
        &mut self,
        key: &str,
        outcome: UnitOutcome,
        facts: &Facts,
        diag: &dyn Diag,
    ) -> Answer {
        let Some(live) = self.live.remove(key) else {
            diag.err(&format_args!(
                "finish for {key}, which this plugin holds no claim on"
            ));
            return Answer::Reply(json!({"ok": true}).to_string());
        };
        let reply = if self.units.finish(&live.unit, outcome, facts, diag) {
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
            .push_back((key.to_string(), live.unit.card().clone()));
        Answer::Reply(reply.to_string())
    }

    /// Do one action a hook called, on the card of the unit `key` names — a live one, or
    /// one finished since. `{"ok":false}` with the sentence, diagnosed too, for an action
    /// afkd would not have bound, a key naming no card, or a board that refused it.
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
            return refuse(format!("{action} acts on a card, and this run has none"));
        };
        let card = match self.live.get(key) {
            Some(live) => live.unit.card(),
            None => match self.finished.iter().find(|(finished, _)| finished == key) {
                Some((_, card)) => card,
                None => {
                    return refuse(format!(
                        "{action} for {key}, which this plugin holds no card for"
                    ))
                }
            },
        };
        match self.units.act(card, &decoded, diag) {
            Ok(()) => Answer::Reply(json!({"ok": true}).to_string()),
            Err(e) => refuse(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    //! The handlers over a mock board: what each call answers, and the state the plugin
    //! keeps between them. The wire itself — a real child, real JSON lines, a real HTTP
    //! board — is `tests/wire.rs`.

    use super::*;
    use crate::board::{Action, MockBoard, SELF_ID};
    use crate::claim::{claim_text, is_claim};
    use crate::common::{CaptureDiag, FakeClock, TempDir};
    use crate::lifecycle::ListPosition;
    use crate::settings::MemberRef;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

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

        fn narrate(&self, line: &str) {
            self.0.narrate(line);
        }
    }

    struct Fixture {
        plugin: Plugin,
        board: Arc<MockBoard>,
        diag: Arc<CaptureDiag>,
        clock: Arc<FakeClock>,
    }

    const SERVICE: &str = "afkd::develop";

    /// The polled card's title: the delimiter a success line wraps it in, wide CJK, an
    /// emoji and an em dash.
    const TITLE: &str = "Fix \"the\" café — 修复 🚨";

    impl Fixture {
        /// A plugin over a mock board, not yet armed.
        fn new() -> Self {
            let board = Arc::new(MockBoard::new());
            let diag = Arc::new(CaptureDiag::default());
            let clock = Arc::new(FakeClock::new());
            let shared = Arc::clone(&board);
            let connect: Connect =
                Box::new(move |_| Box::new(Arc::clone(&shared)) as Box<dyn BoardClient>);
            let plugin = Plugin::with_connect(
                connect,
                Box::new(SharedClock(Arc::clone(&clock))),
                Box::new(Shared(Arc::clone(&diag))),
            );
            Self {
                plugin,
                board,
                diag,
                clock,
            }
        }

        /// The same, armed as the `card` kind over `settings`, as `afkd::develop`.
        fn armed(settings: serde_json::Value) -> Self {
            let mut f = Self::new();
            let hello = f.call(hello(settings));
            assert_eq!(hello["ok"], true, "{:?}", f.diag.errs());
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

        fn finish(&mut self, key: &serde_json::Value, outcome: &str) -> serde_json::Value {
            self.call(
                json!({"call": "finish", "id": "card1", "key": key, "outcome": outcome,
                       "facts": {"signal": "proceed", "reason": null, "duration_ms": 3,
                                 "cost": 0.0, "turns": null, "tokens": null,
                                 "run_name": "260926-100400-unit-card1-1"}}),
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

    /// The `hello` today's afkd writes, carrying `settings`.
    fn hello(settings: serde_json::Value) -> serde_json::Value {
        json!({"call": "hello", "proto": 2, "kind": TRELLO_KIND, "service": SERVICE,
               "roster": [SERVICE, "afkd::discuss"], "owner": "afkd-4242",
               "settings": settings})
    }

    fn settings() -> serde_json::Value {
        json!({"board": "https://trello.com/b/BID/afkd", "api_key": "k", "token": "t",
               "pick_from": "Up for Grabs"})
    }

    /// The board every test polls: `Up for Grabs` holding `card1`, and the two lists the
    /// hooks move to.
    fn seed(board: &MockBoard) {
        board.add_list("Up for Grabs");
        board.add_list("In Progress");
        board.add_list("Review");
        board.add_card("Up for Grabs", "card1", TITLE, "Body");
        board.set_clock(1000);
    }

    fn claims_on(board: &MockBoard, card: &str) -> usize {
        board
            .comments_on(card)
            .iter()
            .filter(|c| is_claim(&c.text))
            .count()
    }

    /// The accepted `hello` lists every optional call — `call` is proto 2's own and is
    /// never listed — and supplies the value `me`.
    #[test]
    fn hello_lists_every_optional_call_and_supplies_me() {
        let mut f = Fixture::new();
        assert_eq!(
            f.call(hello(settings())),
            json!({"ok": true, "proto": 2,
                   "calls": ["release", "renew", "comments", "attempt_failed", "classify"],
                   "values": {"me": "me"}})
        );
        assert!(f.diag.errs().is_empty(), "{:?}", f.diag.errs());
        assert!(f.board.calls().is_empty(), "hello touches no board");
    }

    /// Each refusal is `ok:false` with the problem, in the built-in's own words, on the
    /// diagnostic channel — and leaves the plugin unarmed.
    #[test]
    fn hello_refuses_what_the_kind_cannot_arm_with() {
        let mut without_identity = hello(settings());
        for field in ["service", "roster", "owner"] {
            without_identity.as_object_mut().unwrap().remove(field);
        }
        let mut empty_service = hello(settings());
        empty_service["service"] = json!("");
        let mut empty_owner = hello(settings());
        empty_owner["owner"] = json!("");
        let mut wrong_proto = hello(settings());
        wrong_proto["proto"] = json!(1);
        let mut wrong_kind = hello(settings());
        wrong_kind["kind"] = json!("trello_card");
        let mut settings_fault = hello(settings());
        settings_fault["settings"]["require_label"] = json!(true);
        let mut min_range = hello(settings());
        min_range["settings"]["min_age"] = json!("2m..3m");
        let identity = "afkd did not say which service this is; @afkd/trello needs an afkd \
                        whose `hello` carries `service`, `roster` and `owner`";
        for (request, problem) in [
            (without_identity, identity),
            (empty_service, identity),
            (empty_owner, identity),
            (
                wrong_proto,
                "afkd speaks plugin protocol 1, and this plugin speaks 2",
            ),
            (
                wrong_kind,
                "kind `trello_card` is not provided by @afkd/trello",
            ),
            (
                settings_fault,
                "trigger card: setting `require_label`: setting `require_label` expects a \
                 single value",
            ),
            (
                min_range,
                "trigger card: setting `min_age`: setting `min_age` is not a duration (try \
                 `30s`, `5m`, `1h`): `2m..3m`",
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

    /// A call from a later afkd is refused and the next request is still answered.
    #[test]
    fn an_unknown_call_is_refused() {
        let mut f = Fixture::armed(settings());
        seed(&f.board);
        assert_eq!(
            f.call(json!({"call": "rewind", "key": "k"})),
            json!({"ok": false})
        );
        assert_eq!(
            f.diag.errs(),
            ["afkd sent a call this plugin did not list in its `hello` reply"]
        );
        assert_eq!(f.poll()["fire"], true);
    }

    /// Every armed call hands the board a deadline [`CALL_BUDGET`] from the clock's now
    /// before it runs, and clears it once the reply is built; `hello` and an unknown call
    /// make no board request and hand it none. Each call runs at its own `now`, so a
    /// deadline inherited from the call before would show.
    #[test]
    fn every_armed_call_runs_under_the_call_budget_and_clears_it() {
        let mut f = Fixture::armed(settings());
        seed(&f.board);
        assert!(f.board.call_deadlines().is_empty(), "hello hands none");
        let scratch = TempDir::new("budget");

        let mut key = serde_json::Value::Null;
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
                "attempt_failed" => f.call(
                    json!({"call": "attempt_failed", "key": key, "n": 1, "max": 2,
                           "reason": "cargo test: 3 failed"}),
                ),
                "classify" => f.call(
                    json!({"call": "classify", "key": key, "scratch": scratch.path(),
                           "outcome": "clean"}),
                ),
                "finish" => f.finish(&key, "clean"),
                "call" => f.act("move_to", json!({"list": "Review"}), &key),
                _ => f.call(json!({"call": "release", "key": key})),
            };
            if call == "poll" {
                assert_eq!(reply["fire"], true, "{:?}", f.diag.errs());
                key = reply["unit"]["key"].clone();
            }
            let handed = f.board.call_deadlines();
            assert_eq!(
                handed[handed.len() - 2..],
                [Some(now + CALL_BUDGET), None],
                "`{call}` runs under its own deadline and clears it"
            );
        }
        assert_eq!(
            f.board.call_deadlines().len(),
            16,
            "one set and one clear each"
        );

        f.call(json!({"call": "rewind", "key": "k"}));
        assert_eq!(
            f.board.call_deadlines().len(),
            16,
            "an unknown call hands none"
        );
    }

    /// A board failure on `poll` is the built-in's idle beat, diagnosed; the next beat,
    /// with the board back, claims.
    #[test]
    fn a_poll_board_error_is_an_idle_beat_and_logged() {
        let mut f = Fixture::armed(settings());
        seed(&f.board);
        f.board.fail("list cards");
        assert_eq!(f.poll(), json!({"fire": false}));
        assert_eq!(
            f.diag.errs(),
            ["trello list cards: no response (mock failure)"]
        );
        f.board.clear_failure();
        assert_eq!(f.poll()["fire"], true);
    }

    /// The won unit's success line goes to the narration channel, not the diagnostic one.
    #[test]
    fn a_won_claim_narrates_and_hands_the_unit_over() {
        let mut f = Fixture::armed(settings());
        seed(&f.board);
        let unit = f.poll()["unit"].clone();
        assert_eq!(unit["key"], "card1#c1000");
        assert_eq!(unit["self"], SELF_ID);
        assert_eq!(
            f.diag.narrated(),
            [format!("[trello] claimed card \"{TITLE}\"")]
        );
        assert!(f.diag.errs().is_empty(), "{:?}", f.diag.errs());
    }

    #[test]
    fn classify_parks_on_the_marker_and_echoes_afkd_otherwise() {
        let mut f = Fixture::armed(settings());
        let scratch = TempDir::new("classify");
        let mut classify = |outcome: &str| {
            f.call(
                json!({"call": "classify", "key": "card1#c1", "scratch": scratch.path(),
                          "outcome": outcome}),
            )
        };
        assert_eq!(classify("clean"), json!({"outcome": "clean"}));
        assert_eq!(classify("failed"), json!({"outcome": "failed"}));
        std::fs::write(scratch.path().join(crate::PARK_FILE), b"").unwrap();
        assert_eq!(classify("failed"), json!({"outcome": "park"}));
        assert_eq!(classify("clean"), json!({"outcome": "park"}));
    }

    /// `attempt_failed` marks the live card with the fault's sentence, marks nothing for a
    /// fault with no sentence, and refuses a key the plugin holds no claim on.
    #[test]
    fn attempt_failed_marks_the_live_card_only() {
        let mut f = Fixture::armed(settings());
        seed(&f.board);
        let key = f.poll()["unit"]["key"].clone();
        assert_eq!(
            f.call(
                json!({"call": "attempt_failed", "key": key, "n": 1, "max": 2,
                          "reason": "cargo test: 3 failed\n\nsee run.log"})
            ),
            json!({"ok": true})
        );
        assert_eq!(
            f.call(
                json!({"call": "attempt_failed", "key": key, "n": 2, "max": 2,
                          "reason": null})
            ),
            json!({"ok": true})
        );
        let marks: Vec<String> = f
            .board
            .comments_on("card1")
            .into_iter()
            .filter(|c| c.text.starts_with("[afkd-attempt]"))
            .map(|c| c.text)
            .collect();
        assert_eq!(
            marks,
            ["[afkd-attempt] 1/2: cargo test: 3 failed\n\nsee run.log"]
        );
        assert_eq!(
            f.call(
                json!({"call": "attempt_failed", "key": "card9#c9", "n": 1, "max": 1,
                          "reason": "boom"})
            ),
            json!({"ok": false})
        );
        assert_eq!(
            f.diag.errs(),
            ["attempt_failed for card9#c9, which this plugin holds no claim on"]
        );
    }

    /// The `comments` reply maps each comment the way afkd's watch reads it — the
    /// ObjectId, the member id and the display name, the body verbatim, the post time as
    /// RFC 3339 — and leaves out what the brief already carried, afkd's own comments and
    /// every control marker. A second read reports only what is newer.
    #[test]
    fn a_comments_reply_sends_only_the_delta_and_drops_self_and_markers() {
        let mut f = Fixture::armed(settings());
        seed(&f.board);
        f.board
            .seed_comment_named("card1", "h0", "the ask", "mem-phil", "Phil Ek", 500);
        let unit = f.poll()["unit"].clone();
        assert_eq!(unit["seen"], json!(["h0", "c1000"]));
        f.board.seed_comment_named(
            "card1",
            "h1",
            "看起来不对 🚨\n\n```rust\nlet x = 1;\n```",
            "mem-chen",
            "陳大文",
            1_790_330_280,
        );
        f.board
            .seed_comment_by("card1", "self1", "my own reply", SELF_ID, 1_790_330_281);
        f.board.seed_comment_by(
            "card1",
            "rival",
            &claim_text("afkd-9"),
            "mem-other",
            1_790_330_282,
        );
        f.board.seed_comment_by(
            "card1",
            "park",
            "[afkd-park] service=afkd::discuss",
            "mem-other",
            1_790_330_283,
        );
        f.board
            .seed_comment_named("card1", "h2", "", "mem-alvaro", "Álvaro", 1_790_330_320);

        let reply = f.call(json!({"call": "comments", "key": unit["key"]}));
        assert_eq!(
            reply,
            json!({"comments": [
                {"id": "h1", "author": "mem-chen", "author_name": "陳大文",
                 "body": "看起来不对 🚨\n\n```rust\nlet x = 1;\n```", "at": "2026-09-25T09:58:00Z"},
                {"id": "h2", "author": "mem-alvaro", "author_name": "Álvaro",
                 "body": "", "at": "2026-09-25T09:58:40Z"},
            ]})
        );
        f.board.seed_comment_named(
            "card1",
            "h3",
            "Also the flag.",
            "mem-alvaro",
            "Álvaro",
            1_790_330_400,
        );
        let reply = f.call(json!({"call": "comments", "key": unit["key"]}));
        assert_eq!(reply["comments"].as_array().unwrap().len(), 1);
        assert_eq!(reply["comments"][0]["id"], "h3");
    }

    #[test]
    fn an_unreachable_board_or_an_unknown_key_is_null() {
        let mut f = Fixture::armed(settings());
        seed(&f.board);
        let key = f.poll()["unit"]["key"].clone();
        f.board.fail("read comments");
        assert_eq!(
            f.call(json!({"call": "comments", "key": key})),
            json!({"comments": null})
        );
        assert_eq!(
            f.call(json!({"call": "comments", "key": "card9#c9"})),
            json!({"comments": null})
        );
        assert_eq!(
            f.diag.errs(),
            [
                "trello read comments: no response (mock failure)",
                "comments for card9#c9, which this plugin holds no claim on",
            ]
        );
    }

    /// A thread too long for one line keeps the newest comments that fit, and names the
    /// ones it left out; they are not offered again.
    #[test]
    fn an_overflowing_thread_keeps_the_newest_and_names_the_rest() {
        let mut f = Fixture::armed(settings());
        seed(&f.board);
        let key = f.poll()["unit"]["key"].clone();
        for n in 1..=400u64 {
            f.board.seed_comment_named(
                "card1",
                &format!("h{n:03}"),
                &"ø".repeat(150),
                "mem-alvaro",
                "Álvaro",
                2_000 + n,
            );
        }
        let reply = f.call(json!({"call": "comments", "key": key}));
        let kept = reply["comments"].as_array().unwrap();
        assert!(kept.len() < 400);
        assert_eq!(kept.last().unwrap()["id"], "h400");
        let dropped = 400 - kept.len();
        let errs = f.diag.errs();
        assert_eq!(errs.len(), 1);
        assert!(
            errs[0].starts_with(&format!("{dropped} comments on card1 did not fit")),
            "{}",
            errs[0]
        );
        assert!(
            errs[0].ends_with(&format!(", h{dropped:03}")),
            "{}",
            errs[0]
        );
        assert_eq!(
            f.call(json!({"call": "comments", "key": key})),
            json!({"comments": []})
        );
    }

    /// `release` maps every key shape: a live unit (afkd's stop hand-back) is reversed and
    /// forgotten, a crash victim is reversed, and a key with no `#` names nothing.
    #[test]
    fn release_reverses_a_live_unit_and_a_victim_and_nulls_garbage() {
        let mut f = Fixture::armed(settings());
        seed(&f.board);
        f.board
            .add_card("In Progress", "card2", "Crashed mid-run", "");
        f.board
            .seed_comment("card2", "claim2", &claim_text("afkd-17"), 500);
        let key = f.poll()["unit"]["key"].clone();

        assert_eq!(
            f.call(json!({"call": "release", "key": key})),
            json!({"released": true})
        );
        assert_eq!(claims_on(&f.board, "card1"), 0);
        assert_eq!(
            f.call(json!({"call": "renew", "key": key, "renewal": 1})),
            json!({"ok": false}),
            "a released unit is forgotten"
        );
        assert_eq!(
            f.call(json!({"call": "release", "key": "card2#claim2"})),
            json!({"released": true})
        );
        assert_eq!(claims_on(&f.board, "card2"), 0);
        let cards: Vec<String> = f
            .board
            .list_cards("Up for Grabs")
            .unwrap()
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(cards, ["card1", "card2"], "the victim lands at the bottom");
        for key in ["garbage", ""] {
            assert_eq!(
                f.call(json!({"call": "release", "key": key})),
                json!({"released": null})
            );
        }
    }

    /// A finish that did not land is `held`: afkd keeps the claim and sends `release` for
    /// the key on a later beat. While the board is still down that `release` keeps the key
    /// (`false`); once it is back, it releases the claim and frees the key (`true`).
    #[test]
    fn an_undelivered_finish_is_held_and_a_later_release_delivers_it() {
        let mut f = Fixture::armed(settings());
        seed(&f.board);
        let key = f.poll()["unit"]["key"].clone();
        f.board.fail("delete comment");

        assert_eq!(f.finish(&key, "clean"), json!({"ok": true, "held": true}));
        assert_eq!(
            f.diag.errs(),
            [
                "trello delete comment: no response (mock failure)".to_string(),
                format!(
                    "could not deliver the finish for {}; afkd holds the claim and releases \
                     it on a later beat",
                    key.as_str().unwrap()
                ),
            ]
        );
        assert_eq!(claims_on(&f.board, "card1"), 1, "the lease is held");
        assert_eq!(
            f.call(json!({"call": "release", "key": key})),
            json!({"released": false}),
            "still owed"
        );
        assert_eq!(claims_on(&f.board, "card1"), 1);

        f.board.clear_failure();
        assert_eq!(
            f.call(json!({"call": "release", "key": key})),
            json!({"released": true})
        );
        assert_eq!(claims_on(&f.board, "card1"), 0);
        assert!(
            !f.board
                .actions()
                .iter()
                .any(|a| matches!(a, Action::Move { .. })),
            "a finish moves no card: the hooks are afkd's"
        );
    }

    /// A delivered finish is plain `ok`, and `finish` for a key the plugin never handed
    /// over is diagnosed and still `ok`, touching nothing.
    #[test]
    fn a_delivered_finish_is_ok_and_an_unknown_key_is_ok_and_diagnosed() {
        let mut f = Fixture::armed(settings());
        seed(&f.board);
        let key = f.poll()["unit"]["key"].clone();
        assert_eq!(f.finish(&key, "clean"), json!({"ok": true}));
        assert!(f.diag.errs().is_empty(), "{:?}", f.diag.errs());

        let before = f.board.calls();
        assert_eq!(f.finish(&key, "clean"), json!({"ok": true}));
        assert_eq!(f.board.calls(), before, "the finished unit is forgotten");
        assert_eq!(
            f.diag.errs(),
            [format!(
                "finish for {}, which this plugin holds no claim on",
                key.as_str().unwrap()
            )]
        );
    }

    /// `renew` edits the live claim in place and refuses a key the plugin holds no claim
    /// on.
    #[test]
    fn renew_edits_the_live_claim_and_refuses_an_unknown_key() {
        let mut f = Fixture::armed(settings());
        seed(&f.board);
        let key = f.poll()["unit"]["key"].clone();
        assert_eq!(
            f.call(json!({"call": "renew", "key": key, "renewal": 3})),
            json!({"ok": true})
        );
        assert!(f.board.actions().contains(&Action::EditComment {
            card: "card1".into(),
            comment: "c1000".into(),
            text: "[afkd-claim] owner=afkd-4242 renewal=3".into(),
        }));
        assert_eq!(
            f.call(json!({"call": "renew", "key": "card9#c9", "renewal": 1})),
            json!({"ok": false})
        );
        assert_eq!(
            f.diag.errs(),
            ["renew for card9#c9, which this plugin holds no claim on"]
        );
    }

    /// An oversized brief is cut to fit one line, with one diagnostic naming the card.
    #[test]
    fn an_oversized_brief_is_cut_and_said_so() {
        let mut f = Fixture::armed(settings());
        f.board.add_list("Up for Grabs");
        f.board.add_card(
            "Up for Grabs",
            "card1",
            "修复 the retry storm 🚨",
            &"看起来不对 🚨 — the retry path.\n".repeat(2_600),
        );
        let reply = f.poll();
        assert_eq!(reply["fire"], true);
        let brief = reply["unit"]["files"][0]["text"].as_str().unwrap();
        assert!(
            brief.ends_with("read the whole card with the trello skill]"),
            "{}",
            &brief[brief.len() - 120..]
        );
        assert_eq!(
            f.diag.errs(),
            ["the brief for card1 was cut to fit afkd's 64 KiB plugin line"]
        );
    }

    /// Each of the eight actions, as afkd sends a hook's call of it on the live unit, does
    /// its one board operation on the claimed card, narrates one line naming the card's
    /// title, and answers `ok` — the names and texts byte for byte, `me` read as the authed
    /// member, and a comment's `@{run:x}` left as afkd sent it.
    #[test]
    fn every_action_over_call_acts_on_the_live_card_and_narrates() {
        let mut f = Fixture::armed(settings());
        seed(&f.board);
        f.board.add_list("Blockerat / Väntar");
        let key = f.poll()["unit"]["key"].clone();
        let (actions, narrated) = (f.board.actions().len(), f.diag.narrated().len());
        let markdown = "## 完了 ✅\n\n- took 3m 12s\n- log: @{run:x}\n\nsee the run log.";
        for (action, args) in [
            ("add_member", json!({"member": "me"})),
            ("remove_member", json!({"member": "björn-öst"})),
            ("move_to", json!({"list": "In Progress", "at": "top"})),
            (
                "move_to",
                json!({"list": "Blockerat / Väntar", "at": "bottom"}),
            ),
            ("add_label", json!({"label": "reviewed ✅"})),
            ("remove_label", json!({"label": "Redo"})),
            ("comment", json!({"text": markdown})),
            ("mark_complete", json!({})),
            ("archive", json!({})),
        ] {
            assert_eq!(f.act(action, args, &key), json!({"ok": true}), "{action}");
        }
        assert!(f.diag.errs().is_empty(), "{:?}", f.diag.errs());
        let card = || "card1".to_string();
        assert_eq!(
            f.board.actions()[actions..],
            [
                Action::AddMember {
                    card: card(),
                    member: MemberRef::SelfMember,
                },
                Action::RemoveMember {
                    card: card(),
                    member: MemberRef::Username("björn-öst".into()),
                },
                Action::Move {
                    card: card(),
                    list: "In Progress".into(),
                    position: ListPosition::Top,
                },
                Action::Move {
                    card: card(),
                    list: "Blockerat / Väntar".into(),
                    position: ListPosition::Bottom,
                },
                Action::AddLabel {
                    card: card(),
                    label: "reviewed ✅".into(),
                },
                Action::RemoveLabel {
                    card: card(),
                    label: "Redo".into(),
                },
                Action::Complete(card()),
                Action::Archive(card()),
            ]
        );
        assert_eq!(
            f.board.comments_on("card1").last().map(|c| c.text.clone()),
            Some(markdown.to_string())
        );
        let title = TITLE;
        assert_eq!(
            f.diag.narrated()[narrated..],
            [
                format!("[trello] adding member \"me\" to card \"{title}\""),
                format!("[trello] removing member \"björn-öst\" from card \"{title}\""),
                format!("[trello] moving card \"{title}\" to list \"In Progress\" (at top)"),
                format!(
                    "[trello] moving card \"{title}\" to list \"Blockerat / Väntar\" (at bottom)"
                ),
                format!("[trello] adding label \"reviewed ✅\" to card \"{title}\""),
                format!("[trello] removing label \"Redo\" from card \"{title}\""),
                format!("[trello] commenting on card \"{title}\""),
                format!("[trello] marking card \"{title}\" complete"),
                format!("[trello] archiving card \"{title}\""),
            ]
        );
    }

    /// `poll` and `finish` move nothing themselves, and the post-run hook's calls, which
    /// afkd sends after `finish`, still reach the finished card — a `held` one's too —
    /// until afkd releases the key.
    #[test]
    fn a_post_run_call_acts_on_the_finished_card_until_it_is_released() {
        let mut f = Fixture::armed(settings());
        seed(&f.board);
        let key = f.poll()["unit"]["key"].clone();
        f.board.fail("delete comment");
        assert_eq!(f.finish(&key, "park"), json!({"ok": true, "held": true}));
        let moved = |f: &Fixture| {
            f.board
                .actions()
                .into_iter()
                .filter(|a| matches!(a, Action::Move { .. }))
                .count()
        };
        assert_eq!(moved(&f), 0, "poll and finish run no hook");

        assert_eq!(
            f.act("move_to", json!({"list": "Review", "at": "top"}), &key),
            json!({"ok": true})
        );
        assert_eq!(moved(&f), 1);
        f.board.clear_failure();
        assert_eq!(
            f.call(json!({"call": "release", "key": key})),
            json!({"released": true})
        );
        let released = f.diag.errs().len();
        assert_eq!(
            f.act("comment", json!({"text": "late"}), &key),
            json!({"ok": false, "error": format!(
                "comment for {}, which this plugin holds no card for",
                key.as_str().unwrap()
            )})
        );
        assert_eq!(
            f.diag.errs().len(),
            released + 1,
            "the refusal is diagnosed"
        );
    }

    /// The finished cards a post-run call reaches are bounded: past [`FINISHED_MAX`] the
    /// oldest is forgotten, and the newest still answers.
    #[test]
    fn the_finished_cards_are_bounded_oldest_first() {
        let mut f = Fixture::armed(settings());
        f.board.add_list("Up for Grabs");
        f.board.add_list("Review");
        let mut keys = Vec::new();
        for n in 0..=FINISHED_MAX {
            f.board.add_card(
                "Up for Grabs",
                &format!("card{n}"),
                &format!("卡片 {n}"),
                "",
            );
            f.board.set_clock(1000 + n as u64);
            let key = f.poll()["unit"]["key"].clone();
            assert_eq!(f.finish(&key, "clean"), json!({"ok": true}));
            // The `on_done` afkd runs next, so the next poll claims the next card.
            assert_eq!(
                f.act("move_to", json!({"list": "Review"}), &key),
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

    /// Each call the plugin cannot do answers `ok:false` with its sentence — which afkd
    /// fails the hook's `result` with — and says it on the diagnostic channel too: an
    /// action afkd would not bind, a bare run's `null` key, a key naming no card, and a
    /// board that refused or never answered.
    #[test]
    fn a_call_the_plugin_cannot_do_answers_its_sentence() {
        let mut f = Fixture::armed(settings());
        seed(&f.board);
        let key = f.poll()["unit"]["key"].clone();
        let before = f.board.actions().len();
        let mut expect = Vec::new();
        for (action, args, key, error) in [
            ("rename", json!({}), key.clone(), "no action `rename`"),
            (
                "move_to",
                json!({"list": "Review", "at": "sideways"}),
                key.clone(),
                "move_to: parameter at is top or bottom, not \"sideways\"",
            ),
            (
                "add_label",
                json!({}),
                key.clone(),
                "add_label: parameter label is required",
            ),
            (
                "archive",
                json!({}),
                json!(null),
                "archive acts on a card, and this run has none",
            ),
            (
                "archive",
                json!({}),
                json!("card9#c9"),
                "archive for card9#c9, which this plugin holds no card for",
            ),
            (
                "move_to",
                json!({"list": "Nirgendwo 🚫"}),
                key.clone(),
                "trello resolve list: no list named 'Nirgendwo 🚫'",
            ),
        ] {
            assert_eq!(
                f.act(action, args, &key),
                json!({"ok": false, "error": error}),
                "{action}"
            );
            expect.push(error.to_string());
        }
        f.board.fail("add member");
        assert_eq!(
            f.act("add_member", json!({"member": "me"}), &key),
            json!({"ok": false, "error": "trello add member: no response (mock failure)"})
        );
        expect.push("trello add member: no response (mock failure)".into());
        assert_eq!(f.diag.errs(), expect);
        assert_eq!(f.board.actions().len(), before, "nothing landed");
        assert_eq!(
            f.diag.narrated(),
            [format!("[trello] claimed card \"{TITLE}\"")],
            "and nothing was narrated as done"
        );
    }

    /// A `call` before an accepted `hello` is afkd out of step, like any other call.
    #[test]
    fn a_call_before_hello_is_fatal() {
        let mut f = Fixture::new();
        let request = serde_json::from_value(
            json!({"call": "call", "action": "archive", "args": {}, "key": "card1#c1"}),
        )
        .unwrap();
        assert!(matches!(
            f.plugin.answer(request),
            Answer::Fatal(reason) if reason.contains("before a `hello`")
        ));
    }
}
