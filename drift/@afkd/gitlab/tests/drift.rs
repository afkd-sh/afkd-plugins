//! The gate that holds the **shipped `@afkd/gitlab` plugin** — a manifest v2 provider whose
//! kinds are `service(gitlab)` and `service(gitlab.mr)`, built from source at install time —
//! to a real afkd: the `afkd` first on `PATH`, the installed one and never a build, since a
//! plugin is checked against the afkd it will meet ([`bin_path`]).
//!
//! - The **install** leg installs the tree the release tarball holds and runs one issue
//!   through it on a real daemon, its slots' actions crossing as `call`s on the issue each
//!   is passed and its slots reading every field the issue's handle declares — then one
//!   merge request the same way, through a `service(gitlab.mr)`.
//! - The **outage** leg fails an issue through both its attempts while GitLab is out of
//!   reach, its `on_fail` checking every action's result, and holds the issue to its failed
//!   state once GitLab is back: the slot's labels and comment on it, the bot unassigned, its
//!   claim released.
//! - The **both kinds** leg `afkd validate`s one file with a service of each kind, whose
//!   slots call every action the plugin provides on the item each is passed and name the
//!   bot by the config's own constant `BOT`, and the **handle** leg holds `afkd validate`
//!   to refusing an action passed the other kind's item.
//! - The **README** leg `afkd validate`s every `conf` fence the plugin's README carries,
//!   each a whole entry file, against the installed plugin.
//!
//! The GitLab is the plugin's own loopback fake ([`fake`]), so no leg touches a network, and
//! every wait is bounded and every spawn held in a [`Daemon`]: a claim that never lands must
//! **fail**, not hang.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::fake::{FakeGitlab, TOKEN};
use common::*;
use tempfile::TempDir;

// --- the shipped tree --------------------------------------------------------------

/// The plugin's name, as the manifest spells it and afkd places it.
const NAME: &str = "@afkd/gitlab";

/// The plugin's root — the directory `afkd install` takes, whose path this crate's own
/// mirrors under `drift/`. Canonicalized, so a staged copy and a report both name one path.
fn plugin_root() -> PathBuf {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../@afkd/gitlab");
    std::fs::canonicalize(root).expect("the shipped plugin resolves")
}

/// Copy `src` onto `dst` recursively, leaving out `skip`'s names at the top level and every
/// `__pycache__` below it. `std::fs::copy` carries the source's mode on Unix, so the skill's
/// scripts stay executable, as the release's `cp -a` keeps them.
fn copy_tree(src: &Path, dst: &Path, skip: &[&str]) {
    std::fs::create_dir_all(dst).expect("mk dst");
    for entry in std::fs::read_dir(src).expect("read src") {
        let entry = entry.expect("a directory entry");
        let name = entry.file_name();
        if skip.iter().any(|s| name == **s) || name == "__pycache__" {
            continue;
        }
        let (from, to) = (entry.path(), dst.join(&name));
        if from.is_dir() {
            copy_tree(&from, &to, &[]);
        } else {
            std::fs::copy(&from, &to).expect("copy");
        }
    }
}

/// Stage the tree the release tarball holds — the plugin without its `target/` — under
/// `dst`, and return the staged root. Installing the live checkout instead would hand afkd
/// whatever this host last built there.
fn stage(dst: &Path) -> PathBuf {
    let staged = dst.join("gitlab");
    copy_tree(&plugin_root(), &staged, &["target"]);
    for present in [
        "afkd-plugin.toml",
        "Cargo.toml",
        "Cargo.lock",
        "src",
        "skills/gitlab/SKILL.md",
    ] {
        assert!(
            staged.join(present).exists(),
            "the staged copy holds {present}"
        );
    }
    assert!(
        !staged.join("target").exists(),
        "the staged copy holds no target/"
    );
    staged
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

/// The token's own user, and so the author of every note the plugin posts. Wide, accented
/// and bracketed, as the wire suite's is.
const ME: &str = "björn-öst[bot]";
const ME_ID: u64 = 7;

/// A nested, dotted path-with-namespace, as the wire suite's is: it crosses the real spine
/// and reaches the fake as one percent-encoded `:id` segment.
const PROJECT: &str = "acme/sub.group/widgets";

/// The issue the service claims, titled and described in the shapes that break a naive
/// transport: wide glyphs, an emoji, and a multi-line body with quoting and a fence.
const ISSUE: u64 = 7;
const TITLE: &str = "修复 the retry storm 🚨";
const BODY: &str = "Retries pile up after a 502.\n\n> \"backoff\" — nobody\n\n```\nretry: 0\n```\n";
/// The issue's labels: the source label, and a wide, scoped one beside it.
const LABELS: &[&str] = &["afkd::ready", "優先::high"];

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
fn service(home: &Path, base_url: &str) -> String {
    format!(
        r#"import "core"
import "@afkd/gitlab"

BOT :: "{ME}"

widgets :: service(gitlab) {{
  base_url      "{base_url}"
  project       "{PROJECT}"
  token         "{TOKEN}"
  source_label  "afkd::ready"
  poll_interval 1s
  max_attempts  1

  on_claim(issue: gitlab.Issue) {{ gitlab.assign_me(issue) }}
  on_done(run: core.Run, issue: gitlab.Issue, outcome: core.Outcome) {{
    gitlab.label_remove(issue, "afkd::claimed")
    gitlab.comment(issue, "done in #{{outcome.duration}} by #{{BOT}}: #{{issue.title}}")
    gitlab.close(issue)
  }}

  work_dir "{home}"
  on_run(run: core.Run, issue: gitlab.Issue) {{
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
/// (`env.get("GITLAB_TOKEN")`), as the README's lead example does.
const CONFIG_ENV: &[(&str, &str)] = &[("GITLAB_TOKEN", TOKEN)];

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

/// Everything a failed wait needs to say: the issue as the fake holds it, its notes, the
/// run dirs, the daemon's own log, and its stderr so far — where the plugin's own
/// complaints land.
fn dump(home: &Path, fake: &FakeGitlab, daemon: &StreamingDaemon) -> String {
    let notes: Vec<String> = fake
        .notes(PROJECT, ISSUE)
        .iter()
        .map(|n| format!("  {}: {:?}", n.author, n.body))
        .collect();
    format!(
        "issue: {:?}\nnotes:\n{}\nrun dirs: {:?}\ndaemon.log:\n{}\ndaemon stderr:\n{}",
        fake.issue_state(PROJECT, ISSUE),
        notes.join("\n"),
        run_dirs(home, "widgets"),
        std::fs::read_to_string(daemon_log(home)).unwrap_or_default(),
        daemon.stderr_so_far()
    )
}

/// Poll `ready` to [`BUDGET`], failing with `what` and the [`dump`] when it never holds.
fn wait_for(
    home: &Path,
    fake: &FakeGitlab,
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
fn marked(fake: &FakeGitlab, number: u64) -> bool {
    fake.notes(PROJECT, number)
        .iter()
        .any(|n| n.author == ME && n.body.starts_with("[afkd-claim]"))
}

/// Read `path` whole, naming it when it is missing.
fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Drive one issue through the plugin, installed in `home`, on a real daemon: it is
/// **claimed** (the marker and `afkd::claimed`, and `on_claim`'s call assigned the token's
/// own user), **run** (its run dir is named for the issue, its task carries the issue whole,
/// and `on_run` read every field of its handle as the plugin sent it) and **finished** (the
/// marker deleted, and `on_done`'s calls took the gate off, commented with the issue's
/// title and closed it), and the daemon then drains clean.
fn drive_one_issue(home: &Path) {
    let fake = FakeGitlab::start(ME_ID, ME);
    fake.issue(PROJECT, ISSUE, TITLE, BODY, LABELS, &[]);
    write_config(home, &service(home, fake.base_url()));
    let (code, report) = validate(home);
    assert_eq!(code, Some(0), "the service validates:\n{report}");

    let daemon = spawn_headless_streaming(home, &[]);

    wait_for(home, &fake, &daemon, "the issue is claimed", || {
        let issue = fake.issue_state(PROJECT, ISSUE);
        marked(&fake, ISSUE)
            && issue.labels.iter().any(|l| l == "afkd::claimed")
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
        fake.issue_state(PROJECT, ISSUE).state == "closed" && !marked(&fake, ISSUE)
    });
    let issue = fake.issue_state(PROJECT, ISSUE);
    assert!(
        !issue.labels.iter().any(|l| l == "afkd::claimed"),
        "on_done took afkd::claimed off: {issue:?}"
    );
    assert!(
        fake.notes(PROJECT, ISSUE).iter().any(|n| n.author == ME
            && n.body.starts_with("done in ")
            && n.body.ends_with(&format!(" by {ME}: {TITLE}"))
            && !n.body.contains("#{")),
        "on_done's comment landed with its run fact, `BOT` and the title filled in:\n{}",
        dump(home, &fake, &daemon)
    );
    // Every field verbatim — the `$` lines shell-quote what they interpolate — and the
    // labels by name, as the poll read them before the claim's own.
    assert_eq!(
        read(&home.join(FIELDS)),
        format!(
            "title {TITLE}\nurl {}\nnumber {ISSUE}\nlabel afkd::ready\nlabel 優先::high\n",
            fake.issue_url(PROJECT, ISSUE)
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

/// The merge request the review service claims: the bot's own, on a wide, slashed branch,
/// titled with the quotes a log line wraps it in, wide CJK and an emoji, as the wire
/// suite's is.
const MR: u64 = 7;
const MR_TITLE: &str = "Cap the retry backoff — \"重试\" 上限 🚦";
const BRANCH: &str = "feature/重试-backoff";
/// The human on the merge request, and their review note: multi-line, with an indented code
/// line.
const HUMAN: &str = "陳大文";
const REVIEW: &str = "看起来不对 🚨 — the cap never applies:\n\n    max_backoff = 0\n";

/// The review service that drives [`MR`], `work` its work dir: its `on_run` writes every
/// field of the merge request's handle to [`FIELDS`], a line each, and its `on_done` answers
/// the round with a comment naming the merge request's title — the bot's reply, newer than
/// the feedback, so the next poll does not claim the merge request again.
fn mr_service(work: &Path, base_url: &str) -> String {
    format!(
        r#"import "core"
import "@afkd/gitlab"

reviews :: service(gitlab.mr) {{
  base_url      "{base_url}"
  project       "{PROJECT}"
  token         "{TOKEN}"
  author_me     true
  poll_interval 1s
  max_attempts  1

  on_done(run: core.Run, mr: gitlab.Merge_Request, outcome: core.Outcome) {{
    gitlab.mr_comment(mr, "round answered: #{{mr.title}}")
  }}

  work_dir "{work}"
  on_run(run: core.Run, mr: gitlab.Merge_Request) {{
    $ echo title #{{mr.title}} >> {FIELDS}
    $ echo url #{{mr.url}} >> {FIELDS}
    $ echo number #{{mr.number}} >> {FIELDS}
    $ echo branch #{{mr.branch}} >> {FIELDS}
  }}
}}
"#,
        work = work.display()
    )
}

/// Drive one merge request through the plugin, installed in `home`, on a fresh daemon: the
/// bot's own MR, a human assigned to it and the human's review note two minutes old — new
/// feedback, seeded as the wire suite seeds it — is **claimed**, **run** (`on_run` read
/// every field of its handle as the plugin sent it) and **finished** (the marker deleted,
/// and `on_done`'s reply landed with the title filled in), and the daemon then drains
/// clean.
fn drive_one_mr(home: &Path) {
    let fake = FakeGitlab::start(ME_ID, ME);
    fake.user(99, HUMAN);
    fake.mr(PROJECT, MR, ME, BRANCH, &[HUMAN]);
    fake.retitle(PROJECT, MR, MR_TITLE);
    fake.mr_note(PROJECT, MR, HUMAN, REVIEW, 120);
    let work = home.join("reviews");
    std::fs::create_dir_all(&work).expect("mk the review work dir");
    write_config(home, &mr_service(&work, fake.base_url()));
    let (code, report) = validate(home);
    assert_eq!(code, Some(0), "the review service validates:\n{report}");

    let daemon = spawn_headless_streaming(home, &[]);
    let fields = work.join(FIELDS);
    let answer = format!("round answered: {MR_TITLE}");
    let answered = || {
        fake.mr_notes(PROJECT, MR)
            .iter()
            .any(|n| n.author == ME && n.body == answer)
    };
    let marked = || {
        fake.mr_notes(PROJECT, MR)
            .iter()
            .any(|n| n.author == ME && n.body.starts_with("[afkd-claim]"))
    };
    let report = || {
        let thread: Vec<String> = fake
            .mr_notes(PROJECT, MR)
            .iter()
            .map(|n| format!("  {}: {:?}", n.author, n.body))
            .collect();
        format!(
            "mr: {:?}\nthread:\n{}\n{FIELDS}: {:?}\nrun dirs: {:?}\ndaemon.log:\n{}\n\
             daemon stderr:\n{}",
            fake.mr_state(PROJECT, MR),
            thread.join("\n"),
            std::fs::read_to_string(&fields).unwrap_or_default(),
            run_dirs(home, "reviews"),
            std::fs::read_to_string(daemon_log(home)).unwrap_or_default(),
            daemon.stderr_so_far()
        )
    };
    wait_until(
        "the merge request is claimed, run and answered",
        BUDGET,
        || answered() && !marked(),
        report,
    );
    assert_eq!(
        read(&fields),
        format!(
            "title {MR_TITLE}\nurl {}\nnumber {MR}\nbranch {BRANCH}\n",
            fake.mr_url(PROJECT, MR)
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

/// The label the failing service's `on_fail` adds: scoped, with an emoji.
const NEEDS_HUMAN: &str = "needs::human 🚧";

/// A service whose runs fail: two attempts, each holding until the test creates `release`,
/// then failing with [`OUTAGE_FAULT`]. Its `on_fail` checks every action's result and fails
/// the slot on the first refused, so the slot running to its end through an outage is the
/// plugin answering each queued write `ok`.
fn failing_service(home: &Path, base_url: &str) -> String {
    format!(
        r#"import "core"
import "@afkd/gitlab"

widgets :: service(gitlab) {{
  base_url      "{base_url}"
  project       "{PROJECT}"
  token         "{TOKEN}"
  source_label  "afkd::ready"
  poll_interval 1s
  max_attempts  2

  on_claim(issue: gitlab.Issue) {{
    if gitlab.assign_me(issue) != nil {{ fail("assign_me") }}
  }}
  on_fail(run: core.Run, issue: gitlab.Issue, outcome: core.Outcome) {{
    if gitlab.label_remove(issue, "afkd::ready") != nil {{ fail("label_remove afkd::ready") }}
    if gitlab.unassign(issue) != nil {{ fail("unassign") }}
    if gitlab.label_add(issue, "{NEEDS_HUMAN}") != nil {{ fail("label_add") }}
    if gitlab.comment(issue, "attempts spent: #{{outcome.error}}") != nil {{ fail("comment") }}
    if gitlab.label_remove(issue, "afkd::claimed") != nil {{ fail("label_remove afkd::claimed") }}
  }}

  work_dir "{home}"
  on_run(run: core.Run, issue: gitlab.Issue) {{
    $ i=0; until [ -f release ] || [ $i -ge 600 ]; do sleep 0.1; i=$((i+1)); done
    fail("{OUTAGE_FAULT}")
  }}
}}
"#,
        home = home.display()
    )
}

/// The trello incident of 2026-10-01, replayed on GitLab on a real daemon: GitLab goes out
/// of reach once the issue is claimed and stays out through both failed attempts, the
/// finish and every action `on_fail` calls, and past the first retry. The plugin queues each
/// write it cannot deliver and answers `ok`, so the checked slot runs to its end; once
/// GitLab is back the issue ends in its failed state — the source label off, the bot
/// unassigned, the `needs::human 🚧` label and the slot's comment with the fault on it, its
/// claim released — with nothing given up.
fn fail_one_issue_through_an_outage(home: &Path) {
    let fake = FakeGitlab::start(ME_ID, ME);
    fake.issue(PROJECT, ISSUE, TITLE, BODY, LABELS, &[]);
    write_config(home, &failing_service(home, fake.base_url()));
    let (code, report) = validate(home);
    assert_eq!(code, Some(0), "the service validates:\n{report}");
    assert!(!report.contains("warning:"), "cleanly:\n{report}");

    let daemon = spawn_headless_streaming(home, &[]);
    wait_for(home, &fake, &daemon, "the issue is claimed", || {
        let issue = fake.issue_state(PROJECT, ISSUE);
        marked(&fake, ISSUE)
            && issue.labels.iter().any(|l| l == "afkd::claimed")
            && issue.assignees == [ME]
    });

    let outage_start = Instant::now();
    fake.outage();
    std::fs::write(home.join("release"), "").expect("release the run");
    // `on_fail`'s last action, queued behind everything before it: the marker's delete and
    // the slot's earlier writes. Seeing it means the checked slot ran to its end.
    let queued_last = "removing the label \"afkd::claimed\" is queued behind them";
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
    let noted = || {
        fake.notes(PROJECT, ISSUE).into_iter().any(|n| {
            n.author == ME
                && n.body.starts_with("attempts spent: ")
                && n.body.contains(OUTAGE_FAULT)
                && !n.body.contains("#{")
        })
    };
    wait_until(
        "the issue reaches its failed state",
        outage_for + Duration::from_secs(5) + BUDGET,
        || {
            let issue = fake.issue_state(PROJECT, ISSUE);
            !issue
                .labels
                .iter()
                .any(|l| l == "afkd::claimed" || l == "afkd::ready")
                && issue.labels.iter().any(|l| l == NEEDS_HUMAN)
                && issue.assignees.is_empty()
                && !marked(&fake, ISSUE)
                && noted()
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
fn install_leg_places_the_plugin_and_runs_one_issue_and_one_mr_through_it() {
    let home = TempDir::new().expect("tempdir");
    let stage_dir = TempDir::new().expect("tempdir");
    let staged = stage(stage_dir.path());
    install(home.path(), &staged);
    let exec = plugins_root(home.path())
        .join(NAME)
        .join("target/release/afkd-gitlab");
    let mode = std::fs::metadata(&exec)
        .unwrap_or_else(|e| panic!("the install built {}: {e}", exec.display()))
        .permissions()
        .mode();
    assert!(mode & 0o111 != 0, "{} is executable", exec.display());
    drive_one_issue(home.path());
    drive_one_mr(home.path());
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
import "@afkd/gitlab"

BOT :: "autocoder"

issues :: service(gitlab) {
  base_url        "https://gitlab.example.com"
  project         "acme/sub.group/widgets"
  token           "REPLACE_ME"
  source_label    "afkd::ready"
  follow_comments 30s~90s
  max_attempts    2
  poll_interval   1m~3m

  on_claim(issue: gitlab.Issue) {
    _ = gitlab.assign_me(issue)
    _ = gitlab.label_add(issue, "afkd::working ⚙")
  }
  on_done(run: core.Run, issue: gitlab.Issue, outcome: core.Outcome) {
    _ = gitlab.label_remove(issue, "afkd::working ⚙")
    _ = gitlab.comment(issue, "done by #{BOT} in #{outcome.duration}:\n\n- run #{run.id}\n- 完了 ✅")
    _ = gitlab.close(issue)
  }
  on_fail(run: core.Run, issue: gitlab.Issue, outcome: core.Outcome) {
    _ = gitlab.label_remove(issue, "afkd::working ⚙")
    _ = gitlab.unassign(issue)
  }

  work_dir "/srv/acme/widgets"
  on_run(run: core.Run, issue: gitlab.Issue) {
    $ cat $AFKD_SCRATCH_DIR/task.md
  }
}

reviews :: service(gitlab.mr) {
  project       "4242"
  token         "REPLACE_ME"
  author_me     true
  poll_interval 2m

  on_claim(mr: gitlab.Merge_Request) {
    _ = gitlab.mr_assign_me(mr)
    _ = gitlab.mr_label_add(mr, "afkd::reviewing 👀")
  }
  on_done(run: core.Run, mr: gitlab.Merge_Request, outcome: core.Outcome) {
    _ = gitlab.mr_label_remove(mr, "afkd::reviewing 👀")
    _ = gitlab.mr_comment(mr, "round answered by #{BOT} in #{outcome.duration}")
  }
  on_fail(run: core.Run, mr: gitlab.Merge_Request, outcome: core.Outcome) {
    _ = gitlab.mr_unassign(mr)
    _ = gitlab.mr_close(mr)
  }

  work_dir "/srv/acme/widgets"
  on_run(run: core.Run, mr: gitlab.Merge_Request) {
    $ cat $AFKD_SCRATCH_DIR/task.md
  }
}
"##;

/// Both kinds validate as v2 services against the installed plugin: every setting either
/// writes is one its kind declares, of its type, every action its slots call is one the
/// plugin provides for that kind, with the item and the parameters it declares, and the bot
/// is named by the config's own `BOT`.
#[test]
fn every_kind_validates_as_a_v2_service() {
    let home = TempDir::new().expect("tempdir");
    let stage_dir = TempDir::new().expect("tempdir");
    install(home.path(), &stage(stage_dir.path()));
    let verbs = [
        "assign_me",
        "unassign",
        "label_add",
        "label_remove",
        "close",
        "comment",
    ];
    for verb in verbs {
        for call in [
            format!("gitlab.{verb}(issue"),
            format!("gitlab.mr_{verb}(mr"),
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
}

/// An action acts on the item it is passed, typed by kind, so an issue action passed the
/// merge request a `service(gitlab.mr)` slot is given — `gitlab.comment(mr, "…")` — is a
/// load error naming the handle types, not a call on whatever item it names.
#[test]
fn a_call_with_the_wrong_handle_is_a_load_error() {
    let home = TempDir::new().expect("tempdir");
    let stage_dir = TempDir::new().expect("tempdir");
    install(home.path(), &stage(stage_dir.path()));
    let config = |call: &str| {
        format!(
            r#"import "core"
import "@afkd/gitlab"

reviews :: service(gitlab.mr) {{
  project "group/widgets"
  token   "REPLACE_ME"

  on_done(run: core.Run, mr: gitlab.Merge_Request, outcome: core.Outcome) {{ {call} }}

  work_dir "/srv/acme/widgets"
  on_run(run: core.Run, mr: gitlab.Merge_Request) {{
    $ true
  }}
}}
"#
        )
    };
    write_config(
        home.path(),
        &config(r#"gitlab.comment(mr, "round answered — 完了 ✅")"#),
    );
    let (code, report) = validate(home.path());
    assert_eq!(
        code,
        Some(1),
        "an issue action on an mr is refused:\n{report}"
    );
    assert!(
        report.contains("gitlab.Issue") && report.contains("gitlab.Merge_Request"),
        "the refusal names both handle types:\n{report}"
    );
    // The same file calling the mr kind's own action is valid: the handle is all that was
    // wrong.
    write_config(
        home.path(),
        &config(r#"gitlab.mr_comment(mr, "round answered — 完了 ✅")"#),
    );
    let (code, report) = validate(home.path());
    assert_eq!(code, Some(0), "the mr action validates:\n{report}");
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
