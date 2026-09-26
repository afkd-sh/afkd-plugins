//! The plugin's state across calls, and one handler per call: the thin layer between
//! afkd's wire ([`crate::wire`]) and the `trello` kind's vendor half ([`crate::card`]).
//!
//! Four things the wire forces that the built-in never had to do:
//!
//! - **Who is asking arrives in `hello`.** The built-in read its service, roster and
//!   claim owner off afkd's config; here `hello` carries them, and a `hello` without them
//!   is refused, since the park owner cannot work without a service name.
//! - **Every reply fits one line.** afkd caps a line at 64 KiB. A `poll` whose brief would
//!   overflow it is cut to fit ([`fit_poll`]); a `comments` reply sends only what afkd has
//!   not been told yet, and if that still overflows, the newest that fit.
//! - **An undelivered terminal lifecycle is `held`.** The built-in held the claim when its
//!   terminal moment did not land and its reaper finished it on a later beat; `finish`
//!   answers `{"ok":true,"held":true}` and afkd asks `release` for the key on a later
//!   beat, which replays what is owed.
//! - **The lines go to stderr.** The success narration and the diagnostics alike, which
//!   afkd files under `[@afkd/trello:err]`.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use serde_json::json;

use crate::board::BoardClient;
use crate::card::{is_afkd, Identity, TrelloUnits, Unit};
use crate::client::TrelloClient;
use crate::common::{Clock, Diag};
use crate::rfc3339::format_utc;
use crate::settings::{board_config, BoardConfig};
use crate::wire::{
    fire_line, fit_comments, fit_poll, Facts, Request, UnitOutcome, WireComment, MAX_REPLY, PROTO,
};

/// The one kind this plugin provides.
pub(crate) const TRELLO_KIND: &str = "trello";

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
                        Some(_) => json!({"ok": true, "proto": PROTO, "calls": CALLS}),
                        None => json!({"ok": false, "proto": PROTO}),
                    }
                    .to_string(),
                )
            }
            Request::Poll => on_armed(armed, |a| a.poll(clock, diag)),
            Request::Release { key } => on_armed(armed, |a| a.release(&key, diag)),
            Request::Renew { key, renewal } => on_armed(armed, |a| a.renew(&key, renewal, diag)),
            Request::Comments { key } => on_armed(armed, |a| a.comments(&key, diag)),
            Request::Classify { scratch, outcome } => on_armed(armed, |_| {
                let outcome = TrelloUnits::classify(Path::new(&scratch), outcome);
                Answer::Reply(json!({ "outcome": outcome }).to_string())
            }),
            Request::AttemptFailed {
                key,
                n,
                max,
                reason,
            } => on_armed(armed, |a| {
                a.attempt_failed(&key, n, max, reason.as_deref(), diag)
            }),
            Request::Finish {
                key,
                outcome,
                facts,
            } => on_armed(armed, |a| a.finish(&key, outcome, &facts, diag)),
            Request::Unknown => {
                diag.err(&"afkd sent a call this plugin did not list in its `hello` reply");
                Answer::Reply(json!({"ok": false}).to_string())
            }
        }
    }
}

/// Run `f` on the armed service — or end the process, since afkd sends nothing but
/// `hello` before a `hello` it has seen accepted.
fn on_armed(armed: &mut Option<Armed>, f: impl FnOnce(&mut Armed) -> Answer) -> Answer {
    match armed {
        Some(armed) => f(armed),
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

    /// Release a journal key — a crashed run's, a `held` finish's, or a unit afkd handed
    /// straight back — and forget it if it was live.
    fn release(&mut self, key: &str, diag: &dyn Diag) -> Answer {
        self.live.remove(key);
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

    /// Run a finished unit's terminal lifecycle. A moment that did not land is `held`:
    /// afkd keeps the claim and sends `release` for the key on a later beat, which
    /// replays what is owed.
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
                "could not deliver the terminal lifecycle for {key}; afkd holds the claim and \
                 releases it on a later beat"
            ));
            json!({"ok": true, "held": true})
        };
        Answer::Reply(reply.to_string())
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
    use std::sync::Arc;

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
            let shared = Arc::clone(&board);
            let connect: Connect =
                Box::new(move |_| Box::new(Arc::clone(&shared)) as Box<dyn BoardClient>);
            let plugin = Plugin::with_connect(
                connect,
                Box::new(FakeClock::new()),
                Box::new(Shared(Arc::clone(&diag))),
            );
            Self {
                plugin,
                board,
                diag,
            }
        }

        /// The same, armed as the `trello` kind over `settings`, as `afkd::develop`.
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
                                 "run_name": "260926-100400-card-card1-1"}}),
            )
        }
    }

    /// The `hello` today's afkd writes, carrying `settings`.
    fn hello(settings: serde_json::Value) -> serde_json::Value {
        json!({"call": "hello", "proto": 1, "kind": TRELLO_KIND, "service": SERVICE,
               "roster": [SERVICE, "afkd::discuss"], "owner": "afkd-4242",
               "settings": settings})
    }

    fn settings() -> serde_json::Value {
        json!({"board": "https://trello.com/b/BID/afkd", "api_key": "k", "token": "t",
               "pick_from": "Up for Grabs"})
    }

    /// The board every test polls: `Up for Grabs` holding `card1`, and the two lists the
    /// lifecycle moves to.
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

    #[test]
    fn hello_lists_every_optional_call() {
        let mut f = Fixture::new();
        assert_eq!(
            f.call(hello(settings())),
            json!({"ok": true, "proto": 1,
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
        wrong_proto["proto"] = json!(2);
        let mut wrong_kind = hello(settings());
        wrong_kind["kind"] = json!("gitea");
        let mut settings_fault = hello(settings());
        settings_fault["settings"]["require_label"] = json!(true);
        let mut claim_cost = hello(settings());
        claim_cost["settings"]["on_claim"] = json!({"comment": ["claimed at @{run:cost}"]});
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
                "afkd speaks plugin protocol 2, and this plugin speaks 1",
            ),
            (wrong_kind, "kind `gitea` is not provided by @afkd/trello"),
            (
                settings_fault,
                "trigger trello: setting `require_label`: setting `require_label` expects a \
                 single value",
            ),
            (
                claim_cost,
                "trigger trello: setting `comment`: `@{run:cost}` references the run's facts, \
                 but no run happens at claim time",
            ),
            (
                min_range,
                "trigger trello: setting `min_age`: setting `min_age` is not a duration (try \
                 `30s`, `5m`, `1h`): `2m..3m`",
            ),
        ] {
            let mut f = Fixture::new();
            assert_eq!(f.call(request), json!({"ok": false, "proto": 1}));
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

    /// A terminal lifecycle that did not land is `held`: afkd keeps the claim and sends
    /// `release` for the key on a later beat. While the board is still down that
    /// `release` keeps the key (`false`); once it is back, it delivers the moment and
    /// frees the key (`true`).
    #[test]
    fn an_undelivered_finish_is_held_and_a_later_release_delivers_it() {
        let mut settings = settings();
        settings["on_done"] = json!({"move_to": [{"@value": "Review", "at": ["top"]}]});
        let mut f = Fixture::armed(settings);
        seed(&f.board);
        let key = f.poll()["unit"]["key"].clone();
        f.board.fail("move card");

        assert_eq!(f.finish(&key, "clean"), json!({"ok": true, "held": true}));
        assert_eq!(
            f.diag.errs(),
            [
                "trello move card: no response (mock failure)".to_string(),
                format!(
                    "could not deliver the terminal lifecycle for {}; afkd holds the claim \
                     and releases it on a later beat",
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
        assert!(f.board.actions().iter().any(
            |a| matches!(a, Action::Move { card, list, .. } if card == "card1" && list == "Review")
        ));
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
}
