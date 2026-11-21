//! The **reads** a config calls with an `afkd_board.Board` first: one card by its number, the cards
//! a set of filters keeps, a card's comments, and the board's lists and labels.
//!
//! A read reaches the board only through the `afkd_board.Board` it is passed: it reads no
//! service's settings, and the plugin keeps nothing between calls. Each read
//!
//! 1. decodes its arguments, a refusal sent before any request ([`decode`]): an empty key, a
//!    `limit` outside 1 to 250, and a handle that is no card's;
//! 2. reads the board, `GET /api/board`, for its id, its lists and its labels;
//! 3. checks that a card it was passed is of that board, and that each list it names is one
//!    the board has, before any card is read;
//! 4. reads what it reads;
//! 5. answers with handles keyed `afkd_board:<board uuid>/<object uuid>`, or a value, in one reply
//!    under afkd's 8 MiB for a call ([`fit_call`]).
//!
//! The API filters one list per query, so `cards` of several lists, or with its archived
//! cards, reads each list and each of the two in a query of its own, and merges them in board
//! order ([`merge`]).

use std::time::Instant;

use serde::Serialize;
use serde_json::{Map, Value};

use crate::board::{card_arg, handle_key, other_board, Board};
use crate::client::{BoardAnswer, BoardClient, CardAnswer, CommentAnswer, ListAnswer, CALL_BUDGET};
use crate::rfc3339::unix_millis;
use crate::wire::{fit_call, ok_handle, ok_handles, ok_value, refusal, WireHandle};

/// The reads, in the order the manifest declares them.
pub(crate) const READS: &[&str] = &["card", "cards", "comments", "lists", "labels"];

/// How many cards `cards` reads when its `limit` is left out.
const DEFAULT_LIMIT: i64 = 50;

/// The most cards `cards` reads: the API's largest `limit`.
const MAX_LIMIT: i64 = 250;

/// The fields of an `afkd_board.Card` handle, in the manifest's order. Each is always sent, so afkd
/// never finds one missing. A time is unix milliseconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct CardFields {
    pub(crate) number: i64,
    pub(crate) title: String,
    /// Markdown, `""` when it has none.
    pub(crate) description: String,
    /// The name of the list it is in.
    pub(crate) list: String,
    pub(crate) labels: Vec<String>,
    pub(crate) archived: bool,
    pub(crate) done: bool,
    pub(crate) blocked_by: Vec<i64>,
    pub(crate) blocking: Vec<i64>,
    pub(crate) blocked: bool,
    pub(crate) url: String,
    pub(crate) created_at: u64,
    pub(crate) updated_at: u64,
}

/// The fields of an `afkd_board.Comment` handle, in the manifest's order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct CommentFields {
    /// The number of the card it is on.
    pub(crate) card: i64,
    pub(crate) body: String,
    /// The person's name; `None` for a comment a key made, and once the person's account is
    /// deleted.
    pub(crate) author: Option<String>,
    /// The id of the run it was made in.
    pub(crate) run: Option<String>,
    pub(crate) created_at: u64,
    /// Its card's url.
    pub(crate) url: String,
}

/// What `cards` keeps and how many, as its arguments say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CardQuery {
    /// The lists a card is in any of, each named once; `None` keeps every list.
    pub(crate) lists: Option<Vec<String>>,
    pub(crate) labels: Vec<String>,
    pub(crate) without_labels: Vec<String>,
    pub(crate) done: Option<bool>,
    pub(crate) blocked: Option<bool>,
    pub(crate) text: Option<String>,
    pub(crate) include_archived: bool,
    pub(crate) limit: usize,
}

impl CardQuery {
    /// The query of `GET /api/board/cards` that reads the cards of the list `list`, or of every
    /// list, archived or not as `archived` says.
    fn pairs(&self, list: Option<&str>, archived: bool) -> Vec<(&'static str, String)> {
        let mut pairs = Vec::new();
        if let Some(list) = list {
            pairs.push(("list", list.to_string()));
        }
        pairs.extend(self.labels.iter().map(|l| ("labels", l.clone())));
        pairs.extend(
            self.without_labels
                .iter()
                .map(|l| ("without_labels", l.clone())),
        );
        if let Some(text) = &self.text {
            pairs.push(("text", text.clone()));
        }
        pairs.push(("archived", archived.to_string()));
        if let Some(done) = self.done {
            pairs.push(("done", done.to_string()));
        }
        if let Some(blocked) = self.blocked {
            pairs.push(("blocked", blocked.to_string()));
        }
        pairs.push(("limit", self.limit.to_string()));
        pairs
    }
}

/// One read, decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReadCall {
    Card {
        number: i64,
    },
    Cards(CardQuery),
    /// `comments` of the card a handle names: its number and its board's uuid.
    Comments {
        number: i64,
        board: String,
    },
    Lists,
    Labels,
}

/// Do the read `action` with `args`, and answer it: the reply line, `ok` with what it read,
/// or `ok:false` with the sentence refusing it, which is diagnosed too. A read that lands is
/// narrated.
pub(crate) fn answer(action: &str, args: &Map<String, Value>) -> String {
    let read = decode(action, args).and_then(|(board, call)| {
        let client = BoardClient::new(&board, Instant::now() + CALL_BUDGET);
        let (reply, said) = run(action, &call, &client)?;
        Ok((fit_call(action, reply)?, said))
    });
    match read {
        Ok((line, said)) => {
            eprintln!("afkd-afkd_board: {action}: {said}");
            line
        }
        Err(problem) => refusal(&problem),
    }
}

/// The board and the read `args` write for `action`, or the sentence refusing them.
///
/// # Errors
///
/// Returns the sentence for a `b` that is no board or holds an empty key, an argument missing
/// or of another type than the manifest's, a `limit` outside 1 to 250, and a `card` that is no
/// card's handle.
pub(crate) fn decode(action: &str, args: &Map<String, Value>) -> Result<(Board, ReadCall), String> {
    let board = Board::from_arg(action, args.get("b"))?;
    let call = match action {
        "card" => ReadCall::Card {
            number: required(action, args, "number", Value::as_i64, "an int")?,
        },
        "cards" => ReadCall::Cards(card_query(args)?),
        "comments" => {
            let (number, board) = card_arg(action, args, "card")?;
            ReadCall::Comments { number, board }
        }
        "lists" => ReadCall::Lists,
        "labels" => ReadCall::Labels,
        _ => return Err(format!("no action `{action}`")),
    };
    Ok((board, call))
}

/// What `cards`'s arguments keep, or the sentence refusing them.
fn card_query(args: &Map<String, Value>) -> Result<CardQuery, String> {
    let names = |param| -> Result<Option<Vec<String>>, String> {
        let Some(items) = optional("cards", args, param, Value::as_array, "a list[str]")? else {
            return Ok(None);
        };
        items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| format!("cards: parameter {param} must be a list[str]"))
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some)
    };
    let lists = names("list")?.map(|mut lists| {
        let mut seen = Vec::new();
        lists.retain(|l| {
            let first = !seen.contains(l);
            seen.push(l.clone());
            first
        });
        lists
    });
    let limit = optional("cards", args, "limit", Value::as_i64, "an int")?.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(format!("cards: limit is 1 to {MAX_LIMIT}, not {limit}"));
    }
    Ok(CardQuery {
        lists,
        labels: names("labels")?.unwrap_or_default(),
        without_labels: names("without_labels")?.unwrap_or_default(),
        done: optional("cards", args, "done", Value::as_bool, "a bool")?,
        blocked: optional("cards", args, "blocked", Value::as_bool, "a bool")?,
        text: optional("cards", args, "text", Value::as_str, "a str")?.map(str::to_string),
        include_archived: optional("cards", args, "include_archived", Value::as_bool, "a bool")?
            .unwrap_or(false),
        limit: usize::try_from(limit).unwrap_or(0),
    })
}

/// The argument `param` of `action` read by `read`, or the sentence refusing its absence or a
/// value that is not `what`.
pub(crate) fn required<'a, T>(
    action: &str,
    args: &'a Map<String, Value>,
    param: &str,
    read: impl Fn(&'a Value) -> Option<T>,
    what: &str,
) -> Result<T, String> {
    let value = args
        .get(param)
        .ok_or_else(|| format!("{action}: parameter {param} is required"))?;
    read(value).ok_or_else(|| format!("{action}: parameter {param} must be {what}"))
}

/// The argument `param` of `action` read by `read`, `None` when it is left out or `nil`, or the
/// sentence refusing a value that is not `what`.
pub(crate) fn optional<'a, T>(
    action: &str,
    args: &'a Map<String, Value>,
    param: &str,
    read: impl Fn(&'a Value) -> Option<T>,
    what: &str,
) -> Result<Option<T>, String> {
    match args.get(param) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => read(value)
            .map(Some)
            .ok_or_else(|| format!("{action}: parameter {param} must be {what}")),
    }
}

/// Do the read `call` through `client`: the ok reply and the line narrating it.
fn run(action: &str, call: &ReadCall, client: &BoardClient) -> Result<(String, String), String> {
    let board = client.board(action)?;
    match call {
        ReadCall::Lists => Ok((ok_value(&board.lists), count(board.lists.len(), "list"))),
        ReadCall::Labels => Ok((ok_value(&board.labels), count(board.labels.len(), "label"))),
        ReadCall::Card { number } => {
            let card = card_handle(action, &board.id, &client.card(action, *number)?)?;
            Ok((ok_handle(&card), format!("card #{number}")))
        }
        ReadCall::Cards(query) => {
            let cards = cards(client, &board, query)?
                .iter()
                .map(|card| card_handle(action, &board.id, card))
                .collect::<Result<Vec<_>, _>>()?;
            Ok((ok_handles(&cards), count(cards.len(), "card")))
        }
        ReadCall::Comments {
            number,
            board: theirs,
        } => {
            if *theirs != board.id {
                return Err(other_board(action, &number.to_string(), theirs, &board.id));
            }
            let card = client.card(action, *number)?;
            let comments = client
                .comments(action, *number)?
                .iter()
                .map(|comment| comment_handle(action, &board.id, &card, comment))
                .collect::<Result<Vec<_>, _>>()?;
            let said = format!("{} on #{number}", count(comments.len(), "comment"));
            Ok((ok_handles(&comments), said))
        }
    }
}

/// `n` of `what`, as a narration counts them.
fn count(n: usize, what: &str) -> String {
    format!("{n} {what}{}", if n == 1 { "" } else { "s" })
}

/// The cards `query` keeps of the board `board`: each list it names checked against the
/// board's first, then one query a list and an archived state, merged in board order.
fn cards(
    client: &BoardClient,
    board: &BoardAnswer,
    query: &CardQuery,
) -> Result<Vec<CardAnswer>, String> {
    let lists: Vec<Option<&str>> = match &query.lists {
        None => vec![None],
        Some(names) => {
            if let Some(missing) = names
                .iter()
                .find(|name| !board.lists.iter().any(|l| &l.name == *name))
            {
                return Err(no_list("cards", missing, &board.lists));
            }
            names.iter().map(|name| Some(name.as_str())).collect()
        }
    };
    let archived: &[bool] = if query.include_archived {
        &[false, true]
    } else {
        &[false]
    };
    let mut cards = Vec::new();
    for list in &lists {
        for &archived in archived {
            cards.extend(client.cards("cards", &query.pairs(*list, archived))?);
        }
    }
    Ok(merge(&board.lists, cards, query.limit))
}

/// The cards several queries read, in board order and cut to `limit`: by their list's place
/// among `lists`, then archived after not. Each query comes back in the API's own order, which
/// one stable sort keeps within each list and archived state, so the first `limit` is the API's
/// own first `limit` of the whole board.
pub(crate) fn merge(
    lists: &[ListAnswer],
    mut cards: Vec<CardAnswer>,
    limit: usize,
) -> Vec<CardAnswer> {
    let place = |card: &CardAnswer| {
        lists
            .iter()
            .position(|l| l.name == card.list)
            .unwrap_or(usize::MAX)
    };
    cards.sort_by_key(|card| (place(card), card.archived));
    cards.truncate(limit);
    cards
}

/// The sentence refusing the list `name` passed to `action`, which the board lacks, naming the
/// lists it has as the API names them.
pub(crate) fn no_list(action: &str, name: &str, lists: &[ListAnswer]) -> String {
    let has: Vec<String> = lists.iter().map(|l| format!("`{}`", l.name)).collect();
    let has = match has.as_slice() {
        [] => "none".to_string(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    };
    format!("{action}: The board has no list `{name}`; it has {has}.")
}

/// The instant `at` an answer of the board gave, in unix milliseconds.
fn millis(action: &str, at: &str) -> Result<u64, String> {
    unix_millis(at)
        .ok_or_else(|| format!("{action}: the board answered a time that is no instant, `{at}`"))
}

/// The handle of the card `card` of the board whose uuid is `board`: its number as its id.
pub(crate) fn card_handle(
    action: &str,
    board: &str,
    card: &CardAnswer,
) -> Result<WireHandle<CardFields>, String> {
    Ok(WireHandle {
        id: card.number.to_string(),
        key: handle_key(board, &card.id),
        fields: CardFields {
            number: card.number,
            title: card.title.clone(),
            description: card.description.clone(),
            list: card.list.clone(),
            labels: card.labels.clone(),
            archived: card.archived,
            done: card.done,
            blocked_by: card.blocked_by.clone(),
            blocking: card.blocking.clone(),
            blocked: card.blocked,
            url: card.url.clone(),
            created_at: millis(action, &card.created_at)?,
            updated_at: millis(action, &card.updated_at)?,
        },
    })
}

/// The handle of the comment `comment` on the card `card` of the board whose uuid is `board`:
/// its author the person who made it, never a key, and its url its card's.
pub(crate) fn comment_handle(
    action: &str,
    board: &str,
    card: &CardAnswer,
    comment: &CommentAnswer,
) -> Result<WireHandle<CommentFields>, String> {
    Ok(WireHandle {
        id: comment.id.clone(),
        key: handle_key(board, &comment.id),
        fields: CommentFields {
            card: card.number,
            body: comment.body.clone(),
            author: if comment.by_key {
                None
            } else {
                comment.by.clone()
            },
            run: comment.run.as_ref().map(|run| run.id.clone()),
            created_at: millis(action, &comment.created_at)?,
            url: card.url.clone(),
        },
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

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

    fn card(number: i64, list: &str, archived: bool) -> CardAnswer {
        CardAnswer {
            id: format!("c-{number}"),
            number,
            title: String::new(),
            description: String::new(),
            list: list.to_string(),
            labels: Vec::new(),
            done: false,
            blocked: false,
            blocked_by: Vec::new(),
            blocking: Vec::new(),
            archived,
            url: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    /// `cards`'s every filter decodes, its lists each named once, and `nil` keeps both.
    #[test]
    fn cards_decodes_every_filter() {
        let (_, call) = decode(
            "cards",
            &args(json!({"list": ["Todo", "Done", "Todo"], "labels": ["bug"],
                         "without_labels": [], "done": true, "blocked": null, "text": "retry",
                         "include_archived": true, "limit": 250})),
        )
        .unwrap();
        assert_eq!(
            call,
            ReadCall::Cards(CardQuery {
                lists: Some(vec!["Todo".to_string(), "Done".to_string()]),
                labels: vec!["bug".to_string()],
                without_labels: Vec::new(),
                done: Some(true),
                blocked: None,
                text: Some("retry".to_string()),
                include_archived: true,
                limit: 250,
            })
        );
        let (_, bare) = decode("cards", &args(json!({}))).unwrap();
        let ReadCall::Cards(bare) = bare else {
            panic!("{bare:?}")
        };
        assert_eq!(
            (bare.lists, bare.limit, bare.include_archived),
            (None, 50, false)
        );
        let (_, none) = decode("cards", &args(json!({"list": []}))).unwrap();
        assert!(
            matches!(none, ReadCall::Cards(CardQuery { lists: Some(ref l), .. }) if l.is_empty())
        );
    }

    /// A `limit` outside 1 to 250 and a filter of another type are refused before any request.
    #[test]
    fn cards_refuses_a_limit_outside_its_range() {
        for limit in [0, 251, -1] {
            assert_eq!(
                decode("cards", &args(json!({ "limit": limit }))).unwrap_err(),
                format!("cards: limit is 1 to 250, not {limit}")
            );
        }
        assert_eq!(
            decode("cards", &args(json!({"labels": [3]}))).unwrap_err(),
            "cards: parameter labels must be a list[str]"
        );
        assert_eq!(
            decode("cards", &args(json!({"done": "yes"}))).unwrap_err(),
            "cards: parameter done must be a bool"
        );
    }

    /// `comments` reads the card's number from its id and its board from its key, and a
    /// handle of another shape is refused before any request.
    #[test]
    fn comments_reads_the_cards_number_and_board() {
        let (_, call) = decode(
            "comments",
            &args(json!({"card": {"id": "17", "key": "afkd_board:b-1/c-17"}})),
        )
        .unwrap();
        assert_eq!(
            call,
            ReadCall::Comments {
                number: 17,
                board: "b-1".to_string()
            }
        );
        for card in [
            json!({"id": "ENG-17", "key": "afkd_board:b-1/c-17"}),
            json!({"id": "17", "key": "linear:b-1/c-17"}),
        ] {
            let e = decode("comments", &args(json!({ "card": card }))).unwrap_err();
            assert!(
                e.starts_with("comments: card is no afkd_board.Card: "),
                "{e}"
            );
        }
        assert_eq!(
            decode("card", &args(json!({}))).unwrap_err(),
            "card: parameter number is required"
        );
    }

    /// Cards read list by list, written out of the board's order, come back in it, each list's
    /// archived cards after its others, the order within each query kept, and cut to `limit`.
    #[test]
    fn cards_of_several_queries_merge_in_board_order() {
        let lists: Vec<ListAnswer> = ["Todo", "Doing", "Done"]
            .iter()
            .map(|name| ListAnswer {
                id: format!("l-{name}"),
                name: (*name).to_string(),
            })
            .collect();
        let read = vec![
            card(5, "Done", false),
            card(9, "Done", true),
            card(8, "Done", true),
            card(3, "Todo", false),
            card(1, "Todo", false),
            card(7, "Todo", true),
            card(2, "Doing", false),
        ];
        let numbers = |cards: Vec<CardAnswer>| cards.iter().map(|c| c.number).collect::<Vec<_>>();
        assert_eq!(
            numbers(merge(&lists, read.clone(), 250)),
            [3, 1, 7, 2, 5, 9, 8]
        );
        assert_eq!(numbers(merge(&lists, read, 3)), [3, 1, 7]);
    }

    /// An unknown list is named with the board's lists, as the API's own sentence names them.
    #[test]
    fn an_unknown_list_names_the_boards_lists() {
        let list = |name: &str| ListAnswer {
            id: String::new(),
            name: name.to_string(),
        };
        assert_eq!(
            no_list(
                "cards",
                "Doing",
                &[list("Todo"), list("In progress"), list("Done")]
            ),
            "cards: The board has no list `Doing`; it has `Todo`, `In progress` and `Done`."
        );
        assert_eq!(
            no_list("cards", "Doing", &[list("Todo")]),
            "cards: The board has no list `Doing`; it has `Todo`."
        );
        assert_eq!(
            no_list("move_card", "Doing", &[]),
            "move_card: The board has no list `Doing`; it has none."
        );
    }
}
