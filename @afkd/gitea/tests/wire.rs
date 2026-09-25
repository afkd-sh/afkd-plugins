//! The wire contract suite: the real `afkd-gitea` exec, spoken to exactly as afkd speaks
//! to it (`docs/plugins.md`, "The trigger protocol"), against a stateful fake Gitea
//! reached through the kind's own `base_url`. No afkd is involved.
//!
//! Every leg ends by closing stdin and holding the child to the wire's discipline — a
//! clean exit, one reply line per request, nothing else on stdout
//! ([`Plugin::finish`](common::Plugin::finish)) — so a diagnostic that strayed onto
//! stdout fails whichever leg wrote it.

mod common;

use serde_json::{json, Value};

use common::fake::{FakeGitea, TOKEN};
use common::{Plugin, TempDir, MAX_LINE};

/// The token's login: non-ASCII and bracketed, as the built-in's own fixtures are.
const ME: &str = "björn-öst[bot]";
const REPO: &str = "acme/widgets";
const TITLE: &str = "修复 the retry storm 🚨";
const BODY: &str = "The client retries forever once the token expires.\n\n\
                    ```rust\nlet backoff = Duration::ZERO;\n```\n\n— reported by 陳大文";

/// A repo with issue #7 up for grabs, and the label `on_claim` adds defined.
fn forge() -> FakeGitea {
    let fake = FakeGitea::start(ME);
    fake.define_label(REPO, "afkd/working", false);
    fake.issue(REPO, 7, TITLE, BODY, &["afkd/ready"], &[]);
    fake
}

/// A full `gitea` block, lowered to JSON as afkd lowers it: repeatable keys as arrays,
/// flags as `true`, blocks as objects — and afkd's own three keys along for the ride.
fn settings(fake: &FakeGitea) -> Value {
    json!({
        "base_url": fake.base_url(),
        "repo": REPO,
        "token": TOKEN,
        "source_label": "afkd/ready",
        "poll_interval": "30s",
        "max_attempts": "1",
        "on_claim": {"assign_me": [true], "label_add": ["afkd/working"]},
        "on_done": {"label_remove": ["afkd/working"], "close": [true]},
        "on_fail": {"label_remove": ["afkd/working"], "unassign": [true]},
        "on_park": {"comment": [
            "Parked after @{run:duration} for @{run:cost} — log: .afkd/runs/@{run:name}/run.log"
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
fn markers(fake: &FakeGitea, number: u64) -> Vec<u64> {
    fake.comments(REPO, number)
        .into_iter()
        .filter(|c| c.body.starts_with("[afkd-claim]"))
        .map(|c| c.id)
        .collect()
}

// --- hello ---

#[test]
fn hello_arms_and_lists_every_call_it_answers() {
    let fake = forge();
    let mut plugin = Plugin::spawn();
    assert_eq!(
        plugin.hello("gitea", settings(&fake)),
        json!({"ok": true, "proto": 1, "calls": ["release", "renew", "comments", "classify"]})
    );
    plugin.finish();
}

/// What `hello` cannot arm with answers `ok:false`, with the problem on stderr in the
/// built-in's own sentence — the second kind this plugin does not yet provide, a block
/// the manifest cannot refuse, and a protocol from a later afkd.
#[test]
fn hello_refuses_what_it_cannot_arm_with() {
    let fake = forge();
    let mut both = settings(&fake);
    both["org"] = json!("acme");
    let mut claim_cost = settings(&fake);
    claim_cost["on_claim"] = json!({"comment": ["claimed; budget @{run:cost}"]});
    for (kind, proto, settings, sentence) in [
        (
            "gitea_pr_review",
            1,
            settings(&fake),
            "kind `gitea_pr_review` is not provided by @afkd/gitea",
        ),
        (
            "gitea",
            1,
            both,
            "a gitea trigger takes exactly one of `repo` or `org` (both were set)",
        ),
        (
            "gitea",
            1,
            claim_cost,
            "`@{run:cost}` references the run's facts, but no run happens at claim time",
        ),
        (
            "gitea",
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
        assert!(stderr.contains(sentence), "{stderr}");
        assert!(stderr.starts_with("afkd-gitea: "), "{stderr}");
        plugin.finish();
    }
    assert!(fake.seen().is_empty(), "a refused hello touches no forge");
}

// --- poll ---

/// The won race hands over exactly the unit the built-in would have run: its journal key
/// and session thread, the claim-time comment ids, its identity, the four env names the
/// skill reads, and the scratch layout with the brief unframed. The forge then holds the
/// claim as the built-in leaves it.
#[test]
fn a_won_race_hands_over_the_built_ins_unit() {
    let fake = forge();
    let mut plugin = Plugin::armed(settings(&fake));
    let reply = plugin.poll();
    assert_eq!(reply["fire"], true);
    let marker = markers(&fake, 7);
    assert_eq!(marker.len(), 1, "one marker, ours");
    assert_eq!(
        reply["unit"],
        json!({
            "id": "7",
            "key": format!("acme/widgets#7#{}", marker[0]),
            "thread": "acme/widgets#7",
            "seen": [],
            "self": ME,
            "env": {
                "GITEA_BASE_URL": fake.base_url(),
                "GITEA_ISSUE_NUMBER": "7",
                "GITEA_REPO": REPO,
                "GITEA_TOKEN": TOKEN,
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
    let issue = fake.issue_state(REPO, 7);
    assert_eq!(issue.labels, ["afkd/ready", "afkd/claimed", "afkd/working"]);
    assert_eq!(issue.assignees, [ME]);
    // Both managed labels were created, plain, before the claim.
    let managed: Vec<(String, bool)> = fake
        .labels(REPO)
        .into_iter()
        .filter(|l| l.name == "afkd/claimed" || l.name == "afkd/awaiting-reply")
        .map(|l| (l.name, l.exclusive))
        .collect();
    assert_eq!(
        managed,
        [
            ("afkd/claimed".into(), false),
            ("afkd/awaiting-reply".into(), false)
        ]
    );
    // The race is post → settle → re-read: the marker went up before the thread read.
    let seen = fake.seen();
    let post = seen
        .iter()
        .position(|r| r.method == "POST" && r.path.ends_with("/issues/7/comments"))
        .unwrap();
    assert!(seen[post + 1..]
        .iter()
        .any(|r| r.method == "GET" && r.path.ends_with("/issues/7/comments")));
    // Gitea's issues listing is asked exactly as the built-in asks it.
    let list = seen
        .iter()
        .find(|r| r.path == "/api/v1/repos/acme/widgets/issues")
        .unwrap();
    assert_eq!(
        list.query,
        [
            ("type".to_string(), "issues".to_string()),
            ("state".to_string(), "open".to_string()),
            ("labels".to_string(), String::new()),
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
    fake.comment_with_id(
        424_242,
        REPO,
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
    let issue = fake.issue_state(REPO, 7);
    assert_eq!(issue.labels, ["afkd/ready"]);
    assert!(issue.assignees.is_empty());
    plugin.finish();
}

#[test]
fn an_empty_repo_polls_idle_and_a_closed_or_claimed_issue_is_passed_over() {
    let fake = FakeGitea::start(ME);
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
    fake.issue(REPO, 5, "not for us", "", &[], &["alice"]);
    let mut plugin = Plugin::armed(settings(&fake));
    assert_eq!(plugin.poll(), json!({"fire": false}));
    assert!(
        fake.seen().iter().all(|r| r.method == "GET"),
        "an idle scan writes nothing: {:?}",
        fake.seen()
    );
    plugin.finish();
}

/// `org` scans each of the org's repos, and a bare assignment to the bot is a signal.
#[test]
fn an_org_wide_poll_claims_across_repos() {
    let fake = FakeGitea::start(ME);
    fake.org("acme", &["acme/gadgets", REPO]);
    fake.define_label(REPO, "afkd/working", false);
    fake.issue(REPO, 12, "Assigned to the bot", "", &[], &[ME]);
    let mut settings = settings(&fake);
    settings.as_object_mut().unwrap().remove("repo");
    settings["org"] = json!("acme");
    let mut plugin = Plugin::armed(settings);
    let reply = plugin.poll();
    assert_eq!(reply["unit"]["thread"], "acme/widgets#12");
    assert_eq!(reply["unit"]["env"]["GITEA_REPO"], REPO);
    // A bodyless issue briefs with its title alone.
    assert_eq!(reply["unit"]["files"][0]["text"], "Assigned to the bot");
    plugin.finish();
}

/// A definite verdict — `afkd/claimed` defined exclusive — ends the process with the
/// built-in's sentence and no reply, which afkd turns into a crashed service.
#[test]
fn an_exclusive_claim_label_ends_the_process_with_the_sentence() {
    let fake = forge();
    fake.define_label(REPO, "afkd/claimed", true);
    let mut plugin = Plugin::armed(settings(&fake));
    plugin.send(&json!({"call": "poll"}));
    let status = plugin.exit();
    assert_eq!(status.code(), Some(1));
    let stderr = plugin.stderr_soon("exclusive");
    assert!(
        stderr.contains(
            "afkd-gitea: label “afkd/claimed” on acme/widgets is an exclusive scoped label, and \
             afkd's own labels must not be"
        ),
        "{stderr}"
    );
    assert!(
        markers(&fake, 7).is_empty(),
        "the refusal preceded the claim"
    );
}

// --- renew, comments, classify ---

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
    let a = fake.comment(
        REPO,
        7,
        "陳大文",
        "看起来不对 🚨\n\n    max_backoff = 30\n",
        0,
    );
    fake.advance(1);
    fake.comment(REPO, 7, ME, "Looking into it.", 0);
    fake.advance(1);
    let b = fake.comment(REPO, 7, "álvaro", "Exponential, please — see §4 🙏", 0);
    let reply = read(&mut plugin);
    assert_eq!(
        reply,
        json!({"comments": [
            {"id": a.to_string(), "author": "陳大文", "author_name": "陳大文",
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
    plugin.finish();
}

/// With `discuss_with`, the claim read the thread, so its ids are the unit's `seen` —
/// and never come back as news.
#[test]
fn a_discuss_with_claim_carries_its_thread_as_seen() {
    let fake = forge();
    let asked = fake.comment(REPO, 7, "陳大文", "Can you look at this?", 120);
    let mut settings = settings(&fake);
    settings["discuss_with"] = json!([["陳大文", "álvaro"]]);
    let mut plugin = Plugin::armed(settings);
    let unit = plugin.poll()["unit"].clone();
    assert_eq!(unit["seen"], json!([asked.to_string()]));
    assert_eq!(
        plugin.call(json!({"call": "comments", "key": unit["key"]})),
        json!({"comments": []})
    );
    plugin.finish();
}

#[test]
fn classify_reads_the_park_marker_and_echoes_afkd_otherwise() {
    let fake = forge();
    let mut plugin = Plugin::armed(settings(&fake));
    let scratch = TempDir::new("classify");
    let classify = |plugin: &mut Plugin, outcome: &str| {
        plugin.call(json!({"call": "classify", "key": "acme/widgets#7#1",
                           "scratch": scratch.path(), "outcome": outcome}))
    };
    assert_eq!(classify(&mut plugin, "clean"), json!({"outcome": "clean"}));
    assert_eq!(
        classify(&mut plugin, "failed"),
        json!({"outcome": "failed"})
    );
    std::fs::write(scratch.path().join("park"), b"").unwrap();
    assert_eq!(classify(&mut plugin, "failed"), json!({"outcome": "park"}));
    assert_eq!(classify(&mut plugin, "clean"), json!({"outcome": "park"}));
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
    let issue = fake.issue_state(REPO, 7);
    assert_eq!(issue.state, "closed");
    assert!(!issue.labels.contains(&"afkd/working".to_string()));
    assert!(markers(&fake, 7).is_empty());
    plugin.finish();
}

#[test]
fn a_failed_finish_runs_on_fail_and_keeps_a_human_assignee() {
    let fake = FakeGitea::start(ME);
    fake.define_label(REPO, "afkd/working", false);
    fake.issue(REPO, 7, TITLE, BODY, &["afkd/ready"], &["alice"]);
    let mut plugin = Plugin::armed(settings(&fake));
    let unit = plugin.poll()["unit"].clone();
    assert_eq!(fake.issue_state(REPO, 7).assignees, ["alice", ME]);
    let reply = finish(
        &mut plugin,
        &unit,
        "failed",
        facts("fault", Some("cargo test: 3 failed")),
    );
    assert_eq!(reply, json!({"ok": true}));
    let issue = fake.issue_state(REPO, 7);
    assert_eq!(issue.assignees, ["alice"], "only afkd let go");
    assert_eq!(issue.state, "open");
    assert!(!issue.labels.contains(&"afkd/working".to_string()));
    assert!(markers(&fake, 7).is_empty());
    plugin.finish();
}

/// A park swaps the claim for the awaiting label, lets go of the issue without closing
/// it, and runs `on_park` with the run's facts substituted: 168000 ms is `2m48s`, 0.4217
/// is `$0.42`, and the run name is afkd's.
#[test]
fn a_park_finish_parks_and_substitutes_the_run_facts() {
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
    assert!(issue.labels.contains(&"afkd/awaiting-reply".to_string()));
    assert!(!issue.labels.contains(&"afkd/claimed".to_string()));
    assert!(issue.assignees.is_empty());
    assert_eq!(issue.state, "open");
    let said: Vec<String> = fake.comments(REPO, 7).into_iter().map(|c| c.body).collect();
    assert_eq!(
        said,
        ["Parked after 2m48s for $0.42 — log: .afkd/runs/260925-095800-issue-7-1/run.log"]
    );

    // A human answers, and the parked issue re-arms with the answer in its brief.
    fake.advance(600);
    fake.comment(
        REPO,
        7,
        "陳大文",
        "Use exponential backoff, capped at 30s.",
        0,
    );
    let unit = plugin.poll()["unit"].clone();
    let brief = unit["files"][0]["text"].as_str().unwrap();
    assert!(
        brief.ends_with(
            "\n\n## New comments\n\n**陳大文:** Use exponential backoff, capped at 30s.\n"
        ),
        "{brief}"
    );
    assert!(!fake
        .issue_state(REPO, 7)
        .labels
        .contains(&"afkd/awaiting-reply".to_string()));
    plugin.finish();
}

/// A terminal lifecycle that does not land — `on_done` adds a label the repo never
/// defined, which Gitea drops under a 200 — still answers `ok`, since afkd would crash the
/// service otherwise. The claim is released at once so the next poll retries the issue;
/// the second time the same issue fails that way, it is left claimed for a human.
#[test]
fn an_undelivered_finish_releases_once_then_leaves_it() {
    let fake = forge();
    let mut settings = settings(&fake);
    settings["on_done"] = json!({"label_add": ["undefined"]});
    let mut plugin = Plugin::armed(settings);

    let unit = plugin.poll()["unit"].clone();
    assert_eq!(
        finish(&mut plugin, &unit, "clean", facts("proceed", None)),
        json!({"ok": true})
    );
    assert!(!fake
        .issue_state(REPO, 7)
        .labels
        .contains(&"afkd/claimed".to_string()));
    let key = unit["key"].as_str().unwrap();
    let stderr = plugin.stderr_soon("releasing the claim");
    assert!(
        stderr.contains(&format!(
            "afkd-gitea: could not deliver the terminal lifecycle for {key}; releasing the \
             claim so the next poll retries it"
        )),
        "{stderr}"
    );

    let unit = plugin.poll()["unit"].clone();
    assert_eq!(
        finish(&mut plugin, &unit, "clean", facts("proceed", None)),
        json!({"ok": true})
    );
    assert!(fake
        .issue_state(REPO, 7)
        .labels
        .contains(&"afkd/claimed".to_string()));
    let stderr = plugin.stderr_soon("failed twice");
    assert!(
        stderr.contains("the terminal lifecycle for acme/widgets#7 failed twice; leaving the claim in place for a human"),
        "{stderr}"
    );
    assert_eq!(plugin.poll(), json!({"fire": false}));
    plugin.finish();
}

/// On the `discuss_with` path a turn that said nothing ends with exactly one backstop,
/// so afkd is the last speaker and the issue does not re-fire.
#[test]
fn a_silent_discuss_turn_posts_one_backstop() {
    let fake = forge();
    fake.comment(REPO, 7, "álvaro", "Exponential, please — see §4 🙏", 120);
    let mut settings = settings(&fake);
    settings["discuss_with"] = json!(["anyone"]);
    settings["on_done"] = json!({"label_remove": ["afkd/working"]});
    let mut plugin = Plugin::armed(settings);
    let unit = plugin.poll()["unit"].clone();
    finish(&mut plugin, &unit, "clean", facts("proceed", None));
    let mine: Vec<String> = fake
        .comments(REPO, 7)
        .into_iter()
        .filter(|c| c.author == ME)
        .map(|c| c.body)
        .collect();
    assert_eq!(mine, ["reviewed, nothing to add"]);
    // Handed back by a human, the issue stays quiet: afkd spoke last.
    fake.unlabel(REPO, 7, "afkd/claimed");
    assert_eq!(plugin.poll(), json!({"fire": false}));
    plugin.finish();
}

// --- release ---

/// The three shapes a claim-journal key comes in: one the built-in wrote releases the
/// label and deletes its marker; the pre-marker two-part shape names nothing (`null`, so
/// afkd forgets it); a location that is no repo is kept to retry (`false`).
#[test]
fn release_reads_the_three_key_shapes() {
    let fake = forge();
    fake.issue(
        REPO,
        9,
        "Crashed mid-run",
        "",
        &["afkd/ready", "afkd/claimed"],
        &[ME],
    );
    fake.comment_with_id(
        6744,
        REPO,
        9,
        ME,
        "[afkd-claim] owner=björn-öst[bot] renewal=4",
        900,
    );
    let mut plugin = Plugin::armed(settings(&fake));
    assert_eq!(
        plugin.call(json!({"call": "release", "key": "acme/widgets#9#6744"})),
        json!({"released": true})
    );
    let issue = fake.issue_state(REPO, 9);
    assert!(!issue.labels.contains(&"afkd/claimed".to_string()));
    assert_eq!(issue.assignees, [ME], "a release never touches assignees");
    assert!(markers(&fake, 9).is_empty());
    assert_eq!(
        plugin.call(json!({"call": "release", "key": "acme/widgets#9"})),
        json!({"released": null})
    );
    assert_eq!(
        plugin.call(json!({"call": "release", "key": "no-slash#9#1"})),
        json!({"released": false})
    );
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
    assert_eq!(
        plugin.call(json!({"call": "renew", "key": unit["key"], "renewal": 1})),
        json!({"ok": false})
    );
    plugin.finish();
}

// --- the line budget ---

/// A 100 KiB issue body cannot cross in one 64 KiB line: the brief is cut, the line fits
/// and decodes, and the brief says where the rest is.
#[test]
fn an_oversized_brief_is_cut_to_fit_one_line() {
    let fake = FakeGitea::start(ME);
    fake.define_label(REPO, "afkd/working", false);
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
        brief.ends_with("read the whole issue with the gitea skill]"),
        "{}",
        &brief[brief.len() - 200..]
    );
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
