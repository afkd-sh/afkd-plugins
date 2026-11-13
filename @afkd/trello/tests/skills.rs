//! The skill's placement helpers, `create_card.py` and `move_card.py`, and its fetch by
//! name, `fetch_attachment.py --name`, run as an agent runs them — `python3` on the script,
//! the board credentials in the environment — against the stateful fake Trello through
//! their `TRELLO_API_BASE` seam. Each leg reads the board back: where a card landed, that a
//! refused placement wrote nothing, and which file a fetch wrote where. The proxy legs run a
//! helper behind a tunnelling proxy, as a sandbox with a network list does, and read back
//! which requests rode a tunnel.

mod common;

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;

use serde_json::{json, Value};

use common::fake::{oauth_header, FakeTrello, Seen, KEY, TOKEN};
use common::{binary_patch, TempDir};

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

/// One helper with the fake's credentials and base, and none of the selfdev run's card or
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
        .env("PYTHONDONTWRITEBYTECODE", "1");
    for var in PROXY_VARS {
        command.env_remove(var);
    }
    command
}

/// Run one helper, as [`helper_command`] sets it up.
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

// --- fetch_attachment.py --name ---

/// The newest upload named `work.patch` — a real `git diff --binary` — is written to
/// `--to` byte for byte, past an older upload, a newer link of that name, and an upload
/// earlier in the same second whose id is the higher; the download carries the OAuth
/// header and no query credential, and the path written is printed.
#[test]
fn fetch_attachment_by_name_writes_the_newest_to_the_path() {
    let b = board();
    let card = &b.cards[0];
    let dir = TempDir::new("fetch-by-name");
    let patch = binary_patch(dir.path());
    b.fake
        .attachment(card, "work.patch", b"an older attempt\n", 3600);
    let wanted = b.fake.attachment_at(card, "work.patch", &patch, 1800, 954);
    b.fake
        .attachment_at(card, "work.patch", b"earlier that second\n", 1800, 87);
    b.fake
        .link_attachment(card, "work.patch", "https://example.com/work.patch", 60);
    let to = dir.path().join("earlier.patch");
    let to_arg = to.to_string_lossy().into_owned();

    let out = helper(
        "fetch_attachment.py",
        &["--name", "work.patch", "--to", &to_arg, "--card", card],
        &b.fake,
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(String::from_utf8_lossy(&out.stdout), format!("{to_arg}\n"));
    assert!(
        std::fs::read(&to).unwrap() == patch,
        "the patch came back changed"
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
}

/// With no upload of the name the helper says so, writes nothing — a file already at
/// `--to` is left as it was — downloads nothing, and succeeds.
#[test]
fn fetch_attachment_by_name_with_none_writes_nothing_and_succeeds() {
    let b = board();
    let card = &b.cards[0];
    b.fake.attachment(card, "plan.md", b"plan\n", 60);
    let dir = TempDir::new("fetch-by-name-none");
    let to = dir.path().join("earlier.patch");
    std::fs::write(&to, "kept\n").unwrap();
    let to_arg = to.to_string_lossy().into_owned();
    let out = helper(
        "fetch_attachment.py",
        &["--name", "work.patch", "--to", &to_arg, "--card", card],
        &b.fake,
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!("fetch_attachment.py: no attachment named work.patch on card {card}\n")
    );
    assert_eq!(std::fs::read_to_string(&to).unwrap(), "kept\n");
    let seen = only_request(&b.fake);
    assert_eq!(seen.path, format!("/1/cards/{card}/attachments"));
}

/// An attachment is named by its id or by `--name`: both, or neither, is refused before
/// any request.
#[test]
fn fetch_attachment_takes_an_id_or_a_name_not_both() {
    let b = board();
    let card = &b.cards[0];
    let id = b.fake.attachment(card, "plan.md", b"plan\n", 60);
    for args in [
        vec![
            id.as_str(),
            "--name",
            "plan.md",
            "--to",
            "/tmp/x",
            "--card",
            card,
        ],
        vec!["--to", "/tmp/x", "--card", card],
    ] {
        let out = helper("fetch_attachment.py", &args, &b.fake);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {}", stderr(&out));
        assert!(
            stderr(&out).contains("name the attachment by its id or by --name, not both"),
            "{}",
            stderr(&out)
        );
    }
    assert!(b.fake.seen().is_empty(), "{:?}", b.fake.seen());
}

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
/// the helper has its replies.
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
fn a_helper_reaches_the_api_through_the_proxy_its_environment_names() {
    // A sandbox with a network list hands its processes its proxy in the `*_PROXY`
    // variables and lets no connection past it (afkd's ADR-0084), so a helper tunnels its
    // request through the proxy, with the credentials the proxy URL carries.
    let b = board();
    let (proxy, tunnels) = tunnelling_proxy();
    let out = behind_proxy(
        &mut helper_command(
            "move_card.py",
            &["--card", b.parked.as_str(), b.queue.as_str()],
            &b.fake,
        ),
        &proxy,
    )
    .output()
    .expect("run python3");
    assert!(out.status.success(), "{}", stderr(&out));
    let moved = tunnels_for(&tunnels, &b.fake, &format!("PUT /1/cards/{}?", b.parked));
    assert_eq!(moved.len(), 1, "{:?}", tunnels.lock().unwrap());
    assert_eq!(b.fake.cards_in("Up for Grabs")[0], b.parked);
}

/// A screenshot pasted onto a card: wide glyphs and spaces in its name, and bytes no text
/// decoding or line-ending change leaves alone.
fn screenshot() -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
    bytes.extend((0..=255u8).cycle().take(4096));
    bytes.extend_from_slice(b"\r\n\n\r\0IEND\xaeB`\x82");
    bytes
}

/// Behind the proxy, the attachment's download rides a tunnel too, past the metadata read:
/// the file comes through byte for byte, and the OAuth header with it.
#[test]
fn fetch_attachment_downloads_through_the_proxy_its_environment_names() {
    let b = board();
    let card = &b.cards[0];
    let shot = screenshot();
    let id = b
        .fake
        .attachment(card, "截图 2026-11-13 14.02.png", &shot, 60);
    let dir = TempDir::new("fetch-through-proxy");
    let to = dir.path().join("shot.png");
    let to_arg = to.to_string_lossy().into_owned();
    let (proxy, tunnels) = tunnelling_proxy();

    let out = behind_proxy(
        &mut helper_command(
            "fetch_attachment.py",
            &[&id, "--to", &to_arg, "--card", card],
            &b.fake,
        ),
        &proxy,
    )
    .output()
    .expect("run python3");
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(String::from_utf8_lossy(&out.stdout), format!("{to_arg}\n"));
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
