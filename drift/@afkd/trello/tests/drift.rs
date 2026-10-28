//! The gate that holds the **shipped `@afkd/trello` plugin** — a manifest v2 provider whose
//! main kind is `service(trello)`, shipped as a finished binary — to a real afkd: the
//! `afkd` first on `PATH`, the installed one and never a build, since a plugin is checked
//! against the afkd it will meet ([`bin_path`]).
//!
//! - The **install** leg installs the tree the release tarball holds and runs one card
//!   through it on a real daemon, its slots' actions crossing as `call`s on the card each
//!   is passed and its slots reading every field the card's handle declares.
//! - The **two lists** leg claims, on a real daemon, a card from the second of the two
//!   lists its `pick_from` names, past a held card in the first.
//! - The **outage** leg fails a card through both its attempts while Trello is out of
//!   reach, and holds the card to its failed state once Trello is back: both attempt notes
//!   on it, in Backlog with the Problem label, its claim released.
//! - The **attachments** leg fetches, in `on_claim`, the newest of two plans an earlier
//!   attempt left on the card, and finds no patch; its `on_run` then attaches one, and
//!   the card holds it byte for byte.
//! - The **complete example** leg `afkd validate`s the selfdev pipeline of config language
//!   v2's §24 against the installed plugin, and the **handle** leg holds `afkd validate` to
//!   refusing an action called without its card.
//! - The **cadence** leg holds `afkd validate` to refusing a service that writes the claim
//!   key afkd renamed to `every`, pointing it at `every`, and to taking the same service
//!   with `every`.
//! - The **README** leg `afkd validate`s every `conf` fence the plugin's README carries,
//!   each a whole entry file, against the installed plugin.
//!
//! The Trello is the plugin's own loopback fake ([`fake`]), so no leg touches a network, and
//! every wait is bounded and every spawn held in a [`Daemon`]: a claim that never lands must
//! **fail**, not hang.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use common::fake::{FakeTrello, BOARD, KEY, ME, TOKEN};
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
        plugin_root().join("target/release/afkd-trello")
    })
}

/// Stage the release `tools/pack-release` packs under `dst` — the manifest, the binary and
/// the skill, exactly what the release archive holds — and return the staged root.
fn stage(dst: &Path) -> PathBuf {
    let status = Command::new(plugin_root().join("../../tools/pack-release"))
        .arg("trello")
        .arg(dst)
        .arg(built_binary())
        .status()
        .expect("run tools/pack-release");
    assert!(status.success(), "tools/pack-release trello");
    let staged = dst.join("afkd-trello");
    for present in ["afkd-plugin.toml", "afkd-trello", "skills/trello/SKILL.md"] {
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

/// The card the service claims, its short link in Trello's own shape, titled and described
/// in the shapes that break a naive transport: wide glyphs, an emoji, quotes, and a
/// multi-line body with a quoted line, a fence and a wide attribution.
const SHORT_LINK: &str = "Qb7eLy2w";
const TITLE: &str = "修复 the retry storm 🚨 — \"backoff\" resets";
const BODY: &str = "Retries pile up after a 502.\n\n> \"backoff\" — nobody\n\n\
                    ```rust\nlet backoff = Duration::ZERO;\n```\n\n— reported by 陳大文";

/// The card's labels as the board holds them: a slashed one, a colour-only one (no name,
/// so not a label the card's handle carries) and a wide one.
const LABELS: &[&str] = &["afkd/ready", "", "Väntar på svar"];

/// The file, under the service's work dir, its `on_run` writes the card's handle fields to.
const FIELDS: &str = "fields.txt";

/// A human's question on the card before afkd ever looked, which the brief must carry.
const ASK: &str = "看起来不对 🚨 — it resets on every 401:\n\n```\nGET /1/members/me 401\n```";

/// How long a claim, a run or a finish may take. Generous, because it bounds a failure
/// rather than timing a success: the service polls every second.
const BUDGET: Duration = Duration::from_secs(30);

/// The board the service picks from: the selfdev lists, and one card up for grabs — created
/// a week ago, so no age gate holds it — with [`LABELS`] and Chen's question on it. The fake
/// and the card's id.
fn seed() -> (FakeTrello, String) {
    let fake = FakeTrello::start();
    for list in ["Up for Grabs", "In Progress", "Review"] {
        fake.list(list);
    }
    let chen = fake.member("chen", "陳大文");
    let card = fake.card("Up for Grabs", SHORT_LINK, TITLE, BODY);
    for label in LABELS {
        fake.label(&card, label);
    }
    fake.comment(&card, &chen, ASK, 1800);
    (fake, card)
}

/// The service that drives the seeded card, `home` its work dir: a v2 file importing the
/// plugin, whose slots call its actions on the card they are passed — `add_member` with its
/// `member` left to its default — and whose `on_done` comment afkd interpolates from the outcome and the
/// card's title. The run holds until the test creates `release` — bounded, so a test that
/// never does cannot wedge it — which is what lets the test see the claim and the run
/// mid-flight; then it writes every field of the card's handle to [`FIELDS`], a line each.
fn service(home: &Path, base_url: &str) -> String {
    service_drawing(home, base_url, r#"pick_from    "Up for Grabs""#)
}

/// [`service`], drawing its cards as `intake` says: the `pick_from` line, and any gate
/// written under it.
fn service_drawing(home: &Path, base_url: &str, intake: &str) -> String {
    format!(
        r#"import "core"
import "@afkd/trello"

widgets :: service(trello) {{
  board        "https://trello.com/b/{BOARD}/afkd-drift"
  base_url     "{base_url}"
  api_key      "{KEY}"
  token        "{TOKEN}"
  {intake}
  every        1s
  max_attempts 1

  on_claim(card: trello.Card) {{
    trello.add_member(card)
    trello.move_to(card, "In Progress", at=.top)
  }}
  on_done(run: core.Run, card: trello.Card, outcome: core.Outcome) {{
    trello.move_to(card, "Review", at=.top)
    trello.comment(card, "done in #{{outcome.duration}}: #{{card.title}}")
  }}

  work_dir "{home}"
  on_run(run: core.Run, card: trello.Card) {{
    $ i=0; until [ -f release ] || [ $i -ge 600 ]; do sleep 0.1; i=$((i+1)); done
    $ echo title #{{card.title}} >> {FIELDS}
    $ echo url #{{card.url}} >> {FIELDS}
    for label in card.labels {{
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

/// The credentials a config reads from the daemon's environment
/// (`env.get("TRELLO_API_KEY")`), as the README's lead example and §24 do, and §24's
/// `GH_TOKEN`.
const CONFIG_ENV: &[(&str, &str)] = &[
    ("TRELLO_API_KEY", KEY),
    ("TRELLO_TOKEN", TOKEN),
    ("GH_TOKEN", "gh-token-drift"),
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
    wait_for_within(BUDGET, home, fake, card, daemon, what, ready);
}

/// Poll `ready` to `budget`, failing with `what` and the [`dump`] when it never holds.
fn wait_for_within(
    budget: Duration,
    home: &Path,
    fake: &FakeTrello,
    card: &str,
    daemon: &StreamingDaemon,
    what: &str,
    ready: impl Fn() -> bool,
) {
    let deadline = Instant::now() + budget;
    while !ready() {
        assert!(
            Instant::now() < deadline,
            "{what} within {budget:?}\n{}",
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

/// Read `path` whole, naming it when it is missing.
fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Drive one card through the plugin, installed in `home`, on a real daemon: it is
/// **claimed** (the claim comment, and `on_claim`'s calls added the token's own member and
/// moved it), **run** (its run dir is named for the card, its task carries the card and its
/// thread whole, and `on_run` read every field of its handle as the plugin sent it) and
/// **finished** (the claim released, and `on_done`'s calls moved it and commented with its
/// title), and the daemon then drains clean.
fn drive_one_card(home: &Path) {
    let (fake, card) = seed();
    write_config(home, &service(home, fake.base_url()));
    let (code, report) = validate(home);
    assert_eq!(code, Some(0), "the service validates:\n{report}");

    let daemon = spawn_headless_streaming(home, &[]);

    wait_for(home, &fake, &card, &daemon, "the card is claimed", || {
        claimed(&fake, &card) && fake.list_of(&card) == "In Progress" && fake.members(&card) == [ME]
    });
    // The run dir is made before the spine lays the unit's files into it, so the wait is
    // for a written brief, not the bare dir.
    let run_suffix = format!("-unit-{SHORT_LINK}-1");
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
        fake.comments(&card).iter().any(|c| c.author == me
            && c.text.starts_with("done in ")
            && c.text.ends_with(&format!(": {TITLE}"))
            && !c.text.contains("#{")),
        "on_done's comment landed with its run fact and the title filled in:\n{}",
        dump(home, &fake, &card, &daemon)
    );
    // Every field verbatim — the `$` lines shell-quote what they interpolate — and the
    // labels by name, the colour-only one left out.
    assert_eq!(
        read(&home.join(FIELDS)),
        format!(
            "title {TITLE}\nurl {}\nlabel afkd/ready\nlabel Väntar på svar\n",
            fake.url(&card)
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

/// The two lists [`claim_from_the_second_list`] draws from, in the order its `pick_from`
/// names them.
const FIRST_LIST: &str = "Bakgrundsvideor";
const SECOND_LIST: &str = "Hjälpvideor 🎬";

/// A service whose `pick_from` names two lists, on a real daemon: the first list's only
/// card carries the label `without_label` holds back, so the card in the second list is
/// the one **claimed**, `on_claim` moves it on, and once its run is released it is
/// **finished** into Review; the held card is never claimed, and the daemon drains clean.
fn claim_from_the_second_list(home: &Path) {
    let fake = FakeTrello::start();
    for list in [FIRST_LIST, SECOND_LIST, "In Progress", "Review"] {
        fake.list(list);
    }
    let held = fake.card(
        FIRST_LIST,
        "H3ldB4ck",
        "Bakgrund: stadsvy i skymning — väntar på klipp",
        "",
    );
    fake.label(&held, "afkd/hold");
    let card = fake.card(SECOND_LIST, SHORT_LINK, TITLE, BODY);
    write_config(
        home,
        &service_drawing(
            home,
            fake.base_url(),
            &format!(
                "pick_from     \"{FIRST_LIST}\", \"{SECOND_LIST}\"\n  without_label \"afkd/hold\""
            ),
        ),
    );
    let (code, report) = validate(home);
    assert_eq!(code, Some(0), "the two-list service validates:\n{report}");

    let daemon = spawn_headless_streaming(home, &[]);
    wait_for(
        home,
        &fake,
        &card,
        &daemon,
        "the second list's card is claimed",
        || claimed(&fake, &card) && fake.list_of(&card) == "In Progress",
    );
    std::fs::write(home.join("release"), "").expect("release the run");
    wait_for(home, &fake, &card, &daemon, "the card is finished", || {
        fake.list_of(&card) == "Review" && !claimed(&fake, &card)
    });
    assert!(
        fake.comments(&held).is_empty() && fake.list_of(&held) == FIRST_LIST,
        "the held card is never claimed:\n{}",
        dump(home, &fake, &held, &daemon)
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

/// The selfdev service's failure path, cut down: two attempts, and the live config's
/// `on_fail` — to the bottom of Backlog, with the Problem label. Each attempt holds until
/// the test creates `release`, then fails.
fn failing_service(home: &Path, base_url: &str) -> String {
    format!(
        r#"import "core"
import "@afkd/trello"

widgets :: service(trello) {{
  board        "https://trello.com/b/{BOARD}/afkd-drift"
  base_url     "{base_url}"
  api_key      "{KEY}"
  token        "{TOKEN}"
  pick_from    "Up for Grabs"
  every        1s
  max_attempts 2

  on_claim(card: trello.Card) {{
    trello.move_to(card, "In Progress", at=.top)
  }}
  on_fail(run: core.Run, card: trello.Card, outcome: core.Outcome) {{
    trello.move_to(card, "Backlog", at=.bottom)
    trello.add_label(card, "Problem")
  }}

  work_dir "{home}"
  on_run(run: core.Run, card: trello.Card) {{
    $ i=0; until [ -f release ] || [ $i -ge 600 ]; do sleep 0.1; i=$((i+1)); done
    fail("{OUTAGE_FAULT}")
  }}
}}
"#,
        home = home.display()
    )
}

/// The 2026-10-01 incident, replayed on a real daemon: Trello goes out of reach once the
/// card is claimed and stays out through both failed attempts, the finish and every action
/// `on_fail` calls, and past the first retry. The plugin queues each write it cannot
/// deliver, and once Trello is back
/// the card ends in its failed state — both attempt notes and the watermark on it, in
/// Backlog with the Problem label, its claim released — with nothing given up.
fn fail_one_card_through_an_outage(home: &Path) {
    let (fake, card) = seed();
    fake.list("Backlog");
    write_config(home, &failing_service(home, fake.base_url()));
    let (code, report) = validate(home);
    assert_eq!(code, Some(0), "the service validates:\n{report}");

    let daemon = spawn_headless_streaming(home, &[]);
    wait_for(home, &fake, &card, &daemon, "the card is claimed", || {
        claimed(&fake, &card) && fake.list_of(&card) == "In Progress"
    });

    let outage_start = Instant::now();
    fake.outage();
    std::fs::write(home.join("release"), "").expect("release the run");
    // `on_fail`'s last action, queued behind everything before it: the attempt notes, the
    // watermark and the move. Seeing it means the slot ran to its end during the outage.
    let queued_last = "adding the label \"Problem\" is queued behind them";
    wait_for(
        home,
        &fake,
        &card,
        &daemon,
        "on_fail runs through the outage",
        || daemon.stderr_so_far().contains(queued_last),
    );
    // And the outage outlasts the first retry, 5 s after the first failure.
    wait_for(
        home,
        &fake,
        &card,
        &daemon,
        "a retry fails in the outage",
        || {
            daemon
                .stderr_so_far()
                .contains("is tried again in 10s (try 3)")
        },
    );
    fake.restore();
    let outage_for = outage_start.elapsed();
    assert!(!fake.dropped().is_empty(), "the outage dropped requests");

    // The head write (attempt note 1, queued as the outage began) is next tried at most
    // the outage's length plus the first gap after it failed — the gap doubles from 5 s
    // and its tries fall at +5, +15, +35 s… — and one pass then lands the card's whole
    // backlog.
    let me = fake.me();
    let notes = || -> Vec<String> {
        fake.comments(&card)
            .into_iter()
            .filter(|c| c.author == me && c.text.starts_with("[afkd-attempt]"))
            .map(|c| c.text)
            .collect()
    };
    let ran = || {
        fake.comments(&card)
            .iter()
            .filter(|c| c.author == me && c.text.starts_with("[afkd-ran]"))
            .count()
    };
    wait_for_within(
        outage_for + Duration::from_secs(5) + BUDGET,
        home,
        &fake,
        &card,
        &daemon,
        "the card reaches its failed state",
        || {
            fake.list_of(&card) == "Backlog"
                && fake.labels(&card).contains(&"Problem".to_string())
                && notes().len() == 2
                && ran() == 1
                && !claimed(&fake, &card)
        },
    );
    let notes = notes();
    assert!(
        notes[0].starts_with("[afkd-attempt] 1/2: ") && notes[0].contains(OUTAGE_FAULT),
        "{notes:?}"
    );
    assert!(
        notes[1].starts_with("[afkd-attempt] 2/2: ") && notes[1].contains(OUTAGE_FAULT),
        "{notes:?}"
    );
    let stderr = daemon.stderr_so_far();
    assert!(
        !stderr.contains("gave up") && !stderr.contains("giving up"),
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
fn install_leg_places_the_plugin_and_runs_one_card_through_it() {
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
    let exec = placed.join("afkd-trello");
    let mode = std::fs::metadata(&exec)
        .unwrap_or_else(|e| panic!("the install placed {}: {e}", exec.display()))
        .permissions()
        .mode();
    assert!(mode & 0o111 != 0, "{} is executable", exec.display());
    drive_one_card(home.path());
}

#[test]
fn two_lists_leg_claims_a_card_from_the_second_list() {
    let home = TempDir::new().expect("tempdir");
    let stage_dir = TempDir::new().expect("tempdir");
    install(home.path(), &stage(stage_dir.path()));
    claim_from_the_second_list(home.path());
}

#[test]
fn outage_leg_a_failed_card_still_reaches_backlog_with_its_attempt_notes() {
    let home = TempDir::new().expect("tempdir");
    let stage_dir = TempDir::new().expect("tempdir");
    install(home.path(), &stage(stage_dir.path()));
    fail_one_card_through_an_outage(home.path());
}

// --- files on the card ---------------------------------------------------------------

/// The service the attachments leg drives, `home` its work dir: `on_claim` fetches what an
/// earlier attempt left on the card — its plan and its patch — into `home`, failing on any
/// error, and `on_run`, once the test creates `release`, attaches the patch it finds in
/// `home`, failing if the plugin refuses it.
fn attaching_service(home: &Path, base_url: &str) -> String {
    format!(
        r#"import "core"
import "@afkd/trello"

widgets :: service(trello) {{
  board        "https://trello.com/b/{BOARD}/afkd-drift"
  base_url     "{base_url}"
  api_key      "{KEY}"
  token        "{TOKEN}"
  pick_from    "Up for Grabs"
  every        1s
  max_attempts 1

  on_claim(card: trello.Card) {{
    plan := trello.fetch_attachment(card, "plan.md", "{home}/earlier-plan.md")
    if plan != nil {{ fail("trello.fetch_attachment: #{{plan.message}}") }}
    patch := trello.fetch_attachment(card, "work.patch", "{home}/earlier.patch")
    if patch != nil {{ fail("trello.fetch_attachment: #{{patch.message}}") }}
    trello.move_to(card, "In Progress", at=.top)
  }}
  on_done(run: core.Run, card: trello.Card, outcome: core.Outcome) {{
    trello.move_to(card, "Review", at=.top)
  }}

  work_dir "{home}"
  on_run(run: core.Run, card: trello.Card) {{
    $ i=0; until [ -f release ] || [ $i -ge 600 ]; do sleep 0.1; i=$((i+1)); done
    attached := trello.attach(card, "{home}/work.patch", "work.patch")
    if attached != nil {{ fail("trello.attach: #{{attached.message}}") }}
  }}
}}
"#,
        home = home.display()
    )
}

/// A patch as a run leaves one: 100 kB of diff text whose hunks carry Latin-1 bytes, CRLF
/// line ends and NULs — not valid UTF-8, so nothing on the way may treat it as text.
fn synthetic_patch() -> Vec<u8> {
    let mut patch =
        b"diff --git a/notes.txt b/notes.txt\n--- a/notes.txt\n+++ b/notes.txt\n".to_vec();
    let mut n = 0u32;
    while patch.len() < 100_000 {
        patch.extend_from_slice(format!("@@ -{n},1 +{n},1 @@\r\n-r").as_bytes());
        patch.extend_from_slice(b"\xe4d\r\n+r\xe4ksm\xf6rg\xe5s \0\xff\r\n");
        n += 1;
    }
    assert!(
        std::str::from_utf8(&patch).is_err(),
        "the patch is not UTF-8"
    );
    patch
}

/// A card an earlier attempt left two plans on, on a real daemon: `on_claim` writes the
/// newer plan to the work dir and, with no patch on the card, writes none; once released,
/// `on_run` attaches the patch it is handed, and the card ends holding it byte for byte
/// beside the two plans, finished into Review, and the daemon drains clean.
fn fetch_on_claim_and_attach_on_run(home: &Path) {
    let (fake, card) = seed();
    let newer = "## 計画 — attempt 2\n\n1. Läs kortet 🚧\n2. Bygg.\n";
    fake.attachment(&card, "plan.md", b"## attempt 1\n", 3600);
    fake.attachment(&card, "plan.md", newer.as_bytes(), 1800);
    write_config(home, &attaching_service(home, fake.base_url()));
    let (code, report) = validate(home);
    assert_eq!(code, Some(0), "the attaching service validates:\n{report}");

    let daemon = spawn_headless_streaming(home, &[]);
    wait_for(home, &fake, &card, &daemon, "the card is claimed", || {
        claimed(&fake, &card) && fake.list_of(&card) == "In Progress"
    });
    assert_eq!(
        std::fs::read_to_string(home.join("earlier-plan.md"))
            .ok()
            .as_deref(),
        Some(newer),
        "on_claim fetched the newer plan:\n{}",
        dump(home, &fake, &card, &daemon)
    );
    assert!(
        !home.join("earlier.patch").exists(),
        "no patch on the card, so none written"
    );

    let patch = synthetic_patch();
    std::fs::write(home.join("work.patch"), &patch).expect("write the patch");
    std::fs::write(home.join("release"), "").expect("release the run");
    wait_for(home, &fake, &card, &daemon, "the card is finished", || {
        fake.list_of(&card) == "Review" && !claimed(&fake, &card)
    });
    let files = fake.attachments(&card);
    let names: Vec<&str> = files.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names, ["plan.md", "plan.md", "work.patch"]);
    assert!(
        files[2].1 == patch,
        "the patch landed byte for byte:\n{}",
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

#[test]
fn attachments_leg_on_claim_fetches_the_newest_and_on_run_attaches() {
    let home = TempDir::new().expect("tempdir");
    let stage_dir = TempDir::new().expect("tempdir");
    install(home.path(), &stage(stage_dir.path()));
    fetch_on_claim_and_attach_on_run(home.path());
}

// --- config language v2's complete example -------------------------------------------

/// §24's entry file.
const SECTION_24_DAEMON: &str = r#"// daemon.afkd
import "selfdev"
"#;

/// §24's shared queues.
const SECTION_24_QUEUES: &str = r#"// shared/queues.afkd
develop :: queue { capacity 1 }
discuss :: queue { capacity 2 }
"#;

/// §24's `selfdev/selfdev.afkd`, copied from `docs/lang-v2.md` at afkd master, up to the
/// `develop` service's end. The `discuss` half after it is left out: it writes
/// `discuss_with anyone`, a bare word, where the kind's `discuss_with` is a
/// `list[string]` and is written `[ "anyone" ]`.
const SECTION_24_SELFDEV: &str = r##"// selfdev/selfdev.afkd - Trello-driven pipeline: afkd develops itself.
// Phases prep -> plan -> implement -> commit, each gated by a critic `.ok` with retries;
// on exhaustion the card returns to Backlog. Markers live in the run's scratch dir.

import "core"
import "core:env"
import "core:fs"
import "core:git"
import "vendor:claude"
import "shared"
import "@afkd/trello"

HOME       :: env.get("HOME") ?? "/root"
PROMPTS    :: "docs/agents"
REPO       :: "/home/user/Projects/afkd"
PLUGINS    :: "/home/user/Projects/afkd-plugins"
PNPM_STORE :: "/home/user/Projects/.pnpm-store"

TRELLO_BOARD   :: "https://trello.com/b/BOARDID/afkd"
TRELLO_API_KEY :: env.get("TRELLO_API_KEY") ?? ""
TRELLO_TOKEN   :: env.get("TRELLO_TOKEN") ?? ""

// gh's keyring is unreachable inside the sandbox, so the token travels by env.
GH_TOKEN :: env.get("GH_TOKEN") ?? ""

// Git commands the review-only agents may never run.
GIT_WRITES :: [
  "Bash(git checkout *)",
  "Bash(git clean *)",
  "Bash(git commit *)",
  "Bash(git push *)",
  "Bash(git reset *)",
]

// Plans, implements, commits. Unattended, so bypass_permissions.
builder :: agent(claude) {
  timeout         4h
  model           "opus"
  effort          high
  permission_mode bypass_permissions
  skills          "@afkd/trello/trello"
  tools           "Bash", "Edit", "Glob", "Grep", "Read", "WebFetch", "WebSearch", "Write"
}

// Gates each phase, review-only. Commits stay denied even under bypass_permissions.
critic :: agent(claude) {
  timeout         1h
  model           "opus"
  effort          high
  permission_mode bypass_permissions
  skills          "@afkd/trello/trello"
  tools           "Bash", "Glob", "Grep", "Read", "WebFetch", "WebSearch", "Write"
  deny            GIT_WRITES
}

// Confine the pipeline to the repo plus its Rust/claude toolchain.
// The run's scratch dir is bound automatically.
repo_jail :: sandbox {
  network host

  read_write REPO,
             PLUGINS,
             "#{HOME}/.config/afkd/",
             "#{HOME}/.cargo/",
             "#{HOME}/.cache/pnpm/",
             "#{HOME}/.claude/",
             "#{HOME}/.claude.json",
             "#{PNPM_STORE}/"

  // NOTE: granting ~/.ssh exposes private keys to the agent.
  read_only "/etc/ca-certificates/",
            "/etc/group",
            "/etc/hosts",
            "/etc/nsswitch.conf",
            "/etc/passwd",
            "/etc/resolv.conf",
            "/etc/ssl/",
            "/usr/",
            "#{HOME}/.gitconfig",
            "#{HOME}/.local/bin/",
            "#{HOME}/.local/share/claude/",
            "#{HOME}/.local/share/nvm/",
            "#{HOME}/.rustup/",
            "#{HOME}/.ssh/"
}

// Fresh marker state each run, and a tree that starts where master is.
prep :: proc() {
  fetched := git.fetch("origin", "master")
  if !fetched.ok { fail("git fetch: #{fetched.error}") }
  $ git reset --hard
  $ git checkout -B master origin/master
  $ rm -f $AFKD_SCRATCH_DIR/plan.md $AFKD_SCRATCH_DIR/*.ok \
      $AFKD_SCRATCH_DIR/*.blocked $AFKD_SCRATCH_DIR/*-feedback.md
}

// How a gated phase ended.
Verdict :: enum { blocked, approved, rejected }

// A prompt file's text; a missing or unreadable one fails the run.
prompt :: proc(name: string) -> string {
  text, err := fs.read_file("#{PROMPTS}/#{name}")
  if err != nil { fail("cannot read #{err.path}: #{err.message}") }
  return text
}

// Build, then critique, up to three times.
gate :: proc(run: core.Run, phase: string, build: string, critique: string) -> Verdict {
  for _ in 0..<3 {
    built := builder <- prompt(build)
    if !built.ok { fail("builder: #{built.error}") }
    if fs.is_file("#{run.scratch_dir}/#{phase}.blocked") { return .blocked }

    critiqued := critic <- prompt(critique)
    if !critiqued.ok { fail("critic: #{critiqued.error}") }
    if fs.is_file("#{run.scratch_dir}/#{phase}.ok") { return .approved }
  }
  return .rejected
}

plan :: proc(run: core.Run) {
  verdict := gate(run, "plan", "20-plan-build.md", "21-plan-critique.md")
  if verdict == .blocked {
    fail("card unimplementable as written - see card comment")
  } else if verdict == .rejected {
    fail("plan not approved within retries")
  }
}

implement :: proc(run: core.Run) {
  verdict := gate(run, "impl", "30-implement-build.md", "31-implement-critique.md")
  if verdict == .blocked {
    fail("implementation blocked - see card comment")
  } else if verdict == .rejected {
    fail("implementation not approved within retries")
  }
}

// Commit the approved work, then check the fact rather than the agent's word.
commit :: proc(run: core.Run) {
  committed := builder <- prompt("40-commit.md")
  if !committed.ok { fail("builder: #{committed.error}") }
  if fs.is_file("#{run.scratch_dir}/commit.blocked") {
    fail("commit blocked: approval stale, real change needed - see card comment")
  }

  fetched := git.fetch("origin", "master")
  if !fetched.ok { fail("git fetch: #{fetched.error}") }
  merged, err := git.is_merged("HEAD", into="origin/master")
  if err != nil { fail("cannot tell whether HEAD reached origin/master: #{err.message}") }
  if !merged { fail("the commit never reached origin/master - the push did not land") }
}

task :: proc(run: core.Run) {
  prep()
  plan(run)
  implement(run)
  commit(run)
}

develop :: service(trello) {
  board        TRELLO_BOARD
  api_key      TRELLO_API_KEY
  token        TRELLO_TOKEN
  pick_from    "Up for Grabs"
  min_age      1m
  max_attempts 2
  every        1m~3m

  on_claim(card: trello.Card) {
    added := trello.add_member(card)
    if added != nil { fail("trello.add_member: #{added.message}") }
    moved := trello.move_to(card, "In Progress", at=.top)
    if moved != nil { fail("trello.move_to: #{moved.message}") }
  }
  on_done(run: core.Run, card: trello.Card, outcome: core.Outcome) {
    moved := trello.move_to(card, "Review", at=.top)
    if moved != nil { fail("trello.move_to: #{moved.message}") }
    commented := trello.comment(card, "afkd landed this card in #{outcome.duration}.")
    if commented != nil { fail("trello.comment: #{commented.message}") }
  }
  on_park(run: core.Run, card: trello.Card, outcome: core.Outcome) {
    commented := trello.comment(card, "parked after #{outcome.duration}: waiting for a reply.")
    if commented != nil { fail("trello.comment: #{commented.message}") }
  }
  on_fail(run: core.Run, card: trello.Card, outcome: core.Outcome) {
    moved := trello.move_to(card, "Backlog", at=.bottom)
    if moved != nil { fail("trello.move_to: #{moved.message}") }
    labelled := trello.add_label(card, "Problem")
    if labelled != nil { fail("trello.add_label: #{labelled.message}") }
  }

  work_dir    REPO
  sandbox     repo_jail
  queue       shared.develop
  description "Builds a card promoted to Up for Grabs through prep, critic-gated planning and implementation, then commit and push"

  env {
    GH_TOKEN:              GH_TOKEN,
    PNPM_CONFIG_STORE_DIR: PNPM_STORE,
  }

  on_run(run: core.Run, card: trello.Card) { task(run) }
}
"##;

/// The selfdev pipeline of config language v2's §24, `develop` service and all, validates
/// against the installed plugin: every setting it writes is one the manifest declares, of
/// its type, and every action its slots call is one the plugin provides, on the card its
/// slot is passed.
#[test]
fn the_lang_v2_complete_example_develop_service_validates() {
    let home = TempDir::new().expect("tempdir");
    let stage_dir = TempDir::new().expect("tempdir");
    install(home.path(), &stage(stage_dir.path()));
    let config = config_dir(home.path());
    for (file, text) in [
        ("daemon.afkd", SECTION_24_DAEMON),
        ("shared/queues.afkd", SECTION_24_QUEUES),
        ("selfdev/selfdev.afkd", SECTION_24_SELFDEV),
    ] {
        let path = config.join(file);
        std::fs::create_dir_all(path.parent().expect("has parent")).expect("mk the package");
        std::fs::write(&path, text).expect("write the file");
    }
    let (code, report) = validate(home.path());
    assert_eq!(code, Some(0), "§24 validates:\n{report}");
    assert!(
        report.contains("selfdev/selfdev.afkd") && !report.contains("warning:"),
        "every file checked, cleanly:\n{report}"
    );
}

/// An action acts on the card it is passed, so one called without it — the pre-handle
/// spelling, `trello.move_to("Review")` — is a load error naming the handle type, not a
/// call on an implied card.
#[test]
fn a_call_with_the_wrong_handle_is_a_load_error() {
    let home = TempDir::new().expect("tempdir");
    let stage_dir = TempDir::new().expect("tempdir");
    install(home.path(), &stage(stage_dir.path()));
    let config = |call: &str| {
        format!(
            r#"import "core"
import "@afkd/trello"

widgets :: service(trello) {{
  board     "https://trello.com/b/BOARDID/afkd"
  api_key   "REPLACE_ME"
  token     "REPLACE_ME"
  pick_from "Up for Grabs"

  on_done(run: core.Run, card: trello.Card, outcome: core.Outcome) {{ {call} }}

  work_dir "/srv/acme/widgets"
  on_run(run: core.Run, card: trello.Card) {{
    $ true
  }}
}}
"#
        )
    };
    write_config(home.path(), &config(r#"trello.move_to("Review")"#));
    let (code, report) = validate(home.path());
    assert_eq!(
        code,
        Some(1),
        "a call without its card is refused:\n{report}"
    );
    assert!(
        report.contains("trello.Card"),
        "the refusal names the handle type:\n{report}"
    );
    // The same file with the card passed is valid: the handle is all that was wrong.
    write_config(home.path(), &config(r#"trello.move_to(card, "Review")"#));
    let (code, report) = validate(home.path());
    assert_eq!(code, Some(0), "the call with its card validates:\n{report}");
}

/// `pick_from` is required: a service that names no list to claim from is refused at
/// validate, naming the setting and the service, as a missing `api_key` is. The same
/// service naming one validates.
#[test]
fn a_service_without_pick_from_fails_validate_naming_the_setting() {
    let home = TempDir::new().expect("tempdir");
    let stage_dir = TempDir::new().expect("tempdir");
    install(home.path(), &stage(stage_dir.path()));
    write_config(
        home.path(),
        &service_drawing(
            home.path(),
            "http://127.0.0.1:9",
            "without_label \"afkd/hold\"",
        ),
    );
    let (code, report) = validate(home.path());
    assert_eq!(
        code,
        Some(1),
        "a service without pick_from is refused:\n{report}"
    );
    assert!(
        report.contains(r#"missing required setting "pick_from" for service(trello)"#),
        "the refusal names the setting and the service:\n{report}"
    );
    write_config(home.path(), &service(home.path(), "http://127.0.0.1:9"));
    let (code, report) = validate(home.path());
    assert_eq!(code, Some(0), "the service naming one validates:\n{report}");
}

/// The claim key afkd renamed to `every`, spelled in halves so a `git grep` for it holds
/// this repo to writing it nowhere else.
const OLD_CADENCE: &str = concat!("poll", "_interval");

/// A service whose claim cadence is written `{key} 1m~3m`, the line a group of its own, so
/// the file is as `afkd fmt` prints it under either key.
fn cadence_service(key: &str) -> String {
    format!(
        r#"import "core"
import "@afkd/trello"

widgets :: service(trello) {{
  board     "https://trello.com/b/BOARDID/afkd"
  api_key   "REPLACE_ME"
  token     "REPLACE_ME"
  pick_from "Up for Grabs"

  {key} 1m~3m

  work_dir "/srv/acme/widgets"

  on_run proc(run: core.Run, card: trello.Card) {{
    $ true
  }}
}}
"#
    )
}

/// The claim cadence is afkd's own `every`, and the manifest declares no [`OLD_CADENCE`]
/// beside it: a service writing it with `1m~3m` is refused at validate, naming the
/// setting, the service and the key to write instead, and the same service writing
/// `every 1m~3m` validates.
#[test]
fn the_old_cadence_key_is_refused_naming_every_and_every_validates() {
    let home = TempDir::new().expect("tempdir");
    let stage_dir = TempDir::new().expect("tempdir");
    install(home.path(), &stage(stage_dir.path()));
    write_config(home.path(), &cadence_service(OLD_CADENCE));
    let (code, report) = validate(home.path());
    assert_eq!(code, Some(1), "{OLD_CADENCE} is refused:\n{report}");
    for said in [
        format!(r#"unknown setting "{OLD_CADENCE}" for service(trello)"#),
        r#"did you mean "every"?"#.to_owned(),
    ] {
        assert!(report.contains(&said), "the refusal says {said}:\n{report}");
    }
    write_config(home.path(), &cadence_service("every"));
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
