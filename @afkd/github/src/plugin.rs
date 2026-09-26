//! The plugin's state across calls, and one handler per call: the thin layer between
//! afkd's wire ([`crate::wire`]) and the armed kind's vendor half ([`crate::issue`],
//! behind [`crate::kind`]). Everything here is written once, against the seam, so a
//! second kind slots in beside the first.
//!
//! Three things the wire forces that the built-in never had to do:
//!
//! - **Every reply fits one line.** afkd caps a line at 64 KiB. A `poll` whose brief would
//!   overflow it is cut to fit ([`fit_poll`]); a `comments` reply sends only what afkd has
//!   not been told yet, and if that still overflows, the newest that fit.
//! - **The identity is resolved on whichever call needs it first.** afkd's reaper runs at
//!   the top of a beat, before its poll, so a fresh child's first call after a restart can
//!   be `release` of a crashed run's key — and GitHub's release unassigns the bot by login.
//!   Without the identity, `release` answers `false`, and afkd asks again next beat: the
//!   built-in's reaper waits for the identity the same way.
//! - **An undelivered terminal lifecycle is `held`.** The built-in holds the claim when
//!   its `on_done`/`on_fail` did not land and its reaper releases it on a later beat;
//!   `finish` answers `{"ok":true,"held":true}` and afkd does exactly that, with
//!   `release`.

use std::collections::{BTreeMap, HashSet};

use serde_json::json;

use crate::claim::is_claim;
use crate::client::{Github, GithubClient};
use crate::common::{Clock, Diag};
use crate::issue::IssueUnits;
use crate::kind::{ClaimedUnit, Units};
use crate::rfc3339::format_utc;
use crate::settings::{issue_config, GithubConfig};
use crate::wire::{
    fire_line, fit_comments, fit_poll, Facts, Request, UnitOutcome, WireComment, MAX_REPLY, PROTO,
};

/// The issue kind.
pub(crate) const ISSUE_KIND: &str = "github";

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
pub(crate) type Connect = Box<dyn Fn(&GithubConfig) -> Box<dyn GithubClient>>;

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
    fn finish(&mut self, key: &str, outcome: UnitOutcome, facts: &Facts, diag: &dyn Diag)
        -> Answer;
}

/// One armed service of kind `K`.
struct Armed<K: Units> {
    units: K,
    /// The token's login, resolved on the first call that needs it and kept.
    me: Option<String>,
    /// The units handed over and not yet finished or released, by key.
    live: BTreeMap<String, LiveUnit<K::Unit>>,
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
        let connect: Connect =
            Box::new(|cfg| Box::new(Github::new(&cfg.host, &cfg.token)) as Box<dyn GithubClient>);
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
        _ => return Err(format!("kind `{kind}` is not provided by @afkd/github")),
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
        }
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
}

impl<K: Units> Service for Armed<K> {
    fn calls(&self) -> &'static [&'static str] {
        K::CALLS
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

    /// Release a leftover journal key — a crashed run's, a `held` finish's, or a unit afkd
    /// handed straight back. Without the identity nothing is written and the key is kept
    /// (`false`), so a `/user` outage never drops an entry the release still owes.
    fn release(&mut self, key: &str, diag: &dyn Diag) -> Answer {
        self.live.remove(key);
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

    /// Run a finished unit's terminal lifecycle. A moment that did not land is `held`:
    /// afkd keeps the claim and sends `release` for the key on a later beat, which undoes
    /// the claim so the unit is retried from the top.
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
    //! The handlers over a mock forge: what each call answers, and the state the plugin
    //! keeps between them. The wire itself — a real child, real JSON lines, a real HTTP
    //! forge — is `tests/wire.rs`.

    use super::*;
    use crate::claim::{claim_key, claim_text};
    use crate::client::{Action, MockClient, Repo};
    use crate::common::{CaptureDiag, FakeClock};
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

    /// The token's login: non-ASCII and bracketed, as a GitHub App's bot login is.
    const ME: &str = "björn-öst[bot]";
    const REPO: &str = "acme/widgets";

    impl Fixture {
        /// A plugin over a mock forge whose token is `björn-öst[bot]`'s, not yet armed.
        fn new() -> Self {
            let mock = Arc::new(MockClient::new(ME));
            let diag = Arc::new(CaptureDiag::default());
            let forge = Arc::clone(&mock);
            let connect: Connect =
                Box::new(move |_| Box::new(Arc::clone(&forge)) as Box<dyn GithubClient>);
            let plugin = Plugin::with_connect(
                connect,
                Box::new(FakeClock::new()),
                Box::new(Shared(Arc::clone(&diag))),
            );
            Self { plugin, mock, diag }
        }

        /// The same, armed as the `github` kind over `settings`.
        fn armed(settings: serde_json::Value) -> Self {
            let mut f = Self::new();
            let hello = f.call(
                json!({"call": "hello", "proto": 1, "kind": ISSUE_KIND, "settings": settings}),
            );
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

        fn finish(&mut self, key: &serde_json::Value, outcome: &str) -> serde_json::Value {
            self.call(
                json!({"call": "finish", "id": "7", "key": key, "outcome": outcome,
                             "facts": {"signal": "proceed", "reason": null, "duration_ms": 3,
                                       "cost": 0.0, "turns": null, "tokens": null,
                                       "run_name": "260926-100400-issue-7-1"}}),
            )
        }
    }

    fn settings() -> serde_json::Value {
        json!({"repo": REPO, "token": "PAT", "source_label": "afkd/ready"})
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

    #[test]
    fn hello_lists_exactly_the_calls_the_kind_answers() {
        let mut f = Fixture::new();
        let reply = f.call(json!({"call": "hello", "proto": 1, "kind": "github",
                                  "service": "監視::triage", "roster": ["監視::triage"],
                                  "owner": "陳大文", "settings": settings()}));
        assert_eq!(
            reply,
            json!({"ok": true, "proto": 1, "calls": ["release", "renew", "comments"]})
        );
        assert!(f.diag.lines().is_empty(), "{:?}", f.diag.lines());
        assert_eq!(f.mock.user_reads(), 0, "hello touches no forge");
    }

    /// Each refusal is `ok:false` with the problem, in the built-in's own words, on the
    /// diagnostic channel — and leaves the plugin unarmed.
    #[test]
    fn hello_refuses_what_the_kind_cannot_arm_with() {
        for (kind, proto, settings, problem) in [
            (
                "github_pr_review",
                1,
                settings(),
                "kind `github_pr_review` is not provided by @afkd/github",
            ),
            (
                "gitea",
                1,
                settings(),
                "kind `gitea` is not provided by @afkd/github",
            ),
            (
                "github",
                2,
                settings(),
                "afkd speaks plugin protocol 2, and this plugin speaks 1",
            ),
            (
                "github",
                1,
                json!({"repo": "", "token": "PAT"}),
                "trigger github: setting `repo`: a github trigger needs a `repo` (`owner/name`)",
            ),
            (
                "github",
                1,
                json!({"repo": REPO, "token": "PAT",
                       "on_claim": {"comment": ["spent @{run:cost}"]}}),
                "trigger github: setting `comment`: `@{run:cost}` references the run's facts, \
                 but no run happens at claim time",
            ),
            (
                "github",
                1,
                json!({"repo": REPO, "token": "PAT", "on_done": {"label_add": [true]}}),
                "trigger github: setting `label_add`: `label_add` expects a label name",
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

    /// An identity that will not resolve is an idle beat, retried next poll — and once
    /// resolved it is kept: the forge is asked once for the plugin's whole life.
    #[test]
    fn an_identity_that_will_not_resolve_is_an_idle_beat_retried_next_poll() {
        let mut f = Fixture::armed(settings());
        f.mock.add_issue(7, "Fix", "do it", &["afkd/ready"]);
        f.mock.add_issue(8, "Fix too", "do it", &["afkd/ready"]);
        f.mock.fail("current user");
        assert_eq!(f.poll(), json!({"fire": false}));
        assert_eq!(
            f.diag.lines(),
            ["github current user: no response (mock failure)"]
        );
        f.mock.clear_failure();
        assert_eq!(f.poll()["unit"]["self"], ME);
        assert_eq!(f.poll()["unit"]["id"], "8");
        assert_eq!(f.mock.user_reads(), 2, "one failed ask, one that stuck");
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

    /// afkd's reaper runs before its poll, so a fresh child's first call can be `release`
    /// of a crashed run's key. The identity is resolved right there, and the release is
    /// the whole of the built-in's: the status label, the bot's assignment (and not the
    /// human's), and the marker. A following poll does not ask for the identity again.
    #[test]
    fn release_before_any_poll_resolves_the_identity_and_releases_in_full() {
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
        let mut f = Fixture::armed(settings());
        let key = crashed_claim(&f.mock);
        f.mock.fail("current user");

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
            ["github current user: no response (mock failure)"; 2]
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

    /// A terminal lifecycle that did not land is `held`: afkd keeps the claim and sends
    /// `release` for the key on a later beat, which undoes it — label, the bot's
    /// assignment, marker — so the next poll claims the issue afresh.
    #[test]
    fn an_undelivered_finish_is_held_and_a_later_release_recovers_it() {
        let mut settings = settings();
        settings["on_claim"] = json!({"assign_me": [true]});
        settings["on_done"] = json!({"close": [true]});
        let mut f = Fixture::armed(settings);
        f.mock
            .add_issue_assigned(7, "Fix", "do it", &["afkd/ready"], &["陳大文"]);
        let key = f.poll()["unit"]["key"].clone();
        assert_eq!(
            f.mock.assignees_of(7),
            vec!["陳大文".to_string(), ME.to_string()]
        );
        f.mock.fail("set state");

        assert_eq!(f.finish(&key, "clean"), json!({"ok": true, "held": true}));
        assert_eq!(
            f.diag.lines(),
            [
                "github set state: no response (mock failure)".to_string(),
                format!(
                    "could not deliver the terminal lifecycle for {}; afkd holds the claim \
                     and releases it on a later beat",
                    key.as_str().unwrap()
                ),
            ]
        );
        assert!(f.mock.has_label(7, "afkd/claimed"));
        assert_eq!(markers_on(&f.mock, 7), 0, "the marker goes regardless");
        assert_eq!(f.poll(), json!({"fire": false}), "still claimed");

        f.mock.clear_failure();
        assert_eq!(
            f.call(json!({"call": "release", "key": key})),
            json!({"released": true})
        );
        assert!(!f.mock.has_label(7, "afkd/claimed"));
        assert_eq!(f.mock.assignees_of(7), vec!["陳大文".to_string()]);
        assert_eq!(f.poll()["unit"]["id"], "7");
    }

    /// A delivered finish is plain `ok`, and `finish` for a key the plugin never handed
    /// over is diagnosed and still `ok`, touching nothing.
    #[test]
    fn a_delivered_finish_is_ok_and_an_unknown_key_is_ok_and_diagnosed() {
        let mut settings = settings();
        settings["on_done"] = json!({"close": [true]});
        let mut f = Fixture::armed(settings);
        f.mock.add_issue(7, "Fix", "do it", &["afkd/ready"]);
        let key = f.poll()["unit"]["key"].clone();
        assert_eq!(f.finish(&key, "clean"), json!({"ok": true}));
        assert!(f.diag.lines().is_empty(), "{:?}", f.diag.lines());

        // The finished unit is forgotten: a second `finish` for it closes nothing again.
        let before = f.mock.actions();
        assert_eq!(
            before
                .iter()
                .filter(|a| matches!(a, Action::State { .. }))
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
}
