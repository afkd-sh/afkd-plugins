//! The plugin's state across calls, and one handler per call: the thin layer between
//! afkd's wire ([`crate::wire`]) and the `intake` kind's vendor half ([`crate::card`]).
//!
//! **afkd runs the slots.** `on_claim` after a `poll` hands a unit over, and `on_done`,
//! `on_park` or `on_fail` after its `finish`; what a slot does to a card it does through
//! a read or a write on a `trello.Account`, which this plugin answers and which are no
//! claim's: a call is a read, a write, or refused as `no action`.
//!
//! **Every read and write reaches an account, not the service** ([`crate::reads`],
//! [`crate::writes`]). It comes as a `call`: to the trigger child from a `service(trello)`'s
//! slot, and from anywhere else to a child afkd starts for that one call, which gets no
//! `hello` and reads its `proto` off the call itself ([`once`]). Either way it is answered
//! through the client built for the account it names, and a trigger child keeps the clients it
//! has built ([`Clients`]), so `"me"` is resolved once per account for the child's life.
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
//!
//! A note of the plugin's own that Trello does not take — an attempt note, the discuss
//! backstop, the `[afkd-ran]` watermark — is said in one line and lost: the plugin keeps no
//! queue, and its only retry is the owed finish a `held` reply leaves for `release`.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde_json::{json, Map, Value};

use crate::account::Clients;
use crate::board::BoardClient;
use crate::card::{is_afkd, Identity, TrelloUnits, Unit};
use crate::client::TrelloClient;
use crate::common::{Clock, Diag, CALL_BUDGET};
use crate::reads::{self, READS};
use crate::rfc3339::format_utc;
use crate::settings::{board_config, BoardConfig};
use crate::wire::{
    fire_line, fit_comments, fit_poll, Facts, Fitted, Request, UnitOutcome, WireComment, MAX_REPLY,
    PROTO,
};
use crate::writes::{self, WRITES};

/// The one kind this plugin provides, its main one: a config writes `service(trello)`.
pub(crate) const TRELLO_KIND: &str = "intake";

/// The sentence the process ends on when a request that is no call for a child of its own
/// comes before a `hello` this plugin accepted: afkd is out of step.
const BEFORE_HELLO: &str = "afkd sent a call before a `hello` this plugin accepted";

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
    /// The clients the reads this child answered were made through, one per account.
    clients: Clients,
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
                state_dir,
            } => {
                let id = Identity {
                    owner,
                    service,
                    roster,
                };
                let state_dir = state_dir.map(PathBuf::from);
                let arming = arm(&self.connect, proto, &kind, &settings, id, state_dir)
                    .map_err(|problem| diag.err(&problem))
                    .ok();
                *armed = arming;
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
            } => on_armed(armed, clock, |a| {
                a.finish(&key, outcome, &facts, clock.now(), diag)
            }),
            Request::Call {
                action,
                args,
                proto,
            } => match armed {
                Some(_) => on_armed(armed, clock, |a| a.call(&action, &args, clock, diag)),
                None => once(&action, &args, proto, clock, diag),
            },
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
        None => Answer::Fatal(BEFORE_HELLO.into()),
    }
}

/// Answer the one `call` a child afkd started for it gets, with no `hello` before it: a read or
/// a write, under the call's own budget. An action no one declared is refused, as is a protocol this
/// plugin does not speak. A call with no `proto` at all is no such call: afkd is out of step,
/// as for any request before a `hello`.
fn once(
    action: &str,
    args: &Map<String, Value>,
    proto: Option<u32>,
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
    let answer = if READS.contains(&action) {
        reads::answer
    } else if WRITES.contains(&action) {
        writes::answer
    } else {
        return refuse(format!("no action `{action}`"));
    };
    let mut clients = Clients::default();
    Answer::Reply(answer(
        action,
        args,
        clock.now() + CALL_BUDGET,
        &mut clients,
        diag,
    ))
}

/// Arm the kind `hello` names over its settings, as `id`, or say why not. afkd has already
/// held the block to the manifest, so what can be wrong here is the protocol, the kind, a
/// missing identity, or a rule the manifest cannot express. `state_dir` is the plugin's own
/// directory, where a claim's record outlives the process.
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
        units: TrelloUnits::new(connect(&cfg), &cfg, id, state_dir),
        live: BTreeMap::new(),
        clients: Clients::default(),
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
        } else if let Some(Fitted {
            line,
            description_cut,
            brief_cut,
        }) = fit_poll(&wire)
        {
            if description_cut {
                diag.err(&format_args!(
                    "the description field of {} was cut to fit afkd's 64 KiB plugin line",
                    unit.thread()
                ));
            }
            if brief_cut {
                diag.err(&format_args!(
                    "the brief for {} was cut to fit afkd's 64 KiB plugin line",
                    unit.thread()
                ));
            }
            line
        } else {
            // Only a unit whose fields besides the brief overflow the line reaches this:
            // nothing can be handed over, so the claim is taken back before the service
            // ends.
            self.units.release_stale(&wire.key, clock.now(), diag);
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
    fn release(&mut self, key: &str, now: Instant, diag: &dyn Diag) -> Answer {
        self.live.remove(key);
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

    /// Finish a unit. A finish that did not land is `held`: afkd keeps the claim and sends
    /// `release` for the key on a later beat, which replays what is owed.
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
        Answer::Reply(reply.to_string())
    }

    /// Answer one call a slot made: a write ([`WRITES`]) or a read ([`READS`]) reaches the
    /// `trello.Account` it is passed through this child's client for it, and no card of this
    /// service, whoever claimed what. A write is done now or refused, never retried. Any
    /// other action is refused: `{"ok":false,"error":"no action `…`"}`, diagnosed too.
    fn call(
        &mut self,
        action: &str,
        args: &Map<String, Value>,
        clock: &dyn Clock,
        diag: &dyn Diag,
    ) -> Answer {
        if WRITES.contains(&action) {
            return Answer::Reply(writes::answer(
                action,
                args,
                clock.now() + CALL_BUDGET,
                &mut self.clients,
                diag,
            ));
        }
        if READS.contains(&action) {
            return Answer::Reply(reads::answer(
                action,
                args,
                clock.now() + CALL_BUDGET,
                &mut self.clients,
                diag,
            ));
        }
        let error = format!("no action `{action}`");
        diag.err(&error);
        Answer::Reply(json!({"ok": false, "error": error}).to_string())
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
    use crate::client::TRELLO_BASE;
    use crate::common::{CaptureDiag, FakeClock, TempDir};
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

        /// The same, armed as the `intake` kind over `settings`, as `afkd::develop`.
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
    }

    /// The `hello` today's afkd writes, carrying `settings`.
    fn hello(settings: serde_json::Value) -> serde_json::Value {
        json!({"call": "hello", "proto": 2, "kind": TRELLO_KIND, "service": SERVICE,
               "roster": [SERVICE, "afkd::discuss"], "owner": "afkd-4242",
               "settings": settings})
    }

    fn settings() -> serde_json::Value {
        json!({"board": "https://trello.com/b/BID/afkd", "api_key": "k", "token": "t",
               "claim_from": "Up for Grabs"})
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
    /// never listed.
    #[test]
    fn hello_lists_every_optional_call() {
        let mut f = Fixture::new();
        assert_eq!(
            f.call(hello(settings())),
            json!({"ok": true, "proto": 2,
                   "calls": ["release", "renew", "comments", "attempt_failed", "classify"]})
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
        settings_fault["settings"]["only_labels"] = json!(true);
        let mut min_range = hello(settings());
        min_range["settings"]["min_age"] = json!("2m..3m");
        // The old name in place of `claim_from`: afkd refuses it as unknown, and a `hello`
        // that carries it anyway has no `claim_from`.
        let mut pick_from_for_claim_from = hello(settings());
        let lists = pick_from_for_claim_from["settings"]
            .as_object_mut()
            .unwrap();
        lists.remove("claim_from");
        lists.insert("pick_from".into(), json!("Up for Grabs"));
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
                "trigger intake: setting `only_labels`: `only_labels` expects one or more label \
                 names",
            ),
            (
                min_range,
                "trigger intake: setting `min_age`: setting `min_age` is not a duration (try \
                 `30s`, `5m`, `1h`): `2m..3m`",
            ),
            (
                pick_from_for_claim_from,
                "trigger intake: setting `claim_from`: `claim_from` is required: name the list or \
                 lists new cards are claimed from",
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
            14,
            "one set and one clear each"
        );

        f.call(json!({"call": "rewind", "key": "k"}));
        assert_eq!(
            f.board.call_deadlines().len(),
            14,
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
        assert_eq!(unit["key"], "trello:card1");
        assert_eq!(unit["fields"]["list"], "Up for Grabs");
        assert_eq!(unit["fields"]["name"], TITLE);
        assert_eq!(unit["fields"].get("title"), None, "a card has no title");
        assert_eq!(
            unit["args"],
            json!({"account": {"api_key": "k", "token": "t", "base_url": TRELLO_BASE}})
        );
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
        // The description sits in the brief whole, so both are cut to share the line.
        assert_eq!(
            f.diag.errs(),
            [
                "the description field of card1 was cut to fit afkd's 64 KiB plugin line",
                "the brief for card1 was cut to fit afkd's 64 KiB plugin line"
            ]
        );
    }

    /// An armed child refuses a call that is no read and no write — each of today's old
    /// actions among them — by name: `ok:false` with the sentence, diagnosed, and the kind's
    /// board untouched, so afkd fails the slot's `result` with it.
    #[test]
    fn an_armed_child_refuses_a_call_that_is_no_read_and_no_write_by_name() {
        let mut f = Fixture::armed(settings());
        seed(&f.board);
        let key = f.poll()["unit"]["key"].clone();
        let (actions, calls) = (f.board.actions().len(), f.board.calls().len());
        let mut expect = Vec::new();
        for action in [
            "rename",
            "mark_complete",
            "archive",
            "move_to",
            "comment",
            "attach",
            "fetch_attachment",
        ] {
            let card = json!({"id": "card1", "key": key});
            let reply = f.call(json!({"call": "call", "action": action,
                                      "args": {"card": card, "text": "hej"}}));
            let error = format!("no action `{action}`");
            assert_eq!(reply, json!({"ok": false, "error": error}), "{action}");
            expect.push(error);
        }
        assert_eq!(f.diag.errs(), expect);
        assert_eq!(f.board.actions().len(), actions, "nothing landed");
        assert_eq!(f.board.calls().len(), calls, "the board was never asked");
    }

    /// A `call` before an accepted `hello` is afkd out of step, like any other call.
    #[test]
    fn a_call_before_hello_is_fatal() {
        let mut f = Fixture::new();
        let request = serde_json::from_value(json!({"call": "call", "action": "archive",
                   "args": {"card": {"id": "card1", "key": "card1#c1"}}}))
        .unwrap();
        assert!(matches!(
            f.plugin.answer(request),
            Answer::Fatal(reason) if reason.contains("before a `hello`")
        ));
    }

    /// A reply line `f` answered `request` with, as JSON.
    fn reply_of(f: &mut Fixture, request: serde_json::Value) -> serde_json::Value {
        let request = serde_json::from_value(request).expect("an afkd request");
        match f.plugin.answer(request) {
            Answer::Reply(line) => serde_json::from_str(&line).expect("a JSON reply"),
            Answer::Fatal(reason) => panic!("unexpected fatal: {reason}"),
        }
    }

    /// The one call a child afkd starts for it gets, with no `hello` before it, names its
    /// `proto`: a protocol this plugin does not speak is refused naming both, an action no one
    /// declared is refused, and so is each of the old actions, by name. Each is a reply,
    /// diagnosed, never the fatal a call with no `proto` is.
    #[test]
    fn a_one_shot_call_is_refused_by_name() {
        let acct = json!({"api_key": "k", "token": "t"});
        let once = |action: &str, proto: serde_json::Value, args: serde_json::Value| json!({"call": "call", "proto": proto, "action": action, "args": args});
        for (request, error) in [
            (
                once("boards", json!(1), json!({"acct": acct})),
                "afkd speaks plugin protocol 1, and this plugin speaks 2",
            ),
            (once("assign", json!(2), json!({})), "no action `assign`"),
            (
                once(
                    "archive",
                    json!(2),
                    json!({"card": {"id": "c", "key": "trello:c"}}),
                ),
                "no action `archive`",
            ),
            (
                once(
                    "cards",
                    json!(2),
                    json!({"acct": {"api_key": "", "token": "t"}}),
                ),
                "cards: acct.api_key is empty",
            ),
            (
                once(
                    "boards",
                    json!(2),
                    json!({"acct": {"api_key": "k", "token": ""}}),
                ),
                "boards: acct.token is empty",
            ),
            (
                once("lists", json!(2), json!({"acct": acct})),
                "lists: parameter board is required",
            ),
            (
                once(
                    "create_card",
                    json!(2),
                    json!({"acct": acct, "list": "Backlog", "name": "Plan"}),
                ),
                "create_card: parameter board is required",
            ),
            (
                once(
                    "delete_card",
                    json!(2),
                    json!({"acct": {"api_key": "k", "token": ""}, "card": {"id": "c", "key": "trello:c"}}),
                ),
                "delete_card: acct.token is empty",
            ),
            (
                once(
                    "delete_attachment",
                    json!(2),
                    json!({"acct": acct, "attachment": {"id": "a", "key": "trello:a"}}),
                ),
                "delete_attachment: attachment a has a key this plugin did not hand out: \
                 trello:a",
            ),
        ] {
            let mut f = Fixture::new();
            assert_eq!(
                reply_of(&mut f, request.clone()),
                json!({"ok": false, "error": error}),
                "{request}"
            );
            assert_eq!(f.diag.errs(), [error], "{request}");
            assert!(
                f.board.calls().is_empty(),
                "the kind's board is never touched"
            );
        }
        // No `proto`: no child of its own, so afkd is out of step — the before-hello fatal.
        let mut f = Fixture::new();
        let request = serde_json::from_value(json!({"call": "call", "action": "boards",
                                                   "args": {"acct": acct}}))
        .unwrap();
        assert!(matches!(
            f.plugin.answer(request),
            Answer::Fatal(reason) if reason.contains("before a `hello`")
        ));
    }

    /// An armed child answers a read the same way, whoever claimed what: an account with no
    /// token is refused in a reply, while the kind's own units go on being polled.
    #[test]
    fn an_armed_child_answers_a_read_through_the_account_it_is_passed() {
        let mut f = Fixture::armed(settings());
        seed(&f.board);
        let reply = f.call(json!({"call": "call", "action": "cards",
            "args": {"acct": {"api_key": "k", "token": ""}, "board": "BID"}}));
        assert_eq!(
            reply,
            json!({"ok": false, "error": "cards: acct.token is empty"})
        );
        assert_eq!(f.diag.errs(), ["cards: acct.token is empty"]);
        assert!(
            f.board.calls().is_empty(),
            "the service's board is not the account"
        );
        assert_eq!(f.poll()["fire"], true, "the service claims on");
    }

    /// An armed child answers a write the way it answers a read: a refusal at decode is a
    /// reply, diagnosed, with the kind's board untouched, and the service goes on claiming.
    /// An old action's name is no write's, and is refused as no action.
    #[test]
    fn an_armed_child_answers_a_write_refusal_and_an_old_action_is_no_action() {
        let mut f = Fixture::armed(settings());
        seed(&f.board);
        let account = json!({"api_key": "k", "token": "t"});
        for (action, args, error) in [
            (
                "delete_card",
                json!({"acct": {"api_key": "k", "token": ""}, "card": {"id": "c", "key": "trello:c"}}),
                "delete_card: acct.token is empty",
            ),
            (
                "create_comment",
                json!({"acct": account, "card": {"id": "c", "key": "trello:c"}}),
                "create_comment: parameter body is required",
            ),
            (
                "create_attachment",
                json!({"acct": account, "card": {"id": "c", "key": "trello:c"}, "path": "x.md"}),
                "create_attachment: path must be absolute, not \"x.md\"",
            ),
        ] {
            let reply = f.call(json!({"call": "call", "action": action, "args": args}));
            assert_eq!(reply, json!({"ok": false, "error": error}), "{action}");
        }
        assert_eq!(
            f.diag.errs(),
            [
                "delete_card: acct.token is empty",
                "create_comment: parameter body is required",
                "create_attachment: path must be absolute, not \"x.md\"",
            ]
        );
        assert!(
            f.board.calls().is_empty(),
            "the service's board is not the account"
        );
        // An old action is refused by name, whatever card it names.
        let reply = f.call(json!({"call": "call", "action": "archive",
            "args": {"card": {"id": "nope", "key": "trello:nope"}}}));
        assert_eq!(reply, json!({"ok": false, "error": "no action `archive`"}));
        assert_eq!(f.poll()["fire"], true, "the service claims on");
    }

    /// A `hello` that names the plugin's directory arms the service with it: the claim the
    /// first poll wins is recorded in a file there before the unit is handed over.
    #[test]
    fn hello_hands_the_service_the_directory_claims_are_recorded_in() {
        let dir = TempDir::new("hello-dir");
        let mut f = Fixture::new();
        let mut greeting = hello(settings());
        greeting["state_dir"] = json!(dir.path());
        assert_eq!(f.call(greeting)["ok"], true);
        seed(&f.board);
        let unit = f.poll()["unit"].clone();
        assert_eq!(unit["key"], "trello:card1");
        let record = std::fs::read_to_string(dir.path().join("claims/card1.json")).unwrap();
        let record: serde_json::Value = serde_json::from_str(&record).unwrap();
        assert_eq!(record["claim_id"], "c1000");
    }
}
