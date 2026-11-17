//! The `linear` skill's scripts, run as an agent runs them — `python3` on the script, with the
//! `LINEAR_*` a run carries in its environment — against the stateful fake Linear, whose
//! GraphQL endpoint is the `LINEAR_API_URL` they read.
//!
//! Each script is one of the plugin's actions under the action's name, so each leg holds what
//! it prints to what the plugin itself gives back: its stdout, parsed as JSON, is the value the
//! plugin's reply to the same call carries ([`once`]), and a write's is the plugin's own read of
//! what it changed, once the fake is read back for the change. A script's refusal exits 1 with
//! the plugin's own sentence for the same call. Every leg ends with the fake's record of
//! refused requests empty, so each script's every request is held to Linear's schema and to
//! the documents `skills/linear/lib/operations.graphql` holds.
//!
//! A script that does not exist is a `python3` that exits 2, which every leg's success
//! assertion catches, so each leg fails before its script is written.

// A whole `linear.Issue` handle, written as one `json!`, nests deeper than the default limit.
#![recursion_limit = "256"]

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{json, Value};

use common::fake::sdl::Sdl;
use common::fake::{
    plugin_documents, png, skill_documents, Author, FakeLinear, Seen, API_KEY, TEAM,
    UPLOAD_CACHE_CONTROL, UPLOAD_HEADER,
};
use common::{once, ws, TempDir};

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
    "CommentPost",
    "IssueWrite",
    "IssueCreate",
    "IssueAddLabel",
    "IssueRemoveLabel",
    "RelationCreate",
    "IssueArchive",
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
    /// The run's scratch dir.
    scratch: TempDir,
    /// The plugin's own directory, for the calls the plugin answers.
    state: TempDir,
}

fn team() -> Team {
    let fake = FakeLinear::start();
    for (state, kind) in [
        ("Backlog", "backlog"),
        ("Up Next", "unstarted"),
        ("In Progress", "started"),
        ("審查 Review 👀", "started"),
        ("Done", "completed"),
    ] {
        fake.state(state, kind);
    }
    let queue = QUEUE.iter().map(|t| fake.issue("Up Next", t, "")).collect();
    let issue = fake.issue("In Progress", TITLE, BODY);
    Team {
        fake,
        issue,
        queue,
        scratch: TempDir::new("skill"),
        state: TempDir::new("skill-plugin"),
    }
}

impl Team {
    /// An issue's identifier.
    fn id(&self, issue: &str) -> String {
        self.fake.identifier(issue)
    }

    /// Run one script with the env a run carries and nothing else of Linear's.
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

    /// What the script printed, run with `args`, as JSON; the leg fails with its stderr when
    /// it did not succeed.
    fn json(&self, script: &str, args: &[&str]) -> Value {
        let out = ok(&self.run(script, args));
        serde_json::from_str(&out)
            .unwrap_or_else(|e| panic!("{script} printed no JSON ({e}): {out}"))
    }

    /// The plugin's reply to `action` with `args` beside the workspace the run reaches.
    fn plugin(&self, action: &str, args: Value) -> Value {
        let mut args = args.as_object().expect("an args object").clone();
        args.insert("ws".into(), ws(&self.fake));
        once(action, Value::Object(args), self.state.path()).0
    }

    /// The value the plugin's ok reply to `action` with `args` carries: its handle, its
    /// handles, its value, or nil.
    fn value(&self, action: &str, args: Value) -> Value {
        let reply = self.plugin(action, args);
        assert_eq!(reply["ok"], true, "{action}: {reply}");
        ["handle", "handles", "value"]
            .iter()
            .find_map(|key| reply.get(*key).cloned())
            .unwrap_or(Value::Null)
    }

    /// The sentence the plugin refuses `action` with `args` with.
    fn refusal(&self, action: &str, args: Value) -> String {
        let reply = self.plugin(action, args);
        assert_eq!(reply["ok"], false, "{action} was not refused: {reply}");
        reply["error"].as_str().expect("a refusal").to_string()
    }

    /// The issue the plugin reads by `id`, an identifier or a uuid.
    fn read(&self, id: &str) -> Value {
        self.value("issue", json!({ "id": id }))
    }

    /// The handle `{id, key}` of the issue `id`, as afkd passes the plugin one.
    fn handle(&self, id: &str) -> Value {
        let issue = self.read(id);
        json!({"id": issue["id"], "key": issue["key"]})
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

// --- reads -----------------------------------------------------------------------------

/// The run's issue with every field set and every nested list past the 20 one read holds: 22
/// sub-issues, 21 labels, relations both ways, a link, a subscriber, an assignee, a project, a
/// milestone and a cycle.
fn rich(t: &Team) -> Vec<String> {
    let chen = t.fake.user("陳大文", "chen", "chen@acme.example");
    let project = t
        .fake
        .project("Trackers 🧭", &[TEAM], &[("B — reads", 2.0)]);
    let cycle = t.fake.cycle(TEAM, 12, Some("Sprint 12 — 冬"), 0, 1, true);
    let me = &t.issue;
    t.fake.set_priority(me, 2);
    t.fake.set_estimate(me, 3.0);
    t.fake.set_due(me, "2026-12-24");
    t.fake.assign(me, &chen);
    t.fake.set_project(me, &project, Some("B — reads"));
    t.fake.set_cycle(me, &cycle);
    t.fake.link(me, "https://github.com/acme/widgets/pull/7");
    t.fake.subscribe(me, &chen);
    for n in 0..21 {
        let label = t.fake.team_label(&format!("標籤 {n:02}"), "#5e6ad2");
        t.fake.put_label(me, &label);
    }
    let children: Vec<String> = (0..22)
        .map(|n| {
            let child = t.fake.issue("Backlog", &format!("部分 {n:02} — part"), "");
            t.fake.set_parent(&child, me);
            child
        })
        .collect();
    t.fake.relate(&t.queue[0], me, "blocks");
    t.fake.relate(me, &t.queue[1], "related");
    t.fake.relate(&t.queue[2], me, "duplicate");
    children
}

#[test]
fn issue_prints_the_plugins_handle_by_identifier_or_uuid() {
    let t = team();
    rich(&t);
    let me = t.id(&t.issue);
    let plugin = t.read(&me);
    assert_eq!(plugin["fields"]["sub_issues"].as_array().unwrap().len(), 22);
    assert_eq!(plugin["fields"]["labels"].as_array().unwrap().len(), 21);
    assert_eq!(t.json("issue.py", &[&me]), plugin);
    assert_eq!(t.json("issue.py", &[&t.issue]), plugin, "by its uuid");
    assert!(
        t.sent("IssueChildren").len() >= 2,
        "each list read past its first page"
    );

    let unknown = t.run("issue.py", &["ENG-999"]);
    refused(&unknown, 1, &t.refusal("issue", json!({"id": "ENG-999"})));
    refused(
        &unknown,
        1,
        "issue: no issue ENG-999 in workspace acme: linear read issue: Entity not found",
    );
    t.done();
}

#[test]
fn issues_prints_what_the_plugins_issues_keeps() {
    let t = team();
    rich(&t);
    t.fake.archive(&t.queue[3]);
    for n in 0..23 {
        t.fake.issue("Done", &format!("想法 {n:03} — idea"), "");
    }
    let me = t.id(&t.issue);
    for (args, plugin) in [
        (
            vec!["--state", "Up Next", "--limit", "2"],
            json!({"state": ["Up Next"], "limit": 2}),
        ),
        (
            vec!["--state", "Done", "--limit", "23"],
            json!({"state": ["Done"], "limit": 23}),
        ),
        (
            vec!["--team", TEAM, "--order", "title", "--order", "created"],
            json!({"team": [TEAM], "order": ["title", "created"]}),
        ),
        (
            vec![
                "--labels",
                "標籤 03",
                "--labels",
                "標籤 20",
                "--assignee",
                "chen",
            ],
            json!({"labels": ["標籤 03", "標籤 20"], "assignee": ["chen"]}),
        ),
        (
            vec![
                "--state_type",
                "started",
                "--state_type",
                "canceled",
                "--blocked",
                "true",
            ],
            json!({"state_type": ["started", "canceled"], "blocked": true}),
        ),
        (
            vec!["--parent", &me, "--text", "PART", "--priority", "none"],
            json!({"parent": [me.clone()], "text": "PART", "priority": ["none"]}),
        ),
        (
            vec![
                "--due_before",
                "2026-12-25",
                "--created_after",
                "2020-01-01T00:00:00Z",
            ],
            json!({"due_before": "2026-12-25", "created_after": 1_577_836_800_000_i64}),
        ),
        (
            vec!["--state", "Up Next", "--include_archived"],
            json!({"state": ["Up Next"], "include_archived": true}),
        ),
        (
            vec!["--id", &me, "--id", &t.queue[1], "--project", "Trackers 🧭"],
            json!({"id": [me.clone(), t.queue[1].clone()], "project": ["Trackers 🧭"]}),
        ),
    ] {
        let printed = t.json("issues.py", &args);
        assert_eq!(printed, t.value("issues", plugin), "{args:?}");
        assert!(
            !printed.as_array().unwrap().is_empty(),
            "{args:?} keeps one"
        );
    }

    for (args, plugin) in [
        (vec!["--limit", "0"], json!({"limit": 0})),
        (
            vec!["--due_after", "2026-02-30"],
            json!({"due_after": "2026-02-30"}),
        ),
        (vec!["--id", "ENG"], json!({"id": ["ENG"]})),
        (
            vec![
                "--order", "team", "--order", "state", "--order", "title", "--order", "due",
            ],
            json!({"order": ["team", "state", "title", "due"]}),
        ),
    ] {
        refused(&t.run("issues.py", &args), 1, &t.refusal("issues", plugin));
    }
    t.done();
}

#[test]
fn comments_prints_the_thread_in_reply_order() {
    let t = team();
    let chen = t.fake.user("陳大文", "chen", "chen@acme.example");
    let asked = t.fake.comment(&t.issue, Author::User(chen), COMMENT, 300);
    t.fake.comment(
        &t.issue,
        Author::Bot(t.fake.bot_id()),
        "Plan posted ✅\n\n- one",
        200,
    );
    let slack = "From Slack: \"ship it\" 🚀";
    t.fake
        .comment(&t.issue, Author::External("ext-7f3a".into()), slack, 100);
    let reply = t.fake.comment_at(
        &t.issue,
        Author::User(t.fake.key_user()),
        "回复: fixed in 488931d",
        common::fake::now_ms() - 50_000,
        Some(&asked),
    );
    t.fake.mark_edited(&asked);
    t.fake.resolve(&asked, common::fake::now_ms());
    let me = t.id(&t.issue);
    let printed = t.json("comments.py", &[&me]);
    assert_eq!(
        printed,
        t.value("comments", json!({"issue": t.handle(&me)}))
    );
    let order: Vec<&str> = printed
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        &order[..2],
        [asked.as_str(), reply.as_str()],
        "the reply after what it answers"
    );

    let quiet = t.id(&t.queue[0]);
    assert_eq!(t.json("comments.py", &[&quiet]), json!([]));
    t.done();
}

#[test]
fn states_and_labels_print_the_plugins_handles() {
    let t = team();
    t.fake.team("OPS", "Operations — 運用");
    t.fake.label_of(TEAM, "Bug 🐛", Some("Type"));
    t.fake.label_of("OPS", "Infra", None);
    t.fake.workspace_label("Needs Info", "#f2c94c");
    let states = t.json("states.py", &[TEAM]);
    assert_eq!(states, t.value("states", json!({"team": TEAM})));
    let names: Vec<&str> = states
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["fields"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "Backlog",
            "Up Next",
            "In Progress",
            "審查 Review 👀",
            "Done"
        ]
    );
    assert_eq!(t.json("labels.py", &[]), t.value("labels", json!({})));
    let of_team = t.json("labels.py", &["--team", TEAM]);
    assert_eq!(of_team, t.value("labels", json!({"team": TEAM})));
    assert_eq!(of_team.as_array().unwrap().len(), 2, "{of_team}");
    refused(
        &t.run("states.py", &["NOPE"]),
        1,
        &t.refusal("states", json!({"team": "NOPE"})),
    );
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
fn files_lists_the_plugins_urls_and_download_file_saves_one() {
    let t = team();
    let (issue, [shot, log, plan]) = issue_with_files(&t);
    let id = t.id(&issue);
    let printed = t.json("files.py", &[&id]);
    assert_eq!(printed, json!([shot, log, plan]));
    assert_eq!(printed, t.value("files", json!({"issue": t.handle(&id)})));
    assert_eq!(t.json("files.py", &[&t.queue[0]]), json!([]));

    let to = t.scratch.path().join("計画 1").join("shot.png");
    std::fs::create_dir_all(to.parent().unwrap()).unwrap();
    let to_arg = to.display().to_string();
    assert_eq!(t.json("download_file.py", &[&shot, &to_arg]), Value::Null);
    assert_eq!(std::fs::read(&to).unwrap(), png());
    let names: Vec<String> = std::fs::read_dir(to.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["shot.png"], "no temp file left");
    let get = t
        .fake
        .seen()
        .into_iter()
        .find(|s| s.method == "GET")
        .expect("the download");
    assert!(shot.ends_with(&get.path), "{} fetched {shot}", get.path);
    assert_eq!(get.authorization.as_deref(), Some(API_KEY));

    for (url, to) in [
        (shot.as_str(), "shot.png"),
        ("https://example.com/shot.png", to_arg.as_str()),
    ] {
        let refusal = t.refusal("download_file", json!({"url": url, "to": to}));
        let seen = t.fake.seen().len();
        refused(&t.run("download_file.py", &[url, to]), 1, &refusal);
        assert_eq!(
            t.fake.seen().len(),
            seen,
            "nothing fetched for {url} to {to}"
        );
    }
    t.done();
}

// --- filing and changing issues ---------------------------------------------------------

/// A team label, a workspace label, a user, a project with two milestones and a cycle, for a
/// write to name.
fn names(t: &Team) {
    t.fake.team_label("afkd/ready", "#5e6ad2");
    t.fake.workspace_label("Bug 🐛", "#eb5757");
    t.fake.user("陳大文", "chen", "chen@acme.example");
    t.fake.project(
        "Trackers 🧭",
        &[TEAM],
        &[("C — writes", 3.0), ("B — reads", 2.0)],
    );
    t.fake.cycle(TEAM, 12, Some("Sprint 12 — 冬"), 0, 1, true);
}

#[test]
fn create_issue_files_every_field_as_the_plugin_reads_it() {
    let t = team();
    names(&t);
    let desc = t.scratch.path().join("follow-up.md");
    std::fs::write(&desc, BODY).unwrap();
    let shot = t.scratch.path().join("shot.png");
    std::fs::write(&shot, png()).unwrap();
    let title = "fix(linear): 跟进 a drift red that is not ours - `wire.rs` reds on main";
    let (after, me) = (t.id(&t.queue[1]), t.id(&t.issue));
    let made = t.json(
        "create_issue.py",
        &[
            TEAM,
            title,
            "--description_file",
            &desc.display().to_string(),
            "--files",
            &shot.display().to_string(),
            "--state",
            "Up Next",
            "--after",
            &after,
            "--priority",
            "high",
            "--estimate",
            "3",
            "--due",
            "2026-12-24",
            "--labels",
            "afkd/ready",
            "--labels",
            "Bug 🐛",
            "--assignee",
            "chen@acme.example",
            "--delegate",
            "me",
            "--project",
            "Trackers 🧭",
            "--milestone",
            "B — reads",
            "--cycle",
            "12",
            "--parent",
            &me,
        ],
    );
    let new = t.fake.find_by_title(title).expect("filed");
    assert_eq!(made, t.read(&new), "the plugin's read of what it filed");
    let fields = &made["fields"];
    assert_eq!(
        (
            &fields["state"],
            &fields["priority"],
            &fields["estimate"],
            &fields["due"],
            &fields["assignee"],
            &fields["project"],
            &fields["milestone"],
            &fields["cycle"],
            &fields["parent"],
        ),
        (
            &json!("Up Next"),
            &json!("high"),
            &json!(3),
            &json!("2026-12-24"),
            &json!("chen"),
            &json!("Trackers 🧭"),
            &json!("B — reads"),
            &json!(12),
            &json!(me),
        )
    );
    assert_eq!(fields["labels"], json!(["afkd/ready", "Bug 🐛"]));
    assert!(fields["delegate"].is_string(), "{fields}");
    let asset = &t.fake.uploads()[0].asset_url;
    assert_eq!(
        fields["description"],
        format!("{BODY}\n\n![shot.png]({asset})")
    );
    let put = &t.fake.puts()[0];
    assert_eq!(put.header("authorization"), None);
    assert_eq!(put.header("cache-control"), Some(UPLOAD_CACHE_CONTROL));
    assert!(put.header(UPLOAD_HEADER).is_some());
    let q = &t.queue;
    assert_eq!(
        t.fake.in_state("Up Next"),
        [&q[0], &q[1], &new, &q[2], &q[3]].map(String::clone)
    );

    // `at` with no state lands at that end of the team's default state.
    let top = t.json(
        "create_issue.py",
        &[TEAM, "chore: filed at the top 📥", "--at", "top"],
    );
    let filed = top["fields"]["uuid"].as_str().unwrap().to_string();
    assert_eq!(
        top["fields"]["state"], "Backlog",
        "the team's default state"
    );
    assert_eq!(t.fake.in_state("Backlog")[0], filed);
    let input = &t.sent("IssueCreate")[1].variables["input"];
    assert!(input.get("description").is_none() && input.get("labelIds").is_none());
    t.done();
}

#[test]
fn create_issue_refuses_as_the_plugin_does_and_files_nothing() {
    let t = team();
    names(&t);
    t.fake.team_label("Bug 🐛", "#000000");
    t.fake.team_label("Dup", "#000000");
    t.fake.team_label("Dup", "#111111");
    let elsewhere = t.id(&t.issue);
    let elsewhere_handle = t.handle(&elsewhere);
    let gone = t.scratch.path().join("gone.png").display().to_string();
    for (args, plugin) in [
        (vec!["--state", "Nowhere"], json!({"state": "Nowhere"})),
        (vec!["--labels", "Dup"], json!({"labels": ["Dup"]})),
        (vec!["--labels", "Blocked"], json!({"labels": ["Blocked"]})),
        (
            vec!["--at", "top", "--after", &elsewhere],
            json!({"at": "top", "after": elsewhere_handle}),
        ),
        (
            vec!["--state", "Up Next", "--after", &elsewhere],
            json!({"state": "Up Next", "after": elsewhere_handle}),
        ),
        (vec!["--assignee", "nobody"], json!({"assignee": "nobody"})),
        (vec!["--project", "Billing"], json!({"project": "Billing"})),
        (
            vec!["--milestone", "B — reads"],
            json!({"milestone": "B — reads"}),
        ),
        (
            vec!["--project", "Trackers 🧭", "--milestone", "Z"],
            json!({"project": "Trackers 🧭", "milestone": "Z"}),
        ),
        (vec!["--cycle", "99"], json!({"cycle": 99})),
        (vec!["--due", "2026-02-30"], json!({"due": "2026-02-30"})),
        (vec!["--files", "shot.png"], json!({"files": ["shot.png"]})),
        (vec!["--files", &gone], json!({"files": [gone.clone()]})),
    ] {
        let mut all = vec![TEAM, "Look again 👀"];
        all.extend(args.iter().copied());
        let mut call = plugin.as_object().unwrap().clone();
        call.insert("team".into(), json!(TEAM));
        call.insert("title".into(), json!("Look again 👀"));
        let refusal = t.refusal("create_issue", Value::Object(call));
        refused(&t.run("create_issue.py", &all), 1, &refusal);
    }
    let out = t.run("create_issue.py", &[TEAM, "x", "--labels", "Blocked"]);
    assert!(
        stderr(&out).contains("there are: afkd/ready, Bug 🐛, Dup, Dup, Bug 🐛"),
        "the labels there are, the team's first:\n{}",
        stderr(&out)
    );
    refused(
        &t.run(
            "create_issue.py",
            &[TEAM, "<REPLACE WITH YOUR ISSUE TITLE>"],
        ),
        2,
        "placeholder",
    );
    refused(
        &t.run("create_issue.py", &[TEAM, "  "]),
        2,
        "the title is empty",
    );
    assert!(t.writes().is_empty(), "{:?}", t.writes());
    t.done();
}

#[test]
fn update_issue_sets_clears_and_places_as_the_plugin_reads_it() {
    let t = team();
    names(&t);
    let me = t.id(&t.issue);
    let desc = t.scratch.path().join("desc.md");
    let rewritten = format!("{BODY}\n\nUpdate: 修复 it ✅\n");
    std::fs::write(&desc, &rewritten).unwrap();
    let title = "fix(linear): 修复 the skill - `ask.py` parks the run";
    let changed = t.json(
        "update_issue.py",
        &[
            &me,
            "--title",
            title,
            "--description_file",
            &desc.display().to_string(),
            "--state",
            "Up Next",
            "--after",
            &t.id(&t.queue[1]),
            "--priority",
            "urgent",
            "--estimate",
            "5",
            "--due",
            "2026-12-24",
            "--assignee",
            "me",
            "--project",
            "Trackers 🧭",
            "--milestone",
            "C — writes",
            "--cycle",
            "12",
        ],
    );
    assert_eq!(changed, t.read(&me));
    assert_eq!(changed["fields"]["title"], title);
    assert_eq!(changed["fields"]["description"], rewritten);
    assert_eq!(changed["fields"]["milestone"], "C — writes");
    let q = &t.queue;
    assert_eq!(
        t.fake.in_state("Up Next"),
        [&q[0], &q[1], &t.issue, &q[2], &q[3]].map(String::clone)
    );

    // Each `--clear` is the action's nil, and a field left out is left as it is.
    let cleared = t.json(
        "update_issue.py",
        &[
            &me, "--clear", "estimate", "--clear", "due", "--clear", "project", "--at", "top",
        ],
    );
    assert_eq!(cleared, t.read(&me));
    let fields = &cleared["fields"];
    assert_eq!(
        (&fields["estimate"], &fields["due"], &fields["project"]),
        (&Value::Null, &Value::Null, &Value::Null)
    );
    assert_eq!(
        (&fields["title"], &fields["priority"]),
        (&json!(title), &json!("urgent"))
    );
    assert_eq!(t.fake.in_state("Up Next")[0], t.issue);

    let writes = t.writes().len();
    let nothing = t.json("update_issue.py", &[&me]);
    assert_eq!(nothing, t.read(&me));
    assert_eq!(t.writes().len(), writes, "nothing to change sends nothing");

    let handle = t.handle(&me);
    for (args, plugin) in [
        (vec!["--team", "OPS"], json!({"team": "OPS"})),
        (
            vec!["--files", "/tmp/x.png"],
            json!({"files": ["/tmp/x.png"]}),
        ),
        (vec!["--after", &me], json!({"after": handle})),
        (vec!["--state", "Nowhere"], json!({"state": "Nowhere"})),
        (
            vec!["--milestone", "B — reads"],
            json!({"milestone": "B — reads"}),
        ),
    ] {
        let mut all = vec![me.as_str()];
        all.extend(args.iter().copied());
        let mut call = plugin.as_object().unwrap().clone();
        call.insert("issue".into(), handle.clone());
        let refusal = t.refusal("update_issue", Value::Object(call));
        refused(&t.run("update_issue.py", &all), 1, &refusal);
    }
    refused(
        &t.run(
            "update_issue.py",
            &[&me, "--estimate", "3", "--clear", "estimate"],
        ),
        2,
        "--estimate and --clear estimate cannot both be given",
    );
    refused(
        &t.run(
            "update_issue.py",
            &[&me, "--title", "<REPLACE WITH YOUR NEW TITLE>"],
        ),
        2,
        "placeholder",
    );
    assert_eq!(t.writes().len(), writes, "nothing refused was written");
    t.done();
}

#[test]
fn add_and_remove_label_change_the_issue_as_the_plugin_reads_it() {
    let t = team();
    let ready = t.fake.team_label("afkd/ready", "#5e6ad2");
    let bug = t.fake.workspace_label("Bug 🐛", "#eb5757");
    let waiting = t.fake.team_label("Väntar på svar", "#f2c94c");
    t.fake.put_label(&t.issue, &waiting);
    let me = t.id(&t.issue);
    let on = t.json("add_label.py", &[&me, "afkd/ready"]);
    assert_eq!(on, t.read(&me));
    t.json("add_label.py", &[&me, &bug]);
    assert_eq!(
        t.fake.label_ids(&t.issue),
        [&waiting, &ready, &bug].map(String::clone)
    );

    let adds = t.sent("IssueAddLabel").len();
    let already = t.json("add_label.py", &[&me, "Väntar på svar"]);
    assert_eq!(already, t.read(&me));
    assert_eq!(
        t.sent("IssueAddLabel").len(),
        adds,
        "already on: nothing sent"
    );

    let off = t.json("remove_label.py", &[&me, "Väntar på svar"]);
    assert_eq!(off, t.read(&me));
    assert_eq!(off["fields"]["labels"], json!(["afkd/ready", "Bug 🐛"]));
    let removes = t.sent("IssueRemoveLabel").len();
    let lacking = t.json("remove_label.py", &[&me, "Väntar på svar"]);
    assert_eq!(lacking, t.read(&me));
    assert_eq!(
        t.sent("IssueRemoveLabel").len(),
        removes,
        "not on: nothing sent"
    );

    let handle = t.handle(&me);
    for (script, action) in [
        ("add_label.py", "add_label"),
        ("remove_label.py", "remove_label"),
    ] {
        let refusal = t.refusal(action, json!({"issue": handle, "label": "Blocked"}));
        assert!(
            refusal.contains("there are: afkd/ready, Väntar på svar, Bug 🐛"),
            "{refusal}"
        );
        refused(&t.run(script, &[&me, "Blocked"]), 1, &refusal);
    }
    t.done();
}

#[test]
fn add_relation_relates_both_ways_once() {
    let t = team();
    let (me, blocker) = (t.id(&t.issue), t.id(&t.queue[0]));
    let related = t.json("add_relation.py", &[&me, "blocked_by", &blocker]);
    assert_eq!(related, t.read(&me));
    assert_eq!(related["fields"]["blocked_by"], json!([blocker]));
    assert_eq!(t.read(&blocker)["fields"]["blocks"], json!([me]));
    let again = t.json("add_relation.py", &[&me, "blocked_by", &blocker]);
    assert_eq!(again, related);
    assert_eq!(
        t.sent("RelationCreate").len(),
        1,
        "a relation held is left as it is"
    );

    let handle = t.handle(&me);
    let refusal = t.refusal(
        "add_relation",
        json!({"issue": handle, "relation": "related", "other": handle}),
    );
    refused(
        &t.run("add_relation.py", &[&me, "related", &me]),
        1,
        &refusal,
    );
    refused(
        &t.run("add_relation.py", &[&me, "parent_of", &blocker]),
        2,
        "invalid choice",
    );
    t.done();
}

#[test]
fn archive_issue_archives_it_and_gives_it_back() {
    let t = team();
    let me = t.id(&t.issue);
    let archived = t.json("archive_issue.py", &[&me]);
    assert!(t.fake.archived(&t.issue));
    assert_eq!(archived["fields"]["archived"], true);
    assert_eq!(archived, t.read(&me));
    t.done();
}

// --- comments ----------------------------------------------------------------------------

#[test]
fn create_comment_posts_files_and_replies_as_the_plugin_reads_them() {
    let t = team();
    let me = t.id(&t.issue);
    let shot = t.scratch.path().join("shot.png");
    std::fs::write(&shot, png()).unwrap();
    let log = t.scratch.path().join("run [final].log");
    std::fs::write(&log, "line 1\n[ERR] 失败\n").unwrap();
    let posted = t.json(
        "create_comment.py",
        &[
            &me,
            COMMENT,
            "--files",
            &shot.display().to_string(),
            "--files",
            &log.display().to_string(),
        ],
    );
    let uploads = t.fake.uploads();
    assert_eq!(
        posted["fields"]["body"],
        format!(
            "{COMMENT}\n\n![shot.png]({})\n\n[run \\[final\\].log]({})",
            uploads[0].asset_url, uploads[1].asset_url
        )
    );
    assert_eq!(uploads[1].content_type, "text/plain");
    let thread = t.value("comments", json!({"issue": t.handle(&me)}));
    assert_eq!(posted, thread[0], "the plugin's read of the comment");
    assert_eq!(
        t.fake.comments(&t.issue)[0].author,
        Author::User(t.fake.key_user())
    );

    // A reply to a reply goes in the thread both are in.
    let root = posted["id"].as_str().unwrap().to_string();
    let reply = t.json("create_comment.py", &[&me, "回复 ✅", "--reply_to", &root]);
    assert_eq!(reply["fields"]["reply_to"], json!(root));
    let id = reply["id"].as_str().unwrap().to_string();
    let deeper = t.json("create_comment.py", &[&me, "again", "--reply_to", &id]);
    assert_eq!(deeper["fields"]["reply_to"], json!(root));
    let body_file = t.scratch.path().join("plan.md");
    std::fs::write(&body_file, "## 計画\n\n- one\n").unwrap();
    let from_file = t.json(
        "create_comment.py",
        &[&me, "--body_file", &body_file.display().to_string()],
    );
    assert_eq!(from_file["fields"]["body"], "## 計画\n\n- one\n");
    let thread = t.value("comments", json!({"issue": t.handle(&me)}));
    assert_eq!(thread, json!([posted, reply, deeper, from_file]));

    // A reply to a comment on another issue is refused as the plugin refuses it.
    let other = t.id(&t.queue[0]);
    let refusal = t.refusal(
        "create_comment",
        json!({"issue": t.handle(&other), "body": "x",
               "reply_to": {"id": root, "key": posted["key"]}}),
    );
    let writes = t.writes().len();
    refused(
        &t.run("create_comment.py", &[&other, "x", "--reply_to", &root]),
        1,
        &refusal,
    );
    assert_eq!(t.writes().len(), writes);
    t.done();
}

#[test]
fn create_comment_refuses_before_any_request() {
    let t = team();
    let me = t.id(&t.issue);
    let big = t.scratch.path().join("big.bin");
    let file = std::fs::File::create(&big).unwrap();
    file.set_len(10 * 1024 * 1024 + 1).unwrap();
    let big = big.display().to_string();
    let gone = t.scratch.path().join("gone.png").display().to_string();
    let handle = t.handle(&me);
    for path in [big.as_str(), gone.as_str(), "shot.png"] {
        let refusal = t.refusal(
            "create_comment",
            json!({"issue": handle, "body": "x", "files": [path]}),
        );
        let seen = t.fake.seen().len();
        refused(
            &t.run("create_comment.py", &[&me, "x", "--files", path]),
            1,
            &refusal,
        );
        assert_eq!(t.fake.seen().len(), seen, "no request for {path}");
    }
    let seen = t.fake.seen().len();
    refused(
        &t.run("create_comment.py", &[&me, "<REPLACE WITH YOUR COMMENT>"]),
        2,
        "placeholder",
    );
    refused(
        &t.run("create_comment.py", &[&me, "  \n"]),
        2,
        "the comment is empty",
    );
    refused(&t.run("create_comment.py", &[&me]), 2, "not both");
    refused(
        &t.run_with(
            "create_comment.py",
            &[&me, COMMENT],
            &[("LINEAR_AUTHORIZATION", None)],
        ),
        1,
        "LINEAR_AUTHORIZATION must be set",
    );
    assert_eq!(t.fake.seen().len(), seen, "no request");
    t.done();
}

#[test]
fn a_rate_limited_write_says_the_wait_and_is_never_retried() {
    let t = team();
    t.fake.rate_limit("CommentPost", 1, 7);
    refused(
        &t.run("create_comment.py", &[&t.id(&t.issue), COMMENT]),
        1,
        "create_comment.py: linear post comment: Linear rate limited it; retry after 7s",
    );
    assert_eq!(t.sent("CommentPost").len(), 1, "sent once");
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
        "ask.py: linear post question: Linear refused it (500): injected",
    );
    assert!(!t.scratch.path().join("park").exists(), "no marker");
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
    assert!(skill.len() >= 30, "{:?}", skill.keys());
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

/// The scripts are exactly the plugin's actions they do under the actions' names, and `ask.py`;
/// each is executable, allowed by SKILL.md, shown there, and run by a leg here.
#[test]
fn every_script_is_an_action_with_a_leg_and_a_skill_section() {
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
    let manifest = include_str!("../afkd-plugin.toml");
    let mut scripts: Vec<String> = std::fs::read_dir(skill_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".py"))
        .collect();
    scripts.sort();
    assert_eq!(
        scripts,
        [
            "add_label.py",
            "add_relation.py",
            "archive_issue.py",
            "ask.py",
            "comments.py",
            "create_comment.py",
            "create_issue.py",
            "download_file.py",
            "files.py",
            "issue.py",
            "issues.py",
            "labels.py",
            "remove_label.py",
            "states.py",
            "update_issue.py",
        ]
    );
    for script in &scripts {
        let action = script.trim_end_matches(".py");
        assert!(
            script == "ask.py" || manifest.contains(&format!("name = \"{action}\"\n")),
            "{script} is an action's name"
        );
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
