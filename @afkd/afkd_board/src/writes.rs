//! The **writes** a config calls with an `afkd_board.Board` first: the cards, checklists,
//! comments and lists of a board on app.afkd.sh, each of a card, a checklist or a comment made
//! under the run it names, so the card's activity shows the change under that run.
//!
//! A write reaches the board only through the `afkd_board.Board` it is passed, and the plugin
//! keeps nothing between calls. Each write
//!
//! 1. decodes its arguments, a refusal sent before any request ([`decode`]): an empty key, `at`
//!    with `after`, an argument missing or of another type, a handle that is no card's, and a
//!    `run` that is no `afkd.Run`;
//! 2. reads the board, `GET /api/board`, for its id, its lists and its labels;
//! 3. checks that every card it was passed is of that board, and that each list it names is one
//!    the board has, matched exactly, before anything is written;
//! 4. writes, `run` in the body when the call has one and otherwise no `run` at all, so the
//!    change shows under the key's name;
//! 5. answers with the handle of the card or comment it wrote, the list it wrote, or `{"ok":true}`
//!    for a list it deleted, in one reply under afkd's 8 MiB for a call ([`fit_call`]).
//!
//! `remove_label` of a label the board lacks writes nothing and answers the card as it is: the
//! API refuses it, but the card lacks it all the same. `delete_card` reads the card before it
//! deletes it, and answers that read, since the API answers `204`.

use std::time::Instant;

use serde_json::{Map, Value};

use crate::board::{card_arg, other_board, run_arg, Board};
use crate::client::{BoardAnswer, BoardClient, ListAnswer, NewCard, Place, CALL_BUDGET};
use crate::reads::{card_handle, comment_handle, no_list, optional, required};
use crate::wire::{fit_call, ok_handle, ok_nothing, ok_value, refusal};

/// The writes, in the order the manifest declares them.
pub(crate) const WRITES: &[&str] = &[
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

/// A card a write names, from its handle: its number, and the uuid of the board it is of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CardRef {
    pub(crate) number: i64,
    pub(crate) board: String,
}

/// One write, decoded. A list is named by its name, and a card by its handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WriteCall {
    CreateCard {
        list: String,
        title: String,
        description: Option<String>,
        labels: Vec<String>,
        /// The text of each item: a card is made with its items not ticked.
        checklist: Vec<String>,
        place: Option<Place<CardRef>>,
    },
    UpdateCard {
        card: CardRef,
        title: Option<String>,
        description: Option<String>,
    },
    MoveCard {
        card: CardRef,
        list: String,
        place: Option<Place<CardRef>>,
    },
    AddLabel {
        card: CardRef,
        label: String,
    },
    RemoveLabel {
        card: CardRef,
        label: String,
    },
    AddItem {
        card: CardRef,
        text: String,
    },
    CheckItem {
        card: CardRef,
        index: i64,
        done: bool,
    },
    RemoveItem {
        card: CardRef,
        index: i64,
    },
    MarkDone {
        card: CardRef,
        done: bool,
    },
    /// `add_blocked_by` when `blocked`, `remove_blocked_by` when not.
    Block {
        card: CardRef,
        other: CardRef,
        blocked: bool,
    },
    CreateComment {
        card: CardRef,
        body: String,
    },
    /// `archive_card` when `archived`, `unarchive_card` when not.
    Archive {
        card: CardRef,
        archived: bool,
    },
    DeleteCard {
        card: CardRef,
    },
    CreateList {
        name: String,
        place: Option<Place<String>>,
    },
    UpdateList {
        list: String,
        name: Option<String>,
        place: Option<Place<String>>,
    },
    DeleteList {
        list: String,
    },
}

impl WriteCall {
    /// Every card the call names: the one it writes, and the ones it blocks by or goes after.
    fn cards(&self) -> Vec<&CardRef> {
        fn after(place: &Option<Place<CardRef>>) -> Option<&CardRef> {
            match place {
                Some(Place::After(card)) => Some(card),
                _ => None,
            }
        }
        match self {
            Self::CreateCard { place, .. } => after(place).into_iter().collect(),
            Self::MoveCard { card, place, .. } => {
                [Some(card), after(place)].into_iter().flatten().collect()
            }
            Self::Block { card, other, .. } => vec![card, other],
            Self::UpdateCard { card, .. }
            | Self::AddLabel { card, .. }
            | Self::RemoveLabel { card, .. }
            | Self::AddItem { card, .. }
            | Self::CheckItem { card, .. }
            | Self::RemoveItem { card, .. }
            | Self::MarkDone { card, .. }
            | Self::CreateComment { card, .. }
            | Self::Archive { card, .. }
            | Self::DeleteCard { card } => vec![card],
            Self::CreateList { .. } | Self::UpdateList { .. } | Self::DeleteList { .. } => {
                Vec::new()
            }
        }
    }

    /// Whether the call takes the run it is made in: every write of a card, its checklist or a
    /// comment does, and no write of a list.
    fn takes_run(&self) -> bool {
        !matches!(
            self,
            Self::CreateList { .. } | Self::UpdateList { .. } | Self::DeleteList { .. }
        )
    }
}

/// Do the write `action` with `args`, and answer it: the reply line, `ok` with what it wrote,
/// or `ok:false` with the sentence refusing it, which is diagnosed too. A write that lands is
/// narrated.
pub(crate) fn answer(action: &str, args: &Map<String, Value>) -> String {
    let written = decode(action, args).and_then(|(board, call, made_in)| {
        let client = BoardClient::new(&board, Instant::now() + CALL_BUDGET);
        let (reply, said) = run(action, &call, made_in.as_ref(), &client)?;
        Ok((fit_call(action, reply)?, said))
    });
    match written {
        Ok((line, said)) => {
            eprintln!("afkd-afkd_board: {action}: {said}");
            line
        }
        Err(problem) => refusal(&problem),
    }
}

/// The board, the write and the run `args` make for `action`, or the sentence refusing them.
///
/// # Errors
///
/// Returns the sentence for a `b` that is no board or holds an empty key, an argument missing or
/// of another type than the manifest's, a `card` that is no card's handle, an `at` that is
/// neither `top` nor `bottom`, `at` with `after`, and a `run` that is no `afkd.Run`.
pub(crate) fn decode(
    action: &str,
    args: &Map<String, Value>,
) -> Result<(Board, WriteCall, Option<Value>), String> {
    let board = Board::from_arg(action, args.get("b"))?;
    let card = |param: &str| {
        card_arg(action, args, param).map(|(number, board)| CardRef { number, board })
    };
    let text =
        |param: &str| required(action, args, param, Value::as_str, "a str").map(str::to_string);
    let maybe = |param: &str| {
        optional(action, args, param, Value::as_str, "a str").map(|text| text.map(str::to_string))
    };
    let int = |param: &str| required(action, args, param, Value::as_i64, "an int");
    let done =
        || optional(action, args, "done", Value::as_bool, "a bool").map(|d| d.unwrap_or(true));
    let call = match action {
        "create_card" => WriteCall::CreateCard {
            list: text("list")?,
            title: text("title")?,
            description: maybe("description")?,
            labels: strings(action, args, "labels")?,
            checklist: items(action, args)?,
            place: place(action, args, |_| card("after").map(Some))?,
        },
        "update_card" => WriteCall::UpdateCard {
            card: card("card")?,
            title: maybe("title")?,
            description: maybe("description")?,
        },
        "move_card" => WriteCall::MoveCard {
            card: card("card")?,
            list: text("list")?,
            place: place(action, args, |_| card("after").map(Some))?,
        },
        "add_label" | "remove_label" => {
            let (card, label) = (card("card")?, text("label")?);
            if action == "add_label" {
                WriteCall::AddLabel { card, label }
            } else {
                WriteCall::RemoveLabel { card, label }
            }
        }
        "add_item" => WriteCall::AddItem {
            card: card("card")?,
            text: text("text")?,
        },
        "check_item" => WriteCall::CheckItem {
            card: card("card")?,
            index: int("index")?,
            done: done()?,
        },
        "remove_item" => WriteCall::RemoveItem {
            card: card("card")?,
            index: int("index")?,
        },
        "mark_done" => WriteCall::MarkDone {
            card: card("card")?,
            done: done()?,
        },
        "add_blocked_by" | "remove_blocked_by" => WriteCall::Block {
            card: card("card")?,
            other: card("other")?,
            blocked: action == "add_blocked_by",
        },
        "create_comment" => WriteCall::CreateComment {
            card: card("card")?,
            body: text("body")?,
        },
        "archive_card" | "unarchive_card" => WriteCall::Archive {
            card: card("card")?,
            archived: action == "archive_card",
        },
        "delete_card" => WriteCall::DeleteCard {
            card: card("card")?,
        },
        "create_list" => WriteCall::CreateList {
            name: text("name")?,
            place: place(action, args, |_| text("after").map(Some))?,
        },
        "update_list" => WriteCall::UpdateList {
            list: text("list")?,
            name: maybe("name")?,
            place: place(action, args, |_| text("after").map(Some))?,
        },
        "delete_list" => WriteCall::DeleteList {
            list: text("list")?,
        },
        _ => return Err(format!("no action `{action}`")),
    };
    let run = if call.takes_run() {
        run_arg(action, args)?
    } else {
        None
    };
    Ok((board, call, run))
}

/// The place `args` name for `action`: `at`, `top` or `bottom`, or `after`, read by `after`
/// when it is given; `None` for neither, and a refusal for both.
fn place<A>(
    action: &str,
    args: &Map<String, Value>,
    after: impl FnOnce(&Value) -> Result<Option<A>, String>,
) -> Result<Option<Place<A>>, String> {
    let at = optional(action, args, "at", Value::as_str, "top or bottom")?;
    let named = args.get("after").filter(|after| !after.is_null());
    match (at, named) {
        (Some(_), Some(_)) => Err(format!("{action}: at and after cannot both be given")),
        (Some("top"), None) => Ok(Some(Place::Top)),
        (Some("bottom"), None) => Ok(Some(Place::Bottom)),
        (Some(_), None) => Err(format!("{action}: parameter at must be top or bottom")),
        (None, Some(named)) => Ok(after(named)?.map(Place::After)),
        (None, None) => Ok(None),
    }
}

/// The strings of the list `param` of `action`, none when it is left out.
fn strings(action: &str, args: &Map<String, Value>, param: &str) -> Result<Vec<String>, String> {
    let Some(items) = optional(action, args, param, Value::as_array, "a list[str]")? else {
        return Ok(Vec::new());
    };
    items
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("{action}: parameter {param} must be a list[str]"))
        })
        .collect()
}

/// The text of each `afkd_board.Item` of the `checklist` of `action`, none when it is left out.
/// A card is made with its items not ticked, so an item's `done` is not sent.
fn items(action: &str, args: &Map<String, Value>) -> Result<Vec<String>, String> {
    let Some(items) = optional(action, args, "checklist", Value::as_array, "a list[Item]")? else {
        return Ok(Vec::new());
    };
    items
        .iter()
        .map(|item| {
            item.get("text")
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| format!("{action}: parameter checklist must be a list[Item]"))
        })
        .collect()
}

/// Do the write `call` through `client`, `run` the run it is made in: the ok reply and the line
/// narrating it.
fn run(
    action: &str,
    call: &WriteCall,
    run: Option<&Value>,
    client: &BoardClient,
) -> Result<(String, String), String> {
    let board = client.board(action)?;
    for card in call.cards() {
        if card.board != board.id {
            let number = card.number.to_string();
            return Err(other_board(action, &number, &card.board, &board.id));
        }
    }
    let handle = |card: &crate::client::CardAnswer| card_handle(action, &board.id, card);
    match call {
        WriteCall::CreateCard {
            list,
            title,
            description,
            labels,
            checklist,
            place,
        } => {
            list_id(action, &board, list)?;
            let place = card_place(place.as_ref());
            let card = client.create_card(
                action,
                &NewCard {
                    list,
                    title,
                    description: description.as_deref(),
                    labels,
                    checklist,
                    place: place.as_ref(),
                },
                run,
            )?;
            let said = format!("created #{} in {list}", card.number);
            Ok((ok_handle(&handle(&card)?), said))
        }
        WriteCall::UpdateCard {
            card,
            title,
            description,
        } => {
            let answer = client.update_card(
                action,
                card.number,
                title.as_deref(),
                description.as_deref(),
                run,
            )?;
            Ok((
                ok_handle(&handle(&answer)?),
                format!("updated #{}", card.number),
            ))
        }
        WriteCall::MoveCard { card, list, place } => {
            list_id(action, &board, list)?;
            let place = card_place(place.as_ref()).unwrap_or(Place::Bottom);
            let answer = client.move_card(action, card.number, list, &place, run)?;
            let said = format!("moved #{} to {list}", card.number);
            Ok((ok_handle(&handle(&answer)?), said))
        }
        WriteCall::AddLabel { card, label } => {
            let answer = client.add_label(action, card.number, label, run)?;
            let said = format!("labelled #{} {label}", card.number);
            Ok((ok_handle(&handle(&answer)?), said))
        }
        WriteCall::RemoveLabel { card, label } => {
            if !board.labels.iter().any(|known| known.name == *label) {
                let answer = client.card(action, card.number)?;
                let said = format!(
                    "#{} carries no label {label}, which the board lacks; nothing is written",
                    card.number
                );
                return Ok((ok_handle(&handle(&answer)?), said));
            }
            let answer = client.remove_label(action, card.number, label, run)?;
            let said = format!("took {label} off #{}", card.number);
            Ok((ok_handle(&handle(&answer)?), said))
        }
        WriteCall::AddItem { card, text } => {
            let answer = client.add_item(action, card.number, text, run)?;
            Ok((
                ok_handle(&handle(&answer)?),
                format!("added an item to #{}", card.number),
            ))
        }
        WriteCall::CheckItem { card, index, done } => {
            let answer = client.check_item(action, card.number, *index, *done, run)?;
            let said = format!("item {index} of #{} marked done: {done}", card.number);
            Ok((ok_handle(&handle(&answer)?), said))
        }
        WriteCall::RemoveItem { card, index } => {
            let answer = client.remove_item(action, card.number, *index, run)?;
            let said = format!("removed item {index} of #{}", card.number);
            Ok((ok_handle(&handle(&answer)?), said))
        }
        WriteCall::MarkDone { card, done } => {
            let answer = client.mark_done(action, card.number, *done, run)?;
            let said = format!("#{} marked done: {done}", card.number);
            Ok((ok_handle(&handle(&answer)?), said))
        }
        WriteCall::Block {
            card,
            other,
            blocked,
        } => {
            let answer = client.block(action, card.number, other.number, *blocked, run)?;
            let verb = if *blocked { "blocked" } else { "unblocked" };
            let said = format!("{verb} #{} by #{}", card.number, other.number);
            Ok((ok_handle(&handle(&answer)?), said))
        }
        WriteCall::CreateComment { card, body } => {
            let on = client.card(action, card.number)?;
            let comment = client.create_comment(action, card.number, body, run)?;
            let handle = comment_handle(action, &board.id, &on, &comment)?;
            Ok((ok_handle(&handle), format!("commented on #{}", card.number)))
        }
        WriteCall::Archive { card, archived } => {
            let answer = client.archive(action, card.number, *archived, run)?;
            let verb = if *archived { "archived" } else { "unarchived" };
            Ok((
                ok_handle(&handle(&answer)?),
                format!("{verb} #{}", card.number),
            ))
        }
        WriteCall::DeleteCard { card } => {
            let read = client.card(action, card.number)?;
            client.delete_card(action, card.number)?;
            Ok((
                ok_handle(&handle(&read)?),
                format!("deleted #{}", card.number),
            ))
        }
        WriteCall::CreateList { name, place } => {
            let place = list_place(action, &board, place.as_ref())?;
            let list = client.create_list(action, name, place.as_ref())?;
            Ok((ok_value(&list), format!("created list {}", list.name)))
        }
        WriteCall::UpdateList { list, name, place } => {
            let id = list_id(action, &board, list)?;
            let place = list_place(action, &board, place.as_ref())?;
            let made = client.update_list(action, id, name.as_deref(), place.as_ref())?;
            Ok((ok_value(&made), format!("updated list {}", made.name)))
        }
        WriteCall::DeleteList { list } => {
            let id = list_id(action, &board, list)?;
            client.delete_list(action, id)?;
            Ok((ok_nothing(), format!("deleted list {list}")))
        }
    }
}

/// The place of a card as the API takes it, `after` by number: the board of an `after` card is
/// already checked.
fn card_place(place: Option<&Place<CardRef>>) -> Option<Place<i64>> {
    place.map(|place| match place {
        Place::Top => Place::Top,
        Place::Bottom => Place::Bottom,
        Place::After(card) => Place::After(card.number),
    })
}

/// The place of a list as the API takes it, `after` by its id: the list named is matched
/// exactly, and refused naming the lists the board has when it has none of that name.
fn list_place(
    action: &str,
    board: &BoardAnswer,
    place: Option<&Place<String>>,
) -> Result<Option<Place<String>>, String> {
    place
        .map(|place| match place {
            Place::Top => Ok(Place::Top),
            Place::Bottom => Ok(Place::Bottom),
            Place::After(name) => {
                list_id(action, board, name).map(|id| Place::After(id.to_string()))
            }
        })
        .transpose()
}

/// The id of the list named `name`, matched exactly; or the sentence refusing a name the board
/// lacks, naming the lists it has.
fn list_id<'a>(action: &str, board: &'a BoardAnswer, name: &str) -> Result<&'a str, String> {
    board
        .lists
        .iter()
        .find(|list: &&ListAnswer| list.name == name)
        .map(|list| list.id.as_str())
        .ok_or_else(|| no_list(action, name, &board.lists))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const CARD: &str = r#"{"id": "17", "key": "afkd_board:b-1/c-17"}"#;

    /// `args` as afkd sends them, beside a board.
    fn args(value: Value) -> Map<String, Value> {
        let Value::Object(mut map) = value else {
            panic!("{value}")
        };
        map.insert(
            "b".to_string(),
            json!({"key": "afk_board_k", "base_url": "http://127.0.0.1:9"}),
        );
        map
    }

    fn card(number: i64) -> CardRef {
        CardRef {
            number,
            board: "b-1".to_string(),
        }
    }

    fn handle() -> Value {
        serde_json::from_str(CARD).unwrap()
    }

    const RUN: &str = r#"{"machine": "nexus", "service": "develop/review.0", "instance": "3",
                          "id": "261116-163511-card-17-2"}"#;

    fn run() -> Value {
        serde_json::from_str(RUN).unwrap()
    }

    /// `at` with `after` is refused before any request, in a card's write and a list's, and
    /// `nil` for either is neither.
    #[test]
    fn at_with_after_is_refused_before_any_request() {
        for (action, extra) in [
            ("create_card", json!({"list": "Todo", "title": "T"})),
            ("move_card", json!({"card": handle(), "list": "Todo"})),
        ] {
            let mut given = args(extra.clone());
            given.insert("at".to_string(), json!("top"));
            given.insert("after".to_string(), handle());
            assert_eq!(
                decode(action, &given).unwrap_err(),
                format!("{action}: at and after cannot both be given")
            );
            given.insert("after".to_string(), Value::Null);
            assert!(decode(action, &given).is_ok(), "{action}: after nil");
        }
        for (action, extra) in [
            ("create_list", json!({"name": "Review"})),
            ("update_list", json!({"list": "Todo"})),
        ] {
            let mut given = args(extra);
            given.insert("at".to_string(), json!("bottom"));
            given.insert("after".to_string(), json!("Done"));
            assert_eq!(
                decode(action, &given).unwrap_err(),
                format!("{action}: at and after cannot both be given")
            );
        }
        let wrong = args(json!({"name": "Review", "at": "middle"}));
        assert_eq!(
            decode("create_list", &wrong).unwrap_err(),
            "create_list: parameter at must be top or bottom"
        );
    }

    /// A card's title and list are required, and a card made with everything decodes whole.
    #[test]
    fn create_card_needs_its_list_and_title_and_decodes_the_rest() {
        assert_eq!(
            decode("create_card", &args(json!({"list": "Todo"}))).unwrap_err(),
            "create_card: parameter title is required"
        );
        assert_eq!(
            decode("create_card", &args(json!({"title": "T"}))).unwrap_err(),
            "create_card: parameter list is required"
        );
        let (_, call, run) = decode(
            "create_card",
            &args(json!({"list": "Todo", "title": "T", "description": "D",
                         "labels": ["bug", "afkd"],
                         "checklist": [{"text": "one", "done": false}, {"text": "two", "done": true}],
                         "after": handle(), "run": run()})),
        )
        .unwrap();
        assert_eq!(
            call,
            WriteCall::CreateCard {
                list: "Todo".to_string(),
                title: "T".to_string(),
                description: Some("D".to_string()),
                labels: vec!["bug".to_string(), "afkd".to_string()],
                checklist: vec!["one".to_string(), "two".to_string()],
                place: Some(Place::After(card(17))),
            }
        );
        assert_eq!(run, Some(self::run()));
        let (_, bare, none) =
            decode("create_card", &args(json!({"list": "Todo", "title": "T"}))).unwrap();
        assert_eq!(
            bare,
            WriteCall::CreateCard {
                list: "Todo".to_string(),
                title: "T".to_string(),
                description: None,
                labels: Vec::new(),
                checklist: Vec::new(),
                place: None,
            }
        );
        assert_eq!(none, None, "no run is no run");
    }

    /// A run is decoded for every write of a card, a checklist and a comment, and never for a
    /// write of a list.
    #[test]
    fn the_run_is_decoded_for_card_writes_and_not_list_writes() {
        for action in ["update_card", "add_item", "create_comment", "delete_card"] {
            let given = args(json!({"card": handle(), "text": "x", "body": "x", "run": run()}));
            let (_, _, got) = decode(action, &given).unwrap();
            assert_eq!(got, Some(run()), "{action}");
            let nil = args(json!({"card": handle(), "text": "x", "body": "x", "run": null}));
            assert_eq!(decode(action, &nil).unwrap().2, None, "{action}: nil");
            let bad = args(json!({"card": handle(), "text": "x", "body": "x", "run": "no"}));
            assert_eq!(
                decode(action, &bad).unwrap_err(),
                format!("{action}: parameter run must be an afkd.Run")
            );
        }
        let given = args(json!({"name": "Review", "run": run()}));
        assert_eq!(decode("create_list", &given).unwrap().2, None);
    }

    /// A tick defaults to true, an item's index is its place from 0, and a block names both
    /// cards.
    #[test]
    fn a_tick_defaults_to_true_and_a_block_names_both_cards() {
        let (_, check, _) =
            decode("check_item", &args(json!({"card": handle(), "index": 0}))).unwrap();
        assert_eq!(
            check,
            WriteCall::CheckItem {
                card: card(17),
                index: 0,
                done: true
            }
        );
        let (_, mark, _) =
            decode("mark_done", &args(json!({"card": handle(), "done": false}))).unwrap();
        assert_eq!(
            mark,
            WriteCall::MarkDone {
                card: card(17),
                done: false
            }
        );
        let other = json!({"id": "3", "key": "afkd_board:b-1/c-3"});
        let (_, block, _) = decode(
            "remove_blocked_by",
            &args(json!({"card": handle(), "other": other})),
        )
        .unwrap();
        assert_eq!(
            block,
            WriteCall::Block {
                card: card(17),
                other: card(3),
                blocked: false
            }
        );
    }

    /// An action that is no write is refused, and the list writes name their lists.
    #[test]
    fn an_unknown_action_and_the_list_writes_decode() {
        assert_eq!(
            decode("block_card", &args(json!({}))).unwrap_err(),
            "no action `block_card`"
        );
        let (_, call, _) = decode(
            "update_list",
            &args(json!({"list": "Todo", "name": "Next", "after": "Done"})),
        )
        .unwrap();
        assert_eq!(
            call,
            WriteCall::UpdateList {
                list: "Todo".to_string(),
                name: Some("Next".to_string()),
                place: Some(Place::After("Done".to_string())),
            }
        );
        assert_eq!(
            decode("delete_list", &args(json!({}))).unwrap_err(),
            "delete_list: parameter list is required"
        );
    }
}
