//! The gate that holds the **shipped `@afkd/linear` plugin** — a manifest v2 provider whose
//! main kind is `service(linear)`, shipped as a finished binary — to a real afkd: the `afkd`
//! first on `PATH`, never a build, since a plugin is checked against the afkd it will meet
//! ([`bin_path`]).
//!
//! - The **install** leg installs the tree the release holds and runs one issue through it
//!   on a real daemon with an API key, its slots reading every field the issue's handle
//!   declares, its run reading the description and moving the issue through the plugin's
//!   actions, and its `on_done` commenting on the finished issue; the **app** leg does the
//!   same as an OAuth app, on a token the plugin fetched.
//! - The **files** leg runs one issue whose run puts a PNG into its description and into a
//!   comment through `files=`, and fetches both back with `fetch_files`, byte for byte.
//! - The **validate** legs hold `afkd validate` to a `service(linear)` that writes every
//!   setting in both credential forms, to one whose slots call every action the plugin
//!   declares, to refusing one without `team` or `pick_from`, and to refusing a call of an
//!   action the plugin does not declare.
//!
//! The Linear is the plugin's own loopback fake ([`fake`]), which holds every request to
//! Linear's schema: each leg ends with its record of refused requests empty. No leg touches
//! a network, every wait is bounded, and every spawn is held in a [`Daemon`].

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use common::fake::{png, Author, FakeLinear, API_KEY, CLIENT_ID, CLIENT_SECRET, TEAM};
use common::*;
use tempfile::TempDir;

// --- the shipped tree --------------------------------------------------------------

/// The plugin's name, as the manifest spells it and afkd places it.
const NAME: &str = "@afkd/linear";

/// The plugin's root — the directory `afkd install` takes, whose path this crate's own
/// mirrors under `drift/`. Canonicalized, so a staged copy and a report both name one path.
fn plugin_root() -> PathBuf {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../@afkd/linear");
    std::fs::canonicalize(root).expect("the shipped plugin resolves")
}

/// The plugin's binary, built once per test binary the way the release builds it —
/// `cargo build --release --locked` in the plugin's own directory, for this host rather than
/// musl — since afkd never builds a plugin and a release ships it finished.
fn built_binary() -> &'static Path {
    static BUILT: OnceLock<PathBuf> = OnceLock::new();
    BUILT.get_or_init(|| {
        let status = Command::new("cargo")
            .args(["build", "--release", "--locked", "--quiet"])
            .current_dir(plugin_root())
            .env_remove("CARGO_TARGET_DIR")
            .env_remove("CARGO_BUILD_TARGET_DIR")
            .status()
            .expect("run cargo");
        assert!(status.success(), "the plugin builds");
        plugin_root().join("target/release/afkd-linear")
    })
}

/// Stage the release `tools/pack-release` packs under `dst` — the manifest and the binary,
/// exactly what the release archive holds — and return the staged root.
fn stage(dst: &Path) -> PathBuf {
    let status = Command::new(plugin_root().join("../../tools/pack-release"))
        .arg("linear")
        .arg(dst)
        .arg(built_binary())
        .status()
        .expect("run tools/pack-release");
    assert!(status.success(), "tools/pack-release linear");
    dst.join("afkd-linear")
}

/// Every path under `dir`, relative and sorted, directories included.
fn tree_of(dir: &Path) -> Vec<String> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).expect("read dir") {
            let path = entry.expect("a directory entry").path();
            out.push(
                path.strip_prefix(root)
                    .expect("under root")
                    .to_string_lossy()
                    .into_owned(),
            );
            if path.is_dir() {
                walk(root, &path, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

/// `afkd install <root>` into `home`, failing the leg with the full report unless the
/// plugin is placed.
fn install(home: &Path, root: &Path) {
    let out = run_subcommand_args(home, &["install", &root.display().to_string()]);
    assert!(
        out.status.success(),
        "`afkd install {}` failed ({:?}):\n{}{}",
        root.display(),
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        plugins_root(home).join(NAME).is_dir(),
        "the install placed {NAME}"
    );
}

/// A fresh `$HOME` with the release installed.
fn installed() -> TempDir {
    let home = TempDir::new().expect("tempdir");
    let stage_dir = TempDir::new().expect("tempdir");
    install(home.path(), &stage(stage_dir.path()));
    home
}

// --- the scenario ------------------------------------------------------------------

/// The issue the service claims, titled and described in the shapes that break a naive
/// transport: wide glyphs, an emoji, quotes, and a multi-line body with a quoted line, a
/// fence and a wide attribution.
const TITLE: &str = "修复 the retry storm 🚨 — \"backoff\" resets";
const BODY: &str = "Retries pile up after a 502.\n\n> \"backoff\" — nobody\n\n\
                    ```rust\nlet backoff = Duration::ZERO;\n```\n\n— reported by 陳大文";

/// A human's question on the issue before afkd ever looked, which the brief must carry.
const ASK: &str = "看起来不对 🚨 — it resets on every 401:\n\n```\nPOST /graphql 401\n```";

/// The files, under the service's work dir, its slots write the issue's handle to.
const FIELDS: &str = "fields.txt";
const CLAIMED: &str = "claimed.txt";
const DONE: &str = "done.txt";
const AUTH: &str = "auth.txt";
const API_URL: &str = "api_url.txt";

/// The file, under the service's work dir, `on_run` writes the issue's description to
/// through the plugin's `read_description`.
const DESCRIPTION: &str = "description.md";

/// How long a claim, a run or a finish may take. Generous, because it bounds a failure
/// rather than timing a success: the first beat is immediate.
const BUDGET: Duration = Duration::from_secs(30);

/// The issue the service picks, what it is related to, and the state it is done in.
struct Seeded {
    fake: FakeLinear,
    issue: String,
    epic: String,
    child: String,
    blocker: String,
}

/// The team the service picks from: the states it reads, and one issue in Todo — a
/// sub-issue of an epic, with a sub-issue of its own, two labels, a blocker that is done
/// and a related issue that is not, and Chen's question on it.
fn seed() -> Seeded {
    let fake = FakeLinear::start();
    fake.state("Todo", "unstarted");
    fake.state("In Progress", "started");
    fake.state("Done", "completed");
    let chen = fake.user("陳大文", "chen", "chen@acme.example");
    let epic = fake.issue("In Progress", "Epic: 重试 that works", "");
    let blocker = fake.issue("Done", "Upgrade the client", "");
    let related = fake.issue("In Progress", "Related work", "");
    let issue = fake.issue("Todo", TITLE, BODY);
    let child = fake.issue("In Progress", "Cap the backoff at 30s", "");
    fake.set_parent(&issue, &epic);
    fake.set_parent(&child, &issue);
    fake.relate(&blocker, &issue, "blocks");
    fake.relate(&related, &issue, "related");
    for label in ["afkd/ready", "Väntar på svar"] {
        fake.label(&issue, label);
    }
    fake.comment(&issue, Author::User(chen), ASK, 1800);
    Seeded {
        fake,
        issue,
        epic,
        child,
        blocker,
    }
}

/// The service that drives the seeded issue, `home` its work dir, its credential written as
/// `auth`. It writes no `every`, so the cadence is afkd's default and its first beat is at
/// once. `on_claim` writes the state it claimed from; the run holds until the test creates
/// `release` — bounded, so a test that never does cannot wedge it — then writes every field
/// of the issue's handle to [`FIELDS`], a line each, and the endpoint and credential its env
/// carries, writes the description to [`DESCRIPTION`] and moves the issue to Done through
/// the plugin's actions; `on_done` writes the title and comments on the finished issue.
fn service(home: &Path, base_url: &str, auth: &str) -> String {
    format!(
        r#"import "core"
import "@afkd/linear"

widgets :: service(linear) {{
  team      "{TEAM}"
  base_url  "{base_url}"
  {auth}
  pick_from "Todo"

  on_claim proc(issue: linear.Issue) {{
    $ echo claimed #{{issue.state}} >> {CLAIMED}
  }}
  on_done proc(run: core.Run, issue: linear.Issue, outcome: core.Outcome) {{
    $ echo done #{{issue.title}} >> {DONE}
    landed := linear.comment(issue, "landed #{{issue.title}} ✅")
    if landed != nil {{ fail("linear.comment: #{{landed.message}}") }}
  }}

  work_dir "{home}"
  on_run proc(run: core.Run, issue: linear.Issue) {{
    $ i=0; until [ -f release ] || [ $i -ge 600 ]; do sleep 0.1; i=$((i+1)); done
    $ echo title #{{issue.title}} >> {FIELDS}
    $ echo url #{{issue.url}} >> {FIELDS}
    $ echo state #{{issue.state}} >> {FIELDS}
    $ echo parent #{{issue.parent}} >> {FIELDS}
    for label in issue.labels {{
      $ echo label #{{label}} >> {FIELDS}
    }}
    for sub in issue.sub_issues {{
      $ echo sub #{{sub}} >> {FIELDS}
    }}
    for blocker in issue.blocked_by {{
      $ echo blocked_by #{{blocker}} >> {FIELDS}
    }}
    $ printenv LINEAR_AUTHORIZATION > {AUTH}
    $ printenv LINEAR_API_URL > {API_URL}
    $ cat $AFKD_SCRATCH_DIR/task.md
    described := linear.read_description(issue, "{home}/{DESCRIPTION}")
    if described != nil {{ fail("linear.read_description: #{{described.message}}") }}
    moved := linear.move_to(issue, "Done")
    if moved != nil {{ fail("linear.move_to: #{{moved.message}}") }}
  }}
}}
"#,
        home = home.display()
    )
}

/// Write `config` as `home`'s main config.
fn write_config(home: &Path, config: &str) {
    let conf = daemon_afkd(home);
    std::fs::create_dir_all(conf.parent().expect("has parent")).expect("mk the config dir");
    std::fs::write(&conf, config).expect("write the config");
}

/// `afkd validate` over `home`'s config: the exit code and the report.
fn validate(home: &Path) -> (Option<i32>, String) {
    let out = run_subcommand_args(home, &["validate"]);
    (
        out.status.code(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

/// The service's run dirs, by name.
fn run_dirs(home: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(runs_root(home).join("widgets"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Everything a failed wait needs to say: the issue's state and thread as the fake holds
/// them, the requests it refused, the run dirs, the daemon's own log, and its stderr so far
/// — where the plugin's own complaints land.
fn dump(home: &Path, fake: &FakeLinear, issue: &str, daemon: &StreamingDaemon) -> String {
    let comments: Vec<String> = fake
        .comments(issue)
        .iter()
        .map(|c| format!("  {:?}: {:?}", c.author, c.body))
        .collect();
    format!(
        "state: {:?}\ncomments:\n{}\nrefused: {:?}\nrun dirs: {:?}\ndaemon.log:\n{}\ndaemon stderr:\n{}",
        fake.state_of(issue),
        comments.join("\n"),
        fake.invalid_requests(),
        run_dirs(home),
        std::fs::read_to_string(daemon_log(home)).unwrap_or_default(),
        daemon.stderr_so_far()
    )
}

/// Poll `ready` to [`BUDGET`], failing with `what` and the [`dump`] when it never holds.
fn wait_for(
    home: &Path,
    fake: &FakeLinear,
    issue: &str,
    daemon: &StreamingDaemon,
    what: &str,
    ready: impl Fn() -> bool,
) {
    let deadline = Instant::now() + BUDGET;
    while !ready() {
        assert!(
            Instant::now() < deadline,
            "{what} within {BUDGET:?}\n{}",
            dump(home, fake, issue, daemon)
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Whether `issue`'s thread holds a claim comment.
fn claimed(fake: &FakeLinear, issue: &str) -> bool {
    fake.comments(issue)
        .iter()
        .any(|c| c.body.starts_with("[afkd-claim]"))
}

/// Read `path` whole, naming it when it is missing.
fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Drive the seeded issue through the plugin, installed in `home`, on a real daemon, its
/// service authenticating as `auth` writes it: it is **claimed** (the claim comment, and
/// `on_claim` read its state), **run** (its run dir is named for the issue, its task carries
/// the issue and its thread whole, and `on_run` read every field of its handle, the endpoint
/// and the credential as the plugin sent them, read its description and moved it to Done
/// through the plugin) and **finished** (the `[afkd-ran]` watermark posted, the claim
/// released, and `on_done` read its title and commented on it through the plugin), and the
/// daemon then drains clean.
/// Returns the seeded team, for the leg's own checks.
fn drive_one_issue(home: &Path, auth: &str) -> Seeded {
    let seeded = seed();
    let (fake, issue) = (&seeded.fake, seeded.issue.as_str());
    write_config(home, &service(home, fake.base_url(), auth));
    let (code, report) = validate(home);
    assert_eq!(code, Some(0), "the service validates:\n{report}");

    let daemon = spawn_headless_streaming(home, &[]);
    wait_for(home, fake, issue, &daemon, "the issue is claimed", || {
        claimed(fake, issue) && home.join(CLAIMED).exists()
    });
    let identifier = fake.identifier(issue);
    let run_suffix = format!("-unit-{identifier}-1");
    let brief = || {
        run_dirs(home)
            .into_iter()
            .find(|name| name.ends_with(&run_suffix))
            .map(|run| runs_root(home).join("widgets").join(run).join("task.md"))
            .filter(|task| task.metadata().is_ok_and(|meta| meta.len() > 0))
    };
    wait_for(home, fake, issue, &daemon, "the issue's run starts", || {
        brief().is_some()
    });
    let task = read(&brief().expect("waited for"));
    let asked = format!("**陳大文:** {ASK}");
    let parent = format!(
        "Sub-issue of {}: Epic: 重试 that works",
        fake.identifier(&seeded.epic)
    );
    let sub = format!(
        "- {} (In Progress) Cap the backoff at 30s",
        fake.identifier(&seeded.child)
    );
    for part in [
        TITLE,
        "> \"backoff\" — nobody",
        "let backoff = Duration::ZERO;",
        "— reported by 陳大文",
        &parent,
        &sub,
        &asked,
    ] {
        assert!(task.contains(part), "task.md carries {part:?}:\n{task}");
    }

    std::fs::write(home.join("release"), "").expect("release the run");
    let landed = format!("landed {TITLE} ✅");
    wait_for(home, fake, issue, &daemon, "the issue is finished", || {
        let thread = fake.comments(issue);
        !claimed(fake, issue)
            && thread.iter().any(|c| c.body.starts_with("[afkd-ran]"))
            && thread.iter().any(|c| c.body == landed)
            && home.join(DONE).exists()
    });
    assert_eq!(fake.state_of(issue), "Done", "the run's move_to moved it");
    assert_eq!(
        read(&home.join(DESCRIPTION)),
        BODY,
        "the run's read_description wrote it"
    );
    assert_eq!(
        read(&home.join(API_URL)),
        format!("{}/graphql\n", fake.base_url())
    );
    assert_eq!(run_dirs(home).len(), 1, "one run, never claimed again");
    assert_eq!(read(&home.join(CLAIMED)), "claimed Todo\n");
    assert_eq!(read(&home.join(DONE)), format!("done {TITLE}\n"));
    // Every field verbatim — the `$` lines shell-quote what they interpolate.
    assert_eq!(
        read(&home.join(FIELDS)),
        format!(
            "title {TITLE}\nurl {}\nstate Todo\nparent {}\nlabel afkd/ready\nlabel Väntar på \
             svar\nsub {}\nblocked_by {}\n",
            fake.url(issue),
            fake.identifier(&seeded.epic),
            fake.identifier(&seeded.child),
            fake.identifier(&seeded.blocker),
        )
    );

    daemon.signal(libc::SIGINT);
    let out = daemon.reap(Duration::from_secs(10));
    assert!(
        out.status.success(),
        "the daemon drains clean ({:?}):\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        fake.invalid_requests(),
        Vec::<String>::new(),
        "requests Linear's schema refuses"
    );
    seeded
}

// --- the legs ----------------------------------------------------------------------

#[test]
fn install_leg_places_the_plugin_and_runs_one_issue_through_it() {
    let home = TempDir::new().expect("tempdir");
    let stage_dir = TempDir::new().expect("tempdir");
    let staged = stage(stage_dir.path());
    assert_eq!(
        tree_of(&staged),
        ["afkd-linear", "afkd-plugin.toml"],
        "the release holds the manifest and the binary, and nothing else"
    );
    install(home.path(), &staged);
    let placed = plugins_root(home.path()).join(NAME);
    assert_eq!(
        tree_of(&placed),
        tree_of(&staged),
        "what lands is exactly the release"
    );
    let exec = placed.join("afkd-linear");
    let mode = std::fs::metadata(&exec)
        .unwrap_or_else(|e| panic!("the install placed {}: {e}", exec.display()))
        .permissions()
        .mode();
    assert!(mode & 0o111 != 0, "{} is executable", exec.display());

    let seeded = drive_one_issue(home.path(), &format!("api_key   \"{API_KEY}\""));
    assert_eq!(read(&home.path().join(AUTH)), format!("{API_KEY}\n"));
    assert_eq!(seeded.fake.token_fetches(), 0, "a key fetches no token");
}

#[test]
fn app_auth_leg_runs_one_issue_with_a_fetched_token() {
    let home = installed();
    let seeded = drive_one_issue(
        home.path(),
        &format!("client_id     \"{CLIENT_ID}\"\n  client_secret \"{CLIENT_SECRET}\""),
    );
    let fake = &seeded.fake;
    assert!(fake.token_fetches() >= 1, "the app fetched its token");
    let graphql: Vec<_> = fake
        .seen()
        .into_iter()
        .filter(|s| s.path == "/graphql")
        .collect();
    assert!(!graphql.is_empty());
    for s in &graphql {
        assert!(
            s.authorization
                .as_deref()
                .is_some_and(|a| a.starts_with("Bearer lin_oauth_tok-")),
            "{} went out as {:?}",
            s.operation,
            s.authorization
        );
    }
    assert!(
        read(&home.path().join(AUTH)).starts_with("Bearer lin_oauth_tok-"),
        "the run reaches Linear as the app"
    );
    let watermark = fake
        .comments(&seeded.issue)
        .into_iter()
        .find(|c| c.body.starts_with("[afkd-ran]"))
        .expect("the watermark");
    assert_eq!(
        watermark.author,
        Author::User(fake.app_user()),
        "the app is the actor"
    );
}

/// A config that writes every setting the kind declares — both credential forms, in two
/// services — afkd's own claim keys, and all five slots as `proc`s, validates.
#[test]
fn a_service_linear_config_validates() {
    let home = installed();
    write_config(
        home.path(),
        &format!(
            r#"import "core"
import "@afkd/linear"

develop :: service(linear) {{
  team           "{TEAM}"
  base_url       "https://api.linear.app"
  api_key        "{API_KEY}"
  pick_from      "Todo", "Backlog 📥"
  require_member "me"
  require_label  "afkd/ready"
  without_label  "Hold", "Blockerat / Väntar"
  every          2m
  max_attempts   2
  work_dir       "/srv/acme/widgets"

  on_claim proc(issue: linear.Issue) {{
    $ echo #{{issue.title}} #{{issue.state}}
  }}
  on_run proc(run: core.Run, issue: linear.Issue) {{
    $ echo #{{issue.url}} #{{issue.parent}}
    for sub in issue.sub_issues {{
      $ echo #{{sub}}
    }}
  }}
  on_done proc(run: core.Run, issue: linear.Issue, outcome: core.Outcome) {{
    $ echo done #{{issue.title}}
  }}
  on_fail proc(run: core.Run, issue: linear.Issue, outcome: core.Outcome) {{
    for blocker in issue.blocked_by {{
      $ echo #{{blocker}}
    }}
  }}
  on_park proc(run: core.Run, issue: linear.Issue, outcome: core.Outcome) {{
    for label in issue.labels {{
      $ echo #{{label}}
    }}
  }}
}}

discuss :: service(linear) {{
  team          "{TEAM}"
  client_id     "{CLIENT_ID}"
  client_secret "{CLIENT_SECRET}"
  pick_from     "Triage"
  discuss_with  "chen@acme.example", "me"
  work_dir      "/srv/acme/widgets"

  on_run proc(run: core.Run, issue: linear.Issue) {{
    $ cat $AFKD_SCRATCH_DIR/task.md
  }}
}}
"#
        ),
    );
    let (code, report) = validate(home.path());
    assert_eq!(code, Some(0), "every setting validates:\n{report}");
}

/// A service whose slots call every action the plugin declares, each checked for its error —
/// a new issue in a named state and one in the team's default, a sub-issue, absolute paths
/// for the reads, a comment and a description with files and without, wide and multi-line
/// texts — validates: afkd types every call against the manifest.
#[test]
fn every_action_validates_from_its_slots() {
    let home = installed();
    write_config(
        home.path(),
        &format!(
            r###"import "core"
import "@afkd/linear"

widgets :: service(linear) {{
  team      "{TEAM}"
  api_key   "{API_KEY}"
  pick_from "Todo"
  work_dir  "/srv/acme/widgets"

  on_claim proc(issue: linear.Issue) {{
    moved := linear.move_to(issue, "In Progress 🚧")
    if moved != nil {{ fail("linear.move_to: #{{moved.message}}") }}
    labelled := linear.add_label(issue, "afkd/working")
    if labelled != nil {{ fail("linear.add_label: #{{labelled.message}}") }}
  }}
  on_run proc(run: core.Run, issue: linear.Issue) {{
    thread := linear.list_comments(issue, "/srv/acme/widgets/thread 計画.md")
    if thread != nil {{ fail("linear.list_comments: #{{thread.message}}") }}
    described := linear.read_description(issue, "/srv/acme/widgets/description.md")
    if described != nil {{ fail("linear.read_description: #{{described.message}}") }}
    subs := linear.list_sub_issues(issue, "/srv/acme/widgets/subs.md")
    if subs != nil {{ fail("linear.list_sub_issues: #{{subs.message}}") }}
    files := linear.fetch_files(issue, "/srv/acme/widgets/files 計画")
    if files != nil {{ fail("linear.fetch_files: #{{files.message}}") }}
    $ cat $AFKD_SCRATCH_DIR/task.md
  }}
  on_done proc(run: core.Run, issue: linear.Issue, outcome: core.Outcome) {{
    said := linear.comment(issue, "## 完了 ✅\n\nlanded #{{issue.title}} in #{{outcome.duration}}.")
    if said != nil {{ fail("linear.comment: #{{said.message}}") }}
    shown := linear.comment(issue, "After the fix:", files=["/srv/acme/widgets/screen shot — 修复.png", "/srv/acme/widgets/run.log"])
    if shown != nil {{ fail("linear.comment: #{{shown.message}}") }}
    described := linear.describe(issue, "Done by afkd — 修复 🚀")
    if described != nil {{ fail("linear.describe: #{{described.message}}") }}
    pictured := linear.describe(issue, "", files=["/srv/acme/widgets/before.png"])
    if pictured != nil {{ fail("linear.describe: #{{pictured.message}}") }}
    renamed := linear.rename(issue, "[done] #{{issue.title}}")
    if renamed != nil {{ fail("linear.rename: #{{renamed.message}}") }}
    unlabelled := linear.remove_label(issue, "afkd/working")
    if unlabelled != nil {{ fail("linear.remove_label: #{{unlabelled.message}}") }}
    for sub in issue.sub_issues {{
      closed := linear.move_sub_issue(issue, sub, "Done")
      if closed != nil {{ fail("linear.move_sub_issue: #{{closed.message}}") }}
    }}
    archived := linear.archive(issue)
    if archived != nil {{ fail("linear.archive: #{{archived.message}}") }}
  }}
  on_fail proc(run: core.Run, issue: linear.Issue, outcome: core.Outcome) {{
    filed := linear.create_issue(issue, "Follow up on #{{issue.title}}", "It failed:\n\n#{{outcome.duration}}", state="Backlog 📥")
    if filed != nil {{ fail("linear.create_issue: #{{filed.message}}") }}
    bare := linear.create_issue(issue, "Look again 👀")
    if bare != nil {{ fail("linear.create_issue: #{{bare.message}}") }}
  }}
  on_park proc(run: core.Run, issue: linear.Issue, outcome: core.Outcome) {{
    split := linear.create_sub_issue(issue, "Answer the question 🙋", "Asked on #{{issue.url}}")
    if split != nil {{ fail("linear.create_sub_issue: #{{split.message}}") }}
  }}
}}
"###
        ),
    );
    let (code, report) = validate(home.path());
    assert_eq!(code, Some(0), "every action validates:\n{report}");
}

/// The screenshot the files leg's run puts up: spaces, an em dash and wide CJK.
const SHOT: &str = "screen shot — 修复.png";

/// The marker the files leg's run writes once it has fetched its files.
const FETCHED: &str = "fetched.txt";

/// The files leg's service: its run puts [`SHOT`] into the issue's description and into a
/// comment, fetches the issue's files into `fetched/`, moves it to Done and writes
/// [`FETCHED`], each action checked for its error.
fn files_service(home: &Path, base_url: &str) -> String {
    format!(
        r###"import "core"
import "@afkd/linear"

widgets :: service(linear) {{
  team      "{TEAM}"
  base_url  "{base_url}"
  api_key   "{API_KEY}"
  pick_from "Todo"

  work_dir "{home}"
  on_run proc(run: core.Run, issue: linear.Issue) {{
    described := linear.describe(issue, "## 背景\n\nBefore:", files=["{home}/{SHOT}"])
    if described != nil {{ fail("linear.describe: #{{described.message}}") }}
    said := linear.comment(issue, "After the fix ✅", files=["{home}/{SHOT}"])
    if said != nil {{ fail("linear.comment: #{{said.message}}") }}
    $ mkdir -p fetched
    fetched := linear.fetch_files(issue, "{home}/fetched")
    if fetched != nil {{ fail("linear.fetch_files: #{{fetched.message}}") }}
    moved := linear.move_to(issue, "Done")
    if moved != nil {{ fail("linear.move_to: #{{moved.message}}") }}
    $ echo fetched > {FETCHED}
  }}
}}
"###,
        home = home.display()
    )
}

/// A run puts a PNG on its issue through a real daemon — into the description and into a
/// comment, each embedding it at its asset url — and fetches both back byte for byte, the
/// description's first; every request conforms, and the PUTs carried no credential.
#[test]
fn files_leg_a_run_puts_a_png_on_its_issue_and_fetches_it_back() {
    let home = installed();
    let home = home.path();
    let seeded = seed();
    let (fake, issue) = (&seeded.fake, seeded.issue.as_str());
    std::fs::write(home.join(SHOT), png()).expect("write the screenshot");
    write_config(home, &files_service(home, fake.base_url()));
    let (code, report) = validate(home);
    assert_eq!(code, Some(0), "the service validates:\n{report}");

    let daemon = spawn_headless_streaming(home, &[]);
    wait_for(
        home,
        fake,
        issue,
        &daemon,
        "the run fetched its files",
        || home.join(FETCHED).exists(),
    );
    let fetched = home.join("fetched");
    let mut names: Vec<String> = std::fs::read_dir(&fetched)
        .expect("the fetched dir")
        .map(|e| {
            e.expect("an entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    assert_eq!(names, [format!("1-{SHOT}"), format!("2-{SHOT}")]);
    for name in &names {
        assert_eq!(
            std::fs::read(fetched.join(name)).expect("a fetched file"),
            png(),
            "{name} byte for byte"
        );
    }
    let description = fake.description_of(issue);
    let in_description = fake.asset_urls_of(&description);
    assert_eq!(
        description,
        format!("## 背景\n\nBefore:\n\n![{SHOT}]({})", in_description[0])
    );
    let comment = fake
        .comments(issue)
        .into_iter()
        .find(|c| c.body.starts_with("After the fix ✅"))
        .expect("the comment");
    let in_comment = fake.asset_urls_of(&comment.body);
    assert_eq!(
        comment.body,
        format!("After the fix ✅\n\n![{SHOT}]({})", in_comment[0])
    );
    for url in [&in_description[0], &in_comment[0]] {
        assert_eq!(fake.file(url), Some(png()), "{url} holds the PNG");
    }
    let puts = fake.puts();
    assert_eq!(puts.len(), 2);
    assert!(puts.iter().all(|p| p.header("authorization").is_none()));

    daemon.signal(libc::SIGINT);
    let out = daemon.reap(Duration::from_secs(10));
    assert!(
        out.status.success(),
        "the daemon drains clean ({:?}):\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        fake.invalid_requests(),
        Vec::<String>::new(),
        "requests Linear's schema refuses"
    );
}

/// `team` and `pick_from` are required: a service without either is refused at validate,
/// naming the setting and the service.
#[test]
fn a_service_without_team_or_pick_from_fails_validate_naming_the_setting() {
    let home = installed();
    for (missing, keep) in [
        ("team", "pick_from \"Todo\""),
        ("pick_from", "team \"ENG\""),
    ] {
        write_config(
            home.path(),
            &format!(
                r#"import "core"
import "@afkd/linear"

widgets :: service(linear) {{
  {keep}
  api_key "{API_KEY}"
  work_dir "/srv/acme/widgets"
  on_run proc(run: core.Run, issue: linear.Issue) {{
    $ true
  }}
}}
"#
            ),
        );
        let (code, report) = validate(home.path());
        assert_eq!(
            code,
            Some(1),
            "a service without {missing} is refused:\n{report}"
        );
        assert!(
            report.contains(&format!(
                r#"missing required setting "{missing}" for service(linear)"#
            )),
            "the refusal names the setting and the service:\n{report}"
        );
    }
}

/// A slot calling an action the plugin does not declare is a load error naming the action
/// and the plugin, and listing the actions it has; the same file without the call is valid.
#[test]
fn a_call_to_an_undeclared_action_is_a_load_error() {
    let home = installed();
    let config = |call: &str| {
        format!(
            r#"import "core"
import "@afkd/linear"

widgets :: service(linear) {{
  team      "ENG"
  api_key   "{API_KEY}"
  pick_from "Todo"

  on_done proc(run: core.Run, issue: linear.Issue, outcome: core.Outcome) {{
    {call}
  }}

  work_dir "/srv/acme/widgets"
  on_run proc(run: core.Run, issue: linear.Issue) {{
    $ true
  }}
}}
"#
        )
    };
    write_config(home.path(), &config(r#"linear.assign(issue, "me")"#));
    let (code, report) = validate(home.path());
    assert_eq!(
        code,
        Some(1),
        "a call of an undeclared action is refused:\n{report}"
    );
    assert!(
        report.contains(r#"plugin linear has no action "assign""#),
        "the refusal names the action and the plugin as the file imports it:\n{report}"
    );
    assert!(
        report.contains("move_sub_issue"),
        "and lists the actions it has:\n{report}"
    );
    write_config(home.path(), &config(r#"$ echo #{issue.title}"#));
    let (code, report) = validate(home.path());
    assert_eq!(
        code,
        Some(0),
        "the slot without the call validates:\n{report}"
    );
}
