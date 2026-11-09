//! The `telegram` skill's scripts, run as an agent runs them — `python3` on the script, with
//! the `TELEGRAM_*` a run of a `service(telegram)` carries in its environment — against the
//! stateful fake Bot API, whose root is the `TELEGRAM_API_URL` they read. Each leg reads the
//! fake back: what landed in the chat, what a refused call left unsent, and which file a fetch
//! wrote. Every leg ends with the fake's record of requests the Bot API refuses empty.
//!
//! The scripts keep the plugin's own rules, and the cut of a long text is held to the plugin's
//! over the wire: the same texts sent through the plugin's `send` and through `send.py` leave
//! the same messages in the chat.
//!
//! A script that does not exist is a `python3` that exits 2, which every leg's success
//! assertion catches, so each leg fails before its script is written.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use common::fake::{png, recorded, utf16_len, FakeTelegram, GROUP, PRIVATE, TOKEN};
use common::{Plugin, TempDir};

/// The message the run works: Anna's photo in the group, captioned and replying to an earlier
/// message.
const MESSAGE: i64 = 4711;

/// The proxy variables urllib honours, which would send a loopback request elsewhere.
const PROXIES: [&str; 6] = [
    "http_proxy",
    "HTTP_PROXY",
    "https_proxy",
    "HTTPS_PROXY",
    "all_proxy",
    "ALL_PROXY",
];

/// A text over Telegram's cap, of wide CJK and emoji, in lines.
fn long_text() -> String {
    let line = "ビルド 🚀 — the deploy of staging finished, see the log\n";
    line.repeat(5000 / line.chars().count() + 1)
}

/// The chat a run works in: the fake, holding the run's message, and the run's scratch dir.
struct Chat {
    fake: FakeTelegram,
    scratch: TempDir,
}

fn chat() -> Chat {
    let fake = FakeTelegram::start();
    fake.deliver(recorded(GROUP, MESSAGE));
    Chat {
        fake,
        scratch: TempDir::new("skill"),
    }
}

impl Chat {
    /// Run one script with the env a run of a `service(telegram)` carries and nothing else of
    /// Telegram's.
    fn run(&self, script: &str, args: &[&str]) -> Output {
        self.run_with(script, args, &[])
    }

    /// [`run`](Self::run), each of `env` set to its value or, with none, unset.
    fn run_with(&self, script: &str, args: &[&str], env: &[(&str, Option<&str>)]) -> Output {
        run_script(&self.fake, script, args, env)
    }

    /// The texts of the `sendMessage` requests, in order.
    fn texts(&self) -> Vec<String> {
        texts(&self.fake)
    }

    /// The message of `id` in the group, as the fake holds it now.
    fn message(&self, id: i64) -> Value {
        self.fake
            .message(GROUP, id)
            .unwrap_or_else(|| panic!("no message {id} in the group"))
    }

    /// End the leg: no request the Bot API refuses.
    fn done(&self) {
        assert_eq!(
            self.fake.invalid_requests(),
            Vec::<String>::new(),
            "requests the Bot API refuses"
        );
    }
}

/// Run `script` against `fake` as a run of a `service(telegram)` working [`MESSAGE`] does,
/// each of `env` set to its value or, with none, unset.
fn run_script(
    fake: &FakeTelegram,
    script: &str,
    args: &[&str],
    env: &[(&str, Option<&str>)],
) -> Output {
    let mut cmd = Command::new("python3");
    cmd.arg(skill_dir().join(script)).args(args);
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("TELEGRAM_") {
            cmd.env_remove(name);
        }
    }
    for name in PROXIES {
        cmd.env_remove(name);
    }
    cmd.env("TELEGRAM_BOT_TOKEN", TOKEN)
        .env("TELEGRAM_API_URL", fake.base_url())
        .env("TELEGRAM_CHAT_ID", GROUP.to_string())
        .env("TELEGRAM_MESSAGE_ID", MESSAGE.to_string())
        .env("PYTHONDONTWRITEBYTECODE", "1");
    for (name, value) in env {
        match value {
            Some(value) => cmd.env(name, value),
            None => cmd.env_remove(name),
        };
    }
    cmd.output().expect("run python3")
}

/// The texts of the `sendMessage` requests `fake` saw, in order.
fn texts(fake: &FakeTelegram) -> Vec<String> {
    fake.calls("sendMessage")
        .iter()
        .map(|c| c.params["text"].as_str().expect("a text").to_string())
        .collect()
}

fn skill_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("skills/telegram")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// The stdout of a run that succeeded, failing the leg with its stderr otherwise.
fn ok(out: &Output) -> String {
    assert!(
        out.status.success(),
        "exit {:?}:\n{}{}",
        out.status.code(),
        stdout(out),
        stderr(out)
    );
    stdout(out)
}

/// Assert a run failed with `code`, its stderr holding `needle`.
fn refused(out: &Output, code: i32, needle: &str) {
    assert_eq!(
        out.status.code(),
        Some(code),
        "{}{}",
        stdout(out),
        stderr(out)
    );
    assert!(
        stderr(out).contains(needle),
        "{needle:?} in:\n{}",
        stderr(out)
    );
}

/// The message ids a sending script printed, one `sent message <id>` line each.
fn sent_ids(out: &str) -> Vec<i64> {
    out.lines()
        .map(|line| {
            line.split_whitespace()
                .skip_while(|w| *w != "message")
                .nth(1)
                .and_then(|id| id.parse().ok())
                .unwrap_or_else(|| panic!("a line naming a message: {line:?}"))
        })
        .collect()
}

// --- texts -----------------------------------------------------------------------------

/// A long text goes as two messages, cut after a newline, both ids printed, and nothing lost.
#[test]
fn send_splits_a_long_text_and_prints_every_message() {
    let t = chat();
    let text = long_text();
    let out = ok(&t.run("send.py", &[&text]));
    let ids = sent_ids(&out);
    assert_eq!(ids.len(), 2, "{out}");
    let parts = t.texts();
    assert!(parts[0].ends_with('\n') && utf16_len(&parts[0]) <= 4096);
    assert_eq!(parts.concat(), text, "nothing lost or doubled");
    for (id, part) in ids.iter().zip(&parts) {
        assert_eq!(t.message(*id)["text"], part.as_str());
    }
    assert!(out.contains(&format!("to chat {GROUP}")), "{out}");
    let elsewhere = ok(&t.run("send.py", &["hej 👋", "--chat-id", &PRIVATE.to_string()]));
    let id = sent_ids(&elsewhere)[0];
    assert_eq!(
        t.fake.message(PRIVATE, id).expect("sent")["text"],
        "hej 👋",
        "--chat-id overrides the run's chat"
    );
    t.done();
}

/// The texts whose cut the plugin and the skill must agree on: lines across the cap, one line
/// over two caps, emoji whose pairs straddle the cap, CJK across it, exactly the cap, the cap
/// and a newline, a cut that leaves a part of only Unicode whitespace, and one that leaves a
/// control character Python's `strip` takes as whitespace and Rust's `trim` does not.
fn split_vectors() -> Vec<String> {
    vec![
        ("x".repeat(79) + "\n").repeat(64),
        "a".repeat(9000),
        "a".to_string() + &"😀".repeat(2100),
        "監視".repeat(1500) + "\n" + &"監".repeat(2000),
        "😀".repeat(2048),
        "a".repeat(4096) + "\n",
        "a".repeat(4096) + "\u{3000}\u{2028} \u{a0}",
        "a".repeat(4096) + "\u{3000}\u{1f}",
        long_text(),
    ]
}

/// Each text sent through the plugin's `send`, by a child started for the one call as afkd
/// starts one, and through `send.py` leaves the same messages in the chat, part for part.
#[test]
fn the_plugin_and_the_skill_cut_a_text_alike() {
    for text in split_vectors() {
        let through_plugin = FakeTelegram::start();
        let mut plugin = Plugin::spawn();
        let reply = plugin.call(json!({
            "call": "call", "proto": 2, "action": "send",
            "args": {"chat": GROUP.to_string(), "text": text},
            "settings": {"token": TOKEN, "base_url": through_plugin.base_url()},
        }));
        assert_eq!(reply["ok"], true, "{reply}");
        plugin.finish();

        let through_skill = FakeTelegram::start();
        ok(&run_script(&through_skill, "send.py", &[&text], &[]));

        let (plugin_parts, skill_parts) = (texts(&through_plugin), texts(&through_skill));
        assert!(!plugin_parts.is_empty());
        // Say each part by its length and its last characters, not as the whole text.
        let said = |parts: &[String]| {
            parts
                .iter()
                .map(|p| {
                    let tail: String = p
                        .chars()
                        .skip(p.chars().count().saturating_sub(4))
                        .collect();
                    format!("{} units ending {tail:?}", utf16_len(p))
                })
                .collect::<Vec<_>>()
        };
        assert!(
            skill_parts == plugin_parts,
            "a text of {} UTF-16 units: the plugin sent {:?}, the skill {:?}",
            utf16_len(&text),
            said(&plugin_parts),
            said(&skill_parts),
        );
        for fake in [&through_plugin, &through_skill] {
            assert_eq!(fake.invalid_requests(), Vec::<String>::new());
        }
    }
}

/// A 429 is waited out for the time Telegram asks, and the send lands; one Telegram keeps
/// answering fails after five tries, naming it.
#[test]
fn send_waits_out_a_rate_limit_five_tries_in_all() {
    let t = chat();
    t.fake.rate_limit("sendMessage", 2, 1);
    let start = Instant::now();
    let out = ok(&t.run("send.py", &["deployed ✓"]));
    assert!(
        start.elapsed() >= Duration::from_secs(2),
        "both waits were waited"
    );
    assert_eq!(t.fake.calls("sendMessage").len(), 3);
    assert_eq!(t.message(sent_ids(&out)[0])["text"], "deployed ✓");

    t.fake.rate_limit("sendMessage", 6, 0);
    let before = t.fake.calls("sendMessage").len();
    refused(
        &t.run("send.py", &["again"]),
        1,
        "send: Telegram still rate-limited the bot after 5 tries",
    );
    assert_eq!(t.fake.calls("sendMessage").len() - before, 5);
    t.done();
}

/// A reply replies to the run's message, or to `--message`; a long one replies with its first
/// part only.
#[test]
fn reply_replies_to_the_run_s_message_or_another() {
    let t = chat();
    let out = ok(&t.run("reply.py", &["見ました — on it 👀"]));
    let id = sent_ids(&out)[0];
    let sent = t.fake.calls("sendMessage");
    assert_eq!(
        sent[0].params["reply_parameters"],
        json!({"message_id": MESSAGE})
    );
    assert_eq!(t.message(id)["reply_to_message"]["message_id"], MESSAGE);

    t.fake.deliver(recorded(GROUP, 4712));
    let out = ok(&t.run("reply.py", &[&long_text(), "--message", "4712"]));
    assert_eq!(sent_ids(&out).len(), 2);
    let sent = t.fake.calls("sendMessage");
    assert_eq!(
        sent[1].params["reply_parameters"],
        json!({"message_id": 4712})
    );
    assert!(
        sent[2].params.get("reply_parameters").is_none(),
        "only the first part replies"
    );
    refused(
        &t.run("reply.py", &["x", "--message", "47x"]),
        2,
        "--message is '47x', not a message id",
    );
    t.done();
}

/// A reaction is set and taken off; one Telegram does not take is its refusal, passed on.
#[test]
fn react_sets_takes_off_and_passes_telegram_s_refusal_on() {
    let t = chat();
    let out = ok(&t.run("react.py", &["👍"]));
    assert!(out.contains("reacted 👍 to message 4711"), "{out}");
    assert_eq!(t.fake.reaction(GROUP, MESSAGE), ["👍"]);
    ok(&t.run("react.py", &[""]));
    assert!(t.fake.reaction(GROUP, MESSAGE).is_empty());
    assert_eq!(
        t.fake.calls("setMessageReaction")[1].params["reaction"],
        json!([])
    );
    refused(
        &t.run("react.py", &["✅"]),
        1,
        "react: Telegram refused it: Bad Request: REACTION_INVALID",
    );
    t.done();
}

/// An edit replaces a sent text, or a sent file's caption through the fallback, and refuses a
/// text over the cap before anything is sent.
#[test]
fn edit_replaces_a_text_or_a_caption() {
    let t = chat();
    let id = sent_ids(&ok(&t.run("send.py", &["deploying …"])))[0];
    let out = ok(&t.run("edit.py", &[&id.to_string(), "deployed ✓ — 完了"]));
    assert!(out.contains(&format!("edited message {id}")), "{out}");
    assert_eq!(t.message(id)["text"], "deployed ✓ — 完了");

    let report = t.scratch.path().join("report.txt");
    std::fs::write(&report, "all green\n").unwrap();
    let file = sent_ids(&ok(&t.run(
        "send_file.py",
        &[report.to_str().unwrap(), "--caption", "the report"],
    )))[0];
    ok(&t.run("edit.py", &[&file.to_string(), "the report, final"]));
    assert_eq!(t.message(file)["caption"], "the report, final");
    assert_eq!(t.fake.calls("editMessageCaption").len(), 1);

    let before = t.fake.seen().len();
    refused(
        &t.run("edit.py", &[&id.to_string(), &"x".repeat(4097)]),
        1,
        "4097 UTF-16 units, over the 4096",
    );
    refused(
        &t.run("edit.py", &[&file.to_string(), &"x".repeat(2000)]),
        1,
        "2000 UTF-16 units, over the 1024 a caption holds",
    );
    assert_eq!(
        t.fake.seen().len() - before,
        1,
        "only the over-long caption's editMessageText was sent"
    );
    t.done();
}

/// A delete deletes the message.
#[test]
fn delete_deletes_the_message() {
    let t = chat();
    let id = sent_ids(&ok(&t.run("send.py", &["oops"])))[0];
    let out = ok(&t.run("delete.py", &[&id.to_string()]));
    assert!(out.contains(&format!("deleted message {id}")), "{out}");
    assert!(t.fake.message(GROUP, id).is_none());
    t.done();
}

// --- files -----------------------------------------------------------------------------

/// Each kind goes to its own method, its bytes exactly as on disk under its own name and the
/// part named for the kind, with the caption under it.
#[test]
fn send_file_sends_every_kind_with_its_exact_bytes() {
    let t = chat();
    let path = t.scratch.path().join("dashboard — 監視.png");
    std::fs::write(&path, png()).unwrap();
    for (kind, method) in [
        ("document", "sendDocument"),
        ("photo", "sendPhoto"),
        ("video", "sendVideo"),
        ("audio", "sendAudio"),
        ("voice", "sendVoice"),
    ] {
        let out = ok(&t.run(
            "send_file.py",
            &[
                path.to_str().unwrap(),
                "--kind",
                kind,
                "--caption",
                "監視 ✓",
            ],
        ));
        let id = sent_ids(&out)[0];
        let seen = t.fake.calls(method);
        assert_eq!(seen.len(), 1, "{kind}");
        assert_eq!(seen[0].files.len(), 1);
        let part = &seen[0].files[0];
        assert_eq!(
            (part.field.as_str(), part.filename.as_str()),
            (kind, "dashboard — 監視.png")
        );
        assert_eq!(part.bytes, png(), "{kind}: the exact bytes");
        assert_eq!(seen[0].params["caption"], "監視 ✓");
        assert_eq!(t.message(id)["caption"], "監視 ✓");
    }
    ok(&t.run("send_file.py", &[path.to_str().unwrap()]));
    let documents = t.fake.calls("sendDocument");
    assert!(
        documents[1].params.get("caption").is_none(),
        "a document, with no caption, by default"
    );
    t.done();
}

/// A caption over the cap, a missing file and a photo over its cap are refused before
/// anything is sent.
#[test]
fn send_file_refuses_before_uploading() {
    let t = chat();
    let path = t.scratch.path().join("report.txt");
    std::fs::write(&path, "all green\n").unwrap();
    let big = t.scratch.path().join("big.png");
    let file = std::fs::File::create(&big).unwrap();
    file.set_len(10 * 1024 * 1024 + 1).unwrap();
    let path = path.to_str().unwrap();
    refused(
        &t.run("send_file.py", &[path, "--caption", &"😀".repeat(513)]),
        1,
        "the caption is 1026 UTF-16 units, over the 1024 Telegram takes",
    );
    refused(
        &t.run("send_file.py", &["/nonexistent/report.txt"]),
        1,
        "/nonexistent/report.txt is not a file",
    );
    refused(
        &t.run("send_file.py", &[big.to_str().unwrap(), "--kind", "photo"]),
        1,
        "is 10485761 bytes, over the 10485760 bytes Telegram takes in a photo",
    );
    assert!(t.fake.seen().is_empty(), "nothing was sent");
    t.done();
}

/// A file a message carries is written to the path given, byte for byte; one over the 20 MB
/// a bot may fetch is Telegram's refusal, passed on, and a path whose directory is not there
/// is refused before any request.
#[test]
fn fetch_file_writes_the_file_s_bytes() {
    let t = chat();
    t.fake.seed_file("BQACAgQAAxkBAAIBPWcS_log", &png());
    let to = t.scratch.path().join("deploy — ログ.log");
    let out = ok(&t.run(
        "fetch_file.py",
        &["BQACAgQAAxkBAAIBPWcS_log", to.to_str().unwrap()],
    ));
    assert_eq!(out.trim(), to.to_str().unwrap());
    assert_eq!(std::fs::read(&to).unwrap(), png());

    t.fake.seed_big_file("BAACAgQAAxkBAAIBbig");
    let big = t.scratch.path().join("big.mp4");
    refused(
        &t.run(
            "fetch_file.py",
            &["BAACAgQAAxkBAAIBbig", big.to_str().unwrap()],
        ),
        1,
        "fetch_file: Telegram refused it: Bad Request: file is too big",
    );
    assert!(!big.exists());
    let before = t.fake.seen().len();
    refused(
        &t.run(
            "fetch_file.py",
            &["BQACAgQAAxkBAAIBPWcS_log", "/nonexistent/dir/x.log"],
        ),
        1,
        "the directory of /nonexistent/dir/x.log does not exist",
    );
    assert_eq!(t.fake.seen().len(), before);
    t.done();
}

// --- the skill as a whole --------------------------------------------------------------

/// A script missing the token, or the chat, exits 1 naming it, and SKILL.md's placeholder is
/// refused, each before any request.
#[test]
fn a_missing_credential_or_a_placeholder_is_refused_before_any_request() {
    let t = chat();
    refused(
        &t.run_with("send.py", &["hej"], &[("TELEGRAM_BOT_TOKEN", None)]),
        1,
        "TELEGRAM_BOT_TOKEN must be set in the environment",
    );
    refused(
        &t.run_with("react.py", &["👍"], &[("TELEGRAM_CHAT_ID", None)]),
        1,
        "no chat (set TELEGRAM_CHAT_ID or pass --chat-id)",
    );
    refused(
        &t.run_with("reply.py", &["hej"], &[("TELEGRAM_MESSAGE_ID", None)]),
        1,
        "no message (set TELEGRAM_MESSAGE_ID or pass --message)",
    );
    for (script, args) in [
        ("send.py", vec!["<REPLACE WITH YOUR MESSAGE>"]),
        ("reply.py", vec!["<REPLACE WITH YOUR REPLY>"]),
        ("edit.py", vec!["4712", "<REPLACE WITH THE NEW TEXT>"]),
        ("send_file.py", vec!["<REPLACE WITH THE FILE'S PATH>"]),
    ] {
        refused(
            &t.run(script, &args),
            2,
            "refusing to send the placeholder text",
        );
        let skill_md = std::fs::read_to_string(skill_dir().join("SKILL.md")).unwrap();
        assert!(
            skill_md.contains(args.last().unwrap()),
            "SKILL.md shows {script}'s placeholder"
        );
    }
    assert!(t.fake.seen().is_empty(), "nothing was sent");
    t.done();
}

/// Every script is executable, allowed by SKILL.md, shown in it and run by a leg here; the
/// module they share is imported, not run.
#[test]
fn every_script_has_a_leg_and_a_skill_section() {
    let skill_md = std::fs::read_to_string(skill_dir().join("SKILL.md")).expect("SKILL.md");
    let front = skill_md
        .split("---")
        .nth(1)
        .expect("SKILL.md opens with front matter");
    let allowed = front
        .lines()
        .find(|l| l.starts_with("allowed-tools:"))
        .expect("allowed-tools");
    let legs = include_str!("skills.rs");
    let mut scripts: Vec<String> = std::fs::read_dir(skill_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".py"))
        .collect();
    scripts.sort();
    assert_eq!(
        scripts,
        [
            "delete.py",
            "edit.py",
            "fetch_file.py",
            "react.py",
            "reply.py",
            "send.py",
            "send_file.py"
        ],
        "one script per action of the plugin's"
    );
    for script in &scripts {
        let mode = std::fs::metadata(skill_dir().join(script))
            .unwrap()
            .permissions()
            .mode();
        assert!(mode & 0o111 != 0, "{script} is executable");
        assert!(
            allowed.contains(&format!("Bash(*/{script} *)")),
            "SKILL.md allows {script}"
        );
        assert!(
            skill_md.contains(&format!("skills/telegram/{script}\"")),
            "SKILL.md shows {script}"
        );
        assert!(
            legs.contains(&format!("run(\"{script}\"")) || legs.contains(&format!("\"{script}\",")),
            "a leg runs {script}"
        );
    }
    let lib = std::fs::metadata(skill_dir().join("lib/telegram_api.py"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(lib & 0o111, 0, "the module is imported, not run");
}
