//! The wire contract suite: the real `afkd-gitlab` exec, spoken to exactly as afkd speaks
//! to it (`docs/plugins.md`, "The trigger protocol"), against a stateful fake GitLab
//! reached through the kind's own `base_url`. No afkd is involved.
//!
//! Every leg ends by closing stdin and holding the child to the wire's discipline — a
//! clean exit, one reply line per request, nothing else on stdout
//! ([`Plugin::finish`](common::Plugin::finish)) — so a diagnostic that strayed onto stdout
//! fails whichever leg wrote it.

mod common;

use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use common::fake::{FakeGitlab, TOKEN};
use common::{Plugin, MAX_LINE};

/// The token's user: non-ASCII and bracketed, as the built-in's own fixtures are.
const ME: &str = "björn-öst[bot]";
const ME_ID: u64 = 7;
/// A human who shares the issue with the bot.
const HUMAN: &str = "陳大文";
/// A project in a nested group, with a dot: its path crosses as one encoded segment.
const PROJECT: &str = "acme/sub.group/widgets";
const ENCODED: &str = "acme%2Fsub.group%2Fwidgets";
const TITLE: &str = "修复 the retry storm 🚨";
const BODY: &str = "The client retries forever once the token expires.\n\n\
                    ```rust\nlet backoff = Duration::ZERO;\n```\n\n— reported by 陳大文";

/// A project with issue #7 up for grabs, a human already assigned to it.
fn forge() -> FakeGitlab {
    let fake = FakeGitlab::start(ME_ID, ME);
    fake.user(99, HUMAN);
    fake.issue(PROJECT, 7, TITLE, BODY, &["afkd::ready"], &[HUMAN]);
    fake
}

/// A full `issue` block, lowered to JSON as afkd lowers it: every value a string — and
/// afkd's own three keys along for the ride. The hooks never cross here; afkd runs them.
fn settings(fake: &FakeGitlab) -> Value {
    json!({
        "base_url": fake.base_url(),
        "project": PROJECT,
        "token": TOKEN,
        "source_label": "afkd::ready",
        "poll_interval": "30s",
        "max_attempts": "1",
        "follow_comments": "2m",
    })
}

/// The `finish` envelope's facts, as afkd writes them.
fn facts(signal: &str, reason: Option<&str>) -> Value {
    json!({"signal": signal, "reason": reason, "duration_ms": 168000, "cost": 0.4217,
           "turns": null, "tokens": null, "run_name": "260925-095800-unit-7-1"})
}

/// A slot afkd runs on `unit`: each of `actions` one `call` on the unit's item, in order,
/// every one landing — under the armed kind's names, so `comment` is `mr_comment` on the
/// mr kind.
fn hook(plugin: &mut Plugin, unit: &Value, actions: &[(&str, Value)]) {
    let prefix = match plugin.kind() {
        "issue" => String::new(),
        kind => format!("{kind}_"),
    };
    for (verb, args) in actions {
        let action = format!("{prefix}{verb}");
        assert_eq!(
            plugin.act(&action, args.clone(), unit),
            json!({"ok": true}),
            "{action}: {}",
            plugin.stderr()
        );
    }
}

/// The `on_claim` of a config that takes the item — `gitlab.assign_me(issue)` then
/// `gitlab.label_add(issue, working)`, or the mr kind's `gitlab.mr_assign_me(mr)` and so on — as
/// afkd runs it after the `poll`.
fn on_claim(plugin: &mut Plugin, unit: &Value, working: &str) {
    hook(
        plugin,
        unit,
        &[
            ("assign_me", json!({})),
            ("label_add", json!({ "label": working })),
        ],
    );
}

fn finish(plugin: &mut Plugin, unit: &Value, outcome: &str, facts: Value) -> Value {
    plugin.call(
        json!({"call": "finish", "id": unit["id"], "key": unit["key"],
                       "outcome": outcome, "facts": facts}),
    )
}

/// How long a write queued through an outage may take to land once the forge is back:
/// the worker's first retry comes 5 s after the failure, and its second 10 s after that.
const RETRY_WAIT: Duration = Duration::from_secs(20);

/// Wait, up to [`RETRY_WAIT`], for `done` to hold — the outbox's worker landing what an
/// outage queued, on its own thread and its own clock.
fn soon(what: &str, plugin: &Plugin, done: impl Fn() -> bool) {
    let deadline = Instant::now() + RETRY_WAIT;
    while !done() {
        assert!(
            Instant::now() < deadline,
            "{what} within {RETRY_WAIT:?}; stderr: {}",
            plugin.stderr()
        );
        thread::sleep(Duration::from_millis(50));
    }
}

/// The writes the forge answered from request `from` on, as `METHOD path`, and a `PUT`'s
/// one field after it — GitLab's item `PUT` carries every lifecycle write.
fn writes_since(fake: &FakeGitlab, from: usize) -> Vec<String> {
    fake.seen()[from..]
        .iter()
        .filter(|seen| seen.method != "GET")
        .map(|seen| {
            let fields: Vec<String> = serde_json::from_str::<Value>(&seen.body)
                .ok()
                .and_then(|body| body.as_object().map(|o| o.keys().cloned().collect()))
                .unwrap_or_default();
            match fields.as_slice() {
                [field] if seen.method == "PUT" => {
                    format!("{} {} {field}", seen.method, seen.path)
                }
                _ => format!("{} {}", seen.method, seen.path),
            }
        })
        .collect()
}

/// The ids of the claim markers on an issue.
fn markers(fake: &FakeGitlab, iid: u64) -> Vec<u64> {
    fake.notes(PROJECT, iid)
        .into_iter()
        .filter(|n| n.body.starts_with("[afkd-claim]"))
        .map(|n| n.id)
        .collect()
}

fn labels(fake: &FakeGitlab, iid: u64) -> Vec<String> {
    fake.issue_state(PROJECT, iid).labels
}

fn assignees(fake: &FakeGitlab, iid: u64) -> Vec<String> {
    fake.issue_state(PROJECT, iid).assignees
}

// --- hello ---

/// The accepted `hello` lists the calls the kind answers, after reading the token's username
/// off the forge — the one request `hello` makes.
#[test]
fn hello_arms_and_lists_every_call_it_answers() {
    let fake = forge();
    let mut plugin = Plugin::spawn();
    assert_eq!(
        plugin.hello("issue", settings(&fake)),
        json!({"ok": true, "proto": 2, "calls": ["release", "renew", "comments"]})
    );
    let seen: Vec<(String, String)> = fake
        .seen()
        .into_iter()
        .map(|r| (r.method, r.path))
        .collect();
    assert_eq!(seen, [("GET".to_string(), "/api/v4/user".to_string())]);
    plugin.finish();
}

/// What `hello` cannot arm with answers `ok:false`, with the problem on stderr in the
/// built-in's own sentence — a kind this plugin does not provide, a block the manifest
/// cannot refuse, and a protocol from a later afkd.
#[test]
fn hello_refuses_what_it_cannot_arm_with() {
    let fake = forge();
    let mut no_project = settings(&fake);
    no_project["project"] = json!("");
    for (kind, proto, settings, sentence) in [
        (
            "gitlab_issues",
            2,
            settings(&fake),
            "kind `gitlab_issues` is not provided by @afkd/gitlab",
        ),
        (
            "gitlab_issue",
            2,
            settings(&fake),
            "kind `gitlab_issue` is not provided by @afkd/gitlab",
        ),
        (
            "issue",
            2,
            no_project,
            "trigger issue: setting `project`: a gitlab trigger needs a `project` (numeric id \
             or path-with-namespace)",
        ),
        (
            "issue",
            1,
            settings(&fake),
            "afkd speaks plugin protocol 1, and this plugin speaks 2",
        ),
        (
            "mr",
            2,
            json!({"base_url": fake.base_url(), "project": "", "token": TOKEN,
                   "author_me": "true"}),
            "trigger mr: setting `project`: a gitlab trigger needs a `project` (numeric id or \
             path-with-namespace)",
        ),
    ] {
        let mut plugin = Plugin::spawn();
        let reply = plugin
            .call(json!({"call": "hello", "proto": proto, "kind": kind, "settings": settings}));
        assert_eq!(reply, json!({"ok": false, "proto": 2}), "{kind} {proto}");
        let stderr = plugin.stderr_soon(sentence);
        assert_eq!(stderr, format!("afkd-gitlab: {sentence}\n"));
        plugin.finish();
    }
    assert!(fake.seen().is_empty(), "a refused hello touches no forge");
}

// --- poll ---

/// The won race hands over exactly the unit the built-in would have run: its journal key
/// and session thread, no `seen`, its identity, the four env names the skill reads, and
/// the scratch layout with the brief unframed — with every field the manifest's `Issue`
/// declares, its labels by name as the poll read them. The forge then holds the claim —
/// the marker and the gate, and nothing a hook does until `on_claim`'s calls land.
#[test]
fn a_won_race_hands_over_the_built_ins_unit() {
    let fake = forge();
    fake.issue(
        PROJECT,
        7,
        TITLE,
        BODY,
        &["afkd::ready", "優先::high"],
        &[HUMAN],
    );
    let mut plugin = Plugin::armed(settings(&fake));
    let reply = plugin.poll();
    assert_eq!(reply["fire"], true, "{}", plugin.stderr());
    let marker = markers(&fake, 7);
    assert_eq!(marker.len(), 1, "one marker, ours");
    assert_eq!(
        reply["unit"],
        json!({
            "id": "7",
            "key": format!("acme/sub.group/widgets#7#{}", marker[0]),
            "fields": {
                "title": TITLE,
                "url": fake.issue_url(PROJECT, 7),
                "number": 7,
                "labels": ["afkd::ready", "優先::high"],
            },
            "thread": "acme/sub.group/widgets#7",
            "seen": [],
            "self": ME,
            "env": {
                "GITLAB_BASE_URL": fake.base_url(),
                "GITLAB_ISSUE_NUMBER": "7",
                "GITLAB_PROJECT": PROJECT,
                "GITLAB_TOKEN": TOKEN,
            },
            "files": [
                {"path": "task.md", "text": format!("{TITLE}\n\n{BODY}")},
                {"path": "issue/number", "text": "7"},
            ],
        })
    );

    let notes = fake.notes(PROJECT, 7);
    assert_eq!(notes[0].body, "[afkd-claim] owner=björn-öst[bot]");
    assert_eq!(notes[0].author, ME);
    assert_eq!(
        labels(&fake, 7),
        ["afkd::ready", "優先::high", "afkd::claimed"]
    );
    assert_eq!(assignees(&fake, 7), [HUMAN], "the claim assigns no one");
    on_claim(&mut plugin, &reply["unit"], "afkd::working");
    assert_eq!(
        labels(&fake, 7),
        [
            "afkd::ready",
            "優先::high",
            "afkd::claimed",
            "afkd::working"
        ]
    );
    assert_eq!(assignees(&fake, 7), [HUMAN, ME], "the human was kept");
    // The race is post → settle → re-read: the marker went up before the thread read.
    let seen = fake.seen();
    let notes_path = format!("/api/v4/projects/{ENCODED}/issues/7/notes");
    let post = seen
        .iter()
        .position(|r| r.method == "POST" && r.path == notes_path)
        .expect("the marker was posted");
    assert!(seen[post + 1..]
        .iter()
        .any(|r| r.method == "GET" && r.path == notes_path));
    // The issues listing is asked exactly as the built-in asks it, with the project path
    // encoded into one segment.
    let list = seen
        .iter()
        .find(|r| r.path == format!("/api/v4/projects/{ENCODED}/issues"))
        .expect("the issues were listed");
    assert_eq!(
        list.query,
        [
            ("state".to_string(), "opened".to_string()),
            ("labels".to_string(), "afkd::ready".to_string()),
        ]
    );
    // And the claimed issue is not claimed again.
    assert_eq!(plugin.poll(), json!({"fire": false}));
    plugin.finish();
}

/// A live rival marker posted a minute earlier out-orders ours: nothing is handed over,
/// our marker is taken back, and no status is written.
#[test]
fn a_lost_race_hands_over_nothing_and_takes_its_marker_back() {
    let fake = forge();
    fake.note_with_id(
        424_242,
        PROJECT,
        7,
        "autocoder",
        "[afkd-claim] owner=autocoder",
        60,
    );
    let mut plugin = Plugin::armed(settings(&fake));
    assert_eq!(plugin.poll(), json!({"fire": false}));
    assert_eq!(
        markers(&fake, 7),
        [424_242],
        "only the rival's marker is left"
    );
    assert_eq!(labels(&fake, 7), ["afkd::ready"]);
    assert_eq!(assignees(&fake, 7), [HUMAN]);
    assert!(
        fake.seen().iter().all(|r| r.method != "PUT"),
        "no status write: {:?}",
        fake.seen()
    );
    plugin.finish();
}

#[test]
fn a_lost_race_moves_on_to_the_next_issue() {
    let fake = forge();
    fake.issue(PROJECT, 8, "Cap the backoff", "", &["afkd::ready"], &[]);
    fake.note_with_id(
        424_242,
        PROJECT,
        7,
        "autocoder",
        "[afkd-claim] owner=autocoder",
        60,
    );
    let mut plugin = Plugin::armed(settings(&fake));
    let unit = plugin.poll()["unit"].clone();
    assert_eq!(unit["thread"], "acme/sub.group/widgets#8");
    // A bodyless issue briefs with its title alone.
    assert_eq!(unit["files"][0]["text"], "Cap the backoff");
    assert_eq!(markers(&fake, 7), [424_242]);
    assert!(labels(&fake, 8).contains(&"afkd::claimed".to_string()));
    plugin.finish();
}

#[test]
fn an_empty_project_polls_idle_and_a_closed_claimed_or_unlabelled_issue_is_passed_over() {
    let fake = FakeGitlab::start(ME_ID, ME);
    let mut plugin = Plugin::armed(settings(&fake));
    assert_eq!(plugin.poll(), json!({"fire": false}));

    fake.issue(
        PROJECT,
        3,
        "already mine",
        "",
        &["afkd::ready", "afkd::claimed"],
        &[],
    );
    fake.issue(PROJECT, 4, "done", "", &["afkd::ready"], &[]);
    fake.close(PROJECT, 4);
    fake.issue(PROJECT, 5, "not for us", "", &[], &[HUMAN]);
    assert_eq!(plugin.poll(), json!({"fire": false}));
    assert!(
        fake.seen().iter().all(|r| r.method == "GET"),
        "an idle scan writes nothing: {:?}",
        fake.seen()
    );
    plugin.finish();
}

/// A forge that cannot be read is the built-in's idle beat, said on stderr; the next beat,
/// with the forge back, claims.
#[test]
fn a_forge_error_is_an_idle_beat_and_the_next_poll_claims() {
    let fake = forge();
    fake.fail("list issues", 500);
    let mut plugin = Plugin::armed(settings(&fake));
    assert_eq!(plugin.poll(), json!({"fire": false}));
    let stderr = plugin.stderr_soon("list issues");
    assert_eq!(
        stderr,
        "afkd-gitlab: gitlab list issues: forge returned status 500\n"
    );
    fake.heal("list issues");
    assert_eq!(plugin.poll()["unit"]["id"], "7");
    plugin.finish();
}

// --- renew, comments ---

#[test]
fn renew_rewrites_the_marker_in_place() {
    let fake = forge();
    let mut plugin = Plugin::armed(settings(&fake));
    let unit = plugin.poll()["unit"].clone();
    let before = fake.notes(PROJECT, 7)[0].clone();
    fake.advance(300);
    assert_eq!(
        plugin.call(json!({"call": "renew", "key": unit["key"], "renewal": 1})),
        json!({"ok": true})
    );
    let after = fake.notes(PROJECT, 7)[0].clone();
    assert_eq!(after.id, before.id, "edited, not re-posted");
    assert_eq!(after.body, "[afkd-claim] owner=björn-öst[bot] renewal=1");
    assert_eq!(after.created, before.created);
    assert_eq!(
        after.updated,
        before.updated + 300,
        "the liveness stamp moved"
    );
    // A key it does not hold is declined, not guessed at.
    assert_eq!(
        plugin.call(json!({"call": "renew", "key": "acme/sub.group/widgets#8#1", "renewal": 1})),
        json!({"ok": false})
    );
    plugin.finish();
}

/// `comments` reports what afkd has not been told about, each as afkd's watch reads it:
/// the marker and the bot's own words are left out, a second read carries only what is
/// new, and a forge that cannot be read is `null`, not an empty thread.
#[test]
fn comments_report_what_afkd_has_not_seen() {
    let fake = forge();
    let mut plugin = Plugin::armed(settings(&fake));
    let unit = plugin.poll()["unit"].clone();
    let read = |plugin: &mut Plugin| plugin.call(json!({"call": "comments", "key": unit["key"]}));

    fake.advance(60);
    let a = fake.note(
        PROJECT,
        7,
        HUMAN,
        "看起来不对 🚨\n\n    max_backoff = 30\n",
        0,
    );
    fake.advance(1);
    fake.note(PROJECT, 7, ME, "Looking into it.", 0);
    fake.advance(1);
    let b = fake.note(PROJECT, 7, "álvaro", "Exponential, please — see §4 🙏", 0);
    let reply = read(&mut plugin);
    assert_eq!(
        reply,
        json!({"comments": [
            {"id": a.to_string(), "author": HUMAN, "author_name": HUMAN,
             "body": "看起来不对 🚨\n\n    max_backoff = 30\n", "at": "2026-09-25T09:59:00Z"},
            {"id": b.to_string(), "author": "álvaro", "author_name": "álvaro",
             "body": "Exponential, please — see §4 🙏", "at": "2026-09-25T09:59:02Z"},
        ]})
    );

    fake.advance(30);
    let c = fake.note(PROJECT, 7, "álvaro", "…and cap it at 30s.", 0);
    let reply = read(&mut plugin);
    let ids: Vec<&str> = reply["comments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [c.to_string()]);

    fake.fail("list notes", 500);
    assert_eq!(read(&mut plugin), json!({"comments": null}));
    fake.heal("list notes");
    assert_eq!(read(&mut plugin), json!({"comments": []}));
    assert!(
        plugin
            .stderr_soon("list notes")
            .contains("afkd-gitlab: gitlab list notes: forge returned status 500"),
        "{}",
        plugin.stderr()
    );
    plugin.finish();
}

// --- release ---

/// Issue #9, claimed by a run that crashed: the status label, the bot and a human
/// assigned, and the bot's renewed marker. Returns the journal key that run left.
fn crashed(fake: &FakeGitlab) -> String {
    fake.issue(
        PROJECT,
        9,
        "Crashed mid-run",
        "",
        &["afkd::ready", "afkd::claimed", "afkd::working"],
        &[ME, HUMAN],
    );
    fake.note_with_id(
        6744,
        PROJECT,
        9,
        ME,
        "[afkd-claim] owner=björn-öst[bot] renewal=4",
        900,
    );
    "acme/sub.group/widgets#9#6744".to_string()
}

/// afkd's reaper runs before its poll, so a freshly spawned child's first call after
/// `hello` can be `release` of a key a crashed run left. The identity `hello` read serves
/// it, and the release is the built-in's whole one: the status label, the bot's assignment (the
/// human's is kept) and the marker. A key of the pre-marker shape names nothing (`null`).
#[test]
fn release_reads_the_key_shapes() {
    let fake = forge();
    let key = crashed(&fake);
    let mut plugin = Plugin::armed(settings(&fake));
    assert_eq!(
        plugin.call(json!({"call": "release", "key": key})),
        json!({"released": true})
    );
    assert_eq!(labels(&fake, 9), ["afkd::ready", "afkd::working"]);
    assert_eq!(assignees(&fake, 9), [HUMAN], "only afkd let go");
    assert!(markers(&fake, 9).is_empty());
    let seen = fake.seen();
    assert_eq!(
        (seen[0].method.as_str(), seen[0].path.as_str()),
        ("GET", "/api/v4/user"),
        "the identity came first"
    );

    for key in ["acme/sub.group/widgets#9", "garbage"] {
        assert_eq!(
            plugin.call(json!({"call": "release", "key": key})),
            json!({"released": null})
        );
    }
    plugin.finish();
}

/// With `/user` down — at `hello`, which is accepted without `me`, and at the `release` —
/// `release` writes nothing and keeps the key (`false`), so afkd asks again; healed, the
/// same key releases in full.
#[test]
fn release_without_an_identity_keeps_the_key() {
    let fake = forge();
    let key = crashed(&fake);
    fake.fail("current user", 500);
    let mut plugin = Plugin::armed(settings(&fake));
    assert_eq!(
        plugin.call(json!({"call": "release", "key": key})),
        json!({"released": false})
    );
    assert_eq!(
        plugin.stderr_lines_soon(2),
        "afkd-gitlab: gitlab current user: forge returned status 500\n".repeat(2)
    );
    assert!(
        fake.seen().iter().all(|r| r.path == "/api/v4/user"),
        "nothing past the identity: {:?}",
        fake.seen()
    );
    assert!(labels(&fake, 9).contains(&"afkd::claimed".to_string()));

    fake.heal("current user");
    assert_eq!(
        plugin.call(json!({"call": "release", "key": key})),
        json!({"released": true})
    );
    assert_eq!(assignees(&fake, 9), [HUMAN]);
    assert!(markers(&fake, 9).is_empty());
    plugin.finish();
}

/// A unit afkd hands straight back — it was stopping when the poll landed — is released
/// and forgotten: a later call about it is declined.
#[test]
fn a_released_unit_is_forgotten() {
    let fake = forge();
    let mut plugin = Plugin::armed(settings(&fake));
    let unit = plugin.poll()["unit"].clone();
    assert_eq!(
        plugin.call(json!({"call": "release", "key": unit["key"]})),
        json!({"released": true})
    );
    assert!(markers(&fake, 7).is_empty());
    assert!(!labels(&fake, 7).contains(&"afkd::claimed".to_string()));
    assert_eq!(assignees(&fake, 7), [HUMAN]);
    assert_eq!(
        plugin.call(json!({"call": "renew", "key": unit["key"], "renewal": 1})),
        json!({"ok": false})
    );
    plugin.finish();
}

// --- finish, per outcome ---

/// A clean finish only drops the marker; the issue closes when `on_done`'s calls, which
/// afkd sends after the `finish`, land.
#[test]
fn a_clean_finish_drops_the_marker_and_on_done_closes_after() {
    let fake = forge();
    let mut plugin = Plugin::armed(settings(&fake));
    let unit = plugin.poll()["unit"].clone();
    on_claim(&mut plugin, &unit, "afkd::working");
    assert_eq!(
        finish(&mut plugin, &unit, "clean", facts("proceed", None)),
        json!({"ok": true})
    );
    assert_eq!(
        fake.issue_state(PROJECT, 7).state,
        "opened",
        "the finish closes nothing"
    );
    assert!(markers(&fake, 7).is_empty());

    hook(
        &mut plugin,
        &unit,
        &[
            ("label_remove", json!({"label": "afkd::working"})),
            ("close", json!({})),
        ],
    );
    let issue = fake.issue_state(PROJECT, 7);
    assert_eq!(issue.state, "closed");
    assert_eq!(issue.labels, ["afkd::ready", "afkd::claimed"]);
    assert!(fake
        .seen()
        .iter()
        .any(|r| r.method == "PUT" && r.body == r#"{"state_event":"close"}"#));
    plugin.finish();
}

#[test]
fn a_failed_finish_then_on_fail_keeps_a_human_assignee() {
    let fake = forge();
    let mut plugin = Plugin::armed(settings(&fake));
    let unit = plugin.poll()["unit"].clone();
    on_claim(&mut plugin, &unit, "afkd::working");
    assert_eq!(assignees(&fake, 7), [HUMAN, ME]);
    let reply = finish(
        &mut plugin,
        &unit,
        "failed",
        facts("fault", Some("cargo test: 3 failed")),
    );
    assert_eq!(reply, json!({"ok": true}));
    hook(
        &mut plugin,
        &unit,
        &[
            ("label_remove", json!({"label": "afkd::working"})),
            ("unassign", json!({})),
        ],
    );
    let issue = fake.issue_state(PROJECT, 7);
    assert_eq!(issue.assignees, [HUMAN], "only afkd let go");
    assert_eq!(issue.state, "opened");
    assert!(!issue.labels.contains(&"afkd::working".to_string()));
    assert!(markers(&fake, 7).is_empty());
    plugin.finish();
}

/// GitLab has no park of its own, so a `park` verdict ends the run like any other: the
/// marker goes and nothing else is written. A hook's comment, which afkd interpolated
/// before sending, is posted as it came.
#[test]
fn a_park_finish_only_releases_the_marker_and_a_hook_comment_posts_verbatim() {
    let fake = forge();
    let mut plugin = Plugin::armed(settings(&fake));
    let unit = plugin.poll()["unit"].clone();
    let reply = finish(
        &mut plugin,
        &unit,
        "park",
        facts("fault", Some("parked: awaiting a human reply")),
    );
    assert_eq!(reply, json!({"ok": true}));
    let issue = fake.issue_state(PROJECT, 7);
    assert_eq!(issue.assignees, [HUMAN]);
    assert_eq!(issue.state, "opened");
    assert!(fake.notes(PROJECT, 7).is_empty(), "the marker went");

    let stopped = "Stopped after 2m 48s — log: .afkd/runs/260925-095800-unit-7-1/run.log\n\n\
                   看起来 it needs a human call 🙏";
    hook(
        &mut plugin,
        &unit,
        &[("comment", json!({ "text": stopped }))],
    );
    let said: Vec<String> = fake.notes(PROJECT, 7).into_iter().map(|n| n.body).collect();
    assert_eq!(said, [stopped]);
    plugin.finish();
}

/// Nothing a finish does can leave it undelivered, so nothing is `held`: a marker delete
/// the forge refused — which no retry changes — is said lost, the finish still answers
/// plain `ok`, and the claim — the label — stays until afkd's `release` or a hook takes
/// it.
#[test]
fn a_finish_whose_marker_delete_fails_still_answers_plain_ok() {
    let fake = forge();
    let mut plugin = Plugin::armed(settings(&fake));
    let unit = plugin.poll()["unit"].clone();
    fake.fail("delete comment", 403);

    assert_eq!(
        finish(&mut plugin, &unit, "clean", facts("proceed", None)),
        json!({"ok": true})
    );
    assert_eq!(
        plugin.stderr_soon("delete comment"),
        format!(
            "afkd-gitlab: {}: deleting the claim marker is lost — gitlab delete comment: forge \
             returned status 403\n",
            issue_named()
        )
    );
    assert_eq!(markers(&fake, 7).len(), 1, "left to age out");
    assert!(labels(&fake, 7).contains(&"afkd::claimed".to_string()));
    assert_eq!(plugin.poll(), json!({"fire": false}), "still claimed");
    plugin.finish();
}

// --- the hooks' actions over `call` ---

/// Every action a hook can call crosses the real wire as one `call` and lands on the
/// forge, on the claimed issue: through a live unit, then — after `finish` — a post-run
/// call still reaches it, until afkd releases the key and a call is refused by name.
#[test]
fn every_action_over_call_lands_on_the_forge() {
    let fake = forge();
    let mut plugin = Plugin::armed(settings(&fake));
    let unit = plugin.poll()["unit"].clone();
    let key = &unit["key"];
    let markdown = "## 完了 ✅\n\n- took 3m 12s\n- literal: #{run.x}\n\n```\nok\n```";

    assert_eq!(
        plugin.act("assign_me", json!({}), &unit),
        json!({"ok": true})
    );
    assert_eq!(assignees(&fake, 7), [HUMAN, ME]);
    assert_eq!(
        plugin.act("label_add", json!({"label": "afkd::reviewed ✅"}), &unit),
        json!({"ok": true})
    );
    assert_eq!(
        plugin.act("label_remove", json!({"label": "afkd::ready"}), &unit),
        json!({"ok": true})
    );
    assert_eq!(labels(&fake, 7), ["afkd::claimed", "afkd::reviewed ✅"]);
    assert_eq!(
        plugin.act("comment", json!({"text": markdown}), &unit),
        json!({"ok": true})
    );
    let said: Vec<(String, String)> = fake
        .notes(PROJECT, 7)
        .into_iter()
        .filter(|n| !n.body.starts_with("[afkd-claim]"))
        .map(|n| (n.author, n.body))
        .collect();
    assert_eq!(said, [(ME.to_string(), markdown.to_string())]);
    assert_eq!(
        plugin.act("unassign", json!({}), &unit),
        json!({"ok": true})
    );
    assert_eq!(assignees(&fake, 7), [HUMAN]);

    assert_eq!(
        finish(&mut plugin, &unit, "clean", facts("proceed", None)),
        json!({"ok": true})
    );
    assert_eq!(fake.issue_state(PROJECT, 7).state, "opened");
    assert_eq!(plugin.act("close", json!({}), &unit), json!({"ok": true}));
    assert_eq!(fake.issue_state(PROJECT, 7).state, "closed");

    assert_eq!(
        plugin.call(json!({"call": "release", "key": key})),
        json!({"released": true})
    );
    let refused = format!(
        "comment for {}, which this plugin holds no claim on",
        key.as_str().unwrap()
    );
    assert_eq!(
        plugin.act("comment", json!({"text": "late"}), &unit),
        json!({"ok": false, "error": refused})
    );
    let stderr = plugin.stderr_soon(&refused);
    assert!(
        stderr.contains(&format!("afkd-gitlab: {refused}")),
        "{stderr}"
    );
    plugin.finish();
}

// --- calls the kind does not list ---

/// `classify` and `attempt_failed` are refused like any unlisted call — and the wire
/// stays in step: the next `poll` is answered as normal.
#[test]
fn classify_and_attempt_failed_are_refused_as_unlisted() {
    let fake = forge();
    let mut plugin = Plugin::armed(settings(&fake));
    for request in [
        json!({"call": "classify", "key": "acme/sub.group/widgets#7#1",
               "scratch": "/tmp/afkd-scratch", "outcome": "failed"}),
        json!({"call": "attempt_failed", "key": "acme/sub.group/widgets#7#1", "n": 1, "max": 2,
               "reason": "agent exited 1"}),
    ] {
        assert_eq!(plugin.call(request), json!({"ok": false}));
    }
    let refused = "afkd-gitlab: afkd sent a call this plugin did not list in its `hello` reply\n";
    assert_eq!(plugin.stderr_lines_soon(2), refused.repeat(2));
    assert_eq!(plugin.poll()["unit"]["id"], "7");
    plugin.finish();
}

// --- the line budget ---

/// A 100 KiB issue body cannot cross in one 64 KiB line: the brief is cut, the line fits
/// and decodes, and the brief says where the rest is.
#[test]
fn an_oversized_brief_is_cut_to_fit_one_line() {
    let fake = FakeGitlab::start(ME_ID, ME);
    let body = "看起来不对 🚨 — the retry path.\n".repeat(2_600);
    assert!(body.len() > 100 * 1024);
    fake.issue(PROJECT, 7, TITLE, &body, &["afkd::ready"], &[]);
    let mut plugin = Plugin::armed(settings(&fake));
    let line = plugin.call_raw(json!({"call": "poll"}));
    assert!(line.len() <= MAX_LINE, "{}", line.len());
    let reply: Value = serde_json::from_str(&line).unwrap();
    let brief = reply["unit"]["files"][0]["text"].as_str().unwrap();
    assert!(
        brief.starts_with(&format!("{TITLE}\n\n看起来不对 🚨")),
        "{}",
        &brief[..80]
    );
    assert!(
        brief.ends_with("read the whole issue with the gitlab skill]"),
        "{}",
        &brief[brief.len() - 200..]
    );
    assert!(plugin
        .stderr_soon("cut to fit")
        .contains("the brief for acme/sub.group/widgets#7 was cut to fit"));
    plugin.finish();
}

/// 400 notes of 300 bytes are over the line too: the reply keeps the newest that fit.
#[test]
fn an_overflowing_thread_is_cut_to_fit_one_line() {
    let fake = forge();
    let mut plugin = Plugin::armed(settings(&fake));
    let unit = plugin.poll()["unit"].clone();
    let mut last = 0;
    for _ in 0..400 {
        fake.advance(1);
        last = fake.note(PROJECT, 7, "álvaro", &"x".repeat(300), 0);
    }
    let line = plugin.call_raw(json!({"call": "comments", "key": unit["key"]}));
    assert!(line.len() <= MAX_LINE, "{}", line.len());
    let reply: Value = serde_json::from_str(&line).unwrap();
    let kept = reply["comments"].as_array().unwrap();
    assert!(kept.len() > 100 && kept.len() < 400, "{}", kept.len());
    assert_eq!(
        kept.last().unwrap()["id"],
        last.to_string(),
        "the newest survive"
    );
    let stderr = plugin.stderr_soon("did not fit");
    assert!(
        stderr.contains("did not fit afkd's 64 KiB plugin line"),
        "{stderr}"
    );
    plugin.finish();
}

// --- mr ---

/// The MR's source branch: non-ASCII, and crossing to the run verbatim.
const BRANCH: &str = "feature/重试-backoff";
/// The MR's title: the quotes a log line wraps it in, wide CJK and an emoji.
const MR_TITLE: &str = "Cap the retry backoff — \"重试\" 上限 🚦";
/// The human's review note on MR !7: multi-line, with an indented code line and a trailing
/// newline the brief trims.
const REVIEW: &str = "看起来不对 🚨 — the cap never applies:\n\n    max_backoff = 0\n";

/// A project with the bot's own MR !7 on [`BRANCH`], titled [`MR_TITLE`], a human assigned
/// to it, and the human's review note two minutes old — new feedback, since the bot has not
/// spoken. Returns the fake and the note's id.
fn mr_forge() -> (FakeGitlab, u64) {
    let fake = FakeGitlab::start(ME_ID, ME);
    fake.user(99, HUMAN);
    fake.mr(PROJECT, 7, ME, BRANCH, &[HUMAN]);
    fake.retitle(PROJECT, 7, MR_TITLE);
    let review = fake.mr_note(PROJECT, 7, HUMAN, REVIEW, 120);
    (fake, review)
}

/// A full `mr` block, lowered to JSON as afkd lowers it — `author_me` a `bool` as its word
/// — and afkd's own three keys along for the ride.
fn mr_settings(fake: &FakeGitlab) -> Value {
    json!({
        "base_url": fake.base_url(),
        "project": PROJECT,
        "token": TOKEN,
        "author_me": "true",
        "poll_interval": "30s",
        "max_attempts": "1",
        "follow_comments": "2m",
    })
}

/// The `finish` envelope's facts for an MR round, as afkd writes them.
fn mr_facts(signal: &str, reason: Option<&str>) -> Value {
    json!({"signal": signal, "reason": reason, "duration_ms": 168000, "cost": 0.4217,
           "turns": 12, "tokens": null, "run_name": "260925-095800-unit-7-1"})
}

/// The ids of the claim markers on an MR.
fn mr_markers(fake: &FakeGitlab, iid: u64) -> Vec<u64> {
    fake.mr_notes(PROJECT, iid)
        .into_iter()
        .filter(|n| n.body.starts_with("[afkd-claim]"))
        .map(|n| n.id)
        .collect()
}

/// Whether the fake saw a write — anything but a `GET`.
fn wrote(fake: &FakeGitlab) -> bool {
    fake.seen().iter().any(|r| r.method != "GET")
}

#[test]
fn mr_hello_lists_release_renew_comments() {
    let (fake, _) = mr_forge();
    let mut plugin = Plugin::spawn();
    assert_eq!(
        plugin.hello("mr", mr_settings(&fake)),
        json!({"ok": true, "proto": 2, "calls": ["release", "renew", "comments"]})
    );
    assert!(
        fake.seen().iter().all(|r| r.path == "/api/v4/user"),
        "hello reads only the token's user"
    );
    plugin.finish();
}

/// The won race over an MR with new human feedback hands over exactly the unit the
/// built-in would have run: its journal key and session thread, every field the
/// manifest's `Merge_Request` declares, the claim-time thread as `seen`, its identity, the
/// five env names the skill reads with the branch verbatim, and
/// the scratch layout with the review brief unframed. Every write went to the MR's own
/// paths. A second poll, the claim still live, hands over nothing: our own older marker
/// out-orders the new one, which is taken straight back.
#[test]
fn mr_a_won_race_hands_over_the_built_ins_unit() {
    let (fake, review) = mr_forge();
    let mut plugin = Plugin::armed_as("mr", mr_settings(&fake));
    let reply = plugin.poll();
    assert_eq!(reply["fire"], true, "{}", plugin.stderr());
    let marker = mr_markers(&fake, 7);
    assert_eq!(marker.len(), 1, "one marker, ours");
    assert_eq!(
        reply["unit"],
        json!({
            "id": "7",
            "key": format!("acme/sub.group/widgets#7#{}", marker[0]),
            "fields": {
                "title": MR_TITLE,
                "url": fake.mr_url(PROJECT, 7),
                "number": 7,
                "branch": BRANCH,
            },
            "thread": "acme/sub.group/widgets#7",
            "seen": [review.to_string()],
            "self": ME,
            "env": {
                "GITLAB_BASE_URL": fake.base_url(),
                "GITLAB_MR_BRANCH": BRANCH,
                "GITLAB_MR_NUMBER": "7",
                "GITLAB_PROJECT": PROJECT,
                "GITLAB_TOKEN": TOKEN,
            },
            "files": [
                {"path": "task.md", "text": "Address review feedback on MR !7.\n\n\
                    ## New feedback\n\n**陳大文:** 看起来不对 🚨 — the cap never applies:\n\n    \
                    max_backoff = 0\n"},
                {"path": "mr/number", "text": "7"},
            ],
        })
    );

    let mr = fake.mr_state(PROJECT, 7);
    assert_eq!(mr.labels, ["afkd::claimed"]);
    assert_eq!(mr.assignees, [HUMAN], "the claim assigns no one");
    on_claim(&mut plugin, &reply["unit"], "afkd::reviewing");
    let mr = fake.mr_state(PROJECT, 7);
    assert_eq!(mr.labels, ["afkd::claimed", "afkd::reviewing"]);
    assert_eq!(mr.assignees, [HUMAN, ME], "the human was kept");
    let seen = fake.seen();
    let list = seen
        .iter()
        .find(|r| r.path == format!("/api/v4/projects/{ENCODED}/merge_requests"))
        .expect("the MRs were listed");
    assert_eq!(list.query, [("state".to_string(), "opened".to_string())]);
    let mr_path = format!("/api/v4/projects/{ENCODED}/merge_requests/7");
    assert!(
        seen.iter()
            .filter(|r| r.method != "GET")
            .all(|r| r.path.starts_with(&mr_path)),
        "a write left the MR's paths: {seen:?}"
    );
    assert!(seen.iter().all(|r| !r.path.contains("/issues")), "{seen:?}");

    assert_eq!(plugin.poll(), json!({"fire": false}), "the claim is live");
    assert_eq!(
        mr_markers(&fake, 7),
        marker,
        "the second marker was taken back"
    );
    plugin.finish();
}

/// A live rival marker a minute older out-orders ours: nothing is handed over, our marker
/// is taken back, and no status is written.
#[test]
fn mr_a_lost_race_hands_over_nothing_and_takes_its_marker_back() {
    let (fake, _) = mr_forge();
    let rival = fake.mr_note(PROJECT, 7, "autocoder", "[afkd-claim] owner=autocoder", 60);
    let mut plugin = Plugin::armed_as("mr", mr_settings(&fake));
    assert_eq!(plugin.poll(), json!({"fire": false}));
    assert_eq!(
        mr_markers(&fake, 7),
        [rival],
        "only the rival's marker is left"
    );
    let mr = fake.mr_state(PROJECT, 7);
    assert!(mr.labels.is_empty(), "{:?}", mr.labels);
    assert_eq!(mr.assignees, [HUMAN]);
    assert!(
        fake.seen().iter().all(|r| r.method != "PUT"),
        "no status write: {:?}",
        fake.seen()
    );
    plugin.finish();
}

/// The bot has answered the human's note, and nothing newer has arrived: the MR is idle,
/// and the poll posts nothing at all.
#[test]
fn mr_with_no_new_feedback_does_not_fire() {
    let (fake, _) = mr_forge();
    fake.mr_note(PROJECT, 7, ME, "Pushed 3f2a1c: the cap applies now.", 30);
    let mut plugin = Plugin::armed_as("mr", mr_settings(&fake));
    assert_eq!(plugin.poll(), json!({"fire": false}));
    assert!(
        !wrote(&fake),
        "an idle MR is never claimed: {:?}",
        fake.seen()
    );
    plugin.finish();
}

/// The only note newer than the bot's reply is a rival's claim marker — a stale one, over
/// an hour old, so it could never win a race. Nothing is posted, which proves the MR was
/// refused as having no new feedback rather than lost to the marker.
#[test]
fn mr_whose_only_new_note_is_a_claim_marker_does_not_fire() {
    let fake = FakeGitlab::start(ME_ID, ME);
    fake.mr(PROJECT, 7, ME, BRANCH, &[]);
    fake.mr_note(PROJECT, 7, HUMAN, REVIEW, 7_400);
    fake.mr_note(PROJECT, 7, ME, "Pushed 3f2a1c: the cap applies now.", 7_300);
    fake.mr_note(
        PROJECT,
        7,
        "autocoder",
        "[afkd-claim] owner=autocoder",
        3_700,
    );
    let mut plugin = Plugin::armed_as("mr", mr_settings(&fake));
    assert_eq!(plugin.poll(), json!({"fire": false}));
    assert!(!wrote(&fake), "a marker is not feedback: {:?}", fake.seen());
    plugin.finish();
}

/// Under `author_me` a human's MR is passed over, feedback and all, and nothing is
/// posted; a service armed without the flag, on the same forge, claims it.
#[test]
fn mr_author_me_filters_foreign_mrs() {
    let fake = FakeGitlab::start(ME_ID, ME);
    fake.mr(PROJECT, 8, HUMAN, "fix/y", &[]);
    fake.mr_note(PROJECT, 8, "álvaro", "Exponential, please — see §4 🙏", 60);

    let mut mine = Plugin::armed_as("mr", mr_settings(&fake));
    assert_eq!(mine.poll(), json!({"fire": false}));
    assert!(!wrote(&fake), "{:?}", fake.seen());
    mine.finish();

    let mut settings = mr_settings(&fake);
    settings.as_object_mut().unwrap().remove("author_me");
    let mut anyone = Plugin::armed_as("mr", settings);
    let unit = anyone.poll()["unit"].clone();
    assert_eq!(unit["id"], "8");
    assert_eq!(unit["env"]["GITLAB_MR_BRANCH"], "fix/y");
    anyone.finish();
}

#[test]
fn mr_renew_rewrites_the_marker_in_place() {
    let (fake, _) = mr_forge();
    let mut plugin = Plugin::armed_as("mr", mr_settings(&fake));
    let unit = plugin.poll()["unit"].clone();
    let marker = mr_markers(&fake, 7)[0];
    let note = |fake: &FakeGitlab| {
        fake.mr_notes(PROJECT, 7)
            .into_iter()
            .find(|n| n.id == marker)
            .expect("the marker")
    };
    let before = note(&fake);
    fake.advance(300);
    assert_eq!(
        plugin.call(json!({"call": "renew", "key": unit["key"], "renewal": 1})),
        json!({"ok": true})
    );
    let after = note(&fake);
    assert_eq!(after.body, "[afkd-claim] owner=björn-öst[bot] renewal=1");
    assert_eq!(after.created, before.created);
    assert_eq!(
        after.updated,
        before.updated + 300,
        "the liveness stamp moved"
    );
    let put = fake
        .seen()
        .into_iter()
        .rfind(|r| r.method == "PUT")
        .unwrap();
    assert_eq!(
        put.path,
        format!("/api/v4/projects/{ENCODED}/merge_requests/7/notes/{marker}")
    );
    plugin.finish();
}

/// `comments` reports what afkd has not been told about: the claim-time review note
/// (`seen`), the marker and the bot's own reply are left out; a second read carries only
/// what is new; and a forge that cannot be read is `null`.
#[test]
fn mr_comments_report_what_afkd_has_not_seen() {
    let (fake, _) = mr_forge();
    let mut plugin = Plugin::armed_as("mr", mr_settings(&fake));
    let unit = plugin.poll()["unit"].clone();
    let read = |plugin: &mut Plugin| plugin.call(json!({"call": "comments", "key": unit["key"]}));

    fake.advance(60);
    let a = fake.mr_note(PROJECT, 7, HUMAN, "还有 — the jitter too.\n", 0);
    fake.advance(1);
    fake.mr_note(PROJECT, 7, ME, "On it.", 0);
    fake.advance(1);
    let b = fake.mr_note(PROJECT, 7, "álvaro", "Exponential, please — see §4 🙏", 0);
    assert_eq!(
        read(&mut plugin),
        json!({"comments": [
            {"id": a.to_string(), "author": HUMAN, "author_name": HUMAN,
             "body": "还有 — the jitter too.\n", "at": "2026-09-25T09:59:00Z"},
            {"id": b.to_string(), "author": "álvaro", "author_name": "álvaro",
             "body": "Exponential, please — see §4 🙏", "at": "2026-09-25T09:59:02Z"},
        ]})
    );

    fake.advance(30);
    let c = fake.mr_note(PROJECT, 7, "álvaro", "…and cap it at 30s.", 0);
    let reply = read(&mut plugin);
    assert_eq!(reply["comments"].as_array().unwrap().len(), 1);
    assert_eq!(reply["comments"][0]["id"], c.to_string());

    fake.fail("list notes", 500);
    assert_eq!(read(&mut plugin), json!({"comments": null}));
    assert!(
        plugin
            .stderr_soon("list notes")
            .contains("afkd-gitlab: gitlab list notes: forge returned status 500"),
        "{}",
        plugin.stderr()
    );
    plugin.finish();
}

/// `release` undoes a claim in full — the status label, the bot's assignment (the human's
/// is kept) and the marker — whether afkd hands a live unit straight back or its reaper
/// sends a crashed run's key to a fresh child, before any poll. A pre-marker key names
/// nothing (`null`).
#[test]
fn mr_release_undoes_the_claim_in_full() {
    let (fake, _) = mr_forge();
    let mut first = Plugin::armed_as("mr", mr_settings(&fake));
    let unit = first.poll()["unit"].clone();
    on_claim(&mut first, &unit, "afkd::reviewing");
    let live = unit["key"].clone();
    assert_eq!(
        first.call(json!({"call": "release", "key": live})),
        json!({"released": true})
    );
    let mr = fake.mr_state(PROJECT, 7);
    assert_eq!(mr.labels, ["afkd::reviewing"], "only the status label goes");
    assert_eq!(mr.assignees, [HUMAN], "only afkd let go");
    assert!(mr_markers(&fake, 7).is_empty());

    // Claimed again, then the child dies mid-run.
    let unit = first.poll()["unit"].clone();
    on_claim(&mut first, &unit, "afkd::reviewing");
    let crashed = unit["key"].clone();
    assert_eq!(fake.mr_state(PROJECT, 7).assignees, [HUMAN, ME]);
    drop(first);

    let from = fake.seen().len();
    let mut fresh = Plugin::armed_as("mr", mr_settings(&fake));
    assert_eq!(
        fake.seen()[from].path,
        "/api/v4/user",
        "hello read the identity"
    );
    assert_eq!(
        fresh.call(json!({"call": "release", "key": crashed})),
        json!({"released": true})
    );
    let mr = fake.mr_state(PROJECT, 7);
    assert!(!mr.labels.contains(&"afkd::claimed".to_string()));
    assert_eq!(mr.assignees, [HUMAN]);
    assert!(mr_markers(&fake, 7).is_empty());

    assert_eq!(
        fresh.call(json!({"call": "release", "key": "acme/sub.group/widgets#7"})),
        json!({"released": null})
    );
    fresh.finish();
}

/// A clean round drops the marker, then `on_done`'s call takes the working label off, and
/// nothing closes the MR — a human's merge ends the loop.
#[test]
fn mr_a_clean_finish_then_on_done_leaves_the_mr_open() {
    let (fake, _) = mr_forge();
    let mut plugin = Plugin::armed_as("mr", mr_settings(&fake));
    let unit = plugin.poll()["unit"].clone();
    on_claim(&mut plugin, &unit, "afkd::reviewing");
    assert_eq!(
        finish(&mut plugin, &unit, "clean", mr_facts("proceed", None)),
        json!({"ok": true})
    );
    hook(
        &mut plugin,
        &unit,
        &[("label_remove", json!({"label": "afkd::reviewing"}))],
    );
    let mr = fake.mr_state(PROJECT, 7);
    assert_eq!(mr.labels, ["afkd::claimed"]);
    assert_eq!(mr.state, "opened");
    assert!(mr_markers(&fake, 7).is_empty());
    assert!(
        fake.seen().iter().all(|r| !r.body.contains("state_event")),
        "{:?}",
        fake.seen()
    );
    plugin.finish();
}

/// A failed round, then `on_fail`'s calls: the working label goes, only the bot is
/// unassigned, and the comment — afkd interpolated its run facts — is posted verbatim.
#[test]
fn mr_a_failed_finish_then_on_fail_lets_go() {
    let (fake, review) = mr_forge();
    let mut plugin = Plugin::armed_as("mr", mr_settings(&fake));
    let unit = plugin.poll()["unit"].clone();
    on_claim(&mut plugin, &unit, "afkd::reviewing");
    let reply = finish(
        &mut plugin,
        &unit,
        "failed",
        mr_facts("fault", Some("cargo test: 3 failed")),
    );
    assert_eq!(reply, json!({"ok": true}));
    let stopped = "Stopped after 2m 48s for $0.42 — log: .afkd/runs/260925-095800-unit-7-1/run.log";
    hook(
        &mut plugin,
        &unit,
        &[
            ("label_remove", json!({"label": "afkd::reviewing"})),
            ("unassign", json!({})),
            ("comment", json!({ "text": stopped })),
        ],
    );
    let mr = fake.mr_state(PROJECT, 7);
    assert_eq!(mr.labels, ["afkd::claimed"]);
    assert_eq!(mr.assignees, [HUMAN], "only afkd let go");
    let said: Vec<(u64, String)> = fake
        .mr_notes(PROJECT, 7)
        .into_iter()
        .map(|n| (n.id, n.body))
        .collect();
    assert_eq!(said[0], (review, REVIEW.to_string()));
    assert_eq!(said[1].1, stopped);
    assert_eq!(said.len(), 2, "the marker went: {said:?}");
    plugin.finish();
}

/// An `on_fail` action the forge refuses — which no retry changes — is that `call`'s
/// `ok:false`, with the forge's sentence: afkd fails the hook there, and the claim stays
/// until afkd's `release` of the key undoes it. The comment `on_fail` posted before is the
/// bot's last word, so the MR waits for the human — and once they reply, it is claimed
/// afresh.
#[test]
fn mr_a_refused_on_fail_action_is_the_calls_error_and_release_recovers_the_claim() {
    let (fake, _) = mr_forge();
    let mut plugin = Plugin::armed_as("mr", mr_settings(&fake));
    let unit = plugin.poll()["unit"].clone();
    on_claim(&mut plugin, &unit, "afkd::reviewing");
    let key = unit["key"].as_str().unwrap().to_string();
    assert_eq!(
        finish(&mut plugin, &unit, "failed", mr_facts("fault", None)),
        json!({"ok": true})
    );
    hook(
        &mut plugin,
        &unit,
        &[(
            "comment",
            json!({"text": "Stopped: CI is red — over to you."}),
        )],
    );
    fake.fail("set assignees", 403);
    assert_eq!(
        plugin.act("mr_unassign", json!({}), &unit),
        json!({"ok": false, "error": "gitlab set assignees: forge returned status 403"})
    );
    assert!(fake
        .mr_state(PROJECT, 7)
        .labels
        .contains(&"afkd::claimed".to_string()));
    assert_eq!(
        fake.mr_state(PROJECT, 7).assignees,
        [HUMAN, ME],
        "the claim stays"
    );

    fake.heal("set assignees");
    assert_eq!(
        plugin.call(json!({"call": "release", "key": key})),
        json!({"released": true})
    );
    let mr = fake.mr_state(PROJECT, 7);
    assert!(!mr.labels.contains(&"afkd::claimed".to_string()));
    assert_eq!(mr.assignees, [HUMAN]);
    assert_eq!(plugin.poll(), json!({"fire": false}), "the bot spoke last");

    fake.advance(60);
    fake.mr_note(
        PROJECT,
        7,
        HUMAN,
        "Still failing on CI — see the job log.",
        0,
    );
    let again = plugin.poll()["unit"].clone();
    assert_eq!(again["id"], "7", "claimed afresh");
    assert_ne!(again["key"], unit["key"]);
    plugin.finish();
}

// --- a call names its item ---

/// A call acts on the item its handle names, never on a claim it implies: with issue #7
/// finished and #8 claimed and live, #7's `on_done` — sent after `finish`, as afkd sends
/// it — comments on and closes the finished issue over the real wire and leaves the live
/// one open and silent. On a merge-request service the same holds under its own names: an
/// `mr_comment` after `finish` lands on the merge request its handle names.
#[test]
fn a_post_run_call_acts_on_the_finished_item_its_handle_names() {
    let fake = forge();
    fake.issue(
        PROJECT,
        8,
        "第二の課題 — the next one 🚧",
        "And this.",
        &["afkd::ready"],
        &[],
    );
    let mut plugin = Plugin::armed(settings(&fake));
    let first = plugin.poll()["unit"].clone();
    on_claim(&mut plugin, &first, "afkd::working");
    assert_eq!(
        finish(&mut plugin, &first, "clean", facts("proceed", None)),
        json!({"ok": true})
    );
    let second = plugin.poll()["unit"].clone();
    assert_eq!(second["id"], "8", "{second}");

    let landed = "afkd landed this issue in 2m48s.\n\n— 完了 ✅";
    hook(
        &mut plugin,
        &first,
        &[("comment", json!({ "text": landed })), ("close", json!({}))],
    );
    let said = |iid: u64| -> Vec<String> {
        fake.notes(PROJECT, iid)
            .into_iter()
            .filter(|n| n.body == landed)
            .map(|n| n.body)
            .collect()
    };
    assert_eq!(said(7), [landed]);
    assert_eq!(fake.issue_state(PROJECT, 7).state, "closed");
    assert!(said(8).is_empty(), "the live issue got nothing");
    assert_eq!(fake.issue_state(PROJECT, 8).state, "opened");
    plugin.finish();

    let (fake, _) = mr_forge();
    let mut plugin = Plugin::armed_as("mr", mr_settings(&fake));
    let unit = plugin.poll()["unit"].clone();
    on_claim(&mut plugin, &unit, "afkd::reviewing");
    assert_eq!(
        finish(&mut plugin, &unit, "clean", mr_facts("proceed", None)),
        json!({"ok": true})
    );
    let round = "Round done ✅ — see 4f2a9c1.";
    assert_eq!(
        plugin.act("mr_comment", json!({ "text": round }), &unit),
        json!({"ok": true})
    );
    assert!(fake
        .mr_notes(PROJECT, 7)
        .iter()
        .any(|n| n.author == ME && n.body == round));
    plugin.finish();
}

// --- GitLab out of reach: the outbox ---

/// A label an `on_fail` adds: scoped, with an emoji.
const NEEDS_HUMAN: &str = "needs::human 🚧";

/// What an `on_fail` comments: wide CJK, an em dash, and a fenced block on later lines.
const FAILED_NOTE: &str = "attempts spent — 修复 failed twice\n\n```\nexit 128\n```";

/// The issue as a log line names it.
fn issue_named() -> String {
    format!("issue \"{TITLE}\" ({PROJECT}#7)")
}

/// Whether `stderr` has the line for a write queued because its first try reached no
/// forge: the stage, then whatever the transport said, then what was queued.
fn queued(stderr: &str, item: &str, stage: &str, what: &str) -> bool {
    let (head, tail) = (
        format!("afkd-gitlab: gitlab is unreachable for {item}: gitlab {stage}: no response ("),
        format!("; {what} is queued and retried for up to 1h"),
    );
    stderr
        .lines()
        .any(|line| line.starts_with(&head) && line.ends_with(&tail))
}

/// Each call of an `on_fail` slot during an outage answers `ok` at once, so the slot runs
/// on; the first is queued with what went wrong and the rest behind it, one line each.
/// Once GitLab is back the worker lands all three, in the order they were called, and
/// says so — the unassign re-reading the set, so the human stays.
#[test]
fn a_call_during_an_outage_answers_ok_and_lands_when_gitlab_returns() {
    let fake = forge();
    let mut plugin = Plugin::armed(settings(&fake));
    let unit = plugin.poll()["unit"].clone();
    on_claim(&mut plugin, &unit, "afkd::working");

    fake.outage();
    for (action, args) in [
        ("label_add", json!({"label": NEEDS_HUMAN})),
        ("comment", json!({"text": FAILED_NOTE})),
        ("unassign", json!({})),
    ] {
        assert_eq!(
            plugin.act(action, args, &unit),
            json!({"ok": true}),
            "{action}: {}",
            plugin.stderr()
        );
    }
    let item = issue_named();
    let stderr = plugin.stderr_soon("unassigning the bot is queued behind them");
    assert!(
        queued(
            &stderr,
            &item,
            "add label",
            &format!("adding the label \"{NEEDS_HUMAN}\"")
        ),
        "{stderr}"
    );
    for what in [
        "the comment \"attempts spent — 修复 failed twice…\"".to_string(),
        "unassigning the bot".to_string(),
    ] {
        let line = format!(
            "afkd-gitlab: gitlab still owes {item} earlier writes; {what} is queued behind them"
        );
        assert!(stderr.contains(&line), "{line}\n{stderr}");
    }
    assert!(!fake.dropped().is_empty(), "the first write did go out");
    assert_eq!(assignees(&fake, 7), [HUMAN, ME], "nothing landed");

    let restored = fake.seen().len();
    fake.restore();
    soon("the backlog landed", &plugin, || {
        plugin.stderr().contains("unassigning the bot landed after")
    });
    let issue_path = format!("/api/v4/projects/{ENCODED}/issues/7");
    assert_eq!(
        writes_since(&fake, restored),
        [
            format!("PUT {issue_path} add_labels"),
            format!("POST {issue_path}/notes"),
            format!("PUT {issue_path} assignee_ids"),
        ]
    );
    let issue = fake.issue_state(PROJECT, 7);
    assert!(issue.labels.contains(&NEEDS_HUMAN.to_string()));
    assert_eq!(issue.assignees, [HUMAN], "only afkd let go");
    let said: Vec<String> = fake.notes(PROJECT, 7).into_iter().map(|n| n.body).collect();
    assert!(said.contains(&FAILED_NOTE.to_string()), "{said:?}");
    let stderr = plugin.finish();
    assert!(
        stderr.contains(&format!(
            "afkd-gitlab: gitlab is back for {item}: adding the label \"{NEEDS_HUMAN}\" landed \
             after "
        )),
        "{stderr}"
    );
}

/// The run-end release through an outage: the claim marker's delete is queued at
/// `finish`, and the `on_fail` slot's calls after it queue behind it, so once GitLab is
/// back the marker goes first and the slot's writes follow in order.
#[test]
fn a_finish_during_an_outage_releases_the_marker_before_the_post_run_slot() {
    let fake = forge();
    let mut plugin = Plugin::armed(settings(&fake));
    let unit = plugin.poll()["unit"].clone();
    on_claim(&mut plugin, &unit, "afkd::working");
    let marker = markers(&fake, 7)[0];

    fake.outage();
    let reply = finish(
        &mut plugin,
        &unit,
        "failed",
        facts("fault", Some("cargo test: 3 failed")),
    );
    assert_eq!(reply, json!({"ok": true}));
    for (action, args) in [
        ("label_remove", json!({"label": "afkd::working"})),
        ("unassign", json!({})),
    ] {
        assert_eq!(plugin.act(action, args, &unit), json!({"ok": true}));
    }
    let stderr = plugin.stderr_soon("unassigning the bot is queued behind them");
    assert!(
        queued(
            &stderr,
            &issue_named(),
            "delete comment",
            "deleting the claim marker"
        ),
        "{stderr}"
    );

    let restored = fake.seen().len();
    fake.restore();
    soon("the on_fail state", &plugin, || {
        plugin.stderr().contains("unassigning the bot landed after")
    });
    let issue_path = format!("/api/v4/projects/{ENCODED}/issues/7");
    assert_eq!(
        writes_since(&fake, restored),
        [
            format!("DELETE {issue_path}/notes/{marker}"),
            format!("PUT {issue_path} remove_labels"),
            format!("PUT {issue_path} assignee_ids"),
        ]
    );
    let issue = fake.issue_state(PROJECT, 7);
    assert_eq!(issue.assignees, [HUMAN]);
    assert!(!issue.labels.contains(&"afkd::working".to_string()));
    assert!(markers(&fake, 7).is_empty());
    plugin.finish();
}

/// A 429 or a 5xx is the forge saying "not now": queued, `ok`, and landed once the forge
/// takes it. Any other 4xx is its answer about the write, which no retry changes: the
/// call's sentence, and nothing queued.
#[test]
fn a_429_or_5xx_is_queued_and_another_4xx_answers_its_sentence() {
    let fake = forge();
    let mut plugin = Plugin::armed(settings(&fake));
    let unit = plugin.poll()["unit"].clone();

    for (status, label) in [(429, NEEDS_HUMAN), (503, "afkd::working")] {
        fake.fail("add label", status);
        assert_eq!(
            plugin.act("label_add", json!({"label": label}), &unit),
            json!({"ok": true}),
            "{status}"
        );
        let line = format!(
            "afkd-gitlab: gitlab is unreachable for {}: gitlab add label: forge returned \
             status {status}; adding the label \"{label}\" is queued and retried for up to 1h",
            issue_named()
        );
        let stderr = plugin.stderr_soon(&line);
        assert!(stderr.contains(&line), "{stderr}");
        fake.heal("add label");
        let landed = format!("adding the label \"{label}\" landed after");
        soon("the label landed", &plugin, || {
            plugin.stderr().contains(&landed)
        });
        assert!(labels(&fake, 7).contains(&label.to_string()));
    }

    fake.fail("add label", 403);
    assert_eq!(
        plugin.act("label_add", json!({"label": "afkd::reviewed ✅"}), &unit),
        json!({"ok": false, "error": "gitlab add label: forge returned status 403"})
    );
    fake.heal("add label");
    let stderr = plugin.finish();
    assert!(
        !stderr.contains("still owed"),
        "nothing was queued: {stderr}"
    );
}

/// A write queued through an outage that the forge, once back, refuses is given up at
/// once with one loud line naming the issue and what is lost — and the item's next write
/// still lands.
#[test]
fn a_queued_write_the_forge_then_refuses_is_given_up_loudly() {
    let fake = forge();
    let mut plugin = Plugin::armed(settings(&fake));
    let unit = plugin.poll()["unit"].clone();

    fake.outage();
    for (action, args) in [
        ("label_add", json!({"label": NEEDS_HUMAN})),
        ("comment", json!({"text": FAILED_NOTE})),
    ] {
        assert_eq!(plugin.act(action, args, &unit), json!({"ok": true}));
    }
    plugin.stderr_soon("is queued behind them");
    fake.fail("add label", 403);
    fake.restore();
    let lost = format!(
        "afkd-gitlab: gave up on {} after 5s: adding the label \"{NEEDS_HUMAN}\" is lost — \
         gitlab add label: forge returned status 403",
        issue_named()
    );
    soon("the refused label given up", &plugin, || {
        plugin.stderr().contains(&lost)
    });
    soon("the comment behind it landed", &plugin, || {
        fake.notes(PROJECT, 7).iter().any(|n| n.body == FAILED_NOTE)
    });
    assert!(!labels(&fake, 7).contains(&NEEDS_HUMAN.to_string()));
    fake.heal("add label");
    plugin.finish();
}

/// The MR kind's round end through an outage: the marker's delete and the `on_done`'s
/// calls are queued under the merge request's own name, and land in order once GitLab is
/// back.
#[test]
fn an_mr_round_end_during_an_outage_lands_when_gitlab_returns() {
    let (fake, review) = mr_forge();
    let mut plugin = Plugin::armed_as("mr", mr_settings(&fake));
    let unit = plugin.poll()["unit"].clone();
    on_claim(&mut plugin, &unit, "afkd::reviewing");
    let marker = mr_markers(&fake, 7)[0];

    fake.outage();
    assert_eq!(
        finish(&mut plugin, &unit, "clean", mr_facts("proceed", None)),
        json!({"ok": true})
    );
    let done = "Round done ✅ — 重试 now caps at 30s.\n\nSee the run log.";
    for (action, args) in [
        ("mr_comment", json!({"text": done})),
        ("mr_label_remove", json!({"label": "afkd::reviewing"})),
    ] {
        assert_eq!(plugin.act(action, args, &unit), json!({"ok": true}));
    }
    let mr = format!("merge request \"{MR_TITLE}\" ({PROJECT}#7)");
    let stderr = plugin.stderr_soon("removing the label \"afkd::reviewing\" is queued behind");
    assert!(
        queued(&stderr, &mr, "delete comment", "deleting the claim marker"),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!(
            "afkd-gitlab: gitlab still owes {mr} earlier writes; the comment \"Round done ✅ — \
             重试 now caps at 30s.…\" is queued behind them"
        )),
        "{stderr}"
    );

    let restored = fake.seen().len();
    fake.restore();
    soon("the round end landed", &plugin, || {
        plugin
            .stderr()
            .contains("removing the label \"afkd::reviewing\" landed after")
    });
    let mr_path = format!("/api/v4/projects/{ENCODED}/merge_requests/7");
    assert_eq!(
        writes_since(&fake, restored),
        [
            format!("DELETE {mr_path}/notes/{marker}"),
            format!("POST {mr_path}/notes"),
            format!("PUT {mr_path} remove_labels"),
        ]
    );
    assert!(mr_markers(&fake, 7).is_empty());
    let said: Vec<(u64, String)> = fake
        .mr_notes(PROJECT, 7)
        .into_iter()
        .map(|n| (n.id, n.body))
        .collect();
    assert_eq!(said[0], (review, REVIEW.to_string()));
    assert_eq!(said[1].1, done);
    assert_eq!(fake.mr_state(PROJECT, 7).labels, ["afkd::claimed"]);
    plugin.finish();
}

/// afkd closing stdin while writes are still owed: the plugin exits cleanly, naming each
/// on its way out.
#[test]
fn closing_stdin_with_writes_owed_names_them() {
    let fake = forge();
    let mut plugin = Plugin::armed(settings(&fake));
    let unit = plugin.poll()["unit"].clone();
    on_claim(&mut plugin, &unit, "afkd::working");
    fake.outage();
    assert_eq!(
        finish(&mut plugin, &unit, "clean", facts("proceed", None)),
        json!({"ok": true})
    );
    assert_eq!(plugin.act("close", json!({}), &unit), json!({"ok": true}));
    let stderr = plugin.finish();
    let item = issue_named();
    for what in ["deleting the claim marker", "closing it"] {
        let line = format!("afkd-gitlab: afkd ended the plugin with {what} for {item} still owed");
        assert!(stderr.contains(&line), "{line}\n{stderr}");
    }
}
