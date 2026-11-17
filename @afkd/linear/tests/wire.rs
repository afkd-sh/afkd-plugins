//! The wire contract suite: the real `afkd-linear` exec, spoken to exactly as afkd speaks to
//! it (`docs/plugins.md`, "The trigger protocol"), against a stateful fake Linear reached
//! through the kind's own `base_url`. No afkd is involved.
//!
//! Every leg ends at [`end`]: stdin closed and the child held to the wire's discipline — a
//! clean exit, one reply line per request, nothing else on stdout — and the fake's record of
//! requests that did not conform to Linear's schema empty. So a line that strayed onto
//! stdout, and a request Linear would refuse, fails whichever leg sent it.

// A whole unit, its handle's every field among it, written as one `json!`, nests deeper than
// the default limit.
#![recursion_limit = "256"]

mod common;

use std::time::{Duration, Instant};

use serde_json::{json, Value};

use common::fake::{
    now_ms, png, utc, Author, FakeLinear, API_KEY, CLIENT_ID, CLIENT_SECRET, SCOPE, TEAM,
};
use common::{ws, Plugin, TempDir, DEVELOP, DISCUSS, MAX_LINE, OWNER};

/// An issue title carrying the delimiter a success line wraps it in, wide CJK, an emoji and
/// an em dash.
const TITLE: &str = "修复 the retry storm 🚨 — \"backoff\" resets";
const BODY: &str = "The client retries forever once the token expires.\n\n\
                    ```rust\nlet backoff = Duration::ZERO;\n```\n\n— reported by 陳大文";
/// The two humans on the issue, and what they said before afkd ever looked.
const PHIL: &str = "Please cap it at 30s; the header says so.";
const CHEN: &str = "看起来不对 🚨 — it resets on every 401:\n\n```\nPOST /graphql 401\n```";

/// The team every leg starts from: the states of each category, three users, and one issue
/// in Todo with a two-person thread.
struct Team {
    fake: FakeLinear,
    issue: String,
    phil: String,
    chen: String,
    /// The two human comments' ids.
    asked: [String; 2],
}

fn team() -> Team {
    let fake = FakeLinear::start();
    for (state, kind) in [
        ("Triage", "triage"),
        ("Todo", "unstarted"),
        ("Backlog", "backlog"),
        ("In Progress", "started"),
        ("Done", "completed"),
        ("Canceled", "canceled"),
    ] {
        fake.state(state, kind);
    }
    let phil = fake.user("Phil Ek", "phil", "phil@acme.example");
    let chen = fake.user("陳大文", "chen", "chen@acme.example");
    fake.user("Rival", "rival-host", "rival@acme.example");
    let issue = fake.issue("Todo", TITLE, BODY);
    let asked = [
        fake.comment(&issue, Author::User(phil.clone()), PHIL, 3600),
        fake.comment(&issue, Author::User(chen.clone()), CHEN, 1800),
    ];
    Team {
        fake,
        issue,
        phil,
        chen,
        asked,
    }
}

/// The service's settings, lowered to JSON as afkd lowers them: every value a string, a
/// list an array of them, an unset `str?` `null` — afkd's own two keys along for the ride —
/// and no slot, since afkd runs those.
fn settings(fake: &FakeLinear) -> Value {
    json!({
        "api_key": API_KEY,
        "client_id": null,
        "client_secret": null,
        "base_url": fake.base_url(),
        "team": [TEAM],
        "claim_from": ["Todo"],
        "every": "30s",
        "max_attempts": "2",
    })
}

/// The same service, authenticating as the OAuth app, its API key written `""` as a config
/// reading an unset variable writes it.
fn app_settings(fake: &FakeLinear) -> Value {
    with(
        settings(fake),
        json!({"api_key": "", "client_id": CLIENT_ID, "client_secret": CLIENT_SECRET}),
    )
}

/// `settings` with `extra` on top.
fn with(mut settings: Value, extra: Value) -> Value {
    for (k, v) in extra.as_object().unwrap() {
        settings[k] = v.clone();
    }
    settings
}

/// The grooming service's settings: `discuss_with`, over its own state.
fn discuss_settings(fake: &FakeLinear, who: Value) -> Value {
    with(
        settings(fake),
        json!({"claim_from": ["Triage"], "discuss_with": who}),
    )
}

/// End a leg: the child held to the wire's discipline, and every request it sent one Linear
/// would take. Everything it wrote to stderr comes back.
fn end(plugin: Plugin, fake: &FakeLinear) -> String {
    let stderr = plugin.finish();
    assert_eq!(
        fake.invalid_requests(),
        Vec::<String>::new(),
        "requests Linear's schema refuses"
    );
    stderr
}

/// The `finish` envelope's facts, as afkd writes them.
fn facts(signal: &str, reason: Option<&str>) -> Value {
    json!({"signal": signal, "reason": reason, "duration_ms": 168000, "cost": 0.4217,
           "turns": 3, "tokens": null, "run_name": "261027-095800-unit-ENG-101-1"})
}

fn finish(plugin: &mut Plugin, unit: &Value, outcome: &str, facts: Value) -> Value {
    plugin.call(
        json!({"call": "finish", "id": unit["id"], "key": unit["key"],
                       "outcome": outcome, "facts": facts}),
    )
}

/// The texts on an issue starting with `prefix`.
fn said(fake: &FakeLinear, issue: &str, prefix: &str) -> Vec<String> {
    fake.comments(issue)
        .into_iter()
        .filter(|c| c.body.starts_with(prefix))
        .map(|c| c.body)
        .collect()
}

/// The claim comments on an issue.
fn claims(fake: &FakeLinear, issue: &str) -> Vec<common::fake::Comment> {
    fake.comments(issue)
        .into_iter()
        .filter(|c| c.body.starts_with("[afkd-claim]"))
        .collect()
}

/// The operations sent, in order.
fn operations(fake: &FakeLinear) -> Vec<String> {
    fake.seen().into_iter().map(|s| s.operation).collect()
}

/// Park `unit`: classify it parked, have afkd's question on the issue, and finish it parked.
fn park(plugin: &mut Plugin, fake: &FakeLinear, issue: &str, unit: &Value, as_author: Author) {
    let scratch = TempDir::new("park");
    std::fs::write(scratch.path().join("park"), b"").unwrap();
    assert_eq!(
        plugin.call(
            json!({"call": "classify", "key": unit["key"], "scratch": scratch.path(),
                           "outcome": "clean"})
        ),
        json!({"outcome": "park"})
    );
    fake.comment(
        issue,
        as_author,
        "Ska videon visa appen eller webben? 📱",
        2,
    );
    assert_eq!(
        finish(
            plugin,
            unit,
            "park",
            facts("fault", Some("parked: awaiting a human reply"))
        ),
        json!({"ok": true})
    );
}

// --- hello ---

/// The accepted `hello` lists every optional call it answers, and reaches no Linear.
#[test]
fn hello_arms_and_lists_every_call_it_answers() {
    let t = team();
    let mut plugin = Plugin::spawn();
    assert_eq!(
        plugin.hello(settings(&t.fake)),
        json!({"ok": true, "proto": 2,
               "calls": ["release", "renew", "comments", "attempt_failed", "classify"]})
    );
    assert!(t.fake.seen().is_empty(), "hello touches no Linear");
    end(plugin, &t.fake);
}

/// Each credential afkd let through that is not exactly one of an API key or an app's id
/// and secret is refused at `hello`, naming the setting; so are a block without `team` or
/// `claim_from` (which afkd refuses first), an `only_*` filter `issues` would refuse, and
/// another kind.
#[test]
fn hello_refuses_each_wrong_auth_combination() {
    let t = team();
    let base = settings(&t.fake);
    let without = |keys: &[&str], extra: Value| {
        let mut s = with(base.clone(), extra);
        for key in keys {
            s.as_object_mut().unwrap().remove(*key);
        }
        s
    };
    for (kind, block, sentence) in [
        (
            "intake",
            with(
                base.clone(),
                json!({"client_id": CLIENT_ID, "client_secret": CLIENT_SECRET}),
            ),
            "trigger intake: setting `api_key`: write api_key or client_id and client_secret, \
             not both",
        ),
        (
            "intake",
            with(base.clone(), json!({"api_key": null})),
            "trigger intake: setting `api_key`: needs api_key, or client_id and client_secret",
        ),
        (
            "intake",
            with(base.clone(), json!({"api_key": "", "client_id": CLIENT_ID})),
            "trigger intake: setting `client_id`: client_id needs client_secret",
        ),
        (
            "intake",
            without(&["api_key"], json!({"client_secret": CLIENT_SECRET})),
            "trigger intake: setting `client_secret`: client_secret needs client_id",
        ),
        (
            "intake",
            without(&["team"], json!({})),
            "trigger intake: setting `team`: `team` is required: name each team by its key",
        ),
        (
            "intake",
            without(&["claim_from"], json!({})),
            "trigger intake: setting `claim_from`: `claim_from` is required: name the \
             workflow state or states new issues are claimed from",
        ),
        (
            "intake",
            with(base.clone(), json!({"only_parent": ["the epic 🧭"]})),
            "trigger intake: setting `only_parent`: only_parent \"the epic 🧭\" is neither an \
             identifier (ENG-123) nor a uuid",
        ),
        (
            "card",
            base.clone(),
            "kind `card` is not provided by @afkd/linear",
        ),
        (
            "card",
            base.clone(),
            "kind `card` is not provided by @afkd/linear",
        ),
    ] {
        let mut plugin = Plugin::spawn();
        assert_eq!(
            plugin.call(
                json!({"call": "hello", "proto": 2, "kind": kind, "service": DEVELOP,
                               "roster": [DEVELOP, DISCUSS], "owner": OWNER, "settings": block})
            ),
            json!({"ok": false, "proto": 2})
        );
        assert_eq!(
            plugin.stderr_soon(sentence),
            format!("afkd-linear: {sentence}\n")
        );
        end(plugin, &t.fake);
    }
    assert!(
        t.fake.seen().is_empty(),
        "a refused hello touches no Linear"
    );
}

// --- claiming ---

/// The won race hands over the unit: the identifier as its id, the issue's uuid and the
/// claim's as its key, the uuid as its thread, every field of its handle, the claim-read
/// thread as `seen`, the viewer as `self`, every `LINEAR_*` env value, and the brief
/// unframed — and the claim comment on the issue is the viewer's.
#[test]
fn a_won_race_hands_over_the_unit() {
    let t = team();
    for label in ["afkd/ready", "Väntar på svar"] {
        t.fake.label(&t.issue, label);
    }
    let filed = 1_793_000_000_000;
    t.fake.set_times(&t.issue, filed, filed + 3_600_000);
    let mut plugin = Plugin::armed(settings(&t.fake));
    let reply = plugin.poll();
    assert_eq!(reply["fire"], true, "{}", plugin.stderr());
    let claim = claims(&t.fake, &t.issue);
    assert_eq!(claim.len(), 1, "one claim, ours");
    let claim = &claim[0];
    assert_eq!(claim.body, "[afkd-claim] owner=afkd-4242");
    assert_eq!(claim.author, Author::User(t.fake.key_user()));
    assert_eq!(
        reply["unit"],
        json!({
            "id": "ENG-101",
            "key": format!("{}#{}", t.issue, claim.id),
            "fields": {
                "uuid": t.issue, "workspace": "acme", "team": "ENG", "title": TITLE,
                "url": t.fake.url(&t.issue), "description": BODY,
                "branch": t.fake.branch(&t.issue), "state": "Todo", "state_type": "unstarted",
                "priority": "none", "estimate": null, "due": null, "assignee": null,
                "delegate": null, "creator": null, "project": null, "milestone": null,
                "cycle": null, "labels": ["afkd/ready", "Väntar på svar"], "parent": null,
                "sub_issues": [], "blocks": [], "blocked_by": [], "related": [], "similar": [],
                "duplicates": [], "duplicate_of": null, "links": [], "subscribers": [],
                "created_at": filed, "updated_at": filed + 3_600_000, "completed_at": null,
                "canceled_at": null, "archived": false, "trashed": false,
            },
            "thread": t.issue,
            "seen": [claim.id, t.asked[1], t.asked[0]],
            "self": t.fake.key_user(),
            "env": {
                "LINEAR_API_URL": format!("{}/graphql", t.fake.base_url()),
                "LINEAR_AUTHORIZATION": API_KEY,
                "LINEAR_ISSUE": "ENG-101",
                "LINEAR_ISSUE_ID": t.issue,
            },
            "args": {"workspace": {"api_key": API_KEY, "client_id": null, "client_secret": null,
                                   "base_url": t.fake.base_url()}},
            "files": [{"path": "task.md", "text": format!(
                "{TITLE}\n\n{BODY}\n\n## New comments\n\n**Phil Ek:** {PHIL}\n\n**陳大文:** {CHEN}\n"
            )}],
        })
    );
    assert_eq!(
        plugin.stderr_soon("claimed"),
        format!("[linear] claimed issue ENG-101 \"{TITLE}\"\n")
    );
    // The race is post → settle → re-read: the claim went up before the thread was read.
    let ops = operations(&t.fake);
    let post = ops
        .iter()
        .position(|o| o == "CommentCreate")
        .expect("the claim was posted");
    assert_eq!(ops[post + 1], "IssueComments");
    // The claimed issue, its claim live, is not claimed again.
    assert_eq!(plugin.poll(), json!({"fire": false}));
    end(plugin, &t.fake);
}

/// With nothing claimable in the first state — its one issue under a live claim — the poll
/// claims from the second, and the key names the state the issue came from.
#[test]
fn claim_from_names_two_states_and_claims_from_the_second() {
    let t = team();
    let rival = t.fake.user("Rival Two", "rival-two", "rival2@acme.example");
    t.fake.comment(
        &t.issue,
        Author::User(rival),
        "[afkd-claim] owner=afkd-17 renewal=3",
        5,
    );
    let second = t.fake.issue(
        "Backlog",
        "Hjälpvideo: så bokar du — steg för steg 🎬",
        "Två minuter.",
    );
    let mut plugin = Plugin::armed(with(
        settings(&t.fake),
        json!({"claim_from": ["Todo", "Backlog"]}),
    ));
    let unit = plugin.poll()["unit"].clone();
    assert_eq!(unit["id"], t.fake.identifier(&second));
    let claim = &claims(&t.fake, &second)[0];
    let backlog = t
        .fake
        .seen()
        .into_iter()
        .find_map(|s| {
            (s.operation == "Issues" && s.variables["filter"]["state"].is_object())
                .then(|| {
                    s.variables["filter"]["state"]["id"]["eq"]
                        .as_str()
                        .map(str::to_string)
                })
                .flatten()
        })
        .expect("a state was read");
    assert_ne!(backlog, "", "{backlog}");
    assert!(
        unit["key"]
            .as_str()
            .unwrap()
            .starts_with(&format!("{second}#{}#", claim.id)),
        "the key names the second state: {}",
        unit["key"]
    );
    assert_eq!(unit["fields"]["state"], "Backlog");
    assert_eq!(
        claims(&t.fake, &t.issue).len(),
        1,
        "the held issue is not touched"
    );
    end(plugin, &t.fake);
}

/// Two teams, two states each: the scan reads team by team and state by state in the order
/// written, a key keeps every state but the first team's first, a unit's handle names its own
/// team, and a crashed claim goes back to the bottom of the state of its own team it came from.
#[test]
fn claim_from_scans_team_by_team_and_state_by_state() {
    let t = team();
    t.fake.team("WEB", "Webb & Öppen källkod 🌐");
    let web_todo = t.fake.team_state("WEB", "Todo", "unstarted");
    t.fake.team_state("WEB", "Backlog", "backlog");
    let eng_backlog = t
        .fake
        .issue("Backlog", "Bakgrund: stadsvy i skymning 🌆", "");
    let web_todo_issue = t
        .fake
        .team_issue("WEB", "Todo", "Hjälpvideo — steg för steg 🎬", "");
    let web_waiting = t.fake.team_issue("WEB", "Todo", "Väntar på svar", "");
    let web_backlog = t.fake.team_issue("WEB", "Backlog", "Pris: 1 299 kr", "");
    let mut plugin = Plugin::armed(with(
        settings(&t.fake),
        json!({"team": [TEAM, "WEB"], "claim_from": ["Todo", "Backlog"]}),
    ));
    let mut units = Vec::new();
    for want in [
        &t.issue,
        &eng_backlog,
        &web_todo_issue,
        &web_waiting,
        &web_backlog,
    ] {
        let unit = plugin.poll()["unit"].clone();
        assert_eq!(&unit["thread"], want, "{}", plugin.stderr());
        units.push(unit);
    }
    assert_eq!(plugin.poll(), json!({"fire": false}), "every issue is held");
    let key = |n: usize| units[n]["key"].as_str().unwrap().to_string();
    assert_eq!(
        key(0).matches('#').count(),
        1,
        "the first team's first state"
    );
    assert_eq!(key(2).matches('#').count(), 2, "another team's first state");
    assert!(key(2).ends_with(&format!("#{web_todo}")), "{}", key(2));
    assert_eq!(units[0]["fields"]["team"], TEAM);
    assert_eq!(units[2]["fields"]["team"], "WEB");

    assert_eq!(
        plugin.call(json!({"call": "release", "key": key(2)})),
        json!({"released": true})
    );
    assert_eq!(
        t.fake.in_team_state("WEB", "Todo"),
        [web_waiting.clone(), web_todo_issue.clone()],
        "to the bottom of its own team's state"
    );
    assert!(claims(&t.fake, &web_todo_issue).is_empty());
    end(plugin, &t.fake);
}

/// A `claim_from` state one of the teams lacks fails the poll naming the state and the team,
/// before any issue is read or written.
#[test]
fn a_claim_from_state_a_team_lacks_is_an_idle_beat_naming_both() {
    let t = team();
    t.fake.team("WEB", "Web");
    t.fake.team_state("WEB", "Todo", "unstarted");
    let mut plugin = Plugin::armed(with(
        settings(&t.fake),
        json!({"team": [TEAM, "WEB"], "claim_from": ["Todo", "Backlog"]}),
    ));
    assert_eq!(plugin.poll(), json!({"fire": false}));
    assert_eq!(
        plugin.stderr_soon("Backlog"),
        "afkd-linear: linear resolve state: no workflow state named 'Backlog' in team WEB\n"
    );
    assert!(
        !operations(&t.fake).contains(&"CommentCreate".to_string()),
        "nothing is posted"
    );
    end(plugin, &t.fake);
}

/// Each `only_*` setting and `skip_labels` claims only what the `issues` filter of its name
/// keeps: the team's first Todo issue, which the filter drops, is passed over without a word
/// on it, and the one it keeps is claimed.
#[test]
fn each_only_setting_claims_only_what_its_filter_keeps() {
    type Seed = fn(&Team) -> String;
    let cases: [(&str, Value, bool, Seed); 13] = [
        (
            "only_state_type",
            json!({"claim_from": ["Todo", "Backlog"],
                                   "only_state_type": ["backlog"]}),
            false,
            |t| t.fake.issue("Backlog", "Bakgrund 🌆", ""),
        ),
        (
            "only_priority",
            json!({"only_priority": ["urgent", "high"]}),
            false,
            |t| {
                let kept = t.fake.issue("Todo", "Brådskande 🚨", "");
                t.fake.set_priority(&kept, 2);
                kept
            },
        ),
        (
            "only_labels",
            json!({"only_labels": ["afkd/ready", "Väntar på svar"]}),
            false,
            |t| {
                let kept = t.fake.issue("Todo", "Ready ✅", "");
                t.fake.label(&kept, "afkd/ready");
                t.fake.label(&kept, "Väntar på svar");
                t.fake.label(&t.issue, "afkd/ready");
                kept
            },
        ),
        (
            "only_assignee me, none",
            json!({"only_assignee": ["me", "none"]}),
            false,
            |t| {
                t.fake.assign(&t.issue, &t.phil);
                t.fake.issue("Todo", "Nobody's — 自由", "")
            },
        ),
        (
            "only_assignee by name",
            json!({"only_assignee": ["chen"]}),
            false,
            |t| {
                let kept = t.fake.issue("Todo", "Chen's", "");
                t.fake.assign(&kept, &t.chen);
                kept
            },
        ),
        (
            "only_assignee by email",
            json!({"only_assignee": ["chen@acme.example"]}),
            false,
            |t| {
                let kept = t.fake.issue("Todo", "Chen's", "");
                t.fake.assign(&kept, &t.chen);
                kept
            },
        ),
        (
            "only_delegate me, as the app",
            json!({"only_delegate": ["me"]}),
            true,
            |t| {
                t.fake.assign(&t.issue, &t.phil);
                let kept = t.fake.issue("Todo", "Handed to the app 🤖", "");
                t.fake.assign(&kept, &t.phil);
                t.fake.delegate(&kept, &t.fake.app_user());
                kept
            },
        ),
        (
            "only_project",
            json!({"only_project": ["Trackers 🧭"]}),
            false,
            |t| {
                let project = t.fake.project("Trackers 🧭", &[TEAM], &[]);
                let kept = t.fake.issue("Todo", "In the project", "");
                t.fake.set_project(&kept, &project, None);
                kept
            },
        ),
        (
            "only_milestone",
            json!({"only_milestone": ["Beta — 2"]}),
            false,
            |t| {
                let project = t.fake.project("Trackers 🧭", &[TEAM], &[("Beta — 2", 1.0)]);
                t.fake.set_project(&t.issue, &project, None);
                let kept = t.fake.issue("Todo", "In the milestone", "");
                t.fake.set_project(&kept, &project, Some("Beta — 2"));
                kept
            },
        ),
        ("only_cycle", json!({"only_cycle": ["12"]}), false, |t| {
            let now = now_ms();
            let past = t
                .fake
                .cycle(TEAM, 11, None, now - 9_000_000, now - 1_000, false);
            let cycle = t
                .fake
                .cycle(TEAM, 12, Some("Höst"), now - 1_000, now + 9_000_000, true);
            t.fake.set_cycle(&t.issue, &past);
            let kept = t.fake.issue("Todo", "In cycle 12", "");
            t.fake.set_cycle(&kept, &cycle);
            kept
        }),
        (
            "only_parent",
            json!({"only_parent": ["ENG-102"]}),
            false,
            |t| {
                let epic = t.fake.issue("Backlog", "Epic: 重试 that works", "");
                assert_eq!(t.fake.identifier(&epic), "ENG-102");
                let kept = t.fake.issue("Todo", "Cap the backoff", "");
                t.fake.set_parent(&kept, &epic);
                kept
            },
        ),
        ("only_text", json!({"only_text": "AT 30S"}), false, |t| {
            t.fake.issue("Todo", "Cap the backoff at 30s", "")
        }),
        (
            "skip_labels",
            json!({"skip_labels": ["Blockerat / Väntar", "Hold"]}),
            false,
            |t| {
                t.fake.label(&t.issue, "Blockerat / Väntar");
                t.fake.issue("Todo", "Free", "")
            },
        ),
    ];
    for (case, extra, app, seed) in cases {
        let t = team();
        let kept = seed(&t);
        let base = if app {
            app_settings(&t.fake)
        } else {
            settings(&t.fake)
        };
        let mut plugin = Plugin::armed(with(base, extra));
        assert_eq!(
            plugin.poll()["unit"]["thread"],
            kept,
            "{case}: {}",
            plugin.stderr()
        );
        assert!(
            claims(&t.fake, &t.issue).is_empty(),
            "{case}: the dropped issue is not touched"
        );
        assert_eq!(
            plugin.poll(),
            json!({"fire": false}),
            "{case}: nothing else passes"
        );
        end(plugin, &t.fake);
    }
}

/// Every slot is passed the workspace the service's credential settings build, and a slot's
/// write through it reaches Linear with the service's credential: the API key as it is, or
/// the app's token fetched with its id and secret. The issue the service claimed is the
/// handle it passes.
#[test]
fn a_slots_write_reaches_linear_with_the_services_credential() {
    for app in [false, true] {
        let t = team();
        let base = if app {
            app_settings(&t.fake)
        } else {
            settings(&t.fake)
        };
        let mut plugin = Plugin::armed(base);
        let unit = plugin.poll()["unit"].clone();
        let workspace = unit["args"]["workspace"].clone();
        let (client_id, client_secret) = t.fake.app_credentials();
        assert_eq!(
            workspace,
            if app {
                json!({"api_key": null, "client_id": client_id, "client_secret": client_secret,
                       "base_url": t.fake.base_url()})
            } else {
                json!({"api_key": API_KEY, "client_id": null, "client_secret": null,
                       "base_url": t.fake.base_url()})
            }
        );
        let issue = json!({"id": unit["id"], "key": unit["key"]});
        let moved = plugin.read_call(
            "update_issue",
            json!({"ws": workspace, "issue": issue, "state": "In Progress", "at": "top"}),
        );
        assert_eq!(moved["ok"], true, "{moved}");
        assert_eq!(moved["handle"]["fields"]["state"], "In Progress");
        assert_eq!(t.fake.state_of(&t.issue), "In Progress");
        let write = t
            .fake
            .seen()
            .into_iter()
            .rfind(|s| s.operation == "IssueWrite")
            .expect("the update went out");
        let authorization = write.authorization.unwrap_or_default();
        if app {
            assert!(authorization.starts_with("Bearer "), "{authorization}");
            assert!(t.fake.token_fetches() >= 1);
        } else {
            assert_eq!(authorization, API_KEY);
        }
        end(plugin, &t.fake);
    }
}

/// `discuss_with anyone`: an issue fires on first sight; a turn that spoke leaves afkd the
/// last speaker, so it waits; a human's reply re-arms it.
#[test]
fn discuss_with_anyone_claims_on_first_sight_then_waits_for_a_reply() {
    let t = team();
    let groomed = t.fake.issue("Triage", "Should we cap the backoff?", "");
    let mut plugin = Plugin::armed_as(DISCUSS, discuss_settings(&t.fake, json!(["anyone"])));
    let unit = plugin.poll()["unit"].clone();
    assert_eq!(unit["thread"], groomed);
    t.fake.comment(
        &groomed,
        Author::User(t.fake.key_user()),
        "I'd cap it at 30s. Agreed?",
        0,
    );
    assert_eq!(
        finish(&mut plugin, &unit, "clean", facts("proceed", None)),
        json!({"ok": true})
    );
    assert_eq!(plugin.poll(), json!({"fire": false}), "afkd spoke last");
    t.fake
        .comment(&groomed, Author::User(t.chen.clone()), "Agreed — 30s.", 0);
    assert_eq!(
        plugin.poll()["unit"]["thread"],
        groomed,
        "a reply re-arms it"
    );
    end(plugin, &t.fake);
}

/// `discuss_with` naming a user is armed only by that user's reply.
#[test]
fn discuss_with_a_user_is_armed_only_by_that_user() {
    let t = team();
    let groomed = t.fake.issue("Triage", "Should we cap the backoff?", "");
    t.fake
        .comment(&groomed, Author::User(t.fake.key_user()), "Which cap?", 60);
    t.fake
        .comment(&groomed, Author::User(t.phil.clone()), "30s.", 50);
    let mut plugin = Plugin::armed_as(
        DISCUSS,
        discuss_settings(&t.fake, json!(["chen@acme.example"])),
    );
    assert_eq!(
        plugin.poll(),
        json!({"fire": false}),
        "Phil is not on the list"
    );
    t.fake
        .comment(&groomed, Author::User(t.chen.clone()), "30s, yes.", 40);
    assert_eq!(plugin.poll()["unit"]["thread"], groomed);
    end(plugin, &t.fake);
}

/// A rival's claim that lands a second ahead of ours wins the race: our claim is taken
/// back, the rival's stays, and the scan moves on to the next issue in the same beat.
#[test]
fn a_lost_race_takes_its_marker_back_and_moves_on() {
    let t = team();
    let next = t.fake.issue("Todo", "Cap the backoff at 30s", "");
    t.fake.race_on_next_claim(&t.issue);
    let mut plugin = Plugin::armed(settings(&t.fake));
    let unit = plugin.poll()["unit"].clone();
    assert_eq!(unit["thread"], next);
    assert_eq!(
        unit["files"][0]["text"], "Cap the backoff at 30s",
        "a bare issue briefs its title"
    );
    let left: Vec<String> = claims(&t.fake, &t.issue)
        .into_iter()
        .map(|c| c.body)
        .collect();
    assert_eq!(
        left,
        ["[afkd-claim] owner=afkd-17"],
        "only the rival's claim is left"
    );
    end(plugin, &t.fake);
}

/// An issue someone holds a live claim on is stepped over without a write.
#[test]
fn a_live_claim_is_stepped_over_in_silence() {
    let t = team();
    let next = t.fake.issue("Todo", "Cap the backoff at 30s", "");
    let rival = t.fake.user("Rival Two", "rival-two", "rival2@acme.example");
    t.fake.comment(
        &t.issue,
        Author::User(rival),
        "[afkd-claim] owner=afkd-17 renewal=3",
        5,
    );
    let mut plugin = Plugin::armed(settings(&t.fake));
    assert_eq!(plugin.poll()["unit"]["thread"], next);
    assert!(
        t.fake.seen().iter().all(|s| s.operation != "CommentCreate"
            || s.variables["input"]["issueId"] != t.issue.as_str()),
        "nothing was written to the held issue"
    );
    end(plugin, &t.fake);
}

/// A live claim Linear hands back with its punctuation escaped is still a claim.
#[test]
fn an_escaped_live_claim_is_still_stepped_over() {
    let t = team();
    let next = t.fake.issue("Todo", "Next", "");
    let rival = t.fake.user("Rival Two", "rival-two", "rival2@acme.example");
    t.fake.comment(
        &t.issue,
        Author::User(rival),
        r"\[afkd\-claim\] owner=afkd\_17 renewal=3",
        5,
    );
    let mut plugin = Plugin::armed(settings(&t.fake));
    assert_eq!(plugin.poll()["unit"]["thread"], next);
    assert!(claims(&t.fake, &t.issue).is_empty(), "ours never went up");
    end(plugin, &t.fake);
}

// --- blocked-by ---

/// An issue blocked by one still started is passed over in silence, and the next claimed;
/// once its blocker moves to a completed state it is claimed.
#[test]
fn a_blocked_issue_waits_until_its_blocker_completes() {
    let t = team();
    let blocker = t.fake.issue("In Progress", "Upgrade the client first", "");
    t.fake.relate(&blocker, &t.issue, "blocks");
    let next = t.fake.issue("Todo", "Next", "");
    let mut plugin = Plugin::armed(settings(&t.fake));
    let unit = plugin.poll()["unit"].clone();
    assert_eq!(unit["thread"], next);
    assert!(claims(&t.fake, &t.issue).is_empty());
    assert_eq!(
        finish(&mut plugin, &unit, "clean", facts("proceed", None)),
        json!({"ok": true})
    );
    t.fake.move_to(&next, "Done");
    assert_eq!(plugin.poll(), json!({"fire": false}), "still blocked");
    t.fake.move_to(&blocker, "Done");
    let unit = plugin.poll()["unit"].clone();
    assert_eq!(unit["thread"], t.issue);
    assert_eq!(
        unit["fields"]["blocked_by"],
        json!([t.fake.identifier(&blocker)])
    );
    end(plugin, &t.fake);
}

/// A canceled blocker never completes, so it holds nothing back.
#[test]
fn a_canceled_blocker_frees_the_issue() {
    let t = team();
    let blocker = t.fake.issue("Canceled", "Abandoned prerequisite", "");
    t.fake.relate(&blocker, &t.issue, "blocks");
    let mut plugin = Plugin::armed(settings(&t.fake));
    assert_eq!(plugin.poll()["unit"]["thread"], t.issue);
    end(plugin, &t.fake);
}

/// A related, similar or duplicate relation holds nothing back, and is no blocker.
#[test]
fn a_related_issue_is_not_blocked() {
    let t = team();
    for kind in ["related", "similar", "duplicate"] {
        let other = t.fake.issue("In Progress", &format!("{kind} work"), "");
        t.fake.relate(&other, &t.issue, kind);
    }
    let mut plugin = Plugin::armed(settings(&t.fake));
    let unit = plugin.poll()["unit"].clone();
    assert_eq!(unit["thread"], t.issue);
    assert_eq!(unit["fields"]["blocked_by"], json!([]));
    end(plugin, &t.fake);
}

/// A blocker behind twenty other relations still holds the issue back, in the gate Linear
/// applies to the state's read. Once the blocker is done the issue is claimed, and its handle
/// names it: the full read takes an issue's inverse relations twenty to the page, and reads
/// the rest too.
#[test]
fn a_blocker_behind_twenty_other_relations_still_holds_the_issue_back() {
    let t = team();
    for n in 0..20 {
        let other = t.fake.issue("In Progress", &format!("Related {n} 🔗"), "");
        t.fake.relate(
            &other,
            &t.issue,
            if n % 2 == 0 { "related" } else { "similar" },
        );
    }
    let blocker = t.fake.issue("In Progress", "Upgrade the client first", "");
    t.fake.relate(&blocker, &t.issue, "blocks");
    let next = t.fake.issue("Todo", "Next", "");
    let mut plugin = Plugin::armed(settings(&t.fake));
    let unit = plugin.poll()["unit"].clone();
    assert_eq!(unit["thread"], next, "the blocked issue is passed over");
    assert!(claims(&t.fake, &t.issue).is_empty());
    assert_eq!(
        finish(&mut plugin, &unit, "clean", facts("proceed", None)),
        json!({"ok": true})
    );
    t.fake.move_to(&next, "Done");
    assert_eq!(plugin.poll(), json!({"fire": false}), "still blocked");
    t.fake.move_to(&blocker, "Done");
    let unit = plugin.poll()["unit"].clone();
    assert_eq!(unit["thread"], t.issue);
    assert_eq!(
        unit["fields"]["blocked_by"],
        json!([t.fake.identifier(&blocker)])
    );
    assert!(operations(&t.fake).contains(&"IssueRelations".to_string()));
    end(plugin, &t.fake);
}

// --- the handle and the brief ---

/// A rich issue's family and detail, as the claimed issue's handle carries it: a parent, two
/// sub-issues, two blockers (done and canceled), an issue it blocks, relations of every type
/// both ways, a priority, an estimate, a due date, an assignee, a delegate, a creator, a
/// project and its milestone, a cycle, a link and two subscribers.
struct Rich {
    t: Team,
    epic: String,
    one: String,
    two: String,
    done: String,
    canceled: String,
    blocked: String,
    related: String,
    similar: String,
    original: String,
    copy: String,
}

fn rich() -> Rich {
    let t = team();
    let f = &t.fake;
    let epic = f.issue("Backlog", "Epic: 重试 that works", "");
    f.set_parent(&t.issue, &epic);
    let one = f.issue("Todo", "Cap the backoff", "");
    let two = f.issue("Done", "Read Retry-After 🚦", "");
    for child in [&one, &two] {
        f.set_parent(child, &t.issue);
    }
    let done = f.issue("Done", "Upgrade ureq", "");
    let canceled = f.issue("Canceled", "Rewrite in Go", "");
    let blocked = f.issue("Backlog", "Ship 1.0", "");
    let related = f.issue("In Progress", "Related work", "");
    let similar = f.issue("Backlog", "Retry storm in the worker", "");
    let original = f.issue("Done", "Retry storm, first report", "");
    let copy = f.issue("Canceled", "Retry storm, again", "");
    f.relate(&done, &t.issue, "blocks");
    f.relate(&canceled, &t.issue, "blocks");
    f.relate(&t.issue, &blocked, "blocks");
    f.relate(&related, &t.issue, "related");
    f.relate(&t.issue, &similar, "similar");
    f.relate(&t.issue, &original, "duplicate");
    f.relate(&copy, &t.issue, "duplicate");
    f.label(&one, "afkd/hold");
    f.label(&t.issue, "afkd/ready");
    f.set_priority(&t.issue, 2);
    f.set_estimate(&t.issue, 5.0);
    f.set_due(&t.issue, "2026-12-24");
    f.assign(&t.issue, &t.phil);
    f.delegate(&t.issue, &f.app_user());
    f.set_creator(&t.issue, &t.chen);
    let project = f.project("Trackers 🧭", &["ENG"], &[("B — reads", 1.0)]);
    f.set_project(&t.issue, &project, Some("B — reads"));
    let cycle = f.cycle(
        "ENG",
        12,
        Some("Sprint 12"),
        1_793_000_000_000,
        1_794_000_000_000,
        true,
    );
    f.set_cycle(&t.issue, &cycle);
    f.link(&t.issue, "https://github.com/acme/widgets/pull/42");
    f.subscribe(&t.issue, &t.phil);
    f.subscribe(&t.issue, &t.chen);
    f.set_times(&t.issue, 1_793_000_000_000, 1_793_000_500_000);
    Rich {
        t,
        epic,
        one,
        two,
        done,
        canceled,
        blocked,
        related,
        similar,
        original,
        copy,
    }
}

/// The claimed issue's handle carries every field the design's `linear.Issue` has, read after
/// the won claim: its state and category, its family, its blockers done or not, every
/// relation both ways, its detail and the workspace it is in.
#[test]
fn the_claimed_issue_carries_every_field_of_the_handle() {
    let r = rich();
    let (t, f) = (&r.t, &r.t.fake);
    let id = |issue: &str| f.identifier(issue);
    let mut plugin = Plugin::armed(with(settings(f), json!({"skip_labels": ["afkd/hold"]})));
    let unit = plugin.poll()["unit"].clone();
    assert_eq!(unit["thread"], t.issue);
    assert_eq!(
        unit["fields"],
        json!({
            "uuid": t.issue, "workspace": "acme", "team": "ENG", "title": TITLE,
            "url": f.url(&t.issue), "description": BODY, "branch": f.branch(&t.issue),
            "state": "Todo", "state_type": "unstarted", "priority": "high", "estimate": 5,
            "due": "2026-12-24", "assignee": "phil", "delegate": "afkd-app", "creator": "chen",
            "project": "Trackers 🧭", "milestone": "B — reads", "cycle": 12,
            "labels": ["afkd/ready"], "parent": id(&r.epic),
            "sub_issues": [id(&r.one), id(&r.two)],
            "blocks": [id(&r.blocked)],
            "blocked_by": [id(&r.done), id(&r.canceled)],
            "related": [id(&r.related)], "similar": [id(&r.similar)],
            "duplicates": [id(&r.copy)], "duplicate_of": id(&r.original),
            "links": ["https://github.com/acme/widgets/pull/42"],
            "subscribers": ["phil", "chen"],
            "created_at": 1_793_000_000_000_u64, "updated_at": 1_793_000_500_000_u64,
            "completed_at": null, "canceled_at": null, "archived": false, "trashed": false,
        })
    );
    let ops = operations(f);
    let won = ops
        .iter()
        .rposition(|o| o == "IssueComments")
        .expect("the claim's re-read");
    assert_eq!(ops[won + 1], "IssueFull", "the won issue is read whole");
    end(plugin, f);
}

/// A claimed issue and the same issue a read hands back carry the same fields, the one
/// builder making both: a slot's handle and `linear.issues`'s agree on every field of the
/// richest issue.
#[test]
fn a_claimed_issue_and_the_same_issue_read_carry_the_same_fields() {
    let r = rich();
    let (t, f) = (&r.t, &r.t.fake);
    let mut plugin = Plugin::armed(with(settings(f), json!({"skip_labels": ["afkd/hold"]})));
    let unit = plugin.poll()["unit"].clone();
    let read = plugin.read_call("issues", json!({"ws": ws(f), "id": [t.issue]}));
    assert_eq!(read["handles"][0]["fields"], unit["fields"]);
    assert_eq!(read["handles"][0]["id"], unit["id"]);
    assert_ne!(
        read["handles"][0]["key"], unit["key"],
        "a read's key is the workspace's, the claim's afkd's"
    );
    end(plugin, f);
}

/// A `service(linear)`'s slot reads the issue it claimed through a `linear.Workspace`: its
/// comments and its files, the claim's handle resolved to the issue it holds, live and once
/// finished; a workspace of another organization is refused naming both url keys.
#[test]
fn a_slot_reads_its_claimed_issue_through_a_workspace() {
    let t = team();
    let shot = t
        .fake
        .seed_file("screen shot — 修复.png", "image/png", &png());
    t.fake.comment(
        &t.issue,
        Author::User(t.chen.clone()),
        &format!("Here: ![shot]({shot})"),
        60,
    );
    let state = TempDir::new("wire-slot-reads");
    let mut plugin = Plugin::armed_in(settings(&t.fake), state.path());
    let unit = plugin.poll()["unit"].clone();
    let issue = json!({"id": unit["id"], "key": unit["key"]});
    let comments = plugin.read_call("comments", json!({"ws": ws(&t.fake), "issue": issue}));
    let bodies: Vec<&str> = comments["handles"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["fields"]["body"].as_str().unwrap())
        .collect();
    assert_eq!(
        bodies[..3],
        [PHIL, CHEN, format!("Here: ![shot]({shot})").as_str()]
    );
    assert!(
        bodies[3].starts_with("[afkd-claim]"),
        "the thread is Linear's, afkd's own marker among it: {bodies:?}"
    );
    assert_eq!(
        plugin.read_call("files", json!({"ws": ws(&t.fake), "issue": issue})),
        json!({"ok": true, "value": [shot]})
    );
    assert!(state
        .path()
        .join(format!("organization-{}.txt", t.fake.org_id()))
        .is_file());
    finish(&mut plugin, &unit, "clean", facts("proceed", None));
    assert_eq!(
        plugin.read_call("files", json!({"ws": ws(&t.fake), "issue": issue}))["ok"],
        true,
        "the finished issue is reached too"
    );
    let globex = FakeLinear::start_globex();
    assert_eq!(
        plugin.read_call("comments", json!({"ws": ws(&globex), "issue": issue})),
        json!({"ok": false, "error":
            "comments: issue is ENG-101 of workspace acme, and the linear.Workspace reaches \
             globex"})
    );
    end(plugin, &t.fake);
    assert_eq!(globex.invalid_requests(), Vec::<String>::new());
}

/// The brief names the parent, and lists each sub-issue with its state, between the
/// description and the thread.
#[test]
fn the_brief_lists_the_parent_and_the_sub_issues() {
    let t = team();
    let epic = t.fake.issue("Backlog", "Epic: 重试 that works", "");
    t.fake.set_parent(&t.issue, &epic);
    let one = t.fake.issue("In Progress", "Cap the backoff", "");
    let two = t.fake.issue("Done", "Read Retry-After 🚦", "");
    for child in [&one, &two] {
        t.fake.set_parent(child, &t.issue);
    }
    let mut plugin = Plugin::armed(settings(&t.fake));
    let unit = plugin.poll()["unit"].clone();
    assert_eq!(
        unit["files"][0]["text"],
        format!(
            "{TITLE}\n\n{BODY}\n\nSub-issue of {}: Epic: 重试 that works\n\n## Sub-issues\n\n\
             - {} (In Progress) Cap the backoff\n- {} (Done) Read Retry-After 🚦\n\n\
             ## New comments\n\n**Phil Ek:** {PHIL}\n\n**陳大文:** {CHEN}\n",
            t.fake.identifier(&epic),
            t.fake.identifier(&one),
            t.fake.identifier(&two),
        )
    );
    end(plugin, &t.fake);
}

// --- auth ---

/// An API key goes out as it is, with no `Bearer`, on every request, and no token is ever
/// fetched.
#[test]
fn an_api_key_is_sent_bare_and_no_token_is_fetched() {
    let t = team();
    let mut plugin = Plugin::armed(settings(&t.fake));
    let unit = plugin.poll()["unit"].clone();
    finish(&mut plugin, &unit, "clean", facts("proceed", None));
    let seen = t.fake.seen();
    assert!(seen.len() > 5);
    for s in &seen {
        assert_eq!(s.path, "/graphql");
        assert_eq!(s.authorization.as_deref(), Some(API_KEY));
    }
    assert_eq!(t.fake.token_fetches(), 0);
    end(plugin, &t.fake);
}

/// An app's credentials are traded for a token with the `client_credentials` grant and the
/// fixed scope, and every GraphQL request carries it as `Bearer`; the run's env carries the
/// same header.
#[test]
fn app_credentials_fetch_a_token_with_the_grant_and_scope_and_send_it_as_bearer() {
    let t = team();
    let mut plugin = Plugin::armed(app_settings(&t.fake));
    assert!(t.fake.seen().is_empty(), "no token at hello");
    let unit = plugin.poll()["unit"].clone();
    let seen = t.fake.seen();
    assert_eq!(seen[0].path, "/oauth/token");
    let form: Vec<&str> = seen[0].form.split('&').collect();
    assert_eq!(
        form,
        [
            "grant_type=client_credentials",
            "scope=read%2Cwrite%2Capp%3Aassignable%2Capp%3Amentionable",
            "client_id=app-id-7f3a9c1e",
            "client_secret=app-secret-4b8d%26%3D%2F2e",
        ]
    );
    assert_eq!(SCOPE, "read,write,app:assignable,app:mentionable");
    for s in &seen[1..] {
        assert_eq!(
            s.authorization.as_deref(),
            Some("Bearer lin_oauth_tok-1"),
            "{}",
            s.operation
        );
    }
    assert_eq!(t.fake.token_fetches(), 1);
    assert_eq!(
        unit["env"]["LINEAR_AUTHORIZATION"],
        "Bearer lin_oauth_tok-1"
    );
    assert_eq!(unit["self"], t.fake.app_user(), "the app is the actor");
    assert_eq!(
        claims(&t.fake, &t.issue)[0].author,
        Author::User(t.fake.app_user())
    );
    end(plugin, &t.fake);
}

/// A token is fetched again before it expires: one that lasts 2 s is renewed after 1 s, so
/// the second poll, 1.2 s later, goes out on a fresh token and Linear refuses nothing.
#[test]
fn an_app_token_is_fetched_again_before_it_expires() {
    let t = team();
    t.fake.token_lifetime(2);
    let mut plugin = Plugin::armed(with(
        app_settings(&t.fake),
        json!({"only_labels": ["none-yet"]}),
    ));
    assert_eq!(plugin.poll(), json!({"fire": false}));
    assert_eq!(t.fake.token_fetches(), 1);
    std::thread::sleep(Duration::from_millis(1200));
    assert_eq!(plugin.poll(), json!({"fire": false}));
    assert_eq!(t.fake.token_fetches(), 2, "renewed before it expired");
    let tokens: Vec<String> = t
        .fake
        .seen()
        .into_iter()
        .filter(|s| s.path == "/graphql")
        .filter_map(|s| s.authorization)
        .collect();
    assert_eq!(
        tokens.first().map(String::as_str),
        Some("Bearer lin_oauth_tok-1")
    );
    assert_eq!(
        tokens.last().map(String::as_str),
        Some("Bearer lin_oauth_tok-2")
    );
    let stderr = end(plugin, &t.fake);
    assert!(
        !stderr.contains("authentication"),
        "no request carried an expired token: {stderr}"
    );
}

/// A token Linear revokes is dropped, a new one fetched, and the refused request sent once
/// more, so the poll still claims.
#[test]
fn a_revoked_token_is_fetched_again_and_the_request_retried_once() {
    let t = team();
    let mut plugin = Plugin::armed(with(
        app_settings(&t.fake),
        json!({"only_labels": ["later"]}),
    ));
    assert_eq!(plugin.poll(), json!({"fire": false}));
    t.fake.revoke_tokens();
    t.fake.label(&t.issue, "later");
    assert_eq!(plugin.poll()["unit"]["thread"], t.issue);
    assert_eq!(t.fake.token_fetches(), 2);
    let auth: Vec<String> = t
        .fake
        .seen()
        .into_iter()
        .map(|s| format!("{} {}", s.path, s.authorization.unwrap_or_default()))
        .collect();
    let refused = auth
        .iter()
        .position(|a| a == "/oauth/token ")
        .and_then(|first| {
            auth[first + 1..]
                .iter()
                .position(|a| a == "/oauth/token ")
                .map(|p| p + first + 1)
        })
        .expect("a second fetch");
    assert_eq!(
        auth[refused - 1],
        "/graphql Bearer lin_oauth_tok-1",
        "the refused request"
    );
    assert_eq!(
        auth[refused + 1],
        "/graphql Bearer lin_oauth_tok-2",
        "sent once more"
    );
    end(plugin, &t.fake);
}

// --- the kind's own notes ---

/// An attempt note Linear rate limits is said and lost: `attempt_failed` answers `ok` at
/// once, and the note is tried once.
#[test]
fn a_rate_limited_attempt_note_is_said_and_lost() {
    let t = team();
    let mut plugin = Plugin::armed(settings(&t.fake));
    let unit = plugin.poll()["unit"].clone();
    t.fake.rate_limit("CommentCreate", 1, 7);
    let reason =
        "run_cmd `cargo test` failed: 101\n\n  thread 'retry' panicked at 看 src/retry.rs:42";
    let asked = Instant::now();
    assert_eq!(
        plugin.call(
            json!({"call": "attempt_failed", "key": unit["key"], "n": 1, "max": 2,
                           "reason": reason})
        ),
        json!({"ok": true})
    );
    assert!(asked.elapsed() < Duration::from_secs(5), "answered at once");
    let stderr = plugin.stderr_soon("is lost");
    assert!(
        stderr.contains(&format!(
            "afkd-linear: issue ENG-101 \"{TITLE}\": the comment \"[afkd-attempt] 1/2: run_cmd \
             `cargo test` failed: 101…\" is lost — linear post comment: Linear refused it (429): \
             Rate limit exceeded"
        )),
        "{stderr}"
    );
    assert!(said(&t.fake, &t.issue, "[afkd-attempt]").is_empty());
    let tries = t
        .fake
        .seen()
        .into_iter()
        .filter(|s| {
            s.operation == "CommentCreate"
                && s.variables["input"]["body"]
                    .as_str()
                    .is_some_and(|b| b.starts_with("[afkd-attempt]"))
        })
        .count();
    assert_eq!(tries, 1, "tried once, never again");
    end(plugin, &t.fake);
}

/// A rate-limited poll is an idle beat that says so; the next beat claims.
#[test]
fn a_rate_limited_poll_is_an_idle_beat_and_the_next_poll_claims() {
    let t = team();
    t.fake.rate_limit("Issues", 1, 30);
    let mut plugin = Plugin::armed(settings(&t.fake));
    assert_eq!(plugin.poll(), json!({"fire": false}));
    assert_eq!(
        plugin.stderr_soon("Rate limit"),
        "afkd-linear: linear read parked issues: Linear refused it (429): Rate limit exceeded\n"
    );
    assert_eq!(plugin.poll()["unit"]["thread"], t.issue);
    end(plugin, &t.fake);
}

// --- renew, comments, attempt_failed, classify ---

#[test]
fn renew_edits_the_marker_in_place() {
    let t = team();
    let mut plugin = Plugin::armed(settings(&t.fake));
    let unit = plugin.poll()["unit"].clone();
    let before = claims(&t.fake, &t.issue)[0].clone();
    assert_eq!(
        plugin.call(json!({"call": "renew", "key": unit["key"], "renewal": 1})),
        json!({"ok": true})
    );
    let after = claims(&t.fake, &t.issue)[0].clone();
    assert_eq!(after.id, before.id, "edited, not re-posted");
    assert_eq!(after.body, "[afkd-claim] owner=afkd-4242 renewal=1");
    assert!(after.edited.is_some(), "Linear stamped the edit");
    assert_eq!(
        plugin.call(json!({"call": "renew", "key": format!("{}#nope", t.issue), "renewal": 1})),
        json!({"ok": false}),
        "a key it does not hold is declined"
    );
    end(plugin, &t.fake);
}

/// `comments` reports what afkd has not been told about: what the brief carried, afkd's own
/// words and every control marker are left out; a second read carries only what is new;
/// and a Linear that cannot be read is `null`. A bot's and an external user's comments are
/// someone else's.
#[test]
fn comments_report_only_what_afkd_has_not_seen() {
    let t = team();
    let mut plugin = Plugin::armed(settings(&t.fake));
    let unit = plugin.poll()["unit"].clone();
    let read = |plugin: &mut Plugin| plugin.call(json!({"call": "comments", "key": unit["key"]}));
    // Each a millisecond after the one before, so the order they were posted in is the
    // order of their times and never of their ids.
    let now = now_ms();
    let late = t.fake.comment_at(
        &t.issue,
        Author::User(t.chen.clone()),
        "别急着移走 🚨\n\n    max_backoff = 30\n",
        now,
        None,
    );
    t.fake.comment_at(
        &t.issue,
        Author::User(t.fake.key_user()),
        "Looking into it.",
        now + 1,
        None,
    );
    t.fake.comment_at(
        &t.issue,
        Author::User(t.phil.clone()),
        "[afkd-attempt] 1/2: pasted from another issue",
        now + 2,
        None,
    );
    let external = t.fake.comment_at(
        &t.issue,
        Author::External("x-alvaro".into()),
        "Exponential, please — see §4 🙏",
        now + 3,
        None,
    );
    let at = |id: &str| {
        let c = t
            .fake
            .comments(&t.issue)
            .into_iter()
            .find(|c| c.id == id)
            .unwrap();
        utc(c.created)
    };
    assert_eq!(
        read(&mut plugin),
        json!({"comments": [
            {"id": late, "author": t.chen, "author_name": "陳大文",
             "body": "别急着移走 🚨\n\n    max_backoff = 30\n", "at": at(&late)},
            {"id": external, "author": "x-alvaro", "author_name": "Álvaro (Slack)",
             "body": "Exponential, please — see §4 🙏", "at": at(&external)},
        ]})
    );
    std::thread::sleep(Duration::from_millis(5));
    let more = t.fake.comment(
        &t.issue,
        Author::User(t.phil.clone()),
        "…and cap it at 30s.",
        0,
    );
    let ids: Vec<String> = read(&mut plugin)["comments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids, [more]);
    t.fake.fail("IssueComments", 500);
    assert_eq!(read(&mut plugin), json!({"comments": null}));
    t.fake.heal("IssueComments");
    assert_eq!(read(&mut plugin), json!({"comments": []}));
    end(plugin, &t.fake);
}

#[test]
fn attempt_failed_marks_the_issue() {
    let t = team();
    let mut plugin = Plugin::armed(settings(&t.fake));
    let unit = plugin.poll()["unit"].clone();
    let reason =
        "run_cmd `cargo test` failed: 101\n\n  thread 'retry' panicked at 看 src/retry.rs:42";
    for (n, reason) in [(1, json!(reason)), (2, Value::Null)] {
        assert_eq!(
            plugin.call(
                json!({"call": "attempt_failed", "key": unit["key"], "n": n, "max": 2,
                               "reason": reason})
            ),
            json!({"ok": true})
        );
    }
    assert_eq!(
        said(&t.fake, &t.issue, "[afkd-attempt]"),
        [format!("[afkd-attempt] 1/2: {reason}")],
        "a fault with no sentence marks nothing"
    );
    end(plugin, &t.fake);
}

#[test]
fn classify_parks_on_the_marker_and_echoes_otherwise() {
    let t = team();
    let mut plugin = Plugin::armed(settings(&t.fake));
    let scratch = TempDir::new("classify");
    let classify = |plugin: &mut Plugin, outcome: &str| {
        plugin.call(json!({"call": "classify", "key": format!("{}#1", t.issue),
                           "scratch": scratch.path(), "outcome": outcome}))
    };
    assert_eq!(classify(&mut plugin, "clean"), json!({"outcome": "clean"}));
    std::fs::write(scratch.path().join("park"), b"").unwrap();
    assert_eq!(classify(&mut plugin, "failed"), json!({"outcome": "park"}));
    assert!(
        t.fake.seen().is_empty(),
        "classify reads the scratch dir, not Linear"
    );
    end(plugin, &t.fake);
}

// --- release, finish ---

/// A crashed run's claim is reversed — the claim deleted, and the issue moved to the bottom
/// of the state it was claimed from — whichever process asks; a key with no `#` names
/// nothing.
#[test]
fn release_reverses_a_crashed_claim_to_the_bottom_of_its_state() {
    let t = team();
    let crashed = t.fake.issue("In Progress", "Crashed mid-run", "");
    let claim = t.fake.comment(
        &crashed,
        Author::User(t.fake.key_user()),
        "[afkd-claim] owner=afkd-17 renewal=4",
        900,
    );
    let mut plugin = Plugin::armed(settings(&t.fake));
    assert_eq!(
        plugin.call(json!({"call": "release", "key": format!("{crashed}#{claim}")})),
        json!({"released": true})
    );
    assert!(claims(&t.fake, &crashed).is_empty());
    assert_eq!(
        t.fake.in_state("Todo"),
        [t.issue.clone(), crashed.clone()],
        "at the bottom"
    );
    for key in ["garbage", ""] {
        assert_eq!(
            plugin.call(json!({"call": "release", "key": key})),
            json!({"released": null})
        );
    }
    end(plugin, &t.fake);
}

/// A unit afkd hands back before its run, the `release` marked `handed_back`, keeps its
/// place at the head of its state: the claim is deleted, no `IssueUpdate` is sent, and the
/// next poll hands the same issue over again.
#[test]
fn a_unit_handed_back_keeps_its_place_at_the_head_of_its_state() {
    let t = team();
    let second = t.fake.issue("Todo", "Next in line — 次 🧵", "");
    let mut plugin = Plugin::armed(settings(&t.fake));
    let unit = plugin.poll()["unit"].clone();
    assert_eq!(unit["thread"], t.issue);
    assert_eq!(
        plugin.call(json!({"call": "release", "key": unit["key"], "handed_back": true})),
        json!({"released": true})
    );
    assert!(claims(&t.fake, &t.issue).is_empty());
    assert_eq!(t.fake.in_state("Todo"), [t.issue.clone(), second]);
    assert!(
        !operations(&t.fake).iter().any(|o| o == "IssueUpdate"),
        "{:?}",
        operations(&t.fake)
    );
    assert_eq!(plugin.poll()["unit"]["thread"], t.issue);
    end(plugin, &t.fake);
}

/// A clean finish posts one `[afkd-ran]` watermark carrying, in milliseconds, the boundary
/// fixed at the claim read, prunes the prior one, and releases the claim last; the issue
/// stays where it is.
#[test]
fn a_clean_finish_leaves_one_watermark() {
    let t = team();
    let phil_at = t.fake.comments(&t.issue)[0].created;
    let prior = t.fake.comment(
        &t.issue,
        Author::User(t.fake.key_user()),
        &format!("[afkd-ran] owner=afkd-17 upto={phil_at}"),
        2700,
    );
    let mut plugin = Plugin::armed(settings(&t.fake));
    let unit = plugin.poll()["unit"].clone();
    let brief = unit["files"][0]["text"].as_str().unwrap();
    assert!(
        brief.ends_with(&format!(
            "## Earlier conversation\n\n**Phil Ek:** {PHIL}\n\n\n## New comments\n\n**陳大文:** {CHEN}\n"
        )),
        "the prior watermark delivered Phil's comment: {brief}"
    );
    assert_eq!(
        finish(&mut plugin, &unit, "clean", facts("proceed", None)),
        json!({"ok": true})
    );
    let chen_at = t
        .fake
        .comments(&t.issue)
        .iter()
        .find(|c| c.id == t.asked[1])
        .unwrap()
        .created;
    assert_eq!(
        said(&t.fake, &t.issue, "[afkd-ran]"),
        [format!("[afkd-ran] owner=afkd-4242 upto={chen_at}")]
    );
    assert!(t.fake.comments(&t.issue).iter().all(|c| c.id != prior));
    assert!(claims(&t.fake, &t.issue).is_empty());
    assert_eq!(t.fake.state_of(&t.issue), "Todo", "finish moves nothing");
    assert!(plugin.stderr_soon("released claim").ends_with(&format!(
        "[linear] released claim on issue ENG-101 \"{TITLE}\"\n"
    )));
    end(plugin, &t.fake);
}

/// A park badges the issue — creating the team's label when there is none — posts the owner
/// marker naming the configured service, and only then releases the claim.
#[test]
fn a_park_finish_labels_marks_and_releases() {
    let t = team();
    let mut plugin = Plugin::armed(settings(&t.fake));
    let unit = plugin.poll()["unit"].clone();
    park(
        &mut plugin,
        &t.fake,
        &t.issue,
        &unit,
        Author::User(t.fake.key_user()),
    );
    assert_eq!(t.fake.labels(&t.issue), ["Needs Info"]);
    assert!(
        operations(&t.fake).contains(&"IssueLabelCreate".to_string()),
        "created"
    );
    assert_eq!(
        said(&t.fake, &t.issue, "[afkd-park]"),
        ["[afkd-park] service=afkd::develop"]
    );
    assert!(claims(&t.fake, &t.issue).is_empty());
    let writes: Vec<String> = operations(&t.fake)
        .into_iter()
        .filter(|o| ["CommentCreate", "CommentDelete", "IssueAddLabel"].contains(&o.as_str()))
        .collect();
    assert_eq!(
        writes.last().map(String::as_str),
        Some("CommentDelete"),
        "the release is last"
    );
    assert!(plugin.stderr_soon("parked issue").contains(&format!(
        "[linear] parked issue ENG-101 \"{TITLE}\" awaiting a reply (label \"Needs Info\")\n"
    )));
    end(plugin, &t.fake);
}

/// A park on a team that already has its `Needs Info` label badges the issue with that label,
/// creating none beside it. The parked issue waits, its question unanswered, in the state it
/// sat in; a human's reply re-arms it ahead of `claim_from`, the badge and owner marker coming
/// off.
#[test]
fn a_parked_issue_answered_is_reclaimed_and_unbadged() {
    let t = team();
    let needs_info = t.fake.team_label("Needs Info", "#f2c94c");
    let mut plugin = Plugin::armed(settings(&t.fake));
    let unit = plugin.poll()["unit"].clone();
    park(
        &mut plugin,
        &t.fake,
        &t.issue,
        &unit,
        Author::User(t.fake.key_user()),
    );
    assert_eq!(
        t.fake.label_ids(&t.issue),
        [needs_info],
        "the team's own label"
    );
    assert!(
        !operations(&t.fake).contains(&"IssueLabelCreate".to_string()),
        "no second label is created"
    );
    let next = t.fake.issue("Todo", "Queued behind it", "");
    let unit = plugin.poll()["unit"].clone();
    assert_eq!(unit["thread"], next, "unanswered, so the scan goes on");
    assert_eq!(
        finish(&mut plugin, &unit, "clean", facts("proceed", None)),
        json!({"ok": true})
    );
    t.fake.move_to(&next, "Done");
    assert_eq!(plugin.poll(), json!({"fire": false}), "still unanswered");

    t.fake.comment(
        &t.issue,
        Author::User(t.phil.clone()),
        "Appen — webben får en egen video.",
        0,
    );
    let unit = plugin.poll()["unit"].clone();
    assert_eq!(unit["thread"], t.issue, "the answer re-arms it");
    assert!(t.fake.labels(&t.issue).is_empty(), "unbadged");
    assert!(
        said(&t.fake, &t.issue, "[afkd-park]").is_empty(),
        "the marker is gone"
    );
    assert!(unit["files"][0]["text"]
        .as_str()
        .unwrap()
        .ends_with("## New comments\n\n**Phil Ek:** Appen — webben får en egen video.\n"));
    end(plugin, &t.fake);
}

/// An app whose comments come back authored by its bot actor reads its own question as its
/// own: the parked issue waits for a human, and the bot's words are afkd's in the brief.
#[test]
fn an_app_whose_comments_carry_its_bot_actor_reads_its_own_words_as_its_own() {
    let t = team();
    t.fake.comments_as_bot();
    let mut plugin = Plugin::armed(app_settings(&t.fake));
    let unit = plugin.poll()["unit"].clone();
    assert_eq!(
        claims(&t.fake, &t.issue)[0].author,
        Author::Bot(t.fake.bot_id())
    );
    park(
        &mut plugin,
        &t.fake,
        &t.issue,
        &unit,
        Author::Bot(t.fake.bot_id()),
    );
    assert_eq!(
        plugin.poll(),
        json!({"fire": false}),
        "its own question is no answer"
    );
    t.fake.comment(
        &t.issue,
        Author::User(t.chen.clone()),
        "The Retry-After one.",
        0,
    );
    let unit = plugin.poll()["unit"].clone();
    let brief = unit["files"][0]["text"].as_str().unwrap();
    assert!(
        brief.contains("**afkd:** Ska videon visa appen eller webben? 📱")
            && brief.ends_with("## New comments\n\n**陳大文:** The Retry-After one.\n"),
        "{brief}"
    );
    end(plugin, &t.fake);
}

/// On the `discuss_with` path a turn that said nothing is backstopped, so afkd is the last
/// speaker and the issue does not re-fire; no watermark is posted.
#[test]
fn a_discuss_turn_that_said_nothing_is_backstopped() {
    let t = team();
    let groomed = t.fake.issue("Triage", "Should we cap the backoff?", "");
    t.fake.comment(
        &groomed,
        Author::User(t.phil.clone()),
        "What do you think?",
        60,
    );
    let mut plugin = Plugin::armed_as(DISCUSS, discuss_settings(&t.fake, json!(["anyone"])));
    let unit = plugin.poll()["unit"].clone();
    assert_eq!(
        finish(&mut plugin, &unit, "clean", facts("proceed", None)),
        json!({"ok": true})
    );
    let mine: Vec<String> = t
        .fake
        .comments(&groomed)
        .into_iter()
        .filter(|c| c.author == Author::User(t.fake.key_user()))
        .map(|c| c.body)
        .collect();
    assert_eq!(mine, ["reviewed, nothing to add"]);
    assert!(said(&t.fake, &groomed, "[afkd-ran]").is_empty());
    assert_eq!(plugin.poll(), json!({"fire": false}), "afkd spoke last");
    end(plugin, &t.fake);
}

/// A finish that does not land is `held`: the claim stays, and afkd's later `release` of the
/// key replays what is owed — `false` while Linear still fails, `true` once it lands.
#[test]
fn an_undelivered_finish_is_held_and_release_delivers_it() {
    let t = team();
    let mut plugin = Plugin::armed(settings(&t.fake));
    let unit = plugin.poll()["unit"].clone();
    let key = unit["key"].as_str().unwrap().to_string();
    t.fake.fail("CommentDelete", 500);
    assert_eq!(
        finish(&mut plugin, &unit, "clean", facts("proceed", None)),
        json!({"ok": true, "held": true})
    );
    assert!(
        plugin
            .stderr_soon("afkd holds the claim")
            .ends_with(&format!(
                "afkd-linear: linear delete comment: Linear refused it (500): injected\n\
             afkd-linear: could not deliver the finish for {key}; afkd holds the claim and \
             releases it on a later beat\n"
            )),
        "{}",
        plugin.stderr()
    );
    assert_eq!(claims(&t.fake, &t.issue).len(), 1, "the lease is held");
    assert_eq!(
        plugin.call(json!({"call": "release", "key": key})),
        json!({"released": false})
    );
    t.fake.heal("CommentDelete");
    assert_eq!(
        plugin.call(json!({"call": "release", "key": key})),
        json!({"released": true})
    );
    assert!(claims(&t.fake, &t.issue).is_empty());
    assert_eq!(
        t.fake.state_of(&t.issue),
        "Todo",
        "a delivered finish moves nothing"
    );
    end(plugin, &t.fake);
}

/// A brief over afkd's line is cut to fit, and the unit still decodes, its key whole.
#[test]
fn an_oversized_brief_is_cut_to_fit_one_line() {
    let t = team();
    let big = t.fake.issue(
        "Backlog",
        TITLE,
        &"看起来不对 🚨 — the retry path.\n".repeat(2_600),
    );
    let mut plugin = Plugin::armed(with(settings(&t.fake), json!({"claim_from": ["Backlog"]})));
    let line = plugin.call_raw(json!({"call": "poll"}));
    assert!(line.len() <= MAX_LINE, "{}", line.len());
    let reply: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(reply["unit"]["thread"], big);
    let brief = reply["unit"]["files"][0]["text"].as_str().unwrap();
    assert!(brief.ends_with("read the whole issue in Linear]"));
    let stderr = plugin.stderr_soon("the brief for");
    assert!(stderr.contains(
        "afkd-linear: the description field of ENG-102 was cut to fit afkd's 64 KiB plugin \
         line\n"
    ));
    assert!(stderr
        .contains("afkd-linear: the brief for ENG-102 was cut to fit afkd's 64 KiB plugin line\n"));
    end(plugin, &t.fake);
}

/// A description that only overflows the line carried twice, as the handle's field and in
/// the brief, is cut in the field alone: the brief is handed over whole, and the field ends
/// in the trailer naming the whole size.
#[test]
fn an_oversized_description_is_cut_in_the_handle_before_the_brief() {
    let t = team();
    let description = "看起来不对 🚨 — the retry path.\n".repeat(1_000);
    let big = t.fake.issue("Backlog", TITLE, &description);
    let mut plugin = Plugin::armed(with(settings(&t.fake), json!({"claim_from": ["Backlog"]})));
    let line = plugin.call_raw(json!({"call": "poll"}));
    assert!(line.len() <= MAX_LINE, "{}", line.len());
    let reply: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(reply["unit"]["thread"], big);
    let brief = reply["unit"]["files"][0]["text"].as_str().unwrap();
    assert!(
        brief.starts_with(&format!("{TITLE}\n\n{description}")),
        "the brief is whole"
    );
    let field = reply["unit"]["fields"]["description"].as_str().unwrap();
    assert!(
        field.ends_with(&format!(
            "of {} bytes to fit afkd's 64 KiB plugin line — read the whole issue in Linear]",
            description.len()
        )),
        "{}",
        &field[field.len() - 200..]
    );
    assert!(description.starts_with(field.split("\n\n[afkd-linear:").next().unwrap()));
    let stderr = plugin.stderr_soon("was cut");
    assert!(
        stderr.contains("the description field of ENG-102 was cut"),
        "{stderr}"
    );
    assert!(!stderr.contains("the brief for"), "{stderr}");
    end(plugin, &t.fake);
}

// --- the fake ---

/// The fake refuses what Linear's schema refuses, so every other leg's empty
/// `invalid_requests` means something: the real `Issues` document, sent with a label filter
/// on a field `IssueLabelFilter` does not declare, is a 400 naming it, and recorded.
#[test]
fn the_fake_refuses_a_nonconforming_request() {
    let fake = FakeLinear::start();
    fake.state("Todo", "unstarted");
    let document = include_str!("../src/operations.graphql");
    let start = document.find("query Issues(").unwrap();
    let end_at = document[start..].find("\nquery ").unwrap() + start;
    let query = document[start..end_at].trim();
    let request = |labels: Value| {
        json!({"operationName": "Issues", "query": query, "variables": {
            "filter": {"team": {"id": {"eq": fake.team_id()}}, "labels": labels},
            "sort": [{"manual": {"order": "Ascending"}}], "first": 25}})
    };
    let (status, body) = fake.post_raw(&request(json!({"some": {"name": {"eq": "Needs Info"}}})));
    assert_eq!((status, body["errors"].is_null()), (200, true), "{body}");
    let (status, body) = fake.post_raw(&request(json!({"some": {"title": {"eq": "Needs Info"}}})));
    assert_eq!(status, 400);
    assert_eq!(
        body["errors"][0]["message"],
        "$filter.labels.some.title: IssueLabelFilter declares no field title"
    );
    assert_eq!(body["errors"][0]["extensions"]["type"], "graphql error");
    // SDL-valid, but not the filter the plugin means: refused all the same.
    let (status, _) = fake.post_raw(&request(json!({"name": {"eq": "Needs Info"}})));
    assert_eq!(status, 400);
    assert_eq!(
        fake.invalid_requests(),
        [
            "$filter.labels.some.title: IssueLabelFilter declares no field title",
            "filter.labels is not some.name.eq",
        ]
    );
}

/// A poll Linear answers with a validation error is an idle beat whose line carries Linear's
/// message, and nothing is retried.
#[test]
fn a_poll_refused_as_invalid_is_an_idle_beat_naming_linears_message() {
    let t = team();
    t.fake.refuse_as_invalid("Issues");
    let mut plugin = Plugin::armed(settings(&t.fake));
    assert_eq!(plugin.poll(), json!({"fire": false}));
    assert_eq!(
        plugin.stderr_soon("invalid"),
        "afkd-linear: linear read parked issues: Linear refused it (400): Field \"Issues\" \
         argument is invalid.\n"
    );
    let issues = operations(&t.fake)
        .iter()
        .filter(|o| *o == "Issues")
        .count();
    assert_eq!(issues, 1, "not retried");
    end(plugin, &t.fake);
}
