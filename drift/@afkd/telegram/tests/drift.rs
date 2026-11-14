//! The gate that holds the **shipped `@afkd/telegram` plugin** — a manifest v2 provider whose
//! main kind is `service(telegram)`, shipped as a finished binary — to a real afkd: the
//! `afkd` first on `PATH`, never a build, since a plugin is checked against the afkd it will
//! meet ([`bin_path`]).
//!
//! - The **install** leg installs the tree the release holds and runs one message through it
//!   on a real daemon: its `on_claim` reacts, its run reads every field the message's handle
//!   declares and the env the plugin hands it, replies, edits the reply it got back, sends a
//!   file, fetches the message's photo and deletes the reply, and its `on_done` reacts again;
//!   a message from a chat the service does not list starts nothing.
//! - The **per-call** leg runs an interval service that sends a text and edits the message it
//!   got back, as the bot the plugin's settings file names.
//! - The **validate** legs hold `afkd validate` to a `service(telegram)` that writes every
//!   setting and whose slots call every action with every named parameter, to refusing one
//!   without `token` or `chats`, to refusing a call of an action the plugin does not declare,
//!   and to refusing a returning action's result read as an error, which only an afkd that
//!   read the manifest's `returns` refuses.
//! - The **README** leg validates every `conf` fence of the plugin's README against the
//!   installed release: each a whole entry file, but for the one that is the plugin's
//!   settings file, which is placed as that file for the fences after it.
//! - The **skill** leg runs one message whose run replies through the installed `telegram`
//!   skill, so the env a real daemon hands a run is the env the skill's scripts read.
//!
//! The Telegram is the plugin's own loopback fake ([`fake`]), which holds every request to the
//! Bot API: each leg ends with its record of refused requests empty. No leg touches a
//! network, every wait is bounded, and every spawn is held in a [`Daemon`].

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use common::fake::{png, recorded, FakeTelegram, ELSEWHERE, GROUP, PRIVATE, TOKEN};
use common::*;
use serde_json::json;
use tempfile::TempDir;

// --- the shipped tree --------------------------------------------------------------

/// The plugin's name, as the manifest spells it and afkd places it.
const NAME: &str = "@afkd/telegram";

/// The plugin's root — the directory `afkd install` takes, whose path this crate's own
/// mirrors under `drift/`. Canonicalized, so a staged copy and a report both name one path.
fn plugin_root() -> PathBuf {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../@afkd/telegram");
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
        plugin_root().join("target/release/afkd-telegram")
    })
}

/// Stage the release `tools/pack-release` packs under `dst` — the manifest, the binary and
/// the skill, exactly what the release archive holds — and return the staged root.
fn stage(dst: &Path) -> PathBuf {
    let status = Command::new(plugin_root().join("../../tools/pack-release"))
        .arg("telegram")
        .arg(dst)
        .arg(built_binary())
        .status()
        .expect("run tools/pack-release");
    assert!(status.success(), "tools/pack-release telegram");
    let staged = dst.join("afkd-telegram");
    for present in [
        "afkd-plugin.toml",
        "afkd-telegram",
        "skills/telegram/SKILL.md",
        "skills/telegram/reply.py",
        "skills/telegram/lib/telegram_api.py",
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
        "skills/telegram/lib/__pycache__",
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

/// The message the service hands over: Anna's photo in the group, captioned in Japanese and
/// replying to Chen's question.
const MESSAGE: i64 = 4711;

/// The largest size of its photo, the one its handle lists.
const PHOTO: &str = "AgACAgQAAxkBAAISZ2cvPhoto_x";

/// The message from a chat no service lists, which must start nothing.
const STRANGER: i64 = 77;

/// What the run replies, and what it edits the reply to: wide CJK and an emoji.
const REPLY: &str = "見ました — deploying 🚀";
const EDITED: &str = "deployed ✓ — 完了";

/// The file the run sends: spaces, an em dash and wide CJK in its name.
const SHOT: &str = "dashboard — 監視.png";

/// The files, under the service's work dir, the slots and the run write.
const FIELDS: &str = "fields.txt";
const ENV: &str = "env.txt";
const CLAIMED: &str = "claimed.txt";
const DONE: &str = "done.txt";
const FETCHED: &str = "fetched.jpg";

/// How long a claim, a run or a finish may take. Generous, because it bounds a failure
/// rather than timing a success: the first beat is immediate.
const BUDGET: Duration = Duration::from_secs(30);

/// A fake holding the stranger's message, then Anna's, with Anna's photo downloadable.
fn seed() -> FakeTelegram {
    let fake = FakeTelegram::start();
    fake.deliver(recorded(ELSEWHERE, STRANGER));
    fake.deliver(recorded(GROUP, MESSAGE));
    fake.seed_file(PHOTO, &png());
    fake
}

/// The service that drives the seeded message, `home` its work dir. It writes no `every`, so
/// the cadence is afkd's default and its first beat is at once. `on_claim` reacts 👀; the run
/// writes every field of the message's handle to [`FIELDS`], a line each, and the env the
/// plugin handed it to [`ENV`], replies, edits the reply it got back, sends [`SHOT`], fetches
/// the message's first file to [`FETCHED`] and deletes the reply; `on_done` reacts 👍. Every
/// action is checked for its error. It keeps every run, because the leg counts the run dirs,
/// where the default `keep_runs on_fail` leaves nothing of a clean run.
fn service(home: &Path, base_url: &str) -> String {
    format!(
        r#"import "afkd"
import "@afkd/telegram"

ops :: service(telegram) {{
  token     "{TOKEN}"
  chats     "{GROUP}", "{PRIVATE}"
  base_url  "{base_url}"
  keep_runs always
  work_dir  "{home}"

  on_claim proc(message: telegram.Message) {{
    match telegram.react(message, "👀") {{
      .ok(_) => {{ }}
      .err(e) => fail("telegram.react: #{{e.message}}")
    }}
    $ echo claimed #{{message.id}} >> {CLAIMED}
  }}

  on_run proc(run: afkd.Run, message: telegram.Message) {{
    $ echo id #{{message.id}} >> {FIELDS}
    $ echo key #{{message.key}} >> {FIELDS}
    $ echo chat #{{message.chat}} >> {FIELDS}
    $ echo sender #{{message.sender}} >> {FIELDS}
    $ echo sender_name #{{message.sender_name}} >> {FIELDS}
    $ echo text #{{message.text}} >> {FIELDS}
    $ echo reply_to #{{message.reply_to}} >> {FIELDS}
    for file in message.files {{
      $ echo file #{{file}} >> {FIELDS}
    }}
    $ printenv TELEGRAM_BOT_TOKEN TELEGRAM_API_URL TELEGRAM_CHAT_ID TELEGRAM_MESSAGE_ID > {ENV}
    match telegram.reply(message, "{REPLY}") {{
      .ok(sent) => {{
        $ echo sent #{{sent.id}} #{{sent.reply_to}} #{{sent.text}} >> {FIELDS}
        match telegram.edit(sent, "{EDITED}") {{
          .ok(_) => {{ }}
          .err(e) => fail("telegram.edit: #{{e.message}}")
        }}
        match telegram.send_file(message.chat, "{home}/{SHOT}", kind=.photo, caption="監視 ✓") {{
          .ok(shot) => {{
            for file in shot.files {{
              $ echo shot #{{file}} >> {FIELDS}
            }}
          }}
          .err(e) => fail("telegram.send_file: #{{e.message}}")
        }}
        match telegram.fetch_file(message.files[0], "{home}/{FETCHED}") {{
          .ok(_) => {{ }}
          .err(e) => fail("telegram.fetch_file: #{{e.message}}")
        }}
        match telegram.delete(sent) {{
          .ok(_) => {{ }}
          .err(e) => fail("telegram.delete: #{{e.message}}")
        }}
      }}
      .err(e) => fail("telegram.reply: #{{e.message}}")
    }}
  }}

  on_done proc(run: afkd.Run, message: telegram.Message, outcome: afkd.Outcome) {{
    match telegram.react(message, "👍") {{
      .ok(_) => {{ }}
      .err(e) => fail("telegram.react: #{{e.message}}")
    }}
    $ echo done #{{message.id}} >> {DONE}
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

/// Write `settings` as the plugin's settings file in `home`.
fn write_settings(home: &Path, settings: &str) {
    let file = plugins_root(home).join(format!("{NAME}.afk"));
    std::fs::create_dir_all(file.parent().expect("has parent")).expect("mk the plugins dir");
    std::fs::write(&file, settings).expect("write the settings file");
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

/// The run dirs of `service`, sorted by name: the kept ones under `<state>/runs`, then the
/// ones in flight under the daemon's `$TMPDIR`; a name in both, a run caught mid-move, is the
/// kept one.
fn run_dirs(home: &Path, service: &str) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    for root in [runs_root(home), live_runs_root(home)] {
        for entry in std::fs::read_dir(root.join(service))
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

/// Everything a failed wait needs to say: the requests the fake saw and refused, the run
/// dirs, the daemon's own log, and its stderr so far — where the plugin's own lines land.
fn dump(home: &Path, fake: &FakeTelegram, daemon: &StreamingDaemon) -> String {
    format!(
        "methods: {:?}\nrefused: {:?}\nrun dirs: {:?}\ndaemon.log:\n{}\ndaemon stderr:\n{}",
        fake.methods(),
        fake.invalid_requests(),
        run_dirs(home, "ops"),
        std::fs::read_to_string(daemon_log(home)).unwrap_or_default(),
        daemon.stderr_so_far()
    )
}

/// Poll `ready` to [`BUDGET`], failing with `what` and the [`dump`] when it never holds.
fn wait_for(
    home: &Path,
    fake: &FakeTelegram,
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

/// Read `path` whole, naming it when it is missing.
fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Signal the daemon's graceful drain and hold it to a clean exit, and the fake to every
/// request the plugin sent being one the Bot API takes.
fn drain(daemon: StreamingDaemon, fake: &FakeTelegram) -> String {
    daemon.signal(libc::SIGINT);
    let out = daemon.reap(Duration::from_secs(10));
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        out.status.success(),
        "the daemon drains clean ({:?}):\n{stderr}",
        out.status
    );
    assert_eq!(
        fake.invalid_requests(),
        Vec::<String>::new(),
        "requests the Bot API refuses"
    );
    stderr
}

// --- the legs ----------------------------------------------------------------------

#[test]
fn install_leg_places_the_plugin_and_runs_one_message_through_it() {
    let home = TempDir::new().expect("tempdir");
    let stage_dir = TempDir::new().expect("tempdir");
    let staged = stage(stage_dir.path());
    let mut shipped = vec!["afkd-telegram".to_string(), "afkd-plugin.toml".to_string()];
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
    for exec in ["afkd-telegram", "skills/telegram/reply.py"] {
        let mode = std::fs::metadata(placed.join(exec))
            .unwrap_or_else(|e| panic!("the install placed {exec}: {e}"))
            .permissions()
            .mode();
        assert!(mode & 0o111 != 0, "{exec} is executable");
    }

    let home = home.path();
    let fake = seed();
    std::fs::write(home.join(SHOT), png()).expect("write the screenshot");
    write_config(home, &service(home, fake.base_url()));
    let (code, report) = validate(home);
    assert_eq!(code, Some(0), "the service validates:\n{report}");

    let daemon = spawn_headless_streaming(home, &[]);
    let brief = || {
        run_dirs(home, "ops")
            .into_iter()
            .map(|dir| dir.join("task.md"))
            .find(|task| task.metadata().is_ok_and(|meta| meta.len() > 0))
    };
    wait_for(home, &fake, &daemon, "the message's run starts", || {
        brief().is_some()
    });
    let task = read(&brief().expect("waited for"));
    for part in [
        "Telegram message 4711 in chat -1001234567890 (Drift 🚀)",
        "From: Anna Lindqvist (user 111111111)",
        "Replying to: message 4708",
        "デプロイ failed here — see the log ⬇",
        &format!("- photo, 1280x960, 102400 bytes: {PHOTO}"),
    ] {
        assert!(task.contains(part), "task.md carries {part:?}:\n{task}");
    }
    wait_for(home, &fake, &daemon, "the message is done", || {
        home.join(DONE).exists()
    });

    let sent = fake.calls("sendMessage");
    assert_eq!(sent.len(), 1, "one reply");
    assert_eq!(sent[0].params["text"], REPLY);
    assert_eq!(
        sent[0].params["reply_parameters"],
        json!({"message_id": MESSAGE})
    );
    let reply_id = fake.calls("editMessageText")[0].params["message_id"]
        .as_i64()
        .expect("the edit names a message");
    let shot = fake.calls("sendPhoto");
    assert_eq!(shot.len(), 1);
    assert_eq!(shot[0].files[0].bytes, png(), "the photo's exact bytes");
    assert_eq!(shot[0].files[0].filename, SHOT);
    assert_eq!(
        fake.calls("deleteMessage")[0].params["message_id"],
        reply_id,
        "the run deleted the reply it edited"
    );
    assert!(fake.message(GROUP, reply_id).is_none(), "the reply is gone");
    let actions: Vec<String> = fake
        .methods()
        .into_iter()
        .filter(|m| m != "getUpdates")
        .collect();
    assert_eq!(
        actions,
        [
            "setMessageReaction",
            "sendMessage",
            "editMessageText",
            "sendPhoto",
            "getFile",
            "file",
            "deleteMessage",
            "setMessageReaction",
        ],
        "on_claim, the run's calls in order, then on_done"
    );
    assert_eq!(
        fake.reaction(GROUP, MESSAGE),
        ["👍"],
        "on_done's reaction replaced on_claim's"
    );
    assert!(
        fake.seen().iter().all(|s| s.token_ok),
        "every request as the bot"
    );
    assert_eq!(
        std::fs::read(home.join(FETCHED)).expect("the fetched photo"),
        png(),
        "the photo byte for byte"
    );
    // Every field verbatim — the `$` lines shell-quote what they interpolate — and the file id
    // the sent photo's handle lists, which holds the bytes sent.
    let fields = read(&home.join(FIELDS));
    let (fields, shot_file) = fields
        .trim_end()
        .rsplit_once("\nshot ")
        .expect("the sent photo's file id");
    assert_eq!(
        fields,
        format!(
            "id 4711\nkey telegram:{GROUP}/4711\nchat {GROUP}\nsender 111111111\nsender_name \
             Anna Lindqvist\ntext デプロイ failed here — see the log ⬇\nreply_to 4708\nfile \
             {PHOTO}\nsent {reply_id} 4711 {REPLY}"
        )
    );
    assert_eq!(
        fake.file(shot_file),
        Some(png()),
        "the sent photo's handle lists the file id Telegram gave it"
    );
    assert_eq!(
        read(&home.join(ENV)),
        format!("{TOKEN}\n{}\n{GROUP}\n4711\n", fake.base_url())
    );
    assert_eq!(read(&home.join(CLAIMED)), "claimed 4711\n");
    assert_eq!(read(&home.join(DONE)), "done 4711\n");
    assert_eq!(
        run_dirs(home, "ops").len(),
        1,
        "one run: the stranger's message started none"
    );
    assert!(fake.pending().is_empty(), "both messages are confirmed");

    let stderr = drain(daemon, &fake);
    assert!(
        stderr.contains(&format!("skipped message {STRANGER} in chat {ELSEWHERE}")),
        "the stranger's message is said skipped:\n{stderr}"
    );
    assert!(
        !stderr.contains("rm -rf"),
        "a skipped message's text is never logged:\n{stderr}"
    );
}

/// The per-call leg's service: an interval service, run by hand, that sends a text and edits
/// the message it got back, writing its id once both landed.
fn nightly(home: &Path) -> String {
    format!(
        r#"import "afkd"
import "service:interval"
import "@afkd/telegram"

nightly :: service(interval) {{
  every    24h
  work_dir "{home}"

  on_run proc(run: afkd.Run) {{
    match telegram.send("{GROUP}", "nightly build — ビルド started") {{
      .ok(sent) => {{
        match telegram.edit(sent, "nightly build — ビルド done ✓") {{
          .ok(_) => {{ }}
          .err(e) => fail("telegram.edit: #{{e.message}}")
        }}
        $ echo #{{sent.id}} #{{sent.key}} > sent.txt
      }}
      .err(e) => fail("telegram.send: #{{e.message}}")
    }}
  }}
}}
"#,
        home = home.display()
    )
}

/// A service of another kind calls the plugin as the bot the settings file names: its send
/// gives back the message, which its edit takes.
#[test]
fn per_call_leg_an_interval_service_sends_and_edits_as_the_settings_file_s_bot() {
    let home = installed();
    let home = home.path();
    let fake = FakeTelegram::start();
    write_settings(
        home,
        &format!("token    \"{TOKEN}\"\nbase_url \"{}\"\n", fake.base_url()),
    );
    write_config(home, &nightly(home));
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
            dump(home, &fake, &daemon)
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    wait_for(home, &fake, &daemon, "the run sent and edited", || {
        home.join("sent.txt").exists()
    });
    let sent = fake.calls("sendMessage");
    assert_eq!(sent.len(), 1);
    let id = fake.calls("editMessageText")[0].params["message_id"]
        .as_i64()
        .expect("the edit names a message");
    assert_eq!(
        read(&home.join("sent.txt")),
        format!("{id} telegram:{GROUP}/{id}\n")
    );
    assert_eq!(
        fake.message(GROUP, id).expect("the message")["text"],
        "nightly build — ビルド done ✓",
        "the edit took the message the send gave back"
    );
    assert!(
        fake.seen().iter().all(|s| s.token_ok),
        "every request as the settings file's bot"
    );
    drain(daemon, &fake);
}

/// A config that writes every setting the kind declares and afkd's own claim keys, whose
/// four slots call every action with every named parameter, each checked for its error,
/// validates: afkd types every call, and every returning action's result, against the
/// manifest.
#[test]
fn every_setting_and_action_validates() {
    let home = installed();
    write_config(
        home.path(),
        &format!(
            r##"import "afkd"
import "@afkd/telegram"

ops :: service(telegram) {{
  token        "{TOKEN}"
  chats        "-1001234567890", "111111111", "@ops_channel"
  base_url     "https://api.telegram.org"
  every        5s
  max_attempts 2
  work_dir     "/srv/acme/widgets"

  on_claim proc(message: telegram.Message) {{
    match telegram.react(message, "👀") {{
      .ok(_) => {{ }}
      .err(e) => fail("telegram.react: #{{e.message}}")
    }}
  }}

  on_run proc(run: afkd.Run, message: telegram.Message) {{
    $ echo #{{message.chat}} #{{message.sender}} #{{message.sender_name}} #{{message.text}}
    for file in message.files {{
      match telegram.fetch_file(file, "/srv/acme/widgets/#{{file}} 監視.bin") {{
        .ok(_) => {{ }}
        .err(e) => fail("telegram.fetch_file: #{{e.message}}")
      }}
    }}
    match telegram.send_file("@ops_channel", "/srv/acme/widgets/report — 報告.pdf") {{
      .ok(doc) => {{
        match telegram.delete(doc) {{
          .ok(_) => {{ }}
          .err(e) => fail("telegram.delete: #{{e.message}}")
        }}
      }}
      .err(e) => fail("telegram.send_file: #{{e.message}}")
    }}
    match telegram.send_file(
      message.chat,
      "/srv/acme/widgets/out.document",
      kind=.document,
      caption="監視 ✓",
    ) {{
      .ok(_) => {{ }}
      .err(e) => fail("telegram.send_file: #{{e.message}}")
    }}
    match telegram.send_file(
      message.chat,
      "/srv/acme/widgets/out.photo",
      kind=.photo,
      caption="監視 ✓",
    ) {{
      .ok(_) => {{ }}
      .err(e) => fail("telegram.send_file: #{{e.message}}")
    }}
    match telegram.send_file(
      message.chat,
      "/srv/acme/widgets/out.video",
      kind=.video,
      caption="監視 ✓",
    ) {{
      .ok(_) => {{ }}
      .err(e) => fail("telegram.send_file: #{{e.message}}")
    }}
    match telegram.send_file(
      message.chat,
      "/srv/acme/widgets/out.audio",
      kind=.audio,
      caption="監視 ✓",
    ) {{
      .ok(_) => {{ }}
      .err(e) => fail("telegram.send_file: #{{e.message}}")
    }}
    match telegram.send_file(
      message.chat,
      "/srv/acme/widgets/out.voice",
      kind=.voice,
      caption="監視 ✓",
    ) {{
      .ok(_) => {{ }}
      .err(e) => fail("telegram.send_file: #{{e.message}}")
    }}
  }}

  on_done proc(run: afkd.Run, message: telegram.Message, outcome: afkd.Outcome) {{
    match telegram.reply(message, "done in #{{outcome.duration}} ✓\n\n完了") {{
      .ok(sent) => {{
        match telegram.edit(sent, "#{{sent.text}} — edited") {{
          .ok(_) => {{ }}
          .err(e) => fail("telegram.edit: #{{e.message}}")
        }}
      }}
      .err(e) => fail("telegram.reply: #{{e.message}}")
    }}
    match telegram.react(message, "") {{
      .ok(_) => {{ }}
      .err(e) => fail("telegram.react: #{{e.message}}")
    }}
  }}

  on_fail proc(run: afkd.Run, message: telegram.Message, outcome: afkd.Outcome) {{
    match telegram.send(
      message.chat,
      "#{{message.sender_name}}: that failed after #{{outcome.duration}}",
    ) {{
      .ok(sent) => {{
        $ echo #{{sent.id}} #{{sent.key}} #{{sent.reply_to}}
      }}
      .err(e) => fail("telegram.send: #{{e.message}}")
    }}
  }}
}}
"##
        ),
    );
    let (code, report) = validate(home.path());
    assert_eq!(
        code,
        Some(0),
        "every setting and action validates:\n{report}"
    );
}

/// `token` and `chats` are required: a service without either is refused at validate,
/// naming the setting and the service.
#[test]
fn a_service_without_token_or_chats_fails_validate_naming_the_setting() {
    let home = installed();
    for (missing, keep) in [
        ("token", "chats \"-1001234567890\"".to_string()),
        ("chats", format!("token \"{TOKEN}\"")),
    ] {
        write_config(
            home.path(),
            &format!(
                r#"import "afkd"
import "@afkd/telegram"

ops :: service(telegram) {{
  {keep}
  work_dir "/srv/acme/widgets"

  on_run proc(run: afkd.Run, message: telegram.Message) {{
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
                r#"missing required setting "{missing}" for service(telegram)"#
            )),
            "the refusal names the setting and the service:\n{report}"
        );
    }
}

/// A slot calling an action the plugin does not declare, and one reading a returning
/// action's result as an error, are each a load error; the same file without them is valid.
#[test]
fn an_undeclared_action_or_a_result_read_as_an_error_is_a_load_error() {
    let home = installed();
    let config = |call: &str| {
        format!(
            r#"import "afkd"
import "@afkd/telegram"

ops :: service(telegram) {{
  token    "{TOKEN}"
  chats    "-1001234567890"
  work_dir "/srv/acme/widgets"

  on_run proc(run: afkd.Run, message: telegram.Message) {{
    {call}
  }}
}}
"#
        )
    };
    write_config(home.path(), &config("telegram.forward(message)"));
    let (code, report) = validate(home.path());
    assert_eq!(code, Some(1), "an undeclared action is refused:\n{report}");
    assert!(
        report.contains(r#"plugin telegram has no action "forward""#),
        "the refusal names the action and the plugin as the file imports it:\n{report}"
    );
    assert!(
        report.contains("fetch_file"),
        "and lists the actions it has:\n{report}"
    );

    write_config(
        home.path(),
        &config(r#"if telegram.send(message.chat, "x") != nil { fail("telegram.send") }"#),
    );
    let (code, report) = validate(home.path());
    assert_eq!(
        code,
        Some(1),
        "send gives back a result, not an error:\n{report}"
    );
    assert!(
        report.contains("Result[telegram.Message, telegram.Error]"),
        "the refusal names the result send gives back:\n{report}"
    );

    write_config(home.path(), &config("$ echo #{message.text}"));
    let (code, report) = validate(home.path());
    assert_eq!(
        code,
        Some(0),
        "the slot without either validates:\n{report}"
    );
}

/// The README's `conf` fences, each with the line its opener sits on.
fn conf_fences(readme: &str) -> Vec<(usize, String)> {
    let mut fences = Vec::new();
    let mut open: Option<(usize, Vec<&str>)> = None;
    for (at, line) in readme.lines().enumerate() {
        match open.as_mut() {
            None if line.trim() == "```conf" => open = Some((at + 1, Vec::new())),
            None => {}
            Some(_) if line.trim() == "```" => {
                let (opened, body) = open.take().expect("open");
                fences.push((opened, body.join("\n") + "\n"));
            }
            Some((_, body)) => body.push(line),
        }
    }
    assert!(open.is_none(), "every conf fence in the README closes");
    fences
}

/// The first line of the README fence that is the plugin's settings file.
const SETTINGS_FENCE: &str = "// ~/.config/afkd/plugins/@afkd/telegram.afk";

/// Every `conf` fence of the README validates against the installed release, the settings
/// file's checked against the manifest's `[config]` with the fence after it.
#[test]
fn every_readme_conf_fence_validates() {
    let readme = std::fs::read_to_string(plugin_root().join("README.md")).expect("the README");
    let fences = conf_fences(&readme);
    let openers = readme.lines().filter(|l| l.trim() == "```conf").count();
    assert!(openers >= 3, "the README carries its conf fences");
    assert_eq!(fences.len(), openers, "every conf opener yields a fence");
    assert_eq!(
        fences
            .iter()
            .filter(|(_, f)| f.starts_with(SETTINGS_FENCE))
            .count(),
        1,
        "one fence is the settings file"
    );

    let home = installed();
    for (n, (line, fence)) in fences.iter().enumerate() {
        if fence.starts_with(SETTINGS_FENCE) {
            write_settings(home.path(), fence);
            continue;
        }
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

/// What the skill leg's run replies through `reply.py`: wide CJK, an emoji, quotes and
/// several lines.
const NOTE: &str = "計画 ✅ — \"staging\" is green:\n\nall 3 checks passed 🚀";

/// The skill leg's service: its run calls the installed skill's `reply.py` by path, with the
/// env the run carries and nothing else, the way an agent's Bash tool does.
fn skill_service(home: &Path, base_url: &str, skill: &Path) -> String {
    format!(
        r#"import "afkd"
import "@afkd/telegram"

NOTE :: "{note}"

ops :: service(telegram) {{
  token    "{TOKEN}"
  chats    "{GROUP}"
  base_url "{base_url}"
  work_dir "{home}"

  on_run proc(run: afkd.Run, message: telegram.Message) {{
    $ python3 {skill}/reply.py #{{NOTE}} > replied.txt
  }}
}}
"#,
        home = home.display(),
        skill = skill.display(),
        note = NOTE
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n"),
    )
}

/// A run replies to its message through the installed skill on a real daemon: the reply lands
/// in the message's chat as a reply to it, as the bot, verbatim. `afkd plugins` lists the
/// skill, and every request conforms.
#[test]
fn skill_leg_a_run_replies_through_the_installed_skill() {
    let home = installed();
    let home = home.path();
    let skill = plugins_root(home).join(NAME).join("skills/telegram");
    let out = run_subcommand_args(home, &["plugins"]);
    let listed = String::from_utf8_lossy(&out.stdout).into_owned();
    let line = listed
        .lines()
        .find(|l| l.starts_with(NAME))
        .unwrap_or_else(|| panic!("afkd plugins lists {NAME}:\n{listed}"));
    assert!(
        line.contains("skill telegram"),
        "the skill is supplied:\n{listed}"
    );

    let fake = FakeTelegram::start();
    fake.deliver(recorded(GROUP, MESSAGE));
    write_config(home, &skill_service(home, fake.base_url(), &skill));
    let (code, report) = validate(home);
    assert_eq!(code, Some(0), "the service validates:\n{report}");

    let daemon = spawn_headless_streaming(home, &[]);
    wait_for(home, &fake, &daemon, "the run replied", || {
        std::fs::read_to_string(home.join("replied.txt")).is_ok_and(|out| out.ends_with('\n'))
    });
    let sent = fake.calls("sendMessage");
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].params["text"], NOTE);
    assert_eq!(
        sent[0].params["reply_parameters"],
        json!({"message_id": MESSAGE})
    );
    assert!(sent[0].token_ok, "as the bot the run's env names");
    let replied = read(&home.join("replied.txt"));
    let id: i64 = replied
        .strip_prefix("reply.py: sent message ")
        .and_then(|rest| rest.strip_suffix(&format!(" to chat {GROUP}\n")))
        .and_then(|id| id.parse().ok())
        .unwrap_or_else(|| panic!("reply.py names the message it sent: {replied:?}"));
    let reply = fake.message(GROUP, id).expect("the reply");
    assert_eq!(reply["text"], NOTE);
    assert_eq!(reply["reply_to_message"]["message_id"], MESSAGE);
    drain(daemon, &fake);
}
