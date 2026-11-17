//! The gate that holds the **shipped `@afkd/linear` plugin** — a manifest v2 provider whose
//! main kind is `service(linear)`, shipped as a finished binary — to a real afkd: the `afkd`
//! first on `PATH`, never a build, since a plugin is checked against the afkd it will meet
//! ([`bin_path`]).
//!
//! - The **install** leg installs the tree the release holds and runs one issue through it
//!   on a real daemon with an API key, every slot passed the `linear.Workspace` the service's
//!   credential settings build: `on_claim` moving the issue with `update_issue`, the run
//!   reading the fields of the issue's handle, its comments and its description through the
//!   workspace and moving it to Done, and `on_done` commenting on the finished issue with
//!   `create_comment`, every request with the service's key; the **app** leg does the same as
//!   an OAuth app, on a token the plugin fetched, claiming the issue nobody delegated with
//!   `only_delegate "me", "none"` and delegating it to the app from `on_claim`.
//! - The **reads** leg runs an interval service by hand whose run reads a workspace through
//!   children afkd starts for each call — the viewer, the teams, a team's issues, an issue's
//!   comments and files and a file downloaded — and reads the issue again once it has moved
//!   team, the two handles `==`.
//! - The **writes** leg runs an interval service by hand whose run writes a workspace through
//!   children afkd starts for each call: it moves an issue to the top of another state,
//!   relates it to a blocker, comments on it with a screenshot, resolves the comment, and
//!   replies to it and deletes the reply.
//! - The **two workspaces** leg runs a daily service by hand whose run holds two
//!   `linear.Workspace` values, one per fake: it lists each one's issues, files an issue in
//!   one, updates it, moves it, labels it, relates it to an issue of that workspace and
//!   comments on it, and has the issue's handle refused when passed with the other workspace.
//! - The **files** leg runs one issue whose run puts a PNG into its description and into a
//!   comment through `files=`, and downloads both back through `files` and `download_file`,
//!   byte for byte.
//! - The **validate** legs hold `afkd validate` to a `service(linear)` that writes every
//!   setting in both credential forms, to one whose every slot reads and writes through the
//!   workspace it is passed, to services calling every read and every write with every
//!   parameter, to refusing one without `team` or `claim_from`, and to refusing a call of an
//!   action the plugin does not declare.
//! - The **README** leg validates every `conf` fence of the plugin's README, each a whole
//!   entry file, against the installed release, with every variable the fences read set by
//!   [`CONFIG_ENV`].
//! - The **skill** leg runs one issue whose run works it through the installed `linear`
//!   skill — a comment, then a question that parks the run — so the env a real daemon hands
//!   a run is the env the skill's scripts read, and the issue parks as the plugin parks it.
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

use common::fake::{
    png, Author, FakeLinear, API_KEY, CLIENT_ID, CLIENT_SECRET, GLOBEX_API_KEY, TEAM,
};
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

/// Stage the release `tools/pack-release` packs under `dst` — the manifest, the binary and
/// the skill, exactly what the release archive holds — and return the staged root.
fn stage(dst: &Path) -> PathBuf {
    let status = Command::new(plugin_root().join("../../tools/pack-release"))
        .arg("linear")
        .arg(dst)
        .arg(built_binary())
        .status()
        .expect("run tools/pack-release");
    assert!(status.success(), "tools/pack-release linear");
    let staged = dst.join("afkd-linear");
    for present in [
        "afkd-plugin.toml",
        "afkd-linear",
        "skills/linear/SKILL.md",
        "skills/linear/lib/linear_api.py",
        "skills/linear/lib/operations.graphql",
    ] {
        assert!(staged.join(present).exists(), "the release holds {present}");
    }
    for absent in [
        "Cargo.toml",
        "Cargo.lock",
        "src",
        "tests",
        "README.md",
        "target",
        "skills/linear/lib/__pycache__",
    ] {
        assert!(
            !staged.join(absent).exists(),
            "the release holds no {absent}"
        );
    }
    staged
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
/// The file, under the reads leg's work dir, its run writes what it read to.
const READS: &str = "reads.txt";
const CLAIMED: &str = "claimed.txt";
const DONE: &str = "done.txt";
const AUTH: &str = "auth.txt";
const API_URL: &str = "api_url.txt";

/// The file, under the service's work dir, `on_run` writes the issue's description to, as a
/// read through the workspace finds it.
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

/// The service that drives the seeded issue, `home` its work dir, its credential and any gate
/// written as `keys`. It writes no `every`, so the cadence is afkd's default and its first
/// beat is at once. `on_claim` writes the state it claimed from, moves the issue to In
/// Progress through the workspace it is passed and writes the state that gave back, then runs
/// the statements `claim` holds, a line each; the run holds until the test creates `release`
/// — bounded, so a test that never does cannot wedge it — then writes fields of the issue's
/// handle to [`FIELDS`], a line each, and how many comments a read of it through the
/// workspace finds, the endpoint and credential its env carries, writes the description a
/// read through the workspace finds to [`DESCRIPTION`] and moves the issue to Done;
/// `on_done` writes the title and comments on the finished issue. It keeps every run,
/// because [`drive_one_issue`] counts the finished run's dir, where the default `keep_runs
/// on_fail` leaves nothing of a clean run.
fn service(home: &Path, base_url: &str, keys: &str, claim: &str) -> String {
    format!(
        r#"import "afkd"
import "@afkd/linear"

widgets :: service(linear) {{
  {keys}
  base_url   "{base_url}"
  team       "{TEAM}"
  claim_from "Todo"
  keep_runs  always

  on_claim proc(workspace: linear.Workspace, issue: linear.Issue) {{
    $ echo claimed #{{issue.state}} >> {CLAIMED}
    match linear.update_issue(workspace, issue, state="In Progress") {{
      .ok(started) => {{
        $ echo started #{{started.state}} >> {CLAIMED}
      }}
      .err(e) => fail("linear.update_issue: #{{e.message}}")
    }}
{claim}  }}
  on_done proc(run: afkd.Run, workspace: linear.Workspace, issue: linear.Issue, outcome: afkd.Outcome) {{
    $ echo done #{{issue.title}} >> {DONE}
    match linear.create_comment(workspace, issue, "landed #{{issue.title}} ✅") {{
      .ok(_) => {{ }}
      .err(e) => fail("linear.create_comment: #{{e.message}}")
    }}
  }}

  work_dir "{home}"
  on_run proc(run: afkd.Run, workspace: linear.Workspace, issue: linear.Issue) {{
    $ i=0; until [ -f release ] || [ $i -ge 600 ]; do sleep 0.1; i=$((i+1)); done
    $ echo uuid #{{issue.uuid}} >> {FIELDS}
    $ echo workspace #{{issue.workspace}} team #{{issue.team}} >> {FIELDS}
    $ echo title #{{issue.title}} >> {FIELDS}
    $ echo url #{{issue.url}} >> {FIELDS}
    $ echo state #{{issue.state}} #{{issue.state_type}} >> {FIELDS}
    $ echo priority #{{issue.priority}} >> {FIELDS}
    $ echo parent #{{issue.parent ?? ""}} >> {FIELDS}
    for label in issue.labels {{
      $ echo label #{{label}} >> {FIELDS}
    }}
    for sub in issue.sub_issues {{
      $ echo sub #{{sub}} >> {FIELDS}
    }}
    for blocker in issue.blocked_by {{
      $ echo blocked_by #{{blocker}} >> {FIELDS}
    }}
    match linear.comments(workspace, issue) {{
      .ok(comments) => {{
        $ echo comments #{{len(comments)}} >> {FIELDS}
      }}
      .err(e) => fail("linear.comments: #{{e.message}}")
    }}
    $ printenv LINEAR_AUTHORIZATION > {AUTH}
    $ printenv LINEAR_API_URL > {API_URL}
    $ cat $AFKD_SCRATCH_DIR/task.md
    match linear.issues(workspace, id=[ issue.uuid ]) {{
      .ok(found) => {{
        for fresh in found {{
          $ printf %s #{{fresh.description}} > {DESCRIPTION}
        }}
      }}
      .err(e) => fail("linear.issues: #{{e.message}}")
    }}
    match linear.update_issue(workspace, issue, state="Done") {{
      .ok(_) => {{ }}
      .err(e) => fail("linear.update_issue: #{{e.message}}")
    }}
  }}
}}
"#,
        home = home.display()
    )
}

/// Write `config` as `home`'s main config.
fn write_config(home: &Path, config: &str) {
    let conf = daemon_afk(home);
    std::fs::create_dir_all(conf.parent().expect("has parent")).expect("mk the config dir");
    std::fs::write(&conf, config).expect("write the config");
}

/// The credentials a config reads from the daemon's environment, as the README's examples
/// do: the API key, and the OAuth app's client id and secret.
const CONFIG_ENV: &[(&str, &str)] = &[
    ("LINEAR_API_KEY", API_KEY),
    ("LINEAR_CLIENT_ID", CLIENT_ID),
    ("LINEAR_CLIENT_SECRET", CLIENT_SECRET),
];

/// `afkd validate` over `home`'s config, with [`CONFIG_ENV`] set: the exit code and the
/// report.
fn validate(home: &Path) -> (Option<i32>, String) {
    let out = run_subcommand_env(home, &["validate"], CONFIG_ENV);
    (
        out.status.code(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

/// The service's run dirs, sorted by name: the kept ones under `<state>/runs`, then the ones in
/// flight under the daemon's `$TMPDIR`. A run of the default `keep_runs on_fail` lives in the
/// temp root until it ends, and only a failed or parked one reaches the kept root; a name in
/// both, a run caught mid-move, is the kept one.
fn run_dirs(home: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    for root in [runs_root(home), live_runs_root(home)] {
        for entry in std::fs::read_dir(root.join("widgets"))
            .into_iter()
            .flatten()
            .flatten()
        {
            if !dirs
                .iter()
                .any(|dir| dir.file_name() == Some(&entry.file_name()))
            {
                dirs.push(entry.path());
            }
        }
    }
    dirs.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    dirs
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
/// service writing `keys` and its `on_claim` running `claim` (see [`service`]): it is
/// **claimed** (the claim comment, and `on_claim` read its state and moved it through the
/// workspace it is passed), **run** (its run dir is named for the issue, its task carries
/// the issue and its thread whole, and `on_run` read the fields of its handle, the endpoint
/// and the credential as the plugin sent them, read its comments and its description through
/// the workspace, and moved it to Done) and **finished** (the `[afkd-ran]` watermark posted,
/// the claim released, and `on_done` read its title and commented on it through the
/// workspace), and the daemon then drains clean.
/// Returns the seeded team, for the leg's own checks.
fn drive_one_issue(home: &Path, keys: &str, claim: &str) -> Seeded {
    let seeded = seed();
    let (fake, issue) = (&seeded.fake, seeded.issue.as_str());
    write_config(home, &service(home, fake.base_url(), keys, claim));
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
            .find(|dir| {
                dir.file_name()
                    .is_some_and(|name| name.to_string_lossy().ends_with(&run_suffix))
            })
            .map(|dir| dir.join("task.md"))
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
    assert_eq!(
        fake.state_of(issue),
        "Done",
        "the run's update_issue moved it"
    );
    assert_eq!(
        read(&home.join(DESCRIPTION)),
        BODY,
        "the run read it through the workspace"
    );
    assert_eq!(
        read(&home.join(API_URL)),
        format!("{}/graphql\n", fake.base_url())
    );
    assert_eq!(run_dirs(home).len(), 1, "one run, never claimed again");
    assert_eq!(
        read(&home.join(CLAIMED)),
        "claimed Todo\nstarted In Progress\n"
    );
    assert_eq!(read(&home.join(DONE)), format!("done {TITLE}\n"));
    // Every field verbatim — the `$` lines shell-quote what they interpolate. The thread a
    // read finds is Chen's question and the claim.
    assert_eq!(
        read(&home.join(FIELDS)),
        format!(
            "uuid {issue}\nworkspace acme team ENG\ntitle {TITLE}\nurl {}\n\
             state Todo unstarted\npriority none\nparent {}\nlabel afkd/ready\n\
             label Väntar på svar\nsub {}\nblocked_by {}\ncomments 2\n",
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
    let mut shipped = vec!["afkd-linear".to_string(), "afkd-plugin.toml".to_string()];
    shipped.extend(
        tree_of(&plugin_root().join("skills"))
            .into_iter()
            .filter(|path| !path.contains("__pycache__"))
            .map(|path| format!("skills/{path}")),
    );
    shipped.push("skills".to_string());
    shipped.sort();
    assert_eq!(
        tree_of(&staged),
        shipped,
        "the release holds the manifest, the binary and the skill, and nothing else"
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

    let seeded = drive_one_issue(home.path(), &format!("api_key    \"{API_KEY}\""), "");
    assert_eq!(read(&home.path().join(AUTH)), format!("{API_KEY}\n"));
    let fake = &seeded.fake;
    assert_eq!(fake.token_fetches(), 0, "a key fetches no token");
    let writes: Vec<_> = fake
        .seen()
        .into_iter()
        .filter(|s| ["IssueWrite", "CommentPost"].contains(&s.operation.as_str()))
        .collect();
    assert_eq!(
        writes.len(),
        3,
        "on_claim's move, the run's and on_done's comment: {:?}",
        writes.iter().map(|s| &s.operation).collect::<Vec<_>>()
    );
    for s in &writes {
        assert_eq!(
            s.authorization.as_deref(),
            Some(API_KEY),
            "{} went out with the service's key",
            s.operation
        );
    }
}

#[test]
fn app_auth_leg_runs_one_issue_with_a_fetched_token() {
    let home = installed();
    let seeded = drive_one_issue(
        home.path(),
        &format!(
            "client_id     \"{CLIENT_ID}\"\n  client_secret \"{CLIENT_SECRET}\"\n  \
             only_delegate \"me\", \"none\""
        ),
        "    match linear.update_issue(workspace, issue, delegate=\"me\") {\n      \
         .ok(_) => { }\n      \
         .err(e) => fail(\"linear.update_issue: #{e.message}\")\n    \
         }\n",
    );
    let fake = &seeded.fake;
    assert!(fake.token_fetches() >= 1, "the app fetched its token");
    assert_eq!(
        fake.delegate_of(&seeded.issue),
        Some(fake.app_user()),
        "`only_delegate \"me\", \"none\"` claimed the issue nobody delegated and `on_claim` \
         delegated it to the app"
    );
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
/// services, an `only_*` of every type, enum members, numbers and wide text — afkd's own
/// claim keys, and all five slots as `proc`s, each passed the workspace, validates.
#[test]
fn a_service_linear_config_validates() {
    let home = installed();
    write_config(
        home.path(),
        &format!(
            r#"import "afkd"
import "@afkd/linear"

develop :: service(linear) {{
  api_key         "{API_KEY}"
  client_id       nil
  base_url        "https://api.linear.app"
  team            "{TEAM}", "OPS"
  claim_from      "Todo", "Backlog 📥"
  only_state_type .unstarted, .backlog
  only_priority   .urgent, .high, .none
  only_labels     "afkd/ready", "Väntar på svar"
  only_assignee   "me", "none"
  only_delegate   "me", "none"
  only_project    "Trackers 🧭"
  only_milestone  "B — reads"
  only_cycle      12, 13
  only_parent     "ENG-400"
  only_text       "retry \"storm\""
  skip_labels     "Hold", "Blockerat / Väntar"
  every           2m
  max_attempts    2
  work_dir        "/srv/acme/widgets"

  on_claim proc(workspace: linear.Workspace, issue: linear.Issue) {{
    $ echo #{{issue.title}} #{{issue.state}} #{{workspace.base_url}}
  }}
  on_run proc(run: afkd.Run, workspace: linear.Workspace, issue: linear.Issue) {{
    $ echo #{{issue.url}} #{{issue.parent ?? ""}}
    for sub in issue.sub_issues {{
      $ echo #{{sub}}
    }}
  }}
  on_done proc(run: afkd.Run, workspace: linear.Workspace, issue: linear.Issue, outcome: afkd.Outcome) {{
    $ echo done #{{issue.title}}
  }}
  on_fail proc(run: afkd.Run, workspace: linear.Workspace, issue: linear.Issue, outcome: afkd.Outcome) {{
    for blocker in issue.blocked_by {{
      $ echo #{{blocker}}
    }}
  }}
  on_park proc(run: afkd.Run, workspace: linear.Workspace, issue: linear.Issue, outcome: afkd.Outcome) {{
    for label in issue.labels {{
      $ echo #{{label}}
    }}
  }}
}}

discuss :: service(linear) {{
  client_id     "{CLIENT_ID}"
  client_secret "{CLIENT_SECRET}"
  team          "{TEAM}"
  claim_from    "Triage"
  discuss_with  "chen@acme.example", "me"
  work_dir      "/srv/acme/widgets"

  on_run proc(run: afkd.Run, workspace: linear.Workspace, issue: linear.Issue) {{
    $ cat $AFKD_SCRATCH_DIR/task.md
  }}
}}
"#
        ),
    );
    let (code, report) = validate(home.path());
    assert_eq!(code, Some(0), "every setting validates:\n{report}");
}

/// A service whose every slot reads and writes the issue it claimed through the workspace it
/// is passed, each call checked for its error — its comments and files, a file downloaded,
/// the issue moved, relabelled through `update_issue`'s other fields, delegated and commented
/// on with files and without, wide and multi-line texts — validates: afkd types every slot's
/// `workspace` as a `linear.Workspace`, and every call against the manifest.
#[test]
fn every_slot_reads_and_writes_through_its_workspace() {
    let home = installed();
    write_config(
        home.path(),
        &format!(
            r###"import "afkd"
import "@afkd/linear"

widgets :: service(linear) {{
  team       "{TEAM}"
  api_key    "{API_KEY}"
  claim_from "Todo"
  work_dir   "/srv/acme/widgets"

  on_claim proc(workspace: linear.Workspace, issue: linear.Issue) {{
    match linear.update_issue(workspace, issue, state="In Progress 🚧", at=.top, delegate="me") {{
      .ok(_) => {{ }}
      .err(e) => fail("linear.update_issue: #{{e.message}}")
    }}
    match linear.add_subscriber(workspace, issue) {{
      .ok(_) => {{ }}
      .err(e) => fail("linear.add_subscriber: #{{e.message}}")
    }}
  }}
  on_run proc(run: afkd.Run, workspace: linear.Workspace, issue: linear.Issue) {{
    match linear.comments(workspace, issue) {{
      .ok(comments) => {{
        for comment in comments {{
          $ echo #{{comment.author ?? "-"}} #{{comment.body}}
        }}
      }}
      .err(e) => fail("linear.comments: #{{e.message}}")
    }}
    match linear.files(workspace, issue) {{
      .ok(urls) => {{
        for i, url in urls {{
          match linear.download_file(workspace, url, "/srv/acme/widgets/files 計画/#{{i + 1}}") {{
            .ok(_) => {{ }}
            .err(e) => fail("linear.download_file: #{{e.message}}")
          }}
        }}
      }}
      .err(e) => fail("linear.files: #{{e.message}}")
    }}
    $ cat $AFKD_SCRATCH_DIR/task.md
  }}
  on_done proc(run: afkd.Run, workspace: linear.Workspace, issue: linear.Issue, outcome: afkd.Outcome) {{
    match linear.create_comment(workspace, issue, "## 完了 ✅\n\nlanded #{{issue.title}} in #{{outcome.duration}}.") {{
      .ok(_) => {{ }}
      .err(e) => fail("linear.create_comment: #{{e.message}}")
    }}
    match linear.create_comment(
      workspace,
      issue,
      "After the fix:",
      files=[ "/srv/acme/widgets/screen shot — 修复.png", "/srv/acme/widgets/run.log" ],
    ) {{
      .ok(_) => {{ }}
      .err(e) => fail("linear.create_comment: #{{e.message}}")
    }}
    match linear.update_issue(
      workspace,
      issue,
      title="[done] #{{issue.title}}",
      description="Done by afkd — 修复 🚀",
      files=[ "/srv/acme/widgets/before.png" ],
      state="Done",
    ) {{
      .ok(_) => {{ }}
      .err(e) => fail("linear.update_issue: #{{e.message}}")
    }}
    match linear.archive_issue(workspace, issue) {{
      .ok(_) => {{ }}
      .err(e) => fail("linear.archive_issue: #{{e.message}}")
    }}
  }}
  on_fail proc(run: afkd.Run, workspace: linear.Workspace, issue: linear.Issue, outcome: afkd.Outcome) {{
    match linear.update_issue(workspace, issue, delegate=nil, priority=.urgent) {{
      .ok(_) => {{ }}
      .err(e) => fail("linear.update_issue: #{{e.message}}")
    }}
    match linear.create_comment(workspace, issue, "It failed:\n\n#{{outcome.duration}}") {{
      .ok(_) => {{ }}
      .err(e) => fail("linear.create_comment: #{{e.message}}")
    }}
  }}
  on_park proc(run: afkd.Run, workspace: linear.Workspace, issue: linear.Issue, outcome: afkd.Outcome) {{
    match linear.issues(workspace, parent=[ issue.id ]) {{
      .ok(subs) => {{
        for sub in subs {{
          match linear.add_relation(workspace, issue, .blocked_by, sub) {{
            .ok(_) => {{ }}
            .err(e) => fail("linear.add_relation: #{{e.message}}")
          }}
        }}
      }}
      .err(e) => fail("linear.issues: #{{e.message}}")
    }}
  }}
}}
"###
        ),
    );
    let (code, report) = validate(home.path());
    assert_eq!(code, Some(0), "every slot validates:\n{report}");
}

/// Services calling every read the plugin declares with every parameter it takes, each
/// checked for its error, validate: an interval service reading a workspace by API key and
/// as an app — every `issues` filter, its order and limit, an issue's comments and files and
/// a file downloaded, the viewer, users, teams, each team's states and cycles, labels and
/// projects with a team and without — and a `service(linear)`'s slot reading the issue it
/// claimed and comparing handles. afkd types every argument, field and result against the
/// manifest.
#[test]
fn every_read_validates() {
    let home = installed();
    write_config(
        home.path(),
        &format!(
            r###"import "afkd"
import "std:time"
import "svc:interval"
import "@afkd/linear"

WS  :: linear.Workspace{{ api_key: "{API_KEY}" }}
APP :: linear.Workspace{{
  client_id: "{CLIENT_ID}",
  client_secret: "{CLIENT_SECRET}",
  base_url: "https://api.linear.app",
}}

nightly :: service(interval) {{
  every    1h
  work_dir "/srv/acme/widgets"

  on_run proc(run: afkd.Run) {{
    match linear.issues(
      WS,
      id=[ "ENG-1", "0f6c1d2e-3b4a-4c5d-8e9f-a0b1c2d3e4f5" ],
      team=[ "ENG", "OPS" ],
      state=[ "Todo", "In Progress 🚧" ],
      state_type=[ .unstarted, .started ],
      priority=[ .urgent, .high, .none ],
      labels=[ "afkd/ready" ],
      without_labels=[ "Hold", "Blockerat / Väntar" ],
      assignee=[ "me", "none", "chen@acme.example" ],
      delegate=[ "陳大文" ],
      project=[ "Trackers 🧭" ],
      milestone=[ "B — reads" ],
      cycle=[ 12, 13 ],
      parent=[ "ENG-400" ],
      blocked=false,
      text="retry \"storm\"",
      created_after=time.from_unix(1793000000),
      created_before=time.now(),
      updated_after=time.from_unix(1793000000),
      updated_before=time.now(),
      completed_after=time.from_unix(1793000000),
      completed_before=time.now(),
      canceled_after=time.from_unix(1793000000),
      canceled_before=time.now(),
      due_after="2026-01-01",
      due_before="2026-12-31",
      include_archived=true,
      order=[ .priority, .created, .manual ],
      limit=250,
    ) {{
      .ok(issues) => {{
        for issue in issues {{
          if issue.priority == .urgent && issue.state_type != .canceled && !issue.archived {{
            $ echo #{{issue.id}} #{{issue.title}} #{{issue.due ?? "-"}} #{{issue.estimate ?? 0}}
          }}
          $ echo #{{issue.created_at}} #{{issue.completed_at ?? issue.updated_at}}
          $ echo #{{issue.cycle ?? 0}}
          match linear.comments(WS, issue) {{
            .ok(comments) => {{
              for comment in comments {{
                $ echo #{{comment.author ?? "integration"}} #{{comment.body}}
                $ echo #{{comment.created_at}}
                if comment.reply_to != nil {{
                  $ echo reply #{{comment.issue}} #{{comment.url}}
                }}
              }}
            }}
            .err(e) => fail("linear.comments: #{{e.message}}")
          }}
          match linear.files(WS, issue) {{
            .ok(urls) => {{
              for url in urls {{
                match linear.download_file(WS, url, "/srv/acme/widgets/files/#{{issue.id}}.bin") {{
                  .ok(_) => {{ }}
                  .err(e) => fail("linear.download_file: #{{e.message}}")
                }}
              }}
            }}
            .err(e) => fail("linear.files: #{{e.message}}")
          }}
        }}
      }}
      .err(e) => fail("linear.issues: #{{e.message}}")
    }}
    match linear.issues(WS) {{
      .ok(_) => {{ }}
      .err(e) => fail("linear.issues: #{{e.message}}")
    }}
    match linear.issue(WS, "ENG-1") {{
      .ok(one) => {{
        $ echo #{{one.id}} #{{one.uuid}} #{{one.workspace}} #{{one.parent ?? "-"}}
        match linear.issue(APP, one.uuid) {{
          .ok(again) => {{
            if again != one {{
              $ echo #{{again.id}} is #{{one.id}} of another workspace
            }}
          }}
          .err(e) => fail("linear.issue: #{{e.message}}")
        }}
      }}
      .err(e) => fail("linear.issue: #{{e.message}}")
    }}
    match linear.me(APP) {{
      .ok(me) => {{
        $ echo #{{me.name}} #{{me.full_name}} #{{me.email}} #{{me.app}}
      }}
      .err(e) => fail("linear.me: #{{e.message}}")
    }}
    match linear.users(WS) {{
      .ok(users) => {{
        for user in users {{
          $ echo #{{user.id}} #{{user.name}}
        }}
      }}
      .err(e) => fail("linear.users: #{{e.message}}")
    }}
    match linear.teams(WS) {{
      .ok(teams) => {{
        for team in teams {{
          match linear.states(WS, team.id) {{
            .ok(states) => {{
              for state in states {{
                if state.type == .completed {{
                  $ echo #{{state.team}} #{{state.name}}
                }}
              }}
            }}
            .err(e) => fail("linear.states: #{{e.message}}")
          }}
          match linear.cycles(WS, team.id) {{
            .ok(cycles) => {{
              for cycle in cycles {{
                if cycle.active {{
                  $ echo #{{cycle.number}} #{{cycle.name ?? "-"}}
                  $ echo #{{cycle.starts_at}} #{{cycle.ends_at}}
                }}
              }}
            }}
            .err(e) => fail("linear.cycles: #{{e.message}}")
          }}
        }}
      }}
      .err(e) => fail("linear.teams: #{{e.message}}")
    }}
    match linear.labels(WS) {{
      .ok(labels) => {{
        for label in labels {{
          $ echo #{{label.name}} #{{label.group ?? "-"}} #{{label.team ?? "workspace"}}
        }}
      }}
      .err(e) => fail("linear.labels: #{{e.message}}")
    }}
    match linear.labels(WS, team="ENG") {{
      .ok(_) => {{ }}
      .err(e) => fail("linear.labels: #{{e.message}}")
    }}
    match linear.projects(WS) {{
      .ok(projects) => {{
        for project in projects {{
          $ echo #{{project.name}} #{{project.url}}
          for milestone in project.milestones {{
            $ echo #{{milestone}}
          }}
        }}
      }}
      .err(e) => fail("linear.projects: #{{e.message}}")
    }}
    match linear.projects(WS, team="OPS") {{
      .ok(_) => {{ }}
      .err(e) => fail("linear.projects: #{{e.message}}")
    }}
  }}
}}

widgets :: service(linear) {{
  team       "{TEAM}"
  api_key    "{API_KEY}"
  claim_from "Todo"
  work_dir   "/srv/acme/widgets"

  on_run proc(run: afkd.Run, workspace: linear.Workspace, issue: linear.Issue) {{
    match linear.comments(workspace, issue) {{
      .ok(comments) => {{
        $ echo #{{len(comments)}}
      }}
      .err(e) => fail("linear.comments: #{{e.message}}")
    }}
    match linear.files(workspace, issue) {{
      .ok(urls) => {{
        $ echo #{{len(urls)}}
      }}
      .err(e) => fail("linear.files: #{{e.message}}")
    }}
    match linear.issues(workspace, parent=[ issue.id ]) {{
      .ok(subs) => {{
        for sub in subs {{
          if sub == issue {{
            fail("an issue is its own sub-issue")
          }}
        }}
      }}
      .err(e) => fail("linear.issues: #{{e.message}}")
    }}
    match linear.issue(workspace, issue.uuid) {{
      .ok(anew) => {{
        $ echo #{{anew.id}} #{{anew.state}}
      }}
      .err(e) => fail("linear.issue: #{{e.message}}")
    }}
  }}
}}
"###
        ),
    );
    let (code, report) = validate(home.path());
    assert_eq!(code, Some(0), "every read validates:\n{report}");
}

/// Services calling every write the plugin declares with every parameter it takes, each
/// checked for its error, validate: an interval service updating the issues it read with every
/// field set — a team and a state, `at=.top`, files beside a description, a user by email and
/// as `"me"`, a parent — then every field that can hold nothing cleared with `nil`, and
/// placing one issue `after` another or `at=.bottom`; filing an issue under each with every
/// parameter, the fields it read passed straight in, then one bare and one placed `after` it,
/// and putting a label on it and taking one off; relating each to every other it read by
/// every member of `relation` and taking it off again, linking a url with a title and without and
/// unlinking it, subscribing and unsubscribing a user named and the one left out, archiving,
/// putting in the trash and taking back, commenting with a file, replying, editing the reply
/// with a file, resolving and unresolving the thread and deleting the reply; and a
/// `service(linear)`'s slot updating, labelling, subscribing to, relating and commenting on the
/// issue it claimed, and filing a follow-up in its team. afkd types every argument and result
/// against the manifest.
#[test]
fn every_write_validates() {
    let home = installed();
    write_config(
        home.path(),
        &format!(
            r###"import "afkd"
import "svc:interval"
import "@afkd/linear"

WS :: linear.Workspace{{ api_key: "{API_KEY}" }}

nightly :: service(interval) {{
  every    1h
  work_dir "/srv/acme/widgets"

  on_run proc(run: afkd.Run) {{
    match linear.issues(WS, team=[ "ENG" ], state=[ "Triage" ], limit=2) {{
      .ok(issues) => {{
        for issue in issues {{
          match linear.update_issue(
            WS,
            issue,
            title="修复 the retry storm 🚨 — \"backoff\" resets",
            description="## 背景\n\nIt resets on every 401.",
            files=[ "/srv/acme/widgets/screen shot — 修复.png", "/srv/acme/widgets/run.log" ],
            state="Todo",
            team="OPS",
            at=.top,
            priority=.high,
            estimate=0,
            due="2026-12-24",
            assignee="chen@acme.example",
            delegate="me",
            project="Trackers 🧭",
            milestone="C — writes",
            cycle=13,
            parent=issue,
          ) {{
            .ok(updated) => {{
              $ echo #{{updated.id}} #{{updated.team}} #{{updated.state}} #{{updated.estimate ?? 0}}
            }}
            .err(e) => fail("linear.update_issue: #{{e.message}}")
          }}
          match linear.update_issue(
            WS,
            issue,
            estimate=nil,
            due=nil,
            assignee=nil,
            delegate=nil,
            project=nil,
            milestone=nil,
            cycle=nil,
            parent=nil,
            at=.bottom,
          ) {{
            .ok(cleared) => {{
              if cleared.assignee != nil || cleared.parent != nil {{
                fail("nil cleared nothing")
              }}
            }}
            .err(e) => fail("linear.update_issue: #{{e.message}}")
          }}
          match linear.update_issue(WS, issue, after=issue, priority=.none) {{
            .ok(_) => {{ }}
            .err(e) => fail("linear.update_issue: #{{e.message}}")
          }}
          match linear.create_issue(
            WS,
            issue.team,
            "Cap the backoff at 30s — \"Retry-After\" 🚦",
            description="Linear's header is in seconds.\n\n- [ ] honour it",
            files=[ "/srv/acme/widgets/run.log" ],
            state="Todo",
            at=.top,
            priority=issue.priority,
            estimate=issue.estimate,
            due=issue.due,
            labels=[ "Bug 🐛", "Needs Info" ],
            assignee=issue.assignee,
            delegate="me",
            project=issue.project,
            milestone=issue.milestone,
            cycle=issue.cycle,
            parent=issue,
          ) {{
            .ok(made) => {{
              $ echo #{{made.id}} #{{made.state}} #{{made.parent ?? "-"}}
              match linear.create_issue(WS, made.team, "子任务: read it", parent=nil, assignee=nil) {{
                .ok(bare) => {{
                  $ echo #{{bare.id}} #{{bare.state}}
                }}
                .err(e) => fail("linear.create_issue: #{{e.message}}")
              }}
              match linear.create_issue(WS, "OPS", "Below it", after=made, due=nil) {{
                .ok(_) => {{ }}
                .err(e) => fail("linear.create_issue: #{{e.message}}")
              }}
              match linear.add_label(WS, made, "afkd/ready") {{
                .ok(labelled) => {{
                  for name in labelled.labels {{
                    $ echo #{{name}}
                  }}
                }}
                .err(e) => fail("linear.add_label: #{{e.message}}")
              }}
              match linear.remove_label(WS, made, "Needs Info") {{
                .ok(_) => {{ }}
                .err(e) => fail("linear.remove_label: #{{e.message}}")
              }}
            }}
            .err(e) => fail("linear.create_issue: #{{e.message}}")
          }}
          for other in issues {{
            if other.id == issue.id {{
              continue
            }}
            match linear.add_relation(WS, issue, .blocks, other) {{
              .ok(related) => {{
                $ echo #{{related.id}} #{{related.duplicate_of ?? "none"}}
              }}
              .err(e) => fail("linear.add_relation: #{{e.message}}")
            }}
            match linear.remove_relation(WS, issue, .blocks, other) {{
              .ok(_) => {{ }}
              .err(e) => fail("linear.remove_relation: #{{e.message}}")
            }}
            match linear.add_relation(WS, issue, .blocked_by, other) {{
              .ok(related) => {{
                $ echo #{{related.id}} #{{related.duplicate_of ?? "none"}}
              }}
              .err(e) => fail("linear.add_relation: #{{e.message}}")
            }}
            match linear.remove_relation(WS, issue, .blocked_by, other) {{
              .ok(_) => {{ }}
              .err(e) => fail("linear.remove_relation: #{{e.message}}")
            }}
            match linear.add_relation(WS, issue, .related, other) {{
              .ok(related) => {{
                $ echo #{{related.id}} #{{related.duplicate_of ?? "none"}}
              }}
              .err(e) => fail("linear.add_relation: #{{e.message}}")
            }}
            match linear.remove_relation(WS, issue, .related, other) {{
              .ok(_) => {{ }}
              .err(e) => fail("linear.remove_relation: #{{e.message}}")
            }}
            match linear.add_relation(WS, issue, .similar, other) {{
              .ok(related) => {{
                $ echo #{{related.id}} #{{related.duplicate_of ?? "none"}}
              }}
              .err(e) => fail("linear.add_relation: #{{e.message}}")
            }}
            match linear.remove_relation(WS, issue, .similar, other) {{
              .ok(_) => {{ }}
              .err(e) => fail("linear.remove_relation: #{{e.message}}")
            }}
            match linear.add_relation(WS, issue, .duplicate_of, other) {{
              .ok(related) => {{
                $ echo #{{related.id}} #{{related.duplicate_of ?? "none"}}
              }}
              .err(e) => fail("linear.add_relation: #{{e.message}}")
            }}
            match linear.remove_relation(WS, issue, .duplicate_of, other) {{
              .ok(_) => {{ }}
              .err(e) => fail("linear.remove_relation: #{{e.message}}")
            }}
            match linear.add_relation(WS, issue, .duplicates, other) {{
              .ok(related) => {{
                $ echo #{{related.id}} #{{related.duplicate_of ?? "none"}}
              }}
              .err(e) => fail("linear.add_relation: #{{e.message}}")
            }}
            match linear.remove_relation(WS, issue, .duplicates, other) {{
              .ok(_) => {{ }}
              .err(e) => fail("linear.remove_relation: #{{e.message}}")
            }}
          }}
          match linear.add_link(WS, issue, "https://github.com/acme/widgets/pull/42?tab=files#修复", title="PR #42 — \"backoff\" 🚦") {{
            .ok(linked) => {{
              for url in linked.links {{
                $ echo #{{url}}
              }}
            }}
            .err(e) => fail("linear.add_link: #{{e.message}}")
          }}
          match linear.add_link(WS, issue, "https://ci.acme.example/runs/9001/log.txt") {{
            .ok(_) => {{ }}
            .err(e) => fail("linear.add_link: #{{e.message}}")
          }}
          match linear.remove_link(WS, issue, "https://ci.acme.example/runs/9001/log.txt") {{
            .ok(_) => {{ }}
            .err(e) => fail("linear.remove_link: #{{e.message}}")
          }}
          match linear.add_subscriber(WS, issue, "chen@acme.example") {{
            .ok(subscribed) => {{
              for name in subscribed.subscribers {{
                $ echo #{{name}}
              }}
            }}
            .err(e) => fail("linear.add_subscriber: #{{e.message}}")
          }}
          match linear.remove_subscriber(WS, issue, "陳大文") {{
            .ok(_) => {{ }}
            .err(e) => fail("linear.remove_subscriber: #{{e.message}}")
          }}
          match linear.remove_subscriber(WS, issue) {{
            .ok(_) => {{ }}
            .err(e) => fail("linear.remove_subscriber: #{{e.message}}")
          }}
          match linear.archive_issue(WS, issue) {{
            .ok(archived) => {{
              if !archived.archived {{
                fail("archive_issue archived nothing")
              }}
            }}
            .err(e) => fail("linear.archive_issue: #{{e.message}}")
          }}
          match linear.delete_issue(WS, issue) {{
            .ok(trashed) => {{
              if !trashed.trashed {{
                fail("delete_issue trashed nothing")
              }}
            }}
            .err(e) => fail("linear.delete_issue: #{{e.message}}")
          }}
          match linear.unarchive_issue(WS, issue) {{
            .ok(back) => {{
              $ echo #{{back.id}} #{{back.archived}} #{{back.trashed}}
            }}
            .err(e) => fail("linear.unarchive_issue: #{{e.message}}")
          }}
          match linear.create_comment(WS, issue, "看起来不对 🚨 — it resets on every 401:\n\n```\nPOST /graphql 401\n```", files=[ "/srv/acme/widgets/run.log" ]) {{
            .ok(comment) => {{
              $ echo #{{comment.issue}} #{{comment.author ?? "-"}} #{{comment.reply_to ?? "none"}}
              match linear.create_comment(WS, issue, "Which header?", reply_to=comment) {{
                .ok(reply) => {{
                  match linear.update_comment(WS, reply, "Which header — \"Retry-After\"?", files=[ "/srv/acme/widgets/screen shot — 修复.png" ]) {{
                    .ok(edited) => {{
                      if edited.edited_at == nil {{
                        fail("update_comment edited nothing")
                      }}
                    }}
                    .err(e) => fail("linear.update_comment: #{{e.message}}")
                  }}
                  match linear.resolve_comment(WS, reply) {{
                    .ok(first) => {{
                      if first.resolved_at == nil {{
                        fail("resolve_comment resolved nothing")
                      }}
                    }}
                    .err(e) => fail("linear.resolve_comment: #{{e.message}}")
                  }}
                  match linear.unresolve_comment(WS, comment) {{
                    .ok(_) => {{ }}
                    .err(e) => fail("linear.unresolve_comment: #{{e.message}}")
                  }}
                  match linear.delete_comment(WS, reply) {{
                    .ok(_) => {{ }}
                    .err(e) => fail("linear.delete_comment: #{{e.message}}")
                  }}
                }}
                .err(e) => fail("linear.create_comment: #{{e.message}}")
              }}
            }}
            .err(e) => fail("linear.create_comment: #{{e.message}}")
          }}
        }}
      }}
      .err(e) => fail("linear.issues: #{{e.message}}")
    }}
  }}
}}

widgets :: service(linear) {{
  team       "{TEAM}"
  api_key    "{API_KEY}"
  claim_from "Todo"
  work_dir   "/srv/acme/widgets"

  on_run proc(run: afkd.Run, workspace: linear.Workspace, issue: linear.Issue) {{
    match linear.update_issue(workspace, issue, state="In Progress", at=.top, assignee="me") {{
      .ok(started) => {{
        $ echo #{{started.id}} #{{started.state}}
      }}
      .err(e) => fail("linear.update_issue: #{{e.message}}")
    }}
    match linear.add_subscriber(workspace, issue) {{
      .ok(_) => {{ }}
      .err(e) => fail("linear.add_subscriber: #{{e.message}}")
    }}
    match linear.add_label(workspace, issue, "In flight 🚧") {{
      .ok(_) => {{ }}
      .err(e) => fail("linear.add_label: #{{e.message}}")
    }}
    match linear.remove_label(workspace, issue, "afkd/ready") {{
      .ok(_) => {{ }}
      .err(e) => fail("linear.remove_label: #{{e.message}}")
    }}
    match linear.create_issue(workspace, issue.team, "Follow up on #{{issue.id}}", parent=issue, priority=issue.priority, labels=[ "Problem" ]) {{
      .ok(follow_up) => {{
        $ echo #{{follow_up.id}}
      }}
      .err(e) => fail("linear.create_issue: #{{e.message}}")
    }}
    match linear.issues(workspace, id=[ "ENG-1" ]) {{
      .ok(found) => {{
        for blocker in found {{
          match linear.add_relation(workspace, issue, .blocked_by, blocker) {{
            .ok(_) => {{ }}
            .err(e) => fail("linear.add_relation: #{{e.message}}")
          }}
        }}
      }}
      .err(e) => fail("linear.issues: #{{e.message}}")
    }}
    match linear.create_comment(workspace, issue, "Started on #{{issue.id}} 🚦") {{
      .ok(posted) => {{
        $ echo #{{posted.url}}
      }}
      .err(e) => fail("linear.create_comment: #{{e.message}}")
    }}
  }}
}}
"###
        ),
    );
    let (code, report) = validate(home.path());
    assert_eq!(code, Some(0), "every write validates:\n{report}");
}

/// The reads leg's service: an interval service, run by hand, whose run reads the viewer,
/// the teams, the `ENG` team's issues by when they were filed, each one's comments and
/// files, and downloads each file; then, once the test has moved the issue to another team,
/// reads it again by its uuid and writes `same` when the two handles are `==`. Every line
/// goes to [`READS`].
fn reads_service(home: &Path, base_url: &str) -> String {
    format!(
        r###"import "afkd"
import "svc:interval"
import "@afkd/linear"

WS :: linear.Workspace{{ api_key: "{API_KEY}", base_url: "{base_url}" }}

nightly :: service(interval) {{
  every    24h
  work_dir "{home}"

  on_run proc(run: afkd.Run) {{
    match linear.me(WS) {{
      .ok(me) => {{
        $ echo me #{{me.name}} #{{me.app}} >> {READS}
      }}
      .err(e) => fail("linear.me: #{{e.message}}")
    }}
    match linear.teams(WS) {{
      .ok(teams) => {{
        for team in teams {{
          $ echo team #{{team.id}} #{{team.name}} >> {READS}
        }}
      }}
      .err(e) => fail("linear.teams: #{{e.message}}")
    }}
    match linear.issues(WS, team=[ "{TEAM}" ], order=[ .created ]) {{
      .ok(issues) => {{
        for issue in issues {{
          $ echo issue #{{issue.id}} #{{issue.title}} #{{issue.state_type}} >> {READS}
          match linear.comments(WS, issue) {{
            .ok(comments) => {{
              for comment in comments {{
                $ echo comment #{{comment.author ?? "-"}} #{{comment.body}} >> {READS}
              }}
            }}
            .err(e) => fail("linear.comments: #{{e.message}}")
          }}
          match linear.files(WS, issue) {{
            .ok(urls) => {{
              for url in urls {{
                $ echo file #{{url}} >> {READS}
                match linear.download_file(WS, url, "{home}/downloaded.png") {{
                  .ok(_) => {{ }}
                  .err(e) => fail("linear.download_file: #{{e.message}}")
                }}
              }}
            }}
            .err(e) => fail("linear.files: #{{e.message}}")
          }}
          $ echo read > first.txt
          $ i=0; until [ -f moved ] || [ $i -ge 600 ]; do sleep 0.1; i=$((i+1)); done
          match linear.issues(WS, id=[ issue.uuid ]) {{
            .ok(again) => {{
              for moved in again {{
                $ echo again #{{moved.id}} #{{moved.team}} >> {READS}
                if moved == issue {{
                  $ echo same >> {READS}
                }}
              }}
            }}
            .err(e) => fail("linear.issues: #{{e.message}}")
          }}
        }}
      }}
      .err(e) => fail("linear.issues: #{{e.message}}")
    }}
  }}
}}
"###,
        home = home.display()
    )
}

/// A service of another kind reads a workspace through a child afkd starts for each call:
/// the viewer, the teams, a team's issues, an issue's comments and files and the file
/// itself, byte for byte; and the issue read again after it moved team carries a new
/// identifier and the same key, so the two handles are `==`.
#[test]
fn reads_leg_an_interval_service_reads_through_a_child_per_call() {
    let home = installed();
    let home = home.path();
    let fake = FakeLinear::start();
    fake.team("OPS", "Operations — 運用");
    fake.state("Todo", "unstarted");
    fake.team_state("OPS", "Todo", "unstarted");
    let chen = fake.user("陳大文", "chen", "chen@acme.example");
    let shot = fake.seed_file("screen shot — 修复.png", "image/png", &png());
    let issue = fake.issue("Todo", TITLE, &format!("Before:\n\n![screen shot]({shot})"));
    fake.comment(&issue, Author::User(chen), "看起来不对 🚨 — it resets", 60);
    write_config(home, &reads_service(home, fake.base_url()));
    let (code, report) = validate(home);
    assert_eq!(code, Some(0), "the service validates:\n{report}");

    let daemon = spawn_headless_streaming(home, &[]);
    let deadline = Instant::now() + BUDGET;
    loop {
        let out = run_subcommand_args(home, &["run", "nightly"]);
        if out.status.success() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "`afkd run nightly` within {BUDGET:?}: {}\n{}",
            String::from_utf8_lossy(&out.stderr),
            dump(home, &fake, &issue, &daemon)
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    wait_for(
        home,
        &fake,
        &issue,
        &daemon,
        "the run read the issue",
        || home.join("first.txt").exists(),
    );
    let before = fake.identifier(&issue);
    fake.move_team(&issue, "OPS");
    let after = fake.identifier(&issue);
    std::fs::write(home.join("moved"), "").expect("say the issue moved");
    wait_for(
        home,
        &fake,
        &issue,
        &daemon,
        "the run read it again",
        || std::fs::read_to_string(home.join(READS)).is_ok_and(|r| r.ends_with("same\n")),
    );
    assert_eq!(
        read(&home.join(READS)),
        format!(
            "me afkd false\nteam ENG Engineering\nteam OPS Operations — 運用\n\
             issue {before} {TITLE} unstarted\ncomment chen 看起来不对 🚨 — it resets\n\
             file {shot}\nagain {after} OPS\nsame\n"
        )
    );
    assert_ne!(before, after, "the identifier changed");
    assert_eq!(
        std::fs::read(home.join("downloaded.png")).expect("the downloaded file"),
        png(),
        "the file byte for byte"
    );
    let seen = fake.seen();
    assert!(
        seen.iter().any(|s| s.operation == "IssueList"),
        "the issues were read"
    );
    assert!(
        seen.iter()
            .filter(|s| s.path == "/graphql")
            .all(|s| s.authorization.as_deref() == Some(API_KEY)),
        "every read went out with the workspace's key"
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
}

/// The file, under the writes leg's work dir, its run writes what each write gave back to.
const WRITES: &str = "writes.txt";

/// The writes leg's service: an interval service, run by hand, whose run reads the issues
/// `blocker` and `issue` by identifier and hands both to a proc that moves the issue to the
/// top of Todo, relates it as blocked by the blocker, comments [`NOTE`] on it with the
/// screenshot at [`SHOT`] under `home`, resolves that comment, replies to it and deletes the
/// reply. Each write's result goes to [`WRITES`] under `home`, a line each.
fn writes_service(home: &Path, base_url: &str, issue: &str, blocker: &str) -> String {
    format!(
        r###"import "afkd"
import "svc:interval"
import "@afkd/linear"

WS :: linear.Workspace{{ api_key: "{API_KEY}", base_url: "{base_url}" }}

nightly :: service(interval) {{
  every    24h
  work_dir "{home}"

  on_run proc(run: afkd.Run) {{
    match linear.issues(WS, id=[ "{blocker}" ]) {{
      .ok(blockers) => {{
        for blocker in blockers {{
          match linear.issues(WS, id=[ "{issue}" ]) {{
            .ok(issues) => {{
              for issue in issues {{
                sweep(issue, blocker)
              }}
            }}
            .err(e) => fail("linear.issues: #{{e.message}}")
          }}
        }}
      }}
      .err(e) => fail("linear.issues: #{{e.message}}")
    }}
  }}
}}

sweep :: proc(issue: linear.Issue, blocker: linear.Issue) {{
  match linear.update_issue(WS, issue, state="Todo", at=.top) {{
    .ok(moved) => {{
      $ echo moved #{{moved.id}} #{{moved.state}} >> "{home}/{WRITES}"
    }}
    .err(e) => fail("linear.update_issue: #{{e.message}}")
  }}
  match linear.add_relation(WS, issue, .blocked_by, blocker) {{
    .ok(related) => {{
      for id in related.blocked_by {{
        $ echo blocked_by #{{id}} >> "{home}/{WRITES}"
      }}
    }}
    .err(e) => fail("linear.add_relation: #{{e.message}}")
  }}
  match linear.create_comment(WS, issue, "{note}", files=[ "{home}/{SHOT}" ]) {{
    .ok(comment) => {{
      $ echo commented #{{comment.issue}} >> "{home}/{WRITES}"
      match linear.resolve_comment(WS, comment) {{
        .ok(resolved) => {{
          if resolved.resolved_at != nil {{
            $ echo resolved >> "{home}/{WRITES}"
          }}
        }}
        .err(e) => fail("linear.resolve_comment: #{{e.message}}")
      }}
      match linear.create_comment(WS, issue, "Resolved by the nightly sweep.", reply_to=comment) {{
        .ok(reply) => {{
          match linear.delete_comment(WS, reply) {{
            .ok(_) => {{
              $ echo deleted >> "{home}/{WRITES}"
            }}
            .err(e) => fail("linear.delete_comment: #{{e.message}}")
          }}
        }}
        .err(e) => fail("linear.create_comment: #{{e.message}}")
      }}
    }}
    .err(e) => fail("linear.create_comment: #{{e.message}}")
  }}
}}
"###,
        home = home.display(),
        note = literal(NOTE),
    )
}

/// A service of another kind writes a workspace through a child afkd starts for each call:
/// the issue moved to the top of another state, blocked by another issue, commented on with
/// a screenshot shown after the text, the comment resolved, and a reply to it posted and
/// deleted; the fake holds each change afterwards, and every write went out with the
/// workspace's key.
#[test]
fn writes_leg_an_interval_service_writes_through_a_child_per_call() {
    let home = installed();
    let home = home.path();
    let fake = FakeLinear::start();
    fake.state("Todo", "unstarted");
    fake.state("In Progress", "started");
    let top = fake.issue("Todo", "Read Retry-After", "");
    let issue = fake.issue("In Progress", TITLE, BODY);
    let blocker = fake.issue("Todo", "Upgrade the client", "");
    let (id, blocker_id) = (fake.identifier(&issue), fake.identifier(&blocker));
    std::fs::write(home.join(SHOT), png()).expect("write the screenshot");
    write_config(
        home,
        &writes_service(home, fake.base_url(), &id, &blocker_id),
    );
    let (code, report) = validate(home);
    assert_eq!(code, Some(0), "the service validates:\n{report}");

    let daemon = spawn_headless_streaming(home, &[]);
    let deadline = Instant::now() + BUDGET;
    loop {
        let out = run_subcommand_args(home, &["run", "nightly"]);
        if out.status.success() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "`afkd run nightly` within {BUDGET:?}: {}\n{}",
            String::from_utf8_lossy(&out.stderr),
            dump(home, &fake, &issue, &daemon)
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    wait_for(
        home,
        &fake,
        &issue,
        &daemon,
        "the run wrote every change",
        || std::fs::read_to_string(home.join(WRITES)).is_ok_and(|w| w.ends_with("deleted\n")),
    );
    assert_eq!(
        read(&home.join(WRITES)),
        format!("moved {id} Todo\nblocked_by {blocker_id}\ncommented {id}\nresolved\ndeleted\n")
    );
    assert_eq!(fake.state_of(&issue), "Todo");
    assert_eq!(
        fake.in_state("Todo"),
        [issue.clone(), top, blocker],
        "at the top of Todo"
    );
    assert_eq!(fake.relation_count(), 1);
    let uploads = fake.uploads();
    assert_eq!(uploads.len(), 1);
    assert!(uploads[0].landed, "the screenshot went up");
    assert_eq!(
        fake.file(&uploads[0].asset_url),
        Some(png()),
        "byte for byte"
    );
    let comments = fake.comments(&issue);
    assert_eq!(comments.len(), 1, "the reply is gone: {comments:?}");
    assert_eq!(
        comments[0].body,
        format!("{NOTE}\n\n![{SHOT}]({})", uploads[0].asset_url)
    );
    assert!(comments[0].resolved.is_some(), "the thread is resolved");
    let seen = fake.seen();
    let sent: Vec<&str> = seen
        .iter()
        .map(|s| s.operation.as_str())
        .filter(|op| {
            [
                "IssueWrite",
                "RelationCreate",
                "CommentPost",
                "CommentResolve",
                "CommentDelete",
            ]
            .contains(op)
        })
        .collect();
    assert_eq!(
        sent,
        [
            "IssueWrite",
            "RelationCreate",
            "CommentPost",
            "CommentResolve",
            "CommentPost",
            "CommentDelete"
        ]
    );
    assert!(
        seen.iter()
            .filter(|s| s.path == "/graphql")
            .all(|s| s.authorization.as_deref() == Some(API_KEY)),
        "every write went out with the workspace's key"
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
}

/// The file, under the two workspaces leg's work dir, its run writes what it did to.
const DAILY: &str = "daily.txt";

/// The two workspaces leg's service: a daily service, run by hand, holding `ACME` and
/// `GLOBEX`, a `linear.Workspace` each, reaching the fakes at `acme` and `globex`. Its run
/// lists each one's issues, then files an issue in acme and updates its title and priority,
/// moves it to the top of In Progress, puts a label on it, relates it as blocked by acme's
/// issue and comments on it, each through `ACME`; last it passes the filed issue to
/// `update_issue` with `GLOBEX` and writes the refusal. Each step's result goes to [`DAILY`]
/// under `home`, a line each.
fn daily_service(home: &Path, acme: &str, globex: &str) -> String {
    format!(
        r###"import "afkd"
import "svc:daily"
import "@afkd/linear"

ACME   :: linear.Workspace{{ api_key: "{API_KEY}", base_url: "{acme}" }}
GLOBEX :: linear.Workspace{{ api_key: "{GLOBEX_API_KEY}", base_url: "{globex}" }}

sweep :: service(daily) {{
  at       "04:00"
  work_dir "{home}"

  on_run proc(run: afkd.Run) {{
    match linear.issues(GLOBEX) {{
      .ok(theirs) => {{
        for issue in theirs {{
          $ echo globex #{{issue.id}} #{{issue.workspace}} >> {DAILY}
        }}
      }}
      .err(e) => fail("linear.issues: #{{e.message}}")
    }}
    match linear.issues(ACME, team=[ "{TEAM}" ], state=[ "Todo" ]) {{
      .ok(ours) => {{
        for blocker in ours {{
          $ echo acme #{{blocker.id}} #{{blocker.workspace}} >> {DAILY}
          file_and_work(blocker)
        }}
      }}
      .err(e) => fail("linear.issues: #{{e.message}}")
    }}
  }}
}}

file_and_work :: proc(blocker: linear.Issue) {{
  made := match linear.create_issue(ACME, blocker.team, "{title}", description="{body}", priority=.high, labels=[ "Bug 🐛" ]) {{
    .ok(made) => made
    .err(e) => fail("linear.create_issue: #{{e.message}}")
  }}
  $ echo created #{{made.id}} #{{made.state}} #{{made.priority}} >> {DAILY}
  match linear.update_issue(ACME, made, title="Cap the backoff at 30s 🚦", priority=.urgent) {{
    .ok(updated) => {{
      $ echo updated #{{updated.title}} #{{updated.priority}} >> {DAILY}
    }}
    .err(e) => fail("linear.update_issue: #{{e.message}}")
  }}
  match linear.update_issue(ACME, made, state="In Progress", at=.top) {{
    .ok(moved) => {{
      $ echo moved #{{moved.state}} >> {DAILY}
    }}
    .err(e) => fail("linear.update_issue: #{{e.message}}")
  }}
  match linear.add_label(ACME, made, "Needs Info") {{
    .ok(labelled) => {{
      for name in labelled.labels {{
        $ echo label #{{name}} >> {DAILY}
      }}
    }}
    .err(e) => fail("linear.add_label: #{{e.message}}")
  }}
  match linear.add_relation(ACME, made, .blocked_by, blocker) {{
    .ok(related) => {{
      for id in related.blocked_by {{
        $ echo blocked_by #{{id}} >> {DAILY}
      }}
    }}
    .err(e) => fail("linear.add_relation: #{{e.message}}")
  }}
  match linear.create_comment(ACME, made, "{note}") {{
    .ok(comment) => {{
      $ echo commented #{{comment.issue}} >> {DAILY}
    }}
    .err(e) => fail("linear.create_comment: #{{e.message}}")
  }}
  match linear.update_issue(GLOBEX, made, title="Never written") {{
    .ok(_) => fail("globex took an issue of acme")
    .err(e) => {{
      $ echo refused #{{e.message}} >> {DAILY}
    }}
  }}
}}
"###,
        home = home.display(),
        title = literal(TITLE),
        body = literal(BODY),
        note = literal(NOTE),
    )
}

/// A daily service holding two `linear.Workspace` values works two workspaces from one run,
/// through a child afkd starts for each call: it lists each one's issues, files an issue in
/// acme with a label, updates its title and priority, moves it to the top of In Progress,
/// labels it, relates it as blocked by acme's issue and comments on it, each change landing
/// in acme with acme's key; and the filed issue passed with globex's workspace is refused
/// naming both, with nothing written to globex.
#[test]
fn two_workspaces_leg_a_daily_service_works_each_and_keeps_them_apart() {
    let home = installed();
    let home = home.path();
    let acme = FakeLinear::start();
    acme.state("Todo", "unstarted");
    acme.state("In Progress", "started");
    acme.label_of(TEAM, "Bug 🐛", None);
    acme.workspace_label("Needs Info", "#f2c94c");
    let ahead = acme.issue("In Progress", "Upgrade ureq", "");
    let blocker = acme.issue("Todo", "Upgrade the client", "");
    let globex = FakeLinear::start_globex();
    globex.state("Todo", "unstarted");
    let theirs = globex.issue("Todo", "Globex's own 🌐", "");
    write_config(
        home,
        &daily_service(home, acme.base_url(), globex.base_url()),
    );
    let (code, report) = validate(home);
    assert_eq!(code, Some(0), "the service validates:\n{report}");

    let daemon = spawn_headless_streaming(home, &[]);
    let deadline = Instant::now() + BUDGET;
    loop {
        let out = run_subcommand_args(home, &["run", "sweep"]);
        if out.status.success() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "`afkd run sweep` within {BUDGET:?}: {}\n{}",
            String::from_utf8_lossy(&out.stderr),
            dump(home, &acme, &blocker, &daemon)
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    wait_for(
        home,
        &acme,
        &blocker,
        &daemon,
        "the run worked both workspaces",
        || std::fs::read_to_string(home.join(DAILY)).is_ok_and(|d| d.contains("refused ")),
    );
    let made = acme
        .find_by_title("Cap the backoff at 30s 🚦")
        .expect("the filed issue, retitled");
    let (made_id, blocker_id) = (acme.identifier(&made), acme.identifier(&blocker));
    assert_eq!(
        read(&home.join(DAILY)),
        format!(
            "globex {theirs_id} globex\nacme {blocker_id} acme\n\
             created {made_id} Todo high\nupdated Cap the backoff at 30s 🚦 urgent\n\
             moved In Progress\nlabel Bug 🐛\nlabel Needs Info\nblocked_by {blocker_id}\n\
             commented {made_id}\n\
             refused update_issue: issue is {made_id} of workspace acme, and the \
             linear.Workspace reaches globex\n",
            theirs_id = globex.identifier(&theirs),
        )
    );
    assert_eq!(acme.description_of(&made), BODY);
    assert_eq!(acme.state_of(&made), "In Progress");
    assert_eq!(
        acme.in_state("In Progress"),
        [made.clone(), ahead],
        "at the top of In Progress"
    );
    assert_eq!(acme.labels(&made), ["Bug 🐛", "Needs Info"]);
    assert_eq!(acme.relation_count(), 1);
    let comments = acme.comments(&made);
    assert_eq!(comments.len(), 1);
    assert_eq!(comments[0].body, NOTE);
    assert_eq!(acme.title_of(&made), "Cap the backoff at 30s 🚦");

    let written = |fake: &FakeLinear| -> Vec<String> {
        fake.seen()
            .into_iter()
            .map(|s| s.operation)
            .filter(|op| {
                [
                    "IssueCreate",
                    "IssueWrite",
                    "IssueAddLabel",
                    "RelationCreate",
                    "CommentPost",
                ]
                .contains(&op.as_str())
            })
            .collect()
    };
    assert_eq!(
        written(&acme),
        [
            "IssueCreate",
            "IssueWrite",
            "IssueWrite",
            "IssueAddLabel",
            "RelationCreate",
            "CommentPost"
        ]
    );
    assert_eq!(
        written(&globex),
        Vec::<String>::new(),
        "nothing written to globex"
    );
    for (fake, key) in [(&acme, API_KEY), (&globex, GLOBEX_API_KEY)] {
        assert!(
            fake.seen()
                .iter()
                .filter(|s| s.path == "/graphql")
                .all(|s| s.authorization.as_deref() == Some(key)),
            "each workspace saw only its own key"
        );
    }

    daemon.signal(libc::SIGINT);
    let out = daemon.reap(Duration::from_secs(10));
    assert!(
        out.status.success(),
        "the daemon drains clean ({:?}):\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    for fake in [&acme, &globex] {
        assert_eq!(
            fake.invalid_requests(),
            Vec::<String>::new(),
            "requests Linear's schema refuses"
        );
    }
}

/// The screenshot the files and the writes legs' runs put up: spaces, an em dash and wide
/// CJK.
const SHOT: &str = "screen shot — 修复.png";

/// The marker the files leg's run writes once it has fetched its files.
const FETCHED: &str = "fetched.txt";

/// The files leg's service: its run puts [`SHOT`] into the issue's description and into a
/// comment, downloads each file the issue links into `fetched/`, numbered in the order
/// `files` gives them, moves it to Done and writes [`FETCHED`], each call checked for its
/// error.
fn files_service(home: &Path, base_url: &str) -> String {
    format!(
        r###"import "afkd"
import "@afkd/linear"

widgets :: service(linear) {{
  team       "{TEAM}"
  base_url   "{base_url}"
  api_key    "{API_KEY}"
  claim_from "Todo"

  work_dir "{home}"
  on_run proc(run: afkd.Run, workspace: linear.Workspace, issue: linear.Issue) {{
    match linear.update_issue(workspace, issue, description="## 背景\n\nBefore:", files=[ "{home}/{SHOT}" ]) {{
      .ok(_) => {{ }}
      .err(e) => fail("linear.update_issue: #{{e.message}}")
    }}
    match linear.create_comment(workspace, issue, "After the fix ✅", files=[ "{home}/{SHOT}" ]) {{
      .ok(_) => {{ }}
      .err(e) => fail("linear.create_comment: #{{e.message}}")
    }}
    $ mkdir -p fetched
    match linear.files(workspace, issue) {{
      .ok(urls) => {{
        for i, url in urls {{
          match linear.download_file(workspace, url, "{home}/fetched/#{{i + 1}}-{SHOT}") {{
            .ok(_) => {{ }}
            .err(e) => fail("linear.download_file: #{{e.message}}")
          }}
        }}
      }}
      .err(e) => fail("linear.files: #{{e.message}}")
    }}
    match linear.update_issue(workspace, issue, state="Done") {{
      .ok(_) => {{ }}
      .err(e) => fail("linear.update_issue: #{{e.message}}")
    }}
    $ echo fetched > {FETCHED}
  }}
}}
"###,
        home = home.display()
    )
}

/// A run puts a PNG on its issue through a real daemon — into the description and into a
/// comment, each embedding it at its asset url — and downloads both back byte for byte, the
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

/// `team` and `claim_from` are required: a service without either is refused at validate,
/// naming the setting and the service.
#[test]
fn a_service_without_team_or_claim_from_fails_validate_naming_the_setting() {
    let home = installed();
    for (missing, keep) in [
        ("team", "claim_from \"Todo\""),
        ("claim_from", "team \"ENG\""),
    ] {
        write_config(
            home.path(),
            &format!(
                r#"import "afkd"
import "@afkd/linear"

widgets :: service(linear) {{
  {keep}
  api_key "{API_KEY}"
  work_dir "/srv/acme/widgets"
  on_run proc(run: afkd.Run, workspace: linear.Workspace, issue: linear.Issue) {{
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
            r#"import "afkd"
import "@afkd/linear"

widgets :: service(linear) {{
  team       "ENG"
  api_key    "{API_KEY}"
  claim_from "Todo"

  on_done proc(run: afkd.Run, workspace: linear.Workspace, issue: linear.Issue, outcome: afkd.Outcome) {{
    {call}
  }}

  work_dir "/srv/acme/widgets"
  on_run proc(run: afkd.Run, workspace: linear.Workspace, issue: linear.Issue) {{
    $ true
  }}
}}
"#
        )
    };
    write_config(
        home.path(),
        &config(r#"linear.assign(workspace, issue, "me")"#),
    );
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
        report.contains(
            "its actions are add_label, add_link, add_relation, add_subscriber, archive_issue, \
             comments, create_comment, create_issue, cycles,"
        ),
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

/// The README's `conf` fences, each with the line its opener sits on. An indented fence
/// (one inside a list item) has that indent stripped from its body.
fn conf_fences(readme: &str) -> Vec<(usize, String)> {
    let mut fences = Vec::new();
    let mut open: Option<(usize, usize, Vec<&str>)> = None;
    for (at, line) in readme.lines().enumerate() {
        match open.as_mut() {
            None if line.trim() == "```conf" => {
                let indent = line.len() - line.trim_start().len();
                open = Some((at + 1, indent, Vec::new()));
            }
            None => {}
            Some(_) if line.trim() == "```" => {
                let (opened, _, body) = open.take().expect("open");
                fences.push((opened, body.join("\n") + "\n"));
            }
            Some((_, indent, body)) => body.push(line.get(*indent..).unwrap_or("").trim_end()),
        }
    }
    assert!(open.is_none(), "every conf fence in the README closes");
    fences
}

/// The name of each variable `fence` reads from the environment, each `env.get("NAME")`, in
/// the order it reads them.
fn env_reads(fence: &str) -> Vec<&str> {
    const READ: &str = "env.get(\"";
    fence
        .match_indices(READ)
        .filter_map(|(at, _)| {
            fence[at + READ.len()..]
                .split_once('"')
                .map(|(name, _)| name)
        })
        .collect()
}

#[test]
fn every_readme_conf_fence_validates() {
    let readme = std::fs::read_to_string(plugin_root().join("README.md")).expect("the README");
    let fences = conf_fences(&readme);
    let openers = readme.lines().filter(|l| l.trim() == "```conf").count();
    assert!(openers > 0, "the README carries conf fences");
    assert_eq!(fences.len(), openers, "every conf opener yields a fence");

    // Check that every name a fence reads is set before any fence is validated, so a name the
    // README starts reading fails as itself, not as a load failure.
    let reads: Vec<(usize, &str)> = fences
        .iter()
        .flat_map(|(line, fence)| env_reads(fence).into_iter().map(move |name| (*line, name)))
        .collect();
    assert!(
        !reads.is_empty(),
        "the README's fences read their credentials from the environment"
    );
    let unset: Vec<String> = reads
        .iter()
        .filter(|(_, name)| CONFIG_ENV.iter().all(|(set, _)| set != name))
        .map(|(line, name)| format!("the fence at README line {line} reads {name}"))
        .collect();
    assert!(
        unset.is_empty(),
        "CONFIG_ENV sets every name the README reads, but not:\n{}",
        unset.join("\n")
    );

    let home = installed();
    for (n, (line, fence)) in fences.iter().enumerate() {
        assert!(
            !fence.trim().is_empty(),
            "fence {} (README line {line}) is empty",
            n + 1
        );
        write_config(home.path(), fence);
        let (code, report) = validate(home.path());
        assert_eq!(
            code,
            Some(0),
            "fence {} (README line {line}) validates:\n{fence}\n{report}",
            n + 1
        );
    }
}

/// What the skill leg's run posts through `create_comment.py`, and then asks through `ask.py`,
/// and the comment the writes leg's run posts: wide CJK, an emoji, quotes, a fence and several
/// lines.
const NOTE: &str = "计划 ✅ — \"backoff\" capped at 30s:\n\n```\nlet backoff = 30;\n```";
const QUESTION: &str = "看起来不对 🚨 — should a 401 reset the backoff?\n\nIt does today.";

/// The skill leg's service: its run calls the installed skill's scripts by path, with the
/// env the run carries and nothing else, the way an agent's Bash tool does.
fn skill_service(home: &Path, base_url: &str, skill: &Path) -> String {
    format!(
        r#"import "afkd"
import "@afkd/linear"

NOTE     :: "{note}"
QUESTION :: "{question}"

widgets :: service(linear) {{
  team       "{TEAM}"
  base_url   "{base_url}"
  api_key    "{API_KEY}"
  claim_from "Todo"

  work_dir "{home}"
  on_run proc(run: afkd.Run, workspace: linear.Workspace, issue: linear.Issue) {{
    $ python3 {skill}/create_comment.py "$LINEAR_ISSUE" #{{NOTE}}
    $ python3 {skill}/ask.py #{{QUESTION}}
  }}
}}
"#,
        home = home.display(),
        skill = skill.display(),
        note = literal(NOTE),
        question = literal(QUESTION),
    )
}

/// `text` as the body of a config string literal: its `\`, `"` and newlines escaped.
fn literal(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// A run works its issue through the installed skill on a real daemon: its comment and its
/// question land on the issue verbatim, and the question parks it as the plugin parks one —
/// the `Needs Info` label on, an `[afkd-park]` marker naming the service, the claim
/// released. `afkd plugins` lists the skill, and every request conforms.
#[test]
fn skill_leg_a_run_comments_and_parks_through_the_installed_skill() {
    let home = installed();
    let home = home.path();
    let skill = plugins_root(home).join(NAME).join("skills/linear");
    let out = run_subcommand_args(home, &["plugins"]);
    let listed = String::from_utf8_lossy(&out.stdout).into_owned();
    let line = listed
        .lines()
        .find(|l| l.starts_with(NAME))
        .unwrap_or_else(|| panic!("afkd plugins lists {NAME}:\n{listed}"));
    assert!(
        line.contains("skill linear"),
        "the skill is supplied:\n{listed}"
    );

    let seeded = seed();
    let (fake, issue) = (&seeded.fake, seeded.issue.as_str());
    write_config(home, &skill_service(home, fake.base_url(), &skill));
    let (code, report) = validate(home);
    assert_eq!(code, Some(0), "the service validates:\n{report}");

    let daemon = spawn_headless_streaming(home, &[]);
    wait_for(home, fake, issue, &daemon, "the issue is parked", || {
        let thread = fake.comments(issue);
        fake.labels(issue).iter().any(|l| l == "Needs Info")
            && thread.iter().any(|c| c.body.starts_with("[afkd-park]"))
            && !claimed(fake, issue)
    });
    let thread = fake.comments(issue);
    let said: Vec<&str> = thread
        .iter()
        .filter(|c| !c.body.starts_with("[afkd-"))
        .map(|c| c.body.as_str())
        .collect();
    assert_eq!(said, [ASK, NOTE, QUESTION], "the thread, oldest first");
    let parked = thread
        .iter()
        .find(|c| c.body.starts_with("[afkd-park]"))
        .expect("the park marker");
    assert!(
        parked.body.contains("widgets"),
        "{:?} names the service",
        parked.body
    );
    assert_eq!(fake.state_of(issue), "Todo", "a park moves nothing");
    assert_eq!(
        run_dirs(home).len(),
        1,
        "one run, parked, not claimed again"
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
}
