//! The skill's placement helpers, `create_card.py` and `move_card.py`, run as an agent runs
//! them — `python3` on the script, the board credentials in the environment — against the
//! stateful fake Trello through their `TRELLO_API_BASE` seam. Each leg reads the board
//! back: where a card landed, and that a refused placement wrote nothing.

mod common;

use std::path::Path;
use std::process::{Command, Output};

use serde_json::{json, Value};

use common::fake::{FakeTrello, Seen, KEY, TOKEN};
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

/// The board every leg starts from: "Up for Grabs" holding the queue, and "Backlog"
/// holding one card.
struct Board {
    fake: FakeTrello,
    queue: String,
    /// The queue's card ids, top to bottom.
    cards: Vec<String>,
    /// The Backlog card's id.
    parked: String,
}

fn board() -> Board {
    let fake = FakeTrello::start();
    let queue = fake.list("Up for Grabs");
    fake.list("Backlog");
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
    Board {
        fake,
        queue,
        cards,
        parked,
    }
}

/// Run one helper with the fake's credentials and base, and none of the selfdev run's
/// card or board locators.
fn helper(script: &str, args: &[&str], fake: &FakeTrello) -> Output {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("skills/trello")
        .join(script);
    Command::new("python3")
        .arg(path)
        .args(args)
        .env("TRELLO_API_KEY", KEY)
        .env("TRELLO_TOKEN", TOKEN)
        .env("TRELLO_API_BASE", fake.base_url())
        .env_remove("TRELLO_CARD_ID")
        .env_remove("TRELLO_BOARD_ID")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .output()
        .expect("run python3")
}

/// A description file under a temp dir, as an agent writes one into its scratch dir.
fn desc_file(dir: &TempDir) -> String {
    let path = dir.path().join("follow-up.md");
    std::fs::write(&path, BODY).expect("write the description");
    path.to_string_lossy().into_owned()
}

/// File the follow-up into the queue with `place`; returns the new card's id.
fn create(b: &Board, place: &[&str]) -> String {
    let dir = TempDir::new("create");
    let desc = desc_file(&dir);
    let mut args = vec![b.queue.as_str(), TITLE, "--desc-file", desc.as_str()];
    args.extend_from_slice(place);
    let out = helper("create_card.py", &args, &b.fake);
    assert!(out.status.success(), "{}", stderr(&out));
    let before = &b.cards;
    let after = b.fake.cards_in("Up for Grabs");
    let new: Vec<&String> = after.iter().filter(|c| !before.contains(c)).collect();
    assert_eq!(new.len(), 1, "one card filed: {after:?}");
    new[0].clone()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// The writes a leg sent: every request but a read.
fn writes(fake: &FakeTrello) -> Vec<Seen> {
    fake.seen()
        .into_iter()
        .filter(|s| s.method != "GET")
        .collect()
}

/// The one request a leg sent, asserting there was only one.
fn only_request(fake: &FakeTrello) -> Seen {
    let seen = fake.seen();
    assert_eq!(seen.len(), 1, "{seen:?}");
    seen.into_iter().next().unwrap()
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

#[test]
fn create_card_below_a_named_card_lands_between_it_and_its_neighbour() {
    let b = board();
    let new = create(&b, &["--below", QUEUE[1].0]);
    let [a, bb, c, d] = [0, 1, 2, 3].map(|i| b.cards[i].clone());
    assert_eq!(b.fake.cards_in("Up for Grabs"), [a, bb, new.clone(), c, d]);

    // The title and the multi-line body round-trip, wide glyphs and all.
    let post = b
        .fake
        .seen()
        .into_iter()
        .find(|s| s.method == "POST")
        .expect("a create");
    let body: Value = serde_json::from_str(&post.body).expect("a JSON body");
    assert_eq!(body["name"], TITLE);
    assert_eq!(body["desc"], BODY);
    assert_eq!(b.fake.list_of(&new), "Up for Grabs");
}

#[test]
fn create_card_above_a_named_card_lands_between_it_and_its_neighbour() {
    let b = board();
    // By id this time, not short link.
    let new = create(&b, &["--above", &b.cards[2]]);
    let [a, bb, c, d] = [0, 1, 2, 3].map(|i| b.cards[i].clone());
    assert_eq!(b.fake.cards_in("Up for Grabs"), [a, bb, new, c, d]);
}

#[test]
fn create_card_at_the_ends_of_the_queue() {
    let b = board();
    let first = create(&b, &["--above", QUEUE[0].0]);
    assert_eq!(b.fake.cards_in("Up for Grabs")[0], first);

    let b = board();
    let last = create(&b, &["--below", QUEUE[3].0]);
    assert_eq!(b.fake.cards_in("Up for Grabs").last(), Some(&last));
    let post: Value = serde_json::from_str(&writes(&b.fake)[0].body).unwrap();
    assert_eq!(
        post["pos"], "bottom",
        "the last card's neighbour is the end"
    );
}

#[test]
fn create_card_with_a_card_not_in_the_target_list_creates_nothing() {
    let b = board();
    let parked = b.parked.clone();
    for anchor in [parked.as_str(), "Xx9Yy8Zz", "0000000000000000deadbeef"] {
        for flag in ["--above", "--below"] {
            let out = helper(
                "create_card.py",
                &[&b.queue, TITLE, "--desc", BODY, flag, anchor],
                &b.fake,
            );
            assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
            assert!(
                stderr(&out).contains(&format!("no card {anchor} in list {}", b.queue)),
                "{}",
                stderr(&out)
            );
        }
    }
    assert!(writes(&b.fake).is_empty(), "{:?}", writes(&b.fake));
    assert_eq!(b.fake.cards_in("Up for Grabs"), b.cards);
    assert_eq!(b.fake.cards_in("Backlog"), std::slice::from_ref(&b.parked));
}

#[test]
fn create_card_top_and_bottom_send_todays_request() {
    for (place, pos) in [
        (&[][..], "bottom"),
        (&["--pos", "top"][..], "top"),
        (&["--pos", "bottom"][..], "bottom"),
    ] {
        let b = board();
        let mut args = vec![b.queue.as_str(), TITLE, "--desc", BODY];
        args.extend_from_slice(place);
        let out = helper("create_card.py", &args, &b.fake);
        assert!(out.status.success(), "{}", stderr(&out));
        let seen = only_request(&b.fake);
        assert_eq!(
            (seen.method.as_str(), seen.path.as_str()),
            ("POST", "/1/cards")
        );
        creds_only(&seen);
        let body: Value = serde_json::from_str(&seen.body).expect("a JSON body");
        assert_eq!(
            body,
            json!({"idList": b.queue, "name": TITLE, "desc": BODY, "pos": pos}),
            "{place:?}"
        );
        let landed = b.fake.cards_in("Up for Grabs");
        let at = if pos == "top" { 0 } else { landed.len() - 1 };
        assert!(!b.cards.contains(&landed[at]), "{place:?}: {landed:?}");
    }
}

#[test]
fn create_card_placement_is_exclusive_with_pos() {
    let b = board();
    for args in [
        ["--pos", "top", "--below", QUEUE[0].0],
        ["--above", QUEUE[0].0, "--below", QUEUE[1].0],
    ] {
        let mut all = vec![b.queue.as_str(), TITLE];
        all.extend_from_slice(&args);
        let out = helper("create_card.py", &all, &b.fake);
        assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
        assert!(
            stderr(&out).contains("not allowed with"),
            "{}",
            stderr(&out)
        );
    }
    assert!(b.fake.seen().is_empty());
}

#[test]
fn move_card_below_a_named_card_lands_between_it_and_its_neighbour() {
    let b = board();
    let out = helper(
        "move_card.py",
        &[
            "--card",
            &b.parked,
            "--name",
            "Up for Grabs",
            "--below",
            QUEUE[0].0,
        ],
        &b.fake,
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains(&format!("(below {})", QUEUE[0].0)),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let [a, bb, c, d] = [0, 1, 2, 3].map(|i| b.cards[i].clone());
    assert_eq!(
        b.fake.cards_in("Up for Grabs"),
        [a, b.parked.clone(), bb, c, d]
    );
    assert!(b.fake.cards_in("Backlog").is_empty());
    let put = writes(&b.fake);
    assert_eq!(put.len(), 1, "{put:?}");
    let body: Value = serde_json::from_str(&put[0].body).unwrap();
    assert_eq!(body["idList"], b.queue.as_str());
    assert!(body["pos"].is_f64(), "a number between two cards: {body}");
}

#[test]
fn move_card_above_a_named_card_within_its_own_list() {
    let b = board();
    let [a, bb, c, d] = [0, 1, 2, 3].map(|i| b.cards[i].clone());

    // Up the list: D directly above B.
    let out = helper(
        "move_card.py",
        &["--card", &d, &b.queue, "--above", QUEUE[1].0],
        &b.fake,
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        b.fake.cards_in("Up for Grabs"),
        [a.clone(), d.clone(), bb.clone(), c.clone()]
    );

    // Down the list, the moved card named by its short link: A directly below C. A's own
    // place is left out of the neighbours, so it lands between C and D, not above C.
    let b = board();
    let [a, bb, c, d] = [0, 1, 2, 3].map(|i| b.cards[i].clone());
    let out = helper(
        "move_card.py",
        &["--card", QUEUE[0].0, &b.queue, "--below", &c],
        &b.fake,
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(b.fake.cards_in("Up for Grabs"), [bb, c, a, d]);
}

#[test]
fn move_card_with_a_card_not_in_the_target_list_moves_nothing() {
    let b = board();
    let a = b.cards[0].clone();
    let parked = b.parked.clone();
    for anchor in [parked.as_str(), "Xx9Yy8Zz", "0000000000000000deadbeef"] {
        let out = helper(
            "move_card.py",
            &["--card", &a, &b.queue, "--below", anchor],
            &b.fake,
        );
        assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
        assert!(
            stderr(&out).contains(&format!("no card {anchor} in list {}", b.queue)),
            "{}",
            stderr(&out)
        );
    }

    // The moved card itself, named by short link while --card is its id.
    let out = helper(
        "move_card.py",
        &["--card", &a, &b.queue, "--above", QUEUE[0].0],
        &b.fake,
    );
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(
        stderr(&out).contains(&format!(
            "cannot place card {} relative to itself",
            QUEUE[0].0
        )),
        "{}",
        stderr(&out)
    );

    assert!(writes(&b.fake).is_empty(), "{:?}", writes(&b.fake));
    assert_eq!(b.fake.cards_in("Up for Grabs"), b.cards);
    assert_eq!(b.fake.cards_in("Backlog"), std::slice::from_ref(&b.parked));
}

#[test]
fn move_card_top_and_bottom_send_todays_request() {
    for (place, pos) in [
        (&[][..], "top"),
        (&["--pos", "top"][..], "top"),
        (&["--pos", "bottom"][..], "bottom"),
    ] {
        let b = board();
        let mut args = vec!["--card", b.parked.as_str(), b.queue.as_str()];
        args.extend_from_slice(place);
        let out = helper("move_card.py", &args, &b.fake);
        assert!(out.status.success(), "{}", stderr(&out));
        let seen = only_request(&b.fake);
        assert_eq!(
            (seen.method.as_str(), seen.path.as_str()),
            ("PUT", format!("/1/cards/{}", b.parked).as_str())
        );
        creds_only(&seen);
        let body: Value = serde_json::from_str(&seen.body).expect("a JSON body");
        assert_eq!(body, json!({"idList": b.queue, "pos": pos}), "{place:?}");
        let landed = b.fake.cards_in("Up for Grabs");
        let at = if pos == "top" { 0 } else { landed.len() - 1 };
        assert_eq!(landed[at], b.parked, "{place:?}");
    }
}

#[test]
fn move_card_placement_is_exclusive_with_pos() {
    let b = board();
    let out = helper(
        "move_card.py",
        &[
            "--card", &b.parked, &b.queue, "--pos", "bottom", "--above", QUEUE[0].0,
        ],
        &b.fake,
    );
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(b.fake.seen().is_empty());
}
