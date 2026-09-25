//! The plugin's state across calls, and one handler per call: the thin layer between
//! afkd's wire ([`crate::wire`]) and the armed kind's vendor half ([`crate::issue`] or
//! [`crate::pr`], behind [`crate::kind`]). Everything here is written once and shared by
//! both kinds.
//!
//! Two things the wire forces that the built-in never had to do:
//!
//! - **Every reply fits one line.** afkd caps a line at 64 KiB. A `poll` whose brief would
//!   overflow it is cut to fit ([`fit_poll`]); a `comments` reply sends only what afkd has
//!   not been told yet, and if that still overflows, the newest that fit.
//! - **`finish` always answers `ok`.** afkd crashes the service on a refused `finish`, and
//!   has no "held" reply. The built-in's answer to a terminal lifecycle that did not land
//!   — hold the claim, release it on the next beat, and on a second failure for the same
//!   unit leave it for a human — is performed here, at once.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use serde_json::json;

use crate::claim::is_claim;
use crate::client::{Gitea, GiteaClient};
use crate::common::{ClaimFault, Clock, Diag};
use crate::issue::IssueUnits;
use crate::kind::{ClaimedUnit, Units};
use crate::pr::PrUnits;
use crate::rfc3339::format_utc;
use crate::settings::{issue_config, pr_config, GiteaConfig};
use crate::wire::{
    fire_line, fit_comments, fit_poll, Facts, Request, UnitOutcome, WireComment, MAX_REPLY, PROTO,
};

/// The issue kind.
pub(crate) const ISSUE_KIND: &str = "gitea";

/// The pull-request review kind.
pub(crate) const PR_KIND: &str = "gitea_pr_review";

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
    fn poll(&mut self, clock: &dyn Clock, diag: &dyn Diag) -> Answer;
    fn release(&mut self, key: &str, diag: &dyn Diag) -> Answer;
    fn renew(&self, key: &str, renewal: u64, diag: &dyn Diag) -> Answer;
    fn comments(&mut self, key: &str, diag: &dyn Diag) -> Answer;
    fn classify(&self, scratch: &Path, outcome: UnitOutcome) -> UnitOutcome;
    fn finish(&mut self, key: &str, outcome: UnitOutcome, facts: &Facts, diag: &dyn Diag)
        -> Answer;
}

/// One armed service of kind `K`.
struct Armed<K: Units> {
    units: K,
    /// The token's login, resolved on the first `poll` and kept.
    me: Option<String>,
    /// The units handed over and not yet finished or released, by key.
    live: BTreeMap<String, LiveUnit<K::Unit>>,
    /// The threads whose terminal lifecycle has failed once.
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
                Answer::Reply(
                    match armed {
                        Some(service) => {
                            json!({"ok": true, "proto": PROTO, "calls": service.calls()})
                        }
                        None => json!({"ok": false, "proto": PROTO}),
                    }
                    .to_string(),
                )
            }
            Request::Poll => on_armed(armed, |a| a.poll(clock, diag)),
            Request::Release { key } => on_armed(armed, |a| a.release(&key, diag)),
            Request::Renew { key, renewal } => on_armed(armed, |a| a.renew(&key, renewal, diag)),
            Request::Comments { key } => on_armed(armed, |a| a.comments(&key, diag)),
            Request::Classify { scratch, outcome } => on_armed(armed, |a| {
                let outcome = a.classify(Path::new(&scratch), outcome);
                Answer::Reply(json!({ "outcome": outcome }).to_string())
            }),
            Request::Finish {
                key,
                outcome,
                facts,
            } => on_armed(armed, |a| a.finish(&key, outcome, &facts, diag)),
            Request::Unknown => unlisted(diag),
        }
    }
}

/// The reply to a call this plugin did not list in its `hello` reply.
fn unlisted(diag: &dyn Diag) -> Answer {
    diag.err(&"afkd sent a call this plugin did not list in its `hello` reply");
    Answer::Reply(json!({"ok": false}).to_string())
}

/// Run `f` on the armed kind — or end the process, since afkd sends nothing but `hello`
/// before a `hello` it has seen accepted.
fn on_armed(
    armed: &mut Option<Box<dyn Service>>,
    f: impl FnOnce(&mut dyn Service) -> Answer,
) -> Answer {
    match armed {
        Some(armed) => f(armed.as_mut()),
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
            undelivered: HashSet::new(),
        }
    }
}

impl<K: Units> Service for Armed<K> {
    fn calls(&self) -> &'static [&'static str] {
        K::CALLS
    }

    /// One beat: resolve the identity if it is not yet known, then run the claim race. A
    /// transient forge failure is the built-in's idle beat, diagnosed; a definite one
    /// ends the service with its sentence.
    fn poll(&mut self, clock: &dyn Clock, diag: &dyn Diag) -> Answer {
        let me = match &self.me {
            Some(me) => me.clone(),
            None => match self.units.resolve_me() {
                Ok(me) => self.me.insert(me).clone(),
                Err(e) => {
                    diag.err(&e);
                    return idle();
                }
            },
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

    /// Release a leftover journal key — or a unit afkd handed straight back.
    fn release(&mut self, key: &str, diag: &dyn Diag) -> Answer {
        self.live.remove(key);
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

    /// Run a finished unit's terminal lifecycle. Always `ok` (see the module doc): a
    /// lifecycle that did not land is released at once so the next poll retries the
    /// unit, and the second time the same unit fails that way it is left for a human.
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
        if self.units.finish(&live.unit, outcome, facts, diag) {
            return ok;
        }
        let thread = live.unit.thread();
        if self.undelivered.insert(thread.clone()) {
            self.units.release(&live.unit, diag);
            diag.err(&format_args!(
                "could not deliver the terminal lifecycle for {key}; releasing the claim so \
                 the next poll retries it"
            ));
        } else {
            diag.err(&format_args!(
                "the terminal lifecycle for {thread} failed twice; leaving the claim in place \
                 for a human"
            ));
        }
        ok
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
    }

    impl Fixture {
        /// A plugin over a mock forge whose token is `björn-öst[bot]`'s, not yet armed.
        fn new() -> Self {
            let mock = Arc::new(MockClient::new("björn-öst[bot]"));
            let diag = Arc::new(CaptureDiag::default());
            let forge = Arc::clone(&mock);
            let connect: Connect =
                Box::new(move |_| Box::new(Arc::clone(&forge)) as Box<dyn GiteaClient>);
            let plugin = Plugin::with_connect(
                connect,
                Box::new(FakeClock::new()),
                Box::new(Shared(Arc::clone(&diag))),
            );
            Self { plugin, mock, diag }
        }

        /// The same, armed as the `gitea` kind over `settings`.
        fn armed(settings: serde_json::Value) -> Self {
            Self::armed_as(ISSUE_KIND, settings)
        }

        /// The same, armed as `kind` over `settings`.
        fn armed_as(kind: &str, settings: serde_json::Value) -> Self {
            let mut f = Self::new();
            let hello =
                f.call(json!({"call": "hello", "proto": 1, "kind": kind, "settings": settings}));
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
    }

    fn settings() -> serde_json::Value {
        json!({"repo": "acme/widgets", "token": "PAT", "source_label": "afkd/ready"})
    }

    /// A `gitea_pr_review` block over the bot's own PRs.
    fn pr_settings() -> serde_json::Value {
        json!({"repo": "acme/widgets", "token": "PAT", "author_me": true})
    }

    #[test]
    fn hello_lists_exactly_the_calls_the_kind_answers() {
        let mut f = Fixture::new();
        let reply =
            f.call(json!({"call": "hello", "proto": 1, "kind": "gitea", "settings": settings()}));
        assert_eq!(
            reply,
            json!({"ok": true, "proto": 1, "calls": ["release", "renew", "comments", "classify"]})
        );
    }

    /// The PR kind lists what it answers: no `classify`, since the built-in keeps the
    /// spine's default and never parks a PR.
    #[test]
    fn a_pr_hello_lists_release_renew_comments() {
        let mut f = Fixture::new();
        let reply = f.call(
            json!({"call": "hello", "proto": 1, "kind": "gitea_pr_review", "settings": pr_settings()}),
        );
        assert_eq!(
            reply,
            json!({"ok": true, "proto": 1, "calls": ["release", "renew", "comments"]})
        );
        assert!(f.diag.lines().is_empty(), "{:?}", f.diag.lines());
    }

    /// Each refusal is `ok:false` with the problem, in the built-in's own words, on the
    /// diagnostic channel — and leaves the plugin unarmed.
    #[test]
    fn hello_refuses_what_the_kind_cannot_arm_with() {
        for (kind, proto, settings, problem) in [
            (
                "gitlab",
                1,
                settings(),
                "kind `gitlab` is not provided by @afkd/gitea",
            ),
            (
                "gitea_pr_review",
                1,
                json!({"repo": "acme/widgets", "org": "acme", "token": "PAT", "author_me": true}),
                "trigger gitea_pr_review: setting `org`: a gitea trigger takes exactly one of \
                 `repo` or `org` (both were set)",
            ),
            (
                "gitea",
                2,
                settings(),
                "afkd speaks plugin protocol 2, and this plugin speaks 1",
            ),
            (
                "gitea",
                1,
                json!({"repo": "acme/widgets", "org": "acme", "token": "PAT"}),
                "trigger gitea: setting `org`: a gitea trigger takes exactly one of `repo` or \
                 `org` (both were set)",
            ),
            (
                "gitea",
                1,
                json!({"repo": "acme/widgets", "token": "PAT", "on_claim": {"comment": ["spent @{run:cost}"]}}),
                "trigger gitea: setting `comment`: `@{run:cost}` references the run's facts, \
                 but no run happens at claim time",
            ),
        ] {
            let mut f = Fixture::new();
            let reply = f
                .call(json!({"call": "hello", "proto": proto, "kind": kind, "settings": settings}));
            assert_eq!(reply, json!({"ok": false, "proto": 1}));
            assert_eq!(f.diag.lines(), [problem]);
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

    #[test]
    fn an_identity_that_will_not_resolve_is_an_idle_beat_retried_next_poll() {
        let mut f = Fixture::armed(settings());
        f.mock.add_issue(7, "Fix", "do it", &["afkd/ready"]);
        f.mock.fail("current user");
        assert_eq!(f.poll(), json!({"fire": false}));
        assert_eq!(
            f.diag.lines(),
            ["gitea current user: no response (mock failure)"]
        );
        f.mock.clear_failure();
        assert_eq!(f.poll()["unit"]["self"], "björn-öst[bot]");
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

    /// A terminal lifecycle that did not land still answers `ok` — afkd would crash the
    /// service otherwise. The first time, the claim is released so the next poll retries
    /// the issue; the second time for the same issue, it is left for a human.
    #[test]
    fn an_undelivered_finish_releases_once_then_leaves_the_claim() {
        let mut settings = settings();
        settings["on_done"] = json!({"label_add": ["undefined"]});
        let mut f = Fixture::armed(settings);
        f.mock.add_issue(7, "Fix", "do it", &["afkd/ready"]);
        let facts = json!({"signal": "proceed", "reason": null, "duration_ms": 3, "cost": 0.0,
                           "turns": null, "tokens": null, "run_name": "260925-100400-issue-7-1"});

        let key = f.poll()["unit"]["key"].clone();
        let reply = f.call(
            json!({"call": "finish", "id": "7", "key": key, "outcome": "clean", "facts": facts}),
        );
        assert_eq!(reply, json!({"ok": true}));
        assert!(!f.mock.has_label(7, "afkd/claimed"), "released for a retry");
        assert_eq!(
            f.diag.lines().last().unwrap(),
            &format!(
                "could not deliver the terminal lifecycle for {}; releasing the claim so the \
                 next poll retries it",
                key.as_str().unwrap()
            )
        );

        let key = f.poll()["unit"]["key"].clone();
        let reply = f.call(
            json!({"call": "finish", "id": "7", "key": key, "outcome": "clean", "facts": facts}),
        );
        assert_eq!(reply, json!({"ok": true}));
        assert!(f.mock.has_label(7, "afkd/claimed"), "left in place");
        assert_eq!(
            f.diag.lines().last().unwrap(),
            "the terminal lifecycle for acme/widgets#7 failed twice; leaving the claim in \
             place for a human"
        );
        assert_eq!(f.poll(), json!({"fire": false}), "and not re-claimed");
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
}
