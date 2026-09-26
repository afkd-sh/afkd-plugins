//! The gate that holds the **shipped `@afkd/trello` plugin** — the `trello` trigger kind,
//! built from source at install time — to a real afkd: the `afkd` first on `PATH`, the
//! installed one and never a build, since a plugin is checked against the afkd it will meet
//! ([`bin_path`]). The leg that reads afkd's own source as well wants an afkd checkout named
//! by `AFKD_SRC` ([`afkd_src`]), and skips loudly without one.
//!
//! afkd still has the kind built in, and a plugin may never shadow a built-in, so today
//! `afkd install` refuses this plugin. That refusal shapes the gate:
//!
//! - The **install** leg installs the tree the release tarball holds. While the built-in is
//!   there it expects exactly that refusal, says it is skipping, and passes; once afkd drops
//!   the built-in it builds, places and runs the plugin through one card.
//! - The **live-today** leg runs the same card *now*, by installing a copy whose manifest
//!   renames the kind (`trello_probe`) and whose `exec` is a wrapper that renames `hello`'s
//!   kind back — so the unmodified plugin binary meets afkd's real spine (journal, cadence,
//!   watch, framing) before the rip-out, not after.
//! - The **transcription** leg holds the manifest's vocabulary, table for table, to the
//!   built-in's in afkd's source and to the plugin's own ported copies.
//!
//! Two of the built-in's tables are not where a first reading looks. Trello's lifecycle
//! vocabulary is the private `LIFECYCLE_KEYS` in `crates/trello/src/settings.rs`, not the
//! forge one in `crates/forge/src/lifecycle.rs`; and its info-view keys are the trello arm of
//! `builtin_trigger_keys` in `crates/app/src/headless.rs`, since `trigger_keys` now only
//! dispatches between a plugin's `info_keys` and that table.
//!
//! The Trello is the plugin's own loopback fake ([`fake`]), so no leg touches a network, and
//! every wait is bounded and every spawn held in a [`Daemon`]: a claim that never lands must
//! **fail**, not hang.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::fake::{FakeTrello, BOARD, KEY, TOKEN};
use common::*;
use tempfile::TempDir;

// --- the shipped tree --------------------------------------------------------------

/// The plugin's name, as the manifest spells it and afkd places it.
const NAME: &str = "@afkd/trello";

/// The plugin's root — the directory `afkd install` takes, whose path this crate's own
/// mirrors under `drift/`. Canonicalized, so a staged copy and a report both name one path.
fn plugin_root() -> PathBuf {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../@afkd/trello");
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
    let staged = dst.join("trello");
    copy_tree(&plugin_root(), &staged, &["target"]);
    for present in [
        "afkd-plugin.toml",
        "Cargo.toml",
        "Cargo.lock",
        "src",
        "skills/trello/SKILL.md",
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
    /// Refused because afkd still provides the kind the manifest declares; the report.
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
    let refused = report.contains("declares trigger kind `trello`, which is built in");
    assert!(
        out.status.code() == Some(1) && refused,
        "`afkd install {}` failed, and not because the kind is built in ({:?}):\n{report}",
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
/// naming the kind it asks for, which here is the probe kind; the wrapper renames that one
/// line back to the kind the binary serves, then passes every later line through untouched.
const PROBE_EXEC: &str = r#"#!/bin/sh
# afkd's hello names a probe kind; the plugin binary knows only the real one. Rename the
# kind on that first line, then hand the binary the rest of the stream as it arrives.
here=$(dirname "$0")
{
  IFS= read -r hello
  printf '%s\n' "$hello" | sed -e 's/"kind":"trello_probe"/"kind":"trello"/'
  cat
} | "$here/target/release/afkd-trello"
"#;

/// Turn a staged copy into the probe: the kind renamed so afkd has no built-in to refuse it
/// over, and `exec` pointed at [`PROBE_EXEC`]. Each rewrite must hit exactly one line, so a
/// reshaped manifest reddens here rather than probing nothing.
fn probe_manifest(staged: &Path) {
    let manifest = staged.join("afkd-plugin.toml");
    let text = std::fs::read_to_string(&manifest).expect("the staged manifest");
    let rewrites = [
        (r#"kind = "trello""#, r#"kind = "trello_probe""#),
        (
            r#"exec = "target/release/afkd-trello""#,
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

/// The card the service claims, its short link in Trello's own shape, titled and described
/// in the shapes that break a naive transport: wide glyphs, an emoji, quotes, and a
/// multi-line body with a quoted line, a fence and a wide attribution.
const SHORT_LINK: &str = "Qb7eLy2w";
const TITLE: &str = "修复 the retry storm 🚨 — \"backoff\" resets";
const BODY: &str = "Retries pile up after a 502.\n\n> \"backoff\" — nobody\n\n\
                    ```rust\nlet backoff = Duration::ZERO;\n```\n\n— reported by 陳大文";

/// A human's question on the card before afkd ever looked, which the brief must carry.
const ASK: &str = "看起来不对 🚨 — it resets on every 401:\n\n```\nGET /1/members/me 401\n```";

/// How long a claim, a run or a finish may take. Generous, because it bounds a failure
/// rather than timing a success: the service polls every second.
const BUDGET: Duration = Duration::from_secs(30);

/// The board the service picks from: the selfdev lists, and one card up for grabs — created
/// a week ago, so no age gate holds it — with Chen's question on it. The fake and the card's
/// id.
fn seed() -> (FakeTrello, String) {
    let fake = FakeTrello::start();
    for list in ["Up for Grabs", "In Progress", "Review"] {
        fake.list(list);
    }
    let chen = fake.member("chen", "陳大文");
    let card = fake.card("Up for Grabs", SHORT_LINK, TITLE, BODY);
    fake.comment(&card, &chen, ASK, 1800);
    (fake, card)
}

/// The service that drives the seeded card through a trigger of `kind`, `home` its work dir.
/// The run step holds until the test creates `release` — bounded, so a test that never does
/// cannot wedge it — which is what lets the test see the claim and the run mid-flight.
fn service(home: &Path, kind: &str, base_url: &str) -> String {
    format!(
        r#"service widgets {{
  work_dir "{home}"
  trigger {kind} {{
    board         "https://trello.com/b/{BOARD}/afkd-drift"
    base_url      "{base_url}"
    api_key       "{KEY}"
    token         "{TOKEN}"
    pick_from     "Up for Grabs"
    poll_interval 1s
    max_attempts  1
    on_claim {{ move_to "In Progress" {{ at top }} }}
    on_done {{
      move_to "Review" {{ at top }}
      comment "done in @{{run:duration}}"
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

/// Everything a failed wait needs to say: the card's list and thread as the fake holds
/// them, the run dirs, the daemon's own log, and its stderr so far — where the plugin's
/// own complaints land.
fn dump(home: &Path, fake: &FakeTrello, card: &str, daemon: &StreamingDaemon) -> String {
    let comments: Vec<String> = fake
        .comments(card)
        .iter()
        .map(|c| format!("  {}: {:?}", c.author, c.text))
        .collect();
    format!(
        "list: {:?}\ncomments:\n{}\nrun dirs: {:?}\ndaemon.log:\n{}\ndaemon stderr:\n{}",
        fake.list_of(card),
        comments.join("\n"),
        run_dirs(home),
        std::fs::read_to_string(daemon_log(home)).unwrap_or_default(),
        daemon.stderr_so_far()
    )
}

/// Poll `ready` to [`BUDGET`], failing with `what` and the [`dump`] when it never holds.
fn wait_for(
    home: &Path,
    fake: &FakeTrello,
    card: &str,
    daemon: &StreamingDaemon,
    what: &str,
    ready: impl Fn() -> bool,
) {
    let deadline = Instant::now() + BUDGET;
    while !ready() {
        assert!(
            Instant::now() < deadline,
            "{what} within {BUDGET:?}\n{}",
            dump(home, fake, card, daemon)
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Whether `card`'s thread holds afkd's claim comment.
fn claimed(fake: &FakeTrello, card: &str) -> bool {
    let me = fake.me();
    fake.comments(card)
        .iter()
        .any(|c| c.author == me && c.text.starts_with("[afkd-claim]"))
}

/// Drive one card through the plugin, installed in `home` and serving `kind`, on a real
/// daemon: it is **claimed** (the claim comment, and `on_claim` moved it), **run** (its run
/// dir is named for the card, and its task carries the card and its thread whole) and
/// **finished** (`on_done` applied, the claim released), and the daemon then drains clean.
fn drive_one_card(home: &Path, kind: &str) {
    let (fake, card) = seed();
    write_config(home, &service(home, kind, fake.base_url()));
    let (code, report) = validate(home);
    assert_eq!(code, Some(0), "the service validates:\n{report}");

    let daemon = spawn_headless_streaming(home, &[]);

    wait_for(home, &fake, &card, &daemon, "the card is claimed", || {
        claimed(&fake, &card) && fake.list_of(&card) == "In Progress"
    });
    // The run dir is made before the spine lays the unit's files into it, so the wait is
    // for a written brief, not the bare dir.
    let run_suffix = format!("-card-{SHORT_LINK}-1");
    let brief = || {
        run_dirs(home)
            .into_iter()
            .find(|name| name.ends_with(&run_suffix))
            .map(|run| runs_root(home).join("widgets").join(run).join("task.md"))
            .filter(|task| task.metadata().is_ok_and(|meta| meta.len() > 0))
    };
    wait_for(home, &fake, &card, &daemon, "the card's run starts", || {
        brief().is_some()
    });
    let task = read(&brief().expect("waited for"));
    let asked = format!("**陳大文:** {ASK}");
    for part in [
        TITLE,
        "> \"backoff\" — nobody",
        "let backoff = Duration::ZERO;",
        "— reported by 陳大文",
        &asked,
    ] {
        assert!(task.contains(part), "task.md carries {part:?}:\n{task}");
    }

    std::fs::write(home.join("release"), "").expect("release the run");
    wait_for(home, &fake, &card, &daemon, "the card is finished", || {
        fake.list_of(&card) == "Review" && !claimed(&fake, &card)
    });
    let me = fake.me();
    assert!(
        fake.comments(&card)
            .iter()
            .any(|c| c.author == me && c.text.starts_with("done in ") && !c.text.contains("@{")),
        "on_done's comment landed with its run reference filled in:\n{}",
        dump(home, &fake, &card, &daemon)
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
            "skipping: the afkd on PATH still has `trello` built in, so it refuses {NAME} \
             ({report}); this leg goes live once afkd drops the built-in"
        ),
        Install::Placed => {
            let exec = plugins_root(home.path())
                .join(NAME)
                .join("target/release/afkd-trello");
            let mode = std::fs::metadata(&exec)
                .unwrap_or_else(|e| panic!("the install built {}: {e}", exec.display()))
                .permissions()
                .mode();
            assert!(mode & 0o111 != 0, "{} is executable", exec.display());
            drive_one_card(home.path(), "trello");
        }
    }
}

#[test]
fn the_plugin_binary_runs_on_the_real_spine_under_a_probe_kind() {
    let home = TempDir::new().expect("tempdir");
    let stage_dir = TempDir::new().expect("tempdir");
    let staged = stage(stage_dir.path());
    probe_manifest(&staged);
    if let Install::BuiltIn(report) = install(home.path(), &staged) {
        panic!("the probe kind is never built in, yet afkd refused it:\n{report}");
    }
    drive_one_card(home.path(), "trello_probe");
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

/// The body of the top-level fn whose signature starts with `signature` in `source` (read
/// from `file`): from the signature to the first line that is a bare `}`. The signature must
/// occur exactly once, so a rename or a second definition reddens rather than reading the
/// wrong code.
fn fn_body<'a>(source: &'a str, file: &str, signature: &str) -> &'a str {
    let head = format!("{signature}(");
    let mut found = source.match_indices(&head);
    let (at, _) = found
        .next()
        .unwrap_or_else(|| panic!("{file} defines no `{signature}(…)`"));
    assert!(found.next().is_none(), "{file} defines `{signature}` once");
    let rest = &source[at..];
    let end = rest
        .match_indices("\n}\n")
        .next()
        .unwrap_or_else(|| panic!("{file}'s `{signature}` closes on a bare `}}` line"))
        .0;
    &rest[..end + 2]
}

/// The keys the code in `body` reads off a settings block: every `scalar("…")` and
/// `opt_scalar("…")`, in order.
fn scalar_keys(body: &str) -> Vec<String> {
    body.match_indices("scalar(\"")
        .map(|(at, head)| {
            let rest = &body[at + head.len()..];
            rest[..rest.find('"').expect("the key closes")].to_string()
        })
        .collect()
}

/// The `SCREAMING_CASE` names `body` mentions, sorted and deduplicated — the tables a fn
/// reads.
fn consts_named(body: &str) -> Vec<String> {
    sorted(
        body.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .filter(|word| {
                word.len() > 1
                    && word.starts_with(|c: char| c.is_ascii_uppercase())
                    && !word.contains(|c: char| c.is_ascii_lowercase())
            })
            .map(str::to_string),
    )
}

/// The trello arm of afkd's `builtin_trigger_keys` in `headless` (read from `file`): its
/// `("key", is_list)` pairs, in order. The arm's anchor also opens the display-name dispatch
/// elsewhere in the file, so it is looked for inside that fn only, and must occur once there.
fn trello_info_arm(headless: &str, file: &str) -> Vec<(String, bool)> {
    let body = fn_body(headless, file, "fn builtin_trigger_keys");
    let anchor = "afkd_trello::TRIGGER_KIND {";
    let mut found = body.match_indices(anchor);
    let (at, _) = found
        .next()
        .unwrap_or_else(|| panic!("{file}'s builtin_trigger_keys has no `{anchor}` arm"));
    assert!(
        found.next().is_none(),
        "{file}'s builtin_trigger_keys has one trello arm"
    );
    let arm = &body[at + anchor.len()..];
    let arm = &arm[..arm.find(']').expect("the arm's table closes")];
    let pairs: Vec<(String, bool)> = arm
        .split('(')
        .skip(1)
        .map(|pair| {
            let pair = &pair[..pair
                .find(')')
                .unwrap_or_else(|| panic!("{file}'s trello arm: `({pair}` closes"))];
            match quoted(pair).as_slice() {
                [key] if pair.ends_with(", false") => (key.clone(), false),
                [key] if pair.ends_with(", true") => (key.clone(), true),
                _ => panic!("{file}'s trello arm holds `({pair})`, not a (\"key\", bool) pair"),
            }
        })
        .collect();
    assert!(!pairs.is_empty(), "{file}'s trello arm names no key");
    pairs
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

/// `keys`, sorted and deduplicated.
fn sorted(keys: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut keys: Vec<String> = keys.into_iter().collect();
    keys.sort_unstable();
    keys.dedup();
    keys
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
    let afkd_settings = "crates/trello/src/settings.rs";
    let afkd_lib = "crates/trello/src/lib.rs";
    let afkd_headless = "crates/app/src/headless.rs";
    let (settings, lib, headless) = (
        read(&src.join(afkd_settings)),
        read(&src.join(afkd_lib)),
        read(&src.join(afkd_headless)),
    );
    let ported_settings = "@afkd/trello/src/settings.rs";
    let ported_lifecycle = "@afkd/trello/src/lifecycle.rs";
    let ported_plugin = "@afkd/trello/src/plugin.rs";
    let ported = read(&plugin_root().join("src/settings.rs"));
    let ported_actions = read(&plugin_root().join("src/lifecycle.rs"));
    let ported_kind = read(&plugin_root().join("src/plugin.rs"));

    let manifest: toml::Table = toml::from_str(&read(&plugin_root().join("afkd-plugin.toml")))
        .expect("the manifest parses");
    let triggers = manifest
        .get("trigger")
        .and_then(toml::Value::as_array)
        .expect("the manifest declares triggers");
    assert_eq!(triggers.len(), 1, "the manifest declares the one kind");
    let trigger = triggers[0].as_table().expect("a trigger is a table");

    let kind = str_value(&lib, afkd_lib, "TRIGGER_KIND");
    assert_eq!(
        kind,
        str_value(&ported_kind, ported_plugin, "TRELLO_KIND"),
        "the plugin's TRELLO_KIND is afkd's TRIGGER_KIND"
    );
    assert_eq!(
        trigger.get("kind").and_then(toml::Value::as_str),
        Some(kind.as_str()),
        "the manifest's kind is afkd's TRIGGER_KIND"
    );
    let whose = format!("the manifest's `{kind}`");
    assert_eq!(
        trigger.get("display_name").and_then(toml::Value::as_str),
        Some(str_value(&lib, afkd_lib, "TRIGGER_DISPLAY_NAME").as_str()),
        "{whose} display_name is afkd's TRIGGER_DISPLAY_NAME"
    );

    let allowed = str_list(&settings, afkd_settings, "ALLOWED_KEYS");
    let repeatable = str_list(&settings, afkd_settings, "REPEATABLE_KEYS");
    // (manifest key, afkd's table, what afkd calls it, the plugin's ported const)
    let tables = [
        ("settings", allowed.clone(), "ALLOWED_KEYS", "ALLOWED_KEYS"),
        (
            "repeatable",
            repeatable.clone(),
            "REPEATABLE_KEYS",
            "REPEATABLE_KEYS",
        ),
        (
            "required",
            required_keys(&settings, afkd_settings),
            "require_present keys",
            "REQUIRED_KEYS",
        ),
        (
            "durations",
            str_list(&settings, afkd_settings, "DURATION_KEYS"),
            "DURATION_KEYS",
            "DURATION_KEYS",
        ),
    ];
    for (key, in_tree, afkd_name, name) in tables {
        let declared = strings(trigger, key, &whose);
        assert_eq!(declared, in_tree, "{whose} `{key}` is afkd's {afkd_name}");
        assert_eq!(
            declared,
            str_list(&ported, ported_settings, name),
            "{whose} `{key}` is the plugin's {name}"
        );
    }

    let actions = str_list(&settings, afkd_settings, "LIFECYCLE_KEYS");
    assert_eq!(
        actions,
        str_list(&ported_actions, ported_lifecycle, "LIFECYCLE_KEYS"),
        "the plugin's LIFECYCLE_KEYS is afkd's"
    );
    assert!(
        settings.contains("const LIFECYCLE_REPEATABLE_KEYS: &[&str] = LIFECYCLE_KEYS;"),
        "{afkd_settings}'s lifecycle keys are all repeatable, as the blocks declare"
    );
    assert!(
        actions.iter().any(|a| a == "move_to"),
        "afkd's LIFECYCLE_KEYS has `move_to`, whose block the manifest declares"
    );
    let move_to = scalar_keys(fn_body(&settings, afkd_settings, "fn parse_move_to"));
    assert_eq!(
        move_to,
        ["at"],
        "{afkd_settings}'s parse_move_to reads `at`"
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
    let moments: Vec<&String> = allowed.iter().filter(|k| k.starts_with("on_")).collect();
    let expected: Vec<String> = moments
        .iter()
        .flat_map(|moment| [moment.to_string(), format!("{moment}.move_to")])
        .collect();
    assert_eq!(
        paths, expected,
        "{whose} blocks are each on_* key of ALLOWED_KEYS and its move_to"
    );
    let mut block_settings = Vec::new();
    for block in blocks {
        let block = block.as_table().expect("a block is a table");
        let path = block["path"].as_str().unwrap_or("?");
        let at = format!("{whose} block `{path}`");
        let (in_tree, name) = if path.ends_with(".move_to") {
            (&move_to, "what parse_move_to reads")
        } else {
            (&actions, "LIFECYCLE_KEYS")
        };
        for key in ["settings", "repeatable"] {
            assert_eq!(&strings(block, key, &at), in_tree, "{at} `{key}` is {name}");
        }
        if !path.contains('.') {
            block_settings.extend(strings(block, "settings", &at));
        }
    }

    let union = fn_body(&settings, afkd_settings, "pub fn settings_keys");
    assert!(
        union.contains("ALLOWED_KEYS.iter().chain(LIFECYCLE_KEYS)")
            && consts_named(union) == ["ALLOWED_KEYS", "LIFECYCLE_KEYS"],
        "{afkd_settings}'s settings_keys is the union of ALLOWED_KEYS and LIFECYCLE_KEYS, \
         and of no other table:\n{union}"
    );
    assert_eq!(
        sorted(
            strings(trigger, "settings", &whose)
                .into_iter()
                .chain(block_settings)
        ),
        sorted(allowed.iter().chain(&actions).cloned()),
        "{whose} settings and on_* blocks together are afkd's settings_keys()"
    );

    let arm = trello_info_arm(&headless, afkd_headless);
    let info_keys = strings(trigger, "info_keys", &whose);
    let arm_keys: Vec<&str> = arm.iter().map(|(key, _)| key.as_str()).collect();
    assert_eq!(
        info_keys, arm_keys,
        "{whose} info_keys is the trello arm of afkd's builtin_trigger_keys"
    );
    for (key, is_list) in &arm {
        assert_eq!(
            repeatable.contains(key),
            *is_list,
            "{whose} info key `{key}` reads as a list iff it is repeatable, as afkd's arm has it"
        );
    }
}
