//! The `linear` skill's scripts, run as an agent runs them — `python3` on the script, with the
//! `LINEAR_*` a run carries in its environment — against the stateful fake Linear, whose
//! GraphQL endpoint is the `LINEAR_API_URL` they read. Each leg reads the fake back: what
//! landed where, what a refused call left unwritten, and which file a fetch wrote. Every leg
//! ends with the fake's record of refused requests empty, so each script's every request is
//! held to Linear's schema and to the documents `skills/linear/lib/operations.graphql` holds.
//!
//! A script that does not exist is a `python3` that exits 2, which every leg's success
//! assertion catches, so each leg fails before its script is written.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use common::fake::sdl::Sdl;
use common::fake::{
    plugin_documents, png, skill_documents, stamp, Author, FakeLinear, Seen, API_KEY,
    UPLOAD_CACHE_CONTROL, UPLOAD_HEADER,
};
use common::TempDir;

/// The queue in "Up Next", top to bottom: wide CJK, an emoji, a ` - ` clause and a backticked
/// identifier, as this board's titles carry them.
const QUEUE: [&str; 4] = [
    "fix(linear): 修复 the claim race 🚨 - a rival's claim no longer wins",
    "feat(lang): `in` reads a list's membership - x in xs, not contains(xs, x)",
    "fix(tui): a wide glyph — 漢字 — no longer splits the footer",
    "chore(web): the `check:browser` gate runs headless",
];

/// The run's own issue, in progress.
const TITLE: &str = "feat(linear): 跟进 the skill a run talks to Linear through 🔗";

/// A follow-up's body: a multi-line description with a fence, the Never park block and wide
/// glyphs.
const BODY: &str = "Split off from ENG-105: the red is not ours.\n\n\
                    ## Problem\n\n`cargo test -p linear-drift` reds on 488931d:\n\n\
                    ```\nthe_claim_settles ... FAILED\n```\n\n\
                    ## Never park (owner, 2026-09-30)\n\n\
                    Do **not** use `ask.py` on this issue — 看 the block.\n\n\
                    ## Acceptance\n\n1. The drift gate is green.\n";

/// A comment: wide CJK, an emoji, a fence, quotes and backticks over several lines.
const COMMENT: &str = "看起来不对 🚨 — it resets on every 401:\n\n```\nPOST /graphql 401\n```\n\n\
                       — `backoff` is \"0\", not 30s";

/// The operations that write, as the fake records them.
const WRITES: [&str; 8] = [
    "CommentCreate",
    "IssueUpdate",
    "IssueAddLabel",
    "IssueRemoveLabel",
    "NewIssue",
    "IssueArchive",
    "IssueUnarchive",
    "FileUpload",
];

/// The proxy variables urllib honours, which would send a loopback request elsewhere.
const PROXIES: [&str; 6] = [
    "http_proxy",
    "HTTP_PROXY",
    "https_proxy",
    "HTTPS_PROXY",
    "all_proxy",
    "ALL_PROXY",
];

/// The team every leg starts from: five states, one a wide name, the queue in "Up Next", and
/// the run's own issue in "In Progress".
struct Team {
    fake: FakeLinear,
    /// The run's issue.
    issue: String,
    /// The queue's issue ids, top to bottom.
    queue: Vec<String>,
    /// "Up Next"'s id.
    up_next: String,
    /// The run's scratch dir.
    scratch: TempDir,
}

fn team() -> Team {
    let fake = FakeLinear::start();
    let mut up_next = String::new();
    for (state, kind) in [
        ("Backlog", "backlog"),
        ("Up Next", "unstarted"),
        ("In Progress", "started"),
        ("審查 Review 👀", "started"),
        ("Done", "completed"),
    ] {
        let id = fake.state(state, kind);
        if state == "Up Next" {
            up_next = id;
        }
    }
    let queue = QUEUE.iter().map(|t| fake.issue("Up Next", t, "")).collect();
    let issue = fake.issue("In Progress", TITLE, BODY);
    Team {
        fake,
        issue,
        queue,
        up_next,
        scratch: TempDir::new("skill"),
    }
}

impl Team {
    /// An issue's identifier.
    fn id(&self, issue: &str) -> String {
        self.fake.identifier(issue)
    }

    /// Run one script on the run's issue, with the env a run carries and nothing else of
    /// Linear's.
    fn run(&self, script: &str, args: &[&str]) -> Output {
        self.run_with(script, args, &[])
    }

    /// [`run`](Self::run), each of `env` set to its value or, with none, unset.
    fn run_with(&self, script: &str, args: &[&str], env: &[(&str, Option<&str>)]) -> Output {
        let mut cmd = Command::new("python3");
        cmd.arg(skill_dir().join(script)).args(args);
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("LINEAR_") {
                cmd.env_remove(name);
            }
        }
        for name in PROXIES {
            cmd.env_remove(name);
        }
        cmd.env(
            "LINEAR_API_URL",
            format!("{}/graphql", self.fake.base_url()),
        )
        .env("LINEAR_AUTHORIZATION", API_KEY)
        .env("LINEAR_TEAM_ID", self.fake.team_id())
        .env("LINEAR_ISSUE_ID", &self.issue)
        .env("LINEAR_ISSUE", self.id(&self.issue))
        .env("AFKD_SCRATCH_DIR", self.scratch.path())
        .env("PYTHONDONTWRITEBYTECODE", "1");
        for (name, value) in env {
            match value {
                Some(value) => cmd.env(name, value),
                None => cmd.env_remove(name),
            };
        }
        cmd.output().expect("run python3")
    }

    /// The writes the fake was sent: every mutation, and every PUT.
    fn writes(&self) -> Vec<Seen> {
        self.fake
            .seen()
            .into_iter()
            .filter(|s| s.method == "PUT" || WRITES.contains(&s.operation.as_str()))
            .collect()
    }

    /// The requests of the operation `name`.
    fn sent(&self, name: &str) -> Vec<Seen> {
        self.fake
            .seen()
            .into_iter()
            .filter(|s| s.operation == name)
            .collect()
    }

    /// End the leg: no request the fake refused.
    fn done(&self) {
        assert_eq!(
            self.fake.invalid_requests(),
            Vec::<String>::new(),
            "requests Linear's schema refuses"
        );
    }
}

fn skill_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("skills/linear")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// The stdout of a run that succeeded, failing the leg with its stderr otherwise.
fn ok(out: &Output) -> String {
    assert!(
        out.status.success(),
        "exit {:?}:\n{}{}",
        out.status.code(),
        stdout(out),
        stderr(out)
    );
    stdout(out)
}

/// Assert a run failed with `code`, its stderr holding `needle`.
fn refused(out: &Output, code: i32, needle: &str) {
    assert_eq!(
        out.status.code(),
        Some(code),
        "{}{}",
        stdout(out),
        stderr(out)
    );
    assert!(
        stderr(out).contains(needle),
        "{needle:?} in:\n{}",
        stderr(out)
    );
}

// --- comments -------------------------------------------------------------------------

#[test]
fn list_comments_prints_every_author_oldest_first_with_bodies_verbatim() {
    let t = team();
    let chen = t.fake.user("陳大文", "chen", "chen@acme.example");
    let asked = t.fake.comment(&t.issue, Author::User(chen), COMMENT, 300);
    let plan = "Plan posted ✅\n\n- step one\n- step two";
    t.fake
        .comment(&t.issue, Author::Bot(t.fake.bot_id()), plan, 200);
    let slack = "From Slack: \"ship it\" 🚀";
    t.fake
        .comment(&t.issue, Author::External("ext-7f3a".into()), slack, 100);
    t.fake.mark_edited(&asked);
    let thread = t.fake.comments(&t.issue);
    let at = |n: usize| stamp(thread[n].created);

    let out = ok(&t.run("list_comments.py", &[]));
    assert_eq!(
        out,
        format!(
            "chen · {} (edited)\n{COMMENT}\n\nafkd · {}\n{plan}\n\nÁlvaro (Slack) · {}\n{slack}\n",
            at(0),
            at(1),
            at(2)
        )
    );

    let out = ok(&t.run("list_comments.py", &["--limit", "2"]));
    assert_eq!(
        out,
        format!(
            "afkd · {}\n{plan}\n\nÁlvaro (Slack) · {}\n{slack}\n",
            at(1),
            at(2)
        ),
        "the two newest"
    );
    refused(
        &t.run("list_comments.py", &["--limit", "0"]),
        2,
        "at least 1",
    );
    t.done();
}

#[test]
fn list_comments_reads_every_page_and_says_when_there_are_none() {
    let t = team();
    t.fake.comments_page(3);
    for n in 0..8u64 {
        t.fake.comment(
            &t.issue,
            Author::User(t.fake.key_user()),
            &format!("第{n}条 — note {n}"),
            100 - n,
        );
    }
    let out = ok(&t.run("list_comments.py", &[]));
    let bodies: Vec<&str> = out.lines().filter(|l| l.starts_with('第')).collect();
    let expected: Vec<String> = (0..8).map(|n| format!("第{n}条 — note {n}")).collect();
    assert_eq!(bodies, expected, "every page, oldest first:\n{out}");
    assert_eq!(t.sent("IssueComments").len(), 3, "three pages of three");

    let quiet = t.id(&t.queue[0]);
    let out = ok(&t.run("list_comments.py", &["--issue", &quiet]));
    assert_eq!(out, format!("no comments on issue {quiet}\n"));
    t.done();
}

#[test]
fn post_comment_posts_verbatim_to_the_run_issue_or_another() {
    let t = team();
    let out = ok(&t.run("post_comment.py", &[COMMENT]));
    let me = t.id(&t.issue);
    assert_eq!(out, format!("post_comment.py: posted a comment on {me}\n"));
    let thread = t.fake.comments(&t.issue);
    assert_eq!(thread.len(), 1);
    assert_eq!(thread[0].body, COMMENT);
    assert_eq!(thread[0].author, Author::User(t.fake.key_user()));

    let other = t.id(&t.queue[2]);
    ok(&t.run("post_comment.py", &["see ENG-105 ✅", "--issue", &other]));
    assert_eq!(t.fake.comments(&t.queue[2])[0].body, "see ENG-105 ✅");
    assert_eq!(t.fake.comments(&t.issue).len(), 1);
    t.done();
}

#[test]
fn post_comment_refuses_the_placeholder_and_a_missing_credential_before_any_request() {
    let t = team();
    refused(
        &t.run("post_comment.py", &["<REPLACE WITH YOUR COMMENT>"]),
        2,
        "placeholder",
    );
    refused(&t.run("post_comment.py", &["  \n"]), 2, "empty");
    refused(
        &t.run_with(
            "post_comment.py",
            &[COMMENT],
            &[("LINEAR_AUTHORIZATION", None)],
        ),
        1,
        "LINEAR_AUTHORIZATION must be set",
    );
    refused(
        &t.run_with("post_comment.py", &[COMMENT], &[("LINEAR_ISSUE_ID", None)]),
        1,
        "no issue (set LINEAR_ISSUE_ID or pass --issue)",
    );
    assert!(t.fake.seen().is_empty(), "{:?}", t.fake.seen());
    t.done();
}

#[test]
fn a_rate_limited_write_says_the_wait_and_is_never_retried() {
    let t = team();
    t.fake.rate_limit("CommentCreate", 1, 7);
    refused(
        &t.run("post_comment.py", &[COMMENT]),
        1,
        "post_comment.py: post comment: Linear rate limited it; retry after 7s",
    );
    assert_eq!(t.sent("CommentCreate").len(), 1, "sent once");
    assert!(t.fake.comments(&t.issue).is_empty());
    t.done();
}

#[test]
fn ask_posts_the_question_then_parks_the_run() {
    let t = team();
    let out = ok(&t.run("ask.py", &[COMMENT]));
    let me = t.id(&t.issue);
    assert_eq!(
        out,
        format!("ask.py: asked on {me} and parked the run awaiting a reply\n")
    );
    assert_eq!(t.fake.comments(&t.issue)[0].body, COMMENT);
    assert!(t.scratch.path().join("park").is_file(), "the park marker");
    t.done();
}

#[test]
fn ask_parks_nothing_it_did_not_ask_and_asks_nothing_it_cannot_park() {
    let t = team();
    refused(
        &t.run_with("ask.py", &[COMMENT], &[("AFKD_SCRATCH_DIR", None)]),
        1,
        "AFKD_SCRATCH_DIR must be set",
    );
    assert!(t.fake.seen().is_empty(), "nothing posted");
    refused(
        &t.run("ask.py", &["<REPLACE WITH YOUR QUESTIONS>"]),
        2,
        "placeholder",
    );
    assert!(t.fake.seen().is_empty());

    t.fake.fail("CommentCreate", 500);
    refused(
        &t.run("ask.py", &[COMMENT]),
        1,
        "ask.py: post question: Linear refused it (500): injected",
    );
    assert!(!t.scratch.path().join("park").exists(), "no marker");
    t.done();
}

// --- the description and the title ---------------------------------------------------

#[test]
fn read_description_prints_it_verbatim_and_an_empty_one_as_an_empty_line() {
    let t = team();
    assert_eq!(ok(&t.run("read_description.py", &[])), format!("{BODY}\n"));
    let bare = t.id(&t.queue[0]);
    assert_eq!(ok(&t.run("read_description.py", &["--issue", &bare])), "\n");
    t.done();
}

#[test]
fn set_description_replaces_it_from_a_file_and_takes_exactly_one_source() {
    let t = team();
    let file = t.scratch.path().join("desc.md");
    let rewritten = format!("{BODY}\n<!-- afkd -->\nPlan: 修复 it ✅\n");
    std::fs::write(&file, &rewritten).expect("write the description");
    let out = ok(&t.run(
        "set_description.py",
        &["--desc-file", &file.display().to_string()],
    ));
    let me = t.id(&t.issue);
    assert_eq!(
        out,
        format!("set_description.py: replaced the description of {me}\n")
    );
    assert_eq!(t.fake.description_of(&t.issue), rewritten);
    assert_eq!(
        ok(&t.run("read_description.py", &[])),
        format!("{rewritten}\n")
    );

    let seen = t.fake.seen().len();
    for args in [&["--desc", "x", "--desc-file", "/tmp/x"][..], &[][..]] {
        refused(&t.run("set_description.py", args), 2, "usage");
    }
    assert_eq!(t.fake.seen().len(), seen, "no request");
    t.done();
}

#[test]
fn set_title_retitles_and_refuses_the_placeholder() {
    let t = team();
    let title = "fix(linear): 修复 the skill - `ask.py` parks the run";
    let out = ok(&t.run("set_title.py", &[title]));
    assert_eq!(out, format!("set_title.py: retitled {}\n", t.id(&t.issue)));
    assert_eq!(t.fake.title_of(&t.issue), title);

    let seen = t.fake.seen().len();
    refused(
        &t.run("set_title.py", &["<REPLACE WITH YOUR NEW TITLE>"]),
        2,
        "placeholder",
    );
    assert_eq!(t.fake.seen().len(), seen);
    t.done();
}

// --- moving, archiving and filing issues -----------------------------------------------

#[test]
fn move_issue_lands_beside_an_anchor_or_at_an_end() {
    let t = team();
    let (me, q) = (t.issue.clone(), t.queue.clone());
    let below = t.id(&q[1]);
    let out = ok(&t.run("move_issue.py", &["Up Next", "--below", &below]));
    assert_eq!(
        out,
        format!(
            "move_issue.py: moved {} to Up Next (below {below})\n",
            t.id(&me)
        )
    );
    assert_eq!(
        t.fake.in_state("Up Next"),
        [&q[0], &q[1], &me, &q[2], &q[3]].map(String::clone)
    );
    assert!(t.fake.in_state("In Progress").is_empty());

    // Within its own state, its own place left out; the anchor by id this time.
    ok(&t.run("move_issue.py", &["Up Next", "--above", &q[0]]));
    assert_eq!(t.fake.in_state("Up Next")[0], me);
    ok(&t.run("move_issue.py", &["Up Next", "--below", &t.id(&q[3])]));
    assert_eq!(t.fake.in_state("Up Next").last(), Some(&me));
    ok(&t.run("move_issue.py", &["Up Next", "--above", &t.id(&q[2])]));
    assert_eq!(
        t.fake.in_state("Up Next"),
        [&q[0], &q[1], &me, &q[2], &q[3]].map(String::clone)
    );

    // The default is the bottom, as the plugin's `move_to`; --pos top the top. A wide state
    // name, and a state named by its id.
    ok(&t.run("move_issue.py", &["審查 Review 👀"]));
    assert_eq!(t.fake.state_of(&me), "審查 Review 👀");
    ok(&t.run("move_issue.py", &[&t.up_next, "--pos", "top"]));
    assert_eq!(t.fake.in_state("Up Next")[0], me);
    ok(&t.run("move_issue.py", &["Up Next"]));
    assert_eq!(t.fake.in_state("Up Next").last(), Some(&me));
    t.done();
}

#[test]
fn move_issue_writes_nothing_it_cannot_place() {
    let t = team();
    let parked = t
        .fake
        .issue("Backlog", "refactor(daemon): the run dir — ラン", "");
    let me = t.id(&t.issue);
    for (args, code, needle) in [
        (
            vec!["Up Next", "--above", &t.id(&parked)],
            1,
            format!("no issue {} in state 'Up Next'", t.id(&parked)),
        ),
        (
            vec!["Up Next", "--below", "ENG-999"],
            1,
            "no issue ENG-999 in state 'Up Next'".to_string(),
        ),
        (vec!["Nowhere"], 1, "no state 'Nowhere' on team".to_string()),
        (
            vec!["Up Next", "--below", &me],
            2,
            format!("cannot place {me} relative to itself"),
        ),
        (
            vec!["In Progress", "--above", &me],
            2,
            format!("cannot place {me} relative to itself"),
        ),
        (
            vec!["In Progress", "--above", &t.issue],
            2,
            format!("cannot place {} relative to itself", t.issue),
        ),
        (
            vec!["Up Next", "--pos", "top", "--above", &t.queue[0]],
            2,
            "not allowed with argument".to_string(),
        ),
    ] {
        refused(&t.run("move_issue.py", &args), code, &needle);
    }
    let out = t.run("move_issue.py", &["Nowhere"]);
    assert!(
        stderr(&out).contains("'Up Next'") && stderr(&out).contains("'審查 Review 👀'"),
        "an unknown state lists the ones there are:\n{}",
        stderr(&out)
    );
    assert!(t.writes().is_empty(), "{:?}", t.writes());
    assert_eq!(t.fake.state_of(&t.issue), "In Progress");
    t.done();
}

#[test]
fn archive_issue_archives_and_unarchive_restores() {
    let t = team();
    let me = t.id(&t.issue);
    let out = ok(&t.run("archive_issue.py", &[]));
    assert_eq!(out, format!("archive_issue.py: archived {me}\n"));
    assert!(t.fake.archived(&t.issue));
    let out = ok(&t.run("archive_issue.py", &["--unarchive", "--issue", &me]));
    assert_eq!(out, format!("archive_issue.py: restored {me}\n"));
    assert!(!t.fake.archived(&t.issue));
    t.done();
}

#[test]
fn create_issue_files_into_a_state_beside_an_anchor() {
    let t = team();
    let file = t.scratch.path().join("follow-up.md");
    std::fs::write(&file, BODY).expect("write the description");
    let title = "fix(linear): 跟进 a drift red that is not ours - `wire.rs` reds on main";
    let below = t.id(&t.queue[1]);
    let out = ok(&t.run(
        "create_issue.py",
        &[
            title,
            "--desc-file",
            &file.display().to_string(),
            "--state",
            "Up Next",
            "--below",
            &below,
        ],
    ));
    let new = t.fake.find_by_title(title).expect("filed");
    assert_eq!(
        out,
        format!(
            "create_issue.py: created {} \"{title}\" {}\n",
            t.id(&new),
            t.fake.url(&new)
        )
    );
    let q = &t.queue;
    assert_eq!(
        t.fake.in_state("Up Next"),
        [&q[0], &q[1], &new, &q[2], &q[3]].map(String::clone)
    );
    assert_eq!(t.fake.description_of(&new), BODY);
    assert_eq!(t.fake.parent_of(&new), None);

    // At the ends, and at the bottom by default.
    for (args, top) in [
        (vec!["--above", q[0].as_str()], true),
        (vec!["--pos", "top"], true),
        (vec![], false),
    ] {
        let title = format!("chore: filed {args:?} 📥");
        let mut all = vec![title.as_str(), "--state", "Up Next"];
        all.extend(args);
        ok(&t.run("create_issue.py", &all));
        let filed = t.fake.find_by_title(&title).expect("filed");
        let state = t.fake.in_state("Up Next");
        let landed = if top { state.first() } else { state.last() };
        assert_eq!(landed, Some(&filed), "{all:?}");
    }
    t.done();
}

#[test]
fn create_issue_files_a_sub_issue_or_into_the_default_state() {
    let t = team();
    let me = t.id(&t.issue);
    ok(&t.run(
        "create_issue.py",
        &[
            "Answer the question 🙋",
            "--parent",
            &me,
            "--desc",
            "Asked on the run.",
        ],
    ));
    let child = t
        .fake
        .find_by_title("Answer the question 🙋")
        .expect("filed");
    assert_eq!(t.fake.parent_of(&child), Some(t.issue.clone()));
    assert_eq!(t.fake.state_of(&child), "Backlog", "Linear's default state");
    let input = &t.sent("NewIssue")[0].variables["input"];
    assert!(input.get("stateId").is_none() && input.get("sortOrder").is_none());
    assert_eq!(input["description"], "Asked on the run.");

    let seen = t.fake.seen().len();
    refused(
        &t.run(
            "create_issue.py",
            &["Look again 👀", "--above", &t.queue[0]],
        ),
        2,
        "place it in a --state",
    );
    refused(
        &t.run("create_issue.py", &["<REPLACE WITH YOUR ISSUE TITLE>"]),
        2,
        "placeholder",
    );
    refused(
        &t.run(
            "create_issue.py",
            &["x", "--state", "Up Next", "--above", "ENG-999"],
        ),
        1,
        "no issue ENG-999 in state 'Up Next'",
    );
    assert_eq!(t.writes().len(), 1, "only the sub-issue was filed");
    assert!(t.fake.seen().len() > seen, "the anchor was read");
    t.done();
}

// --- sub-issues ------------------------------------------------------------------------

#[test]
fn list_sub_issues_prints_each_with_its_state_in_filing_order() {
    let t = team();
    let me = t.id(&t.issue);
    assert_eq!(
        ok(&t.run("list_sub_issues.py", &[])),
        format!("no sub-issues on issue {me}\n")
    );
    let cap = t.fake.issue("Up Next", "Cap the backoff at 30s ⏱", "");
    let docs = t.fake.issue("Done", "文档 the `Retry-After` rule", "");
    t.fake.set_parent(&cap, &t.issue);
    t.fake.set_parent(&docs, &t.issue);
    assert_eq!(
        ok(&t.run("list_sub_issues.py", &[])),
        format!(
            "- {} (Up Next) Cap the backoff at 30s ⏱\n- {} (Done) 文档 the `Retry-After` rule\n",
            t.id(&cap),
            t.id(&docs)
        )
    );
    t.done();
}

#[test]
fn move_sub_issue_moves_a_child_and_refuses_any_other_issue() {
    let t = team();
    let cap = t.fake.issue("Up Next", "Cap the backoff at 30s ⏱", "");
    t.fake.set_parent(&cap, &t.issue);
    let out = ok(&t.run("move_sub_issue.py", &[&t.id(&cap), "Done"]));
    assert_eq!(
        out,
        format!("move_sub_issue.py: moved {} to Done\n", t.id(&cap))
    );
    assert_eq!(t.fake.state_of(&cap), "Done");

    let before = t.writes().len();
    let stranger = t.id(&t.queue[0]);
    refused(
        &t.run("move_sub_issue.py", &[&stranger, "Done"]),
        1,
        &format!("{stranger} is no sub-issue of {}", t.id(&t.issue)),
    );
    assert_eq!(t.writes().len(), before, "nothing written");
    assert_eq!(t.fake.state_of(&t.queue[0]), "Up Next");
    t.done();
}

// --- labels ----------------------------------------------------------------------------

/// The team's labels: two of the team's and one of the workspace's, the run's issue carrying
/// the second.
fn labelled(t: &Team) -> [String; 3] {
    let ready = t.fake.team_label("afkd/ready", "#5e6ad2");
    let bug = t.fake.workspace_label("Bug 🐛", "#eb5757");
    let waiting = t.fake.team_label("Väntar på svar", "#f2c94c");
    t.fake.put_label(&t.issue, &waiting);
    [ready, bug, waiting]
}

#[test]
fn list_labels_marks_the_workspace_s_and_the_issue_s() {
    let t = team();
    assert_eq!(
        ok(&t.run("list_labels.py", &[])),
        format!("no labels on team {}\n", t.fake.team_id())
    );
    let [ready, bug, waiting] = labelled(&t);
    assert_eq!(
        ok(&t.run("list_labels.py", &[])),
        format!(
            "{ready}  #5e6ad2  afkd/ready\n{bug}  #eb5757  Bug 🐛 [workspace]\n\
             {waiting}  #f2c94c  Väntar på svar [on issue]\n"
        )
    );
    t.done();
}

#[test]
fn add_label_by_name_or_id_and_never_twice() {
    let t = team();
    let [ready, bug, waiting] = labelled(&t);
    let me = t.id(&t.issue);
    let out = ok(&t.run("add_label.py", &["afkd/ready"]));
    assert_eq!(out, format!("add_label.py: put afkd/ready on {me}\n"));
    ok(&t.run("add_label.py", &[&bug]));
    assert_eq!(
        t.fake.label_ids(&t.issue),
        [&waiting, &ready, &bug].map(String::clone)
    );

    let before = t.writes().len();
    let out = ok(&t.run("add_label.py", &["Väntar på svar"]));
    assert_eq!(
        out,
        format!("add_label.py: {me} already carries Väntar på svar; nothing to do\n")
    );
    refused(
        &t.run("add_label.py", &["Blocked"]),
        1,
        "no label 'Blocked' on team",
    );
    let twin = t.fake.team_label("Bug 🐛", "#000000");
    let out = t.run("add_label.py", &["Bug 🐛"]);
    refused(&out, 1, "2 labels are named 'Bug 🐛'");
    assert!(stderr(&out).contains(&bug) && stderr(&out).contains(&twin));
    assert_eq!(t.writes().len(), before, "nothing written");
    t.done();
}

#[test]
fn remove_label_by_name_or_id_and_never_what_is_not_there() {
    let t = team();
    let [ready, bug, _] = labelled(&t);
    t.fake.put_label(&t.issue, &bug);
    let me = t.id(&t.issue);
    let out = ok(&t.run("remove_label.py", &["Väntar på svar"]));
    assert_eq!(
        out,
        format!("remove_label.py: took Väntar på svar off {me}\n")
    );
    ok(&t.run("remove_label.py", &[&bug]));
    assert!(t.fake.label_ids(&t.issue).is_empty());

    let before = t.writes().len();
    let out = ok(&t.run("remove_label.py", &[&ready]));
    assert_eq!(
        out,
        format!("remove_label.py: {me} does not carry afkd/ready; nothing to do\n")
    );
    refused(
        &t.run("remove_label.py", &["Blocked"]),
        1,
        "no label 'Blocked' on team",
    );
    let twin = t.fake.team_label("Bug 🐛", "#000000");
    let out = t.run("remove_label.py", &["Bug 🐛"]);
    refused(&out, 1, "2 labels are named 'Bug 🐛'");
    assert!(stderr(&out).contains(&bug) && stderr(&out).contains(&twin));
    assert_eq!(t.writes().len(), before, "nothing written");
    assert_eq!(t.fake.labels(&t.issue), Vec::<String>::new());
    t.done();
}

// --- navigating the team ---------------------------------------------------------------

#[test]
fn list_states_prints_the_workflow_in_order() {
    let t = team();
    let out = ok(&t.run("list_states.py", &[]));
    let lines: Vec<(String, String)> = out
        .lines()
        .map(|l| {
            let mut parts = l.splitn(3, "  ");
            let (_, kind, name) = (parts.next(), parts.next(), parts.next());
            (kind.unwrap().to_string(), name.unwrap().to_string())
        })
        .collect();
    assert_eq!(
        lines,
        [
            ("backlog", "Backlog"),
            ("unstarted", "Up Next"),
            ("started", "In Progress"),
            ("started", "審查 Review 👀"),
            ("completed", "Done"),
        ]
        .map(|(k, n)| (k.to_string(), n.to_string()))
    );
    assert!(out
        .lines()
        .nth(1)
        .unwrap()
        .starts_with(&format!("{}  ", t.up_next)));
    t.done();
}

#[test]
fn list_issues_prints_a_state_in_manual_order_across_pages() {
    let t = team();
    let line = |id: &String| format!("{}  {}", t.id(id), t.fake.title_of(id));
    let out = ok(&t.run("list_issues.py", &["Up Next"]));
    assert_eq!(
        out,
        t.queue.iter().map(|q| line(q) + "\n").collect::<String>()
    );
    let out = ok(&t.run("list_issues.py", &["Up Next", "--limit", "2"]));
    assert_eq!(
        out,
        format!("{}\n{}\n", line(&t.queue[0]), line(&t.queue[1]))
    );
    assert_eq!(
        ok(&t.run("list_issues.py", &["Done"])),
        "no issues in state Done\n"
    );

    for n in 0..120 {
        t.fake.issue("Backlog", &format!("想法 {n:03} — idea"), "");
    }
    let before = t.sent("StateIssues").len();
    let out = ok(&t.run("list_issues.py", &["Backlog", "--limit", "500"]));
    assert_eq!(out.lines().count(), 120);
    assert!(out.lines().last().unwrap().ends_with("想法 119 — idea"));
    assert_eq!(t.sent("StateIssues").len() - before, 3, "pages of 50");
    let out = ok(&t.run("list_issues.py", &["Backlog"]));
    assert_eq!(out.lines().count(), 50, "50 by default");
    t.done();
}

// --- files -----------------------------------------------------------------------------

/// An issue linking three Linear files and one page elsewhere: a screenshot and a log in its
/// description, the screenshot again and a wide-named plan in a comment, and a file in a
/// control marker, which carries none. Returns the issue and the three urls.
fn issue_with_files(t: &Team) -> (String, [String; 3]) {
    let shot = t.fake.seed_file("shot.png", "image/png", &png());
    let log = t
        .fake
        .seed_file("run.log", "text/plain", b"line 1\nline 2\n");
    let plan = t
        .fake
        .seed_file("計画 plan.md", "text/markdown", "## 計画\n".as_bytes());
    let stray = t.fake.seed_file("stray.png", "image/png", &png());
    let description = format!(
        "## 背景\n\nBefore:\n\n![shot.png]({shot})\n\n\
         See [the docs](https://example.com/docs) and [run.log]({log})."
    );
    let issue = t.fake.issue("In Progress", "Files 📎", &description);
    let chen = t.fake.user("陳大文", "chen", "chen@acme.example");
    t.fake.comment(
        &issue,
        Author::User(chen),
        &format!("Again: ![shot.png]({shot}) and [計画 plan.md]({plan})"),
        300,
    );
    t.fake.comment(
        &issue,
        Author::User(t.fake.key_user()),
        &format!("\\[afkd-attempt\\] 1/2: [stray.png]({stray})"),
        200,
    );
    (issue, [shot, log, plan])
}

#[test]
fn list_files_numbers_the_issue_s_linear_files_in_fetch_order() {
    let t = team();
    let me = t.id(&t.issue);
    assert_eq!(
        ok(&t.run("list_files.py", &[])),
        format!("no files on issue {me}\n")
    );
    let (issue, [shot, log, plan]) = issue_with_files(&t);
    let out = ok(&t.run("list_files.py", &["--issue", &t.id(&issue)]));
    assert_eq!(
        out,
        format!("1  shot.png  {shot}\n2  run.log  {log}\n3  計画 plan.md  {plan}\n")
    );
    t.done();
}

#[test]
fn fetch_file_writes_a_numbered_file_to_scratch_byte_for_byte() {
    let t = team();
    let (issue, [shot, ..]) = issue_with_files(&t);
    let out = ok(&t.run("fetch_file.py", &["1", "--issue", &t.id(&issue)]));
    let dir = t.scratch.path().join("attachments");
    assert_eq!(out, format!("{}\n", dir.join("1-shot.png").display()));
    assert_eq!(std::fs::read(dir.join("1-shot.png")).unwrap(), png());
    let names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["1-shot.png"], "no temp file left");
    let get = t
        .fake
        .seen()
        .into_iter()
        .find(|s| s.method == "GET")
        .expect("the download");
    assert!(shot.ends_with(&get.path), "{} fetched {shot}", get.path);
    assert_eq!(get.authorization.as_deref(), Some(API_KEY));

    // By its url, to a path of the leg's choosing.
    let to = t.scratch.path().join("plan 計画.md");
    let plan = issue_with_files(&t);
    let out = ok(&t.run(
        "fetch_file.py",
        &[
            &plan.1[2],
            "--to",
            &to.display().to_string(),
            "--issue",
            &plan.0,
        ],
    ));
    assert_eq!(out, format!("{}\n", to.display()));
    assert_eq!(std::fs::read_to_string(&to).unwrap(), "## 計画\n");
    t.done();
}

#[test]
fn fetch_file_by_name_takes_the_newest_and_writes_nothing_for_none() {
    let t = team();
    let older = t
        .fake
        .seed_file("plan.md", "text/markdown", b"first plan\n");
    let newer = t
        .fake
        .seed_file("plan.md", "text/markdown", b"second plan \xe2\x9c\x85\n");
    let issue = t
        .fake
        .issue("In Progress", "Plans", &format!("[plan.md]({older})"));
    t.fake.comment(
        &issue,
        Author::User(t.fake.key_user()),
        &format!("The retry's plan: [plan.md]({newer})"),
        60,
    );
    let id = t.id(&issue);
    let to = t.scratch.path().join("earlier-plan.md");
    let to_arg = to.display().to_string();
    ok(&t.run(
        "fetch_file.py",
        &["--name", "plan.md", "--to", &to_arg, "--issue", &id],
    ));
    assert_eq!(std::fs::read_to_string(&to).unwrap(), "second plan ✅\n");

    std::fs::write(&to, "kept").unwrap();
    let out = ok(&t.run(
        "fetch_file.py",
        &["--name", "nope.md", "--to", &to_arg, "--issue", &id],
    ));
    assert_eq!(
        out,
        format!("fetch_file.py: no file named nope.md on issue {id}\n")
    );
    assert_eq!(std::fs::read_to_string(&to).unwrap(), "kept");

    let seen = t.fake.seen().len();
    refused(
        &t.run("fetch_file.py", &["1", "--name", "plan.md"]),
        2,
        "not both",
    );
    refused(&t.run("fetch_file.py", &[]), 2, "not both");
    assert_eq!(t.fake.seen().len(), seen, "no request");
    refused(
        &t.run("fetch_file.py", &["7", "--issue", &id]),
        1,
        &format!("no file 7 on issue {id}"),
    );
    t.done();
}

#[test]
fn post_file_uploads_without_the_credential_and_embeds_it_in_a_comment() {
    let t = team();
    let shot = t.scratch.path().join("shot.png");
    std::fs::write(&shot, png()).unwrap();
    let out = ok(&t.run(
        "post_file.py",
        &[&shot.display().to_string(), "--comment", "After the fix ✅"],
    ));
    let uploads = t.fake.uploads();
    let asset = &uploads[0].asset_url;
    assert_eq!(
        out,
        format!(
            "post_file.py: put shot.png on {} at {asset}\n",
            t.id(&t.issue)
        )
    );
    assert_eq!(
        (
            uploads[0].filename.as_str(),
            uploads[0].content_type.as_str()
        ),
        ("shot.png", "image/png")
    );
    assert_eq!(t.fake.file(asset), Some(png()));
    let put = &t.fake.puts()[0];
    assert_eq!(put.status, 200);
    assert_eq!(put.header("authorization"), None);
    assert_eq!(put.header("cache-control"), Some(UPLOAD_CACHE_CONTROL));
    assert!(put.header(UPLOAD_HEADER).is_some());
    assert_eq!(
        t.fake.comments(&t.issue)[0].body,
        format!("After the fix ✅\n\n![shot.png]({asset})")
    );

    // Any other file is linked, not shown, and a name is escaped as link text.
    let log = t.scratch.path().join("run.log");
    std::fs::write(&log, "line 1\n[ERR] 失败\n").unwrap();
    ok(&t.run(
        "post_file.py",
        &[&log.display().to_string(), "--name", "run [final].log"],
    ));
    let asset = &t.fake.uploads()[1].asset_url;
    assert_eq!(t.fake.uploads()[1].content_type, "text/plain");
    assert_eq!(
        t.fake.comments(&t.issue)[1].body,
        format!("[run \\[final\\].log]({asset})")
    );
    t.done();
}

#[test]
fn post_file_refuses_a_missing_or_too_large_file_before_any_request() {
    let t = team();
    let big = t.scratch.path().join("big.bin");
    let file = std::fs::File::create(&big).unwrap();
    file.set_len(10 * 1024 * 1024 + 1).unwrap();
    refused(
        &t.run("post_file.py", &[&big.display().to_string()]),
        1,
        "is over the 10 MiB a file may hold",
    );
    let gone = t.scratch.path().join("gone.png");
    refused(
        &t.run("post_file.py", &[&gone.display().to_string()]),
        1,
        &format!("no file at {}", gone.display()),
    );
    assert!(t.fake.seen().is_empty(), "{:?}", t.fake.seen());
    t.done();
}

// --- the skill as a whole --------------------------------------------------------------

/// Every document the skill sends conforms to Linear's schema, holds the plugin's own text
/// where it shares the plugin's name, and is sent by some script.
#[test]
fn every_skill_operation_conforms_and_agrees_with_the_plugins() {
    let schema = Sdl::parse(include_str!("schema.graphql")).expect("the excerpt parses");
    let skill = skill_documents();
    let plugin = plugin_documents();
    let sources: String = std::fs::read_dir(skill_dir())
        .unwrap()
        .chain(std::fs::read_dir(skill_dir().join("lib")).unwrap())
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "py"))
        .map(|p| std::fs::read_to_string(p).unwrap())
        .collect();
    assert!(skill.len() >= 15, "{:?}", skill.keys());
    for (name, document) in &skill {
        let checked = schema
            .check_document(document)
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(&checked.name, name);
        if let Some(own) = plugin.get(name) {
            assert_eq!(own, document, "{name} is the plugin's document");
        }
        assert!(
            sources.contains(&format!("\"{name}\"")),
            "no script sends {name}"
        );
    }
}

/// Every script is executable, allowed by SKILL.md and run by a leg here.
#[test]
fn every_script_has_a_leg_and_a_skill_section() {
    let skill_md = std::fs::read_to_string(skill_dir().join("SKILL.md")).expect("SKILL.md");
    let front = skill_md
        .split("---")
        .nth(1)
        .expect("SKILL.md opens with front matter");
    let allowed = front
        .lines()
        .find(|l| l.starts_with("allowed-tools:"))
        .expect("allowed-tools");
    let legs = include_str!("skills.rs");
    let mut scripts: Vec<String> = std::fs::read_dir(skill_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".py"))
        .collect();
    scripts.sort();
    assert_eq!(scripts.len(), 19, "{scripts:?}");
    for script in &scripts {
        let mode = std::fs::metadata(skill_dir().join(script))
            .unwrap()
            .permissions()
            .mode();
        assert!(mode & 0o111 != 0, "{script} is executable");
        assert!(
            allowed.contains(&format!("Bash(*/{script} *)")),
            "SKILL.md allows {script}"
        );
        assert!(
            skill_md.contains(&format!("skills/linear/{script}\"")),
            "SKILL.md shows {script}"
        );
        assert!(
            legs.contains(&format!("run(\"{script}\"")) || legs.contains(&format!("\"{script}\",")),
            "a leg runs {script}"
        );
    }
    let lib = std::fs::metadata(skill_dir().join("lib/linear_api.py"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(lib & 0o111, 0, "the module is imported, not run");
}
