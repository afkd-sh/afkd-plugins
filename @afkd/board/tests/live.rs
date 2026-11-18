//! The live check: every read against a real board, the API the fake stands in for. It runs
//! only with `AFKD_BOARD_LIVE_KEY`, one of a board's API keys, at `AFKD_BOARD_LIVE_URL`
//! (`https://app.afkd.sh` when unset), and otherwise skips, saying so: the release workflow
//! and a run have no key, so the fake's suites are what gates a release. It writes nothing.

mod common;

use serde_json::{json, Value};

use common::once;

/// The board the live check reads, or `None` when no key is set.
fn live_board() -> Option<Value> {
    let key = std::env::var("AFKD_BOARD_LIVE_KEY")
        .ok()
        .filter(|k| !k.is_empty())?;
    let base_url =
        std::env::var("AFKD_BOARD_LIVE_URL").unwrap_or_else(|_| "https://app.afkd.sh".to_string());
    Some(json!({"key": key, "base_url": base_url}))
}

/// The ok reply's `field`.
fn ok<'a>(reply: &'a Value, field: &str) -> &'a Value {
    assert_eq!(reply["ok"], true, "{reply}");
    &reply[field]
}

#[test]
fn every_read_answers_against_a_live_board() {
    let Some(b) = live_board() else {
        eprintln!("SKIP: AFKD_BOARD_LIVE_KEY is unset, so no live board is read");
        return;
    };
    let (lists, _) = once("lists", json!({ "b": b }));
    for list in ok(&lists, "value").as_array().expect("lists") {
        assert!(
            list["id"].as_str().is_some_and(|id| !id.is_empty()),
            "{list}"
        );
    }
    let (labels, _) = once("labels", json!({ "b": b }));
    assert!(ok(&labels, "value").is_array(), "{labels}");

    let (cards, _) = once(
        "cards",
        json!({"b": b, "include_archived": true, "limit": 5}),
    );
    let cards = ok(&cards, "handles").as_array().expect("cards");
    let Some(first) = cards.first() else {
        eprintln!("SKIP: the live board has no card, so no card or comment is read");
        return;
    };
    let key = first["key"].as_str().expect("a key");
    let (board, card) = key
        .strip_prefix("board:")
        .and_then(|k| k.split_once('/'))
        .expect("board:<board>/<card>");
    assert!(!board.is_empty() && !card.is_empty(), "{key}");

    let number = first["fields"]["number"].clone();
    let (one, _) = once("card", json!({"b": b, "number": number}));
    let one = ok(&one, "handle");
    assert_eq!((&one["id"], &one["key"]), (&first["id"], &first["key"]));

    let (comments, _) = once("comments", json!({"b": b, "card": first}));
    for comment in ok(&comments, "handles").as_array().expect("comments") {
        let key = comment["key"].as_str().expect("a key");
        assert!(key.starts_with(&format!("board:{board}/")), "{key}");
        assert_eq!(comment["fields"]["url"], first["fields"]["url"]);
    }
}
