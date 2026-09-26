//! The gate that holds the **shipped `@afkd/gitlab` plugin** — the `gitlab` and
//! `gitlab_mr_review` trigger kinds, built from source at install time — to a real afkd: the
//! `afkd` first on `PATH`, the installed one and never a build, since a plugin is checked
//! against the afkd it will meet ([`bin_path`]). The leg that reads afkd's own source as well
//! wants an afkd checkout named by `AFKD_SRC` ([`afkd_src`]), and skips loudly without one.
//!
//! afkd still has both kinds built in, and a plugin may never shadow a built-in, so today
//! `afkd install` refuses this plugin. That refusal shapes the gate:
//!
//! - The **install** leg installs the tree the release tarball holds. While the built-in is
//!   there it expects exactly that refusal, says it is skipping, and passes; once afkd drops
//!   the built-in it builds, places and runs the plugin through one issue.
//! - The **live-today** leg runs the same issue *now*, by installing a copy whose manifest
//!   renames both kinds (`gitlab_probe`, `gitlab_mr_review_probe`) and whose `exec` is a
//!   wrapper that renames `hello`'s kind back — so the unmodified plugin binary meets
//!   afkd's real spine (journal, cadence, watch, framing) before the rip-out, not after.
//! - The **README** leg `afkd validate`s every `conf` fence the plugin's README carries:
//!   against the built-in's vocabulary today, against the manifest's once the install goes
//!   live.
//! - The **transcription** leg holds the manifest's vocabulary, table for table, to the
//!   built-in's in afkd's source and to the plugin's own ported copies.
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

/// What `afkd install` made of a staged tree.
enum Install {
    /// Built and placed under the plugins root.
    Placed,
    /// Refused because afkd still provides a kind the manifest declares; the report.
    BuiltIn(String),
}

/// `afkd install <root>` into `home`. A refusal is recognised by its exact sentence, and
/// anything else that is not a success fails the leg with the full report.
fn install(home: &Path, root: &Path) -> Install {
    let out = run_subcommand_args(home, &["install", &root.display().to_string()]);
    let report = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    if out.status.success() {
        return Install::Placed;
    }
    let refused = ["gitlab", "gitlab_mr_review"].iter().any(|kind| {
        report.contains(&format!(
            "declares trigger kind `{kind}`, which is built in"
        ))
    });
    assert!(
        out.status.code() == Some(1) && refused,
        "`afkd install {}` failed, and not because a kind is built in ({:?}):\n{report}",
        root.display(),
        out.status.code()
    );
    assert!(
        !plugins_root(home).join(NAME).exists(),
        "a refused install placed nothing:\n{report}"
    );
    Install::BuiltIn(report.trim().to_string())
}

/// The wrapper the probe manifest's `exec` names. afkd greets the plugin with a `hello`
/// naming the kind it asks for, which here is a probe kind; the wrapper renames that one
/// line back to the kind the binary serves, then passes every later line through untouched.
/// The MR rule runs first, so `gitlab_probe` can never match inside `gitlab_mr_review_probe`.
const PROBE_EXEC: &str = r#"#!/bin/sh
# afkd's hello names a probe kind; the plugin binary knows only the real ones. Rename the
# kind on that first line, then hand the binary the rest of the stream as it arrives.
here=$(dirname "$0")
{
  IFS= read -r hello
  printf '%s\n' "$hello" | sed -e 's/"kind":"gitlab_mr_review_probe"/"kind":"gitlab_mr_review"/' \
                               -e 's/"kind":"gitlab_probe"/"kind":"gitlab"/'
  cat
} | "$here/target/release/afkd-gitlab"
"#;

/// Turn a staged copy into the probe: both kinds renamed so afkd has no built-in to refuse
/// it over, and `exec` pointed at [`PROBE_EXEC`]. Each rewrite must hit exactly one line, so
/// a reshaped manifest reddens here rather than probing nothing.
fn probe_manifest(staged: &Path) {
    let manifest = staged.join("afkd-plugin.toml");
    let text = std::fs::read_to_string(&manifest).expect("the staged manifest");
    let rewrites = [
        (r#"kind = "gitlab""#, r#"kind = "gitlab_probe""#),
        (
            r#"kind = "gitlab_mr_review""#,
            r#"kind = "gitlab_mr_review_probe""#,
        ),
        (
            r#"exec = "target/release/afkd-gitlab""#,
            r#"exec = "probe-exec""#,
        ),
    ];
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    for (from, to) in rewrites {
        let hits: Vec<&mut String> = lines.iter_mut().filter(|line| *line == from).collect();
        assert_eq!(hits.len(), 1, "the manifest has exactly one `{from}` line");
        for line in hits {
            *line = to.to_string();
        }
    }
    std::fs::write(&manifest, lines.join("\n") + "\n").expect("write the probe manifest");
    let exec = staged.join("probe-exec");
    std::fs::write(&exec, PROBE_EXEC).expect("write the probe exec");
    std::fs::set_permissions(&exec, std::fs::Permissions::from_mode(0o755)).expect("chmod");
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

/// How long a claim, a run or a finish may take. Generous, because it bounds a failure
/// rather than timing a success: the service polls every second.
const BUDGET: Duration = Duration::from_secs(30);

/// The service that drives [`ISSUE`] through a trigger of `kind`, `home` its work dir. The
/// run step holds until the test creates `release` — bounded, so a test that never does
/// cannot wedge it — which is what lets the test see the claim and the run mid-flight.
fn service(home: &Path, kind: &str, base_url: &str) -> String {
    format!(
        r#"service widgets {{
  work_dir "{home}"
  trigger {kind} {{
    base_url      "{base_url}"
    project       "{PROJECT}"
    token         "{TOKEN}"
    source_label  "afkd::ready"
    poll_interval 1s
    max_attempts  1
    on_claim {{ label_add "afkd::working" }}
    on_done {{
      label_remove "afkd::working"
      comment "done in @{{run:duration}}"
      close
    }}
  }}
  run {{
    run_cmd """i=0; until [ -f release ] || [ $i -ge 600 ]; do sleep 0.1; i=$((i+1)); done"""
    run_cmd "cat $AFKD_SCRATCH_DIR/task.md"
  }}
}}
"#,
        home = home.display()
    )
}

/// Write `config` as `home`'s main config.
fn write_config(home: &Path, config: &str) {
    let conf = main_conf(home);
    std::fs::create_dir_all(conf.parent().expect("has parent")).expect("mk the config dir");
    std::fs::write(&conf, config).expect("write the config");
}

/// `afkd validate` over `home`'s main config: the exit code and the report.
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

/// Everything a failed wait needs to say: the issue as the fake holds it, its notes, the
/// run dirs and the daemon's own log.
fn dump(home: &Path, fake: &FakeGitlab) -> String {
    let notes: Vec<String> = fake
        .notes(PROJECT, ISSUE)
        .iter()
        .map(|n| format!("  {}: {:?}", n.author, n.body))
        .collect();
    format!(
        "issue: {:?}\nnotes:\n{}\nrun dirs: {:?}\ndaemon.log:\n{}",
        fake.issue_state(PROJECT, ISSUE),
        notes.join("\n"),
        run_dirs(home),
        std::fs::read_to_string(daemon_log(home)).unwrap_or_default()
    )
}

/// Poll `ready` to [`BUDGET`], failing with `what` and the [`dump`] when it never holds.
fn wait_for(home: &Path, fake: &FakeGitlab, what: &str, ready: impl Fn() -> bool) {
    let deadline = Instant::now() + BUDGET;
    while !ready() {
        assert!(
            Instant::now() < deadline,
            "{what} within {BUDGET:?}\n{}",
            dump(home, fake)
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Whether `fake`'s notes on [`ISSUE`] hold a claim marker.
fn marked(fake: &FakeGitlab) -> bool {
    fake.notes(PROJECT, ISSUE)
        .iter()
        .any(|n| n.author == ME && n.body.starts_with("[afkd-claim]"))
}

/// Drive one issue through the plugin, installed in `home` and serving `kind`, on a real
/// daemon: it is **claimed** (the marker, the plugin's own `afkd::claimed`, and
/// `on_claim`'s label), **run** (its run dir is named for the issue, and its task carries the
/// issue whole) and **finished** (`on_done` applied, the marker deleted), and the daemon
/// then drains clean.
fn drive_one_issue(home: &Path, kind: &str) {
    let fake = FakeGitlab::start(ME_ID, ME);
    fake.issue(PROJECT, ISSUE, TITLE, BODY, &["afkd::ready"], &[]);
    write_config(home, &service(home, kind, fake.base_url()));
    let (code, report) = validate(home);
    assert_eq!(code, Some(0), "the service validates:\n{report}");

    let daemon = spawn_headless_streaming(home, &[]);

    wait_for(home, &fake, "the issue is claimed", || {
        let labels = fake.issue_state(PROJECT, ISSUE).labels;
        marked(&fake)
            && ["afkd::claimed", "afkd::working"]
                .iter()
                .all(|want| labels.iter().any(|l| l == want))
    });
    let run_suffix = format!("-issue-{ISSUE}-1");
    wait_for(home, &fake, "the issue's run starts", || {
        run_dirs(home)
            .iter()
            .any(|name| name.ends_with(&run_suffix))
    });
    let run = run_dirs(home)
        .into_iter()
        .find(|name| name.ends_with(&run_suffix))
        .expect("waited for");
    let task = std::fs::read_to_string(runs_root(home).join("widgets").join(&run).join("task.md"))
        .unwrap_or_else(|e| panic!("the run {run} has a task.md: {e}"));
    for part in [TITLE, "\"backoff\" — nobody", "retry: 0"] {
        assert!(task.contains(part), "task.md carries {part:?}:\n{task}");
    }

    std::fs::write(home.join("release"), "").expect("release the run");
    wait_for(home, &fake, "the issue is finished", || {
        fake.issue_state(PROJECT, ISSUE).state == "closed" && !marked(&fake)
    });
    let issue = fake.issue_state(PROJECT, ISSUE);
    assert!(
        !issue.labels.iter().any(|l| l == "afkd::working"),
        "on_done took afkd::working off: {issue:?}"
    );
    let notes = fake.notes(PROJECT, ISSUE);
    assert!(
        notes
            .iter()
            .any(|n| n.author == ME && n.body.starts_with("done in ") && !n.body.contains("@{")),
        "on_done's comment landed with its run reference filled in:\n{}",
        dump(home, &fake)
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
fn install_leg_is_refused_while_the_kind_is_built_in_and_runs_once_it_is_not() {
    let home = TempDir::new().expect("tempdir");
    let stage_dir = TempDir::new().expect("tempdir");
    let staged = stage(stage_dir.path());
    match install(home.path(), &staged) {
        Install::BuiltIn(report) => eprintln!(
            "skipping: the afkd on PATH still has `gitlab` built in, so it refuses {NAME} \
             ({report}); this leg goes live once afkd drops the built-in"
        ),
        Install::Placed => {
            let exec = plugins_root(home.path())
                .join(NAME)
                .join("target/release/afkd-gitlab");
            let mode = std::fs::metadata(&exec)
                .unwrap_or_else(|e| panic!("the install built {}: {e}", exec.display()))
                .permissions()
                .mode();
            assert!(mode & 0o111 != 0, "{} is executable", exec.display());
            drive_one_issue(home.path(), "gitlab");
        }
    }
}

#[test]
fn the_plugin_binary_runs_on_the_real_spine_under_probe_kinds() {
    let home = TempDir::new().expect("tempdir");
    let stage_dir = TempDir::new().expect("tempdir");
    let staged = stage(stage_dir.path());
    probe_manifest(&staged);
    if let Install::BuiltIn(report) = install(home.path(), &staged) {
        panic!("the probe kinds are never built in, yet afkd refused them:\n{report}");
    }
    drive_one_issue(home.path(), "gitlab_probe");
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
    match install(home.path(), &stage(stage_dir.path())) {
        Install::BuiltIn(_) => eprintln!(
            "the afkd on PATH has `gitlab` built in: the fences validate against its vocabulary"
        ),
        Install::Placed => eprintln!("{NAME} installed: the fences validate against its manifest"),
    }
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

// --- the transcription -----------------------------------------------------------------

/// The quoted strings of the `const <name>: &[&str] = &[…]` declared in `source` (read
/// from `file`), in order. A missing declaration panics naming both, so a rename reddens
/// rather than comparing against nothing.
fn str_list(source: &str, file: &str, name: &str) -> Vec<String> {
    let head = format!("const {name}: &[&str] = &[");
    let mut found = source.match_indices(&head);
    let (at, _) = found
        .next()
        .unwrap_or_else(|| panic!("{file} declares no `{head}…]`"));
    assert!(found.next().is_none(), "{file} declares `{name}` once");
    let body = &source[at + head.len()..];
    let body = &body[..body.find(']').expect("the list closes")];
    quoted(body)
}

/// The value of the `const <name>: &str = "…";` declared in `source` (read from `file`).
fn str_value(source: &str, file: &str, name: &str) -> String {
    let head = format!("const {name}: &str = ");
    let at = source
        .find(&head)
        .unwrap_or_else(|| panic!("{file} declares no `{head}…`"));
    let rest = &source[at + head.len()..];
    quoted(&rest[..rest.find(';').expect("the const ends")])
        .pop()
        .unwrap_or_else(|| panic!("{file}'s `{name}` is a string literal"))
}

/// Every `"…"` in `text`, in order. The tables hold plain key names, so no escape occurs.
fn quoted(text: &str) -> Vec<String> {
    text.split('"')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect()
}

/// The keys afkd's settings demand: every `require_present("…")` in its code, comments
/// aside.
fn required_keys(source: &str, file: &str) -> Vec<String> {
    let mut keys: Vec<String> = source
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .flat_map(|line| {
            line.match_indices("require_present(\"")
                .map(move |(at, head)| {
                    let rest = &line[at + head.len()..];
                    rest[..rest.find('"').expect("the key closes")].to_string()
                })
        })
        .collect();
    keys.dedup();
    assert!(!keys.is_empty(), "{file} requires no key");
    keys
}

/// A manifest array of strings, read off `table[key]`.
fn strings(table: &toml::Table, key: &str, whose: &str) -> Vec<String> {
    table
        .get(key)
        .and_then(toml::Value::as_array)
        .unwrap_or_else(|| panic!("{whose} declares `{key}` as an array"))
        .iter()
        .map(|v| {
            v.as_str()
                .unwrap_or_else(|| panic!("{whose}'s `{key}` holds strings"))
                .to_string()
        })
        .collect()
}

/// Read `path` whole, naming it when it is missing.
fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn the_manifest_transcribes_the_in_tree_tables() {
    let Some(src) = afkd_src() else {
        eprintln!("skipping: AFKD_SRC names no afkd checkout, and the in-tree tables are afkd's");
        return;
    };
    let afkd_settings = "crates/gitlab/src/settings.rs";
    let afkd_lifecycle = "crates/forge/src/lifecycle.rs";
    let afkd_lib = "crates/gitlab/src/lib.rs";
    let (settings, lifecycle, lib) = (
        read(&src.join(afkd_settings)),
        read(&src.join(afkd_lifecycle)),
        read(&src.join(afkd_lib)),
    );
    let ported_settings = "@afkd/gitlab/src/settings.rs";
    let ported_lifecycle = "@afkd/gitlab/src/lifecycle.rs";
    let ported = read(&plugin_root().join("src/settings.rs"));
    let ported_actions = read(&plugin_root().join("src/lifecycle.rs"));

    let manifest: toml::Table = toml::from_str(&read(&plugin_root().join("afkd-plugin.toml")))
        .expect("the manifest parses");
    let triggers = manifest
        .get("trigger")
        .and_then(toml::Value::as_array)
        .expect("the manifest declares triggers");
    assert_eq!(triggers.len(), 2, "the manifest declares the two kinds");

    let required = required_keys(&settings, afkd_settings);
    let actions = str_list(&lifecycle, afkd_lifecycle, "LIFECYCLE_KEYS");
    assert_eq!(
        actions,
        str_list(&ported_actions, ported_lifecycle, "LIFECYCLE_KEYS"),
        "the plugin's LIFECYCLE_KEYS is afkd's"
    );
    let durations = str_list(&settings, afkd_settings, "DURATION_KEYS");
    assert_eq!(
        durations,
        str_list(&ported, ported_settings, "DURATION_KEYS"),
        "the plugin's DURATION_KEYS is afkd's"
    );

    // (index, kind const, display const, the tables' suffix)
    let kinds = [
        (
            0,
            "ISSUE_TRIGGER_KIND",
            "ISSUE_TRIGGER_DISPLAY_NAME",
            "ISSUE",
        ),
        (
            1,
            "MR_REVIEW_TRIGGER_KIND",
            "MR_REVIEW_TRIGGER_DISPLAY_NAME",
            "MR",
        ),
    ];
    for (index, kind_const, display_const, suffix) in kinds {
        let trigger = triggers[index].as_table().expect("a trigger is a table");
        let kind = str_value(&lib, afkd_lib, kind_const);
        let whose = format!("the manifest's `{kind}`");
        assert_eq!(
            trigger.get("kind").and_then(toml::Value::as_str),
            Some(kind.as_str()),
            "trigger {index} is afkd's {kind_const}"
        );
        assert_eq!(
            trigger.get("display_name").and_then(toml::Value::as_str),
            Some(str_value(&lib, afkd_lib, display_const).as_str()),
            "{whose} display_name is afkd's {display_const}"
        );

        let allowed = format!("ALLOWED_{suffix}_KEYS");
        let repeatable = format!("REPEATABLE_{suffix}_KEYS");
        let required_const = format!("REQUIRED_{suffix}_KEYS");
        let settings_keys = str_list(&settings, afkd_settings, &allowed);
        let tables = [
            ("settings", settings_keys.clone(), &allowed),
            (
                "repeatable",
                str_list(&settings, afkd_settings, &repeatable),
                &repeatable,
            ),
        ];
        for (key, in_tree, name) in tables {
            let declared = strings(trigger, key, &whose);
            assert_eq!(declared, in_tree, "{whose} `{key}` is afkd's {name}");
            assert_eq!(
                declared,
                str_list(&ported, ported_settings, name),
                "{whose} `{key}` is the plugin's {name}"
            );
        }
        let declared = strings(trigger, "required", &whose);
        assert_eq!(
            declared, required,
            "{whose} `required` is what afkd's settings require_present"
        );
        assert_eq!(
            declared,
            str_list(&ported, ported_settings, &required_const),
            "{whose} `required` is the plugin's {required_const}"
        );
        assert_eq!(
            strings(trigger, "durations", &whose),
            durations,
            "{whose} `durations` is afkd's DURATION_KEYS"
        );

        let blocks = trigger
            .get("block")
            .and_then(toml::Value::as_array)
            .unwrap_or_else(|| panic!("{whose} declares its blocks"));
        let paths: Vec<&str> = blocks
            .iter()
            .map(|b| {
                b.get("path")
                    .and_then(toml::Value::as_str)
                    .expect("a block path")
            })
            .collect();
        let moments: Vec<&str> = settings_keys
            .iter()
            .map(String::as_str)
            .filter(|k| k.starts_with("on_"))
            .collect();
        assert_eq!(
            paths, moments,
            "{whose} blocks are the on_* keys of {allowed}"
        );
        for block in blocks {
            let block = block.as_table().expect("a block is a table");
            let at = format!("{whose} block `{}`", block["path"].as_str().unwrap_or("?"));
            for key in ["settings", "repeatable"] {
                assert_eq!(
                    strings(block, key, &at),
                    actions,
                    "{at} `{key}` is LIFECYCLE_KEYS"
                );
            }
        }
    }
}
