//! The **handles** the plugin hands afkd: a card, a comment, a checklist, a check item, an
//! attachment, a board, a list, a label and a member. afkd carries `id` and `key` for every
//! handle; the plugin fills them, and declares only the other fields ([`WireHandle`]).
//!
//! A handle's key is `trello:<Trello's id>`, so the same object read twice, or claimed and
//! read, is `==`. The two exceptions are an attachment's, `trello:<card id>/<attachment id>`,
//! and a check item's, `trello:<card id>/<item id>`: Trello has no route to either outside
//! its card, and a handle crosses the wire as its `id` and `key` alone, so
//! `download_attachment` and `update_check_item` could reach theirs by nothing else.
//!
//! Each builder reads Trello's JSON, so a card the claim read, a card a board read and a
//! comment an action listing gave all arrive in one shape. A time is unix milliseconds,
//! as `@afkd/linear` sends a `time`; a field that may hold nothing is `null`, and one that
//! is a list is `[]`, so a field is never left out: afkd refuses a handle missing one.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::Value;

use crate::client::{object_id_timestamp, parse_labels_field};
use crate::rfc3339::parse_rfc3339_millis;
use crate::wire::WireHandle;

/// What every key a read hands out starts with.
const KEY_PREFIX: &str = "trello:";

/// The key of the object Trello knows as `id`.
pub(crate) fn read_key(id: &str) -> String {
    format!("{KEY_PREFIX}{id}")
}

/// The key of attachment `attachment_id` of card `card_id`.
pub(crate) fn attachment_key(card_id: &str, attachment_id: &str) -> String {
    format!("{KEY_PREFIX}{card_id}/{attachment_id}")
}

/// The key of check item `item_id` of card `card_id`, which has the shape of an
/// attachment's for the same reason: Trello has no route to an item outside its card.
pub(crate) fn check_item_key(card_id: &str, item_id: &str) -> String {
    format!("{KEY_PREFIX}{card_id}/{item_id}")
}

/// The Trello id a [`read_key`] holds, or `None` for a key of any other shape: one an older
/// release journaled (`card#claim`), a two-id key's, or none of this plugin's.
pub(crate) fn parse_read_key(key: &str) -> Option<&str> {
    let id = key.strip_prefix(KEY_PREFIX)?;
    (!id.is_empty() && !id.contains(['#', '/'])).then_some(id)
}

/// The two ids a key of the shape `trello:<card id>/<id>` holds.
fn parse_two_ids(key: &str) -> Option<(&str, &str)> {
    let (card, id) = key.strip_prefix(KEY_PREFIX)?.split_once('/')?;
    let plain = |id: &str| !id.is_empty() && !id.contains(['#', '/']);
    (plain(card) && plain(id)).then_some((card, id))
}

/// The card id and the attachment id an [`attachment_key`] holds.
pub(crate) fn parse_attachment_key(key: &str) -> Option<(&str, &str)> {
    parse_two_ids(key)
}

/// The card id and the item id a [`check_item_key`] holds.
pub(crate) fn parse_check_item_key(key: &str) -> Option<(&str, &str)> {
    parse_two_ids(key)
}

/// The fields of a `trello.Card` handle, in the manifest's order.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct CardFields {
    /// The card's name, as today's handle has it until the actions are removed.
    pub(crate) title: String,
    /// Trello's `idShort`, the number in the card's address.
    pub(crate) number: i64,
    pub(crate) name: String,
    pub(crate) url: String,
    /// Markdown; `""` when the card has none.
    pub(crate) description: String,
    /// The board's short link.
    pub(crate) board: String,
    /// The name of the list the card is in.
    pub(crate) list: String,
    /// The label names; a colour-only label is left out.
    pub(crate) labels: Vec<String>,
    /// The usernames of the members on the card.
    pub(crate) members: Vec<String>,
    pub(crate) due: Option<u64>,
    pub(crate) due_complete: bool,
    /// Trello's `closed`.
    pub(crate) archived: bool,
    pub(crate) created_at: u64,
    pub(crate) last_activity_at: u64,
}

/// The fields of a `trello.Comment` handle.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct CommentFields {
    /// The short link of the card it is on.
    pub(crate) card: String,
    pub(crate) body: String,
    /// The commenter's username.
    pub(crate) author: Option<String>,
    pub(crate) created_at: u64,
    pub(crate) edited_at: Option<u64>,
}

/// The fields of a `trello.Checklist` handle.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct ChecklistFields {
    pub(crate) card: String,
    pub(crate) name: String,
    /// The names of its items, in order.
    pub(crate) items: Vec<String>,
}

/// The fields of a `trello.Check_Item` handle.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct CheckItemFields {
    pub(crate) card: String,
    /// The id of its checklist.
    pub(crate) checklist: String,
    pub(crate) name: String,
    pub(crate) done: bool,
}

/// The fields of a `trello.Attachment` handle.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct AttachmentFields {
    pub(crate) card: String,
    pub(crate) name: String,
    pub(crate) url: String,
    pub(crate) bytes: Option<u64>,
    pub(crate) mime_type: Option<String>,
    pub(crate) created_at: u64,
    /// A file Trello holds, not a link.
    pub(crate) is_upload: bool,
}

/// The fields of a `trello.Board` handle.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct BoardFields {
    pub(crate) name: String,
    pub(crate) url: String,
    pub(crate) archived: bool,
}

/// The fields of a `trello.List` handle.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct ListFields {
    pub(crate) name: String,
    /// The board's short link.
    pub(crate) board: String,
    pub(crate) archived: bool,
}

/// The fields of a `trello.Label` handle.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct LabelFields {
    pub(crate) name: String,
    pub(crate) color: Option<String>,
    /// The board's short link.
    pub(crate) board: String,
}

/// The fields of a `trello.Member` handle.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct MemberFields {
    pub(crate) full_name: String,
}

/// What a card's fields name that the card itself carries only the id of: its list, its
/// members and its board.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Lookups {
    /// List id → name.
    pub(crate) lists: HashMap<String, String>,
    /// Member id → username.
    pub(crate) members: HashMap<String, String>,
    /// The board's short link.
    pub(crate) board: String,
}

impl Lookups {
    /// The lookups a card read with its list, board and members nested holds
    /// (`GET /cards/{id}?list=true&board=true&members=true`).
    pub(crate) fn nested(card: &Value) -> Self {
        let named = |nest: &Value, key: &str| -> Option<(String, String)> {
            Some((
                nest.get("id")?.as_str()?.to_string(),
                nest.get(key)?.as_str()?.to_string(),
            ))
        };
        Self {
            lists: card
                .get("list")
                .and_then(|list| named(list, "name"))
                .into_iter()
                .collect(),
            members: card
                .get("members")
                .and_then(Value::as_array)
                .map(|members| {
                    members
                        .iter()
                        .filter_map(|m| named(m, "username"))
                        .collect()
                })
                .unwrap_or_default(),
            board: str_of(card.get("board").unwrap_or(&Value::Null), "shortLink").to_string(),
        }
    }
}

/// `value`'s string field `key`, `""` when it has none.
pub(crate) fn str_of<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// A non-empty string field.
fn some_str(value: &Value, key: &str) -> Option<String> {
    Some(str_of(value, key))
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// A time as unix milliseconds; before the epoch, nothing Trello holds, it is 0.
pub(crate) fn millis(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// The time the string field `key` of `value` holds, in unix milliseconds.
fn time_of(value: &Value, key: &str) -> Option<u64> {
    parse_rfc3339_millis(value.get(key)?.as_str()?).map(millis)
}

/// When the object Trello knows as `id` was made: the second its ObjectId begins with.
fn created_of(id: &str) -> u64 {
    object_id_timestamp(id).map_or(0, millis)
}

/// The card `card` as a handle: its `id` the short link, its key `trello:<card id>`. `None`
/// for JSON that holds no card id. A card whose id is no ObjectId is made at 0, and one that
/// names no last activity was last active when it was made.
pub(crate) fn build_card(card: &Value, lookups: &Lookups) -> Option<WireHandle<CardFields>> {
    let id = card.get("id")?.as_str()?;
    let created_at = created_of(id);
    let name = str_of(card, "name").to_string();
    let named = |ids: &Value, names: &HashMap<String, String>| -> Vec<String> {
        ids.as_array()
            .map(|ids| {
                ids.iter()
                    .filter_map(Value::as_str)
                    .filter_map(|id| names.get(id).cloned())
                    .collect()
            })
            .unwrap_or_default()
    };
    Some(WireHandle {
        id: some_str(card, "shortLink").unwrap_or_else(|| id.to_string()),
        key: read_key(id),
        fields: CardFields {
            title: name.clone(),
            number: card.get("idShort").and_then(Value::as_i64).unwrap_or(0),
            name,
            url: str_of(card, "url").to_string(),
            description: str_of(card, "desc").to_string(),
            board: lookups.board.clone(),
            list: lookups
                .lists
                .get(str_of(card, "idList"))
                .cloned()
                .unwrap_or_default(),
            labels: parse_labels_field(card),
            members: named(
                card.get("idMembers").unwrap_or(&Value::Null),
                &lookups.members,
            ),
            due: time_of(card, "due"),
            due_complete: card
                .get("dueComplete")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            archived: card.get("closed").and_then(Value::as_bool).unwrap_or(false),
            created_at,
            last_activity_at: time_of(card, "dateLastActivity").unwrap_or(created_at),
        },
    })
}

/// The comment action `action`, on the card whose short link is `card`, as a handle: its
/// `id` and key those of the action. `None` for an action with no id or text.
pub(crate) fn build_comment(action: &Value, card: &str) -> Option<WireHandle<CommentFields>> {
    let id = action.get("id")?.as_str()?;
    let data = action.get("data")?;
    let body = data.get("text")?.as_str()?;
    Some(WireHandle {
        id: id.to_string(),
        key: read_key(id),
        fields: CommentFields {
            card: card.to_string(),
            body: body.to_string(),
            author: action
                .get("memberCreator")
                .and_then(|creator| some_str(creator, "username")),
            created_at: time_of(action, "date").unwrap_or_else(|| created_of(id)),
            edited_at: time_of(data, "dateLastEdited"),
        },
    })
}

/// The items of the checklist `checklist` in order: by Trello's `pos`, an item with none
/// last, and the order Trello listed them in among equals.
fn ordered_items(checklist: &Value) -> Vec<&Value> {
    let mut items: Vec<&Value> = checklist
        .get("checkItems")
        .and_then(Value::as_array)
        .map(|items| items.iter().collect())
        .unwrap_or_default();
    items.sort_by(|a, b| position(a).total_cmp(&position(b)));
    items
}

/// Trello's `pos` of `value`, past every other when it has none.
pub(crate) fn position(value: &Value) -> f64 {
    value.get("pos").and_then(Value::as_f64).unwrap_or(f64::MAX)
}

/// The checklist `checklist` of the card whose short link is `card`, as a handle: its items
/// the names of the check items in order. `None` for JSON with no id.
pub(crate) fn build_checklist(
    checklist: &Value,
    card: &str,
) -> Option<WireHandle<ChecklistFields>> {
    let id = checklist.get("id")?.as_str()?;
    Some(WireHandle {
        id: id.to_string(),
        key: read_key(id),
        fields: ChecklistFields {
            card: card.to_string(),
            name: str_of(checklist, "name").to_string(),
            items: ordered_items(checklist)
                .into_iter()
                .map(|item| str_of(item, "name").to_string())
                .collect(),
        },
    })
}

/// The check items of the checklist `checklist` of the card `card_id`, whose short link is
/// `card`, in order, each as a handle: its key carries the card's id too ([`check_item_key`]).
pub(crate) fn build_check_items(
    checklist: &Value,
    card_id: &str,
    card: &str,
) -> Vec<WireHandle<CheckItemFields>> {
    let list_id = str_of(checklist, "id");
    ordered_items(checklist)
        .into_iter()
        .filter_map(|item| {
            let id = item.get("id")?.as_str()?;
            Some(WireHandle {
                id: id.to_string(),
                key: check_item_key(card_id, id),
                fields: CheckItemFields {
                    card: card.to_string(),
                    checklist: list_id.to_string(),
                    name: str_of(item, "name").to_string(),
                    done: str_of(item, "state") == "complete",
                },
            })
        })
        .collect()
}

/// The attachment `attachment` of the card `card_id`, whose short link is `card`, as a
/// handle: its key carries the card's id too ([`attachment_key`]). `None` for JSON with no id.
pub(crate) fn build_attachment(
    attachment: &Value,
    card_id: &str,
    card: &str,
) -> Option<WireHandle<AttachmentFields>> {
    let id = attachment.get("id")?.as_str()?;
    Some(WireHandle {
        id: id.to_string(),
        key: attachment_key(card_id, id),
        fields: AttachmentFields {
            card: card.to_string(),
            name: str_of(attachment, "name").to_string(),
            url: str_of(attachment, "url").to_string(),
            bytes: attachment.get("bytes").and_then(Value::as_u64),
            mime_type: some_str(attachment, "mimeType"),
            created_at: time_of(attachment, "date").unwrap_or_else(|| created_of(id)),
            is_upload: attachment
                .get("isUpload")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        },
    })
}

/// The board `board` as a handle: its `id` the short link, its key `trello:<board id>`.
/// `None` for JSON with no id or short link.
pub(crate) fn build_board(board: &Value) -> Option<WireHandle<BoardFields>> {
    let id = board.get("id")?.as_str()?;
    Some(WireHandle {
        id: some_str(board, "shortLink")?,
        key: read_key(id),
        fields: BoardFields {
            name: str_of(board, "name").to_string(),
            url: str_of(board, "url").to_string(),
            archived: board
                .get("closed")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        },
    })
}

/// The list `list` of the board whose short link is `board`, as a handle.
pub(crate) fn build_list(list: &Value, board: &str) -> Option<WireHandle<ListFields>> {
    let id = list.get("id")?.as_str()?;
    Some(WireHandle {
        id: id.to_string(),
        key: read_key(id),
        fields: ListFields {
            name: str_of(list, "name").to_string(),
            board: board.to_string(),
            archived: list.get("closed").and_then(Value::as_bool).unwrap_or(false),
        },
    })
}

/// The label `label` of the board whose short link is `board`, as a handle. A label with no
/// name has `""`, and one with no colour `null`.
pub(crate) fn build_label(label: &Value, board: &str) -> Option<WireHandle<LabelFields>> {
    let id = label.get("id")?.as_str()?;
    Some(WireHandle {
        id: id.to_string(),
        key: read_key(id),
        fields: LabelFields {
            name: str_of(label, "name").to_string(),
            color: some_str(label, "color"),
            board: board.to_string(),
        },
    })
}

/// The member `member` as a handle: its `id` the username, its key `trello:<member id>`.
/// `None` for JSON with no id or username.
pub(crate) fn build_member(member: &Value) -> Option<WireHandle<MemberFields>> {
    let id = member.get("id")?.as_str()?;
    Some(WireHandle {
        id: some_str(member, "username")?,
        key: read_key(id),
        fields: MemberFields {
            full_name: str_of(member, "fullName").to_string(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A card as Trello's default fields give it, with everything a card can carry: a wide
    /// name, an emoji, two named labels and a colour-only one, two members, a due date and
    /// a last activity to the millisecond.
    fn rich_card() -> Value {
        json!({
            "id": "6a4dd5de1234abcd5678ef90",
            "shortLink": "Qb7eLy2w",
            "idShort": 12,
            "name": "修复 the retry storm 🚨",
            "desc": "## 背景\n\nIt retries forever.",
            "url": "https://trello.com/c/Qb7eLy2w/12-retry-storm",
            "idList": "list-2",
            "idMembers": ["m1", "gone", "m2"],
            "labels": [{"id": "l1", "name": "afkd/ready", "color": "green"},
                       {"id": "l2", "name": "", "color": "red"},
                       {"id": "l3", "name": "Väntar på svar", "color": null}],
            "closed": true,
            "due": "2026-12-24T10:00:00.250Z",
            "dueComplete": true,
            "dateLastActivity": "2026-11-02T08:15:30.500Z",
        })
    }

    fn lookups() -> Lookups {
        Lookups {
            lists: HashMap::from([("list-2".into(), "Doing".into())]),
            members: HashMap::from([("m1".into(), "phil".into()), ("m2".into(), "陳大文".into())]),
            board: "1Rkelydw".into(),
        }
    }

    /// Keys are `trello:<id>`, an attachment's carries its card's, and a key of any other
    /// shape reads as none.
    #[test]
    fn keys_round_trip_and_nothing_else_parses() {
        assert_eq!(
            read_key("6a4dd5de1234abcd5678ef90"),
            "trello:6a4dd5de1234abcd5678ef90"
        );
        assert_eq!(parse_read_key("trello:abc"), Some("abc"));
        assert_eq!(attachment_key("card1", "att1"), "trello:card1/att1");
        assert_eq!(
            parse_attachment_key("trello:card1/att1"),
            Some(("card1", "att1"))
        );
        assert_eq!(check_item_key("card1", "item1"), "trello:card1/item1");
        assert_eq!(
            parse_check_item_key("trello:card1/item1"),
            Some(("card1", "item1"))
        );
        for key in [
            "",
            "trello:",
            "card1#claim1",
            "card1#claim1#list",
            "trello:card1/att1",
            "trello:a#b",
            "linear:org/issue",
        ] {
            assert_eq!(parse_read_key(key), None, "{key}");
        }
        for key in [
            "trello:card1",
            "trello:/att1",
            "trello:card1/",
            "trello:a/b/c",
            "x",
        ] {
            assert_eq!(parse_attachment_key(key), None, "{key}");
            assert_eq!(parse_check_item_key(key), None, "{key}");
        }
    }

    /// A rich card carries every field: the short link as the id, the list, the members
    /// as usernames (an id the board does not know is left out), the named labels, the
    /// times in milliseconds, `closed` as `archived`, and the creation time from its id.
    #[test]
    fn a_rich_card_fills_every_field() {
        let handle = build_card(&rich_card(), &lookups()).expect("a card");
        assert_eq!(handle.id, "Qb7eLy2w");
        assert_eq!(handle.key, "trello:6a4dd5de1234abcd5678ef90");
        assert_eq!(
            handle.fields,
            CardFields {
                title: "修复 the retry storm 🚨".into(),
                number: 12,
                name: "修复 the retry storm 🚨".into(),
                url: "https://trello.com/c/Qb7eLy2w/12-retry-storm".into(),
                description: "## 背景\n\nIt retries forever.".into(),
                board: "1Rkelydw".into(),
                list: "Doing".into(),
                labels: vec!["afkd/ready".into(), "Väntar på svar".into()],
                members: vec!["phil".into(), "陳大文".into()],
                due: Some(1_798_106_400_250),
                due_complete: true,
                archived: true,
                created_at: 0x6a4d_d5de * 1000,
                last_activity_at: 1_793_607_330_500,
            }
        );
    }

    /// A bare card — an id and nothing else — still carries every field: empty text and
    /// lists, `false`, no due date, and last active when it was made. Its short link is
    /// its id.
    #[test]
    fn a_bare_card_fills_every_field_empty() {
        let bare = json!({"id": "6a4dd5de1234abcd5678ef90"});
        let handle = build_card(&bare, &Lookups::default()).expect("a card");
        assert_eq!(handle.id, "6a4dd5de1234abcd5678ef90");
        let created = 0x6a4d_d5de * 1000;
        assert_eq!(
            handle.fields,
            CardFields {
                title: String::new(),
                number: 0,
                name: String::new(),
                url: String::new(),
                description: String::new(),
                board: String::new(),
                list: String::new(),
                labels: Vec::new(),
                members: Vec::new(),
                due: None,
                due_complete: false,
                archived: false,
                created_at: created,
                last_activity_at: created,
            }
        );
        assert!(build_card(&json!({"name": "x"}), &Lookups::default()).is_none());
    }

    /// The lookups a nested card read carries: its list and board and its members.
    #[test]
    fn nested_lookups_read_the_cards_own_list_board_and_members() {
        let mut card = rich_card();
        card["list"] = json!({"id": "list-2", "name": "Doing"});
        card["board"] = json!({"id": "6a00000000000000000000b1", "shortLink": "1Rkelydw"});
        card["members"] =
            json!([{"id": "m1", "username": "phil"}, {"id": "m2", "username": "陳大文"}]);
        let nested = Lookups::nested(&card);
        assert_eq!(
            nested,
            Lookups {
                board: "1Rkelydw".into(),
                ..lookups()
            }
        );
        assert_eq!(
            build_card(&card, &nested),
            build_card(&rich_card(), &lookups())
        );
        assert_eq!(Lookups::nested(&json!({})), Lookups::default());
    }

    /// A comment action: the card's short link, the author's username (none when Trello
    /// names none), the times to the millisecond, and an edit stamp only once edited.
    #[test]
    fn a_comment_carries_its_author_and_times() {
        let action = json!({
            "id": "6a4dd5de1234abcd5678ef91",
            "type": "commentCard",
            "idMemberCreator": "m1",
            "date": "2026-11-02T08:15:30.125Z",
            "data": {"text": "看起来不对 🚨", "dateLastEdited": "2026-11-02T09:00:00.000Z"},
            "memberCreator": {"id": "m1", "username": "phil", "fullName": "Phil Ek"},
        });
        let handle = build_comment(&action, "Qb7eLy2w").expect("a comment");
        assert_eq!(handle.id, "6a4dd5de1234abcd5678ef91");
        assert_eq!(handle.key, "trello:6a4dd5de1234abcd5678ef91");
        assert_eq!(
            handle.fields,
            CommentFields {
                card: "Qb7eLy2w".into(),
                body: "看起来不对 🚨".into(),
                author: Some("phil".into()),
                created_at: 1_793_607_330_125,
                edited_at: Some(1_793_610_000_000),
            }
        );
        let bare = json!({"id": "6a4dd5de1234abcd5678ef92", "data": {"text": ""}});
        let handle = build_comment(&bare, "Qb7eLy2w").expect("a comment");
        assert_eq!(handle.fields.author, None);
        assert_eq!(handle.fields.edited_at, None);
        assert_eq!(
            handle.fields.created_at,
            0x6a4d_d5de * 1000,
            "dated by its id"
        );
        assert!(build_comment(&json!({"id": "x", "data": {}}), "c").is_none());
    }

    /// A checklist's items, and the check items, come in `pos` order whatever order Trello
    /// listed them in, an item with no `pos` last; `done` is Trello's `complete` state.
    #[test]
    fn checklists_and_their_items_are_in_pos_order() {
        let checklist = json!({
            "id": "cl1", "name": "Acceptance", "pos": 1,
            "checkItems": [
                {"id": "i2", "name": "second", "state": "incomplete", "pos": 2},
                {"id": "i3", "name": "unplaced", "state": "complete"},
                {"id": "i1", "name": "first", "state": "complete", "pos": 1},
            ],
        });
        let list = build_checklist(&checklist, "Qb7eLy2w").expect("a checklist");
        assert_eq!((list.id.as_str(), list.key.as_str()), ("cl1", "trello:cl1"));
        assert_eq!(
            list.fields,
            ChecklistFields {
                card: "Qb7eLy2w".into(),
                name: "Acceptance".into(),
                items: vec!["first".into(), "second".into(), "unplaced".into()],
            }
        );
        let items = build_check_items(&checklist, "card1", "Qb7eLy2w");
        let seen: Vec<(&str, &str, bool)> = items
            .iter()
            .map(|i| (i.id.as_str(), i.fields.name.as_str(), i.fields.done))
            .collect();
        assert_eq!(
            seen,
            [
                ("i1", "first", true),
                ("i2", "second", false),
                ("i3", "unplaced", true)
            ]
        );
        assert!(items
            .iter()
            .all(|i| i.fields.checklist == "cl1" && i.fields.card == "Qb7eLy2w"));
        assert_eq!(items[0].key, "trello:card1/i1");

        let empty = json!({"id": "cl2"});
        assert_eq!(
            build_checklist(&empty, "c")
                .expect("a checklist")
                .fields
                .items,
            Vec::<String>::new()
        );
        assert!(build_check_items(&empty, "c1", "c").is_empty());
    }

    /// An uploaded file and a link: the key carries the card id, a link has no size and
    /// no type (`""` is none), and the time is the listing's `date`.
    #[test]
    fn an_attachment_keys_by_its_card_and_reads_file_or_link() {
        let file = json!({"id": "a1", "name": "plan 計画.md", "url": "https://trello.com/1/cards/c/attachments/a1/download/plan.md",
                          "bytes": 1234, "mimeType": "text/markdown", "isUpload": true,
                          "date": "2026-11-02T08:15:30.007Z"});
        let handle =
            build_attachment(&file, "6a4dd5de1234abcd5678ef90", "Qb7eLy2w").expect("an attachment");
        assert_eq!(handle.id, "a1");
        assert_eq!(handle.key, "trello:6a4dd5de1234abcd5678ef90/a1");
        assert_eq!(
            handle.fields,
            AttachmentFields {
                card: "Qb7eLy2w".into(),
                name: "plan 計画.md".into(),
                url: "https://trello.com/1/cards/c/attachments/a1/download/plan.md".into(),
                bytes: Some(1234),
                mime_type: Some("text/markdown".into()),
                created_at: 1_793_607_330_007,
                is_upload: true,
            }
        );
        let link = json!({"id": "6a4dd5de1234abcd5678ef93", "name": "spec", "url": "https://example.com/spec",
                          "bytes": null, "mimeType": "", "isUpload": false});
        let handle = build_attachment(&link, "c1", "Qb7eLy2w").expect("an attachment");
        assert_eq!(handle.fields.bytes, None);
        assert_eq!(handle.fields.mime_type, None);
        assert!(!handle.fields.is_upload);
        assert_eq!(
            handle.fields.created_at,
            0x6a4d_d5de * 1000,
            "dated by its id"
        );
    }

    /// A board, list, label and member: the id and key each carries, `closed` as
    /// `archived`, a label with no name or colour, and a member by username.
    #[test]
    fn a_board_a_list_a_label_and_a_member_fill_their_fields() {
        let board = json!({"id": "6a00000000000000000000b1", "shortLink": "1Rkelydw",
                           "name": "afkd 板", "url": "https://trello.com/b/1Rkelydw/afkd", "closed": false});
        let handle = build_board(&board).expect("a board");
        assert_eq!(
            (handle.id.as_str(), handle.key.as_str()),
            ("1Rkelydw", "trello:6a00000000000000000000b1")
        );
        assert_eq!(
            handle.fields,
            BoardFields {
                name: "afkd 板".into(),
                url: "https://trello.com/b/1Rkelydw/afkd".into(),
                archived: false
            }
        );
        assert!(build_board(&json!({"id": "x"})).is_none(), "no short link");

        let list = build_list(
            &json!({"id": "l1", "name": "Doing", "closed": true}),
            "1Rkelydw",
        )
        .expect("a list");
        assert_eq!((list.id.as_str(), list.key.as_str()), ("l1", "trello:l1"));
        assert_eq!(
            list.fields,
            ListFields {
                name: "Doing".into(),
                board: "1Rkelydw".into(),
                archived: true
            }
        );

        let label = build_label(&json!({"id": "lb1", "name": "", "color": null}), "1Rkelydw")
            .expect("a label");
        assert_eq!(
            label.fields,
            LabelFields {
                name: String::new(),
                color: None,
                board: "1Rkelydw".into()
            }
        );
        let label = build_label(
            &json!({"id": "lb2", "name": "bug", "color": "red"}),
            "1Rkelydw",
        )
        .expect("a label");
        assert_eq!(label.fields.color.as_deref(), Some("red"));

        let member = build_member(&json!({"id": "m1", "username": "phil", "fullName": "Phil Ek"}))
            .expect("a member");
        assert_eq!(
            (member.id.as_str(), member.key.as_str()),
            ("phil", "trello:m1")
        );
        assert_eq!(
            member.fields,
            MemberFields {
                full_name: "Phil Ek".into()
            }
        );
        assert!(build_member(&json!({"id": "m1"})).is_none(), "no username");
    }
}
