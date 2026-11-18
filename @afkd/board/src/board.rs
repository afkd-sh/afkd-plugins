//! A `board.Board`, the struct a config builds and passes every read: one of a board's API
//! keys and the address the board is served at. Beside it, the keys of the handles a read
//! hands back, `board:<board uuid>/<object uuid>`, which afkd's `==` compares, and the check
//! that a handle a read is passed is of the board it reaches.

use std::fmt;

use serde_json::Value;

/// What a `board.Board` holds. Its `Debug` leaves the key out, so nothing that prints one
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
                "{action}: parameter b must be a board.Board, with its key and its base_url"
            ));
        };
        if key.trim().is_empty() {
            return Err(format!(
                "{action}: the board.Board's key is empty; set key to one of the board's API \
                 keys, made in its Board settings"
            ));
        }
        Ok(Self { key, base_url })
    }
}

/// The key of the object `object` of the board `board`, both uuids.
pub(crate) fn handle_key(board: &str, object: &str) -> String {
    format!("board:{board}/{object}")
}

/// The board's uuid and the object's in a handle's `key`, `board:<board>/<object>`, or
/// `None` for a key of any other shape.
pub(crate) fn parse_key(key: &str) -> Option<(&str, &str)> {
    let (board, object) = key.strip_prefix("board:")?.split_once('/')?;
    (!board.is_empty() && !object.is_empty() && !object.contains('/')).then_some((board, object))
}

/// The sentence refusing the card `number` of the board `theirs`, passed `action` with a
/// `board.Board` that reaches the board `ours`.
pub(crate) fn other_board(action: &str, number: &str, theirs: &str, ours: &str) -> String {
    format!(
        "{action}: card is #{number} of board {theirs}, and the board.Board reaches board {ours}"
    )
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
                    "cards: the board.Board's key is empty; set key to one of the board's API \
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
                "card: parameter b must be a board.Board, with its key and its base_url"
                    .to_string()
            )
        );
    }

    /// A key splits into its board and its object, and one of another shape is none.
    #[test]
    fn a_handle_key_parses_back_into_its_two_uuids() {
        let key = handle_key("b-1", "c-2");
        assert_eq!(key, "board:b-1/c-2");
        assert_eq!(parse_key(&key), Some(("b-1", "c-2")));
        for other in [
            "linear:b-1/c-2",
            "board:b-1",
            "board:/c-2",
            "board:b-1/",
            "board:b-1/c-2/d",
            "b-1/c-2",
        ] {
            assert_eq!(parse_key(other), None, "{other}");
        }
    }
}
