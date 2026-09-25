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
/// built-in's own sentence — a kind this plugin does not provide, a block the manifest
/// cannot refuse (for either kind), and a protocol from a later afkd.
#[test]
fn hello_refuses_what_it_cannot_arm_with() {
    let fake = forge();
    let mut both = settings(&fake);
    both["org"] = json!("acme");
    let mut claim_cost = settings(&fake);
    claim_cost["on_claim"] = json!({"comment": ["claimed; budget @{run:cost}"]});
    let mut pr_both = pr_settings(&fake);
    pr_both["org"] = json!("acme");
    for (kind, proto, settings, sentence) in [
        (
            "gitlab",
            1,
            settings(&fake),
            "kind `gitlab` is not provided by @afkd/gitea",
        ),
        (
            "gitea_pr_review",
            1,
            pr_both,
            "trigger gitea_pr_review: setting `org`: a gitea trigger takes exactly one of \
             `repo` or `org` (both were set)",
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

// --- gitea_pr_review ---

const PR_KIND: &str = "gitea_pr_review";
const HEAD: &str = "feature/retry-backoff";
/// A review comment as a human writes one: wide text, an emoji, a fenced block, and a
/// trailing newline the brief trims.
const ASK: &str =
    "看起来不对 🚨 — the backoff never caps.\n\n```rust\nlet backoff = Duration::ZERO;\n```\n";

/// The bot's own PR #7 mid-review: the bot said it pushed a fix ten minutes ago, then
/// 陳大文 commented and carol reviewed. Returns the fake and carol's review id.
fn pr_forge() -> (FakeGitea, u64) {
    let fake = FakeGitea::start(ME);
    fake.define_label(REPO, "afkd/working", false);
    fake.pull(REPO, 7, "Cap the retry backoff", ME, HEAD);
    fake.comment(REPO, 7, ME, "Pushed a fix.", 600);
    fake.comment(REPO, 7, "陳大文", ASK, 300);
    let review = fake.review(REPO, 7, "carol", 200);
    (fake, review)
}

/// A full `gitea_pr_review` block, lowered as afkd lowers it: `author_me` a bare flag,
/// the lifecycle blocks as objects, and afkd's own keys along for the ride.
fn pr_settings(fake: &FakeGitea) -> Value {
    json!({
        "base_url": fake.base_url(),
        "repo": REPO,
        "token": TOKEN,
        "author_me": true,
        "poll_interval": "2m",
        "max_attempts": "2",
        "on_claim": {"assign_me": [true], "label_add": ["afkd/working"]},
        "on_done": {"label_remove": ["afkd/working"]},
        "on_fail": {"label_remove": ["afkd/working"], "unassign": [true]},
    })
}

/// The PR kind lists what it answers: no `classify` — the built-in keeps the spine's
/// default, so a PR never parks — and no `attempt_failed`.
#[test]
fn a_pr_hello_arms_and_lists_release_renew_comments() {
    let (fake, _) = pr_forge();
    let mut plugin = Plugin::spawn();
    assert_eq!(
        plugin.hello(PR_KIND, pr_settings(&fake)),
        json!({"ok": true, "proto": 1, "calls": ["release", "renew", "comments"]})
    );
    assert!(fake.seen().is_empty(), "hello touches no forge");
    plugin.finish();
}

/// The won race over a PR with new human feedback hands over exactly the unit the
/// built-in would have run: its journal key and session thread, the claim-time comment
/// ids, its identity, the five env names, and the review brief beside `pr/number`. The
/// forge then carries the claim's status — only `afkd/claimed` is created, since a PR
/// never parks — and a second poll does not claim the same PR again.
#[test]
fn a_won_pr_race_hands_over_the_built_ins_unit() {
    let (fake, review) = pr_forge();
    let mut plugin = Plugin::armed_as(PR_KIND, pr_settings(&fake));
    let reply = plugin.poll();
    assert_eq!(reply["fire"], true, "{}", plugin.stderr());
    let thread = fake.comments(REPO, 7);
    let marker = markers(&fake, 7);
    assert_eq!(marker.len(), 1, "one marker, ours");
    assert_eq!(
        reply["unit"],
        json!({
            "id": "7",
            "key": format!("acme/widgets#7#{}", marker[0]),
            "thread": "acme/widgets#7",
            "seen": [thread[0].id.to_string(), thread[1].id.to_string()],
            "self": ME,
            "env": {
                "GITEA_BASE_URL": fake.base_url(),
                "GITEA_PR_BRANCH": HEAD,
                "GITEA_PR_NUMBER": "7",
                "GITEA_REPO": REPO,
                "GITEA_TOKEN": TOKEN,
            },
            "files": [
                {"path": "task.md", "text": format!(
                    "Address review feedback on PR #7.\n\n## New feedback\n\n**陳大文:** {}\n\n\
                     **carol:** (review {review})\n",
                    ASK.trim_end()
                )},
                {"path": "pr/number", "text": "7"},
            ],
        })
    );

    let issue = fake.issue_state(REPO, 7);
    assert_eq!(issue.labels, ["afkd/claimed", "afkd/working"]);
    assert_eq!(issue.assignees, [ME]);
    let managed: Vec<(String, bool)> = fake
        .labels(REPO)
        .into_iter()
        .filter(|l| l.name.starts_with("afkd/") && l.name != "afkd/working")
        .map(|l| (l.name, l.exclusive))
        .collect();
    assert_eq!(managed, [("afkd/claimed".into(), false)]);
    // Gitea is asked for the open PRs, and for this PR's reviews.
    let seen = fake.seen();
    let list = seen
        .iter()
        .find(|r| r.path == "/api/v1/repos/acme/widgets/pulls")
        .expect("the pulls were listed");
    assert_eq!(list.query, [("state".to_string(), "open".to_string())]);
    assert!(seen
        .iter()
        .any(|r| r.method == "GET" && r.path == "/api/v1/repos/acme/widgets/pulls/7/reviews"));
    // The feedback is still unanswered, so the PR is a candidate again — but the live
    // marker out-orders the re-claim, which takes its own marker back.
    assert_eq!(plugin.poll(), json!({"fire": false}));
    assert_eq!(markers(&fake, 7), marker, "only the first marker remains");
    plugin.finish();
}

/// A live rival marker posted a minute earlier out-orders ours: nothing is handed over,
/// our marker is taken back, and no status is written.
#[test]
fn a_lost_pr_race_hands_over_nothing() {
    let (fake, _) = pr_forge();
    fake.comment_with_id(
        424_242,
        REPO,
        7,
        "autocoder",
        "[afkd-claim] owner=autocoder",
        60,
    );
    let mut plugin = Plugin::armed_as(PR_KIND, pr_settings(&fake));
    assert_eq!(plugin.poll(), json!({"fire": false}));
    assert_eq!(
        markers(&fake, 7),
        [424_242],
        "only the rival's marker is left"
    );
    let issue = fake.issue_state(REPO, 7);
    assert!(issue.labels.is_empty(), "{:?}", issue.labels);
    assert!(issue.assignees.is_empty());
    plugin.finish();
}

/// A PR whose feedback the bot already answered does not fire — a review older than its
/// reply, and a rival's claim marker newer than it, included: a marker is not feedback.
/// The idle scan writes nothing and creates no label.
#[test]
fn a_pr_with_no_new_feedback_does_not_fire() {
    let fake = FakeGitea::start(ME);
    fake.pull(REPO, 7, "Cap the retry backoff", ME, HEAD);
    fake.comment(REPO, 7, "陳大文", ASK, 300);
    fake.review(REPO, 7, "carol", 200);
    fake.comment(REPO, 7, ME, "Capped at 30s — see the new commit.", 100);
    fake.comment(REPO, 7, "autocoder", "[afkd-claim] owner=autocoder", 50);
    let mut plugin = Plugin::armed_as(PR_KIND, pr_settings(&fake));
    assert_eq!(plugin.poll(), json!({"fire": false}));
    assert!(
        fake.seen().iter().all(|r| r.method == "GET"),
        "an idle scan writes nothing: {:?}",
        fake.seen()
    );
    assert!(fake.labels(REPO).is_empty());
    plugin.finish();
}

/// `author_me` passes over a PR someone else opened without reading its thread; without
/// the flag the same PR is anyone's to review.
#[test]
fn author_me_passes_over_someone_elses_pr() {
    let fake = FakeGitea::start(ME);
    fake.define_label(REPO, "afkd/working", false);
    fake.pull(REPO, 8, "Add jitter", "alice", "alice/jitter");
    fake.comment(REPO, 8, "陳大文", ASK, 300);
    let mut plugin = Plugin::armed_as(PR_KIND, pr_settings(&fake));
    assert_eq!(plugin.poll(), json!({"fire": false}));
    let seen = fake.seen();
    assert!(seen.iter().all(|r| r.method == "GET"), "{seen:?}");
    assert!(
        !seen.iter().any(|r| r.path.contains("/8/")),
        "the filter precedes the thread reads: {seen:?}"
    );
    plugin.finish();

    let mut settings = pr_settings(&fake);
    settings.as_object_mut().unwrap().remove("author_me");
    let mut plugin = Plugin::armed_as(PR_KIND, settings);
    let unit = plugin.poll()["unit"].clone();
    assert_eq!(unit["thread"], "acme/widgets#8");
    assert_eq!(unit["env"]["GITEA_PR_BRANCH"], "alice/jitter");
    plugin.finish();
}

#[test]
fn a_pr_claim_renews_in_place() {
    let (fake, _) = pr_forge();
    let mut plugin = Plugin::armed_as(PR_KIND, pr_settings(&fake));
    let unit = plugin.poll()["unit"].clone();
    let marker = markers(&fake, 7)[0];
    let before = fake
        .comments(REPO, 7)
        .into_iter()
        .find(|c| c.id == marker)
        .unwrap();
    fake.advance(300);
    assert_eq!(
        plugin.call(json!({"call": "renew", "key": unit["key"], "renewal": 1})),
        json!({"ok": true})
    );
    let after = fake
        .comments(REPO, 7)
        .into_iter()
        .find(|c| c.id == marker)
        .expect("edited, not re-posted");
    assert_eq!(after.body, "[afkd-claim] owner=björn-öst[bot] renewal=1");
    assert_eq!(after.created, before.created);
    assert_eq!(
        after.updated,
        before.updated + 300,
        "the liveness stamp moved"
    );
    assert_eq!(
        plugin.call(json!({"call": "renew", "key": "acme/widgets#8#1", "renewal": 1})),
        json!({"ok": false})
    );
    plugin.finish();
}

/// `comments` over a PR reports what humans said after the claim, as afkd's watch reads
/// it: the claim-time thread the brief was built from is `seen` and never re-sent, the
/// bot's own words and its marker are left out, a second read carries only what is new,
/// and a forge that cannot be read is `null`.
#[test]
fn pr_comments_report_only_what_afkd_has_not_seen() {
    let (fake, _) = pr_forge();
    let mut plugin = Plugin::armed_as(PR_KIND, pr_settings(&fake));
    let unit = plugin.poll()["unit"].clone();
    let read = |plugin: &mut Plugin| plugin.call(json!({"call": "comments", "key": unit["key"]}));

    fake.advance(60);
    let a = fake.comment(REPO, 7, "álvaro", "Also the jitter?\n\n- ±10%\n- ±20%", 0);
    fake.advance(1);
    fake.comment(REPO, 7, ME, "On it.", 0);
    fake.advance(1);
    let b = fake.comment(REPO, 7, "陳大文", "👍 cap looks right", 0);
    assert_eq!(
        read(&mut plugin),
        json!({"comments": [
            {"id": a.to_string(), "author": "álvaro", "author_name": "álvaro",
             "body": "Also the jitter?\n\n- ±10%\n- ±20%", "at": "2026-09-25T09:59:00Z"},
            {"id": b.to_string(), "author": "陳大文", "author_name": "陳大文",
             "body": "👍 cap looks right", "at": "2026-09-25T09:59:02Z"},
        ]})
    );

    fake.advance(30);
    let c = fake.comment(REPO, 7, "carol", "LGTM once CI is green.", 0);
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
    plugin.finish();
}

/// `release` drops a live PR claim — the status label and the marker, never an assignee
/// — and forgets the unit; and a journal key a crashed plugin left behind releases the
/// same way from a fresh one.
#[test]
fn a_pr_release_drops_the_claim() {
    let (fake, _) = pr_forge();
    // An earlier plugin claimed #7 and died mid-run, leaving its journal key.
    let mut crashed = Plugin::armed_as(PR_KIND, pr_settings(&fake));
    let stale = crashed.poll()["unit"]["key"].clone();
    drop(crashed);
    assert_eq!(
        fake.issue_state(REPO, 7).labels,
        ["afkd/claimed", "afkd/working"]
    );

    let mut plugin = Plugin::armed_as(PR_KIND, pr_settings(&fake));
    assert_eq!(
        plugin.call(json!({"call": "release", "key": stale})),
        json!({"released": true})
    );
    let issue = fake.issue_state(REPO, 7);
    assert_eq!(issue.labels, ["afkd/working"]);
    assert_eq!(issue.assignees, [ME], "a release never touches assignees");
    assert!(markers(&fake, 7).is_empty());

    // With the stale marker gone, the unanswered feedback is claimed afresh — and a
    // live unit handed straight back is released and forgotten.
    let unit = plugin.poll()["unit"].clone();
    assert_ne!(unit["key"], stale);
    assert_eq!(
        plugin.call(json!({"call": "release", "key": unit["key"]})),
        json!({"released": true})
    );
    assert!(!fake
        .issue_state(REPO, 7)
        .labels
        .contains(&"afkd/claimed".to_string()));
    assert!(markers(&fake, 7).is_empty());
    assert_eq!(
        plugin.call(json!({"call": "renew", "key": unit["key"], "renewal": 1})),
        json!({"ok": false})
    );
    plugin.finish();
}

/// A clean round runs `on_done`, leaves the PR open — a human's merge ends the loop, not
/// afkd — and takes the marker off. The status label stays, since it is not the gate:
/// the loop is the watermark's. Once the bot has answered, the PR is quiet; the next
/// human word fires it again, with only that word in the brief.
#[test]
fn a_clean_pr_finish_runs_on_done_and_leaves_the_pr_open() {
    let (fake, _) = pr_forge();
    let mut plugin = Plugin::armed_as(PR_KIND, pr_settings(&fake));
    let unit = plugin.poll()["unit"].clone();
    let mut done = facts("proceed", None);
    done["run_name"] = json!("260925-095800-pr-7-1");
    assert_eq!(
        finish(&mut plugin, &unit, "clean", done),
        json!({"ok": true})
    );
    let issue = fake.issue_state(REPO, 7);
    assert_eq!(issue.state, "open");
    assert_eq!(issue.labels, ["afkd/claimed"]);
    assert_eq!(issue.assignees, [ME], "`on_done` keeps the bot on its PR");
    assert!(markers(&fake, 7).is_empty());

    // The agent answered through the skill during the run.
    fake.advance(60);
    fake.comment(REPO, 7, ME, "Capped at 30s; see 4f2a9c1.", 0);
    assert_eq!(plugin.poll(), json!({"fire": false}));

    fake.advance(60);
    fake.comment(
        REPO,
        7,
        "álvaro",
        "Thanks — one nit: name it `MAX_BACKOFF`.",
        0,
    );
    let unit = plugin.poll()["unit"].clone();
    assert_eq!(
        unit["files"][0]["text"],
        "Address review feedback on PR #7.\n\n## New feedback\n\n\
         **álvaro:** Thanks — one nit: name it `MAX_BACKOFF`.\n"
    );
    plugin.finish();
}

/// A failed round runs `on_fail`: the bot lets go and drops its working label, the PR
/// stays open, and the marker comes off.
#[test]
fn a_failed_pr_finish_runs_on_fail() {
    let (fake, _) = pr_forge();
    let mut plugin = Plugin::armed_as(PR_KIND, pr_settings(&fake));
    let unit = plugin.poll()["unit"].clone();
    assert_eq!(fake.issue_state(REPO, 7).assignees, [ME]);
    let reply = finish(
        &mut plugin,
        &unit,
        "failed",
        facts("fault", Some("cargo test: 3 failed")),
    );
    assert_eq!(reply, json!({"ok": true}));
    let issue = fake.issue_state(REPO, 7);
    assert!(issue.assignees.is_empty());
    assert_eq!(issue.labels, ["afkd/claimed"]);
    assert_eq!(issue.state, "open");
    assert!(markers(&fake, 7).is_empty());
    plugin.finish();
}

/// `classify` is not a call the PR kind lists, so it is refused like any unlisted call
/// — even with a park marker in the scratch directory.
#[test]
fn a_pr_service_refuses_classify() {
    let (fake, _) = pr_forge();
    let mut plugin = Plugin::armed_as(PR_KIND, pr_settings(&fake));
    let scratch = TempDir::new("pr-classify");
    std::fs::write(scratch.path().join("park"), b"").unwrap();
    assert_eq!(
        plugin.call(json!({"call": "classify", "key": "acme/widgets#7#1",
                           "scratch": scratch.path(), "outcome": "failed"})),
        json!({"ok": false})
    );
    let stderr = plugin.stderr_soon("did not list");
    assert!(
        stderr
            .contains("afkd-gitea: afkd sent a call this plugin did not list in its `hello` reply"),
        "{stderr}"
    );
    plugin.finish();
}
