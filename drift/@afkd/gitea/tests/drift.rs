//! The gate that holds the **shipped `@afkd/gitea` plugin** — a manifest v2 provider whose
//! kinds are `service(gitea)` and `service(gitea.pr)`, built from source at install time —
//! to a real afkd: the `afkd` first on `PATH`, the installed one and never a build, since a
//! plugin is checked against the afkd it will meet ([`bin_path`]).
//!
//! - The **install** leg installs the tree the release tarball holds and runs one issue
//!   through it on a real daemon, its slots' actions crossing as `call`s on the issue each
//!   is passed.
//! - The **both kinds** leg `afkd validate`s one file with a service of each kind, whose
//!   slots call every action the plugin provides on the item each is passed and read its
//!   value `me`, and the **handle** leg holds `afkd validate` to refusing an action passed
//!   the other kind's item.
//! - The **README** leg `afkd validate`s every `conf` fence the plugin's README carries,
//!   each a whole entry file, against the installed plugin.
//!
//! The Gitea is the plugin's own loopback fake ([`fake`]), so no leg touches a network, and
//! every wait is bounded and every spawn held in a [`Daemon`]: a claim that never lands must
//! **fail**, not hang.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::fake::{FakeGitea, TOKEN};
use common::*;
use tempfile::TempDir;

// --- the shipped tree --------------------------------------------------------------

/// The plugin's name, as the manifest spells it and afkd places it.
const NAME: &str = "@afkd/gitea";

/// The plugin's root — the directory `afkd install` takes, whose path this crate's own
/// mirrors under `drift/`. Canonicalized, so a staged copy and a report both name one path.
fn plugin_root() -> PathBuf {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../@afkd/gitea");
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
    let staged = dst.join("gitea");
    copy_tree(&plugin_root(), &staged, &["target"]);
    for present in ["afkd-plugin.toml", "Cargo.toml", "Cargo.lock", "src"] {
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

/// The token's own login, and so the author of every comment the plugin posts. Wide,
/// accented and bracketed, as the wire suite's is.
const ME: &str = "björn-öst[bot]";

const REPO: &str = "acme/widgets";

/// The issue the service claims, titled and described in the shapes that break a naive
/// transport: wide glyphs, an emoji, and a multi-line body with quoting and a fence.
const ISSUE: u64 = 7;
const TITLE: &str = "修复 the retry storm 🚨";
const BODY: &str = "Retries pile up after a 502.\n\n> \"backoff\" — nobody\n\n```\nretry: 0\n```\n";

/// How long a claim, a run or a finish may take. Generous, because it bounds a failure
/// rather than timing a success: the service polls every second.
const BUDGET: Duration = Duration::from_secs(30);

/// The service that drives [`ISSUE`], `home` its work dir: a v2 file importing the plugin,
/// whose slots call its actions on the issue they are passed and whose `on_done` comment
/// afkd interpolates — the outcome's duration and the plugin's value `me` both. The run
/// holds until the test creates `release` — bounded, so a test that never does cannot wedge
/// it — which is what lets the test see the claim and the run mid-flight.
fn service(home: &Path, base_url: &str) -> String {
    format!(
        r#"import "@afkd/gitea"

widgets :: service(gitea) {{
  base_url      "{base_url}"
  repo          "{REPO}"
  token         "{TOKEN}"
  source_label  "afkd/ready"
  poll_interval 1s
  max_attempts  1

  on_claim(run: afkd.Run, issue: gitea.Issue) {{ gitea.assign_me(issue) }}
  on_done(run: afkd.Run, issue: gitea.Issue, outcome: afkd.Outcome) {{
    gitea.label_remove(issue, "afkd/claimed")
    gitea.comment(issue, "done in #{{outcome.duration}} by #{{gitea.me}}")
    gitea.close(issue)
  }}

  work_dir "{home}"
  on_run(run: afkd.Run, issue: gitea.Issue) {{
    $ i=0; until [ -f release ] || [ $i -ge 600 ]; do sleep 0.1; i=$((i+1)); done
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

/// The credential a config reads from the daemon's environment (`env.GITEA_TOKEN`), as
/// the README's lead example does.
const CONFIG_ENV: &[(&str, &str)] = &[("GITEA_TOKEN", TOKEN)];

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

/// Everything a failed wait needs to say: the issue as the fake holds it, its thread, the
/// run dirs, the daemon's own log, and its stderr so far — where the plugin's own
/// complaints land.
fn dump(home: &Path, fake: &FakeGitea, daemon: &StreamingDaemon) -> String {
    let comments: Vec<String> = fake
        .comments(REPO, ISSUE)
        .iter()
        .map(|c| format!("  {}: {:?}", c.author, c.body))
        .collect();
    format!(
        "issue: {:?}\ncomments:\n{}\nrun dirs: {:?}\ndaemon.log:\n{}\ndaemon stderr:\n{}",
        fake.issue_state(REPO, ISSUE),
        comments.join("\n"),
        run_dirs(home),
        std::fs::read_to_string(daemon_log(home)).unwrap_or_default(),
        daemon.stderr_so_far()
    )
}

/// Poll `ready` to [`BUDGET`], failing with `what` and the [`dump`] when it never holds.
fn wait_for(
    home: &Path,
    fake: &FakeGitea,
    daemon: &StreamingDaemon,
    what: &str,
    ready: impl Fn() -> bool,
) {
    let deadline = Instant::now() + BUDGET;
    while !ready() {
        assert!(
            Instant::now() < deadline,
            "{what} within {BUDGET:?}\n{}",
            dump(home, fake, daemon)
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Whether `fake`'s thread on [`ISSUE`] holds a claim marker.
fn marked(fake: &FakeGitea) -> bool {
    fake.comments(REPO, ISSUE)
        .iter()
        .any(|c| c.author == ME && c.body.starts_with("[afkd-claim]"))
}

/// Read `path` whole, naming it when it is missing.
fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Drive one issue through the plugin, installed in `home`, on a real daemon: it is
/// **claimed** (the marker and `afkd/claimed`, and `on_claim`'s call assigned the token's
/// own user), **run** (its run dir is named for the issue, and its task carries the issue
/// whole) and **finished** (the marker deleted, and `on_done`'s calls took the gate off,
/// commented and closed it), and the daemon then drains clean.
fn drive_one_issue(home: &Path) {
    let fake = FakeGitea::start(ME);
    fake.issue(REPO, ISSUE, TITLE, BODY, &["afkd/ready"], &[]);
    write_config(home, &service(home, fake.base_url()));
    let (code, report) = validate(home);
    assert_eq!(code, Some(0), "the service validates:\n{report}");

    let daemon = spawn_headless_streaming(home, &[]);

    wait_for(home, &fake, &daemon, "the issue is claimed", || {
        let issue = fake.issue_state(REPO, ISSUE);
        marked(&fake) && issue.labels.iter().any(|l| l == "afkd/claimed") && issue.assignees == [ME]
    });
    // The run dir is made before the spine lays the unit's files into it, so the wait is
    // for a written brief, not the bare dir.
    let run_suffix = format!("-unit-{ISSUE}-1");
    let brief = || {
        run_dirs(home)
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
        fake.issue_state(REPO, ISSUE).state == "closed" && !marked(&fake)
    });
    let issue = fake.issue_state(REPO, ISSUE);
    assert!(
        !issue.labels.iter().any(|l| l == "afkd/claimed"),
        "on_done took afkd/claimed off: {issue:?}"
    );
    assert!(
        fake.comments(REPO, ISSUE).iter().any(|c| c.author == ME
            && c.body.starts_with("done in ")
            && c.body.ends_with(&format!(" by {ME}"))
            && !c.body.contains("#{")),
        "on_done's comment landed with its run fact and `gitea.me` filled in:\n{}",
        dump(home, &fake, &daemon)
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
fn install_leg_places_the_plugin_and_runs_one_issue_through_it() {
    let home = TempDir::new().expect("tempdir");
    let stage_dir = TempDir::new().expect("tempdir");
    let staged = stage(stage_dir.path());
    install(home.path(), &staged);
    let exec = plugins_root(home.path())
        .join(NAME)
        .join("target/release/afkd-gitea");
    let mode = std::fs::metadata(&exec)
        .unwrap_or_else(|e| panic!("the install built {}: {e}", exec.display()))
        .permissions()
        .mode();
    assert!(mode & 0o111 != 0, "{} is executable", exec.display());
    drive_one_issue(home.path());
}

/// A service of each kind, whose slots between them call every action the plugin provides —
/// each kind's own six, on the item its slot is passed — and read its value `me`, the run
/// and the outcome — the issue kind's `on_park` among them — in the argument shapes the
/// manifest types: wide and slashed label names, and a multi-line comment.
const BOTH_KINDS: &str = r##"import "@afkd/gitea"

issues :: service(gitea) {
  base_url        "https://gitea.example.com"
  org             "acme"
  token           "REPLACE_ME"
  source_label    "afkd/ready"
  discuss_with    [ "alice", "陳大文" ]
  follow_comments 30s to 90s
  max_attempts    2
  poll_interval   1m to 3m

  on_claim(run: afkd.Run, issue: gitea.Issue) {
    gitea.assign_me(issue)
    gitea.label_add(issue, "afkd/working ⚙")
  }
  on_done(run: afkd.Run, issue: gitea.Issue, outcome: afkd.Outcome) {
    gitea.label_remove(issue, "afkd/working ⚙")
    gitea.comment(issue, "done by #{gitea.me} in #{outcome.duration}:\n\n- run #{run.id}\n- 完了 ✅")
    gitea.close(issue)
  }
  on_fail(run: afkd.Run, issue: gitea.Issue, outcome: afkd.Outcome) {
    gitea.label_remove(issue, "afkd/working ⚙")
    gitea.unassign(issue)
  }
  on_park(run: afkd.Run, issue: gitea.Issue, outcome: afkd.Outcome) { gitea.label_add(issue, "needs/human") }

  work_dir "/srv/acme/widgets"
  on_run(run: afkd.Run, issue: gitea.Issue) {
    $ cat $AFKD_SCRATCH_DIR/task.md
  }
}

reviews :: service(gitea.pr) {
  base_url      "https://gitea.example.com"
  repo          "acme/widgets"
  token         "REPLACE_ME"
  author_me     true
  poll_interval 2m

  on_claim(run: afkd.Run, pr: gitea.Pull_Request) {
    gitea.pr_assign_me(pr)
    gitea.pr_label_add(pr, "afkd/reviewing 👀")
  }
  on_done(run: afkd.Run, pr: gitea.Pull_Request, outcome: afkd.Outcome) {
    gitea.pr_label_remove(pr, "afkd/reviewing 👀")
    gitea.pr_comment(pr, "round answered by #{gitea.me} in #{outcome.duration}")
  }
  on_fail(run: afkd.Run, pr: gitea.Pull_Request, outcome: afkd.Outcome) {
    gitea.pr_unassign(pr)
    gitea.pr_close(pr)
  }

  work_dir "/srv/acme/widgets"
  on_run(run: afkd.Run, pr: gitea.Pull_Request) {
    $ cat $AFKD_SCRATCH_DIR/task.md
  }
}
"##;

/// Both kinds validate as v2 services against the installed plugin: every setting either
/// writes is one its kind declares, of its type, every action its slots call is one the
/// plugin provides for that kind, with the item and the parameters it declares, and
/// `gitea.me` is a value it has.
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
        for call in [format!("gitea.{verb}(issue"), format!("gitea.pr_{verb}(pr")] {
            assert!(BOTH_KINDS.contains(&call), "the file calls {call}");
        }
    }
    for used in ["gitea.me", "on_park", "outcome.duration", "run.id"] {
        assert!(BOTH_KINDS.contains(used), "the file uses {used}");
    }
    write_config(home.path(), BOTH_KINDS);
    let (code, report) = validate(home.path());
    assert_eq!(code, Some(0), "both kinds validate:\n{report}");
    assert!(!report.contains("warning:"), "cleanly:\n{report}");
}

/// An action acts on the item it is passed, typed by kind, so an issue action passed the
/// pull request a `service(gitea.pr)` slot is given — `gitea.comment(pr, "…")` — is a load
/// error naming the handle types, not a call on whatever item it names.
#[test]
fn a_call_with_the_wrong_handle_is_a_load_error() {
    let home = TempDir::new().expect("tempdir");
    let stage_dir = TempDir::new().expect("tempdir");
    install(home.path(), &stage(stage_dir.path()));
    let config = |call: &str| {
        format!(
            r#"import "@afkd/gitea"

reviews :: service(gitea.pr) {{
  repo  "acme/widgets"
  token "REPLACE_ME"

  on_done(run: afkd.Run, pr: gitea.Pull_Request, outcome: afkd.Outcome) {{ {call} }}

  work_dir "/srv/acme/widgets"
  on_run(run: afkd.Run, pr: gitea.Pull_Request) {{
    $ true
  }}
}}
"#
        )
    };
    write_config(
        home.path(),
        &config(r#"gitea.comment(pr, "round answered — 完了 ✅")"#),
    );
    let (code, report) = validate(home.path());
    assert_eq!(
        code,
        Some(1),
        "an issue action on a pr is refused:\n{report}"
    );
    assert!(
        report.contains("gitea.Issue") && report.contains("gitea.Pull_Request"),
        "the refusal names both handle types:\n{report}"
    );
    // The same file calling the pr kind's own action is valid: the handle is all that was
    // wrong.
    write_config(
        home.path(),
        &config(r#"gitea.pr_comment(pr, "round answered — 完了 ✅")"#),
    );
    let (code, report) = validate(home.path());
    assert_eq!(code, Some(0), "the pr action validates:\n{report}");
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
