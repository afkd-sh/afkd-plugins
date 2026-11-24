//! The gate that holds the **shipped `@afkd/trello` plugin** — a manifest v2 provider whose
//! main kind is `service(trello)`, shipped as a finished binary — to a real afkd: the
//! `afkd` first on `PATH`, the installed one and never a build, since a plugin is checked
//! against the afkd it will meet ([`bin_path`]).
//!
//! - The **install** leg installs the tree the release tarball holds and runs one card
//!   through it end to end on a real daemon: a `service(trello)` against the fake claims the
//!   card from its `claim_from` list; its `on_claim` reads the board with `trello.cards` and
//!   faults the run unless the card it was passed is `==` the card read; its `on_run` reads
//!   the card's comments with `trello.comments(account, card)`, writes one with
//!   `trello.create_comment(account, card, …)` and reads every field the card's handle
//!   declares; its `on_done` moves the card with `trello.update_card(account, card, list=…)`.
//!   The comment landed, the card moved and the claim was released.
//! - The **two lists** leg claims, on a real daemon, a card from the second of the two
//!   lists its `claim_from` names, past a held card in the first.
//! - The **outage** leg fails a card through both its attempts while Trello is out of
//!   reach: `on_fail`'s writes come back `.err`, the notes the plugin could not post are said
//!   lost and never queued, the finish is held, and once Trello is back afkd's `release`
//!   frees the claim and the card is left in its list.
//! - The **attachments** leg downloads, in `on_claim`, the newest of two plans an earlier
//!   attempt left on the card with `trello.attachments` and `trello.download_attachment`, and
//!   finds no patch; its `on_run` then uploads one with `trello.create_attachment`, and the
//!   card holds it byte for byte.
//! - The **handle** leg holds `afkd validate` to refusing a call made without its card.
//! - The **required setting** leg holds `afkd validate` to refusing a service that names no
//!   `claim_from`.
//! - The **kind** leg holds `afkd validate` to taking a service written `service(trello)` and
//!   to refusing, as unknown, each setting the kind was renamed from.
//! - The **cadence** leg holds `afkd validate` to refusing a service that writes the claim
//!   key afkd renamed to `every`, pointing it at `every`, and to taking the same service
//!   with `every`.
//! - The **README** leg `afkd validate`s every `conf` fence the plugin's README carries,
//!   each a whole entry file, against the installed plugin, with every variable the fences
//!   read set by [`CONFIG_ENV`].
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
/// the skill and the two licence texts, exactly what the release archive holds — and return
/// the staged root.
fn stage(dst: &Path) -> PathBuf {
    let status = Command::new(plugin_root().join("../../tools/pack-release"))
        .arg("trello")
        .arg(dst)
        .arg(built_binary())
        .status()
        .expect("run tools/pack-release");
    assert!(status.success(), "tools/pack-release trello");
    let staged = dst.join("afkd-trello");
    for present in [
        "afkd-plugin.toml",
        "LICENSE-MIT",
        "LICENSE-APACHE",
        "afkd-trello",
        "skills/trello/SKILL.md",
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

/// The card the service claims, its short link in Trello's own shape, named and described
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

/// The file its `on_run` writes the card's description to, whole: a field of many lines, with
/// quotes and a fence in it.
const DESCRIPTION: &str = "description.txt";

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
    // A label a slot may put on a card: no write makes one the board lacks.
    fake.board_label(BOARD, "Problem", Some("red"));
    let chen = fake.member("chen", "陳大文");
    let card = fake.card("Up for Grabs", SHORT_LINK, TITLE, BODY);
    for label in LABELS {
        fake.label(&card, label);
    }
    fake.comment(&card, &chen, ASK, 1800);
    (fake, card)
}

/// The service that drives the seeded card, `home` its work dir: a v2 file importing the
/// plugin, whose slots reach Trello through the `trello.Account` they are passed — `on_claim`
/// puts the token's own member on the card (`add_member` with its `member` left to its default)
/// and moves it, `on_done` moves it to Review. The run holds until the test creates `release`
/// — bounded, so a test that never does cannot wedge it — which is what lets the test see the
/// claim and the run mid-flight; then it reads the card's comments with `trello.comments` and
/// writes one saying how many it read with `trello.create_comment`, writes every field of the
/// card's handle to [`FIELDS`], a line each, and the description to [`DESCRIPTION`]. Its
/// `on_claim` first reads the board's cards of the list with `trello.cards` and faults the run
/// unless the card it was passed is `==` the one card read ([`claim_check`]) — the claimed card
/// and the card read are one handle to a real afkd.
fn service(home: &Path, base_url: &str) -> String {
    service_with(
        home,
        base_url,
        r#"claim_from   "Up for Grabs""#,
        &claim_check(),
    )
}

/// The statements an `on_claim` runs first to hold the claimed card to the card `trello.cards`
/// reads: the board's cards of `Up for Grabs`, read through the `account` the slot was passed
/// — which reaches the fake only if the plugin built it from the service's settings — are
/// the one card, and it is `==` the card the slot was passed. Written before any move, so the
/// card is read where the claim found it.
fn claim_check() -> String {
    format!(
        r#"    match trello.cards(account, "https://trello.com/b/{BOARD}/afkd-drift", list=[ "Up for Grabs" ]) {{
      .ok(cards) => {{
        if len(cards) != 1 {{
          fail("trello.cards read #{{len(cards)}} cards of Up for Grabs, not the one claimed")
        }}
        for found in cards {{
          if found != card {{
            fail("the card trello.cards read is not the card claimed: #{{found.id}} and #{{card.id}}")
          }}
        }}
      }}
      .err(e) => fail("trello.cards: #{{e.message}}")
    }}
"#
    )
}

/// [`service`] without the claim check, drawing its cards as `intake` says: the `claim_from`
/// line, and any gate written under it.
fn service_drawing(home: &Path, base_url: &str, intake: &str) -> String {
    service_with(home, base_url, intake, "")
}

/// A service drawing its cards as `intake` says, its `on_claim` running `check` before it
/// adds the token's member and moves the card.
fn service_with(home: &Path, base_url: &str, intake: &str, check: &str) -> String {
    format!(
        r#"import "afkd"
import "@afkd/trello"

widgets :: service(trello) {{
  board        "https://trello.com/b/{BOARD}/afkd-drift"
  base_url     "{base_url}"
  api_key      "{KEY}"
  token        "{TOKEN}"
  {intake}
  every        1s
  max_attempts 1

  on_claim proc(account: trello.Account, card: trello.Card) {{
{check}    match trello.add_member(account, card) {{
      .ok(_) => {{ }}
      .err(e) => fail("trello.add_member: #{{e.message}}")
    }}
    match trello.update_card(account, card, list="In Progress", at=.top) {{
      .ok(_) => {{ }}
      .err(e) => fail("trello.update_card: #{{e.message}}")
    }}
  }}
  on_done proc(run: afkd.Run, account: trello.Account, card: trello.Card, outcome: afkd.Outcome) {{
    match trello.update_card(account, card, list="Review", at=.top) {{
      .ok(_) => {{ }}
      .err(e) => fail("trello.update_card: #{{e.message}}")
    }}
  }}

  work_dir "{home}"
  on_run proc(run: afkd.Run, account: trello.Account, card: trello.Card) {{
    $ i=0; until [ -f release ] || [ $i -ge 600 ]; do sleep 0.1; i=$((i+1)); done
    match trello.comments(account, card) {{
      .ok(thread) => {{
        match trello.create_comment(account, card, "saw #{{len(thread)}} comments on #{{card.name}}") {{
          .ok(_) => {{ }}
          .err(e) => fail("trello.create_comment: #{{e.message}}")
        }}
      }}
      .err(e) => fail("trello.comments: #{{e.message}}")
    }}
    $ echo number #{{card.number}} >> {FIELDS}
    $ echo name #{{card.name}} >> {FIELDS}
    $ echo url #{{card.url}} >> {FIELDS}
    $ echo board #{{card.board}} >> {FIELDS}
    $ echo list #{{card.list}} >> {FIELDS}
    $ printf %s #{{card.description}} > {DESCRIPTION}
    for label in card.labels {{
      $ echo label #{{label}} >> {FIELDS}
    }}
    for member in card.members {{
      $ echo member #{{member}} >> {FIELDS}
    }}
    if card.due != nil {{
      $ echo due set >> {FIELDS}
    }}
    if card.due_complete {{
      $ echo due_complete yes >> {FIELDS}
    }}
    if card.archived {{
      $ echo archived yes >> {FIELDS}
    }}
    $ echo created_at #{{card.created_at}} >> {FIELDS}
    $ echo last_activity_at #{{card.last_activity_at}} >> {FIELDS}
    $ cat $AFKD_SCRATCH_DIR/task/README.md
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

/// The credentials a config reads from the daemon's environment
/// (`env.get("TRELLO_API_KEY")`), as the README's lead example does, and a second
/// account's.
const CONFIG_ENV: &[(&str, &str)] = &[
    ("TRELLO_API_KEY", KEY),
    ("TRELLO_TOKEN", TOKEN),
    // The second account the README's two-account example reads.
    ("GLOBEX_TRELLO_API_KEY", "globex-key-drift"),
    ("GLOBEX_TRELLO_TOKEN", "globex-token-drift"),
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
/// thread whole, and `on_run` read every field of its handle as the plugin sent it, read the
/// thread with `trello.comments` and wrote a comment saying how long it was) and **finished**
/// (the claim released, and `on_done`'s call moved the card to Review), and the daemon then
/// drains clean.
fn drive_one_card(home: &Path) {
    let (fake, card) = seed();
    // A member already on the card, and a due date marked complete, so the fields the run
    // reads are not all empty.
    fake.card_member(&card, &fake.member("chen", "陳大文"));
    fake.due(&card, 3 * 86_400, true);
    write_config(home, &service(home, fake.base_url()));
    let (code, report) = validate(home);
    assert_eq!(code, Some(0), "the service validates:\n{report}");

    let daemon = spawn_headless_streaming(home, &[]);

    wait_for(home, &fake, &card, &daemon, "the card is claimed", || {
        claimed(&fake, &card)
            && fake.list_of(&card) == "In Progress"
            && fake.members(&card) == ["chen", ME]
    });
    // The run dir is made before the spine lays the unit's files into it, so the wait is
    // for a written brief, not the bare dir.
    let run_suffix = format!("-unit-{SHORT_LINK}-1");
    let brief = || {
        run_dirs(home)
            .into_iter()
            .find(|dir| {
                dir.file_name()
                    .is_some_and(|name| name.to_string_lossy().ends_with(&run_suffix))
            })
            .map(|dir| dir.join("task/README.md"))
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
        assert!(
            task.contains(part),
            "task/README.md carries {part:?}:\n{task}"
        );
    }

    std::fs::write(home.join("release"), "").expect("release the run");
    wait_for(home, &fake, &card, &daemon, "the card is finished", || {
        fake.list_of(&card) == "Review" && !claimed(&fake, &card)
    });
    // `on_run` read the thread — Chen's question and afkd's own claim — and said so on the card,
    // as the token's member; the claim is released and the card moved.
    let me = fake.me();
    assert!(
        fake.comments(&card)
            .iter()
            .any(|c| c.author == me && c.text == format!("saw 2 comments on {TITLE}")),
        "on_run's comment landed with the count it read and the card's name filled in:\n{}",
        dump(home, &fake, &card, &daemon)
    );
    assert!(
        !claimed(&fake, &card),
        "the claim is released:\n{}",
        dump(home, &fake, &card, &daemon)
    );
    // The `on_claim` read the board with `trello.cards` and held the card it was passed to
    // be the one card read: had they not been `==`, it faulted and the card was never moved.
    // Every field the card's handle declares verbatim — the `$` lines shell-quote what they
    // interpolate — the card as the claim found it (still in `Up for Grabs`, and with the
    // member that was on it then), its labels by name with the colour-only one left out,
    // and each time a line with a value, which only a real afkd knows how to spell.
    let fields = read(&home.join(FIELDS));
    let (times, rest): (Vec<&str>, Vec<&str>) = fields
        .lines()
        .partition(|line| line.starts_with("created_at ") || line.starts_with("last_activity_at "));
    assert_eq!(
        rest,
        [
            "number 1".to_string(),
            format!("name {TITLE}"),
            format!("url {}", fake.url(&card)),
            format!("board {BOARD}"),
            "list Up for Grabs".to_string(),
            "label afkd/ready".to_string(),
            "label Väntar på svar".to_string(),
            "member chen".to_string(),
            "due set".to_string(),
            "due_complete yes".to_string(),
        ],
        "fields.txt:\n{fields}"
    );
    assert_eq!(times.len(), 2, "both times: {fields}");
    for (time, name) in times.iter().zip(["created_at", "last_activity_at"]) {
        assert!(
            time.strip_prefix(name)
                .is_some_and(|value| value.trim().len() > 1),
            "{name} is a line with a value: {time:?}"
        );
    }
    assert_eq!(
        read(&home.join(DESCRIPTION)),
        BODY,
        "the description, whole, as the card holds it"
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

/// The two lists [`claim_from_the_second_list`] draws from, in the order its `claim_from`
/// names them.
const FIRST_LIST: &str = "Bakgrundsvideor";
const SECOND_LIST: &str = "Hjälpvideor 🎬";

/// A service whose `claim_from` names two lists, on a real daemon: the first list's only
/// card carries the label `skip_labels` holds back, so the card in the second list is
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
                "claim_from   \"{FIRST_LIST}\", \"{SECOND_LIST}\"\n  skip_labels  \"afkd/hold\""
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

/// The file under the work dir where `on_fail` notes each write that came back `.err`.
const REFUSED: &str = "on_fail_refused";

/// What `on_fail` faults with when one of its writes lands while Trello is out of reach.
const LANDED_IN_OUTAGE: &str = "an on_fail write landed during the outage";

/// The selfdev service's failure path, cut down: two attempts, and the live config's
/// `on_fail` — to the bottom of Backlog, with the Problem label — except that it matches each
/// write: one that lands faults the slot, since Trello is out of reach when it runs, and one
/// that comes back `.err` is the answer expected, noted in a line of `REFUSED`. Each attempt holds until the test creates
/// `release`, then fails.
fn failing_service(home: &Path, base_url: &str) -> String {
    format!(
        r#"import "afkd"
import "@afkd/trello"

widgets :: service(trello) {{
  board        "https://trello.com/b/{BOARD}/afkd-drift"
  base_url     "{base_url}"
  api_key      "{KEY}"
  token        "{TOKEN}"
  claim_from   "Up for Grabs"
  every        1s
  max_attempts 2

  on_claim proc(account: trello.Account, card: trello.Card) {{
    match trello.update_card(account, card, list="In Progress", at=.top) {{
      .ok(_) => {{ }}
      .err(e) => fail("trello.update_card: #{{e.message}}")
    }}
  }}
  on_fail proc(run: afkd.Run, account: trello.Account, card: trello.Card, outcome: afkd.Outcome) {{
    match trello.update_card(account, card, list="Backlog", at=.bottom) {{
      .ok(_) => fail("{LANDED_IN_OUTAGE}")
      .err(_) => {{
        $ echo update_card >> {home}/{REFUSED}
      }}
    }}
    match trello.add_label(account, card, "Problem") {{
      .ok(_) => fail("{LANDED_IN_OUTAGE}")
      .err(_) => {{
        $ echo add_label >> {home}/{REFUSED}
      }}
    }}
  }}

  work_dir "{home}"
  on_run proc(run: afkd.Run, account: trello.Account, card: trello.Card) {{
    $ i=0; until [ -f release ] || [ $i -ge 600 ]; do sleep 0.1; i=$((i+1)); done
    fail("{OUTAGE_FAULT}")
  }}
}}
"#,
        home = home.display()
    )
}

/// The 2026-10-01 incident, replayed on a real daemon: Trello goes out of reach once the
/// card is claimed and stays out through both failed attempts, the finish and every write
/// `on_fail` makes. The plugin retries nothing: the attempt notes and the watermark it could
/// not post are said lost, naming the card, and `on_fail`'s writes come back `.err` — a write
/// that landed would fault the slot. The finish is held with the claim on the card; once
/// Trello is back afkd's `release` of its key frees the claim, and the card is left in the
/// list `on_claim` put it in, with no note, no watermark, no Problem label and nothing queued.
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

    fake.outage();
    std::fs::write(home.join("release"), "").expect("release the run");
    // Both attempts failed and the finish could not release the claim: the plugin said it
    // would hold it for afkd's later `release`. The watermark and both notes went the same way.
    wait_for(
        home,
        &fake,
        &card,
        &daemon,
        "the finish is held through the outage",
        || {
            daemon
                .stderr_so_far()
                .contains("afkd holds the claim and releases it on a later beat")
        },
    );
    // `on_fail` runs after the finish reply: Trello stays out until both of its writes have
    // come back `.err`, or one could land once it is restored.
    let refused = home.join(REFUSED);
    wait_for(
        home,
        &fake,
        &card,
        &daemon,
        "both of on_fail's writes are refused",
        || std::fs::read_to_string(&refused).is_ok_and(|seen| seen.lines().count() >= 2),
    );
    assert!(!fake.dropped().is_empty(), "the outage dropped requests");
    assert!(
        claimed(&fake, &card),
        "the claim is held, as it could not be released"
    );
    fake.restore();

    // Trello is back: afkd's `release` of the held key deletes the claim, and nothing the
    // outage swallowed is replayed.
    wait_for(
        home,
        &fake,
        &card,
        &daemon,
        "the held finish is released once Trello is back",
        || !claimed(&fake, &card),
    );
    assert_eq!(
        fake.list_of(&card),
        "In Progress",
        "the card is left in the list on_claim put it in:\n{}",
        dump(home, &fake, &card, &daemon)
    );
    assert!(
        !fake.labels(&card).iter().any(|l| l == "Problem"),
        "no Problem label: on_fail's writes were refused"
    );
    let me = fake.me();
    let ours: Vec<String> = fake
        .comments(&card)
        .into_iter()
        .filter(|c| c.author == me)
        .map(|c| c.text)
        .collect();
    assert!(
        ours.iter()
            .all(|text| !text.starts_with("[afkd-attempt]") && !text.starts_with("[afkd-ran]")),
        "the notes the outage swallowed were never retried: {ours:?}"
    );
    let stderr = daemon.stderr_so_far();
    assert!(
        stderr.contains("the comment \"[afkd-attempt] 1/2: ") && stderr.contains("is lost"),
        "the lost attempt note is said, naming the card and the comment:\n{stderr}"
    );
    assert!(
        !stderr.contains(LANDED_IN_OUTAGE) && !stderr.contains("queued"),
        "no write landed in the outage and nothing was queued:\n{stderr}"
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
fn outage_leg_a_failed_card_is_held_then_released_with_nothing_retried_or_queued() {
    let home = TempDir::new().expect("tempdir");
    let stage_dir = TempDir::new().expect("tempdir");
    install(home.path(), &stage(stage_dir.path()));
    fail_one_card_through_an_outage(home.path());
}

// --- files on the card ---------------------------------------------------------------

/// The service the attachments leg drives, `home` its work dir: `on_claim` lists the card's
/// attachments with `trello.attachments` and downloads each plan and patch an earlier attempt
/// left on it into `home` with `trello.download_attachment` — oldest first, so the newest of a
/// name is the file that stays — failing on any error, and `on_run`, once the test creates
/// `release`, uploads the patch it finds in `home` with `trello.create_attachment`, failing if
/// the plugin refuses it.
fn attaching_service(home: &Path, base_url: &str) -> String {
    format!(
        r#"import "afkd"
import "@afkd/trello"

widgets :: service(trello) {{
  board        "https://trello.com/b/{BOARD}/afkd-drift"
  base_url     "{base_url}"
  api_key      "{KEY}"
  token        "{TOKEN}"
  claim_from   "Up for Grabs"
  every        1s
  max_attempts 1

  on_claim proc(account: trello.Account, card: trello.Card) {{
    match trello.attachments(account, card) {{
      .ok(held) => {{
        for att in held {{
          if att.name == "plan.md" {{
            match trello.download_attachment(account, att, "{home}/earlier-plan.md") {{
              .ok(_) => {{ }}
              .err(e) => fail("trello.download_attachment: #{{e.message}}")
            }}
          }}
          if att.name == "work.patch" {{
            match trello.download_attachment(account, att, "{home}/earlier.patch") {{
              .ok(_) => {{ }}
              .err(e) => fail("trello.download_attachment: #{{e.message}}")
            }}
          }}
        }}
      }}
      .err(e) => fail("trello.attachments: #{{e.message}}")
    }}
    match trello.update_card(account, card, list="In Progress", at=.top) {{
      .ok(_) => {{ }}
      .err(e) => fail("trello.update_card: #{{e.message}}")
    }}
  }}
  on_done proc(run: afkd.Run, account: trello.Account, card: trello.Card, outcome: afkd.Outcome) {{
    match trello.update_card(account, card, list="Review", at=.top) {{
      .ok(_) => {{ }}
      .err(e) => fail("trello.update_card: #{{e.message}}")
    }}
  }}

  work_dir "{home}"
  on_run proc(run: afkd.Run, account: trello.Account, card: trello.Card) {{
    $ i=0; until [ -f release ] || [ $i -ge 600 ]; do sleep 0.1; i=$((i+1)); done
    match trello.create_attachment(account, card, "{home}/work.patch", name="work.patch") {{
      .ok(_) => {{ }}
      .err(e) => fail("trello.create_attachment: #{{e.message}}")
    }}
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

/// A card an earlier attempt left two plans on, on a real daemon: `on_claim` downloads both
/// plans to one path in the work dir — the newer last, so it is the one left — and, with no
/// patch on the card, downloads none; once released, `on_run` uploads the patch it is handed,
/// and the card ends holding it byte for byte beside the two plans, finished into Review, and
/// the daemon drains clean.
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
        "on_claim downloaded the newer plan:\n{}",
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
fn attachments_leg_on_claim_downloads_the_newest_and_on_run_uploads() {
    let home = TempDir::new().expect("tempdir");
    let stage_dir = TempDir::new().expect("tempdir");
    install(home.path(), &stage(stage_dir.path()));
    fetch_on_claim_and_attach_on_run(home.path());
}

// --- the handle ----------------------------------------------------------------------

/// A call acts on the card it is passed, so one made without it — `trello.add_label(account,
/// "Problem")`, the new label write with its card left out — is a load error naming the
/// handle type, not a call on an implied card.
#[test]
fn a_call_with_the_wrong_handle_is_a_load_error() {
    let home = TempDir::new().expect("tempdir");
    let stage_dir = TempDir::new().expect("tempdir");
    install(home.path(), &stage(stage_dir.path()));
    let config = |call: &str| {
        format!(
            r#"import "afkd"
import "@afkd/trello"

widgets :: service(trello) {{
  board      "https://trello.com/b/BOARDID/afkd"
  api_key    "REPLACE_ME"
  token      "REPLACE_ME"
  claim_from "Up for Grabs"

  on_done proc(run: afkd.Run, account: trello.Account, card: trello.Card, outcome: afkd.Outcome) {{ {call} }}

  work_dir "/srv/acme/widgets"
  on_run proc(run: afkd.Run, account: trello.Account, card: trello.Card) {{
    $ true
  }}
}}
"#
        )
    };
    write_config(
        home.path(),
        &config(r#"match trello.add_label(account, "Problem") { .ok(_) => { }; .err(_) => { } }"#),
    );
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
    write_config(
        home.path(),
        &config(
            r#"match trello.add_label(account, card, "Problem") { .ok(_) => { }; .err(_) => { } }"#,
        ),
    );
    let (code, report) = validate(home.path());
    assert_eq!(code, Some(0), "the call with its card validates:\n{report}");
}

/// `claim_from` is required: a service that names no list to claim from is refused at
/// validate, naming the setting and the service, as a missing `api_key` is. The same
/// service naming one validates.
#[test]
fn a_service_without_claim_from_fails_validate_naming_the_setting() {
    let home = TempDir::new().expect("tempdir");
    let stage_dir = TempDir::new().expect("tempdir");
    install(home.path(), &stage(stage_dir.path()));
    write_config(
        home.path(),
        &service_drawing(
            home.path(),
            "http://127.0.0.1:9",
            "skip_labels \"afkd/hold\"",
        ),
    );
    let (code, report) = validate(home.path());
    assert_eq!(
        code,
        Some(1),
        "a service without claim_from is refused:\n{report}"
    );
    assert!(
        report.contains(r#"missing required setting "claim_from" for service(trello)"#),
        "the refusal names the setting and the service:\n{report}"
    );
    write_config(home.path(), &service(home.path(), "http://127.0.0.1:9"));
    let (code, report) = validate(home.path());
    assert_eq!(code, Some(0), "the service naming one validates:\n{report}");
}

/// The kind is `intake`, the main one, so a service is written `service(trello)` and validates;
/// and each setting the kind was renamed from is refused as unknown beside `claim_from`, like
/// any name the manifest does not declare.
#[test]
fn the_kind_is_service_trello_and_the_renamed_settings_are_unknown() {
    let home = TempDir::new().expect("tempdir");
    let stage_dir = TempDir::new().expect("tempdir");
    install(home.path(), &stage(stage_dir.path()));
    let service = service(home.path(), "http://127.0.0.1:9");
    write_config(home.path(), &service);
    let (code, report) = validate(home.path());
    assert_eq!(code, Some(0), "service(trello) validates:\n{report}");

    for old in [
        "pick_from",
        "require_member",
        "require_label",
        "without_label",
    ] {
        write_config(
            home.path(),
            &service_drawing(
                home.path(),
                "http://127.0.0.1:9",
                &format!("claim_from \"Up for Grabs\"\n  {old} \"afkd/hold\""),
            ),
        );
        let (code, report) = validate(home.path());
        assert_eq!(code, Some(1), "{old} is refused:\n{report}");
        assert!(
            report.contains(&format!(r#"unknown setting "{old}" for service(trello)"#)),
            "the refusal names {old} as unknown:\n{report}"
        );
    }
}

/// The claim key afkd renamed to `every`, spelled in halves so a `git grep` for it holds
/// this repo to writing it nowhere else.
const OLD_CADENCE: &str = concat!("poll", "_interval");

/// A service whose claim cadence is written `{key} 1m~3m`, the line a group of its own, so
/// the file is as `afkd fmt` prints it under either key.
fn cadence_service(key: &str) -> String {
    format!(
        r#"import "afkd"
import "@afkd/trello"

widgets :: service(trello) {{
  board      "https://trello.com/b/BOARDID/afkd"
  api_key    "REPLACE_ME"
  token      "REPLACE_ME"
  claim_from "Up for Grabs"

  {key} 1m~3m

  work_dir "/srv/acme/widgets"

  on_run proc(run: afkd.Run, account: trello.Account, card: trello.Card) {{
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
