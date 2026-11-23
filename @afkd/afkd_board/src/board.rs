//! An `afkd_board.Board`, the struct a config builds and passes every read: one of a board's API
//! keys and the address the board is served at. Beside it, the keys of the handles a read or a
//! write hands back, `afkd_board:<board uuid>/<object uuid>`, which afkd's `==` compares, the
//! check that a handle a call is passed is of the board it reaches, and the two arguments every
//! write reads the same way: a `Card` handle and the `afkd.Run` it is made in.

use std::fmt;

use serde_json::{json, Map, Value};

use crate::reads::required;

/// What an `afkd_board.Board` holds. Its `Debug` leaves the key out, so nothing that prints one
/// prints the credential.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Board {
    /// The board's API key, `afk_board_…`.
    pub(crate) key: String,
    /// The board's address, its API under `<base_url>/api/board`.
    pub(crate) base_url: String,
}

impl fmt::Debug for Board {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Board")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl Board {
    /// The board `action` was passed as its argument `b`, as afkd sends a struct, an object
    /// holding every field; or the sentence refusing one that is missing, is no board, or
    /// holds an empty key, which reaches no board and is refused before any request.
    pub(crate) fn from_arg(action: &str, arg: Option<&Value>) -> Result<Self, String> {
        let arg = arg.ok_or_else(|| format!("{action}: parameter b is required"))?;
        let field = |name: &str| arg.get(name).and_then(Value::as_str).map(str::to_string);
        let (Some(key), Some(base_url)) = (field("key"), field("base_url")) else {
            return Err(format!(
                "{action}: parameter b must be an afkd_board.Board, with its key and its base_url"
            ));
        };
        if key.trim().is_empty() {
            return Err(format!(
                "{action}: the afkd_board.Board's key is empty; set key to one of the board's API \
                 keys, made in its Board settings"
            ));
        }
        Ok(Self { key, base_url })
    }
}

/// The key of the object `object` of the board `board`, both uuids.
pub(crate) fn handle_key(board: &str, object: &str) -> String {
    format!("afkd_board:{board}/{object}")
}

/// The board's uuid and the object's in a handle's `key`, `afkd_board:<board>/<object>`, or
/// `None` for a key of any other shape.
pub(crate) fn parse_key(key: &str) -> Option<(&str, &str)> {
    let (board, object) = key.strip_prefix("afkd_board:")?.split_once('/')?;
    (!board.is_empty() && !object.is_empty() && !object.contains('/')).then_some((board, object))
}

/// The sentence refusing the card `number` of the board `theirs`, passed `action` with an
/// `afkd_board.Board` that reaches the board `ours`.
pub(crate) fn other_board(action: &str, number: &str, theirs: &str, ours: &str) -> String {
    format!(
        "{action}: card is #{number} of board {theirs}, \
         and the afkd_board.Board reaches board {ours}"
    )
}

/// The number and the board's uuid of the `afkd_board.Card` handle `action` was passed as `param`,
/// as afkd sends a handle, `{"id": "<number>", "key": "afkd_board:<board>/<card>"}`; or the
/// sentence refusing an argument that is missing, is no object, or is no card's handle.
pub(crate) fn card_arg(
    action: &str,
    args: &Map<String, Value>,
    param: &str,
) -> Result<(i64, String), String> {
    let card = required(action, args, param, Value::as_object, "an afkd_board.Card")?;
    let (id, key) = (
        card.get("id").and_then(Value::as_str).unwrap_or(""),
        card.get("key").and_then(Value::as_str).unwrap_or(""),
    );
    let (Some((board, _)), Ok(number)) = (parse_key(key), id.parse::<i64>()) else {
        return Err(format!(
            "{action}: {param} is no afkd_board.Card: its id is `{id}` and its key `{key}`"
        ));
    };
    Ok((number, board.to_string()))
}

/// The run `action` was passed as its argument `run`, as afkd sends an `afkd.Run?`, rebuilt as
/// exactly the `{"machine", "service", "instance", "id"}` the board's API takes, any other field
/// a newer afkd sends dropped; `None` for a run that is left out or `nil`. afkd sends every field
/// of a run as a string, the `instance` as its digits, and the board's API takes it as a number,
/// so it is read from either. Anything else is refused.
pub(crate) fn run_arg(action: &str, args: &Map<String, Value>) -> Result<Option<Value>, String> {
    let run = match args.get("run") {
        None | Some(Value::Null) => return Ok(None),
        Some(run) => run,
    };
    let text = |name: &str| run.get(name).and_then(Value::as_str);
    let instance = match run.get("instance") {
        Some(Value::String(digits)) => digits.parse::<i64>().ok(),
        Some(number) => number.as_i64(),
        None => None,
    };
    let (Some(machine), Some(service), Some(instance), Some(id)) =
        (text("machine"), text("service"), instance, text("id"))
    else {
        return Err(format!("{action}: parameter run must be an afkd.Run"));
    };
    Ok(Some(json!({
        "machine": machine,
        "service": service,
        "instance": instance,
        "id": id,
    })))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// A board's every field is read, and its `Debug` names its address but never its key.
    #[test]
    fn a_board_is_read_whole_and_debugs_without_its_key() {
        let arg = json!({"key": "afk_board_SUPERSECRET", "base_url": "https://app.afkd.sh"});
        let board = Board::from_arg("lists", Some(&arg)).unwrap();
        assert_eq!(board.key, "afk_board_SUPERSECRET");
        assert_eq!(board.base_url, "https://app.afkd.sh");
        let debug = format!("{board:?}");
        assert_eq!(debug, r#"Board { base_url: "https://app.afkd.sh", .. }"#);
    }

    /// An empty key, a blank one, a missing board and one short of a field are refused, the
    /// empty key naming the field.
    #[test]
    fn a_board_without_a_key_is_refused() {
        for key in ["", "  "] {
            let arg = json!({"key": key, "base_url": "https://app.afkd.sh"});
            assert_eq!(
                Board::from_arg("cards", Some(&arg)),
                Err(
                    "cards: the afkd_board.Board's key is empty; set key to one of the board's API \
                     keys, made in its Board settings"
                        .to_string()
                )
            );
        }
        assert_eq!(
            Board::from_arg("card", None),
            Err("card: parameter b is required".to_string())
        );
        assert_eq!(
            Board::from_arg("card", Some(&json!({"key": "k"}))),
            Err(
                "card: parameter b must be an afkd_board.Board, with its key and its base_url"
                    .to_string()
            )
        );
    }

    fn args(value: Value) -> Map<String, Value> {
        let Value::Object(map) = value else {
            panic!("{value}")
        };
        map
    }

    /// A run is rebuilt as its four fields alone, so a field a newer afkd sends is dropped, and
    /// the instance, which afkd sends as a string of digits, becomes a number.
    #[test]
    fn a_run_keeps_its_four_fields_and_drops_the_rest() {
        let sent = json!({"machine": "nexus", "service": "develop/review.0", "instance": "2",
                          "id": "261116-163511-card-17-2",
                          "scratch_dir": "/tmp/x/runs/develop/review.0/261116-163511-card-17-2"});
        let kept = json!({"machine": "nexus", "service": "develop/review.0",
                          "instance": 2, "id": "261116-163511-card-17-2"});
        assert_eq!(
            run_arg("create_card", &args(json!({ "run": sent }))),
            Ok(Some(kept.clone()))
        );
        let numeric = json!({"machine": "nexus", "service": "develop/review.0", "instance": 2,
                             "id": "261116-163511-card-17-2", "cost": 1.5});
        assert_eq!(
            run_arg("create_card", &args(json!({ "run": numeric }))),
            Ok(Some(kept))
        );
    }

    /// A run left out or `nil` is none, and anything else that is no run is refused naming the
    /// parameter.
    #[test]
    fn a_missing_run_is_none_and_a_malformed_one_is_refused() {
        assert_eq!(run_arg("add_item", &args(json!({}))), Ok(None));
        assert_eq!(run_arg("add_item", &args(json!({"run": null}))), Ok(None));
        for run in [
            json!("261116-163511"),
            json!([1]),
            json!({}),
            json!({"machine": "m", "service": "s", "instance": 0}),
            json!({"machine": "m", "service": "s", "instance": "zero", "id": "i"}),
            json!({"machine": "m", "service": "s", "instance": 0.5, "id": "i"}),
            json!({"machine": 1, "service": "s", "instance": 0, "id": "i"}),
        ] {
            assert_eq!(
                run_arg("add_item", &args(json!({ "run": run }))),
                Err("add_item: parameter run must be an afkd.Run".to_string()),
                "{run}"
            );
        }
    }

    /// A card handle gives its number and its board's uuid, and the sentence refusing one
    /// names the parameter it was passed as.
    #[test]
    fn a_card_handle_gives_its_number_and_board() {
        let card = json!({"id": "17", "key": "afkd_board:b-1/c-17"});
        assert_eq!(
            card_arg("move_card", &args(json!({ "card": card })), "card"),
            Ok((17, "b-1".to_string()))
        );
        assert_eq!(
            card_arg("add_blocked_by", &args(json!({ "other": card })), "other"),
            Ok((17, "b-1".to_string()))
        );
        for bad in [
            json!({"id": "ENG-17", "key": "afkd_board:b-1/c-17"}),
            json!({"id": "17", "key": "linear:b-1/c-17"}),
            json!({"id": "17"}),
        ] {
            let e = card_arg("add_blocked_by", &args(json!({ "other": bad })), "other");
            assert!(
                e.as_ref()
                    .is_err_and(|e| e.starts_with("add_blocked_by: other is no afkd_board.Card: ")),
                "{e:?}"
            );
        }
        assert_eq!(
            card_arg("move_card", &args(json!({})), "card"),
            Err("move_card: parameter card is required".to_string())
        );
        assert_eq!(
            card_arg("move_card", &args(json!({"card": 17})), "card"),
            Err("move_card: parameter card must be an afkd_board.Card".to_string())
        );
    }

    /// A key splits into its board and its object, and one of another shape is none.
    #[test]
    fn a_handle_key_parses_back_into_its_two_uuids() {
        let key = handle_key("b-1", "c-2");
        assert_eq!(key, "afkd_board:b-1/c-2");
        assert_eq!(parse_key(&key), Some(("b-1", "c-2")));
        for other in [
            "linear:b-1/c-2",
            "board:b-1/c-2",
            "afkd_board:b-1",
            "afkd_board:/c-2",
            "afkd_board:b-1/",
            "afkd_board:b-1/c-2/d",
            "b-1/c-2",
        ] {
            assert_eq!(parse_key(other), None, "{other}");
        }
    }
}
