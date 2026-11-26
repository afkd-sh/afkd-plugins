//! The skill's scripts, one per operation, run as an agent runs them — `python3 -I` on the script,
//! the board's address and credential in the environment — against the loopback fake board. Each
//! leg runs a script on one board and the plugin's own one-shot `call` of the same action on an
//! identical one, and holds what the script prints to the value the binary's reply carries (a
//! handle, an array of them, a value, or nothing as `null`) and the board it leaves to the board
//! the call leaves. The uuids two boards mint differ, so each is read through its own board's ids
//! before the two are compared.
//!
//! The run a change is shown under is held too: a script that knows no machine sends no `run`,
//! and says so once; one that knows all four sends exactly them; a list's change sends none.

mod common;

use std::path::Path;
use std::process::{Command, Output};

use serde_json::{json, Value};

use common::fake::{FakeBoard, NewCard, Run, T0};
use common::{board, once, TempDir};

const KEY: &str = "afk_board_train-0123456789";

/// The scripts, one for each operation of the plugin.
const SCRIPTS: &[&str] = &[
    "card",
    "cards",
    "comments",
    "checklist",
    "lists",
    "labels",
    "create_card",
    "update_card",
    "move_card",
    "add_label",
    "remove_label",
    "add_item",
    "check_item",
    "remove_item",
    "mark_done",
    "add_blocked_by",
    "remove_blocked_by",
    "create_comment",
    "archive_card",
    "unarchive_card",
    "delete_card",
    "create_list",
    "update_list",
    "delete_list",
];

/// How many cards [`world`] seeds.
const CARDS: i64 = 5;

/// The board every leg starts from, its lists `Todo`, `Doing`, `Done` and `Spare`, its labels `bug`
/// and `ready`, and:
///
/// - #1, Todo: `bug`, a markdown description, a checklist of two items, the second done, and two
///   comments, by Ed and by the key `nightly` in a run;
/// - #2, Todo;
/// - #3, Done: done;
/// - #4, Todo: archived;
/// - #5, Doing: blocked by #2, which is not done.
fn world() -> FakeBoard {
    let fake = FakeBoard::start(
        "Release train 🚂",
        KEY,
        &["Todo", "Doing", "Done", "Spare"],
        &[("bug", "red"), ("ready", "green")],
    );
    fake.card(NewCard {
        description: "It retries forever.\n\n```rust\nlet backoff = 0;\n```".to_string(),
        labels: vec!["bug".to_string()],
        checklist: vec![
            ("复现 the storm 🚨".to_string(), false),
            ("Bound the backoff".to_string(), true),
        ],
        ..NewCard::new("Todo", "修复 the \"retry\" storm 🚨")
    });
    fake.card(NewCard::new("Todo", "Second"));
    fake.card(NewCard {
        done: true,
        ..NewCard::new("Done", "Shipped")
    });
    fake.card(NewCard::new("Todo", "An old idea"));
    fake.archive(4, T0 + 3600);
    fake.card(NewCard {
        blocked_by: vec![2],
        ..NewCard::new("Doing", "Waits for the second")
    });
    fake.comment(1, "看起来不对 🚨 — it resets", "Ed", false, None, 60);
    fake.comment(
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
    fake
}

/// The variables a proxy is named by, in both the cases Python reads.
const PROXY_VARS: [&str; 8] = [
    "HTTP_PROXY",
    "http_proxy",
    "HTTPS_PROXY",
    "https_proxy",
    "ALL_PROXY",
    "all_proxy",
    "NO_PROXY",
    "no_proxy",
];

/// One script with the fake's address and credential, and none of the run's variables nor any
/// proxy the test's own environment names.
fn command(script: &str, args: &[&str], fake: &FakeBoard) -> Command {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("skills/afkd_board")
        .join(format!("{script}.py"));
    let mut command = Command::new("python3");
    command
        .arg("-I")
        .arg(path)
        .args(args)
        .env("AFKD_BOARD_URL", fake.base_url())
        .env("AFKD_BOARD_AUTHORIZATION", format!("Bearer {KEY}"))
        .env_remove("AFKD_BOARD_RUN")
        .env_remove("AFKD_SCRATCH_DIR")
        .env_remove("AFKD_INSTANCE");
    for var in PROXY_VARS {
        command.env_remove(var);
    }
    command
}

fn run_script(script: &str, args: &[&str], fake: &FakeBoard) -> Output {
    command(script, args, fake).output().expect("run python3")
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// What a script that must succeed printed, parsed as JSON.
fn printed(script: &str, args: &[&str], fake: &FakeBoard) -> Value {
    let out = run_script(script, args, fake);
    assert!(out.status.success(), "{script} {args:?}: {}", stderr(&out));
    assert_eq!(stderr(&out), "", "{script} said something on stderr");
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "{script}: stdout is JSON ({e}): {}",
            String::from_utf8_lossy(&out.stdout)
        )
    })
}

/// The handle of the card `number` of `fake`, as afkd passes one to an action.
fn card(fake: &FakeBoard, number: i64) -> Value {
    json!({"id": number.to_string(),
           "key": format!("afkd_board:{}/{}", fake.board_id(), fake.card_id(number))})
}

/// Every uuid `fake` has minted that a reply names, with the name each is read through: the
/// board, each card that exists and each list.
fn ids(fake: &FakeBoard) -> Vec<(String, String)> {
    let mut ids = vec![(fake.board_id(), "BOARD".to_string())];
    for n in 1..=CARDS + 2 {
        if fake.card_state(n).is_some() {
            ids.push((fake.card_id(n), format!("CARD{n}")));
        }
    }
    for name in fake.list_names() {
        ids.push((fake.list_id(&name), format!("LIST:{name}")));
    }
    ids
}

/// `value` with each uuid of `ids` read through its name.
fn read_through(value: &Value, ids: &[(String, String)]) -> Value {
    match value {
        Value::String(text) => Value::String(ids.iter().fold(text.clone(), |text, (id, name)| {
            text.replace(id.as_str(), name)
        })),
        Value::Array(items) => Value::Array(items.iter().map(|v| read_through(v, ids)).collect()),
        Value::Object(fields) => {
            let mut read: serde_json::Map<String, Value> = fields
                .iter()
                .map(|(k, v)| (k.clone(), read_through(v, ids)))
                .collect();
            // A comment's uuid is minted, so two boards' differ: a comment handle is held to its
            // key being its board and its id, and is read through as `COMMENT`.
            let is_comment = fields
                .get("fields")
                .is_some_and(|f| f.get("body").is_some());
            if let (true, Some(Value::String(id)), Some(Value::String(key))) =
                (is_comment, fields.get("id"), fields.get("key"))
            {
                let board = ids.first().map(|(id, _)| id.as_str()).unwrap_or_default();
                assert_eq!(key, &format!("afkd_board:{board}/{id}"), "a comment's key");
                read.insert("id".to_string(), json!("COMMENT"));
                read.insert("key".to_string(), json!("afkd_board:BOARD/COMMENT"));
            }
            Value::Object(read)
        }
        other => other.clone(),
    }
}

/// What a reply of the binary gives back, the way a script prints it.
fn given(reply: &Value) -> Value {
    assert_eq!(reply["ok"], true, "{reply}");
    for shape in ["handle", "handles", "value"] {
        if let Some(value) = reply.get(shape) {
            return value.clone();
        }
    }
    Value::Null
}

/// What a board holds, to compare two boards by: every card by its number, and the lists.
fn held(fake: &FakeBoard) -> Value {
    let cards: Vec<Value> = (1..=CARDS + 2)
        .filter_map(|n| {
            let state = fake.card_state(n)?;
            Some(
                json!({"number": n, "title": state.title, "description": state.description,
                        "list": state.list, "labels": state.labels,
                        "checklist": state.checklist, "done": state.done,
                        "blocked_by": state.blocked_by, "archived": state.archived,
                        "comments": fake.comments_on(n).iter()
                            .map(|(body, by, by_key, run)| json!({"body": body, "by": by,
                                "by_key": by_key, "run": run.as_ref().map(|r| r.id.clone())}))
                            .collect::<Vec<_>>()}),
            )
        })
        .collect();
    json!({"cards": cards, "lists": fake.list_names(), "labels": fake.labels()})
}

/// Run `script` with `argv` on one fresh board and the action `action` with the arguments `args`
/// builds on another, and hold what the script printed to what the call gave back, and the two
/// boards to one another.
fn same_as_the_call(script: &str, argv: &[&str], action: &str, args: impl Fn(&FakeBoard) -> Value) {
    let (by_script, by_call) = (world(), world());
    let before = (ids(&by_script), ids(&by_call));
    let out = printed(script, argv, &by_script);
    let (reply, _) = once(action, args(&by_call));
    let wanted = given(&reply);
    let mut at_script = before.0;
    at_script.extend(ids(&by_script));
    let mut at_call = before.1;
    at_call.extend(ids(&by_call));
    assert_eq!(
        read_through(&out, &at_script),
        read_through(&wanted, &at_call),
        "{script} {argv:?} against `{action}`"
    );
    assert_eq!(
        held(&by_script),
        held(&by_call),
        "{script} {argv:?} left the board `{action}` does not"
    );
    for fake in [&by_script, &by_call] {
        assert_eq!(fake.invalid_requests(), Vec::<String>::new(), "{script}");
    }
}

/// Every operation of the manifest has a script of its name, and the skill's guide documents each
/// one: the script's own name, and the arguments it takes in its table.
#[test]
fn every_operation_has_a_script_and_the_guide_documents_it() {
    let manifest =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("afkd-plugin.toml"))
            .expect("the manifest");
    let mut actions: Vec<&str> = manifest
        .split("[[action]]\nname = \"")
        .skip(1)
        .filter_map(|rest| rest.split_once('"').map(|(name, _)| name))
        .collect();
    actions.sort_unstable();
    let mut scripts: Vec<&str> = SCRIPTS.to_vec();
    scripts.sort_unstable();
    assert_eq!(scripts, actions, "one script for each action of the plugin");
    let skill = Path::new(env!("CARGO_MANIFEST_DIR")).join("skills/afkd_board");
    let guide = std::fs::read_to_string(skill.join("SKILL.md")).expect("SKILL.md");
    for script in &actions {
        assert!(skill.join(format!("{script}.py")).is_file(), "{script}.py");
        assert!(
            guide.contains(&format!("| `{script}.py`"))
                || guide.contains(&format!("`, `{script}.py` |")),
            "the guide's table lists {script}.py"
        );
    }
    assert!(
        guide.contains("python3 \"${CLAUDE_PLUGIN_ROOT}/skills/afkd_board/"),
        "the guide shows how a script is run"
    );
    let mut files: Vec<String> = std::fs::read_dir(&skill)
        .expect("the skill")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".py"))
        .collect();
    files.sort();
    let want: Vec<String> = actions.iter().map(|a| format!("{a}.py")).collect();
    assert_eq!(files, want, "no script without an operation");
}

#[test]
fn the_reads_print_what_the_call_gives() {
    same_as_the_call(
        "card",
        &["1"],
        "card",
        |f| json!({"b": board(f), "number": 1}),
    );
    same_as_the_call(
        "card",
        &["4"],
        "card",
        |f| json!({"b": board(f), "number": 4}),
    );
    same_as_the_call("cards", &[], "cards", |f| json!({"b": board(f)}));
    same_as_the_call(
        "cards",
        &[
            "--list",
            "Todo",
            "--list",
            "Doing",
            "--label",
            "bug",
            "--without_label",
            "ready",
            "--done",
            "false",
            "--blocked",
            "false",
            "--text",
            "RETRY",
            "--include_archived",
            "--limit",
            "7",
        ],
        "cards",
        |f| {
            json!({"b": board(f), "list": ["Todo", "Doing"], "labels": ["bug"],
                   "without_labels": ["ready"], "done": false, "blocked": false,
                   "text": "RETRY", "include_archived": true, "limit": 7})
        },
    );
    same_as_the_call(
        "cards",
        &["--list", "Todo", "--include_archived", "--blocked", "true"],
        "cards",
        |f| json!({"b": board(f), "list": ["Todo"], "include_archived": true, "blocked": true}),
    );
    same_as_the_call(
        "cards",
        &["--include_archived"],
        "cards",
        |f| json!({"b": board(f), "include_archived": true}),
    );
    same_as_the_call(
        "comments",
        &["1"],
        "comments",
        |f| json!({"b": board(f), "card": card(f, 1)}),
    );
    same_as_the_call(
        "checklist",
        &["1"],
        "checklist",
        |f| json!({"b": board(f), "card": card(f, 1)}),
    );
    same_as_the_call(
        "checklist",
        &["2"],
        "checklist",
        |f| json!({"b": board(f), "card": card(f, 2)}),
    );
    same_as_the_call("lists", &[], "lists", |f| json!({"b": board(f)}));
    same_as_the_call("labels", &[], "labels", |f| json!({"b": board(f)}));
}

#[test]
fn the_card_writes_print_what_the_call_gives() {
    same_as_the_call(
        "create_card",
        &[
            "Todo",
            "New 🚨 \"card\"",
            "--description",
            "Why\n\n```\nx\n```",
            "--label",
            "bug",
            "--label",
            "fresh",
            "--item",
            "Reproduce it",
            "--item",
            "复现",
            "--at",
            "top",
        ],
        "create_card",
        |f| {
            json!({"b": board(f), "list": "Todo", "title": "New 🚨 \"card\"",
                   "description": "Why\n\n```\nx\n```", "labels": ["bug", "fresh"],
                   "checklist": [{"text": "Reproduce it", "done": false},
                                 {"text": "复现", "done": false}], "at": "top"})
        },
    );
    same_as_the_call(
        "create_card",
        &["Doing", "Bare"],
        "create_card",
        |f| json!({"b": board(f), "list": "Doing", "title": "Bare"}),
    );
    same_as_the_call(
        "create_card",
        &["Todo", "Placed", "--after", "2"],
        "create_card",
        |f| json!({"b": board(f), "list": "Todo", "title": "Placed", "after": card(f, 2)}),
    );
    same_as_the_call(
        "update_card",
        &["1", "--title", "Renamed 🚨", "--description", "Text"],
        "update_card",
        |f| json!({"b": board(f), "card": card(f, 1), "title": "Renamed 🚨", "description": "Text"}),
    );
    same_as_the_call(
        "update_card",
        &["2", "--title", "Only the title"],
        "update_card",
        |f| json!({"b": board(f), "card": card(f, 2), "title": "Only the title"}),
    );
    same_as_the_call(
        "move_card",
        &["1", "Doing"],
        "move_card",
        |f| json!({"b": board(f), "card": card(f, 1), "list": "Doing"}),
    );
    same_as_the_call(
        "move_card",
        &["2", "Doing", "--at", "top"],
        "move_card",
        |f| json!({"b": board(f), "card": card(f, 2), "list": "Doing", "at": "top"}),
    );
    same_as_the_call(
        "move_card",
        &["1", "Doing", "--after", "5"],
        "move_card",
        |f| json!({"b": board(f), "card": card(f, 1), "list": "Doing", "after": card(f, 5)}),
    );
    same_as_the_call(
        "add_label",
        &["2", "ready"],
        "add_label",
        |f| json!({"b": board(f), "card": card(f, 2), "label": "ready"}),
    );
    same_as_the_call(
        "add_label",
        &["2", "brand-new 🆕"],
        "add_label",
        |f| json!({"b": board(f), "card": card(f, 2), "label": "brand-new 🆕"}),
    );
    same_as_the_call(
        "remove_label",
        &["1", "bug"],
        "remove_label",
        |f| json!({"b": board(f), "card": card(f, 1), "label": "bug"}),
    );
    same_as_the_call(
        "remove_label",
        &["1", "never-made"],
        "remove_label",
        |f| json!({"b": board(f), "card": card(f, 1), "label": "never-made"}),
    );
    same_as_the_call(
        "add_item",
        &["1", "Ship it 🚀"],
        "add_item",
        |f| json!({"b": board(f), "card": card(f, 1), "text": "Ship it 🚀"}),
    );
    same_as_the_call(
        "check_item",
        &["1", "0"],
        "check_item",
        |f| json!({"b": board(f), "card": card(f, 1), "index": 0}),
    );
    same_as_the_call(
        "check_item",
        &["1", "1", "--done", "false"],
        "check_item",
        |f| json!({"b": board(f), "card": card(f, 1), "index": 1, "done": false}),
    );
    same_as_the_call(
        "remove_item",
        &["1", "0"],
        "remove_item",
        |f| json!({"b": board(f), "card": card(f, 1), "index": 0}),
    );
    same_as_the_call(
        "mark_done",
        &["2"],
        "mark_done",
        |f| json!({"b": board(f), "card": card(f, 2)}),
    );
    same_as_the_call(
        "mark_done",
        &["3", "--done", "false"],
        "mark_done",
        |f| json!({"b": board(f), "card": card(f, 3), "done": false}),
    );
    same_as_the_call(
        "add_blocked_by",
        &["1", "2"],
        "add_blocked_by",
        |f| json!({"b": board(f), "card": card(f, 1), "other": card(f, 2)}),
    );
    same_as_the_call(
        "remove_blocked_by",
        &["5", "2"],
        "remove_blocked_by",
        |f| json!({"b": board(f), "card": card(f, 5), "other": card(f, 2)}),
    );
    same_as_the_call(
        "archive_card",
        &["2"],
        "archive_card",
        |f| json!({"b": board(f), "card": card(f, 2)}),
    );
    same_as_the_call(
        "unarchive_card",
        &["4"],
        "unarchive_card",
        |f| json!({"b": board(f), "card": card(f, 4)}),
    );
    same_as_the_call(
        "delete_card",
        &["2"],
        "delete_card",
        |f| json!({"b": board(f), "card": card(f, 2)}),
    );
}

/// A comment's uuid is minted, so two boards' differ: the handle is held to the call's field by
/// field, its `id` is its key's last segment, and the board holds the comment.
#[test]
fn create_comment_prints_the_comment_handle() {
    let text = "plan: \"quoted\" 计划\nline 2 🚨 & 100% + more";
    let dir = TempDir::new("comment");
    let file = dir.path().join("comment.md");
    std::fs::write(&file, text).expect("write the comment");
    let file = file.to_string_lossy().into_owned();
    for argv in [vec!["1", text], vec!["1", "--body_file", &file]] {
        let (by_script, by_call) = (world(), world());
        let out = printed("create_comment", &argv, &by_script);
        let (reply, _) = once(
            "create_comment",
            json!({"b": board(&by_call), "card": card(&by_call, 1), "body": text}),
        );
        let wanted = given(&reply);
        assert_eq!(out["fields"], wanted["fields"], "{argv:?}");
        assert_eq!(out["fields"]["body"], text);
        assert_eq!(
            out["fields"]["author"],
            Value::Null,
            "a key's comment names no person"
        );
        let uuid = out["id"].as_str().expect("an id");
        assert_eq!(
            out["key"],
            format!("afkd_board:{}/{uuid}", by_script.board_id())
        );
        assert_eq!(held(&by_script), held(&by_call));
    }
}

#[test]
fn the_list_writes_print_what_the_call_gives() {
    same_as_the_call(
        "create_list",
        &["Review"],
        "create_list",
        |f| json!({"b": board(f), "name": "Review"}),
    );
    same_as_the_call(
        "create_list",
        &["Review", "--after", "Todo"],
        "create_list",
        |f| json!({"b": board(f), "name": "Review", "after": "Todo"}),
    );
    same_as_the_call(
        "create_list",
        &["Icebox 🧊", "--at", "top"],
        "create_list",
        |f| json!({"b": board(f), "name": "Icebox 🧊", "at": "top"}),
    );
    same_as_the_call(
        "update_list",
        &["Todo", "--name", "Backlog", "--after", "Done"],
        "update_list",
        |f| json!({"b": board(f), "list": "Todo", "name": "Backlog", "after": "Done"}),
    );
    same_as_the_call(
        "update_list",
        &["Doing", "--at", "bottom"],
        "update_list",
        |f| json!({"b": board(f), "list": "Doing", "at": "bottom"}),
    );
    same_as_the_call(
        "delete_list",
        &["Spare"],
        "delete_list",
        |f| json!({"b": board(f), "list": "Spare"}),
    );
}

/// A refusal is the plugin's sentence after the script's name, on stderr, exit 1, with nothing
/// printed and nothing written: the board's own, the plugin's, and a missing variable.
#[test]
fn a_refusal_is_the_plugins_sentence_and_writes_nothing() {
    let fake = world();
    let before = held(&fake);
    for (script, argv, sentence) in [
        (
            "move_card",
            vec!["1", "Nowhere"],
            "move_card: The board has no list `Nowhere`; it has `Todo`, `Doing`, `Done` and `Spare`.",
        ),
        (
            "cards",
            vec!["--list", "Nowhere"],
            "cards: The board has no list `Nowhere`; it has `Todo`, `Doing`, `Done` and `Spare`.",
        ),
        ("cards", vec!["--limit", "0"], "cards: limit is 1 to 250, not 0"),
        ("card", vec!["99"], "card: The board has no card #99."),
        (
            "mark_done",
            vec!["5"],
            "mark_done: Card #5 is blocked by #2, which is not done.",
        ),
        (
            "delete_list",
            vec!["Todo"],
            "delete_list: The list `Todo` holds 2 cards that are not archived; move or archive them first.",
        ),
    ] {
        let out = run_script(script, &argv, &fake);
        assert_eq!(out.status.code(), Some(1), "{script} {argv:?}: {}", stderr(&out));
        assert_eq!(stderr(&out), format!("{script}.py: {sentence}\n"), "{script}");
        assert!(out.stdout.is_empty(), "{script} printed on a refusal");
    }
    assert_eq!(held(&fake), before, "a refusal changed nothing");

    let mut bare = command("lists", &[], &fake);
    bare.env_remove("AFKD_BOARD_URL")
        .env_remove("AFKD_BOARD_AUTHORIZATION");
    let out = bare.output().expect("run python3");
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        stderr(&out),
        "lists.py: AFKD_BOARD_URL and AFKD_BOARD_AUTHORIZATION must be set in the environment\n"
    );

    let out = run_script(
        "create_comment",
        &["1", "<REPLACE WITH YOUR COMMENT>"],
        &fake,
    );
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out).contains("refusing to send the placeholder text"),
        "{}",
        stderr(&out)
    );
    assert_eq!(held(&fake), before);
    assert_eq!(fake.invalid_requests(), Vec::<String>::new());
}

/// A script given a key the board does not know says what the board says, and never the key.
#[test]
fn a_refused_key_is_the_boards_sentence_and_never_printed() {
    let fake = world();
    let mut wrong = command("lists", &[], &fake);
    wrong.env(
        "AFKD_BOARD_AUTHORIZATION",
        "Bearer afk_board_wrong-0123456789",
    );
    let out = wrong.output().expect("run python3");
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        stderr(&out),
        "lists.py: lists: the board key is missing, unknown or revoked\n"
    );
    let mut down = command("lists", &[], &fake);
    down.env("AFKD_BOARD_URL", "http://127.0.0.1:1");
    let out = down.output().expect("run python3");
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).starts_with("lists.py: lists: cannot reach the board at 127.0.0.1:1: "),
        "{}",
        stderr(&out)
    );
    assert!(!stderr(&out).contains(KEY));
}

/// The run a script's change is made in. With a scratch dir and an instance set but no machine
/// named, no `run` is sent, and the script says so once; with the machine named, the four fields
/// are sent as they are, and the board shows the comment under that run; a list's change sends
/// none either way, and outside a run nothing is said.
#[test]
fn a_script_sends_the_run_only_when_it_knows_all_four_fields() {
    let comment = |env: &[(&str, &str)], fake: &FakeBoard, args: &[&str]| {
        let mut command = command("create_comment", args, fake);
        for (name, value) in env {
            command.env(name, value);
        }
        let out = command.output().expect("run python3");
        assert!(out.status.success(), "{}", stderr(&out));
        out
    };
    let scratch = "/tmp/afkd/runs/nova/develop/261126-205930-unit-17-1";

    // Everything but the machine: no run, and one sentence about it.
    let fake = world();
    let out = comment(
        &[
            ("AFKD_SCRATCH_DIR", scratch),
            ("AFKD_INSTANCE", "2"),
            ("AFKD_BOARD_RUN", r#"{"service":"develop"}"#),
        ],
        &fake,
        &["1", "no machine yet"],
    );
    let said = stderr(&out);
    assert_eq!(said.matches("not this run").count(), 1, "{said}");
    assert!(said.contains("machine of the run is not known"), "{said}");
    assert_eq!(fake.writes().len(), 1);
    assert_eq!(
        fake.writes()[0].body,
        Some(json!({"body": "no machine yet"})),
        "no `run` key"
    );

    // Outside a service run: nothing is said, and no run is sent.
    let fake = world();
    let out = comment(&[], &fake, &["1", "outside a run"]);
    assert_eq!(stderr(&out), "");
    assert_eq!(
        fake.writes()[0].body,
        Some(json!({"body": "outside a run"}))
    );

    // The machine named: exactly the four fields, the instance as the string it is.
    let fake = world();
    let out = comment(
        &[
            ("AFKD_SCRATCH_DIR", scratch),
            ("AFKD_INSTANCE", "2"),
            (
                "AFKD_BOARD_RUN",
                r#"{"service":"develop","machine":"nova"}"#,
            ),
        ],
        &fake,
        &["1", "under a run"],
    );
    assert_eq!(stderr(&out), "");
    let run = json!({"machine": "nova", "service": "develop", "instance": "2",
                     "id": "261126-205930-unit-17-1"});
    assert_eq!(
        fake.writes()[0].body,
        Some(json!({"body": "under a run", "run": run}))
    );
    let made = fake.comments_on(1).pop().expect("the comment");
    assert_eq!(
        made.3,
        Some(Run {
            machine: "nova".to_string(),
            service: "develop".to_string(),
            instance: "2".to_string(),
            id: "261126-205930-unit-17-1".to_string(),
        })
    );

    // Left to its default the instance is "0", and a trailing slash does not hide the id.
    let fake = world();
    comment(
        &[
            ("AFKD_SCRATCH_DIR", "/tmp/afkd/runs/nova/develop/run-9/"),
            (
                "AFKD_BOARD_RUN",
                r#"{"service":"develop","machine":"nova"}"#,
            ),
        ],
        &fake,
        &["1", "default instance"],
    );
    assert_eq!(
        fake.writes()[0].body.as_ref().expect("a body")["run"],
        json!({"machine": "nova", "service": "develop", "instance": "0", "id": "run-9"})
    );

    // A list's change takes no run, and the board refuses one a script sent it.
    let fake = world();
    let mut create = command("create_list", &["Review"], &fake);
    create.env("AFKD_SCRATCH_DIR", scratch).env(
        "AFKD_BOARD_RUN",
        r#"{"service":"develop","machine":"nova"}"#,
    );
    let out = create.output().expect("run python3");
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(fake.writes()[0].body, Some(json!({"name": "Review"})));
    assert_eq!(fake.invalid_requests(), Vec::<String>::new());
}

/// Every write of a card, a checklist or a comment sends the run the same way, and a delete of a
/// card sends none: the board's API takes no body for it.
#[test]
fn every_write_of_a_card_sends_the_run_it_knows() {
    let fake = world();
    let env = [
        ("AFKD_SCRATCH_DIR", "/tmp/afkd/runs/nova/develop/run-9"),
        (
            "AFKD_BOARD_RUN",
            r#"{"service":"develop","machine":"nova"}"#,
        ),
    ];
    let write = |script: &str, argv: &[&str]| {
        let mut command = command(script, argv, &fake);
        for (name, value) in env {
            command.env(name, value);
        }
        let out = command.output().expect("run python3");
        assert!(out.status.success(), "{script} {argv:?}: {}", stderr(&out));
    };
    for (script, argv) in [
        ("create_card", vec!["Todo", "Made"]),
        ("update_card", vec!["2", "--title", "T"]),
        ("move_card", vec!["2", "Doing"]),
        ("add_label", vec!["2", "ready"]),
        ("remove_label", vec!["2", "ready"]),
        ("add_item", vec!["2", "x"]),
        ("check_item", vec!["2", "0"]),
        ("remove_item", vec!["2", "0"]),
        ("mark_done", vec!["2"]),
        ("add_blocked_by", vec!["2", "1"]),
        ("remove_blocked_by", vec!["2", "1"]),
        ("create_comment", vec!["2", "hej"]),
        ("archive_card", vec!["2"]),
        ("unarchive_card", vec!["2"]),
    ] {
        let before = fake.writes().len();
        write(script, &argv);
        let sent = &fake.writes()[before..];
        assert_eq!(sent.len(), 1, "{script}: {sent:?}");
        let body = sent[0].body.as_ref().expect("a body");
        assert_eq!(
            body["run"],
            json!({"machine": "nova", "service": "develop", "instance": "0", "id": "run-9"}),
            "{script}"
        );
    }
    let before = fake.writes().len();
    write("delete_card", &["2"]);
    assert_eq!(
        fake.writes()[before].body,
        None,
        "a delete of a card sends no body"
    );
    assert_eq!(fake.invalid_requests(), Vec::<String>::new());
}

/// A script behind a tunnelling proxy, as a sandbox with a network list runs it, reaches the board
/// through it.
#[test]
fn a_script_behind_a_proxy_reaches_the_board_through_it() {
    use std::io::{Read, Write};
    use std::net::{Shutdown, TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};

    let fake = world();
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind the proxy");
    let proxy = format!("http://{}", listener.local_addr().expect("addr"));
    let target = fake.base_url().trim_start_matches("http://").to_string();
    let tunnelled = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen = Arc::clone(&tunnelled);
    std::thread::spawn(move || {
        for client in listener.incoming().flatten() {
            let (seen, target) = (Arc::clone(&seen), target.clone());
            std::thread::spawn(move || {
                let mut client = client;
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    if client.read(&mut byte).unwrap_or(0) == 0 {
                        return;
                    }
                    head.push(byte[0]);
                }
                let line = String::from_utf8_lossy(&head)
                    .lines()
                    .next()
                    .unwrap_or("")
                    .to_string();
                seen.lock().expect("lock").push(line);
                let Ok(upstream) = TcpStream::connect(&target) else {
                    return;
                };
                let _ = client.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n");
                let (mut up_r, mut down_w) = (
                    upstream.try_clone().expect("clone"),
                    client.try_clone().expect("clone"),
                );
                let (mut cl_r, mut up_w) = (client, upstream);
                let t = std::thread::spawn(move || {
                    let _ = std::io::copy(&mut cl_r, &mut up_w);
                    let _ = up_w.shutdown(Shutdown::Write);
                });
                let _ = std::io::copy(&mut up_r, &mut down_w);
                let _ = down_w.shutdown(Shutdown::Write);
                let _ = t.join();
            });
        }
    });
    let mut command = command("lists", &[], &fake);
    command
        .env("http_proxy", &proxy)
        .env("HTTP_PROXY", &proxy)
        .env("NO_PROXY", "")
        .env("no_proxy", "");
    let out = command.output().expect("run python3");
    assert!(out.status.success(), "{}", stderr(&out));
    let lists: Value = serde_json::from_slice(&out.stdout).expect("JSON");
    assert_eq!(lists.as_array().expect("an array").len(), 4);
    let lines = tunnelled.lock().expect("lock").clone();
    assert!(
        lines.iter().any(|l| l.starts_with("CONNECT ")),
        "the script rode a tunnel: {lines:?}"
    );
}
