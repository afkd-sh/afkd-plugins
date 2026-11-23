//! The writes suite: the real `afkd-afkd_board` exec, sent each write as afkd sends one — a child
//! started for the one call, a `call` with the protocol and the arguments afkd bound — against the
//! fake board an `afkd_board.Board` reaches. No afkd is involved.
//!
//! Every write is called through [`once`] and held to the handle or value it gives, and to the
//! state the fake board is left in. Each refusal the brief names is covered, and so is the run:
//! a write given one reaches the fake with exactly its `machine`, `service`, `instance` and `id`,
//! and one given none, or `nil`, with no `run` key at all. Every leg ends with the fake's record of
//! requests the plugin should never have sent empty.

mod common;

use serde_json::{json, Value};

use common::fake::{CardState, FakeBoard, NewCard, Run, Seen};
use common::{board, once};

const KEY: &str = "afk_board_writes-0123456789";

/// The run every write is made in, and its four fields as the board takes them.
fn run() -> Value {
    json!({"machine": "nexus", "service": "develop/review.0", "instance": "blue-2",
           "id": "261116-163511-card-17-2"})
}

/// The same run as a newer afkd sends it, with a field the board's API does not take.
fn newer_run() -> Value {
    let mut run = run();
    run["scratch_dir"] = json!("/tmp/afkd/runs/x");
    run
}

/// The board every write is held to, its lists `Todo`, `Doing`, `Done` and `Empty`, its labels `bug`
/// and `afkd`, and its cards:
///
/// - #1, Todo: `bug`, a description and two items not ticked;
/// - #2, Todo;
/// - #3, Doing;
/// - #4, Done: done;
/// - #5, Todo: blocked by #2 and #3, which are not done.
///
/// So `Todo` holds #1, #2 and #5, in that order.
fn world() -> FakeBoard {
    let fake = FakeBoard::start(
        "Release train",
        KEY,
        &["Todo", "Doing", "Done", "Empty"],
        &[("bug", "red"), ("afkd", "blue")],
    );
    fake.card(NewCard {
        description: "The lead.".to_string(),
        labels: vec!["bug".to_string()],
        checklist: vec![
            ("Reproduce it".to_string(), false),
            ("Bound the backoff".to_string(), false),
        ],
        ..NewCard::new("Todo", "Lead")
    });
    fake.card(NewCard::new("Todo", "Second"));
    fake.card(NewCard::new("Doing", "Third"));
    fake.card(NewCard {
        done: true,
        ..NewCard::new("Done", "Shipped")
    });
    fake.card(NewCard {
        blocked_by: vec![2, 3],
        ..NewCard::new("Todo", "Waits")
    });
    fake
}

/// `action` of `fake` with `args` beside its board.
fn call(fake: &FakeBoard, action: &str, mut args: Value) -> (Value, String) {
    args["b"] = board(fake);
    once(action, args)
}

/// The handle of the card `number` of `fake`, as afkd passes one.
fn card(fake: &FakeBoard, number: i64) -> Value {
    json!({"id": number.to_string(),
           "key": format!("afkd_board:{}/{}", fake.board_id(), fake.card_id(number))})
}

/// The reply's ok handle.
fn handle(reply: &Value) -> &Value {
    assert_eq!(reply["ok"], true, "{reply}");
    &reply["handle"]
}

/// The `error` of a refused reply.
fn refused(reply: &Value) -> &str {
    assert_eq!(reply["ok"], false, "{reply}");
    reply["error"].as_str().expect("an error")
}

fn no_invalid(fake: &FakeBoard) {
    assert_eq!(fake.invalid_requests(), Vec::<String>::new());
}

/// The method and path of each request the fake saw, in order.
fn requests(seen: &[Seen]) -> Vec<String> {
    seen.iter()
        .map(|s| format!("{} {}", s.method, s.path))
        .collect()
}

/// The one write the fake saw: its method, its path and its body.
fn only_write(fake: &FakeBoard) -> (String, String, Option<Value>) {
    let writes = fake.writes();
    assert_eq!(writes.len(), 1, "{writes:?}");
    let write = &writes[0];
    (write.method.clone(), write.path.clone(), write.body.clone())
}

/// What the fake holds of the card `number`.
fn state(fake: &FakeBoard, number: i64) -> CardState {
    fake.card_state(number).expect("the card")
}

/// The numbers of the cards of the list `name`, in order.
fn order(fake: &FakeBoard, name: &str) -> Vec<i64> {
    fake.list_state(name).expect("the list").cards
}

/// A card is made with the next number, in the list at the place named, with its description, its
/// labels, one of them new and so in the next free colour, and its items not ticked, in the run it
/// is made in, of which a field the API does not take is left off.
#[test]
fn create_card_makes_the_next_card_with_everything_it_is_given() {
    let fake = world();
    let (reply, stderr) = call(
        &fake,
        "create_card",
        json!({"list": "Todo", "title": "New 🚨", "description": "Why",
               "labels": ["bug", "fresh"],
               "checklist": [{"text": "one", "done": false}, {"text": "two", "done": true}],
               "at": "top", "run": newer_run()}),
    );
    let made = handle(&reply);
    assert_eq!(made["id"], "6");
    assert_eq!(
        made["key"],
        format!("afkd_board:{}/{}", fake.board_id(), fake.card_id(6))
    );
    assert_eq!(made["fields"]["number"], 6);
    assert_eq!(made["fields"]["title"], "New 🚨");
    assert_eq!(made["fields"]["list"], "Todo");
    assert_eq!(made["fields"]["labels"], json!(["bug", "fresh"]));
    assert_eq!(
        state(&fake, 6),
        CardState {
            title: "New 🚨".to_string(),
            description: "Why".to_string(),
            list: "Todo".to_string(),
            labels: vec!["bug".to_string(), "fresh".to_string()],
            checklist: vec![("one".to_string(), false), ("two".to_string(), false)],
            done: false,
            blocked_by: Vec::new(),
            archived: false,
        }
    );
    assert_eq!(
        fake.labels(),
        [("bug", "red"), ("afkd", "blue"), ("fresh", "amber")]
            .map(|(n, c)| (n.to_string(), c.to_string()))
    );
    assert_eq!(order(&fake, "Todo"), [6, 1, 2, 5]);
    let (method, path, body) = only_write(&fake);
    assert_eq!(
        (method.as_str(), path.as_str()),
        ("POST", "/api/board/cards")
    );
    assert_eq!(
        body,
        Some(
            json!({"list": "Todo", "title": "New 🚨", "description": "Why",
                    "labels": ["bug", "fresh"], "checklist": ["one", "two"],
                    "at": "top", "run": run()})
        )
    );
    assert!(
        stderr.contains("afkd-afkd_board: create_card: created #6 in Todo"),
        "{stderr}"
    );
    no_invalid(&fake);
}

/// A card goes right after another of its list, with a title and a list alone otherwise.
#[test]
fn create_card_goes_after_a_card_and_needs_only_a_list_and_a_title() {
    let fake = world();
    let (reply, _) = call(
        &fake,
        "create_card",
        json!({"list": "Todo", "title": "Between", "after": card(&fake, 1)}),
    );
    assert_eq!(handle(&reply)["id"], "6");
    assert_eq!(order(&fake, "Todo"), [1, 6, 2, 5]);
    let (_, _, body) = only_write(&fake);
    assert_eq!(
        body,
        Some(json!({"list": "Todo", "title": "Between", "after": 1}))
    );

    let (reply, _) = call(
        &fake,
        "create_card",
        json!({"list": "Done", "title": "Last", "run": null}),
    );
    assert_eq!(handle(&reply)["id"], "7");
    assert_eq!(order(&fake, "Done"), [4, 7]);
    assert_eq!(
        fake.writes()[1].body,
        Some(json!({"list": "Done", "title": "Last"}))
    );
    no_invalid(&fake);
}

/// A card's title and description change, each left as it is when it is not given.
#[test]
fn update_card_changes_its_title_and_its_description() {
    let fake = world();
    let (reply, _) = call(
        &fake,
        "update_card",
        json!({"card": card(&fake, 1), "title": "Renamed", "description": "New text",
               "run": run()}),
    );
    assert_eq!(handle(&reply)["fields"]["title"], "Renamed");
    assert_eq!(handle(&reply)["fields"]["description"], "New text");
    let (method, path, body) = only_write(&fake);
    assert_eq!(
        (method.as_str(), path.as_str()),
        ("PATCH", "/api/board/cards/1")
    );
    assert_eq!(
        body,
        Some(json!({"title": "Renamed", "description": "New text", "run": run()}))
    );

    let (reply, _) = call(
        &fake,
        "update_card",
        json!({"card": card(&fake, 1), "description": "Only this"}),
    );
    assert_eq!(handle(&reply)["fields"]["title"], "Renamed");
    assert_eq!(state(&fake, 1).description, "Only this");
    no_invalid(&fake);
}

/// A card moves to a list at its top, after a card of it, or, with neither, at its bottom, which a
/// card already in the list is sent to as well.
#[test]
fn move_card_goes_to_a_list_at_the_top_after_a_card_or_at_the_bottom() {
    let fake = world();
    let (reply, _) = call(
        &fake,
        "move_card",
        json!({"card": card(&fake, 5), "list": "Doing", "at": "top", "run": run()}),
    );
    assert_eq!(handle(&reply)["fields"]["list"], "Doing");
    assert_eq!(order(&fake, "Doing"), [5, 3]);
    assert_eq!(
        fake.writes()[0].body,
        Some(json!({"list": "Doing", "at": "top", "run": run()}))
    );

    call(
        &fake,
        "move_card",
        json!({"card": card(&fake, 2), "list": "Doing", "after": card(&fake, 3)}),
    );
    assert_eq!(order(&fake, "Doing"), [5, 3, 2]);
    assert_eq!(
        fake.writes()[1].body,
        Some(json!({"list": "Doing", "after": 3}))
    );

    let (reply, _) = call(
        &fake,
        "move_card",
        json!({"card": card(&fake, 1), "list": "Todo"}),
    );
    assert_eq!(handle(&reply)["fields"]["list"], "Todo");
    assert_eq!(order(&fake, "Todo"), [1]);
    assert_eq!(
        fake.writes()[2].body,
        Some(json!({"list": "Todo", "at": "bottom"})),
        "with neither at nor after it goes at the bottom, sent so"
    );

    call(
        &fake,
        "move_card",
        json!({"card": card(&fake, 5), "list": "Todo"}),
    );
    call(
        &fake,
        "move_card",
        json!({"card": card(&fake, 1), "list": "Todo"}),
    );
    assert_eq!(
        order(&fake, "Todo"),
        [5, 1],
        "a card in its list goes to the bottom"
    );
    no_invalid(&fake);
}

/// A label is put on, one the board lacks made in the next free colour, and taken off; a name with
/// a space and a slash is one path segment, escaped.
#[test]
fn labels_are_put_on_and_taken_off() {
    let fake = world();
    let (reply, _) = call(
        &fake,
        "add_label",
        json!({"card": card(&fake, 1), "label": "afkd", "run": run()}),
    );
    assert_eq!(handle(&reply)["fields"]["labels"], json!(["bug", "afkd"]));
    let (method, path, body) = only_write(&fake);
    assert_eq!(
        (method.as_str(), path.as_str()),
        ("POST", "/api/board/cards/1/labels/afkd")
    );
    assert_eq!(body, Some(json!({"run": run()})));

    call(
        &fake,
        "add_label",
        json!({"card": card(&fake, 1), "label": "needs review/ok"}),
    );
    assert_eq!(
        fake.writes()[1].path,
        "/api/board/cards/1/labels/needs%20review%2Fok"
    );
    assert_eq!(
        fake.writes()[1].body,
        Some(json!({})),
        "no run, no `run` key"
    );
    assert_eq!(
        state(&fake, 1).labels,
        ["bug", "afkd", "needs review/ok"].map(str::to_string)
    );
    assert_eq!(
        fake.labels().last().cloned(),
        Some(("needs review/ok".to_string(), "amber".to_string())),
        "the colour the fewest labels have, the earliest of them"
    );

    let (reply, _) = call(
        &fake,
        "remove_label",
        json!({"card": card(&fake, 1), "label": "bug", "run": run()}),
    );
    assert_eq!(
        handle(&reply)["fields"]["labels"],
        json!(["afkd", "needs review/ok"])
    );
    let writes = fake.writes();
    assert_eq!(
        (writes[2].method.as_str(), writes[2].path.as_str()),
        ("DELETE", "/api/board/cards/1/labels/bug")
    );
    assert_eq!(writes[2].body, Some(json!({"run": run()})));
    no_invalid(&fake);
}

/// Removing a label the board lacks is ok and writes nothing: the card is read and answered as it
/// is. Removing one the board has and the card lacks is sent, and the card is left as it is.
#[test]
fn removing_a_label_the_board_lacks_is_ok_and_sends_no_write() {
    let fake = world();
    let (reply, stderr) = call(
        &fake,
        "remove_label",
        json!({"card": card(&fake, 1), "label": "ghost", "run": run()}),
    );
    assert_eq!(handle(&reply)["fields"]["labels"], json!(["bug"]));
    assert_eq!(handle(&reply)["id"], "1");
    assert_eq!(fake.writes(), Vec::<Seen>::new());
    assert_eq!(
        requests(&fake.seen()),
        ["GET /api/board", "GET /api/board/cards/1"]
    );
    assert!(stderr.contains("nothing is written"), "{stderr}");

    let (reply, _) = call(
        &fake,
        "remove_label",
        json!({"card": card(&fake, 2), "label": "afkd"}),
    );
    assert_eq!(handle(&reply)["fields"]["labels"], json!([]));
    assert_eq!(
        only_write(&fake).1,
        "/api/board/cards/2/labels/afkd",
        "a label the board has is sent"
    );
    no_invalid(&fake);
}

/// An item is added, ticked and unticked by its place from 0, and removed; one past the end is
/// refused with the API's own sentence.
#[test]
fn a_checklist_is_added_to_ticked_and_cut() {
    let fake = world();
    let (reply, _) = call(
        &fake,
        "add_item",
        json!({"card": card(&fake, 1), "text": "Ship it", "run": run()}),
    );
    assert_eq!(handle(&reply)["id"], "1");
    assert_eq!(
        state(&fake, 1).checklist,
        [
            ("Reproduce it", false),
            ("Bound the backoff", false),
            ("Ship it", false)
        ]
        .map(|(t, d)| (t.to_string(), d))
    );
    assert_eq!(
        fake.writes()[0].body,
        Some(json!({"text": "Ship it", "run": run()}))
    );

    call(
        &fake,
        "check_item",
        json!({"card": card(&fake, 1), "index": 1, "done": true, "run": run()}),
    );
    assert_eq!(
        state(&fake, 1).checklist[1],
        ("Bound the backoff".to_string(), true)
    );
    let writes = fake.writes();
    assert_eq!(
        (writes[1].method.as_str(), writes[1].path.as_str()),
        ("PATCH", "/api/board/cards/1/checklist/1")
    );
    assert_eq!(writes[1].body, Some(json!({"done": true, "run": run()})));
    call(
        &fake,
        "check_item",
        json!({"card": card(&fake, 1), "index": 1, "done": false}),
    );
    assert!(!state(&fake, 1).checklist[1].1);
    assert_eq!(fake.writes()[2].body, Some(json!({"done": false})));

    let (reply, _) = call(
        &fake,
        "remove_item",
        json!({"card": card(&fake, 1), "index": 0, "run": run()}),
    );
    assert_eq!(handle(&reply)["id"], "1");
    assert_eq!(
        state(&fake, 1).checklist,
        [("Bound the backoff", false), ("Ship it", false)].map(|(t, d)| (t.to_string(), d))
    );
    let writes = fake.writes();
    assert_eq!(
        (writes[3].method.as_str(), writes[3].path.as_str()),
        ("DELETE", "/api/board/cards/1/checklist/0")
    );
    assert_eq!(writes[3].body, Some(json!({"run": run()})));

    let (reply, _) = call(
        &fake,
        "check_item",
        json!({"card": card(&fake, 1), "index": 5, "done": true}),
    );
    assert_eq!(
        refused(&reply),
        "check_item: Card #1 has no item 5; its checklist has 2."
    );
    no_invalid(&fake);
}

/// A card is marked done and not done, and a blocked one is refused naming the cards that are not
/// done, which leaves it as it was.
#[test]
fn mark_done_marks_a_card_and_refuses_a_blocked_one() {
    let fake = world();
    let (reply, _) = call(
        &fake,
        "mark_done",
        json!({"card": card(&fake, 2), "done": true, "run": run()}),
    );
    assert_eq!(handle(&reply)["fields"]["done"], true);
    assert!(state(&fake, 2).done);
    assert_eq!(
        fake.writes()[0].body,
        Some(json!({"done": true, "run": run()}))
    );
    let (reply, _) = call(
        &fake,
        "mark_done",
        json!({"card": card(&fake, 2), "done": false}),
    );
    assert_eq!(handle(&reply)["fields"]["done"], false);

    let (reply, _) = call(
        &fake,
        "mark_done",
        json!({"card": card(&fake, 5), "done": true, "run": run()}),
    );
    assert_eq!(
        refused(&reply),
        "mark_done: Card #5 is blocked by #2 and #3, which are not done."
    );
    assert!(!state(&fake, 5).done);

    call(
        &fake,
        "mark_done",
        json!({"card": card(&fake, 2), "done": true}),
    );
    call(
        &fake,
        "mark_done",
        json!({"card": card(&fake, 3), "done": true}),
    );
    let (reply, _) = call(
        &fake,
        "mark_done",
        json!({"card": card(&fake, 5), "done": true}),
    );
    assert_eq!(
        handle(&reply)["fields"]["done"],
        true,
        "its blockers are done"
    );
    no_invalid(&fake);
}

/// A block is put on and taken off, a done card blocked by one that is not done answers `done` as
/// the board left it, and a block that makes a chain back, or of a card by itself, is refused.
#[test]
fn blocks_are_put_on_and_taken_off_and_a_chain_back_is_refused() {
    let fake = world();
    let (reply, _) = call(
        &fake,
        "add_blocked_by",
        json!({"card": card(&fake, 1), "other": card(&fake, 2), "run": run()}),
    );
    assert_eq!(handle(&reply)["fields"]["blocked_by"], json!([2]));
    assert_eq!(handle(&reply)["fields"]["blocked"], true);
    let (method, path, body) = only_write(&fake);
    assert_eq!(
        (method.as_str(), path.as_str()),
        ("POST", "/api/board/cards/1/blocked_by/2")
    );
    assert_eq!(body, Some(json!({"run": run()})));

    let (reply, _) = call(
        &fake,
        "add_blocked_by",
        json!({"card": card(&fake, 4), "other": card(&fake, 2)}),
    );
    assert_eq!(
        handle(&reply)["fields"]["done"],
        false,
        "a done card blocked by one that is not done is not done"
    );
    assert!(!state(&fake, 4).done);

    let (reply, _) = call(
        &fake,
        "remove_blocked_by",
        json!({"card": card(&fake, 5), "other": card(&fake, 2), "run": run()}),
    );
    assert_eq!(handle(&reply)["fields"]["blocked_by"], json!([3]));
    let writes = fake.writes();
    assert_eq!(
        (writes[2].method.as_str(), writes[2].path.as_str()),
        ("DELETE", "/api/board/cards/5/blocked_by/2")
    );

    let (reply, _) = call(
        &fake,
        "add_blocked_by",
        json!({"card": card(&fake, 2), "other": card(&fake, 1)}),
    );
    assert_eq!(
        refused(&reply),
        "add_blocked_by: Card #2 cannot be blocked by #1, since #1 is blocked by #2."
    );
    let (reply, _) = call(
        &fake,
        "add_blocked_by",
        json!({"card": card(&fake, 3), "other": card(&fake, 5)}),
    );
    assert_eq!(
        refused(&reply),
        "add_blocked_by: Card #3 cannot be blocked by #5, since #5 is blocked by #3."
    );
    let (reply, _) = call(
        &fake,
        "add_blocked_by",
        json!({"card": card(&fake, 1), "other": card(&fake, 1)}),
    );
    assert_eq!(
        refused(&reply),
        "add_blocked_by: Card #1 cannot be blocked by itself."
    );
    assert_eq!(state(&fake, 2).blocked_by, Vec::<i64>::new());
    no_invalid(&fake);
}

/// A comment is made and handed back as a `Comment` handle, its run's id among its fields, and the
/// board holds the run it was made in whole.
#[test]
fn create_comment_gives_the_comment_made_in_its_run() {
    let fake = world();
    let (reply, _) = call(
        &fake,
        "create_comment",
        json!({"card": card(&fake, 1), "body": "Looks good 🎉\n\n- done", "run": run()}),
    );
    let made = handle(&reply);
    let comments = fake.comments_on(1);
    assert_eq!(comments.len(), 1);
    assert_eq!(
        comments[0],
        (
            "Looks good 🎉\n\n- done".to_string(),
            Some("nightly".to_string()),
            true,
            Some(Run {
                machine: "nexus".to_string(),
                service: "develop/review.0".to_string(),
                instance: "blue-2".to_string(),
                id: "261116-163511-card-17-2".to_string(),
            })
        )
    );
    assert_eq!(made["fields"]["card"], 1);
    assert_eq!(made["fields"]["body"], "Looks good 🎉\n\n- done");
    assert_eq!(made["fields"]["author"], Value::Null, "a key made it");
    assert_eq!(made["fields"]["run"], "261116-163511-card-17-2");
    assert_eq!(made["fields"]["url"], fake.card_url(1));
    assert!(
        made["key"]
            .as_str()
            .is_some_and(|k| k.starts_with(&format!("afkd_board:{}/", fake.board_id()))),
        "{made}"
    );
    let (method, path, body) = only_write(&fake);
    assert_eq!(
        (method.as_str(), path.as_str()),
        ("POST", "/api/board/cards/1/comments")
    );
    assert_eq!(
        body,
        Some(json!({"body": "Looks good 🎉\n\n- done", "run": run()}))
    );

    let (reply, _) = call(
        &fake,
        "create_comment",
        json!({"card": card(&fake, 1), "body": "No run"}),
    );
    assert_eq!(handle(&reply)["fields"]["run"], Value::Null);
    assert_eq!(fake.comments_on(1)[1].3, None);
    no_invalid(&fake);
}

/// A card is archived, which leaves its list, and brought back to the bottom of it; one deleted
/// is answered as it was read before, and gone, the DELETE sent with no body whatever run it was
/// given.
#[test]
fn a_card_is_archived_brought_back_and_deleted() {
    let fake = world();
    let (reply, _) = call(
        &fake,
        "archive_card",
        json!({"card": card(&fake, 2), "run": run()}),
    );
    assert_eq!(handle(&reply)["fields"]["archived"], true);
    assert_eq!(order(&fake, "Todo"), [1, 5]);
    assert_eq!(fake.list_state("Todo").expect("a list").archived, [2]);
    assert_eq!(
        only_write(&fake),
        (
            "POST".to_string(),
            "/api/board/cards/2/archive".to_string(),
            Some(json!({"run": run()}))
        )
    );

    let (reply, _) = call(
        &fake,
        "unarchive_card",
        json!({"card": card(&fake, 2), "run": run()}),
    );
    assert_eq!(handle(&reply)["fields"]["archived"], false);
    assert_eq!(order(&fake, "Todo"), [1, 5, 2], "back at its list's bottom");
    assert_eq!(fake.writes()[1].path, "/api/board/cards/2/unarchive");

    let (reply, stderr) = call(
        &fake,
        "delete_card",
        json!({"card": card(&fake, 1), "run": run()}),
    );
    assert_eq!(handle(&reply)["id"], "1");
    assert_eq!(handle(&reply)["fields"]["title"], "Lead");
    assert_eq!(handle(&reply)["fields"]["labels"], json!(["bug"]));
    assert_eq!(fake.card_state(1), None, "the card is gone");
    let seen = fake.seen();
    let tail: Vec<String> = requests(&seen[seen.len() - 3..]);
    assert_eq!(
        tail,
        [
            "GET /api/board",
            "GET /api/board/cards/1",
            "DELETE /api/board/cards/1"
        ],
        "the card is read before it is deleted"
    );
    assert_eq!(
        seen.last().map(|s| s.body.clone()),
        Some(None),
        "the board's DELETE takes no body, so no run reaches it"
    );
    assert!(stderr.contains("deleted #1"), "{stderr}");
    no_invalid(&fake);
}

/// A list is made at the bottom, the top or after another, renamed and moved, and deleted once it
/// holds no card that is not archived, the deletion answering `{"ok":true}`; a list writes no
/// `run`.
#[test]
fn lists_are_made_renamed_moved_and_deleted() {
    let fake = world();
    let (reply, _) = call(
        &fake,
        "create_list",
        json!({"name": "Review", "run": run()}),
    );
    assert_eq!(reply["ok"], true, "{reply}");
    assert_eq!(reply["value"]["name"], "Review");
    assert_eq!(
        reply["value"]["id"],
        fake.list_state("Review").expect("the list").id
    );
    assert_eq!(
        fake.list_names(),
        ["Todo", "Doing", "Done", "Empty", "Review"]
    );
    assert_eq!(
        only_write(&fake),
        (
            "POST".to_string(),
            "/api/board/lists".to_string(),
            Some(json!({"name": "Review"}))
        ),
        "a list is made with no run"
    );

    call(&fake, "create_list", json!({"name": "Icebox", "at": "top"}));
    call(
        &fake,
        "create_list",
        json!({"name": "Mid", "after": "Icebox"}),
    );
    assert_eq!(
        fake.list_names(),
        ["Icebox", "Mid", "Todo", "Doing", "Done", "Empty", "Review"]
    );
    let writes = fake.writes();
    assert_eq!(writes[1].body, Some(json!({"name": "Icebox", "at": "top"})));
    assert_eq!(
        writes[2].body,
        Some(json!({"name": "Mid", "after": fake.list_state("Icebox").expect("a list").id}))
    );

    let (reply, _) = call(
        &fake,
        "update_list",
        json!({"list": "Doing", "name": "Active", "after": "Done"}),
    );
    assert_eq!(reply["value"]["name"], "Active");
    assert_eq!(
        fake.list_names(),
        ["Icebox", "Mid", "Todo", "Done", "Active", "Empty", "Review"]
    );
    assert_eq!(order(&fake, "Active"), [3], "its cards keep it");
    assert_eq!(state(&fake, 3).list, "Active");
    let writes = fake.writes();
    assert_eq!(
        writes[3].path,
        format!(
            "/api/board/lists/{}",
            fake.list_state("Active").expect("a list").id
        )
    );
    assert_eq!(
        writes[3].body,
        Some(json!({"name": "Active", "after": fake.list_state("Done").expect("a list").id}))
    );

    let (reply, _) = call(&fake, "update_list", json!({"list": "Review", "at": "top"}));
    assert_eq!(reply["value"]["name"], "Review");
    assert_eq!(fake.list_names()[0], "Review");

    let (reply, _) = call(&fake, "delete_list", json!({"list": "Empty"}));
    assert_eq!(reply, json!({"ok": true}));
    assert_eq!(fake.list_state("Empty"), None);
    let last = fake.writes().pop().expect("a write");
    assert_eq!(
        (last.method.as_str(), last.body),
        ("DELETE", None),
        "a list is deleted with no body"
    );
    no_invalid(&fake);
}

/// A list another has the name of is refused, and so is one that holds cards that are not
/// archived, naming how many, which leaves it as it was.
#[test]
fn a_list_name_taken_and_a_list_that_holds_cards_are_refused() {
    let fake = world();
    let (reply, _) = call(&fake, "create_list", json!({"name": "Todo"}));
    assert_eq!(
        refused(&reply),
        "create_list: The name `Todo` is taken by another list of Release train."
    );
    let (reply, _) = call(&fake, "delete_list", json!({"list": "Todo"}));
    assert_eq!(
        refused(&reply),
        "delete_list: The list `Todo` holds 3 cards that are not archived; move or archive them \
         first."
    );
    let (reply, _) = call(&fake, "delete_list", json!({"list": "Doing"}));
    assert_eq!(
        refused(&reply),
        "delete_list: The list `Doing` holds 1 card that is not archived; move or archive it \
         first."
    );
    assert_eq!(order(&fake, "Todo"), [1, 2, 5]);
    assert_eq!(fake.list_names(), ["Todo", "Doing", "Done", "Empty"]);

    call(&fake, "archive_card", json!({"card": card(&fake, 3)}));
    let (reply, _) = call(&fake, "delete_list", json!({"list": "Doing"}));
    assert_eq!(reply, json!({"ok": true}), "its archived card goes with it");
    assert_eq!(fake.card_state(3), None);
    no_invalid(&fake);
}

/// `at` with `after` is refused by the plugin before it sends anything, in every write that takes
/// both: the fake sees nothing at all.
#[test]
fn at_with_after_is_refused_before_anything_is_sent() {
    let fake = world();
    let lead = card(&fake, 1);
    for (action, args) in [
        (
            "create_card",
            json!({"list": "Todo", "title": "T", "at": "top", "after": lead}),
        ),
        (
            "move_card",
            json!({"card": lead, "list": "Done", "at": "bottom", "after": card(&fake, 4)}),
        ),
        (
            "create_list",
            json!({"name": "Review", "at": "top", "after": "Todo"}),
        ),
        (
            "update_list",
            json!({"list": "Todo", "at": "top", "after": "Done"}),
        ),
    ] {
        let (reply, _) = call(&fake, action, args);
        assert_eq!(
            refused(&reply),
            format!("{action}: at and after cannot both be given")
        );
    }
    assert_eq!(fake.seen(), Vec::<Seen>::new(), "not one request");
    no_invalid(&fake);
}

/// A list name is matched exactly, and one the board lacks is refused naming the lists it has,
/// with no write sent.
#[test]
fn an_unknown_list_is_refused_naming_the_lists_there_are() {
    let fake = world();
    for (action, args, name) in [
        ("create_card", json!({"list": "todo", "title": "T"}), "todo"),
        (
            "move_card",
            json!({"card": card(&fake, 1), "list": "Nope"}),
            "Nope",
        ),
        (
            "create_list",
            json!({"name": "New", "after": "Nope"}),
            "Nope",
        ),
        (
            "update_list",
            json!({"list": "Nope", "name": "New"}),
            "Nope",
        ),
        (
            "update_list",
            json!({"list": "Todo", "after": "Nope"}),
            "Nope",
        ),
        ("delete_list", json!({"list": "Nope"}), "Nope"),
    ] {
        let (reply, _) = call(&fake, action, args);
        assert_eq!(
            refused(&reply),
            format!(
                "{action}: The board has no list `{name}`; it has `Todo`, `Doing`, `Done` and \
                 `Empty`."
            )
        );
    }
    assert_eq!(fake.writes(), Vec::<Seen>::new(), "no write was sent");
    no_invalid(&fake);
}

/// A card handle of another board is refused naming both boards, with no write sent, whichever
/// argument it is passed as.
#[test]
fn a_card_of_another_board_is_refused() {
    let fake = world();
    let other = FakeBoard::start("Ops", "afk_board_ops-0123456789", &["Inbox"], &[]);
    other.card(NewCard::new("Inbox", "Rotate keys"));
    let theirs = card(&other, 1);
    let refusal = format!(
        "card is #1 of board {}, and the afkd_board.Board reaches board {}",
        other.board_id(),
        fake.board_id()
    );
    for (action, args) in [
        ("update_card", json!({"card": theirs, "title": "x"})),
        (
            "add_blocked_by",
            json!({"card": card(&fake, 1), "other": theirs}),
        ),
        (
            "create_card",
            json!({"list": "Todo", "title": "T", "after": theirs}),
        ),
        (
            "move_card",
            json!({"card": card(&fake, 1), "list": "Done", "after": theirs}),
        ),
        ("create_comment", json!({"card": theirs, "body": "x"})),
        ("delete_card", json!({"card": theirs})),
    ] {
        let (reply, _) = call(&fake, action, args);
        assert_eq!(refused(&reply), format!("{action}: {refusal}"));
    }
    assert_eq!(fake.writes(), Vec::<Seen>::new());
    assert_eq!(other.seen(), Vec::<Seen>::new());
    no_invalid(&fake);
    no_invalid(&other);
}

/// A write given a run reaches the fake with exactly that run's `machine`, `service`, `instance`
/// and `id`, whichever write of a card, its checklist or a comment it is, and one given none, or
/// `nil`, reaches it with no `run` key.
#[test]
fn a_write_reaches_the_board_under_its_run_and_one_without_sends_none() {
    /// The arguments of the write `action` on the cards of `fake`, bar its run.
    fn args_of(fake: &FakeBoard, action: &str) -> Value {
        let (one, two, five) = (card(fake, 1), card(fake, 2), card(fake, 5));
        match action {
            "create_card" => json!({"list": "Todo", "title": "T"}),
            "update_card" => json!({"card": one, "title": "T"}),
            "move_card" => json!({"card": one, "list": "Done"}),
            "add_label" => json!({"card": one, "label": "afkd"}),
            "remove_label" => json!({"card": one, "label": "bug"}),
            "add_item" => json!({"card": one, "text": "x"}),
            "check_item" => json!({"card": one, "index": 0, "done": true}),
            "remove_item" => json!({"card": one, "index": 0}),
            "mark_done" => json!({"card": two, "done": true}),
            "add_blocked_by" => json!({"card": one, "other": two}),
            "remove_blocked_by" => json!({"card": five, "other": two}),
            "create_comment" => json!({"card": one, "body": "x"}),
            "archive_card" | "unarchive_card" => json!({"card": one}),
            other => panic!("no arguments for {other}"),
        }
    }
    for action in [
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
    ] {
        for given in [Some(run()), Some(newer_run()), None, Some(Value::Null)] {
            let fake = world();
            let mut args = args_of(&fake, action);
            if let Some(given) = &given {
                args["run"] = given.clone();
            }
            let (reply, _) = call(&fake, action, args);
            assert_eq!(reply["ok"], true, "{action}: {reply}");
            let writes = fake.writes();
            assert_eq!(writes.len(), 1, "{action}: {writes:?}");
            let body = writes[0].body.as_ref().expect("a body");
            match &given {
                Some(given) if !given.is_null() => {
                    assert_eq!(body["run"], run(), "{action}: exactly the four fields");
                }
                _ => assert!(
                    body.get("run").is_none(),
                    "{action}: a write without a run sends none: {body}"
                ),
            }
            no_invalid(&fake);
        }
    }
}

/// A run that is no `afkd.Run`, and a key that is empty, are refused before any request.
#[test]
fn a_malformed_run_and_an_empty_key_are_refused_before_any_request() {
    let fake = world();
    for run in [
        json!("261116-163511"),
        json!({"machine": "nexus"}),
        json!({"machine": "nexus", "service": "s", "instance": 0, "id": "i"}),
    ] {
        let (reply, _) = call(
            &fake,
            "update_card",
            json!({"card": card(&fake, 1), "title": "x", "run": run}),
        );
        assert_eq!(
            refused(&reply),
            "update_card: parameter run must be an afkd.Run"
        );
    }
    let (reply, _) = once(
        "create_card",
        json!({"b": {"key": "", "base_url": fake.base_url()}, "list": "Todo", "title": "T"}),
    );
    assert!(
        refused(&reply).starts_with("create_card: the afkd_board.Board's key is empty"),
        "{reply}"
    );
    assert_eq!(fake.seen(), Vec::<Seen>::new());
    no_invalid(&fake);
}

/// A card the board has no number of is refused with the board's own sentence.
#[test]
fn a_card_the_board_lacks_is_refused_by_the_board() {
    let fake = world();
    let mut gone = card(&fake, 1);
    gone["id"] = json!("99");
    let (reply, _) = call(&fake, "update_card", json!({"card": gone, "title": "x"}));
    assert_eq!(refused(&reply), "update_card: The board has no card #99.");
    let (reply, _) = call(&fake, "create_comment", json!({"card": gone, "body": "x"}));
    assert_eq!(
        refused(&reply),
        "create_comment: The board has no card #99."
    );
    assert_eq!(
        requests(&fake.writes()),
        ["PATCH /api/board/cards/99"],
        "a comment's card is read first, so only the update reached the board"
    );
    no_invalid(&fake);
}
