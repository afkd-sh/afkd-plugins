//! The wire contract suite: the real `afkd-gitlab` exec, spoken to exactly as afkd speaks
//! to it (`docs/plugins.md`, "The trigger protocol"), against a stateful fake GitLab
//! reached through the kind's own `base_url`. No afkd is involved.
//!
//! Every leg ends by closing stdin and holding the child to the wire's discipline — a
//! clean exit, one reply line per request, nothing else on stdout
//! ([`Plugin::finish`](common::Plugin::finish)) — so a diagnostic that strayed onto stdout
//! fails whichever leg wrote it.

mod common;

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

/// A full `gitlab` block, lowered to JSON as afkd lowers it: repeatable keys as arrays,
/// flags as `true`, blocks as objects — and afkd's own three keys along for the ride.
fn settings(fake: &FakeGitlab) -> Value {
    json!({
        "base_url": fake.base_url(),
        "project": PROJECT,
        "token": TOKEN,
        "source_label": "afkd::ready",
        "poll_interval": "30s",
        "max_attempts": "1",
        "follow_comments": "2m",
        "on_claim": {"assign_me": [true], "label_add": ["afkd::working"]},
        "on_done": {"label_remove": ["afkd::working"], "close": [true]},
        "on_fail": {"label_remove": ["afkd::working"], "unassign": [true], "comment": [
            "Stopped after @{run:duration} for @{run:cost} — log: .afkd/runs/@{run:name}/run.log"
        ]},
    })
}

/// The `finish` envelope's facts, as afkd writes them.
fn facts(signal: &str, reason: Option<&str>) -> Value {
    json!({"signal": signal, "reason": reason, "duration_ms": 168000, "cost": 0.4217,
           "turns": null, "tokens": null, "run_name": "260925-095800-issue-7-1"})
}

fn finish(plugin: &mut Plugin, unit: &Value, outcome: &str, facts: Value) -> Value {
    plugin.call(
        json!({"call": "finish", "id": unit["id"], "key": unit["key"],
                       "outcome": outcome, "facts": facts}),
    )
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

#[test]
fn hello_arms_and_lists_every_call_it_answers() {
    let fake = forge();
    let mut plugin = Plugin::spawn();
    assert_eq!(
        plugin.hello("gitlab", settings(&fake)),
        json!({"ok": true, "proto": 1, "calls": ["release", "renew", "comments"]})
    );
    assert!(fake.seen().is_empty(), "hello touches no forge");
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
    let mut claim_cost = settings(&fake);
    claim_cost["on_claim"] = json!({"comment": ["claimed; budget @{run:cost}"]});
    for (kind, proto, settings, sentence) in [
        (
            "gitlab_mr_review",
            1,
            settings(&fake),
            "kind `gitlab_mr_review` is not provided by @afkd/gitlab",
        ),
        (
            "gitea",
            1,
            settings(&fake),
            "kind `gitea` is not provided by @afkd/gitlab",
        ),
        (
            "gitlab",
            1,
            no_project,
            "trigger gitlab: setting `project`: a gitlab trigger needs a `project` (numeric id \
             or path-with-namespace)",
        ),
        (
            "gitlab",
            1,
            claim_cost,
            "trigger gitlab: setting `comment`: `@{run:cost}` references the run's facts, but no \
             run happens at claim time",
        ),
        (
            "gitlab",
            2,
            settings(&fake),
            "afkd speaks plugin protocol 2, and this plugin speaks 1",
        ),
    ] {
        let mut plugin = Plugin::spawn();
        let reply = plugin
            .call(json!({"call": "hello", "proto": proto, "kind": kind, "settings": settings}));
        assert_eq!(reply, json!({"ok": false, "proto": 1}), "{kind} {proto}");
        let stderr = plugin.stderr_soon(sentence);
        assert_eq!(stderr, format!("afkd-gitlab: {sentence}\n"));
        plugin.finish();
    }
    assert!(fake.seen().is_empty(), "a refused hello touches no forge");
}

// --- poll ---

/// The won race hands over exactly the unit the built-in would have run: its journal key
/// and session thread, no `seen`, its identity, the four env names the skill reads, and
/// the scratch layout with the brief unframed. The forge then holds the claim as the
/// built-in leaves it.
#[test]
fn a_won_race_hands_over_the_built_ins_unit() {
    let fake = forge();
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
        ["afkd::ready", "afkd::claimed", "afkd::working"]
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

/// afkd's reaper runs before its poll, so a freshly spawned child's first call can be
/// `release` of a key a crashed run left. The plugin resolves its identity there, and the
/// release is the built-in's whole one: the status label, the bot's assignment (the
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

/// With `/user` down, `release` writes nothing and keeps the key (`false`), so afkd asks
/// again; healed, the same key releases in full.
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
        plugin.stderr_soon("current user"),
        "afkd-gitlab: gitlab current user: forge returned status 500\n"
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

#[test]
fn a_clean_finish_runs_on_done_and_drops_the_marker() {
    let fake = forge();
    let mut plugin = Plugin::armed(settings(&fake));
    let unit = plugin.poll()["unit"].clone();
    assert_eq!(
        finish(&mut plugin, &unit, "clean", facts("proceed", None)),
        json!({"ok": true})
    );
    let issue = fake.issue_state(PROJECT, 7);
    assert_eq!(issue.state, "closed");
    assert_eq!(issue.labels, ["afkd::ready", "afkd::claimed"]);
    assert!(markers(&fake, 7).is_empty());
    assert!(fake
        .seen()
        .iter()
        .any(|r| r.method == "PUT" && r.body == r#"{"state_event":"close"}"#));
    plugin.finish();
}

#[test]
fn a_failed_finish_runs_on_fail_and_keeps_a_human_assignee() {
    let fake = forge();
    let mut plugin = Plugin::armed(settings(&fake));
    let unit = plugin.poll()["unit"].clone();
    assert_eq!(assignees(&fake, 7), [HUMAN, ME]);
    let reply = finish(
        &mut plugin,
        &unit,
        "failed",
        facts("fault", Some("cargo test: 3 failed")),
    );
    assert_eq!(reply, json!({"ok": true}));
    let issue = fake.issue_state(PROJECT, 7);
    assert_eq!(issue.assignees, [HUMAN], "only afkd let go");
    assert_eq!(issue.state, "opened");
    assert!(!issue.labels.contains(&"afkd::working".to_string()));
    assert!(markers(&fake, 7).is_empty());
    plugin.finish();
}

/// GitLab has no `on_park`: a park runs `on_fail`, with the run's facts substituted —
/// 168000 ms is `2m48s`, 0.4217 is `$0.42`, and the run name is afkd's.
#[test]
fn a_park_finish_runs_on_fail_and_substitutes_the_run_facts() {
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
    let said: Vec<String> = fake.notes(PROJECT, 7).into_iter().map(|n| n.body).collect();
    assert_eq!(
        said,
        ["Stopped after 2m48s for $0.42 — log: .afkd/runs/260925-095800-issue-7-1/run.log"]
    );
    plugin.finish();
}

/// A terminal lifecycle that does not land is `held`: the marker still goes, the claim
/// stays (so the next poll passes the issue over), and afkd's later `release` of the key
/// undoes it — its own marker delete answered `404`, diagnosed, not fatal — after which
/// the issue is claimed afresh.
#[test]
fn an_undelivered_finish_is_held_and_release_recovers_it() {
    let fake = forge();
    let mut plugin = Plugin::armed(settings(&fake));
    let unit = plugin.poll()["unit"].clone();
    let key = unit["key"].as_str().unwrap().to_string();
    fake.fail("set state", 500);

    assert_eq!(
        finish(&mut plugin, &unit, "clean", facts("proceed", None)),
        json!({"ok": true, "held": true})
    );
    let stderr = plugin.stderr_soon("afkd holds the claim");
    assert_eq!(
        stderr,
        format!(
            "afkd-gitlab: gitlab set state: forge returned status 500\n\
             afkd-gitlab: could not deliver the terminal lifecycle for {key}; afkd holds the \
             claim and releases it on a later beat\n"
        )
    );
    assert!(markers(&fake, 7).is_empty(), "the marker goes regardless");
    assert!(labels(&fake, 7).contains(&"afkd::claimed".to_string()));
    assert_eq!(plugin.poll(), json!({"fire": false}), "still claimed");

    fake.heal("set state");
    assert_eq!(
        plugin.call(json!({"call": "release", "key": key})),
        json!({"released": true})
    );
    assert!(!labels(&fake, 7).contains(&"afkd::claimed".to_string()));
    assert_eq!(assignees(&fake, 7), [HUMAN]);
    assert!(
        plugin
            .stderr_soon("delete comment")
            .ends_with("afkd-gitlab: gitlab delete comment: forge returned status 404\n"),
        "{}",
        plugin.stderr()
    );
    assert_eq!(plugin.poll()["unit"]["id"], "7", "claimed afresh");
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
