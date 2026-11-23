//! The skill's scripts, one per Trello operation, run as an agent runs them — `python3` on the
//! script, the board credentials in the environment — against the stateful fake Trello through
//! their `TRELLO_API_BASE` seam. Each leg parses what the script prints (a handle's `id`, `key`
//! and `fields`, a list as an array, nothing as `null`) and reads the fake back: what it then
//! holds, and that a refusal wrote nothing. The proxy legs run a script behind a tunnelling
//! proxy, as a sandbox with a network list does, and read back which requests rode a tunnel.

mod common;

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;

use serde_json::{json, Value};

use common::fake::{oauth_header, FakeTrello, Seen, BOARD, KEY, ME, TOKEN};
use common::TempDir;

/// The queue's four cards, top to bottom: wide CJK, an emoji, a ` - ` clause and a
/// backticked identifier, as this board's titles carry them.
const QUEUE: [(&str, &str); 4] = [
    (
        "Qa1Bc2De",
        "fix(trello): 修复 the claim race 🚨 - a rival's claim no longer wins",
    ),
    (
        "Rb2Cd3Ef",
        "feat(lang): `in` reads a list's membership - x in xs, not contains(xs, x)",
    ),
    (
        "Sc3De4Fg",
        "fix(tui): a wide glyph — 漢字 — no longer splits the footer",
    ),
    (
        "Td4Ef5Gh",
        "chore(web): the `check:browser` gate runs headless",
    ),
];

/// The follow-up a build files: a multi-line body with the Never park block and wide
/// glyphs.
const TITLE: &str = "fix(trello): 跟进 a drift red that is not gitea's - `wire.rs` reds on main";
const BODY: &str = "Split off from Mqn089od: the red is not gitea's.\n\n\
                    ## Problem\n\n`cargo test -p trello-drift` reds on 488931d:\n\n\
                    ```\nthe_manifest_transcribes_the_in_tree_tables ... FAILED\n```\n\n\
                    ## Never park (owner, 2026-09-30)\n\n\
                    Do **not** use `ask.py` on this card — 看 the block.\n\n\
                    ## Acceptance\n\n1. The drift gate is green.\n";

/// A comment with quotes, a newline, wide glyphs and the characters a query string escapes.
const COMMENT: &str = "plan: \"quoted\" 计划\nline 2 🚨 & 100% + more";

/// The placeholders SKILL.md shows, which a script refuses.
const TITLE_PLACEHOLDER: &str = "<REPLACE WITH YOUR CARD TITLE>";
const NAME_PLACEHOLDER: &str = "<REPLACE WITH YOUR NEW TITLE>";
const COMMENT_PLACEHOLDER: &str = "<REPLACE WITH YOUR COMMENT>";

/// The board every leg starts from: "Up for Grabs" holding the queue, and "Backlog"
/// holding one card; a member `phil` and two labels, `afkd/ready` and `Hold`.
struct Board {
    fake: FakeTrello,
    queue: String,
    backlog: String,
    /// The queue's card ids, top to bottom.
    cards: Vec<String>,
    /// The Backlog card's id.
    parked: String,
    /// `phil`'s member id.
    phil: String,
    /// The ids of the labels `afkd/ready` and `Hold`.
    ready: String,
    hold: String,
}

fn board() -> Board {
    let fake = FakeTrello::start();
    let queue = fake.list("Up for Grabs");
    let backlog = fake.list("Backlog");
    let cards = QUEUE
        .iter()
        .map(|(link, name)| fake.card("Up for Grabs", link, name, ""))
        .collect();
    let parked = fake.card(
        "Backlog",
        "Xx9Yy8Zz",
        "refactor(daemon): the run dir — ラン — is named once",
        "",
    );
    let phil = fake.member("phil", "Phil Ek");
    let ready = fake.board_label(BOARD, "afkd/ready", Some("green"));
    let hold = fake.board_label(BOARD, "Hold", Some("red"));
    Board {
        fake,
        queue,
        backlog,
        cards,
        parked,
        phil,
        ready,
        hold,
    }
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

/// One script with the fake's credentials and base, and none of the selfdev run's card or
/// board locators, nor any proxy the test's own environment names.
fn helper_command(script: &str, args: &[&str], fake: &FakeTrello) -> Command {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("skills/trello")
        .join(script);
    let mut command = Command::new("python3");
    command
        .arg(path)
        .args(args)
        .env("TRELLO_API_KEY", KEY)
        .env("TRELLO_TOKEN", TOKEN)
        .env("TRELLO_API_BASE", fake.base_url())
        .env_remove("TRELLO_CARD_ID")
        .env_remove("TRELLO_BOARD_ID")
        .env_remove("AFKD_SCRATCH_DIR")
        .env("PYTHONDONTWRITEBYTECODE", "1");
    for var in PROXY_VARS {
        command.env_remove(var);
    }
    command
}

/// Run one script, as [`helper_command`] sets it up.
fn helper(script: &str, args: &[&str], fake: &FakeTrello) -> Output {
    helper_command(script, args, fake)
        .output()
        .expect("run python3")
}

/// Name `proxy` in every variable afkd's sandbox hands a child, with `NO_PROXY` emptied so
/// the loopback fake stands in for a remote host.
fn behind_proxy<'a>(command: &'a mut Command, proxy: &str) -> &'a mut Command {
    for var in &PROXY_VARS[..6] {
        command.env(var, proxy);
    }
    command.env("NO_PROXY", "").env("no_proxy", "")
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// What a script printed, parsed as JSON.
fn json_of(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is JSON ({e}): {}",
            String::from_utf8_lossy(&out.stdout)
        )
    })
}

/// Run a script that must succeed and return what it printed, as JSON.
fn run(script: &str, args: &[&str], fake: &FakeTrello) -> Value {
    let out = helper(script, args, fake);
    assert!(out.status.success(), "{script} {args:?}: {}", stderr(&out));
    json_of(&out)
}

/// [`run`] with `env` set in the script's environment.
fn run_env(script: &str, args: &[&str], fake: &FakeTrello, env: &[(&str, &str)]) -> Value {
    let mut command = helper_command(script, args, fake);
    for (name, value) in env {
        command.env(name, value);
    }
    let out = command.output().expect("run python3");
    assert!(out.status.success(), "{script} {args:?}: {}", stderr(&out));
    json_of(&out)
}

/// Run a script that must refuse: it exits `code`, and its stderr is `<script>: ` and then a
/// sentence holding `needle`, and nothing on stdout.
fn refused(script: &str, args: &[&str], fake: &FakeTrello, code: i32, needle: &str) {
    let out = helper(script, args, fake);
    assert_eq!(
        out.status.code(),
        Some(code),
        "{script} {args:?}: {}",
        stderr(&out)
    );
    let said = stderr(&out);
    assert!(
        said.starts_with(&format!("{script}: ")) || code == 2,
        "{script}: {said}"
    );
    assert!(
        said.contains(needle),
        "{script}: {said}\n(wanted: {needle})"
    );
    assert!(out.stdout.is_empty(), "{script} printed on a refusal");
}

/// The writes a leg sent: every request but a read.
fn writes(fake: &FakeTrello) -> Vec<Seen> {
    fake.seen()
        .into_iter()
        .filter(|s| s.method != "GET")
        .collect()
}

/// The JSON body of a request.
fn body_of(seen: &Seen) -> Value {
    serde_json::from_str(&seen.body).expect("a JSON body")
}

/// The query of a request that carries only the credentials.
fn creds_only(seen: &Seen) {
    let mut query = seen.query.clone();
    query.sort();
    assert_eq!(
        query,
        [
            ("key".to_string(), KEY.to_string()),
            ("token".to_string(), TOKEN.to_string()),
        ]
    );
}

/// The `id` of each handle of an array.
fn ids(handles: &Value) -> Vec<String> {
    handles
        .as_array()
        .expect("an array of handles")
        .iter()
        .map(|h| h["id"].as_str().expect("an id").to_string())
        .collect()
}

/// The `fields.name` of each handle of an array.
fn names(handles: &Value) -> Vec<String> {
    handles
        .as_array()
        .expect("an array of handles")
        .iter()
        .map(|h| h["fields"]["name"].as_str().expect("a name").to_string())
        .collect()
}

/// The queue's short links, top to bottom.
fn links() -> Vec<String> {
    QUEUE.iter().map(|(link, _)| (*link).to_string()).collect()
}

/// A text file under a temp dir, as an agent writes one into its scratch dir.
fn text_file(dir: &TempDir, name: &str, text: &str) -> String {
    let path = dir.path().join(name);
    std::fs::write(&path, text).expect("write the file");
    path.to_string_lossy().into_owned()
}

// --- the environment ---------------------------------------------------------------------

#[test]
fn a_script_defaults_to_the_runs_card_and_board_and_names_what_is_unset() {
    let b = board();
    let card = &b.cards[0];
    // The run's card and board, from the environment.
    let read = run_env("card.py", &[], &b.fake, &[("TRELLO_CARD_ID", card)]);
    assert_eq!(read["id"], QUEUE[0].0);
    let lists = run_env(
        "lists.py",
        &[],
        &b.fake,
        &[("TRELLO_BOARD_ID", "https://trello.com/b/1Rkelydw/afkd")],
    );
    assert_eq!(names(&lists), ["Up for Grabs", "Backlog"]);
    // A flag beats it.
    let other = run_env(
        "card.py",
        &["--card", &b.parked],
        &b.fake,
        &[("TRELLO_CARD_ID", card)],
    );
    assert_eq!(other["id"], "Xx9Yy8Zz");

    // Neither: exit 1 naming the variable, before any request.
    let fresh = board();
    refused("card.py", &[], &fresh.fake, 1, "TRELLO_CARD_ID");
    refused("lists.py", &[], &fresh.fake, 1, "TRELLO_BOARD_ID");
    refused("delete_card.py", &[], &fresh.fake, 1, "TRELLO_CARD_ID");
    assert!(fresh.fake.seen().is_empty());

    // The credential unset: exit 1 naming it, before any request.
    for var in ["TRELLO_API_KEY", "TRELLO_TOKEN"] {
        let out = helper_command("card.py", &["--card", card], &fresh.fake)
            .env_remove(var)
            .output()
            .expect("run python3");
        assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
        assert!(
            stderr(&out).contains(&format!("{var} must be set in the environment")),
            "{}",
            stderr(&out)
        );
    }
    assert!(fresh.fake.seen().is_empty());
}

// --- reads -------------------------------------------------------------------------------

#[test]
fn card_prints_the_card_as_a_handle_without_a_title() {
    let b = board();
    let id = &b.cards[0];
    b.fake.label_card(id, &b.ready);
    b.fake.card_member(id, &b.phil);
    b.fake.due(id, 3600, true);

    let card = run("card.py", &["--card", id], &b.fake);
    assert_eq!(card["id"], QUEUE[0].0, "the short link");
    assert_eq!(card["key"], format!("trello:{id}"));
    let fields = &card["fields"];
    assert_eq!(fields["name"], QUEUE[0].1);
    assert_eq!(fields["number"], 1);
    assert_eq!(fields["description"], "");
    assert_eq!(fields["list"], "Up for Grabs");
    assert_eq!(fields["board"], BOARD);
    assert_eq!(fields["labels"], json!(["afkd/ready"]));
    assert_eq!(fields["members"], json!(["phil"]));
    assert_eq!(fields["due"], b.fake.due_millis_of(id).unwrap());
    assert_eq!(fields["due_complete"], true);
    assert_eq!(fields["archived"], false);
    assert!(fields["url"].as_str().unwrap().contains(QUEUE[0].0));
    assert!(fields["created_at"].as_u64().unwrap() > 0);
    assert!(fields["last_activity_at"].as_u64().unwrap() >= fields["created_at"].as_u64().unwrap());
    assert!(fields.get("title").is_none(), "a Card has no title");
    assert!(writes(&b.fake).is_empty());
}

#[test]
fn cards_lists_a_boards_cards_list_by_list_top_to_bottom() {
    let b = board();
    let all = run("cards.py", &["--board", BOARD], &b.fake);
    let mut want = links();
    want.push("Xx9Yy8Zz".to_string());
    assert_eq!(ids(&all), want);
    assert_eq!(all[0]["key"], format!("trello:{}", b.cards[0]));
    assert_eq!(all[0]["fields"]["list"], "Up for Grabs");
    assert_eq!(all[4]["fields"]["list"], "Backlog");
    assert_eq!(all[0]["fields"]["board"], BOARD);

    let two = run("cards.py", &["--board", BOARD, "--limit", "2"], &b.fake);
    assert_eq!(ids(&two), want[..2]);
    assert!(writes(&b.fake).is_empty());
}

#[test]
fn cards_keeps_what_every_filter_keeps() {
    let b = board();
    b.fake.label_card(&b.cards[0], &b.ready);
    b.fake.label_card(&b.cards[1], &b.ready);
    b.fake.label_card(&b.cards[1], &b.hold);
    b.fake.card_member(&b.cards[2], &b.phil);
    b.fake.card_member(&b.cards[3], &b.fake.me());

    let of = |args: &[&str]| {
        let mut all = vec!["--board", BOARD];
        all.extend_from_slice(args);
        ids(&run("cards.py", &all, &b.fake))
    };
    assert_eq!(of(&["--list", "Backlog"]), ["Xx9Yy8Zz"]);
    assert_eq!(
        of(&["--list", "Backlog", "--list", "Up for Grabs"]).len(),
        5,
        "any of the lists"
    );
    assert_eq!(of(&["--labels", "afkd/ready"]), links()[..2]);
    assert_eq!(
        of(&["--labels", "afkd/ready", "--labels", "Hold"]),
        [QUEUE[1].0],
        "every label named"
    );
    assert_eq!(
        of(&["--labels", "afkd/ready", "--without_labels", "Hold"]),
        [QUEUE[0].0]
    );
    assert_eq!(of(&["--members", "phil"]), [QUEUE[2].0]);
    assert_eq!(of(&["--members", "me"]), [QUEUE[3].0]);
    assert_eq!(
        of(&["--members", "me", "--members", "phil"]),
        [QUEUE[2].0, QUEUE[3].0],
        "any of the members"
    );
    assert!(writes(&b.fake).is_empty());
}

#[test]
fn cards_leaves_out_archived_cards_and_lists_unless_asked() {
    let b = board();
    b.fake.archive(&b.cards[0]);
    let kept = run("cards.py", &["--board", BOARD], &b.fake);
    assert_eq!(ids(&kept), ["Rb2Cd3Ef", "Sc3De4Fg", "Td4Ef5Gh", "Xx9Yy8Zz"]);

    let all = run(
        "cards.py",
        &["--board", BOARD, "--include_archived"],
        &b.fake,
    );
    assert_eq!(ids(&all).len(), 5);
    let archived = all
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == QUEUE[0].0)
        .expect("the archived card");
    assert_eq!(archived["fields"]["archived"], true);

    b.fake.archive_list(&b.backlog);
    let open = run("cards.py", &["--board", BOARD], &b.fake);
    assert!(!ids(&open).contains(&"Xx9Yy8Zz".to_string()), "{open}");
    let every = run(
        "cards.py",
        &["--board", BOARD, "--include_archived"],
        &b.fake,
    );
    assert!(ids(&every).contains(&"Xx9Yy8Zz".to_string()), "{every}");
}

#[test]
fn cards_refuses_a_name_the_board_lacks_before_it_reads_a_card() {
    let b = board();
    refused(
        "cards.py",
        &["--board", BOARD, "--list", "Nope"],
        &b.fake,
        1,
        "the board has no list named \"Nope\"; its lists are \"Up for Grabs\", \"Backlog\"",
    );
    refused(
        "cards.py",
        &["--board", BOARD, "--without_labels", "ghost"],
        &b.fake,
        1,
        "the board has no label named \"ghost\"; its labels are \"afkd/ready\", \"Hold\"",
    );
    refused(
        "cards.py",
        &["--board", BOARD, "--members", "ghost"],
        &b.fake,
        1,
        "the board has no member named \"ghost\"; its members are \"afkd-bot\", \"phil\"",
    );
    b.fake.list("Backlog");
    refused(
        "cards.py",
        &["--board", BOARD, "--list", "Backlog"],
        &b.fake,
        1,
        "2 lists are named \"Backlog\" (ids ",
    );
    assert!(
        b.fake.seen().iter().all(|s| !s.path.ends_with("/cards")),
        "no card was read"
    );

    // A limit outside 1 to 1000 is refused before any request.
    let fresh = board();
    for limit in ["0", "1001"] {
        refused(
            "cards.py",
            &["--board", BOARD, "--limit", limit],
            &fresh.fake,
            2,
            "outside 1 to 1000",
        );
    }
    refused(
        "cards.py",
        &["--board", "nope"],
        &fresh.fake,
        1,
        "no board \"nope\" on this account",
    );
}

#[test]
fn comments_lists_the_newest_oldest_first_with_their_authors() {
    let b = board();
    let card = &b.cards[0];
    let first = b.fake.comment(card, &b.fake.me(), "plan 计划 🚨", 300);
    let second = b.fake.comment(card, &b.phil, "second\nline", 200);
    let third = b.fake.comment(card, &b.phil, "third", 100);
    b.fake.edit_comment(&second, "second\nline, edited");

    let all = run("comments.py", &["--card", card], &b.fake);
    assert_eq!(ids(&all), [first.clone(), second.clone(), third.clone()]);
    assert_eq!(all[0]["key"], format!("trello:{first}"));
    let fields = &all[0]["fields"];
    assert_eq!(fields["card"], QUEUE[0].0, "the card's short link");
    assert_eq!(fields["body"], "plan 计划 🚨");
    assert_eq!(fields["author"], ME);
    assert!(fields["created_at"].as_u64().unwrap() > 0);
    assert_eq!(fields["edited_at"], Value::Null);
    assert_eq!(all[1]["fields"]["body"], "second\nline, edited");
    assert_eq!(all[1]["fields"]["author"], "phil");
    assert!(all[1]["fields"]["edited_at"].as_u64().is_some());

    let newest = run("comments.py", &["--card", card, "--limit", "2"], &b.fake);
    assert_eq!(ids(&newest), [second, third]);
    assert!(writes(&b.fake).is_empty());
}

#[test]
fn checklists_and_check_items_read_a_cards_checklists() {
    let b = board();
    let card = &b.cards[0];
    let items = b
        .fake
        .checklist(card, "Acceptance", &[("one", false), ("two 二", true)]);
    b.fake.checklist(card, "Later", &[("three", false)]);
    let rows = b.fake.checklists_on(card);

    let lists = run("checklists.py", &["--card", card], &b.fake);
    assert_eq!(ids(&lists), [rows[0].0.clone(), rows[1].0.clone()]);
    assert_eq!(lists[0]["key"], format!("trello:{}", rows[0].0));
    assert_eq!(lists[0]["fields"]["card"], QUEUE[0].0);
    assert_eq!(lists[0]["fields"]["name"], "Acceptance");
    assert_eq!(lists[0]["fields"]["items"], json!(["one", "two 二"]));

    let all = run("check_items.py", &["--card", card], &b.fake);
    assert_eq!(ids(&all)[..2], items);
    assert_eq!(all.as_array().unwrap().len(), 3);
    assert_eq!(all[0]["key"], format!("trello:{card}/{}", items[0]));
    assert_eq!(all[0]["fields"]["checklist"], rows[0].0.as_str());
    assert_eq!(all[0]["fields"]["card"], QUEUE[0].0);
    assert_eq!(all[0]["fields"]["name"], "one");
    assert_eq!(all[0]["fields"]["done"], false);
    assert_eq!(all[1]["fields"]["done"], true);
    assert_eq!(all[2]["fields"]["checklist"], rows[1].0.as_str());
}

#[test]
fn attachments_lists_files_and_links_oldest_first() {
    let b = board();
    let card = &b.cards[0];
    let link = b
        .fake
        .link_attachment(card, "spec", "https://example.com/spec", 60);
    let file = b.fake.attachment(card, "plan.md", b"plan\n", 120);

    let all = run("attachments.py", &["--card", card], &b.fake);
    assert_eq!(ids(&all), [file.clone(), link.clone()]);
    assert_eq!(all[0]["key"], format!("trello:{card}/{file}"));
    let up = &all[0]["fields"];
    assert_eq!(up["card"], QUEUE[0].0);
    assert_eq!(up["name"], "plan.md");
    assert_eq!(up["bytes"], 5);
    assert_eq!(up["mime_type"], "application/octet-stream");
    assert_eq!(up["is_upload"], true);
    assert!(up["url"].as_str().unwrap().contains("/download/"));
    assert!(up["created_at"].as_u64().unwrap() > 0);
    let url = &all[1]["fields"];
    assert_eq!(url["url"], "https://example.com/spec");
    assert_eq!(url["bytes"], Value::Null);
    assert_eq!(url["mime_type"], Value::Null);
    assert_eq!(url["is_upload"], false);
}

#[test]
fn me_boards_lists_labels_and_members_read_the_account() {
    let b = board();
    let me = run("me.py", &[], &b.fake);
    assert_eq!(me["id"], ME);
    assert_eq!(me["key"], format!("trello:{}", b.fake.me()));
    assert_eq!(me["fields"], json!({"full_name": "afkd"}));

    b.fake.board("Zz9Yy8Xx", "Other");
    let boards = run("boards.py", &[], &b.fake);
    assert_eq!(ids(&boards), [BOARD, "Zz9Yy8Xx"]);
    assert_eq!(
        boards[0]["key"],
        format!("trello:{}", b.fake.board_id(BOARD))
    );
    assert_eq!(names(&boards), ["afkd", "Other"]);
    assert_eq!(boards[1]["fields"]["archived"], false);
    assert_eq!(
        boards[1]["fields"]["url"],
        "https://trello.com/b/Zz9Yy8Xx/other"
    );
    b.fake.close_board("Zz9Yy8Xx");
    assert_eq!(ids(&run("boards.py", &[], &b.fake)), [BOARD]);

    let lists = run("lists.py", &["--board", BOARD], &b.fake);
    assert_eq!(ids(&lists), [b.queue.clone(), b.backlog.clone()]);
    assert_eq!(lists[0]["key"], format!("trello:{}", b.queue));
    assert_eq!(
        lists[0]["fields"],
        json!({"name": "Up for Grabs", "board": BOARD, "archived": false})
    );
    b.fake.archive_list(&b.backlog);
    assert_eq!(
        ids(&run("lists.py", &["--board", BOARD], &b.fake)),
        std::slice::from_ref(&b.queue),
        "archived lists are left out"
    );

    let plain = b.fake.board_label(BOARD, "", Some("green"));
    let labels = run("labels.py", &["--board", BOARD], &b.fake);
    assert_eq!(ids(&labels), [b.ready.clone(), b.hold.clone(), plain]);
    assert_eq!(
        labels[0]["fields"],
        json!({"name": "afkd/ready", "color": "green", "board": BOARD})
    );
    assert_eq!(labels[2]["fields"]["name"], "");

    let members = run("members.py", &["--board", BOARD], &b.fake);
    assert_eq!(ids(&members), [ME, "phil"]);
    assert_eq!(members[1]["key"], format!("trello:{}", b.phil));
    assert_eq!(members[1]["fields"], json!({"full_name": "Phil Ek"}));
    assert!(writes(&b.fake).is_empty());
}

// --- download_attachment.py ---------------------------------------------------------------

/// A screenshot pasted onto a card: wide glyphs and spaces in its name, and bytes no text
/// decoding or line-ending change leaves alone.
fn screenshot() -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
    bytes.extend((0..=255u8).cycle().take(4096));
    bytes.extend_from_slice(b"\r\n\n\r\0IEND\xaeB`\x82");
    bytes
}

/// The file is written to `to` byte for byte, `null` is printed, the download carries the OAuth
/// header and no query credential, and no temp file outlives the write.
#[test]
fn download_attachment_writes_the_file_to_the_path() {
    let b = board();
    let card = &b.cards[0];
    let shot = screenshot();
    b.fake
        .attachment(card, "older.png", b"an older attempt\n", 3600);
    let wanted = b
        .fake
        .attachment(card, "截图 2026-11-13 14.02.png", &shot, 60);
    let dir = TempDir::new("download");
    let to = dir.path().join("shot.png");
    let to_arg = to.to_string_lossy().into_owned();

    let printed = run(
        "download_attachment.py",
        &[&wanted, &to_arg, "--card", card],
        &b.fake,
    );
    assert_eq!(printed, Value::Null);
    assert!(
        std::fs::read(&to).unwrap() == shot,
        "the file came back changed"
    );
    let downloads: Vec<Seen> = b
        .fake
        .seen()
        .into_iter()
        .filter(|s| s.path.contains("/download/"))
        .collect();
    assert_eq!(downloads.len(), 1, "{downloads:?}");
    assert!(
        downloads[0]
            .path
            .contains(&format!("/attachments/{wanted}/")),
        "{}",
        downloads[0].path
    );
    assert_eq!(downloads[0].authorization, Some(oauth_header()));
    assert!(downloads[0].query.is_empty(), "{:?}", downloads[0].query);
    assert_eq!(
        std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with('.'))
            .count(),
        0,
        "no temp file outlives the write"
    );
    assert!(writes(&b.fake).is_empty());
}

#[test]
fn download_attachment_refuses_a_link_a_big_file_and_a_relative_path() {
    let b = board();
    let card = &b.cards[0];
    let link = b
        .fake
        .link_attachment(card, "spec", "https://example.com/spec", 60);
    let big = b
        .fake
        .attachment_sized(card, "big.bin", 10 * 1024 * 1024 + 1, 60);
    let small = b.fake.attachment(card, "plan.md", b"plan\n", 60);
    let dir = TempDir::new("download-refused");
    let to = dir.path().join("kept.md");
    std::fs::write(&to, "kept\n").unwrap();
    let to_arg = to.to_string_lossy().into_owned();

    refused(
        "download_attachment.py",
        &[&link, &to_arg, "--card", card],
        &b.fake,
        1,
        "is a link to https://example.com/spec, not a file Trello holds",
    );
    refused(
        "download_attachment.py",
        &[&big, &to_arg, "--card", card],
        &b.fake,
        1,
        "is 10485761 bytes, over the 10 MiB the plugin fetches",
    );
    refused(
        "download_attachment.py",
        &["0000000000000000deadbeef", &to_arg, "--card", card],
        &b.fake,
        1,
        "no attachment 0000000000000000deadbeef on card",
    );
    assert_eq!(std::fs::read_to_string(&to).unwrap(), "kept\n");
    assert!(
        b.fake.seen().iter().all(|s| !s.path.contains("/download/")),
        "nothing was downloaded"
    );

    let fresh = board();
    refused(
        "download_attachment.py",
        &[&small, "relative/shot.png", "--card", card],
        &fresh.fake,
        1,
        "to must be absolute, not \"relative/shot.png\"",
    );
    assert!(fresh.fake.seen().is_empty(), "refused before any request");
}

// --- create_card.py -----------------------------------------------------------------------

#[test]
fn create_card_files_a_card_with_its_labels_members_and_due() {
    let b = board();
    let dir = TempDir::new("create");
    let desc = text_file(&dir, "follow-up.md", BODY);
    let card = run(
        "create_card.py",
        &[
            "Up for Grabs",
            TITLE,
            "--board",
            BOARD,
            "--description_file",
            &desc,
            "--at",
            "top",
            "--labels",
            "afkd/ready",
            "--labels",
            "Hold",
            "--members",
            "me",
            "--members",
            "phil",
            "--due",
            "2026-12-24T10:00:00.250Z",
        ],
        &b.fake,
    );

    let filed = b.fake.cards_in("Up for Grabs");
    assert_eq!(filed.len(), 5);
    let new = &filed[0];
    assert!(!b.cards.contains(new), "a new card, at the top");
    assert_eq!(card["key"], format!("trello:{new}"));
    let fields = &card["fields"];
    assert_eq!(fields["name"], TITLE);
    assert_eq!(fields["description"], BODY);
    assert_eq!(fields["list"], "Up for Grabs");
    assert_eq!(fields["board"], BOARD);
    assert_eq!(fields["labels"], json!(["afkd/ready", "Hold"]));
    assert_eq!(fields["members"], json!([ME, "phil"]));
    assert_eq!(fields["due"], 1_798_106_400_250u64);
    assert!(fields.get("title").is_none());
    assert_eq!(b.fake.labels(new), ["afkd/ready", "Hold"]);
    assert_eq!(b.fake.members(new), [ME, "phil"]);
    assert_eq!(b.fake.due_millis_of(new), Some(1_798_106_400_250));

    let sent = writes(&b.fake);
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(
        (sent[0].method.as_str(), sent[0].path.as_str()),
        ("POST", "/1/cards")
    );
    creds_only(&sent[0]);
    assert_eq!(
        body_of(&sent[0]),
        json!({
            "idList": b.queue, "name": TITLE, "desc": BODY, "pos": "top",
            "idLabels": [b.ready, b.hold],
            "idMembers": [b.fake.me(), b.phil],
            "due": "2026-12-24T10:00:00.250Z",
        })
    );
}

#[test]
fn create_card_uses_the_runs_board_and_files_at_the_bottom() {
    let b = board();
    let card = run_env(
        "create_card.py",
        &["Up for Grabs", TITLE, "--description", BODY],
        &b.fake,
        &[("TRELLO_BOARD_ID", BOARD)],
    );
    let filed = b.fake.cards_in("Up for Grabs");
    let new = card["key"].as_str().unwrap().trim_start_matches("trello:");
    assert_eq!(filed.last().map(String::as_str), Some(new));
    assert_eq!(filed[..4], b.cards[..]);
    let sent = writes(&b.fake);
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(
        body_of(&sent[0]),
        json!({"idList": b.queue, "name": TITLE, "desc": BODY, "pos": "bottom"})
    );
    // A list by its id.
    let by_id = run_env(
        "create_card.py",
        &[&b.backlog, "by id"],
        &b.fake,
        &[("TRELLO_BOARD_ID", BOARD)],
    );
    assert_eq!(by_id["fields"]["list"], "Backlog");
}

#[test]
fn create_card_refuses_a_name_the_board_lacks_and_creates_nothing() {
    let b = board();
    let cases: [(&[&str], &str); 3] = [
        (
            &["Nope", "x"],
            "the board has no list named \"Nope\"; its lists are \"Up for Grabs\", \"Backlog\"",
        ),
        (
            &["Backlog", "x", "--labels", "ghost"],
            "the board has no label named \"ghost\"; its labels are \"afkd/ready\", \"Hold\"",
        ),
        (
            &["Backlog", "x", "--members", "ghost"],
            "the board has no member named \"ghost\"; its members are \"afkd-bot\", \"phil\"",
        ),
    ];
    for (args, said) in cases {
        let mut all = args.to_vec();
        all.extend_from_slice(&["--board", BOARD]);
        refused("create_card.py", &all, &b.fake, 1, said);
    }
    b.fake.list("Backlog");
    refused(
        "create_card.py",
        &["Backlog", "x", "--board", BOARD],
        &b.fake,
        1,
        "2 lists are named \"Backlog\" (ids ",
    );
    assert!(writes(&b.fake).is_empty(), "{:?}", writes(&b.fake));
    assert_eq!(b.fake.cards_in("Backlog"), std::slice::from_ref(&b.parked));
}

#[test]
fn create_card_refuses_the_placeholder_and_text_past_the_limit_before_any_request() {
    let b = board();
    refused(
        "create_card.py",
        &["Backlog", TITLE_PLACEHOLDER, "--board", BOARD],
        &b.fake,
        2,
        "placeholder",
    );
    let long = "é".repeat(16_385);
    refused(
        "create_card.py",
        &["Backlog", "x", "--board", BOARD, "--description", &long],
        &b.fake,
        1,
        "description is 16385 characters, over the 16384 Trello takes",
    );
    assert!(b.fake.seen().is_empty(), "refused before any request");

    // 16 384 characters, however many bytes, is within the limit.
    let edge = "é".repeat(16_384);
    let card = run(
        "create_card.py",
        &["Backlog", "x", "--board", BOARD, "--description", &edge],
        &b.fake,
    );
    assert_eq!(
        card["fields"]["description"]
            .as_str()
            .unwrap()
            .chars()
            .count(),
        16_384
    );
}

// --- update_card.py -----------------------------------------------------------------------

#[test]
fn update_card_changes_the_card_in_one_put() {
    let b = board();
    let card = &b.cards[0];
    let dir = TempDir::new("update");
    let desc = text_file(&dir, "desc.md", BODY);
    let updated = run(
        "update_card.py",
        &[
            "--card",
            card,
            "--name",
            "a new name 名",
            "--description_file",
            &desc,
            "--due",
            "2026-12-24T10:00:00Z",
            "--due_complete",
            "true",
        ],
        &b.fake,
    );
    assert_eq!(updated["id"], QUEUE[0].0);
    assert_eq!(updated["fields"]["name"], "a new name 名");
    assert_eq!(updated["fields"]["description"], BODY);
    assert_eq!(updated["fields"]["due"], 1_798_106_400_000u64);
    assert_eq!(updated["fields"]["due_complete"], true);
    assert_eq!(updated["fields"]["list"], "Up for Grabs", "not moved");
    assert_eq!(b.fake.cards_in("Up for Grabs"), b.cards, "not placed");
    assert!(b.fake.complete(card));

    let sent = writes(&b.fake);
    assert_eq!(sent.len(), 1, "one PUT: {sent:?}");
    assert_eq!(
        (sent[0].method.as_str(), sent[0].path.as_str()),
        ("PUT", format!("/1/cards/{card}").as_str())
    );
    creds_only(&sent[0]);
    assert_eq!(
        body_of(&sent[0]),
        json!({
            "name": "a new name 名", "desc": BODY, "due": "2026-12-24T10:00:00.000Z",
            "dueComplete": true,
        })
    );

    // `--clear due` takes the date away, and `--due_complete false` unmarks it.
    let cleared = run(
        "update_card.py",
        &["--card", card, "--clear", "due", "--due_complete", "false"],
        &b.fake,
    );
    assert_eq!(cleared["fields"]["due"], Value::Null);
    assert_eq!(cleared["fields"]["due_complete"], false);
    assert_eq!(b.fake.due_of(card), None);
    let sent = writes(&b.fake);
    assert_eq!(
        body_of(&sent[1]),
        json!({"due": null, "dueComplete": false})
    );
}

#[test]
fn update_card_with_no_flag_sends_nothing() {
    let b = board();
    let card = &b.cards[1];
    let read = run("update_card.py", &["--card", card], &b.fake);
    assert_eq!(read["id"], QUEUE[1].0);
    assert_eq!(read["fields"]["name"], QUEUE[1].1);
    assert!(writes(&b.fake).is_empty(), "{:?}", writes(&b.fake));
}

#[test]
fn update_card_moves_a_card_to_the_top_of_a_named_list() {
    let b = board();
    let moved = run(
        "update_card.py",
        &["--card", &b.parked, "--list", "Up for Grabs"],
        &b.fake,
    );
    assert_eq!(moved["fields"]["list"], "Up for Grabs");
    let [a, bb, c, d] = [0, 1, 2, 3].map(|i| b.cards[i].clone());
    assert_eq!(
        b.fake.cards_in("Up for Grabs"),
        [b.parked.clone(), a, bb, c, d],
        "a moved card lands at the top"
    );
    assert!(b.fake.cards_in("Backlog").is_empty());
    let sent = writes(&b.fake);
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(body_of(&sent[0]), json!({"idList": b.queue, "pos": "top"}));
}

#[test]
fn update_card_places_a_card_at_the_bottom_of_its_own_list_or_another() {
    let b = board();
    // `--at` alone places the card in the list it is in.
    run(
        "update_card.py",
        &["--card", &b.cards[0], "--at", "bottom"],
        &b.fake,
    );
    let [a, bb, c, d] = [0, 1, 2, 3].map(|i| b.cards[i].clone());
    assert_eq!(b.fake.cards_in("Up for Grabs"), [bb, c, d, a]);
    assert_eq!(
        body_of(&writes(&b.fake)[0]),
        json!({"idList": b.queue, "pos": "bottom"})
    );

    // A move to another list, by its id, at the bottom.
    run(
        "update_card.py",
        &["--card", &b.parked, "--list", &b.queue, "--at", "bottom"],
        &b.fake,
    );
    assert_eq!(b.fake.cards_in("Up for Grabs").last(), Some(&b.parked));
}

#[test]
fn update_card_resolves_a_list_on_the_cards_own_board() {
    let b = board();
    b.fake.board("Zz9Yy8Xx", "Other");
    let other = b.fake.list_on("Zz9Yy8Xx", "Backlog");
    run(
        "update_card.py",
        &["--card", &b.cards[0], "--list", "Backlog"],
        &b.fake,
    );
    assert_eq!(b.fake.list_of(&b.cards[0]), "Backlog");
    assert_eq!(
        b.fake.cards_in("Backlog"),
        [b.cards[0].clone(), b.parked.clone()],
        "the card's own board's Backlog, not {other}"
    );
}

#[test]
fn update_card_refuses_what_it_cannot_send_and_writes_nothing() {
    let b = board();
    let card = &b.cards[0];
    refused(
        "update_card.py",
        &["--card", card, "--list", "Nope"],
        &b.fake,
        1,
        "the board has no list named \"Nope\"; its lists are \"Up for Grabs\", \"Backlog\"",
    );
    assert!(writes(&b.fake).is_empty(), "{:?}", writes(&b.fake));

    let fresh = board();
    refused(
        "update_card.py",
        &["--card", card, "--name", NAME_PLACEHOLDER],
        &fresh.fake,
        2,
        "placeholder",
    );
    let long = "x".repeat(16_385);
    refused(
        "update_card.py",
        &["--card", card, "--description", &long],
        &fresh.fake,
        1,
        "description is 16385 characters, over the 16384 Trello takes",
    );
    refused(
        "update_card.py",
        &[
            "--card",
            card,
            "--description",
            "a",
            "--description_file",
            "/x",
        ],
        &fresh.fake,
        2,
        "not allowed with",
    );
    refused(
        "update_card.py",
        &["--card", card, "--due", "next tuesday"],
        &fresh.fake,
        2,
        "no RFC 3339 time",
    );
    refused(
        "update_card.py",
        &[
            "--card",
            card,
            "--due",
            "2026-12-24T10:00:00Z",
            "--clear",
            "due",
        ],
        &fresh.fake,
        2,
        "not allowed with",
    );
    assert!(fresh.fake.seen().is_empty(), "refused before any request");
}

// --- archive_card.py, unarchive_card.py, delete_card.py --------------------------------------

#[test]
fn archive_unarchive_and_delete_a_card() {
    let b = board();
    let card = &b.cards[0];
    let archived = run("archive_card.py", &["--card", card], &b.fake);
    assert_eq!(archived["fields"]["archived"], true);
    assert_eq!(archived["id"], QUEUE[0].0);
    assert!(b.fake.archived(card));
    assert!(!b.fake.cards_in("Up for Grabs").contains(card));

    let back = run("unarchive_card.py", &["--card", card], &b.fake);
    assert_eq!(back["fields"]["archived"], false);
    assert!(!b.fake.archived(card));
    assert!(b.fake.cards_in("Up for Grabs").contains(card));

    let gone = run("delete_card.py", &["--card", card], &b.fake);
    assert_eq!(gone, Value::Null);
    assert!(!b.fake.has_card(card));
    assert!(b.fake.has_card(&b.cards[1]));
    let sent = writes(&b.fake);
    assert_eq!(
        sent.iter().map(|s| s.method.as_str()).collect::<Vec<_>>(),
        ["PUT", "PUT", "DELETE"]
    );
    assert_eq!(body_of(&sent[0]), json!({"closed": true}));
    assert_eq!(body_of(&sent[1]), json!({"closed": false}));

    refused(
        "delete_card.py",
        &["--card", card],
        &b.fake,
        1,
        "trello delete card: board returned status 404",
    );
}

// --- comments -----------------------------------------------------------------------------

#[test]
fn create_comment_posts_the_text_verbatim_as_a_json_body() {
    let b = board();
    let card = &b.cards[0];
    let comment = run("create_comment.py", &[COMMENT, "--card", card], &b.fake);
    let posted = b.fake.comments(card);
    assert_eq!(posted.len(), 1);
    assert_eq!(posted[0].text, COMMENT);
    assert_eq!(comment["id"], posted[0].id.as_str());
    assert_eq!(comment["key"], format!("trello:{}", posted[0].id));
    assert_eq!(comment["fields"]["body"], COMMENT);
    assert_eq!(comment["fields"]["card"], QUEUE[0].0);
    assert_eq!(comment["fields"]["author"], ME);
    assert_eq!(comment["fields"]["edited_at"], Value::Null);

    let sent = writes(&b.fake);
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0].path, format!("/1/cards/{card}/actions/comments"));
    creds_only(&sent[0]);
    assert_eq!(body_of(&sent[0]), json!({"text": COMMENT}));

    // The text from a file, and the run's card.
    let dir = TempDir::new("comment");
    let file = text_file(&dir, "comment.md", BODY);
    let from_file = run_env(
        "create_comment.py",
        &["--body_file", &file],
        &b.fake,
        &[("TRELLO_CARD_ID", card)],
    );
    assert_eq!(from_file["fields"]["body"], BODY);
    assert_eq!(b.fake.comments(card).len(), 2);
}

#[test]
fn create_comment_refuses_the_placeholder_an_empty_text_and_text_past_the_limit() {
    let b = board();
    let card = &b.cards[0];
    refused(
        "create_comment.py",
        &[COMMENT_PLACEHOLDER, "--card", card],
        &b.fake,
        2,
        "placeholder",
    );
    refused(
        "create_comment.py",
        &["  ", "--card", card],
        &b.fake,
        2,
        "empty",
    );
    refused(
        "create_comment.py",
        &["a", "--body_file", "/x", "--card", card],
        &b.fake,
        2,
        "not both",
    );
    let long = "é".repeat(16_385);
    refused(
        "create_comment.py",
        &[&long, "--card", card],
        &b.fake,
        1,
        "body is 16385 characters, over the 16384 Trello takes",
    );
    assert!(b.fake.seen().is_empty(), "refused before any request");
    assert!(b.fake.comments(card).is_empty());

    let edge = "é".repeat(16_384);
    let ok = run("create_comment.py", &[&edge, "--card", card], &b.fake);
    assert_eq!(
        ok["fields"]["body"].as_str().unwrap().chars().count(),
        16_384
    );
}

#[test]
fn update_comment_replaces_a_comment_the_token_wrote_and_only_that() {
    let b = board();
    let card = &b.cards[0];
    let own = b.fake.comment(card, &b.fake.me(), "old", 60);
    let theirs = b.fake.comment(card, &b.phil, "theirs", 60);

    let edited = run("update_comment.py", &[&own, COMMENT], &b.fake);
    assert_eq!(edited["id"], own.as_str());
    assert_eq!(edited["fields"]["body"], COMMENT);
    assert_eq!(edited["fields"]["card"], QUEUE[0].0);
    assert!(edited["fields"]["edited_at"].as_u64().is_some());
    let sent = writes(&b.fake);
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(
        (sent[0].method.as_str(), sent[0].path.as_str()),
        ("PUT", format!("/1/actions/{own}").as_str())
    );
    assert_eq!(body_of(&sent[0]), json!({"text": COMMENT}));

    refused(
        "update_comment.py",
        &[&theirs, "mine now"],
        &b.fake,
        1,
        "board returned status 401: unauthorized permission requested",
    );
    let texts: Vec<String> = b.fake.comments(card).into_iter().map(|c| c.text).collect();
    assert_eq!(texts, [COMMENT, "theirs"]);

    refused(
        "update_comment.py",
        &[&own, COMMENT_PLACEHOLDER],
        &b.fake,
        2,
        "placeholder",
    );
    let long = "x".repeat(16_385);
    refused(
        "update_comment.py",
        &[&own, &long],
        &b.fake,
        1,
        "body is 16385 characters",
    );
}

#[test]
fn delete_comment_deletes_a_comment_the_token_wrote_and_only_that() {
    let b = board();
    let card = &b.cards[0];
    let own = b.fake.comment(card, &b.fake.me(), "mine", 60);
    let theirs = b.fake.comment(card, &b.phil, "theirs", 60);
    refused(
        "delete_comment.py",
        &[&theirs],
        &b.fake,
        1,
        "board returned status 401: unauthorized permission requested",
    );
    assert!(b.fake.has_comment(&theirs));

    let gone = run("delete_comment.py", &[&own], &b.fake);
    assert_eq!(gone, Value::Null);
    assert!(!b.fake.has_comment(&own));
    assert!(b.fake.has_comment(&theirs));
}

// --- checklists ---------------------------------------------------------------------------

#[test]
fn create_checklist_makes_the_checklist_and_its_items_in_order() {
    let b = board();
    let card = &b.cards[0];
    let list = run(
        "create_checklist.py",
        &[
            "Acceptance",
            "--items",
            "first",
            "--items",
            "second 二",
            "--card",
            card,
        ],
        &b.fake,
    );
    let rows = b.fake.checklists_on(card);
    assert_eq!(rows.len(), 1);
    assert_eq!(list["id"], rows[0].0.as_str());
    assert_eq!(list["key"], format!("trello:{}", rows[0].0));
    assert_eq!(
        list["fields"],
        json!({"card": QUEUE[0].0, "name": "Acceptance", "items": ["first", "second 二"]})
    );
    let items: Vec<&str> = rows[0].2.iter().map(|i| i.1.as_str()).collect();
    assert_eq!(items, ["first", "second 二"]);

    let bare = run("create_checklist.py", &["Empty", "--card", card], &b.fake);
    assert_eq!(bare["fields"]["items"], json!([]));
    assert_eq!(b.fake.checklists_on(card).len(), 2);
}

#[test]
fn create_checklist_says_how_far_it_got_when_an_item_is_refused() {
    let b = board();
    let card = &b.cards[0];
    b.fake.fail("create check item", 400);
    refused(
        "create_checklist.py",
        &["Acceptance", "--items", "a", "--items", "b", "--card", card],
        &b.fake,
        1,
        "was made on card Qa1Bc2De with 0 of its 2 items, then: trello create check item: \
         board returned status 400: injected",
    );
    let rows = b.fake.checklists_on(card);
    assert_eq!(rows.len(), 1, "the checklist stays, as far as it got");
    assert!(rows[0].2.is_empty());
}

#[test]
fn delete_checklist_deletes_the_checklist() {
    let b = board();
    let card = &b.cards[0];
    b.fake.checklist(card, "Acceptance", &[("one", false)]);
    b.fake.checklist(card, "Later", &[]);
    let rows = b.fake.checklists_on(card);
    let gone = run("delete_checklist.py", &[&rows[0].0], &b.fake);
    assert_eq!(gone, Value::Null);
    let left = b.fake.checklists_on(card);
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].1, "Later");
}

#[test]
fn create_check_item_adds_an_item_at_the_bottom_or_the_top() {
    let b = board();
    let card = &b.cards[0];
    b.fake.checklist(card, "Acceptance", &[("one", false)]);
    let list = b.fake.checklists_on(card)[0].0.clone();

    let bottom = run("create_check_item.py", &[&list, "last 末"], &b.fake);
    let top = run(
        "create_check_item.py",
        &[&list, "first", "--at", "top"],
        &b.fake,
    );
    let rows = b.fake.checklists_on(card);
    let items: Vec<&str> = rows[0].2.iter().map(|i| i.1.as_str()).collect();
    assert_eq!(items, ["first", "one", "last 末"]);
    assert_eq!(bottom["id"], rows[0].2[2].0.as_str());
    assert_eq!(bottom["key"], format!("trello:{card}/{}", rows[0].2[2].0));
    assert_eq!(
        bottom["fields"],
        json!({"card": QUEUE[0].0, "checklist": list, "name": "last 末", "done": false})
    );
    assert_eq!(top["id"], rows[0].2[0].0.as_str());
    let sent = writes(&b.fake);
    assert_eq!(
        sent[0].query.iter().find(|(k, _)| k == "pos").unwrap().1,
        "bottom"
    );
    assert_eq!(
        sent[1].query.iter().find(|(k, _)| k == "pos").unwrap().1,
        "top"
    );
    assert_eq!(body_of(&sent[1]), json!({"name": "first"}));
}

#[test]
fn update_check_item_ticks_unticks_and_renames() {
    let b = board();
    let card = &b.cards[0];
    let items = b
        .fake
        .checklist(card, "Acceptance", &[("one", false), ("two", false)]);

    let done = run(
        "update_check_item.py",
        &[&items[0], "--done", "--card", card],
        &b.fake,
    );
    assert_eq!(done["id"], items[0].as_str());
    assert_eq!(done["key"], format!("trello:{card}/{}", items[0]));
    assert_eq!(done["fields"]["done"], true);
    assert_eq!(done["fields"]["name"], "one");
    assert!(b.fake.checklists_on(card)[0].2[0].2);
    assert!(
        !b.fake.checklists_on(card)[0].2[1].2,
        "the other is untouched"
    );

    let undone = run(
        "update_check_item.py",
        &[&items[0], "--undone", "--name", "uno", "--card", card],
        &b.fake,
    );
    assert_eq!(undone["fields"]["done"], false);
    assert_eq!(undone["fields"]["name"], "uno");
    let sent = writes(&b.fake);
    assert_eq!(sent.len(), 2);
    assert_eq!(body_of(&sent[0]), json!({"state": "complete"}));
    assert_eq!(
        body_of(&sent[1]),
        json!({"name": "uno", "state": "incomplete"})
    );

    // Neither flag, by the run's card: nothing is sent.
    let same = run_env(
        "update_check_item.py",
        &[&items[1]],
        &b.fake,
        &[("TRELLO_CARD_ID", card)],
    );
    assert_eq!(same["fields"]["name"], "two");
    assert_eq!(writes(&b.fake).len(), 2, "no PUT");

    refused(
        "update_check_item.py",
        &[&items[1], "--done", "--undone", "--card", card],
        &b.fake,
        2,
        "not allowed with",
    );
}

#[test]
fn delete_check_item_deletes_the_item() {
    let b = board();
    let card = &b.cards[0];
    let items = b
        .fake
        .checklist(card, "Acceptance", &[("one", false), ("two", true)]);
    let gone = run(
        "delete_check_item.py",
        &[&items[0], "--card", card],
        &b.fake,
    );
    assert_eq!(gone, Value::Null);
    let rows = b.fake.checklists_on(card);
    assert_eq!(rows[0].2.len(), 1);
    assert_eq!(rows[0].2[0].1, "two");
}

// --- attachments --------------------------------------------------------------------------

#[test]
fn create_attachment_uploads_the_file_under_its_own_name_or_a_given_one() {
    let b = board();
    let card = &b.cards[0];
    let dir = TempDir::new("attach");
    let shot = screenshot();
    let path = dir.path().join("shot.png");
    std::fs::write(&path, &shot).unwrap();
    let path_arg = path.to_string_lossy().into_owned();

    let own = run(
        "create_attachment.py",
        &[&path_arg, "--card", card],
        &b.fake,
    );
    assert_eq!(own["fields"]["name"], "shot.png");
    assert_eq!(own["fields"]["bytes"], shot.len());
    assert_eq!(own["fields"]["is_upload"], true);
    assert_eq!(own["fields"]["card"], QUEUE[0].0);
    assert_eq!(
        own["key"],
        format!("trello:{card}/{}", own["id"].as_str().unwrap())
    );
    let named = run(
        "create_attachment.py",
        &[&path_arg, "--name", "截图 final.png", "--card", card],
        &b.fake,
    );
    assert_eq!(named["fields"]["name"], "截图 final.png");

    let held = b.fake.attachments(card);
    assert_eq!(
        held.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
        ["shot.png", "截图 final.png"]
    );
    assert!(
        held.iter().all(|(_, bytes)| *bytes == shot),
        "byte for byte"
    );
    let posts = writes(&b.fake);
    assert_eq!(posts.len(), 2);
    assert!(posts[0]
        .content_type
        .as_deref()
        .is_some_and(|t| t.starts_with("multipart/form-data; boundary=")));
    creds_only(&posts[0]);
}

#[test]
fn create_attachment_refuses_what_it_cannot_read_before_any_request() {
    let b = board();
    let card = &b.cards[0];
    let dir = TempDir::new("attach-refused");
    let missing = dir.path().join("missing.md").to_string_lossy().into_owned();
    let folder = dir.path().to_string_lossy().into_owned();
    let big = dir.path().join("big.bin");
    std::fs::File::create(&big)
        .unwrap()
        .set_len(10 * 1024 * 1024 + 1)
        .unwrap();
    let big = big.to_string_lossy().into_owned();
    let small = text_file(&dir, "small.md", "small\n");

    refused(
        "create_attachment.py",
        &["relative.md", "--card", card],
        &b.fake,
        1,
        "path must be absolute, not \"relative.md\"",
    );
    refused(
        "create_attachment.py",
        &[&missing, "--card", card],
        &b.fake,
        1,
        &format!("no file at {missing}"),
    );
    refused(
        "create_attachment.py",
        &[&folder, "--card", card],
        &b.fake,
        1,
        &format!("{folder} is not a regular file"),
    );
    refused(
        "create_attachment.py",
        &[&big, "--card", card],
        &b.fake,
        1,
        "is over the 10 MiB Trello takes in one attachment",
    );
    refused(
        "create_attachment.py",
        &[&small, "--name", "", "--card", card],
        &b.fake,
        1,
        "name must not be empty",
    );
    assert!(b.fake.seen().is_empty(), "refused before any request");
    assert!(b.fake.attachments(card).is_empty());
}

#[test]
fn delete_attachment_takes_the_attachment_off_the_card() {
    let b = board();
    let card = &b.cards[0];
    let keep = b.fake.attachment(card, "keep.md", b"keep\n", 120);
    let drop = b.fake.attachment(card, "drop.md", b"drop\n", 60);
    let gone = run("delete_attachment.py", &[&drop, "--card", card], &b.fake);
    assert_eq!(gone, Value::Null);
    assert!(!b.fake.has_attachment(&drop));
    assert!(b.fake.has_attachment(&keep));
}

// --- labels and members -------------------------------------------------------------------

#[test]
fn add_label_puts_a_board_label_on_the_card_by_name_or_id() {
    let b = board();
    let card = &b.cards[0];
    let named = run("add_label.py", &["afkd/ready", "--card", card], &b.fake);
    assert_eq!(named["id"], QUEUE[0].0);
    assert_eq!(named["fields"]["labels"], json!(["afkd/ready"]));
    assert_eq!(b.fake.labels(card), ["afkd/ready"]);
    let sent = writes(&b.fake);
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(
        (sent[0].method.as_str(), sent[0].path.as_str()),
        ("POST", format!("/1/cards/{card}/idLabels").as_str())
    );
    assert!(sent[0]
        .query
        .contains(&("value".to_string(), b.ready.clone())));

    let by_id = run("add_label.py", &[&b.hold, "--card", card], &b.fake);
    assert_eq!(by_id["fields"]["labels"], json!(["afkd/ready", "Hold"]));

    // Already on the card: the card is printed, and no POST goes out.
    let again = run("add_label.py", &["Hold", "--card", card], &b.fake);
    assert_eq!(again["fields"]["labels"], json!(["afkd/ready", "Hold"]));
    assert_eq!(writes(&b.fake).len(), 2, "no third POST");
}

#[test]
fn add_label_never_creates_a_label_the_board_lacks() {
    let b = board();
    let card = &b.cards[0];
    refused(
        "add_label.py",
        &["Needs Info", "--card", card],
        &b.fake,
        1,
        "the board has no label named \"Needs Info\"; its labels are \"afkd/ready\", \"Hold\"",
    );
    assert!(writes(&b.fake).is_empty(), "{:?}", writes(&b.fake));
    assert!(b.fake.labels(card).is_empty());
    let labels = run("labels.py", &["--board", BOARD], &b.fake);
    assert_eq!(names(&labels), ["afkd/ready", "Hold"], "none created");

    // Two labels of one name are refused with their ids.
    let twin = b.fake.board_label(BOARD, "Hold", Some("yellow"));
    refused(
        "add_label.py",
        &["Hold", "--card", card],
        &b.fake,
        1,
        &format!("2 labels are named \"Hold\" (ids {}, {twin})", b.hold),
    );
    assert!(writes(&b.fake).is_empty());
}

#[test]
fn remove_label_takes_a_label_off_and_prints_the_card_when_it_is_not_there() {
    let b = board();
    let card = &b.cards[0];
    b.fake.label_card(card, &b.ready);
    b.fake.label_card(card, &b.hold);

    let removed = run("remove_label.py", &["afkd/ready", "--card", card], &b.fake);
    assert_eq!(removed["fields"]["labels"], json!(["Hold"]));
    assert_eq!(b.fake.labels(card), ["Hold"]);
    let sent = writes(&b.fake);
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(
        (sent[0].method.as_str(), sent[0].path.as_str()),
        (
            "DELETE",
            format!("/1/cards/{card}/idLabels/{}", b.ready).as_str()
        )
    );

    // A label the card lacks, by name and by id, and one the board lacks: the card, no write.
    for label in ["afkd/ready", b.ready.as_str(), "No Such Label"] {
        let same = run("remove_label.py", &[label, "--card", card], &b.fake);
        assert_eq!(same["fields"]["labels"], json!(["Hold"]), "{label}");
    }
    assert_eq!(writes(&b.fake).len(), 1, "no further DELETE");
}

#[test]
fn add_member_defaults_to_me_and_refuses_a_username_the_board_lacks() {
    let b = board();
    let card = &b.cards[0];
    let me = run("add_member.py", &["--card", card], &b.fake);
    assert_eq!(me["fields"]["members"], json!([ME]));
    assert_eq!(b.fake.members(card), [ME]);
    let sent = writes(&b.fake);
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(
        (sent[0].method.as_str(), sent[0].path.as_str()),
        ("POST", format!("/1/cards/{card}/idMembers").as_str())
    );
    assert!(sent[0].query.contains(&("value".to_string(), b.fake.me())));

    let phil = run("add_member.py", &["phil", "--card", card], &b.fake);
    assert_eq!(phil["fields"]["members"], json!([ME, "phil"]));

    // Already on the card: the card is printed and no POST goes out.
    let again = run("add_member.py", &["me", "--card", card], &b.fake);
    assert_eq!(again["fields"]["members"], json!([ME, "phil"]));
    assert_eq!(writes(&b.fake).len(), 2, "no third POST");

    refused(
        "add_member.py",
        &["ghost", "--card", card],
        &b.fake,
        1,
        "the board has no member named \"ghost\"; its members are \"afkd-bot\", \"phil\"",
    );
    assert_eq!(writes(&b.fake).len(), 2);
}

#[test]
fn remove_member_defaults_to_me_and_prints_the_card_when_they_are_not_there() {
    let b = board();
    let card = &b.cards[0];
    b.fake.card_member(card, &b.fake.me());
    b.fake.card_member(card, &b.phil);

    let removed = run("remove_member.py", &["--card", card], &b.fake);
    assert_eq!(removed["fields"]["members"], json!(["phil"]));
    assert_eq!(b.fake.members(card), ["phil"]);
    let sent = writes(&b.fake);
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(
        (sent[0].method.as_str(), sent[0].path.as_str()),
        (
            "DELETE",
            format!("/1/cards/{card}/idMembers/{}", b.fake.me()).as_str()
        )
    );

    // Not on the card, and not on the board: the card, no write.
    for who in ["me", "ghost"] {
        let same = run("remove_member.py", &[who, "--card", card], &b.fake);
        assert_eq!(same["fields"]["members"], json!(["phil"]), "{who}");
    }
    assert_eq!(writes(&b.fake).len(), 1, "no further DELETE");

    let gone = run("remove_member.py", &["phil", "--card", card], &b.fake);
    assert_eq!(gone["fields"]["members"], json!([]));
}

// --- ask.py -------------------------------------------------------------------------------

#[test]
fn ask_posts_the_question_then_writes_the_park_marker() {
    let b = board();
    let card = &b.cards[0];
    let scratch = TempDir::new("ask");
    let out = helper_command("ask.py", &[COMMENT, "--card", card], &b.fake)
        .env("AFKD_SCRATCH_DIR", scratch.path())
        .output()
        .expect("run python3");
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("parked the run awaiting a reply"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let posted = b.fake.comments(card);
    assert_eq!(posted.len(), 1);
    assert_eq!(posted[0].text, COMMENT);
    assert!(scratch.path().join("park").is_file(), "the park marker");

    // No scratch dir to park in: nothing is posted. The placeholder is refused.
    let fresh = board();
    let out = helper(
        "ask.py",
        &["a question", "--card", &fresh.cards[0]],
        &fresh.fake,
    );
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("AFKD_SCRATCH_DIR"),
        "{}",
        stderr(&out)
    );
    let out = helper_command(
        "ask.py",
        &["<REPLACE WITH YOUR QUESTIONS>", "--card", &fresh.cards[0]],
        &fresh.fake,
    )
    .env("AFKD_SCRATCH_DIR", scratch.path())
    .output()
    .expect("run python3");
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(fresh.fake.seen().is_empty(), "nothing was posted");
}

// --- the proxy -----------------------------------------------------------------------------

/// One connection the proxy tunnelled: the `CONNECT` head it was sent, and the request
/// line it carried.
#[derive(Debug, Clone)]
struct Tunnel {
    head: String,
    request: String,
}

/// A proxy on a loopback port that answers every `CONNECT` by tunnelling to where it asks,
/// and records each tunnel; its URL, with credentials, and the records.
///
/// A tunnel is recorded before its request goes upstream, so every record is in place once
/// the script has its replies.
fn tunnelling_proxy() -> (String, Arc<Mutex<Vec<Tunnel>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let at = listener.local_addr().expect("its address");
    let tunnels = Arc::new(Mutex::new(Vec::new()));
    let records = Arc::clone(&tunnels);
    thread::spawn(move || {
        for client in listener.incoming().flatten() {
            let records = Arc::clone(&records);
            thread::spawn(move || tunnel(client, &records));
        }
    });
    (format!("http://afkd:t0k3n@{at}"), tunnels)
}

/// Serve one `CONNECT`: answer it, record it with the request line it carries, and copy
/// both ways until each side is done.
fn tunnel(mut client: TcpStream, records: &Mutex<Vec<Tunnel>>) {
    let head = read_until(&mut client, b"\r\n\r\n");
    let target = head.split(' ').nth(1).expect("a CONNECT target").to_owned();
    let mut upstream = TcpStream::connect(&target).expect("the fake answers");
    client
        .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
        .expect("answer the CONNECT");
    let request = read_until(&mut client, b"\r\n");
    records
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(Tunnel {
            head,
            request: request.clone(),
        });
    upstream
        .write_all(request.as_bytes())
        .expect("pass the request line on");
    let (mut up_read, mut up_write) = (upstream.try_clone().expect("clone"), upstream);
    let mut client_read = client.try_clone().expect("clone");
    let back = thread::spawn(move || {
        let _ = std::io::copy(&mut up_read, &mut client);
        let _ = client.shutdown(Shutdown::Write);
    });
    let _ = std::io::copy(&mut client_read, &mut up_write);
    let _ = up_write.shutdown(Shutdown::Write);
    let _ = back.join();
}

/// Read a stream byte by byte through the first `end`, as text.
fn read_until(stream: &mut TcpStream, end: &[u8]) -> String {
    let mut text = Vec::new();
    let mut byte = [0u8];
    while !text.ends_with(end) {
        stream
            .read_exact(&mut byte)
            .expect("a line through its end");
        text.push(byte[0]);
    }
    String::from_utf8(text).expect("a text line")
}

/// The fake's `host:port`, as a `CONNECT` names it.
fn authority(fake: &FakeTrello) -> &str {
    let api = fake.base_url().trim_start_matches("http://");
    api.split('/').next().unwrap_or(api)
}

/// The tunnels whose request line starts with `request`, each asserted to be a `CONNECT` to
/// the fake carrying the proxy URL's credentials.
fn tunnels_for(tunnels: &Mutex<Vec<Tunnel>>, fake: &FakeTrello, request: &str) -> Vec<Tunnel> {
    let tunnels = tunnels.lock().unwrap_or_else(PoisonError::into_inner);
    let carrying: Vec<Tunnel> = tunnels
        .iter()
        .filter(|t| t.request.starts_with(request))
        .cloned()
        .collect();
    for t in &carrying {
        assert!(
            t.head.starts_with(&format!("CONNECT {} ", authority(fake))),
            "{t:?}"
        );
        assert!(
            t.head
                .contains("Proxy-Authorization: Basic YWZrZDp0MGszbg==\r\n"),
            "{t:?}"
        );
    }
    carrying
}

#[test]
fn a_script_reaches_the_api_through_the_proxy_its_environment_names() {
    // A sandbox with a network list hands its processes its proxy in the `*_PROXY`
    // variables and lets no connection past it (afkd's ADR-0084), so a script tunnels its
    // requests through the proxy, with the credentials the proxy URL carries.
    let b = board();
    let (proxy, tunnels) = tunnelling_proxy();
    let out = behind_proxy(
        &mut helper_command(
            "update_card.py",
            &["--card", b.parked.as_str(), "--list", "Up for Grabs"],
            &b.fake,
        ),
        &proxy,
    )
    .output()
    .expect("run python3");
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(json_of(&out)["fields"]["list"], "Up for Grabs");
    let moved = tunnels_for(&tunnels, &b.fake, &format!("PUT /1/cards/{}?", b.parked));
    assert_eq!(moved.len(), 1, "{:?}", tunnels.lock().unwrap());
    assert_eq!(b.fake.cards_in("Up for Grabs")[0], b.parked);
}

/// Behind the proxy, the attachment's download rides a tunnel too, past the metadata read:
/// the file comes through byte for byte, and the OAuth header with it.
#[test]
fn download_attachment_downloads_through_the_proxy_its_environment_names() {
    let b = board();
    let card = &b.cards[0];
    let shot = screenshot();
    let id = b
        .fake
        .attachment(card, "截图 2026-11-13 14.02.png", &shot, 60);
    let dir = TempDir::new("download-through-proxy");
    let to = dir.path().join("shot.png");
    let to_arg = to.to_string_lossy().into_owned();
    let (proxy, tunnels) = tunnelling_proxy();

    let out = behind_proxy(
        &mut helper_command(
            "download_attachment.py",
            &[&id, &to_arg, "--card", card],
            &b.fake,
        ),
        &proxy,
    )
    .output()
    .expect("run python3");
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(json_of(&out), Value::Null);
    assert!(
        std::fs::read(&to).unwrap() == shot,
        "the screenshot came back changed"
    );
    let download = format!("GET /1/cards/{card}/attachments/{id}/download/");
    let carried = tunnels_for(&tunnels, &b.fake, &download);
    assert_eq!(carried.len(), 1, "{:?}", tunnels.lock().unwrap());
    let downloads: Vec<Seen> = b
        .fake
        .seen()
        .into_iter()
        .filter(|s| s.path.contains("/download/"))
        .collect();
    assert_eq!(downloads.len(), 1, "{downloads:?}");
    assert_eq!(downloads[0].authorization, Some(oauth_header()));
    assert!(downloads[0].query.is_empty(), "{:?}", downloads[0].query);
}

// --- the skill as a whole -----------------------------------------------------------------

fn skill_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("skills/trello")
}

/// The scripts are exactly the plugin's operations under the operations' names, `card.py`
/// (the read of one card, which the plugin's `Card` handles already give) and `ask.py`; each
/// is executable, allowed by SKILL.md, shown there, and run by a leg here; the library
/// beside them is imported, not run.
#[test]
fn every_script_is_an_action_with_a_leg_and_a_skill_section() {
    let skill_md = std::fs::read_to_string(skill_dir().join("SKILL.md")).expect("SKILL.md");
    let front = skill_md
        .split("---")
        .nth(1)
        .expect("SKILL.md opens with front matter");
    let allowed = front
        .lines()
        .find(|l| l.starts_with("allowed-tools:"))
        .expect("allowed-tools");
    // The legs are everything above this test: its own list of names is no leg.
    let source = include_str!("skills.rs");
    let legs = source
        .split("fn every_script_is_an_action_with_a_leg_and_a_skill_section")
        .next()
        .expect("the legs");
    let manifest = include_str!("../afkd-plugin.toml");
    let mut scripts: Vec<String> = std::fs::read_dir(skill_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".py"))
        .collect();
    scripts.sort();
    assert_eq!(
        scripts,
        [
            "add_label.py",
            "add_member.py",
            "archive_card.py",
            "ask.py",
            "attachments.py",
            "boards.py",
            "card.py",
            "cards.py",
            "check_items.py",
            "checklists.py",
            "comments.py",
            "create_attachment.py",
            "create_card.py",
            "create_check_item.py",
            "create_checklist.py",
            "create_comment.py",
            "delete_attachment.py",
            "delete_card.py",
            "delete_check_item.py",
            "delete_checklist.py",
            "delete_comment.py",
            "download_attachment.py",
            "labels.py",
            "lists.py",
            "me.py",
            "members.py",
            "remove_label.py",
            "remove_member.py",
            "unarchive_card.py",
            "update_card.py",
            "update_check_item.py",
            "update_comment.py",
        ]
    );
    // What a script takes no argument for, SKILL.md allows bare too.
    let bare = [
        "archive_card.py",
        "attachments.py",
        "boards.py",
        "card.py",
        "check_items.py",
        "checklists.py",
        "comments.py",
        "labels.py",
        "lists.py",
        "me.py",
        "members.py",
        "add_member.py",
        "remove_member.py",
    ];
    for script in &scripts {
        let action = script.trim_end_matches(".py");
        assert!(
            script == "ask.py"
                || script == "card.py"
                || manifest.contains(&format!("name = \"{action}\"\n")),
            "{script} is an action's name"
        );
        let mode = std::fs::metadata(skill_dir().join(script))
            .unwrap()
            .permissions()
            .mode();
        assert!(mode & 0o111 != 0, "{script} is executable");
        assert!(
            allowed.contains(&format!("Bash(*/{script} *)")),
            "SKILL.md allows {script}"
        );
        if bare.contains(&script.as_str()) {
            assert!(
                allowed.contains(&format!("Bash(*/{script})")),
                "SKILL.md allows {script} with no argument"
            );
        }
        assert!(
            skill_md.contains(&format!("skills/trello/{script}\"")),
            "SKILL.md shows {script}"
        );
        assert!(
            legs.contains(&format!("run(\"{script}\""))
                || legs.contains(&format!("run_env(\n        \"{script}\""))
                || legs.contains(&format!("\"{script}\",")),
            "a leg runs {script}"
        );
    }
    let lib = std::fs::metadata(skill_dir().join("lib/trello_api.py"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(lib & 0o111, 0, "the module is imported, not run");
}
