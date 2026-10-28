//! The gate that holds the **shipped `@afkd/github` plugin** — a manifest v2 provider whose
//! kinds are `service(github)` and `service(github.pr)`, shipped as a finished binary —
//! to a real afkd: the `afkd` first on `PATH`, the installed one and never a build, since a
//! plugin is checked against the afkd it will meet ([`bin_path`]).
//!
//! - The **install** leg installs the tree the release tarball holds and runs one issue
//!   through it on a real daemon, its slots' actions crossing as `call`s on the issue each
//!   is passed and its slots reading every field the issue's handle declares — then one
//!   pull request the same way, through a `service(github.pr)`.
//! - The **outage** leg fails an issue through both its attempts while GitHub is out of
//!   reach, its `on_fail` checking every action's result, and holds the issue to its failed
//!   state once GitHub is back: the slot's labels and comment on it, the bot unassigned, its
//!   claim released.
//! - The **both kinds** leg `afkd validate`s one file with a service of each kind, whose
//!   slots call every action the plugin provides on the item each is passed and name the
//!   bot by the config's own constant `BOT`, and refuses the same file spelling the label
//!   actions noun-first, as they were before they were named Odin-style. The **handle**
//!   leg holds `afkd validate` to refusing an action passed the other kind's item.
//! - The **cadence** leg holds `afkd validate` to refusing a service of either kind that
//!   writes the claim key afkd renamed to `every`, pointing it at `every`, and to taking
//!   the same services with `every`.
//! - The **README** leg `afkd validate`s every `conf` fence the plugin's README carries,
//!   each a whole entry file, against the installed plugin.
//!
//! The GitHub is the plugin's own loopback fake ([`fake`]), reached through a loopback `host`
//! the client resolves to the GHES `…/api/v3` base, so no leg touches a network; and every
//! wait is bounded and every spawn held in a [`Daemon`]: a claim that never lands must
//! **fail**, not hang.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use common::fake::{FakeGithub, TOKEN};
use common::*;
use tempfile::TempDir;

// --- the shipped tree --------------------------------------------------------------

/// The plugin's name, as the manifest spells it and afkd places it.
const NAME: &str = "@afkd/github";

/// The plugin's root — the directory `afkd install` takes, whose path this crate's own
/// mirrors under `drift/`. Canonicalized, so a staged copy and a report both name one path.
fn plugin_root() -> PathBuf {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../@afkd/github");
    std::fs::canonicalize(root).expect("the shipped plugin resolves")
}

/// The plugin's binary, built once per test binary the way the release builds it —
/// `cargo build --release --locked` in the plugin's own directory, for this host rather than
/// musl — since afkd never builds a plugin and a release ships it finished. An inherited
/// target-directory override would send the binary away from where this reads it.
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
        plugin_root().join("target/release/afkd-github")
    })
}

/// Stage the release `tools/pack-release` packs under `dst` — the manifest, the binary and
/// the skill, exactly what the release archive holds — and return the staged root.
fn stage(dst: &Path) -> PathBuf {
    let status = Command::new(plugin_root().join("../../tools/pack-release"))
        .arg("github")
        .arg(dst)
        .arg(built_binary())
        .status()
        .expect("run tools/pack-release");
    assert!(status.success(), "tools/pack-release github");
    let staged = dst.join("afkd-github");
    for present in ["afkd-plugin.toml", "afkd-github", "skills/github/SKILL.md"] {
        assert!(staged.join(present).exists(), "the release holds {present}");
    }
    for absent in [
        "Cargo.toml",
        "Cargo.lock",
        "src",
        "tests",
        "README.md",
        "target",
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
            let rel = path.strip_prefix(root).expect("under root");
            out.push(rel.to_string_lossy().into_owned());
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

// --- the scenario ------------------------------------------------------------------

/// The token's own login, and so the author of every comment the plugin posts. Wide,
/// accented and bracketed, as the wire suite's is.
const ME: &str = "björn-öst[bot]";

/// The repository, as the wire suite names it: the fake keys its state by this string.
const REPO: &str = "acme/widgets";

/// The issue the service claims, titled and described in the shapes that break a naive
/// transport: wide glyphs, an emoji, and a multi-line body with quoting and a fence.
const ISSUE: u64 = 7;
const TITLE: &str = "修复 the retry storm 🚨";
const BODY: &str = "Retries pile up after a 502.\n\n> \"backoff\" — nobody\n\n```\nretry: 0\n```\n";
/// The issue's labels: the source label, and a wide, scoped one beside it.
const LABELS: &[&str] = &["afkd/ready", "優先/high"];

/// The file, under a service's work dir, its `on_run` writes the item's handle fields to.
const FIELDS: &str = "fields.txt";

/// How long a claim, a run or a finish may take. Generous, because it bounds a failure
/// rather than timing a success: the service polls every second.
const BUDGET: Duration = Duration::from_secs(30);

/// The service that drives [`ISSUE`], `home` its work dir: a v2 file importing the plugin,
/// whose slots call its actions on the issue they are passed and whose `on_done` comment
/// afkd interpolates — the outcome's duration, the config's constant `BOT` and the issue's
/// title. The run holds until the test creates `release` — bounded, so a test that never
/// does cannot wedge it — which is what lets the test see the claim and the run mid-flight;
/// then it writes every field of the issue's handle to [`FIELDS`], a line each.
fn service(home: &Path, host: &str) -> String {
    format!(
        r#"import "core"
import "@afkd/github"

BOT :: "{ME}"

widgets :: service(github) {{
  host         "{host}"
  repo         "{REPO}"
  token        "{TOKEN}"
  source_label "afkd/ready"
  every        1s
  max_attempts 1

  on_claim(issue: github.Issue) {{ github.assign_me(issue) }}
  on_done(run: core.Run, issue: github.Issue, outcome: core.Outcome) {{
    github.remove_label(issue, "afkd/claimed")
    github.comment(issue, "done in #{{outcome.duration}} by #{{BOT}}: #{{issue.title}}")
    github.close(issue)
  }}

  work_dir "{home}"
  on_run(run: core.Run, issue: github.Issue) {{
    $ i=0; until [ -f release ] || [ $i -ge 600 ]; do sleep 0.1; i=$((i+1)); done
    $ echo title #{{issue.title}} >> {FIELDS}
    $ echo url #{{issue.url}} >> {FIELDS}
    $ echo number #{{issue.number}} >> {FIELDS}
    for label in issue.labels {{
      $ echo label #{{label}} >> {FIELDS}
    }}
    $ cat $AFKD_SCRATCH_DIR/task.md
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

/// The credential a config reads from the daemon's environment
/// (`env.get("GITHUB_TOKEN")`), as the README's lead example does.
const CONFIG_ENV: &[(&str, &str)] = &[("GITHUB_TOKEN", TOKEN)];

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

/// `service`'s run dirs, by name.
fn run_dirs(home: &Path, service: &str) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(runs_root(home).join(service))
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Everything a failed wait needs to say: the issue as the fake holds it, its thread, the
/// run dirs, the daemon's own log, and its stderr so far — where the plugin's own
/// complaints land.
fn dump(home: &Path, fake: &FakeGithub, daemon: &StreamingDaemon) -> String {
    let comments: Vec<String> = fake
        .comments(REPO, ISSUE)
        .iter()
        .map(|c| format!("  {}: {:?}", c.author, c.body))
        .collect();
    format!(
        "issue: {:?}\ncomments:\n{}\nrun dirs: {:?}\ndaemon.log:\n{}\ndaemon stderr:\n{}",
        fake.issue_state(REPO, ISSUE),
        comments.join("\n"),
        run_dirs(home, "widgets"),
        std::fs::read_to_string(daemon_log(home)).unwrap_or_default(),
        daemon.stderr_so_far()
    )
}

/// Poll `ready` to [`BUDGET`], failing with `what` and the [`dump`] when it never holds.
fn wait_for(
    home: &Path,
    fake: &FakeGithub,
    daemon: &StreamingDaemon,
    what: &str,
    ready: impl Fn() -> bool,
) {
    wait_until(what, BUDGET, ready, || dump(home, fake, daemon));
}

/// Poll `ready` to `budget`, failing with `what` and `dump`'s report when it never holds.
fn wait_until(what: &str, budget: Duration, ready: impl Fn() -> bool, dump: impl Fn() -> String) {
    let deadline = Instant::now() + budget;
    while !ready() {
        assert!(
            Instant::now() < deadline,
            "{what} within {budget:?}\n{}",
            dump()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Whether `fake`'s thread on issue `number` holds a claim marker.
fn marked(fake: &FakeGithub, number: u64) -> bool {
    fake.comments(REPO, number)
        .iter()
        .any(|c| c.author == ME && c.body.starts_with("[afkd-claim]"))
}

/// Read `path` whole, naming it when it is missing.
fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Drive one issue through the plugin, installed in `home`, on a real daemon: it is
/// **claimed** (the marker and `afkd/claimed`, and `on_claim`'s call assigned the token's
/// own user), **run** (its run dir is named for the issue, its task carries the issue whole,
/// and `on_run` read every field of its handle as the plugin sent it) and **finished** (the
/// marker deleted, and `on_done`'s calls took the gate off, commented with the issue's
/// title and closed it), and the daemon then drains clean.
fn drive_one_issue(home: &Path) {
    let fake = FakeGithub::start(ME);
    fake.issue(REPO, ISSUE, TITLE, BODY, LABELS, &[]);
    write_config(home, &service(home, fake.host()));
    let (code, report) = validate(home);
    assert_eq!(code, Some(0), "the service validates:\n{report}");

    let daemon = spawn_headless_streaming(home, &[]);

    wait_for(home, &fake, &daemon, "the issue is claimed", || {
        let issue = fake.issue_state(REPO, ISSUE);
        marked(&fake, ISSUE)
            && issue.labels.iter().any(|l| l == "afkd/claimed")
            && issue.assignees == [ME]
    });
    // The run dir is made before the spine lays the unit's files into it, so the wait is
    // for a written brief, not the bare dir.
    let run_suffix = format!("-unit-{ISSUE}-1");
    let brief = || {
        run_dirs(home, "widgets")
            .into_iter()
            .find(|name| name.ends_with(&run_suffix))
            .map(|run| runs_root(home).join("widgets").join(run).join("task.md"))
            .filter(|task| task.metadata().is_ok_and(|meta| meta.len() > 0))
    };
    wait_for(home, &fake, &daemon, "the issue's run starts", || {
        brief().is_some()
    });
    let task = read(&brief().expect("waited for"));
    for part in [TITLE, "> \"backoff\" — nobody", "```\nretry: 0\n```"] {
        assert!(task.contains(part), "task.md carries {part:?}:\n{task}");
    }

    std::fs::write(home.join("release"), "").expect("release the run");
    wait_for(home, &fake, &daemon, "the issue is finished", || {
        fake.issue_state(REPO, ISSUE).state == "closed" && !marked(&fake, ISSUE)
    });
    let issue = fake.issue_state(REPO, ISSUE);
    assert!(
        !issue.labels.iter().any(|l| l == "afkd/claimed"),
        "on_done took afkd/claimed off: {issue:?}"
    );
    assert!(
        fake.comments(REPO, ISSUE).iter().any(|c| c.author == ME
            && c.body.starts_with("done in ")
            && c.body.ends_with(&format!(" by {ME}: {TITLE}"))
            && !c.body.contains("#{")),
        "on_done's comment landed with its run fact, `BOT` and the title filled in:\n{}",
        dump(home, &fake, &daemon)
    );
    // Every field verbatim — the `$` lines shell-quote what they interpolate — and the
    // labels by name, as the poll read them before the claim's own.
    assert_eq!(
        read(&home.join(FIELDS)),
        format!(
            "title {TITLE}\nurl {}\nnumber {ISSUE}\nlabel afkd/ready\nlabel 優先/high\n",
            fake.url(REPO, ISSUE)
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
}

/// The pull request the review service claims: the bot's own, on a wide, slashed branch,
/// titled with the quotes a log line wraps it in, wide CJK and an emoji, as the wire
/// suite's is.
const PR: u64 = 7;
const PR_TITLE: &str = "Cap the retry backoff — \"重试\" 上限 🚦";
const BRANCH: &str = "fix/重试-retry-cap";
/// The human on the pull request, and their review comment: multi-line, with an indented
/// code line.
const HUMAN: &str = "陳大文";
const REVIEW: &str = "看起来不对 🚨 — the cap never applies:\n\n    max_backoff = 0\n";

/// The review service that drives [`PR`], `work` its work dir: its `on_run` writes every
/// field of the pull request's handle to [`FIELDS`], a line each, and its `on_done` answers
/// the round with a comment naming the pull request's title — the bot's reply, newer than
/// the feedback, so the next poll does not claim the pull request again.
fn pr_service(work: &Path, host: &str) -> String {
    format!(
        r#"import "core"
import "@afkd/github"

reviews :: service(github.pr) {{
  host         "{host}"
  repo         "{REPO}"
  token        "{TOKEN}"
  author_me    true
  every        1s
  max_attempts 1

  on_done(run: core.Run, pr: github.Pull_Request, outcome: core.Outcome) {{
    github.pr_comment(pr, "round answered: #{{pr.title}}")
  }}

  work_dir "{work}"
  on_run(run: core.Run, pr: github.Pull_Request) {{
    $ echo title #{{pr.title}} >> {FIELDS}
    $ echo url #{{pr.url}} >> {FIELDS}
    $ echo number #{{pr.number}} >> {FIELDS}
    $ echo branch #{{pr.branch}} >> {FIELDS}
  }}
}}
"#,
        work = work.display()
    )
}

/// Drive one pull request through the plugin, installed in `home`, on a fresh daemon: the
/// bot's own PR, a human assigned to it and the human's review comment two minutes old —
/// new feedback, seeded as the wire suite seeds it — is **claimed**, **run** (`on_run` read
/// every field of its handle as the plugin sent it) and **finished** (the marker deleted,
/// and `on_done`'s reply landed with the title filled in), and the daemon then drains
/// clean.
fn drive_one_pr(home: &Path) {
    let fake = FakeGithub::start(ME);
    fake.pull(REPO, PR, ME, BRANCH, &[], &[HUMAN]);
    fake.retitle(REPO, PR, PR_TITLE);
    fake.comment(REPO, PR, HUMAN, REVIEW, 120);
    let work = home.join("reviews");
    std::fs::create_dir_all(&work).expect("mk the review work dir");
    write_config(home, &pr_service(&work, fake.host()));
    let (code, report) = validate(home);
    assert_eq!(code, Some(0), "the review service validates:\n{report}");

    let daemon = spawn_headless_streaming(home, &[]);
    let fields = work.join(FIELDS);
    let answer = format!("round answered: {PR_TITLE}");
    let answered = || {
        fake.comments(REPO, PR)
            .iter()
            .any(|c| c.author == ME && c.body == answer)
    };
    let report = || {
        let thread: Vec<String> = fake
            .comments(REPO, PR)
            .iter()
            .map(|c| format!("  {}: {:?}", c.author, c.body))
            .collect();
        format!(
            "pr: {:?}\nthread:\n{}\n{FIELDS}: {:?}\nrun dirs: {:?}\ndaemon.log:\n{}\n\
             daemon stderr:\n{}",
            fake.issue_state(REPO, PR),
            thread.join("\n"),
            std::fs::read_to_string(&fields).unwrap_or_default(),
            run_dirs(home, "reviews"),
            std::fs::read_to_string(daemon_log(home)).unwrap_or_default(),
            daemon.stderr_so_far()
        )
    };
    wait_until(
        "the pull request is claimed, run and answered",
        BUDGET,
        || answered() && !marked(&fake, PR),
        report,
    );
    assert_eq!(
        read(&fields),
        format!(
            "title {PR_TITLE}\nurl {}\nnumber {PR}\nbranch {BRANCH}\n",
            fake.url(REPO, PR)
        ),
        "{}",
        report()
    );

    daemon.signal(libc::SIGINT);
    let out = daemon.reap(Duration::from_secs(10));
    assert!(
        out.status.success(),
        "the daemon drains clean ({:?}):\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Why both attempts of the failing service fail: the outage's own sentence.
const OUTAGE_FAULT: &str = "git fetch: could not resolve host — the network went away 🚨";

/// The label the failing service's `on_fail` adds: a slash, a space and an emoji.
const NEEDS_HUMAN: &str = "needs/human 🚧";

/// A service whose runs fail: two attempts, each holding until the test creates `release`,
/// then failing with [`OUTAGE_FAULT`]. Its `on_fail` checks every action's result and fails
/// the slot on the first refused, so the slot running to its end through an outage is the
/// plugin answering each queued write `ok`.
fn failing_service(home: &Path, host: &str) -> String {
    format!(
        r#"import "core"
import "@afkd/github"

widgets :: service(github) {{
  host         "{host}"
  repo         "{REPO}"
  token        "{TOKEN}"
  source_label "afkd/ready"
  every        1s
  max_attempts 2

  on_claim(issue: github.Issue) {{
    if github.assign_me(issue) != nil {{ fail("assign_me") }}
  }}
  on_fail(run: core.Run, issue: github.Issue, outcome: core.Outcome) {{
    if github.remove_label(issue, "afkd/ready") != nil {{ fail("remove_label afkd/ready") }}
    if github.unassign(issue) != nil {{ fail("unassign") }}
    if github.add_label(issue, "{NEEDS_HUMAN}") != nil {{ fail("add_label") }}
    if outcome.error == nil {{ fail("on_fail without an error") }}
    if github.comment(issue, "attempts spent: #{{outcome.error.message}}") != nil {{ fail("comment") }}
    if github.remove_label(issue, "afkd/claimed") != nil {{ fail("remove_label afkd/claimed") }}
  }}

  work_dir "{home}"
  on_run(run: core.Run, issue: github.Issue) {{
    $ i=0; until [ -f release ] || [ $i -ge 600 ]; do sleep 0.1; i=$((i+1)); done
    fail("{OUTAGE_FAULT}")
  }}
}}
"#,
        home = home.display()
    )
}

/// The trello incident of 2026-10-01, replayed on GitHub on a real daemon: GitHub goes out
/// of reach once the issue is claimed and stays out through both failed attempts, the
/// finish and every action `on_fail` calls, and past the first retry. The plugin queues each
/// write it cannot deliver and answers `ok`, so the checked slot runs to its end; once
/// GitHub is back the issue ends in its failed state — the source label off, the bot
/// unassigned, the `needs/human 🚧` label and the slot's comment with the fault on it, its
/// claim released — with nothing given up.
fn fail_one_issue_through_an_outage(home: &Path) {
    let fake = FakeGithub::start(ME);
    fake.issue(REPO, ISSUE, TITLE, BODY, LABELS, &[]);
    write_config(home, &failing_service(home, fake.host()));
    let (code, report) = validate(home);
    assert_eq!(code, Some(0), "the service validates:\n{report}");
    assert!(!report.contains("warning:"), "cleanly:\n{report}");

    let daemon = spawn_headless_streaming(home, &[]);
    wait_for(home, &fake, &daemon, "the issue is claimed", || {
        let issue = fake.issue_state(REPO, ISSUE);
        marked(&fake, ISSUE)
            && issue.labels.iter().any(|l| l == "afkd/claimed")
            && issue.assignees == [ME]
    });

    let outage_start = Instant::now();
    fake.outage();
    std::fs::write(home.join("release"), "").expect("release the run");
    // `on_fail`'s last action, queued behind everything before it: the marker's delete and
    // the slot's earlier writes. Seeing it means the checked slot ran to its end.
    let queued_last = "removing the label \"afkd/claimed\" is queued behind them";
    wait_for(
        home,
        &fake,
        &daemon,
        "on_fail runs through the outage",
        || daemon.stderr_so_far().contains(queued_last),
    );
    // And the outage outlasts the first retry, 5 s after the first failure.
    wait_for(home, &fake, &daemon, "a retry fails in the outage", || {
        daemon
            .stderr_so_far()
            .contains("is tried again in 10s (try 3)")
    });
    fake.restore();
    let outage_for = outage_start.elapsed();
    assert!(!fake.dropped().is_empty(), "the outage dropped requests");

    // The head write (the claim marker's delete, queued at the finish) is next tried at
    // most the outage's length plus the first gap after it failed — the gap doubles from
    // 5 s, and its tries fall at +5, +15, +35 s… — and one pass then lands the issue's
    // whole backlog.
    let commented = || {
        fake.comments(REPO, ISSUE).into_iter().any(|c| {
            c.author == ME
                && c.body.starts_with("attempts spent: ")
                && c.body.contains(OUTAGE_FAULT)
                && !c.body.contains("#{")
        })
    };
    wait_until(
        "the issue reaches its failed state",
        outage_for + Duration::from_secs(5) + BUDGET,
        || {
            let issue = fake.issue_state(REPO, ISSUE);
            !issue
                .labels
                .iter()
                .any(|l| l == "afkd/claimed" || l == "afkd/ready")
                && issue.labels.iter().any(|l| l == NEEDS_HUMAN)
                && issue.assignees.is_empty()
                && !marked(&fake, ISSUE)
                && commented()
        },
        || dump(home, &fake, &daemon),
    );
    let stderr = daemon.stderr_so_far();
    assert!(
        !stderr.contains("gave up"),
        "nothing was given up:\n{stderr}"
    );

    daemon.signal(libc::SIGINT);
    let out = daemon.reap(Duration::from_secs(10));
    assert!(
        out.status.success(),
        "the daemon drains clean ({:?}):\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
}

// --- the legs ----------------------------------------------------------------------

#[test]
fn install_leg_places_the_plugin_and_runs_one_issue_and_one_pr_through_it() {
    let home = TempDir::new().expect("tempdir");
    let stage_dir = TempDir::new().expect("tempdir");
    let staged = stage(stage_dir.path());
    install(home.path(), &staged);
    let placed = plugins_root(home.path()).join(NAME);
    assert_eq!(
        tree_of(&placed),
        tree_of(&staged),
        "what lands is exactly the release's contents"
    );
    let exec = placed.join("afkd-github");
    let mode = std::fs::metadata(&exec)
        .unwrap_or_else(|e| panic!("the install placed {}: {e}", exec.display()))
        .permissions()
        .mode();
    assert!(mode & 0o111 != 0, "{} is executable", exec.display());
    drive_one_issue(home.path());
    drive_one_pr(home.path());
}

#[test]
fn outage_leg_a_failed_issue_still_reaches_its_on_fail_state() {
    let home = TempDir::new().expect("tempdir");
    let stage_dir = TempDir::new().expect("tempdir");
    install(home.path(), &stage(stage_dir.path()));
    fail_one_issue_through_an_outage(home.path());
}

/// A service of each kind, whose slots between them call every action the plugin provides —
/// each kind's own six, on the item its slot is passed — name the bot by the config's own
/// `BOT`, read the run and the outcome, in the argument shapes the manifest types: wide and
/// scoped label names, and a multi-line comment.
const BOTH_KINDS: &str = r##"import "core"
import "@afkd/github"

BOT :: "autocoder"

issues :: service(github) {
  host         "ghe.example.com"
  repo         "acme/widgets"
  token        "REPLACE_ME"
  source_label "afkd/ready"
  max_attempts 2
  every        1m~3m

  on_claim(issue: github.Issue) {
    _ = github.assign_me(issue)
    _ = github.add_label(issue, "afkd/working ⚙")
  }
  on_done(run: core.Run, issue: github.Issue, outcome: core.Outcome) {
    _ = github.remove_label(issue, "afkd/working ⚙")
    _ = github.comment(issue, "done by #{BOT} in #{outcome.duration}:\n\n- run #{run.id}\n- 完了 ✅")
    _ = github.close(issue)
  }
  on_fail(run: core.Run, issue: github.Issue, outcome: core.Outcome) {
    _ = github.remove_label(issue, "afkd/working ⚙")
    _ = github.unassign(issue)
  }

  work_dir "/srv/acme/widgets"
  on_run(run: core.Run, issue: github.Issue) {
    $ cat $AFKD_SCRATCH_DIR/task.md
  }
}

reviews :: service(github.pr) {
  repo      "acme/widgets"
  token     "REPLACE_ME"
  author_me true
  every     2m

  on_claim(pr: github.Pull_Request) {
    _ = github.pr_assign_me(pr)
    _ = github.pr_add_label(pr, "afkd/reviewing 👀")
  }
  on_done(run: core.Run, pr: github.Pull_Request, outcome: core.Outcome) {
    _ = github.pr_remove_label(pr, "afkd/reviewing 👀")
    _ = github.pr_comment(pr, "round answered by #{BOT} in #{outcome.duration}")
  }
  on_fail(run: core.Run, pr: github.Pull_Request, outcome: core.Outcome) {
    _ = github.pr_unassign(pr)
    _ = github.pr_close(pr)
  }

  work_dir "/srv/acme/widgets"
  on_run(run: core.Run, pr: github.Pull_Request) {
    $ cat $AFKD_SCRATCH_DIR/task.md
  }
}
"##;

/// Both kinds validate as v2 services against the installed plugin: every setting either
/// writes is one its kind declares, of its type, every action its slots call is one the
/// plugin provides for that kind, with the item and the parameters it declares, and the bot
/// is named by the config's own `BOT`. The label actions' noun-first names —
/// `github.label_add`, `github.pr_label_remove` — are no aliases: the same file
/// spelling them so is refused.
#[test]
fn every_kind_validates_as_a_v2_service() {
    let home = TempDir::new().expect("tempdir");
    let stage_dir = TempDir::new().expect("tempdir");
    install(home.path(), &stage(stage_dir.path()));
    let verbs = [
        "assign_me",
        "unassign",
        "add_label",
        "remove_label",
        "close",
        "comment",
    ];
    for verb in verbs {
        for call in [
            format!("github.{verb}(issue"),
            format!("github.pr_{verb}(pr"),
        ] {
            assert!(BOTH_KINDS.contains(&call), "the file calls {call}");
        }
    }
    for used in ["BOT", "outcome.duration", "run.id"] {
        assert!(BOTH_KINDS.contains(used), "the file uses {used}");
    }
    write_config(home.path(), BOTH_KINDS);
    let (code, report) = validate(home.path());
    assert_eq!(code, Some(0), "both kinds validate:\n{report}");
    assert!(!report.contains("warning:"), "cleanly:\n{report}");

    let noun_first = BOTH_KINDS
        .replace("add_label(", "label_add(")
        .replace("remove_label(", "label_remove(");
    write_config(home.path(), &noun_first);
    let (code, report) = validate(home.path());
    assert_eq!(code, Some(1), "the noun-first names are refused:\n{report}");
    for old in [
        "label_add",
        "label_remove",
        "pr_label_add",
        "pr_label_remove",
    ] {
        assert!(
            report
                .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                .any(|word| word == old),
            "the refusal names {old}:\n{report}"
        );
    }
}

/// An action acts on the item it is passed, typed by kind, so an issue action passed the
/// pull request a `service(github.pr)` slot is given — `github.comment(pr, "…")` — is a
/// load error naming the handle types, not a call on whatever item it names.
#[test]
fn a_call_with_the_wrong_handle_is_a_load_error() {
    let home = TempDir::new().expect("tempdir");
    let stage_dir = TempDir::new().expect("tempdir");
    install(home.path(), &stage(stage_dir.path()));
    let config = |call: &str| {
        format!(
            r#"import "core"
import "@afkd/github"

reviews :: service(github.pr) {{
  repo  "acme/widgets"
  token "REPLACE_ME"

  on_done(run: core.Run, pr: github.Pull_Request, outcome: core.Outcome) {{ {call} }}

  work_dir "/srv/acme/widgets"
  on_run(run: core.Run, pr: github.Pull_Request) {{
    $ true
  }}
}}
"#
        )
    };
    write_config(
        home.path(),
        &config(r#"github.comment(pr, "round answered — 完了 ✅")"#),
    );
    let (code, report) = validate(home.path());
    assert_eq!(
        code,
        Some(1),
        "an issue action on a pr is refused:\n{report}"
    );
    assert!(
        report.contains("github.Issue") && report.contains("github.Pull_Request"),
        "the refusal names both handle types:\n{report}"
    );
    // The same file calling the pr kind's own action is valid: the handle is all that was
    // wrong.
    write_config(
        home.path(),
        &config(r#"github.pr_comment(pr, "round answered — 完了 ✅")"#),
    );
    let (code, report) = validate(home.path());
    assert_eq!(code, Some(0), "the pr action validates:\n{report}");
}

/// The claim key afkd renamed to `every`, spelled in halves so a `git grep` for it holds
/// this repo to writing it nowhere else.
const OLD_CADENCE: &str = concat!("poll", "_interval");

/// A service of each kind whose claim cadence is written `{key} 1m~3m`, the line a group
/// of its own, so the file is as `afkd fmt` prints it under either key.
fn cadence_services(key: &str) -> String {
    format!(
        r#"import "core"
import "@afkd/github"

issues :: service(github) {{
  repo  "acme/widgets"
  token "REPLACE_ME"

  {key} 1m~3m

  work_dir "/srv/acme/widgets"

  on_run proc(run: core.Run, issue: github.Issue) {{
    $ true
  }}
}}

reviews :: service(github.pr) {{
  repo  "acme/widgets"
  token "REPLACE_ME"

  {key} 1m~3m

  work_dir "/srv/acme/widgets"

  on_run proc(run: core.Run, pr: github.Pull_Request) {{
    $ true
  }}
}}
"#
    )
}

/// The claim cadence is afkd's own `every`, and neither kind declares [`OLD_CADENCE`]
/// beside it: a service of either kind writing it with `1m~3m` is refused at validate,
/// naming the setting, the service and the key to write instead, and the same services
/// writing `every 1m~3m` validate.
#[test]
fn the_old_cadence_key_is_refused_naming_every_and_every_validates() {
    let home = TempDir::new().expect("tempdir");
    let stage_dir = TempDir::new().expect("tempdir");
    install(home.path(), &stage(stage_dir.path()));
    write_config(home.path(), &cadence_services(OLD_CADENCE));
    let (code, report) = validate(home.path());
    assert_eq!(code, Some(1), "{OLD_CADENCE} is refused:\n{report}");
    for said in [
        format!(r#"unknown setting "{OLD_CADENCE}" for service(github)"#),
        format!(r#"unknown setting "{OLD_CADENCE}" for service(github.pr)"#),
        r#"did you mean "every"?"#.to_owned(),
    ] {
        assert!(report.contains(&said), "the refusal says {said}:\n{report}");
    }
    write_config(home.path(), &cadence_services("every"));
    let (code, report) = validate(home.path());
    assert_eq!(code, Some(0), "every validates:\n{report}");
    assert!(!report.contains("warning:"), "cleanly:\n{report}");
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

#[test]
fn every_readme_conf_fence_validates() {
    let readme = std::fs::read_to_string(plugin_root().join("README.md")).expect("the README");
    let fences = conf_fences(&readme);
    let openers = readme.lines().filter(|l| l.trim() == "```conf").count();
    assert!(openers > 0, "the README carries conf fences");
    assert_eq!(fences.len(), openers, "every conf opener yields a fence");

    let home = TempDir::new().expect("tempdir");
    let stage_dir = TempDir::new().expect("tempdir");
    install(home.path(), &stage(stage_dir.path()));
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
