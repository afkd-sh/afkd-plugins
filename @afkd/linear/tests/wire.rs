//! The wire contract suite: the real `afkd-linear` exec, spoken to exactly as afkd speaks to
//! it (`docs/plugins.md`, "The trigger protocol"), against a stateful fake Linear reached
//! through the kind's own `base_url`. No afkd is involved.
//!
//! Every leg ends at [`end`]: stdin closed and the child held to the wire's discipline — a
//! clean exit, one reply line per request, nothing else on stdout — and the fake's record of
//! requests that did not conform to Linear's schema empty. So a line that strayed onto
//! stdout, and a request Linear would refuse, fails whichever leg sent it.

mod common;

use std::time::{Duration, Instant};

use serde_json::{json, Value};

use common::fake::{utc, Author, FakeLinear, API_KEY, CLIENT_ID, CLIENT_SECRET, SCOPE, TEAM};
use common::{Plugin, TempDir, DEVELOP, DISCUSS, MAX_LINE, OWNER};

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

/// The service's settings, lowered to JSON as afkd lowers them: every value a string —
/// afkd's own two keys along for the ride — and no slot, since afkd runs those.
fn settings(fake: &FakeLinear) -> Value {
    json!({
        "team": TEAM,
        "base_url": fake.base_url(),
        "api_key": API_KEY,
        "pick_from": "Todo",
        "every": "30s",
        "max_attempts": "2",
    })
}

/// The same service, authenticating as the OAuth app.
fn app_settings(fake: &FakeLinear) -> Value {
    let mut s = settings(fake);
    let s_obj = s.as_object_mut().unwrap();
    s_obj.remove("api_key");
    s_obj.insert("client_id".into(), json!(CLIENT_ID));
    s_obj.insert("client_secret".into(), json!(CLIENT_SECRET));
    s
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
        json!({"pick_from": "Triage", "discuss_with": who}),
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

/// Poll `done` for up to `budget`, every 50 ms.
fn within(budget: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    done()
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
/// `pick_from` (which afkd refuses first), and another kind.
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
            "issue",
            with(
                base.clone(),
                json!({"client_id": CLIENT_ID, "client_secret": CLIENT_SECRET}),
            ),
            "trigger issue: setting `api_key`: write api_key or client_id and client_secret, not \
             both",
        ),
        (
            "issue",
            without(&["api_key"], json!({})),
            "trigger issue: setting `api_key`: needs api_key, or client_id and client_secret",
        ),
        (
            "issue",
            without(&["api_key"], json!({"client_id": CLIENT_ID})),
            "trigger issue: setting `client_id`: client_id needs client_secret",
        ),
        (
            "issue",
            without(&["api_key"], json!({"client_secret": CLIENT_SECRET})),
            "trigger issue: setting `client_secret`: client_secret needs client_id",
        ),
        (
            "issue",
            without(&["team"], json!({})),
            "trigger issue: setting `team`: `team` is required: name the team by its key",
        ),
        (
            "issue",
            without(&["pick_from"], json!({})),
            "trigger issue: setting `pick_from`: `pick_from` is required: name the workflow \
             state or states new issues are claimed from",
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
/// claim's as its key, the uuid as its thread, the claim-read thread as `seen`, the viewer
/// as `self`, every `LINEAR_*` env value, and the brief unframed — and the claim comment on
/// the issue is the viewer's.
#[test]
fn a_won_race_hands_over_the_unit() {
    let t = team();
    for label in ["afkd/ready", "Väntar på svar"] {
        t.fake.label(&t.issue, label);
    }
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
                "title": TITLE,
                "url": t.fake.url(&t.issue),
                "labels": ["afkd/ready", "Väntar på svar"],
                "state": "Todo",
                "parent": "",
                "sub_issues": [],
                "blocked_by": [],
            },
            "thread": t.issue,
            "seen": [claim.id, t.asked[1], t.asked[0]],
            "self": t.fake.key_user(),
            "env": {
                "LINEAR_API_URL": format!("{}/graphql", t.fake.base_url()),
                "LINEAR_AUTHORIZATION": API_KEY,
                "LINEAR_ISSUE": "ENG-101",
                "LINEAR_ISSUE_ID": t.issue,
                "LINEAR_TEAM_ID": t.fake.team_id(),
            },
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
fn pick_from_names_two_states_and_claims_from_the_second() {
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
        json!({"pick_from": [["Todo", "Backlog"]]}),
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

/// `require_member me` claims the issue assigned to the API key's user, past one assigned
/// to someone else.
#[test]
fn require_member_me_matches_the_assignee() {
    let t = team();
    t.fake.assign(&t.issue, &t.phil);
    let mine = t.fake.issue("Todo", "Mine", "");
    t.fake.assign(&mine, &t.fake.key_user());
    let mut plugin = Plugin::armed(with(settings(&t.fake), json!({"require_member": "me"})));
    assert_eq!(plugin.poll()["unit"]["thread"], mine);
    assert!(claims(&t.fake, &t.issue).is_empty());
    end(plugin, &t.fake);
}

/// `require_member me`, for an app, holds for an issue delegated to the app — what
/// `app:assignable` makes it — even with a human assignee.
#[test]
fn require_member_me_matches_the_delegate() {
    let t = team();
    t.fake.assign(&t.issue, &t.phil);
    let handed = t.fake.issue("Todo", "Handed to the app 🤖", "");
    t.fake.assign(&handed, &t.phil);
    t.fake.delegate(&handed, &t.fake.app_user());
    let mut plugin = Plugin::armed(with(app_settings(&t.fake), json!({"require_member": "me"})));
    let unit = plugin.poll()["unit"].clone();
    assert_eq!(unit["thread"], handed);
    assert_eq!(unit["self"], t.fake.app_user());
    end(plugin, &t.fake);
}

/// A user is named by display name or by email, either way the same user.
#[test]
fn require_member_names_a_user_by_display_name_and_by_email() {
    for name in ["chen", "chen@acme.example"] {
        let t = team();
        t.fake.assign(&t.issue, &t.phil);
        let chens = t.fake.issue("Todo", "Chen's", "");
        t.fake.assign(&chens, &t.chen);
        let mut plugin = Plugin::armed(with(settings(&t.fake), json!({"require_member": name})));
        assert_eq!(plugin.poll()["unit"]["thread"], chens, "{name}");
        end(plugin, &t.fake);
    }
}

/// A `require_member` naming no user is an idle beat that says so, and posts nothing.
#[test]
fn require_member_unknown_user_is_an_idle_beat_naming_it() {
    let t = team();
    let mut plugin = Plugin::armed(with(
        settings(&t.fake),
        json!({"require_member": "ghost@acme.example"}),
    ));
    assert_eq!(plugin.poll(), json!({"fire": false}));
    assert_eq!(
        plugin.stderr_soon("ghost"),
        "afkd-linear: linear resolve user: no user whose display name or email is \
         'ghost@acme.example'\n"
    );
    assert!(claims(&t.fake, &t.issue).is_empty());
    end(plugin, &t.fake);
}

#[test]
fn require_label_claims_only_the_labelled_issue() {
    let t = team();
    let ready = t.fake.issue("Todo", "Ready ✅", "");
    t.fake.label(&ready, "afkd/ready");
    let mut plugin = Plugin::armed(with(
        settings(&t.fake),
        json!({"require_label": "afkd/ready"}),
    ));
    assert_eq!(plugin.poll()["unit"]["thread"], ready);
    assert!(claims(&t.fake, &t.issue).is_empty());
    end(plugin, &t.fake);
}

/// An issue carrying a denied label is never claimed, whatever required label it carries
/// too.
#[test]
fn without_label_wins_over_require_label() {
    let t = team();
    t.fake.label(&t.issue, "afkd/ready");
    t.fake.label(&t.issue, "Blockerat / Väntar");
    let ready = t.fake.issue("Todo", "Ready", "");
    t.fake.label(&ready, "afkd/ready");
    let mut plugin = Plugin::armed(with(
        settings(&t.fake),
        json!({"require_label": "afkd/ready", "without_label": [["Hold", "Blockerat / Väntar"]]}),
    ));
    assert_eq!(plugin.poll()["unit"]["thread"], ready);
    assert!(claims(&t.fake, &t.issue).is_empty());
    end(plugin, &t.fake);
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

/// A blocker behind twenty other relations still holds the issue back: `Issues` reads an
/// issue's inverse relations twenty to the page, and the rest are read too. Once the blocker
/// is done the issue is claimed, naming it.
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
    assert!(operations(&t.fake).contains(&"IssueRelations".to_string()));
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

// --- the handle and the brief ---

/// The handle carries the issue's state, its parent's identifier, its sub-issues' and its
/// blockers' — done or not — exactly, and `parent` is `""` for an issue with none.
#[test]
fn the_handle_carries_state_parent_sub_issues_and_blocked_by() {
    let t = team();
    let epic = t.fake.issue("Backlog", "Epic: 重试 that works", "");
    t.fake.set_parent(&t.issue, &epic);
    let one = t.fake.issue("Todo", "Cap the backoff", "");
    let two = t.fake.issue("Done", "Read Retry-After 🚦", "");
    for child in [&one, &two] {
        t.fake.set_parent(child, &t.issue);
    }
    let done = t.fake.issue("Done", "Upgrade ureq", "");
    let canceled = t.fake.issue("Canceled", "Rewrite in Go", "");
    let related = t.fake.issue("In Progress", "Related work", "");
    t.fake.relate(&done, &t.issue, "blocks");
    t.fake.relate(&canceled, &t.issue, "blocks");
    t.fake.relate(&related, &t.issue, "related");
    t.fake.label(&one, "afkd/hold");
    let mut plugin = Plugin::armed(with(
        settings(&t.fake),
        json!({"without_label": "afkd/hold"}),
    ));
    let unit = plugin.poll()["unit"].clone();
    assert_eq!(unit["thread"], t.issue);
    assert_eq!(
        unit["fields"],
        json!({
            "title": TITLE,
            "url": t.fake.url(&t.issue),
            "labels": [],
            "state": "Todo",
            "parent": t.fake.identifier(&epic),
            "sub_issues": [t.fake.identifier(&one), t.fake.identifier(&two)],
            "blocked_by": [t.fake.identifier(&done), t.fake.identifier(&canceled)],
        })
    );
    end(plugin, &t.fake);
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
        json!({"require_label": "none-yet"}),
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
        json!({"require_label": "later"}),
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

// --- rate limits, through the outbox ---

/// An attempt note Linear rate limits is queued, `attempt_failed` answers `ok` at once, and
/// the plugin's worker lands it — no sooner than the `Retry-After` Linear named.
#[test]
fn a_rate_limited_attempt_note_is_queued_and_lands_from_the_outbox() {
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
    let stderr = plugin.stderr_soon("is queued");
    assert!(
        stderr.contains("linear could not take a write for issue ENG-101 \"") &&
        stderr.contains("Linear refused it (429): Rate limit exceeded; the comment \"[afkd-attempt] 1/2: run_cmd `cargo test` failed: 101…\" is queued and retried for up to 1h"),
        "{stderr}"
    );
    assert!(
        within(Duration::from_secs(20), || !said(
            &t.fake,
            &t.issue,
            "[afkd-attempt]"
        )
        .is_empty()),
        "the outbox never delivered; stderr: {}",
        plugin.stderr()
    );
    assert_eq!(
        said(&t.fake, &t.issue, "[afkd-attempt]"),
        [format!("[afkd-attempt] 1/2: {reason}")]
    );
    let posts: Vec<Instant> = t
        .fake
        .seen()
        .into_iter()
        .filter(|s| {
            s.operation == "CommentCreate"
                && s.variables["input"]["body"]
                    .as_str()
                    .is_some_and(|b| b.starts_with("[afkd-attempt]"))
        })
        .map(|s| s.at)
        .collect();
    assert_eq!(posts.len(), 2, "the refused try, and the one that landed");
    assert!(
        posts[1] - posts[0] >= Duration::from_secs(7),
        "retried after {:?}, before the Retry-After",
        posts[1] - posts[0]
    );
    assert!(plugin.stderr_soon("posting the comment").contains(
        "[linear] posting the comment \"[afkd-attempt] 1/2: run_cmd `cargo test` failed: 101…\" on issue ENG-101"
    ));
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
    let late = t.fake.comment(
        &t.issue,
        Author::User(t.chen.clone()),
        "别急着移走 🚨\n\n    max_backoff = 30\n",
        0,
    );
    t.fake.comment(
        &t.issue,
        Author::User(t.fake.key_user()),
        "Looking into it.",
        0,
    );
    t.fake.comment(
        &t.issue,
        Author::User(t.phil.clone()),
        "[afkd-attempt] 1/2: pasted from another issue",
        0,
    );
    let external = t.fake.comment(
        &t.issue,
        Author::External("x-alvaro".into()),
        "Exponential, please — see §4 🙏",
        0,
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
    assert_eq!(t.fake.labels(&t.issue), ["Awaiting Reply"]);
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
        "[linear] parked issue ENG-101 \"{TITLE}\" awaiting a reply (label \"Awaiting Reply\")\n"
    )));
    end(plugin, &t.fake);
}

/// A parked issue waits, its question unanswered, in the state it sat in; a human's reply
/// re-arms it ahead of `pick_from`, the badge and owner marker coming off.
#[test]
fn a_parked_issue_answered_is_reclaimed_and_unbadged() {
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

/// Linear out of reach while afkd reports a failed attempt: the call still answers `ok`,
/// the note is said queued, and once Linear is back the worker lands it.
#[test]
fn a_call_during_an_outage_answers_ok_and_lands_when_linear_returns() {
    let t = team();
    let mut plugin = Plugin::armed(settings(&t.fake));
    let unit = plugin.poll()["unit"].clone();
    t.fake.outage();
    assert_eq!(
        plugin.call(
            json!({"call": "attempt_failed", "key": unit["key"], "n": 1, "max": 2,
                           "reason": "git fetch: could not resolve host 🚨"})
        ),
        json!({"ok": true})
    );
    assert!(!t.fake.dropped().is_empty(), "the note tried Linear");
    assert!(plugin
        .stderr_soon("is queued")
        .contains("the comment \"[afkd-attempt] 1/2: git fetch: could not resolve host 🚨\" is queued and retried for up to 1h"));
    t.fake.restore();
    assert!(
        within(Duration::from_secs(15), || !said(
            &t.fake,
            &t.issue,
            "[afkd-attempt]"
        )
        .is_empty()),
        "the outbox never delivered; stderr: {}",
        plugin.stderr()
    );
    end(plugin, &t.fake);
}

/// afkd closing the pipe with writes still owed ends the process cleanly, and names each.
#[test]
fn closing_stdin_with_writes_owed_names_them() {
    let t = team();
    let mut plugin = Plugin::armed(settings(&t.fake));
    let unit = plugin.poll()["unit"].clone();
    t.fake.outage();
    plugin.call(
        json!({"call": "attempt_failed", "key": unit["key"], "n": 1, "max": 1,
                       "reason": "ran out of time"}),
    );
    let stderr = end(plugin, &t.fake);
    assert!(
        stderr.contains(&format!(
            "afkd-linear: afkd ended the plugin with the comment \"[afkd-attempt] 1/1: ran out \
             of time\" for issue ENG-101 \"{TITLE}\" still owed\n"
        )),
        "{stderr}"
    );
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
    let mut plugin = Plugin::armed(with(settings(&t.fake), json!({"pick_from": "Backlog"})));
    let line = plugin.call_raw(json!({"call": "poll"}));
    assert!(line.len() <= MAX_LINE, "{}", line.len());
    let reply: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(reply["unit"]["thread"], big);
    let brief = reply["unit"]["files"][0]["text"].as_str().unwrap();
    assert!(brief.ends_with("read the whole issue in Linear]"));
    assert!(plugin
        .stderr_soon("was cut")
        .contains("afkd-linear: the brief for ENG-102 was cut to fit afkd's 64 KiB plugin line\n"));
    end(plugin, &t.fake);
}

// --- the actions ---

/// Call `action` on the issue `unit` names, its handle first and `args` after, as afkd binds
/// them.
fn act(plugin: &mut Plugin, unit: &Value, action: &str, args: Value) -> Value {
    let mut bound = json!({"issue": {"id": unit["id"], "key": unit["key"]}});
    for (k, v) in args.as_object().unwrap() {
        bound[k] = v.clone();
    }
    plugin.call(json!({"call": "call", "action": action, "args": bound}))
}

/// How the success lines name the seeded issue.
fn named() -> String {
    format!("issue ENG-101 \"{TITLE}\"")
}

/// A comment lands byte for byte — multi-line markdown, wide CJK, an emoji, and a literal
/// `@{run:x}` afkd did not interpolate — authored by the credential, and is narrated.
#[test]
fn comment_posts_the_text_byte_for_byte() {
    let t = team();
    let mut plugin = Plugin::armed(settings(&t.fake));
    let unit = plugin.poll()["unit"].clone();
    let text = "## 完了 ✅\n\n- took 3m\n- log: @{run:x}\n\n```rust\nlet backoff = 30;\n```";
    assert_eq!(
        act(&mut plugin, &unit, "comment", json!({"text": text})),
        json!({"ok": true})
    );
    let posted = t.fake.comments(&t.issue).pop().expect("the comment");
    assert_eq!(posted.body, text);
    assert_eq!(posted.author, Author::User(t.fake.key_user()));
    assert!(plugin
        .stderr_soon("commenting on")
        .contains(&format!("[linear] commenting on {}\n", named())));
    end(plugin, &t.fake);
}

/// The description is replaced whole, with a wide multi-line one and then with none.
#[test]
fn describe_replaces_the_description() {
    let t = team();
    let mut plugin = Plugin::armed(settings(&t.fake));
    let unit = plugin.poll()["unit"].clone();
    let text = "## 背景\n\nThe backoff resets on every 401.\n\n- [ ] cap it at 30s 🚦";
    assert_eq!(
        act(&mut plugin, &unit, "describe", json!({"text": text})),
        json!({"ok": true})
    );
    assert_eq!(t.fake.description_of(&t.issue), text);
    assert_eq!(
        act(&mut plugin, &unit, "describe", json!({"text": ""})),
        json!({"ok": true})
    );
    assert_eq!(t.fake.description_of(&t.issue), "");
    assert!(plugin
        .stderr_lines_soon(3)
        .contains(&format!("[linear] describing {}\n", named())));
    end(plugin, &t.fake);
}

/// The title is replaced, and the line names the new one.
#[test]
fn rename_retitles_the_issue() {
    let t = team();
    let mut plugin = Plugin::armed(settings(&t.fake));
    let unit = plugin.poll()["unit"].clone();
    let title = "修复: cap the backoff at 30s — \"Retry-After\" 🚦";
    assert_eq!(
        act(&mut plugin, &unit, "rename", json!({"title": title})),
        json!({"ok": true})
    );
    assert_eq!(t.fake.title_of(&t.issue), title);
    assert!(plugin
        .stderr_soon("renaming")
        .contains(&format!("[linear] renaming {} to \"{title}\"\n", named())));
    end(plugin, &t.fake);
}

/// The issue lands in the team's state of the name; a name the team lacks is refused naming
/// it and the team, and the issue stays where it was.
#[test]
fn move_to_moves_the_issue_into_the_named_state() {
    let t = team();
    let mut plugin = Plugin::armed(settings(&t.fake));
    let unit = plugin.poll()["unit"].clone();
    let refusal = "linear resolve state: no workflow state named 'Nirgendwo 🚫' in team ENG";
    assert_eq!(
        act(
            &mut plugin,
            &unit,
            "move_to",
            json!({"state": "Nirgendwo 🚫"})
        ),
        json!({"ok": false, "error": refusal})
    );
    assert_eq!(t.fake.state_of(&t.issue), "Todo");
    assert_eq!(
        act(&mut plugin, &unit, "move_to", json!({"state": "Done"})),
        json!({"ok": true})
    );
    assert_eq!(t.fake.state_of(&t.issue), "Done");
    let stderr = plugin.stderr_soon("moving");
    assert!(stderr.contains(&format!("{refusal}\n")), "{stderr}");
    assert!(
        stderr.contains(&format!("[linear] moving {} to state \"Done\"\n", named())),
        "{stderr}"
    );
    end(plugin, &t.fake);
}

/// A label the team has is put on as it is; one it lacks is created for the team first.
#[test]
fn add_label_puts_a_label_on_creating_it_when_the_team_lacks_it() {
    let t = team();
    let other = t.fake.issue("Backlog", "Elsewhere", "");
    t.fake.label(&other, "afkd/ready");
    let mut plugin = Plugin::armed(settings(&t.fake));
    let unit = plugin.poll()["unit"].clone();
    assert_eq!(
        act(
            &mut plugin,
            &unit,
            "add_label",
            json!({"label": "afkd/ready"})
        ),
        json!({"ok": true})
    );
    assert!(
        !operations(&t.fake).contains(&"IssueLabelCreate".to_string()),
        "the team's label is reused"
    );
    let wide = "needs review 👀 レビュー";
    assert_eq!(
        act(&mut plugin, &unit, "add_label", json!({"label": wide})),
        json!({"ok": true})
    );
    assert!(operations(&t.fake).contains(&"IssueLabelCreate".to_string()));
    assert_eq!(t.fake.labels(&t.issue), ["afkd/ready", wide]);
    assert!(plugin.stderr_lines_soon(3).contains(&format!(
        "[linear] adding label \"{wide}\" to {}\n",
        named()
    )));
    end(plugin, &t.fake);
}

/// A label comes off; a name no label has is a no-op that answers `ok` and sends no removal.
#[test]
fn remove_label_takes_it_off_and_an_unknown_name_is_a_no_op() {
    let t = team();
    for label in ["afkd/ready", "Väntar på svar"] {
        t.fake.label(&t.issue, label);
    }
    let mut plugin = Plugin::armed(settings(&t.fake));
    let unit = plugin.poll()["unit"].clone();
    assert_eq!(
        act(
            &mut plugin,
            &unit,
            "remove_label",
            json!({"label": "Väntar på svar"})
        ),
        json!({"ok": true})
    );
    assert_eq!(t.fake.labels(&t.issue), ["afkd/ready"]);
    let removals = |fake: &FakeLinear| {
        operations(fake)
            .iter()
            .filter(|o| *o == "IssueRemoveLabel")
            .count()
    };
    assert_eq!(removals(&t.fake), 1);
    assert_eq!(
        act(
            &mut plugin,
            &unit,
            "remove_label",
            json!({"label": "Nirgendwo"})
        ),
        json!({"ok": true})
    );
    assert_eq!(removals(&t.fake), 1, "nothing to take off");
    assert_eq!(t.fake.labels(&t.issue), ["afkd/ready"]);
    assert!(plugin.stderr_lines_soon(3).contains(&format!(
        "[linear] removing label \"Väntar på svar\" from {}\n",
        named()
    )));
    end(plugin, &t.fake);
}

/// A new issue is filed in the service's team under no parent: in the team's default state
/// when the slot names none, and in the named one when it does, with its title and its
/// description.
#[test]
fn create_issue_files_one_in_the_team() {
    let t = team();
    let mut plugin = Plugin::armed(settings(&t.fake));
    let unit = plugin.poll()["unit"].clone();
    let body = "Found while fixing ENG-101.\n\n```\nRetry-After: 30\n```";
    assert_eq!(
        act(
            &mut plugin,
            &unit,
            "create_issue",
            json!({"title": "Read Retry-After 🚦", "description": body})
        ),
        json!({"ok": true})
    );
    assert_eq!(
        act(
            &mut plugin,
            &unit,
            "create_issue",
            json!({"title": "子任务: log the 401s", "description": "", "state": "In Progress"})
        ),
        json!({"ok": true})
    );
    let first = t.fake.find_by_title("Read Retry-After 🚦").expect("filed");
    assert_eq!(t.fake.state_of(&first), "Backlog", "the team's default");
    assert_eq!(t.fake.description_of(&first), body);
    assert_eq!(t.fake.parent_of(&first), None);
    let second = t.fake.find_by_title("子任务: log the 401s").expect("filed");
    assert_eq!(t.fake.state_of(&second), "In Progress");
    assert_eq!(t.fake.description_of(&second), "");
    let sent: Vec<Value> = t
        .fake
        .seen()
        .into_iter()
        .filter(|s| s.operation == "IssueCreate")
        .map(|s| s.variables["input"].clone())
        .collect();
    assert_eq!(sent[0]["teamId"], t.fake.team_id());
    assert!(
        sent[1].get("description").is_none(),
        "none for an empty one"
    );
    assert!(plugin.stderr_lines_soon(3).contains(&format!(
        "[linear] creating issue \"Read Retry-After 🚦\" in team ENG from {}\n",
        named()
    )));
    end(plugin, &t.fake);
}

/// A sub-issue is filed under the claimed issue, and the next claim of it lists it.
#[test]
fn create_sub_issue_files_one_under_the_issue() {
    let t = team();
    let mut plugin = Plugin::armed(settings(&t.fake));
    let unit = plugin.poll()["unit"].clone();
    assert_eq!(
        act(
            &mut plugin,
            &unit,
            "create_sub_issue",
            json!({"title": "Cap the backoff at 30s", "description": "split off 🚧"})
        ),
        json!({"ok": true})
    );
    let child = t
        .fake
        .find_by_title("Cap the backoff at 30s")
        .expect("filed");
    assert_eq!(t.fake.parent_of(&child).as_deref(), Some(t.issue.as_str()));
    assert_eq!(t.fake.state_of(&child), "Backlog");
    assert_eq!(
        finish(&mut plugin, &unit, "clean", facts("proceed", None)),
        json!({"ok": true})
    );
    let again = plugin.poll()["unit"].clone();
    assert_eq!(again["thread"], t.issue);
    assert_eq!(
        again["fields"]["sub_issues"],
        json!([t.fake.identifier(&child)])
    );
    assert!(plugin.stderr().contains(&format!(
        "[linear] creating sub-issue \"Cap the backoff at 30s\" under {}\n",
        named()
    )));
    end(plugin, &t.fake);
}

/// An archived issue is out of every scan: once its run is done it is never claimed again.
#[test]
fn archive_takes_the_issue_out_of_every_scan() {
    let t = team();
    let mut plugin = Plugin::armed(settings(&t.fake));
    let unit = plugin.poll()["unit"].clone();
    assert_eq!(
        act(&mut plugin, &unit, "archive", json!({})),
        json!({"ok": true})
    );
    assert!(t.fake.archived(&t.issue));
    assert_eq!(
        finish(&mut plugin, &unit, "clean", facts("proceed", None)),
        json!({"ok": true})
    );
    assert_eq!(
        plugin.poll(),
        json!({"fire": false}),
        "archived, so never claimed"
    );
    assert!(plugin
        .stderr()
        .contains(&format!("[linear] archiving {}\n", named())));
    end(plugin, &t.fake);
}

/// The thread is written oldest first, each comment as the brief renders it and markers
/// dropped: before afkd says anything it is the brief's own new-comments section, and
/// afkd's comment after it is afkd's. A relative `to` is refused and nothing is written.
#[test]
fn list_comments_writes_the_thread_oldest_first_as_the_brief_renders_it() {
    let t = team();
    let mut plugin = Plugin::armed(settings(&t.fake));
    let unit = plugin.poll()["unit"].clone();
    let brief = unit["files"][0]["text"].as_str().unwrap();
    let (_, section) = brief
        .split_once("## New comments\n\n")
        .expect("the brief's thread");
    let dir = TempDir::new("thread");
    let to = dir.path().join("thread 計画.md");
    assert_eq!(
        act(&mut plugin, &unit, "list_comments", json!({"to": to})),
        json!({"ok": true})
    );
    assert_eq!(
        std::fs::read_to_string(&to).unwrap(),
        section,
        "the brief's rendering"
    );
    assert_eq!(
        section,
        format!("**Phil Ek:** {PHIL}\n\n**陳大文:** {CHEN}\n")
    );

    act(
        &mut plugin,
        &unit,
        "comment",
        json!({"text": "Which header? 🤔"}),
    );
    assert_eq!(
        act(&mut plugin, &unit, "list_comments", json!({"to": to})),
        json!({"ok": true})
    );
    assert_eq!(
        std::fs::read_to_string(&to).unwrap(),
        format!("{section}\n**afkd:** Which header? 🤔\n")
    );
    assert_eq!(
        act(
            &mut plugin,
            &unit,
            "list_comments",
            json!({"to": "thread.md"})
        ),
        json!({"ok": false, "error": "list_comments: to must be absolute, not \"thread.md\""})
    );
    assert!(!std::path::Path::new("thread.md").exists());
    assert!(plugin.stderr_lines_soon(5).contains(&format!(
        "[linear] writing the comments on {} to {} (3 comments)\n",
        named(),
        to.display()
    )));
    end(plugin, &t.fake);
}

/// The description is read fresh, not off the claim: after a `describe` the file holds the
/// new text, and after one that empties it, nothing.
#[test]
fn read_description_writes_the_description_linear_holds_now() {
    let t = team();
    let mut plugin = Plugin::armed(settings(&t.fake));
    let unit = plugin.poll()["unit"].clone();
    let dir = TempDir::new("description");
    let to = dir.path().join("description.md");
    assert_eq!(
        act(&mut plugin, &unit, "read_description", json!({"to": to})),
        json!({"ok": true})
    );
    assert_eq!(std::fs::read_to_string(&to).unwrap(), BODY);
    let text = "## 背景\n\nCapped at 30s now. ✅\n";
    act(&mut plugin, &unit, "describe", json!({"text": text}));
    act(&mut plugin, &unit, "read_description", json!({"to": to}));
    assert_eq!(std::fs::read_to_string(&to).unwrap(), text, "a fresh read");
    act(&mut plugin, &unit, "describe", json!({"text": ""}));
    act(&mut plugin, &unit, "read_description", json!({"to": to}));
    assert_eq!(std::fs::read_to_string(&to).unwrap(), "");
    assert!(plugin.stderr_lines_soon(6).contains(&format!(
        "[linear] writing the description of {} to {}\n",
        named(),
        to.display()
    )));
    end(plugin, &t.fake);
}

/// Every sub-issue is written a line each with its state, past a page of them, and the lines
/// the brief lists are the file's own.
#[test]
fn list_sub_issues_writes_each_with_its_state() {
    let t = team();
    let mut children = vec![
        t.fake.issue("In Progress", "Cap the backoff at 30s", ""),
        t.fake
            .issue("Done", "子任务: read Retry-After 🚦 — \"seconds\"", ""),
    ];
    for n in 0..51 {
        children.push(t.fake.issue("Backlog", &format!("Step {n} 🧱"), ""));
    }
    for child in &children {
        t.fake.set_parent(child, &t.issue);
    }
    let mut plugin = Plugin::armed(settings(&t.fake));
    let unit = plugin.poll()["unit"].clone();
    let dir = TempDir::new("subs");
    let to = dir.path().join("subs.md");
    assert_eq!(
        act(&mut plugin, &unit, "list_sub_issues", json!({"to": to})),
        json!({"ok": true})
    );
    let written = std::fs::read_to_string(&to).unwrap();
    let lines: Vec<&str> = written.lines().collect();
    assert_eq!(lines.len(), 53, "every page");
    assert!(written.ends_with('\n'));
    assert_eq!(
        lines[..2],
        [
            format!(
                "- {} (In Progress) Cap the backoff at 30s",
                t.fake.identifier(&children[0])
            ),
            format!(
                "- {} (Done) 子任务: read Retry-After 🚦 — \"seconds\"",
                t.fake.identifier(&children[1])
            ),
        ]
    );
    assert_eq!(
        lines[52],
        format!(
            "- {} (Backlog) Step 50 🧱",
            t.fake.identifier(&children[52])
        )
    );
    let brief = unit["files"][0]["text"].as_str().unwrap();
    let listed: Vec<&str> = brief
        .split_once("## Sub-issues\n\n")
        .and_then(|(_, rest)| rest.split_once("\n\n"))
        .expect("the brief's sub-issues")
        .0
        .lines()
        .collect();
    assert_eq!(listed, lines[..listed.len()], "the brief's own lines");
    assert!(plugin
        .stderr_soon("writing the sub-issues")
        .contains(&format!(
            "[linear] writing the sub-issues of {} to {} (53 sub-issues)\n",
            named(),
            to.display()
        )));
    end(plugin, &t.fake);
}

/// A sub-issue named by its identifier lands in the state; an issue that is not one of this
/// issue's sub-issues is refused naming both, and stays where it is.
#[test]
fn move_sub_issue_moves_a_sub_issue_and_refuses_an_issue_that_is_none() {
    let t = team();
    let child = t.fake.issue("In Progress", "Cap the backoff at 30s", "");
    t.fake.set_parent(&child, &t.issue);
    let stranger = t.fake.issue("Backlog", "Someone else's", "");
    let mut plugin = Plugin::armed(settings(&t.fake));
    let unit = plugin.poll()["unit"].clone();
    let sub = t.fake.identifier(&child);
    assert_eq!(
        act(
            &mut plugin,
            &unit,
            "move_sub_issue",
            json!({"sub_issue": sub, "state": "Done"})
        ),
        json!({"ok": true})
    );
    assert_eq!(t.fake.state_of(&child), "Done");
    let other = t.fake.identifier(&stranger);
    assert_eq!(
        act(
            &mut plugin,
            &unit,
            "move_sub_issue",
            json!({"sub_issue": other, "state": "Done"})
        ),
        json!({"ok": false, "error": format!("linear move issue: no sub-issue {other} of ENG-101")})
    );
    assert_eq!(t.fake.state_of(&stranger), "Backlog");
    assert!(plugin.stderr_lines_soon(3).contains(&format!(
        "[linear] moving sub-issue {sub} of {} to state \"Done\"\n",
        named()
    )));
    end(plugin, &t.fake);
}

/// The post-run slot's calls, which afkd sends after `finish`, reach the finished issue its
/// handle names — after the next issue is claimed too — and never the live one.
#[test]
fn a_post_run_call_acts_on_the_finished_issue_its_handle_names() {
    let t = team();
    let next = t.fake.issue("Todo", "Next 🚧", "");
    let mut plugin = Plugin::armed(settings(&t.fake));
    let unit = plugin.poll()["unit"].clone();
    assert_eq!(
        finish(&mut plugin, &unit, "clean", facts("proceed", None)),
        json!({"ok": true})
    );
    assert_eq!(
        act(&mut plugin, &unit, "move_to", json!({"state": "Done"})),
        json!({"ok": true})
    );
    assert_eq!(t.fake.state_of(&t.issue), "Done");
    let live = plugin.poll()["unit"].clone();
    assert_eq!(live["thread"], next);
    assert_eq!(
        act(&mut plugin, &unit, "comment", json!({"text": "landed ✅"})),
        json!({"ok": true})
    );
    assert_eq!(said(&t.fake, &t.issue, "landed"), ["landed ✅"]);
    assert!(
        said(&t.fake, &next, "landed").is_empty(),
        "the live issue is untouched"
    );
    assert_eq!(t.fake.state_of(&next), "Todo");
    end(plugin, &t.fake);
}

/// With Linear out of reach, a slot's writes answer `ok` at once and are queued, the second
/// behind the first; once Linear is back they land in the order they were asked for, each
/// narrated.
#[test]
fn an_action_during_an_outage_answers_ok_and_lands_in_order() {
    let t = team();
    let mut plugin = Plugin::armed(settings(&t.fake));
    let unit = plugin.poll()["unit"].clone();
    t.fake.outage();
    let asked = Instant::now();
    assert_eq!(
        act(
            &mut plugin,
            &unit,
            "comment",
            json!({"text": "landed ✅\n\nsee the log"})
        ),
        json!({"ok": true})
    );
    assert_eq!(
        act(&mut plugin, &unit, "move_to", json!({"state": "Done"})),
        json!({"ok": true})
    );
    assert!(
        asked.elapsed() < Duration::from_secs(10),
        "answered at once"
    );
    let stderr = plugin.stderr_soon("queued behind");
    assert!(
        stderr.contains("the comment \"landed ✅…\" is queued and retried for up to 1h"),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!(
            "linear still owes {} earlier writes; moving it to state \"Done\" is queued behind \
             them",
            named()
        )),
        "{stderr}"
    );
    t.fake.restore();
    assert!(
        within(Duration::from_secs(20), || t.fake.state_of(&t.issue)
            == "Done"),
        "the outbox never delivered; stderr: {}",
        plugin.stderr()
    );
    assert_eq!(
        said(&t.fake, &t.issue, "landed"),
        ["landed ✅\n\nsee the log"]
    );
    let writes: Vec<String> = operations(&t.fake)
        .into_iter()
        .filter(|o| o == "CommentCreate" || o == "IssueUpdate")
        .collect();
    assert_eq!(
        writes[writes.len() - 2..],
        ["CommentCreate", "IssueUpdate"],
        "the comment first"
    );
    let stderr = plugin.stderr_soon("moving issue");
    assert!(
        stderr.contains(&format!("[linear] commenting on {}\n", named())),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!("[linear] moving {} to state \"Done\"\n", named())),
        "{stderr}"
    );
    end(plugin, &t.fake);
}

/// A call the plugin cannot do answers its sentence: an action it does not declare, a
/// string where the handle goes, and a key it holds no issue for. It answers on after.
#[test]
fn an_unknown_action_or_a_bad_handle_is_refused_with_its_sentence() {
    let t = team();
    let mut plugin = Plugin::armed(settings(&t.fake));
    let unit = plugin.poll()["unit"].clone();
    for (action, args, error) in [
        (
            "assign",
            json!({"issue": {"id": unit["id"], "key": unit["key"]}, "who": "me"}),
            "no action `assign`".to_string(),
        ),
        (
            "comment",
            json!({"issue": "ENG-101", "text": "done 🚀"}),
            "comment: parameter issue must be an item handle".to_string(),
        ),
        (
            "comment",
            json!({"issue": {"id": "ENG-999", "key": "nope#c9"}, "text": "done 🚀"}),
            "comment for nope#c9, which this plugin holds no issue for".to_string(),
        ),
    ] {
        assert_eq!(
            plugin.call(json!({"call": "call", "action": action, "args": args})),
            json!({"ok": false, "error": error})
        );
    }
    assert_eq!(
        plugin.call(json!({"call": "renew", "key": unit["key"], "renewal": 1})),
        json!({"ok": true})
    );
    assert_eq!(
        said(&t.fake, &t.issue, "done"),
        Vec::<String>::new(),
        "nothing posted"
    );
    end(plugin, &t.fake);
}

// --- request conformance ---

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
    let (status, body) = fake.post_raw(&request(
        json!({"some": {"name": {"eq": "Awaiting Reply"}}}),
    ));
    assert_eq!((status, body["errors"].is_null()), (200, true), "{body}");
    let (status, body) = fake.post_raw(&request(
        json!({"some": {"title": {"eq": "Awaiting Reply"}}}),
    ));
    assert_eq!(status, 400);
    assert_eq!(
        body["errors"][0]["message"],
        "$filter.labels.some.title: IssueLabelFilter declares no field title"
    );
    assert_eq!(body["errors"][0]["extensions"]["type"], "graphql error");
    // SDL-valid, but not the filter the plugin means: refused all the same.
    let (status, _) = fake.post_raw(&request(json!({"name": {"eq": "Awaiting Reply"}})));
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
