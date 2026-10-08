//! The wire contract suite: the real `afkd-github` exec, spoken to exactly as afkd speaks
//! to it (`docs/plugins.md`, "The trigger protocol"), against a stateful fake GitHub
//! reached through the kind's own `host` — a non-`github.com` host, so the client resolves
//! it to the GHES `/api/v3` root. No afkd is involved.
//!
//! Every leg ends by closing stdin and holding the child to the wire's discipline — a
//! clean exit, one reply line per request, nothing else on stdout
//! ([`Plugin::finish`](common::Plugin::finish)) — so a diagnostic that strayed onto stdout
//! fails whichever leg wrote it.

mod common;

use serde_json::{json, Value};

use common::fake::{FakeGithub, TOKEN};
use common::{Plugin, MAX_LINE};

/// The token's login: non-ASCII and bracketed, as a GitHub App's bot login is.
const ME: &str = "björn-öst[bot]";
/// A human who shares the issue with the bot.
const HUMAN: &str = "陳大文";
const REPO: &str = "acme/widgets";
const TITLE: &str = "修复 the retry storm 🚨";
const BODY: &str = "The client retries forever once the token expires.\n\n\
                    ```rust\nlet backoff = Duration::ZERO;\n```\n\n— reported by 陳大文";

/// A repo with issue #7 up for grabs, a human already assigned to it.
fn forge() -> FakeGithub {
    let fake = FakeGithub::start(ME);
    fake.issue(REPO, 7, TITLE, BODY, &["afkd/ready"], &[HUMAN]);
    fake
}

/// A full `issue` block, lowered to JSON as afkd lowers it: every value a string — and
/// afkd's own three keys along for the ride. The hooks never cross here; afkd runs them.
fn settings(fake: &FakeGithub) -> Value {
    json!({
        "host": fake.host(),
        "repo": REPO,
        "token": TOKEN,
        "source_label": "afkd/ready",
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
/// every one landing — under the armed kind's names, so `comment` is `pr_comment` on the
/// pr kind.
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

/// The `on_claim` of a config that takes the item — `github.assign_me(issue)` then
/// `github.label_add(issue, working)`, or the pr kind's `github.pr_assign_me(pr)` and so on — as
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

/// The ids of the claim markers on an issue.
fn markers(fake: &FakeGithub, number: u64) -> Vec<u64> {
    fake.comments(REPO, number)
        .into_iter()
        .filter(|c| c.body.starts_with("[afkd-claim]"))
        .map(|c| c.id)
        .collect()
}

fn labels(fake: &FakeGithub, number: u64) -> Vec<String> {
    fake.issue_state(REPO, number).labels
}

fn assignees(fake: &FakeGithub, number: u64) -> Vec<String> {
    fake.issue_state(REPO, number).assignees
}

// --- hello ---

/// The accepted `hello` lists the calls the kind answers, after reading the token's login
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
    assert_eq!(seen, [("GET".to_string(), "/api/v3/user".to_string())]);
    plugin.finish();
}

/// What `hello` cannot arm with answers `ok:false`, with the problem on stderr in the
/// built-in's own sentence — a kind this plugin does not provide, a block the manifest
/// cannot refuse (on either kind), and a protocol from a later afkd.
#[test]
fn hello_refuses_what_it_cannot_arm_with() {
    let fake = forge();
    let mut no_repo = settings(&fake);
    no_repo["repo"] = json!("");
    let mut pr_no_repo = pr_settings(&fake);
    pr_no_repo["repo"] = json!("");
    for (kind, proto, settings, sentence) in [
        (
            "github_issues",
            2,
            settings(&fake),
            "kind `github_issues` is not provided by @afkd/github",
        ),
        (
            "github_issue",
            2,
            settings(&fake),
            "kind `github_issue` is not provided by @afkd/github",
        ),
        (
            "issue",
            2,
            no_repo,
            "trigger issue: setting `repo`: a github trigger needs a `repo` (`owner/name`)",
        ),
        (
            "issue",
            1,
            settings(&fake),
            "afkd speaks plugin protocol 1, and this plugin speaks 2",
        ),
        (
            "pr",
            2,
            pr_no_repo,
            "trigger pr: setting `repo`: a github trigger needs a `repo` (`owner/name`)",
        ),
    ] {
        let mut plugin = Plugin::spawn();
        let reply = plugin
            .call(json!({"call": "hello", "proto": proto, "kind": kind, "settings": settings}));
        assert_eq!(reply, json!({"ok": false, "proto": 2}), "{kind} {proto}");
        let stderr = plugin.stderr_soon(sentence);
        assert_eq!(stderr, format!("afkd-github: {sentence}\n"));
        plugin.finish();
    }
    assert!(fake.seen().is_empty(), "a refused hello touches no forge");
}

// --- poll ---

/// The won race hands over exactly the unit the built-in would have run: its journal key
/// and session thread, no `seen`, its identity, the four env names the skill reads, and the
/// scratch layout with the brief unframed — with every field the manifest's `Issue`
/// declares, its labels by name as the poll read them. The forge then holds the claim — the
/// marker and the gate, and nothing a hook does until `on_claim`'s calls land — and every
/// request went to the GHES root with the token as a `Bearer`.
#[test]
fn a_won_race_hands_over_the_built_ins_unit() {
    let fake = forge();
    fake.issue(REPO, 7, TITLE, BODY, &["afkd/ready", "優先/high"], &[HUMAN]);
    let mut plugin = Plugin::armed(settings(&fake));
    let reply = plugin.poll();
    assert_eq!(reply["fire"], true, "{}", plugin.stderr());
    let marker = markers(&fake, 7);
    assert_eq!(marker.len(), 1, "one marker, ours");
    assert_eq!(
        reply["unit"],
        json!({
            "id": "7",
            "key": format!("acme/widgets#7#{}", marker[0]),
            "fields": {
                "title": TITLE,
                "url": fake.url(REPO, 7),
                "number": 7,
                "labels": ["afkd/ready", "優先/high"],
            },
            "thread": "acme/widgets#7",
            "seen": [],
            "self": ME,
            "env": {
                "GITHUB_HOST": fake.host(),
                "GITHUB_ISSUE_NUMBER": "7",
                "GITHUB_REPO": REPO,
                "GITHUB_TOKEN": TOKEN,
            },
            "files": [
                {"path": "task.md", "text": format!("{TITLE}\n\n{BODY}")},
                {"path": "issue/number", "text": "7"},
            ],
        })
    );

    let comments = fake.comments(REPO, 7);
    assert_eq!(comments[0].body, "[afkd-claim] owner=björn-öst[bot]");
    assert_eq!(comments[0].author, ME);
    assert_eq!(
        labels(&fake, 7),
        ["afkd/ready", "優先/high", "afkd/claimed"]
    );
    assert_eq!(assignees(&fake, 7), [HUMAN], "the claim assigns no one");
    on_claim(&mut plugin, &reply["unit"], "afkd/working");
    assert_eq!(
        labels(&fake, 7),
        ["afkd/ready", "優先/high", "afkd/claimed", "afkd/working"]
    );
    assert_eq!(assignees(&fake, 7), [HUMAN, ME], "the human was kept");
    // The race is post → settle → re-read: the marker went up before the thread read.
    let seen = fake.seen();
    let comments_path = "/api/v3/repos/acme/widgets/issues/7/comments";
    let post = seen
        .iter()
        .position(|r| r.method == "POST" && r.path == comments_path)
        .expect("the marker was posted");
    assert!(seen[post + 1..]
        .iter()
        .any(|r| r.method == "GET" && r.path == comments_path));
    // The issues listing is asked exactly as the built-in asks it.
    let list = seen
        .iter()
        .find(|r| r.path == "/api/v3/repos/acme/widgets/issues")
        .expect("the issues were listed");
    assert_eq!(
        list.query,
        [
            ("state".to_string(), "open".to_string()),
            ("labels".to_string(), "afkd/ready".to_string()),
        ]
    );
    // The assignee is added by login on the dedicated endpoint.
    assert!(seen.iter().any(|r| r.method == "POST"
        && r.path == "/api/v3/repos/acme/widgets/issues/7/assignees"
        && r.body == r#"{"assignees":["björn-öst[bot]"]}"#));
    assert!(
        seen.iter()
            .all(|r| r.path.starts_with("/api/v3/") && r.auth == format!("Bearer {TOKEN}")),
        "{seen:?}"
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
    fake.comment_with_id(
        424_242,
        REPO,
        7,
        "autocoder",
        "[afkd-claim] owner=autocoder",
        60,
        60,
    );
    let mut plugin = Plugin::armed(settings(&fake));
    assert_eq!(plugin.poll(), json!({"fire": false}));
    assert_eq!(
        markers(&fake, 7),
        [424_242],
        "only the rival's marker is left"
    );
    assert_eq!(labels(&fake, 7), ["afkd/ready"]);
    assert_eq!(assignees(&fake, 7), [HUMAN]);
    let writes: Vec<_> = fake
        .seen()
        .into_iter()
        .filter(|r| r.method != "GET")
        .map(|r| (r.method, r.path))
        .collect();
    assert_eq!(
        writes.len(),
        2,
        "our marker, posted and deleted: {writes:?}"
    );
    assert!(writes[1]
        .1
        .starts_with("/api/v3/repos/acme/widgets/issues/comments/"));
    plugin.finish();
}

#[test]
fn a_lost_race_moves_on_to_the_next_issue() {
    let fake = forge();
    fake.issue(REPO, 8, "Cap the backoff", "", &["afkd/ready"], &[]);
    fake.comment_with_id(
        424_242,
        REPO,
        7,
        "autocoder",
        "[afkd-claim] owner=autocoder",
        60,
        60,
    );
    let mut plugin = Plugin::armed(settings(&fake));
    let unit = plugin.poll()["unit"].clone();
    assert_eq!(unit["thread"], "acme/widgets#8");
    // A bodyless issue briefs with its title alone.
    assert_eq!(unit["files"][0]["text"], "Cap the backoff");
    assert_eq!(markers(&fake, 7), [424_242]);
    assert!(labels(&fake, 8).contains(&"afkd/claimed".to_string()));
    plugin.finish();
}

/// A marker the built-in trigger left — its own text, renewed by its fire in the built-in's
/// renewal spelling — is a claim here too, judged live by its last edit: renewed two
/// minutes ago it blocks, however old its creation; left unrenewed past the claim's hour
/// it does not.
#[test]
fn a_marker_the_built_in_left_is_a_claim_while_its_renewal_is_live() {
    let fake = forge();
    fake.comment_with_id(
        31_337,
        REPO,
        7,
        ME,
        "[afkd-claim] owner=björn-öst[bot] renewal=40",
        4 * 3600,
        120,
    );
    let mut plugin = Plugin::armed(settings(&fake));
    assert_eq!(plugin.poll(), json!({"fire": false}), "{}", plugin.stderr());
    assert_eq!(markers(&fake, 7), [31_337]);
    assert!(!labels(&fake, 7).contains(&"afkd/claimed".to_string()));

    // An hour and a second with no renewal: the built-in's run is dead.
    fake.advance(3600 - 120 + 1);
    let unit = plugin.poll()["unit"].clone();
    assert_eq!(unit["id"], "7", "{}", plugin.stderr());
    plugin.finish();
}

#[test]
fn an_empty_repo_polls_idle_and_a_closed_claimed_unlabelled_or_pull_request_is_passed_over() {
    let fake = FakeGithub::start(ME);
    let mut plugin = Plugin::armed(settings(&fake));
    assert_eq!(plugin.poll(), json!({"fire": false}));

    fake.issue(
        REPO,
        3,
        "already mine",
        "",
        &["afkd/ready", "afkd/claimed"],
        &[],
    );
    fake.issue(REPO, 4, "done", "", &["afkd/ready"], &[]);
    fake.close(REPO, 4);
    fake.issue(REPO, 5, "not for us", "", &[], &[HUMAN]);
    // A pull request wearing the source label: GitHub lists it among the issues.
    fake.pull(REPO, 6, HUMAN, "fix/the-boat", &["afkd/ready"], &[]);
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
        "afkd-github: github list issues: forge returned status 500\n"
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
    let before = fake.comments(REPO, 7)[0].clone();
    fake.advance(300);
    assert_eq!(
        plugin.call(json!({"call": "renew", "key": unit["key"], "renewal": 1})),
        json!({"ok": true})
    );
    let after = fake.comments(REPO, 7)[0].clone();
    assert_eq!(after.id, before.id, "edited, not re-posted");
    assert_eq!(after.body, "[afkd-claim] owner=björn-öst[bot] renewal=1");
    assert_eq!(after.created, before.created);
    assert_eq!(
        after.updated,
        before.updated + 300,
        "the liveness stamp moved"
    );
    assert!(fake.seen().iter().any(|r| r.method == "PATCH"
        && r.path == format!("/api/v3/repos/acme/widgets/issues/comments/{}", after.id)));
    // A key it does not hold is declined, not guessed at.
    assert_eq!(
        plugin.call(json!({"call": "renew", "key": "acme/widgets#8#1", "renewal": 1})),
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
    let a = fake.comment(REPO, 7, HUMAN, "看起来不对 🚨\n\n    max_backoff = 30\n", 0);
    fake.advance(1);
    fake.comment(REPO, 7, ME, "Looking into it.", 0);
    fake.advance(1);
    let b = fake.comment(REPO, 7, "álvaro", "Exponential, please — see §4 🙏", 0);
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
    let c = fake.comment(REPO, 7, "álvaro", "…and cap it at 30s.", 0);
    let reply = read(&mut plugin);
    let ids: Vec<&str> = reply["comments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [c.to_string()]);

    fake.fail("list comments", 500);
    assert_eq!(read(&mut plugin), json!({"comments": null}));
    fake.heal("list comments");
    assert_eq!(read(&mut plugin), json!({"comments": []}));
    assert!(
        plugin
            .stderr_soon("list comments")
            .contains("afkd-github: github list comments: forge returned status 500"),
        "{}",
        plugin.stderr()
    );
    plugin.finish();
}

// --- release ---

/// Issue #9, claimed by a run that crashed — the built-in's or this plugin's, since both
/// leave the same traces: the status label, the bot and a human assigned, and the bot's
/// renewed marker. Returns the journal key that run left, in the built-in's shape.
fn crashed(fake: &FakeGithub) -> String {
    fake.issue(
        REPO,
        9,
        "Crashed mid-run",
        "",
        &["afkd/ready", "afkd/claimed", "afkd/working"],
        &[ME, HUMAN],
    );
    fake.comment_with_id(
        6744,
        REPO,
        9,
        ME,
        "[afkd-claim] owner=björn-öst[bot] renewal=4",
        900,
        600,
    );
    "acme/widgets#9#6744".to_string()
}

/// afkd's reaper runs before its poll, so a freshly spawned child's first call after
/// `hello` can be `release` of a key a crashed run left. The identity `hello` read serves
/// it, and the release is the built-in's whole one: the status label, the bot's assignment (the
/// human's is kept) and the marker. A key of the pre-marker shape names nothing (`null`);
/// one whose repo half is not `owner/name` is kept for a later try (`false`).
#[test]
fn release_reads_the_key_shapes() {
    let fake = forge();
    let key = crashed(&fake);
    let mut plugin = Plugin::armed(settings(&fake));
    assert_eq!(
        plugin.call(json!({"call": "release", "key": key})),
        json!({"released": true})
    );
    assert_eq!(labels(&fake, 9), ["afkd/ready", "afkd/working"]);
    assert_eq!(assignees(&fake, 9), [HUMAN], "only afkd let go");
    assert!(markers(&fake, 9).is_empty());
    let seen = fake.seen();
    assert_eq!(
        (seen[0].method.as_str(), seen[0].path.as_str()),
        ("GET", "/api/v3/user"),
        "the identity came first"
    );
    assert!(seen.iter().any(|r| r.method == "DELETE"
        && r.path == "/api/v3/repos/acme/widgets/issues/9/labels/afkd%2Fclaimed"));

    for key in ["acme/widgets#9", "garbage"] {
        assert_eq!(
            plugin.call(json!({"call": "release", "key": key})),
            json!({"released": null})
        );
    }
    assert_eq!(
        plugin.call(json!({"call": "release", "key": "not-a-repo#9#6744"})),
        json!({"released": false})
    );
    assert_eq!(
        fake.seen().len(),
        seen.len(),
        "no call for a key it cannot use"
    );
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
        "afkd-github: github current user: forge returned status 500\n".repeat(2)
    );
    assert!(
        fake.seen().iter().all(|r| r.path == "/api/v3/user"),
        "nothing past the identity: {:?}",
        fake.seen()
    );
    assert!(labels(&fake, 9).contains(&"afkd/claimed".to_string()));

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
    assert!(!labels(&fake, 7).contains(&"afkd/claimed".to_string()));
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
    on_claim(&mut plugin, &unit, "afkd/working");
    assert_eq!(
        finish(&mut plugin, &unit, "clean", facts("proceed", None)),
        json!({"ok": true})
    );
    assert_eq!(
        fake.issue_state(REPO, 7).state,
        "open",
        "the finish closes nothing"
    );
    assert!(markers(&fake, 7).is_empty());

    hook(
        &mut plugin,
        &unit,
        &[
            ("label_remove", json!({"label": "afkd/working"})),
            ("close", json!({})),
        ],
    );
    let issue = fake.issue_state(REPO, 7);
    assert_eq!(issue.state, "closed");
    assert_eq!(issue.labels, ["afkd/ready", "afkd/claimed"]);
    assert!(fake.seen().iter().any(|r| r.method == "PATCH"
        && r.path == "/api/v3/repos/acme/widgets/issues/7"
        && r.body == r#"{"state":"closed"}"#));
    plugin.finish();
}

#[test]
fn a_failed_finish_then_on_fail_keeps_a_human_assignee() {
    let fake = forge();
    let mut plugin = Plugin::armed(settings(&fake));
    let unit = plugin.poll()["unit"].clone();
    on_claim(&mut plugin, &unit, "afkd/working");
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
            ("label_remove", json!({"label": "afkd/working"})),
            ("unassign", json!({})),
        ],
    );
    let issue = fake.issue_state(REPO, 7);
    assert_eq!(issue.assignees, [HUMAN], "only afkd let go");
    assert_eq!(issue.state, "open");
    assert!(!issue.labels.contains(&"afkd/working".to_string()));
    assert!(markers(&fake, 7).is_empty());
    assert!(fake.seen().iter().any(|r| r.method == "DELETE"
        && r.path == "/api/v3/repos/acme/widgets/issues/7/assignees"
        && r.body == r#"{"assignees":["björn-öst[bot]"]}"#));
    plugin.finish();
}

/// GitHub has no park of its own, so a `park` verdict ends the run like any other: the
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
    let issue = fake.issue_state(REPO, 7);
    assert_eq!(issue.assignees, [HUMAN]);
    assert_eq!(issue.state, "open");
    assert!(fake.comments(REPO, 7).is_empty(), "the marker went");

    let stopped = "Stopped after 2m 48s — log: .afkd/runs/260925-095800-unit-7-1/run.log\n\n\
                   看起来 it needs a human call 🙏";
    hook(
        &mut plugin,
        &unit,
        &[("comment", json!({ "text": stopped }))],
    );
    let said: Vec<String> = fake.comments(REPO, 7).into_iter().map(|c| c.body).collect();
    assert_eq!(said, [stopped]);
    plugin.finish();
}

/// Nothing a finish does can leave it undelivered, so nothing is `held`: a marker delete
/// the forge refused is diagnosed, the finish still answers plain `ok`, and the claim —
/// the label — stays until afkd's `release` or a hook takes it.
#[test]
fn a_finish_whose_marker_delete_fails_still_answers_plain_ok() {
    let fake = forge();
    let mut plugin = Plugin::armed(settings(&fake));
    let unit = plugin.poll()["unit"].clone();
    fake.fail("delete comment", 500);

    assert_eq!(
        finish(&mut plugin, &unit, "clean", facts("proceed", None)),
        json!({"ok": true})
    );
    assert_eq!(
        plugin.stderr_soon("delete comment"),
        "afkd-github: github delete comment: forge returned status 500\n"
    );
    assert_eq!(markers(&fake, 7).len(), 1, "left to age out");
    assert!(labels(&fake, 7).contains(&"afkd/claimed".to_string()));
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
        plugin.act("label_add", json!({"label": "afkd/reviewed ✅"}), &unit),
        json!({"ok": true})
    );
    assert_eq!(
        plugin.act("label_remove", json!({"label": "afkd/ready"}), &unit),
        json!({"ok": true})
    );
    assert_eq!(labels(&fake, 7), ["afkd/claimed", "afkd/reviewed ✅"]);
    assert_eq!(
        plugin.act("comment", json!({"text": markdown}), &unit),
        json!({"ok": true})
    );
    let said: Vec<(String, String)> = fake
        .comments(REPO, 7)
        .into_iter()
        .filter(|c| !c.body.starts_with("[afkd-claim]"))
        .map(|c| (c.author, c.body))
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
    assert_eq!(fake.issue_state(REPO, 7).state, "open");
    assert_eq!(plugin.act("close", json!({}), &unit), json!({"ok": true}));
    assert_eq!(fake.issue_state(REPO, 7).state, "closed");

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
        stderr.contains(&format!("afkd-github: {refused}")),
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
        json!({"call": "classify", "key": "acme/widgets#7#1",
               "scratch": "/tmp/afkd-scratch", "outcome": "failed"}),
        json!({"call": "attempt_failed", "key": "acme/widgets#7#1", "n": 1, "max": 2,
               "reason": "agent exited 1"}),
    ] {
        assert_eq!(plugin.call(request), json!({"ok": false}));
    }
    let refused = "afkd-github: afkd sent a call this plugin did not list in its `hello` reply\n";
    assert_eq!(plugin.stderr_lines_soon(2), refused.repeat(2));
    assert_eq!(plugin.poll()["unit"]["id"], "7");
    plugin.finish();
}

// --- the line budget ---

/// A 100 KiB issue body cannot cross in one 64 KiB line: the brief is cut, the line fits
/// and decodes, and the brief says where the rest is.
#[test]
fn an_oversized_brief_is_cut_to_fit_one_line() {
    let fake = FakeGithub::start(ME);
    let body = "看起来不对 🚨 — the retry path.\n".repeat(2_600);
    assert!(body.len() > 100 * 1024);
    fake.issue(REPO, 7, TITLE, &body, &["afkd/ready"], &[]);
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
        brief.ends_with("read the whole issue with the github skill]"),
        "{}",
        &brief[brief.len() - 200..]
    );
    assert!(plugin
        .stderr_soon("cut to fit")
        .contains("the brief for acme/widgets#7 was cut to fit"));
    plugin.finish();
}

/// 400 comments of 300 bytes are over the line too: the reply keeps the newest that fit.
#[test]
fn an_overflowing_thread_is_cut_to_fit_one_line() {
    let fake = forge();
    let mut plugin = Plugin::armed(settings(&fake));
    let unit = plugin.poll()["unit"].clone();
    let mut last = 0;
    for _ in 0..400 {
        fake.advance(1);
        last = fake.comment(REPO, 7, "álvaro", &"x".repeat(300), 0);
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

// --- pr ---

/// The PR's head branch: non-ASCII, and crossing to the run verbatim.
const BRANCH: &str = "fix/重试-retry-cap";
/// The PR's title: the quotes a log line wraps it in, wide CJK and an emoji.
const PR_TITLE: &str = "Cap the retry backoff — \"重试\" 上限 🚦";
/// The human's review comment on PR #7: multi-line, with an indented code line and a
/// trailing newline the brief trims.
const REVIEW: &str = "看起来不对 🚨 — the cap never applies:\n\n    max_backoff = 0\n";

/// A repo with the bot's own PR #7 on [`BRANCH`], titled [`PR_TITLE`], a human assigned to
/// it, and the human's review comment two minutes old — new feedback, since the bot has not
/// spoken. Returns the fake and the comment's id.
fn pr_forge() -> (FakeGithub, u64) {
    let fake = FakeGithub::start(ME);
    fake.pull(REPO, 7, ME, BRANCH, &[], &[HUMAN]);
    fake.retitle(REPO, 7, PR_TITLE);
    let review = fake.comment(REPO, 7, HUMAN, REVIEW, 120);
    (fake, review)
}

/// A full `pr` block, lowered to JSON as afkd lowers it — `author_me` a `bool` as its word
/// — and afkd's own three keys along for the ride.
fn pr_settings(fake: &FakeGithub) -> Value {
    json!({
        "host": fake.host(),
        "repo": REPO,
        "token": TOKEN,
        "author_me": "true",
        "poll_interval": "30s",
        "max_attempts": "1",
        "follow_comments": "2m",
    })
}

/// The `finish` envelope's facts for a PR round, as afkd writes them.
fn pr_facts(signal: &str, reason: Option<&str>) -> Value {
    json!({"signal": signal, "reason": reason, "duration_ms": 168000, "cost": 0.4217,
           "turns": 12, "tokens": null, "run_name": "260925-095800-unit-7-1"})
}

/// Whether the fake saw a write — anything but a `GET`.
fn wrote(fake: &FakeGithub) -> bool {
    fake.seen().iter().any(|r| r.method != "GET")
}

#[test]
fn pr_hello_lists_release_renew_comments() {
    let (fake, _) = pr_forge();
    let mut plugin = Plugin::spawn();
    assert_eq!(
        plugin.hello("pr", pr_settings(&fake)),
        json!({"ok": true, "proto": 2, "calls": ["release", "renew", "comments"]})
    );
    assert!(
        fake.seen().iter().all(|r| r.path == "/api/v3/user"),
        "hello reads only the token's user"
    );
    plugin.finish();
}

/// The won race over a PR with a human's new comment hands over exactly the unit the
/// built-in would have run: its journal key and session thread, every field the manifest's
/// `Pull_Request` declares, the claim-time thread as `seen`, its identity, the five env
/// names the skill reads with the branch verbatim, and the scratch layout with the review
/// brief unframed. The open pulls were listed as the built-in lists them, the PR's reviews
/// were read, and every write went to the PR's issue paths — a PR is an issue — at the GHES
/// root with the token as a `Bearer`. A second poll, the claim still live, hands over
/// nothing: our own older marker out-orders the new one, which is taken straight back.
#[test]
fn pr_a_won_race_hands_over_the_built_ins_unit() {
    let (fake, review) = pr_forge();
    let mut plugin = Plugin::armed_as("pr", pr_settings(&fake));
    let reply = plugin.poll();
    assert_eq!(reply["fire"], true, "{}", plugin.stderr());
    let marker = markers(&fake, 7);
    assert_eq!(marker.len(), 1, "one marker, ours");
    assert_eq!(
        reply["unit"],
        json!({
            "id": "7",
            "key": format!("acme/widgets#7#{}", marker[0]),
            "fields": {
                "title": PR_TITLE,
                "url": fake.url(REPO, 7),
                "number": 7,
                "branch": BRANCH,
            },
            "thread": "acme/widgets#7",
            "seen": [review.to_string()],
            "self": ME,
            "env": {
                "GITHUB_HOST": fake.host(),
                "GITHUB_PR_BRANCH": BRANCH,
                "GITHUB_PR_NUMBER": "7",
                "GITHUB_REPO": REPO,
                "GITHUB_TOKEN": TOKEN,
            },
            "files": [
                {"path": "task.md", "text": "Address review feedback on PR #7.\n\n\
                    ## New feedback\n\n**陳大文:** 看起来不对 🚨 — the cap never applies:\n\n    \
                    max_backoff = 0\n"},
                {"path": "pr/number", "text": "7"},
            ],
        })
    );

    assert_eq!(labels(&fake, 7), ["afkd/claimed"]);
    assert_eq!(assignees(&fake, 7), [HUMAN], "the claim assigns no one");
    on_claim(&mut plugin, &reply["unit"], "afkd/reviewing");
    assert_eq!(labels(&fake, 7), ["afkd/claimed", "afkd/reviewing"]);
    assert_eq!(assignees(&fake, 7), [HUMAN, ME], "the human was kept");
    let seen = fake.seen();
    let list = seen
        .iter()
        .find(|r| r.path == "/api/v3/repos/acme/widgets/pulls")
        .expect("the pulls were listed");
    assert_eq!(list.query, [("state".to_string(), "open".to_string())]);
    assert!(
        seen.iter()
            .any(|r| r.method == "GET" && r.path == "/api/v3/repos/acme/widgets/pulls/7/reviews"),
        "the reviews were read: {seen:?}"
    );
    assert!(
        seen.iter()
            .filter(|r| r.method != "GET")
            .all(|r| r.path.starts_with("/api/v3/repos/acme/widgets/issues/")),
        "a write left the PR's issue paths: {seen:?}"
    );
    assert!(
        seen.iter()
            .all(|r| r.path.starts_with("/api/v3/") && r.auth == format!("Bearer {TOKEN}")),
        "{seen:?}"
    );

    assert_eq!(plugin.poll(), json!({"fire": false}), "the claim is live");
    assert_eq!(
        markers(&fake, 7),
        marker,
        "the second marker was taken back"
    );
    plugin.finish();
}

/// A review alone is a round: the bot answered the human's comment, and the only word
/// newer than its reply is a submitted review, which fires the PR and briefs as the
/// reviewer's `(review N)` line — the comment already answered stays out of it.
#[test]
fn pr_a_review_alone_fires_the_round() {
    let fake = FakeGithub::start(ME);
    fake.pull(REPO, 7, ME, BRANCH, &[], &[]);
    fake.comment(REPO, 7, HUMAN, REVIEW, 400);
    fake.comment(REPO, 7, ME, "Pushed 3f2a1c: the cap applies now.", 300);
    let review = fake.review(REPO, 7, "álvaro", 60);
    let mut plugin = Plugin::armed_as("pr", pr_settings(&fake));
    let unit = plugin.poll()["unit"].clone();
    assert_eq!(unit["id"], "7", "{}", plugin.stderr());
    assert_eq!(
        unit["files"][0]["text"],
        format!("Address review feedback on PR #7.\n\n## New feedback\n\n**álvaro:** (review {review})\n")
    );
    plugin.finish();
}

/// A live rival marker a minute older out-orders ours: nothing is handed over, our marker
/// is taken back, and no status is written.
#[test]
fn pr_a_lost_race_hands_over_nothing_and_takes_its_marker_back() {
    let (fake, _) = pr_forge();
    let rival = fake.comment(REPO, 7, "autocoder", "[afkd-claim] owner=autocoder", 60);
    let mut plugin = Plugin::armed_as("pr", pr_settings(&fake));
    assert_eq!(plugin.poll(), json!({"fire": false}));
    assert_eq!(
        markers(&fake, 7),
        [rival],
        "only the rival's marker is left"
    );
    assert!(labels(&fake, 7).is_empty(), "{:?}", labels(&fake, 7));
    assert_eq!(assignees(&fake, 7), [HUMAN]);
    let writes: Vec<_> = fake
        .seen()
        .into_iter()
        .filter(|r| r.method != "GET")
        .map(|r| (r.method, r.path))
        .collect();
    assert_eq!(
        writes.len(),
        2,
        "our marker, posted and deleted: {writes:?}"
    );
    plugin.finish();
}

/// The bot has answered the human's comment, and nothing newer has arrived — a review
/// older than its reply included: the PR is idle, and the poll posts nothing at all.
#[test]
fn pr_with_no_new_feedback_does_not_fire() {
    let (fake, _) = pr_forge();
    fake.review(REPO, 7, "álvaro", 90);
    fake.comment(REPO, 7, ME, "Pushed 3f2a1c: the cap applies now.", 30);
    let mut plugin = Plugin::armed_as("pr", pr_settings(&fake));
    assert_eq!(plugin.poll(), json!({"fire": false}));
    assert!(
        !wrote(&fake),
        "an idle PR is never claimed: {:?}",
        fake.seen()
    );
    plugin.finish();
}

/// The only comment newer than the bot's reply is a rival's claim marker — a stale one,
/// over an hour old, so it could never win a race. Nothing is posted, which proves the PR
/// was refused as having no new feedback rather than lost to the marker.
#[test]
fn pr_whose_only_new_comment_is_a_claim_marker_does_not_fire() {
    let fake = FakeGithub::start(ME);
    fake.pull(REPO, 7, ME, BRANCH, &[], &[]);
    fake.comment(REPO, 7, HUMAN, REVIEW, 7_400);
    fake.comment(REPO, 7, ME, "Pushed 3f2a1c: the cap applies now.", 7_300);
    fake.comment(REPO, 7, "autocoder", "[afkd-claim] owner=autocoder", 3_700);
    let mut plugin = Plugin::armed_as("pr", pr_settings(&fake));
    assert_eq!(plugin.poll(), json!({"fire": false}));
    assert!(!wrote(&fake), "a marker is not feedback: {:?}", fake.seen());
    plugin.finish();
}

/// Under `author_me` a human's PR is passed over, feedback and all, and nothing is
/// posted; a service armed without the flag, on the same forge, claims it.
#[test]
fn pr_author_me_filters_foreign_prs() {
    let fake = FakeGithub::start(ME);
    fake.pull(REPO, 8, HUMAN, "fix/y", &[], &[]);
    fake.comment(REPO, 8, "álvaro", "Exponential, please — see §4 🙏", 60);

    let mut mine = Plugin::armed_as("pr", pr_settings(&fake));
    assert_eq!(mine.poll(), json!({"fire": false}));
    assert!(!wrote(&fake), "{:?}", fake.seen());
    assert!(
        fake.seen().iter().all(|r| !r.path.ends_with("/reviews")),
        "a filtered PR's feedback is never read: {:?}",
        fake.seen()
    );
    mine.finish();

    let mut settings = pr_settings(&fake);
    settings.as_object_mut().unwrap().remove("author_me");
    let mut anyone = Plugin::armed_as("pr", settings);
    let unit = anyone.poll()["unit"].clone();
    assert_eq!(unit["id"], "8");
    assert_eq!(unit["env"]["GITHUB_PR_BRANCH"], "fix/y");
    anyone.finish();
}

#[test]
fn pr_renew_rewrites_the_marker_in_place() {
    let (fake, _) = pr_forge();
    let mut plugin = Plugin::armed_as("pr", pr_settings(&fake));
    let unit = plugin.poll()["unit"].clone();
    let marker = markers(&fake, 7)[0];
    let comment = |fake: &FakeGithub| {
        fake.comments(REPO, 7)
            .into_iter()
            .find(|c| c.id == marker)
            .expect("the marker")
    };
    let before = comment(&fake);
    fake.advance(300);
    assert_eq!(
        plugin.call(json!({"call": "renew", "key": unit["key"], "renewal": 1})),
        json!({"ok": true})
    );
    let after = comment(&fake);
    assert_eq!(after.body, "[afkd-claim] owner=björn-öst[bot] renewal=1");
    assert_eq!(after.created, before.created);
    assert_eq!(
        after.updated,
        before.updated + 300,
        "the liveness stamp moved"
    );
    let patch = fake
        .seen()
        .into_iter()
        .rfind(|r| r.method == "PATCH")
        .unwrap();
    assert_eq!(
        patch.path,
        format!("/api/v3/repos/acme/widgets/issues/comments/{marker}")
    );
    plugin.finish();
}

/// `comments` reports what afkd has not been told about: the claim-time review comment
/// (`seen`), the marker and the bot's own reply are left out; a second read carries only
/// what is new; and a forge that cannot be read is `null`.
#[test]
fn pr_comments_report_what_afkd_has_not_seen() {
    let (fake, _) = pr_forge();
    let mut plugin = Plugin::armed_as("pr", pr_settings(&fake));
    let unit = plugin.poll()["unit"].clone();
    let read = |plugin: &mut Plugin| plugin.call(json!({"call": "comments", "key": unit["key"]}));

    fake.advance(60);
    let a = fake.comment(REPO, 7, HUMAN, "还有 — the jitter too.\n", 0);
    fake.advance(1);
    fake.comment(REPO, 7, ME, "On it.", 0);
    fake.advance(1);
    let b = fake.comment(REPO, 7, "álvaro", "Exponential, please — see §4 🙏", 0);
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
    let c = fake.comment(REPO, 7, "álvaro", "…and cap it at 30s.", 0);
    let reply = read(&mut plugin);
    assert_eq!(reply["comments"].as_array().unwrap().len(), 1);
    assert_eq!(reply["comments"][0]["id"], c.to_string());

    fake.fail("list comments", 500);
    assert_eq!(read(&mut plugin), json!({"comments": null}));
    assert!(
        plugin
            .stderr_soon("list comments")
            .contains("afkd-github: github list comments: forge returned status 500"),
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
fn pr_release_undoes_the_claim_in_full() {
    let (fake, _) = pr_forge();
    let mut first = Plugin::armed_as("pr", pr_settings(&fake));
    let unit = first.poll()["unit"].clone();
    on_claim(&mut first, &unit, "afkd/reviewing");
    let live = unit["key"].clone();
    assert_eq!(
        first.call(json!({"call": "release", "key": live})),
        json!({"released": true})
    );
    assert_eq!(
        labels(&fake, 7),
        ["afkd/reviewing"],
        "only the status label goes"
    );
    assert_eq!(assignees(&fake, 7), [HUMAN], "only afkd let go");
    assert!(markers(&fake, 7).is_empty());

    // Claimed again, then the child dies mid-run.
    let unit = first.poll()["unit"].clone();
    on_claim(&mut first, &unit, "afkd/reviewing");
    let crashed = unit["key"].clone();
    assert_eq!(assignees(&fake, 7), [HUMAN, ME]);
    drop(first);

    let from = fake.seen().len();
    let mut fresh = Plugin::armed_as("pr", pr_settings(&fake));
    assert_eq!(
        fake.seen()[from].path,
        "/api/v3/user",
        "hello read the identity"
    );
    assert_eq!(
        fresh.call(json!({"call": "release", "key": crashed})),
        json!({"released": true})
    );
    assert!(!labels(&fake, 7).contains(&"afkd/claimed".to_string()));
    assert_eq!(assignees(&fake, 7), [HUMAN]);
    assert!(markers(&fake, 7).is_empty());

    assert_eq!(
        fresh.call(json!({"call": "release", "key": "acme/widgets#7"})),
        json!({"released": null})
    );
    fresh.finish();
}

/// A clean round drops the marker, then `on_done`'s call takes the working label off, and
/// nothing closes the PR — a human's merge ends the loop.
#[test]
fn pr_a_clean_finish_then_on_done_leaves_the_pr_open() {
    let (fake, _) = pr_forge();
    let mut plugin = Plugin::armed_as("pr", pr_settings(&fake));
    let unit = plugin.poll()["unit"].clone();
    on_claim(&mut plugin, &unit, "afkd/reviewing");
    assert_eq!(
        finish(&mut plugin, &unit, "clean", pr_facts("proceed", None)),
        json!({"ok": true})
    );
    hook(
        &mut plugin,
        &unit,
        &[("label_remove", json!({"label": "afkd/reviewing"}))],
    );
    let pr = fake.issue_state(REPO, 7);
    assert_eq!(pr.labels, ["afkd/claimed"]);
    assert_eq!(pr.state, "open");
    assert!(markers(&fake, 7).is_empty());
    assert!(
        fake.seen().iter().all(|r| !r.body.contains("\"state\"")),
        "{:?}",
        fake.seen()
    );
    plugin.finish();
}

/// A failed round, then `on_fail`'s calls: the working label goes, only the bot is
/// unassigned, and the comment — afkd interpolated its run facts — is posted verbatim.
#[test]
fn pr_a_failed_finish_then_on_fail_lets_go() {
    let (fake, review) = pr_forge();
    let mut plugin = Plugin::armed_as("pr", pr_settings(&fake));
    let unit = plugin.poll()["unit"].clone();
    on_claim(&mut plugin, &unit, "afkd/reviewing");
    let reply = finish(
        &mut plugin,
        &unit,
        "failed",
        pr_facts("fault", Some("cargo test: 3 failed")),
    );
    assert_eq!(reply, json!({"ok": true}));
    let stopped = "Stopped after 2m 48s for $0.42 — log: .afkd/runs/260925-095800-unit-7-1/run.log";
    hook(
        &mut plugin,
        &unit,
        &[
            ("label_remove", json!({"label": "afkd/reviewing"})),
            ("unassign", json!({})),
            ("comment", json!({ "text": stopped })),
        ],
    );
    let pr = fake.issue_state(REPO, 7);
    assert_eq!(pr.labels, ["afkd/claimed"]);
    assert_eq!(pr.assignees, [HUMAN], "only afkd let go");
    assert_eq!(pr.state, "open");
    let said: Vec<(u64, String)> = fake
        .comments(REPO, 7)
        .into_iter()
        .map(|c| (c.id, c.body))
        .collect();
    assert_eq!(said[0], (review, REVIEW.to_string()));
    assert_eq!(said[1].1, stopped);
    assert_eq!(said.len(), 2, "the marker went: {said:?}");
    plugin.finish();
}

/// An `on_fail` action the forge refuses is that `call`'s `ok:false`, with the forge's
/// sentence: afkd fails the hook there, and the claim stays until afkd's `release` of the
/// key undoes it. The comment `on_fail` posted before is the bot's last word, so the PR
/// waits for the human — the label is status, not the gate — and once they reply, it is
/// claimed afresh.
#[test]
fn pr_a_refused_on_fail_action_is_the_calls_error_and_release_recovers_the_claim() {
    let (fake, _) = pr_forge();
    let mut plugin = Plugin::armed_as("pr", pr_settings(&fake));
    let unit = plugin.poll()["unit"].clone();
    on_claim(&mut plugin, &unit, "afkd/reviewing");
    let key = unit["key"].as_str().unwrap().to_string();
    assert_eq!(
        finish(&mut plugin, &unit, "failed", pr_facts("fault", None)),
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
    fake.fail("remove assignees", 500);
    assert_eq!(
        plugin.act("pr_unassign", json!({}), &unit),
        json!({"ok": false, "error": "github remove assignees: forge returned status 500"})
    );
    assert!(labels(&fake, 7).contains(&"afkd/claimed".to_string()));
    assert_eq!(assignees(&fake, 7), [HUMAN, ME], "the claim stays");

    fake.heal("remove assignees");
    assert_eq!(
        plugin.call(json!({"call": "release", "key": key})),
        json!({"released": true})
    );
    assert!(!labels(&fake, 7).contains(&"afkd/claimed".to_string()));
    assert_eq!(assignees(&fake, 7), [HUMAN]);
    assert_eq!(plugin.poll(), json!({"fire": false}), "the bot spoke last");

    fake.advance(60);
    fake.comment(REPO, 7, HUMAN, "Still failing on CI — see the job log.", 0);
    let again = plugin.poll()["unit"].clone();
    assert_eq!(again["id"], "7", "claimed afresh");
    assert_ne!(again["key"], unit["key"]);
    plugin.finish();
}

// --- a call names its item ---

/// A call acts on the item its handle names, never on a claim it implies: with issue #7
/// finished and #8 claimed and live, #7's `on_done` — sent after `finish`, as afkd sends
/// it — comments on and closes the finished issue over the real wire and leaves the live
/// one open and silent. On a pull-request service the same holds under its own names: a
/// `pr_comment` after `finish` lands on the pull request its handle names.
#[test]
fn a_post_run_call_acts_on_the_finished_item_its_handle_names() {
    let fake = forge();
    fake.issue(
        REPO,
        8,
        "第二の課題 — the next one 🚧",
        "And this.",
        &["afkd/ready"],
        &[],
    );
    let mut plugin = Plugin::armed(settings(&fake));
    let first = plugin.poll()["unit"].clone();
    on_claim(&mut plugin, &first, "afkd/working");
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
    let said = |n: u64| -> Vec<String> {
        fake.comments(REPO, n)
            .into_iter()
            .filter(|c| c.body == landed)
            .map(|c| c.body)
            .collect()
    };
    assert_eq!(said(7), [landed]);
    assert_eq!(fake.issue_state(REPO, 7).state, "closed");
    assert!(said(8).is_empty(), "the live issue got nothing");
    assert_eq!(fake.issue_state(REPO, 8).state, "open");
    plugin.finish();

    let (fake, _) = pr_forge();
    let mut plugin = Plugin::armed_as("pr", pr_settings(&fake));
    let unit = plugin.poll()["unit"].clone();
    on_claim(&mut plugin, &unit, "afkd/reviewing");
    assert_eq!(
        finish(&mut plugin, &unit, "clean", pr_facts("proceed", None)),
        json!({"ok": true})
    );
    let round = "Round done ✅ — see 4f2a9c1.";
    assert_eq!(
        plugin.act("pr_comment", json!({ "text": round }), &unit),
        json!({"ok": true})
    );
    assert!(fake
        .comments(REPO, 7)
        .iter()
        .any(|c| c.author == ME && c.body == round));
    plugin.finish();
}
