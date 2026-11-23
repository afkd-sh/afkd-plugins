//! The gate that holds the **shipped `@afkd/afkd_board` plugin** — a manifest v2 provider with no
//! trigger kind, shipped as a finished binary — to a real afkd: the `afkd` first on `PATH`,
//! never a build, since a plugin is checked against the afkd it will meet ([`bin_path`]).
//!
//! - The **install by name** leg packs the release as `.github/workflows/release-afkd_board.yml`
//!   does, serves it and an index naming it on loopback, and installs `@afkd/afkd_board` by its
//!   name alone: afkd reads the index once, fills in the platform, fetches the archive once and
//!   places exactly the manifest and the binary.
//! - The **published index** leg holds the entry the site's `public/plugins.json` ships to the
//!   manifest's name and to this repository's fixed `afkd_board` release, in the afkd.sh checkout
//!   `AFKD_SITE_SRC` names ([`site_src`]); it skips loudly without one.
//! - The **validate** leg holds `afkd validate` to a service calling every read with every
//!   parameter and reading every field of every handle and value, through two `afkd_board.Board`
//!   values; and another calling every write as the brief writes it, positionally where it says
//!   so (`afkd_board.create_list(TRAIN, "Review")`), `run=run` on every write of a card, a
//!   checklist and a comment, the run `nil` and left out as well.
//! - The **reads** leg runs an interval service by hand whose run reads two boards through
//!   children afkd starts for each call: the lists, the labels and the cards of one, the cards
//!   of the other, one card read twice and `==` itself, its comments, and the refusal of that
//!   card passed with the other board, naming both.
//! - The **writes** leg runs an interval service by hand whose run echoes its own `machine`,
//!   `service`, `instance` and `id`, makes a card with `run=run` and comments on a card with no
//!   run: the board's fake recorded the card's body `run` as exactly the echoed four fields, and
//!   the comment's body with no `run` key.
//! - The **README** leg validates every `conf` fence of the plugin's README, each a whole entry
//!   file, against the installed release, with every variable the fences read set by
//!   [`CONFIG_ENV`].
//!
//! The boards are the plugin's own loopback fake ([`fake`]), which records each request it
//! should never have been sent: each leg that reads or writes ends with that record empty. No
//! leg touches a network, every wait is bounded, and every spawn is held in a [`Daemon`].

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use common::fake::{FakeBoard, NewCard, Run, T0};
use common::*;
use tempfile::TempDir;

// --- the shipped tree --------------------------------------------------------------

/// The plugin's name, as the manifest spells it and afkd places it.
const NAME: &str = "@afkd/afkd_board";

/// The source the site's index lists the plugin under: this repository's fixed `afkd_board`
/// release, one archive per platform, `{platform}` filled in by afkd.
const RELEASE_URL: &str =
    "https://github.com/afkd-sh/afkd-plugins/releases/download/afkd_board/afkd-afkd_board-{platform}.tar.gz";

/// The plugin's root — the directory `afkd install` takes, whose path this crate's own
/// mirrors under `drift/`. Canonicalized, so a staged copy and a report both name one path.
fn plugin_root() -> PathBuf {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../@afkd/afkd_board");
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
        plugin_root().join("target/release/afkd-afkd_board")
    })
}

/// What a release holds, relative and sorted: the manifest, the binary and the two licence
/// texts, nothing else.
const RELEASE_TREE: &[&str] = &[
    "LICENSE-APACHE",
    "LICENSE-MIT",
    "afkd-afkd_board",
    "afkd-plugin.toml",
];

/// Stage the release `tools/pack-release` packs under `dst` and return the staged root,
/// `<dst>/afkd-afkd_board`, failing unless it holds exactly [`RELEASE_TREE`].
fn stage(dst: &Path) -> PathBuf {
    let status = Command::new(plugin_root().join("../../tools/pack-release"))
        .arg("afkd_board")
        .arg(dst)
        .arg(built_binary())
        .status()
        .expect("run tools/pack-release");
    assert!(status.success(), "tools/pack-release afkd_board");
    let staged = dst.join("afkd-afkd_board");
    assert_eq!(
        tree_of(&staged),
        RELEASE_TREE,
        "the release holds the manifest, the binary and the licence texts alone"
    );
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

/// The manifest's line `key = "…"`'s value.
fn manifest_value(key: &str) -> String {
    let manifest =
        std::fs::read_to_string(plugin_root().join("afkd-plugin.toml")).expect("the manifest");
    let prefix = format!("{key} = ");
    manifest
        .lines()
        .find_map(|line| line.strip_prefix(prefix.as_str()))
        .map(|value| value.trim().trim_matches('"').to_string())
        .unwrap_or_else(|| panic!("the manifest declares a {key}"))
}

// --- the configs -------------------------------------------------------------------

/// Write `config` as `home`'s main config.
fn write_config(home: &Path, config: &str) {
    let conf = daemon_afk(home);
    std::fs::create_dir_all(conf.parent().expect("has parent")).expect("mk the config dir");
    std::fs::write(&conf, config).expect("write the config");
}

/// The keys a config reads from the daemon's environment, as the README's examples do: one
/// board's and another's.
const CONFIG_ENV: &[(&str, &str)] = &[
    ("AFKD_BOARD_KEY", "afk_board_train-0123456789"),
    ("AFKD_OPS_BOARD_KEY", "afk_board_ops-0123456789"),
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

/// Read `path` whole, naming it when it is missing.
fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

// --- install by name ---------------------------------------------------------------

#[test]
fn the_published_index_entry_matches_the_shipped_plugin() {
    // afkd never cross-checks an index key against the manifest it fetches, so a key that
    // drifts from the declared name installs under one spelling and lists under another.
    // Caught here instead, over the two files that would drift: the index afkd.sh serves and
    // the manifest this repository ships.
    let Some(site) = site_src() else {
        eprintln!("SKIP: AFKD_SITE_SRC names no afkd.sh checkout, and the index is the site's");
        return;
    };
    let path = site.join("public/plugins.json");
    let shipped = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("the index at {}: {err}", path.display()));
    let index: serde_json::Value =
        serde_json::from_str(&shipped).expect("the shipped index is JSON");
    let plugins = index["plugins"]
        .as_object()
        .expect("the index has a `plugins` object");
    assert!(
        plugins.contains_key(NAME),
        "the shipped index does not list {NAME}: {plugins:?}"
    );
    assert_eq!(
        manifest_value("name"),
        NAME,
        "the index key and the manifest's declared name have drifted apart"
    );
    assert_eq!(
        plugins[NAME]["source"].as_str(),
        Some(RELEASE_URL),
        "the entry does not point at this repository's afkd_board release"
    );
    // `PluginSource::classify` routes on the `.tar.gz` suffix: a release URL without it would
    // be taken for a git remote and cloned.
    assert!(
        RELEASE_URL.ends_with(".tar.gz"),
        "the release URL is not classified as a tarball"
    );
}

#[test]
fn installs_by_name_through_a_loopback_index() {
    // The publishing claim end to end, with nothing of this repository reachable but what a
    // stranger's host reaches: an index, and an archive at the address it names, packed as the
    // release workflow packs it.
    let packing = TempDir::new().expect("a packing dir");
    stage(packing.path());
    let archive = packing.path().join("afkd-afkd_board.tar.gz");
    let packed = Command::new("tar")
        .arg("czf")
        .arg(&archive)
        .arg("-C")
        .arg(packing.path())
        .arg("afkd-afkd_board")
        .status()
        .expect("run tar");
    assert!(packed.success(), "tar packs the staged release");
    let tarball = serve(
        "application/gzip",
        std::fs::read(&archive).expect("the archive"),
    );
    let listing = format!(
        "{{\"index\": 1, \"plugins\": {{\"{NAME}\": {{\"source\": \"{}/afkd-afkd_board-{{platform}}.tar.gz\"}}}}}}",
        tarball.base
    );
    let index = serve("application/json", listing.into_bytes());
    let index_url = format!("{}/plugins.json", index.base);

    let home = TempDir::new().expect("tempdir");
    let out = run_subcommand_env(
        home.path(),
        &["install", NAME],
        &[("AFKD_PLUGIN_INDEX_URL", index_url.as_str())],
    );
    let report = String::from_utf8_lossy(&out.stdout).into_owned();
    let fault = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(out.status.code(), Some(0), "{report}{fault}");
    let version = manifest_value("version");
    assert!(
        report.contains(&format!("Installed {NAME} {version} (provider)")),
        "the report names the plugin, its version and its shape: {report}"
    );
    assert_eq!(
        index.requests(),
        ["GET /plugins.json HTTP/1.1"],
        "a bare name resolves through the index once"
    );
    assert_eq!(
        tarball.requests(),
        [format!(
            "GET /afkd-afkd_board-{}-linux.tar.gz HTTP/1.1",
            std::env::consts::ARCH
        )],
        "the archive for this host's platform is fetched once"
    );

    let placed = plugins_root(home.path()).join(NAME);
    assert_eq!(
        tree_of(&placed),
        RELEASE_TREE,
        "the install placed exactly the archive's contents"
    );
    let mode = std::fs::metadata(placed.join("afkd-afkd_board"))
        .expect("stat the binary")
        .permissions()
        .mode();
    assert_eq!(
        mode & 0o111,
        0o111,
        "the binary is still executable once placed"
    );
}

// --- validate ----------------------------------------------------------------------

/// A service calling every read the plugin declares with every parameter it takes, through
/// two `afkd_board.Board` values, one with a `base_url`, and reading every field of every handle
/// and value a read gives back, the optional ones through `??`, validates: afkd types every
/// argument and field against the manifest.
#[test]
fn every_read_validates() {
    let home = installed();
    write_config(
        home.path(),
        r###"import "afkd"
import "svc:interval"
import "@afkd/afkd_board"

TRAIN :: afkd_board.Board{ key: "afk_board_train-0123456789" }
OPS   :: afkd_board.Board{ key: "afk_board_ops-0123456789", base_url: "http://127.0.0.1:8088" }

nightly :: service(interval) {
  every    1h
  work_dir "/srv/acme/widgets"

  on_run proc(run: afkd.Run) {
    match afkd_board.lists(TRAIN) {
      .ok(lists) => {
        for list in lists {
          $ echo #{list.id} #{list.name}
        }
      }
      .err(e) => fail("afkd_board.lists: #{e.message}")
    }
    match afkd_board.labels(OPS) {
      .ok(labels) => {
        for label in labels {
          $ echo #{label.name} #{label.colour}
        }
      }
      .err(e) => fail("afkd_board.labels: #{e.message}")
    }
    lead := match afkd_board.card(TRAIN, 17) {
      .ok(card) => card
      .err(e) => fail("afkd_board.card: #{e.message}")
    }
    match afkd_board.cards(
      TRAIN,
      list=[ "Todo", "In progress 🚧" ],
      labels=[ "bug" ],
      without_labels=[ "hold", "待定" ],
      done=false,
      blocked=nil,
      text="retry storm 🚨",
      include_archived=true,
      limit=250,
    ) {
      .ok(cards) => {
        for card in cards {
          $ echo #{card.id} #{card.number} #{card.title} #{card.list} #{card.url}
          $ echo #{card.description} #{card.created_at} #{card.updated_at}
          for label in card.labels {
            $ echo #{label}
          }
          for blocker in card.blocked_by {
            $ echo blocked by #{blocker}
          }
          for blocked in card.blocking {
            $ echo blocks #{blocked}
          }
          if card.archived {
            $ echo archived
          }
          if card.done {
            $ echo done
          }
          if card.blocked {
            $ echo blocked
          }
          if card == lead {
            $ echo same
          }
          match afkd_board.comments(TRAIN, card) {
            .ok(comments) => {
              for comment in comments {
                $ echo #{comment.id} #{comment.card} #{comment.author ?? "key"} #{comment.run ?? "-"}
                $ echo #{comment.body} #{comment.created_at} #{comment.url}
              }
            }
            .err(e) => fail("afkd_board.comments: #{e.message}")
          }
        }
      }
      .err(e) => fail("afkd_board.cards: #{e.message}")
    }
    match afkd_board.cards(OPS, done=true, blocked=false) {
      .ok(cards) => {
        for card in cards {
          $ echo #{card.id}
        }
      }
      .err(e) => fail("afkd_board.cards: #{e.message}")
    }
  }
}
"###,
    );
    let (code, report) = validate(home.path());
    assert_eq!(code, Some(0), "every read validates:\n{report}");
}

/// A match on the call `call`, of an action that gives nothing a config reads, which fails the run
/// with the call's name on an `.err`.
fn checked(call: &str) -> String {
    let name = call
        .trim_start_matches("afkd_board.")
        .split('(')
        .next()
        .unwrap_or(call);
    format!(
        "    match {call} {{\n      .ok(_) => {{ }}\n      .err(e) => fail(\"afkd_board.{name}: \
         #{{e.message}}\")\n    }}\n"
    )
}

/// A service calling every write the plugin declares as the brief writes them, validates: the
/// list writes with their name or their list positional (`afkd_board.create_list(TRAIN,
/// "Review")`) and `name`, `at` and `after` named, a card made with every parameter and a handle
/// read passed as `after`, `title` positional in `create_card` and named in `update_card`, a tick
/// with its `done` positional and left to its default, `run=run`, `run=nil` and no run on the
/// writes of a card, a checklist and a comment, and the handles and values they give read
/// field by field.
#[test]
fn every_write_validates() {
    let home = installed();
    let calls = [
        r#"afkd_board.create_list(TRAIN, "Icebox", at=.top)"#,
        r#"afkd_board.create_list(TRAIN, "Mid", after="Todo")"#,
        r#"afkd_board.update_list(TRAIN, "Review", name="Reviewed", after="Todo")"#,
        r#"afkd_board.update_list(TRAIN, "Icebox", at=.bottom)"#,
        r#"afkd_board.create_card(TRAIN, "Todo", "Placed", after=lead, run=run)"#,
        r#"afkd_board.create_card(TRAIN, "Todo", "Bare")"#,
        r#"afkd_board.update_card(TRAIN, lead, title="Renamed 🚨", run=run)"#,
        r#"afkd_board.update_card(TRAIN, lead, description="Text", run=nil)"#,
        r#"afkd_board.move_card(TRAIN, lead, "Doing", at=.top, run=run)"#,
        r#"afkd_board.move_card(TRAIN, lead, "Doing", after=other, run=run)"#,
        r#"afkd_board.move_card(TRAIN, lead, "Todo")"#,
        r#"afkd_board.add_label(TRAIN, lead, "bug", run=run)"#,
        r#"afkd_board.remove_label(TRAIN, lead, "待定", run=run)"#,
        r#"afkd_board.add_item(TRAIN, lead, "Ship it", run=run)"#,
        r#"afkd_board.check_item(TRAIN, lead, 0, false, run=run)"#,
        r#"afkd_board.check_item(TRAIN, lead, 0, run=run)"#,
        r#"afkd_board.remove_item(TRAIN, lead, 0, run=run)"#,
        r#"afkd_board.mark_done(TRAIN, lead, run=run)"#,
        r#"afkd_board.mark_done(TRAIN, lead, false, run=run)"#,
        r#"afkd_board.add_blocked_by(TRAIN, lead, other, run=run)"#,
        r#"afkd_board.remove_blocked_by(TRAIN, lead, other, run=run)"#,
        r#"afkd_board.archive_card(TRAIN, lead, run=run)"#,
        r#"afkd_board.unarchive_card(TRAIN, lead, run=run)"#,
        r#"afkd_board.delete_card(TRAIN, lead, run=run)"#,
        r#"afkd_board.delete_list(TRAIN, "Reviewed")"#,
    ];
    let checked: String = calls.iter().map(|call| checked(call)).collect();
    let config = r###"import "afkd"
import "svc:interval"
import "@afkd/afkd_board"

TRAIN :: afkd_board.Board{ key: "afk_board_train-0123456789" }

nightly :: service(interval) {
  every    1h
  work_dir "/srv/acme/widgets"

  on_run proc(run: afkd.Run) {
    made := match afkd_board.create_list(TRAIN, "Review") {
      .ok(list) => list
      .err(e) => fail("afkd_board.create_list: #{e.message}")
    }
    $ echo #{made.id} #{made.name}
    lead := match afkd_board.card(TRAIN, 17) {
      .ok(card) => card
      .err(e) => fail("afkd_board.card: #{e.message}")
    }
    other := match afkd_board.card(TRAIN, 18) {
      .ok(card) => card
      .err(e) => fail("afkd_board.card: #{e.message}")
    }
    first := match afkd_board.create_card(
      TRAIN,
      "Todo",
      "Title",
      description="Why 🚨",
      labels=[ "bug", "待定" ],
      checklist=[ afkd_board.Item{ text: "Reproduce it", done: false } ],
      at=.top,
      run=run,
    ) {
      .ok(card) => card
      .err(e) => fail("afkd_board.create_card: #{e.message}")
    }
    $ echo #{first.id} #{first.number} #{first.title} #{first.list} #{first.url}
    comment := match afkd_board.create_comment(TRAIN, lead, "Looks good", run=run) {
      .ok(written) => written
      .err(e) => fail("afkd_board.create_comment: #{e.message}")
    }
    $ echo #{comment.id} #{comment.card} #{comment.body} #{comment.author ?? "key"} #{comment.run ?? "-"}
CHECKED  }
}
"###
    .replace("CHECKED", &checked);
    write_config(home.path(), &config);
    let (code, report) = validate(home.path());
    assert_eq!(code, Some(0), "every write validates:\n{report}");
}

// --- the reads leg -----------------------------------------------------------------

/// The file, under the reads leg's work dir, its run writes what each read gave back to.
const READS: &str = "reads.txt";

/// How long the reads leg waits for its run.
const BUDGET: Duration = Duration::from_secs(30);

/// The key of the reads leg's first board, and of its second.
const TRAIN_KEY: &str = "afk_board_train-0123456789";
const OPS_KEY: &str = "afk_board_ops-0123456789";

/// The title of the first board's first card: wide CJK, an emoji and quotes.
const TITLE: &str = "修复 the \"retry\" storm 🚨";

/// The reads leg's service: an interval service whose run reads the two boards at
/// `train` and `ops` through an `afkd_board.Board` each, writing what it read to [`READS`] under
/// `home`, and `end` last.
fn reads_service(home: &Path, train: &str, ops: &str) -> String {
    format!(
        r###"import "afkd"
import "svc:interval"
import "@afkd/afkd_board"

TRAIN :: afkd_board.Board{{ key: "{TRAIN_KEY}", base_url: "{train}" }}
OPS   :: afkd_board.Board{{ key: "{OPS_KEY}", base_url: "{ops}" }}

nightly :: service(interval) {{
  every    24h
  work_dir "{home}"

  on_run proc(run: afkd.Run) {{
    match afkd_board.lists(TRAIN) {{
      .ok(lists) => {{
        for list in lists {{
          $ echo list #{{list.name}} >> {READS}
        }}
      }}
      .err(e) => fail("afkd_board.lists: #{{e.message}}")
    }}
    match afkd_board.labels(TRAIN) {{
      .ok(labels) => {{
        for label in labels {{
          $ echo label #{{label.name}} #{{label.colour}} >> {READS}
        }}
      }}
      .err(e) => fail("afkd_board.labels: #{{e.message}}")
    }}
    match afkd_board.cards(TRAIN, list=[ "In progress", "Todo" ]) {{
      .ok(cards) => {{
        for card in cards {{
          $ echo card #{{card.id}} #{{card.title}} #{{card.list}} >> {READS}
        }}
      }}
      .err(e) => fail("afkd_board.cards: #{{e.message}}")
    }}
    match afkd_board.cards(OPS) {{
      .ok(cards) => {{
        for card in cards {{
          $ echo ops #{{card.id}} #{{card.title}} >> {READS}
        }}
      }}
      .err(e) => fail("afkd_board.cards: #{{e.message}}")
    }}
    lead := match afkd_board.card(TRAIN, 1) {{
      .ok(card) => card
      .err(e) => fail("afkd_board.card: #{{e.message}}")
    }}
    $ echo key #{{lead.key}} >> {READS}
    match afkd_board.cards(TRAIN, labels=[ "bug" ]) {{
      .ok(cards) => {{
        for card in cards {{
          if card == lead {{
            $ echo same >> {READS}
          }}
        }}
      }}
      .err(e) => fail("afkd_board.cards: #{{e.message}}")
    }}
    match afkd_board.comments(TRAIN, lead) {{
      .ok(comments) => {{
        for comment in comments {{
          $ echo comment #{{comment.author ?? "key"}} #{{comment.run ?? "-"}} #{{comment.body}} >> {READS}
        }}
      }}
      .err(e) => fail("afkd_board.comments: #{{e.message}}")
    }}
    match afkd_board.comments(OPS, lead) {{
      .ok(comments) => fail("afkd_board.comments read a card of TRAIN through OPS")
      .err(e) => {{
        $ echo refused #{{e.message}} >> {READS}
      }}
    }}
    match afkd_board.cards(TRAIN, include_archived=true) {{
      .ok(cards) => {{
        for card in cards {{
          $ echo all #{{card.id}} >> {READS}
        }}
      }}
      .err(e) => fail("afkd_board.cards: #{{e.message}}")
    }}
    $ echo end >> {READS}
  }}
}}
"###,
        home = home.display()
    )
}

/// The service's run dirs, sorted by name: the kept ones under `<state>/runs`, then the ones in
/// flight under the daemon's `$TMPDIR`; a name in both, a run caught mid-move, is the kept one.
fn run_dirs(home: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    for root in [runs_root(home), live_runs_root(home)] {
        for entry in std::fs::read_dir(root.join("nightly"))
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

/// Everything a failed wait needs to say: what the run wrote so far to `file`, the requests each
/// board should never have been sent, each run's log, the daemon's own log, and its stderr so far
/// — where the plugin's own complaints land.
fn dump(home: &Path, file: &str, boards: &[&FakeBoard], daemon: &StreamingDaemon) -> String {
    let logs: Vec<String> = run_dirs(home)
        .iter()
        .map(|dir| {
            format!(
                "{}:\n{}",
                dir.display(),
                std::fs::read_to_string(dir.join("run.log")).unwrap_or_default()
            )
        })
        .collect();
    let invalid: Vec<Vec<String>> = boards.iter().map(|b| b.invalid_requests()).collect();
    format!(
        "{file}:\n{}\nrefused: {invalid:?}\nruns:\n{}\ndaemon.log:\n{}\ndaemon stderr:\n{}",
        std::fs::read_to_string(home.join(file)).unwrap_or_default(),
        logs.join("\n"),
        std::fs::read_to_string(daemon_log(home)).unwrap_or_default(),
        daemon.stderr_so_far()
    )
}

/// A service of another kind reads two boards side by side through a child afkd starts for
/// each call, each through its own `afkd_board.Board`: one board's lists, labels and cards of
/// two lists in the board's order, the other's cards, one card read by `afkd_board.card` and by
/// `afkd_board.cards` and `==` itself, its key, its comments, a person's and a key's in a run,
/// and its refusal when passed with the other board, naming both. Each board is sent its own key
/// alone.
#[test]
fn reads_leg_two_boards_through_a_child_per_call() {
    let home = installed();
    let home = home.path();
    let train = FakeBoard::start(
        "Release train 🚂",
        TRAIN_KEY,
        &["Todo", "In progress", "Done"],
        &[("bug", "red"), ("媒体", "amber")],
    );
    train.card(NewCard {
        labels: vec!["bug".to_string()],
        description: "The client retries forever.\n\n```rust\nlet backoff = 0;\n```".to_string(),
        ..NewCard::new("Todo", TITLE)
    });
    train.card(NewCard {
        labels: vec!["媒体".to_string()],
        ..NewCard::new("In progress", "Cut the tag")
    });
    train.card(NewCard {
        done: true,
        ..NewCard::new("Done", "Ship 0.1")
    });
    train.card(NewCard::new("Todo", "An old idea"));
    train.archive(4, T0 + 3600);
    train.comment(1, "看起来不对 🚨 — it resets", "Ed", false, None, 60);
    train.comment(
        1,
        "Backoff capped at 30s",
        "nightly",
        true,
        Some(Run {
            machine: "nova".to_string(),
            service: "develop".to_string(),
            instance: "0".to_string(),
            id: "develop-0-42".to_string(),
        }),
        120,
    );
    let ops = FakeBoard::start("Ops", OPS_KEY, &["Inbox"], &[("pager", "red")]);
    ops.card(NewCard::new("Inbox", "Rotate the 🔑 keys"));
    assert_ne!(train.board_id(), ops.board_id(), "two boards, two uuids");

    write_config(home, &reads_service(home, train.base_url(), ops.base_url()));
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
            dump(home, READS, &[&train, &ops], &daemon)
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    while !std::fs::read_to_string(home.join(READS)).is_ok_and(|r| r.ends_with("end\n")) {
        assert!(
            Instant::now() < deadline,
            "the run read both boards within {BUDGET:?}\n{}",
            dump(home, READS, &[&train, &ops], &daemon)
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        read(&home.join(READS)),
        format!(
            "list Todo\nlist In progress\nlist Done\nlabel bug red\nlabel 媒体 amber\n\
             card 1 {TITLE} Todo\ncard 2 Cut the tag In progress\nops 1 Rotate the 🔑 keys\n\
             key afkd_board:{}/{}\nsame\ncomment Ed - 看起来不对 🚨 — it resets\n\
             comment key develop-0-42 Backoff capped at 30s\n\
             refused comments: card is #1 of board {}, and the afkd_board.Board reaches board {}\n\
             all 1\nall 4\nall 2\nall 3\nend\n",
            train.board_id(),
            train.card_id(1),
            train.board_id(),
            ops.board_id()
        ),
        "what the run read\n{}",
        dump(home, READS, &[&train, &ops], &daemon)
    );
    for (board, key) in [(&train, TRAIN_KEY), (&ops, OPS_KEY)] {
        let seen = board.seen();
        assert!(!seen.is_empty(), "the run read the board {key} reaches");
        let bearer = format!("Bearer {key}");
        assert!(
            seen.iter()
                .all(|s| s.authorization.as_deref() == Some(bearer.as_str())),
            "every request to a board went out with its own key alone: {seen:?}"
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
    for board in [&train, &ops] {
        assert_eq!(
            board.invalid_requests(),
            Vec::<String>::new(),
            "requests the board's API refuses"
        );
    }
}

// --- the writes leg ----------------------------------------------------------------

/// The file, under the writes leg's work dir, its run echoes its own run to, one `<field> <value>`
/// a line.
const ECHOED: &str = "run.txt";

/// The file, under the writes leg's work dir, its run writes what each write gave back to.
const WRITTEN: &str = "writes.txt";

/// The writes leg's service: an interval service whose run echoes the `machine`, the `service`,
/// the `instance` and the `id` of the `afkd.Run` it is passed to [`ECHOED`], makes a card in
/// `Todo` with `run=run`, and comments on the card it read with no run, writing to [`WRITTEN`]
/// under `home`, and `end` last. The board is the one at `train`.
fn writes_service(home: &Path, train: &str) -> String {
    r###"import "afkd"
import "svc:interval"
import "@afkd/afkd_board"

TRAIN :: afkd_board.Board{ key: "KEY", base_url: "URL" }

nightly :: service(interval) {
  every    24h
  work_dir "HOME"

  on_run proc(run: afkd.Run) {
    $ echo machine #{run.machine} >> ECHOED
    $ echo service #{run.service} >> ECHOED
    $ echo instance #{run.instance} >> ECHOED
    $ echo id #{run.id} >> ECHOED
    lead := match afkd_board.card(TRAIN, 1) {
      .ok(card) => card
      .err(e) => fail("afkd_board.card: #{e.message}")
    }
    match afkd_board.create_card(TRAIN, "Todo", "Made in a run 🚀", description="By a run", run=run) {
      .ok(card) => {
        $ echo card #{card.id} #{card.title} >> WRITTEN
      }
      .err(e) => fail("afkd_board.create_card: #{e.message}")
    }
    match afkd_board.create_comment(TRAIN, lead, "No run here") {
      .ok(comment) => {
        $ echo comment #{comment.card} #{comment.run ?? "-"} >> WRITTEN
      }
      .err(e) => fail("afkd_board.create_comment: #{e.message}")
    }
    $ echo end >> WRITTEN
  }
}
"###
    .replace("KEY", TRAIN_KEY)
    .replace("URL", train)
    .replace("ECHOED", ECHOED)
    .replace("WRITTEN", WRITTEN)
    .replace("HOME", &home.display().to_string())
}

/// A service whose run writes to a board through children afkd starts for each call proves
/// acceptance 3 through a real afkd: a write passed `run=run` reaches the board with that run's
/// `machine`, `service`, `instance` and `id`, as the run itself echoed them, and a write passed no
/// run reaches it with no `run` key at all. The board is never sent a request it refuses.
#[test]
fn writes_leg_each_write_under_its_run() {
    let home = installed();
    let home = home.path();
    let train = FakeBoard::start("Release train 🚂", TRAIN_KEY, &["Todo", "Done"], &[]);
    train.card(NewCard::new("Todo", "Lead"));

    write_config(home, &writes_service(home, train.base_url()));
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
            dump(home, WRITTEN, &[&train], &daemon)
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    while !std::fs::read_to_string(home.join(WRITTEN)).is_ok_and(|w| w.ends_with("end\n")) {
        assert!(
            Instant::now() < deadline,
            "the run wrote to the board within {BUDGET:?}\n{}",
            dump(home, WRITTEN, &[&train], &daemon)
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    let echoed = read(&home.join(ECHOED));
    let field = |name: &str| -> String {
        echoed
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{name} ")))
            .unwrap_or_else(|| panic!("the run echoed no {name}:\n{echoed}"))
            .to_string()
    };
    let run = serde_json::json!({
        "machine": field("machine"),
        "service": field("service"),
        "instance": field("instance"),
        "id": field("id"),
    });
    assert_eq!(
        read(&home.join(WRITTEN)),
        "card 2 Made in a run 🚀\ncomment 1 -\nend\n",
        "what the run wrote\n{}",
        dump(home, WRITTEN, &[&train], &daemon)
    );

    let writes = train.writes();
    let paths: Vec<String> = writes
        .iter()
        .map(|w| format!("{} {}", w.method, w.path))
        .collect();
    assert_eq!(
        paths,
        ["POST /api/board/cards", "POST /api/board/cards/1/comments"],
        "{writes:?}"
    );
    let made = writes[0].body.as_ref().expect("the card's body");
    assert_eq!(
        made.get("run"),
        Some(&run),
        "the card was made under the run the service echoed: {made}"
    );
    let comment = writes[1].body.as_ref().expect("the comment's body");
    assert_eq!(
        comment,
        &serde_json::json!({"body": "No run here"}),
        "a comment written with no run carries no `run` key"
    );
    let bearer = format!("Bearer {TRAIN_KEY}");
    assert!(
        train
            .seen()
            .iter()
            .all(|s| s.authorization.as_deref() == Some(bearer.as_str())),
        "every request went out with the board's key alone"
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
        train.invalid_requests(),
        Vec::<String>::new(),
        "requests the board's API refuses"
    );
}

// --- the README --------------------------------------------------------------------

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
        "the README's fences read their keys from the environment"
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
