//! The **account writes** a config calls with a `trello.Account` first: a card made, changed,
//! archived, unarchived and deleted, a label or a member put on a card and taken off it, a
//! comment made, changed and deleted, a checklist and its items, and an attachment made and
//! deleted.
//!
//! A write reaches Trello only through the account it is passed ([`Account`]), as a read does
//! ([`crate::reads`]): it reads no service's settings, and the kind's own client is never used
//! for it. Each write
//!
//! 1. decodes its arguments, a refusal sent before any request ([`decode`]);
//! 2. resolves the names it was given — a list, a label, a member — against the board, so one
//!    the board lacks is refused before anything is sent;
//! 3. sends its request or requests through the account's retrying client;
//! 4. reads what it made or changed back, the way the reads read it, so the handle it answers
//!    with is `==` to the one `trello.cards`, `comments`, `checklists`, `check_items` or
//!    `attachments` hands back; a delete answers `{"ok":true}` and no handle.
//!
//! A write is never queued: it is done now, or refused with the reason. A request Trello
//! answered "too many requests" is tried again within the call's deadline, and a 5xx is tried
//! again on every verb but `POST`, which Trello may already have carried out, so a card is
//! never made twice. When Trello refuses a write it says why, and the refusal keeps its words:
//! another member's comment is not the token's to edit.

use std::time::{Duration, Instant, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{json, Map, Value};

use crate::account::{Account, Clients};
use crate::attachment::read_upload;
use crate::board::BoardError;
use crate::card::ACTION_TAG;
use crate::client::{pos_value, ListPosition, TrelloClient};
use crate::common::Diag;
use crate::handles::{
    build_attachment, build_check_items, build_checklist, build_comment, parse_attachment_key,
    parse_check_item_key, parse_read_key, str_of, AttachmentFields, CardFields, CheckItemFields,
    ChecklistFields, CommentFields,
};
use crate::reads::{
    array, board_arg, board_labels, board_members, card_arg, card_id, fit_call, handle_arg,
    one_named, optional, read_board, string, HandleArg, ATTACHMENT_FIELDS,
};
use crate::rfc3339::format_utc_millis;
use crate::settings::ME;
use crate::wire::WireHandle;

/// The writes, in the order the manifest declares them, after the reads
/// ([`READS`](crate::reads::READS)).
pub(crate) const WRITES: &[&str] = &[
    "create_card",
    "update_card",
    "archive_card",
    "unarchive_card",
    "delete_card",
    "create_comment",
    "update_comment",
    "delete_comment",
    "create_checklist",
    "delete_checklist",
    "create_check_item",
    "update_check_item",
    "delete_check_item",
    "create_attachment",
    "delete_attachment",
    "add_label",
    "remove_label",
    "add_member",
    "remove_member",
];

/// The most characters Trello takes in a card's description or a comment.
const MAX_TEXT: usize = 16_384;

/// The latest `due`, in unix milliseconds, that is a date of four digits: 9999-12-31.
const MAX_DUE_MS: u64 = 253_402_300_799_999;

/// A `T?` parameter of a write: left out, it leaves the field as it is; `nil`, it clears it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) enum Change<T> {
    #[default]
    Keep,
    Clear,
    Set(T),
}

/// The card `create_card` files, its names still to be resolved against the board.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NewCard {
    /// The board, by address or short link, reduced to the id its address carries.
    pub(crate) board: String,
    pub(crate) list: String,
    pub(crate) name: String,
    pub(crate) description: Option<String>,
    pub(crate) at: ListPosition,
    pub(crate) labels: Vec<String>,
    pub(crate) members: Vec<String>,
    /// Unix milliseconds.
    pub(crate) due: Option<u64>,
}

/// What `update_card` changes on a card: each field left out stays as it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CardEdit {
    pub(crate) card: HandleArg,
    pub(crate) name: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) list: Option<String>,
    pub(crate) at: Option<ListPosition>,
    pub(crate) due: Change<u64>,
    pub(crate) due_complete: Option<bool>,
}

/// One write, decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WriteCall {
    CreateCard(Box<NewCard>),
    UpdateCard(Box<CardEdit>),
    ArchiveCard(HandleArg),
    UnarchiveCard(HandleArg),
    DeleteCard(HandleArg),
    CreateComment {
        card: HandleArg,
        body: String,
    },
    UpdateComment {
        comment: HandleArg,
        body: String,
    },
    DeleteComment(HandleArg),
    CreateChecklist {
        card: HandleArg,
        name: String,
        items: Vec<String>,
    },
    DeleteChecklist(HandleArg),
    CreateCheckItem {
        checklist: HandleArg,
        name: String,
        at: ListPosition,
    },
    UpdateCheckItem {
        item: HandleArg,
        name: Option<String>,
        done: Option<bool>,
    },
    DeleteCheckItem(HandleArg),
    CreateAttachment {
        card: HandleArg,
        path: String,
        name: Option<String>,
    },
    DeleteAttachment(HandleArg),
    /// Put the label `label` of the card's board on the card.
    AddLabel {
        card: HandleArg,
        label: String,
    },
    /// Take the label `label` off the card.
    RemoveLabel {
        card: HandleArg,
        label: String,
    },
    /// Put the board member `member`, a username or `me`, on the card.
    AddMember {
        card: HandleArg,
        member: String,
    },
    /// Take the board member `member` off the card.
    RemoveMember {
        card: HandleArg,
        member: String,
    },
}

/// What a label or member write acts on a card with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tag {
    Label,
    Member,
}

/// Do the write `action` with `args`, and answer it: the reply line, `ok` with the handle of
/// what it made or changed (a delete: `ok` alone), or `ok:false` with the sentence refusing it,
/// which is diagnosed too. A write that lands is narrated. Every request of the call ends by
/// `deadline`.
pub(crate) fn answer(
    action: &str,
    args: &Map<String, Value>,
    deadline: Instant,
    clients: &mut Clients,
    diag: &dyn Diag,
) -> String {
    let refuse = |error: String| {
        diag.err(&error);
        json!({ "ok": false, "error": error }).to_string()
    };
    let (account, call) = match decode(action, args) {
        Ok(decoded) => decoded,
        Err(problem) => return refuse(problem),
    };
    let client = clients.client(&account, deadline);
    let outcome = run(action, &call, client).and_then(|(reply, said)| {
        let line = fit_call(action, reply)?;
        Ok((line, said))
    });
    clients.clear_deadlines();
    match outcome {
        Ok((line, said)) => {
            diag.narrate(&said);
            line
        }
        Err(problem) => refuse(problem),
    }
}

/// The account and the write `args` ask for `action`, or the sentence refusing them.
///
/// # Errors
///
/// Returns the sentence for an `acct` that is no account or holds an empty key or token, an
/// argument missing or of another type than the manifest's, a handle whose key is none of the
/// kind the write takes, an `at` that is neither top nor bottom, a `path` that is not absolute,
/// and a `description` or `body` past Trello's 16 384 characters.
pub(crate) fn decode(
    action: &str,
    args: &Map<String, Value>,
) -> Result<(Account, WriteCall), String> {
    let account = Account::from_arg(action, args.get("acct"))?;
    let call = match action {
        "create_card" => WriteCall::CreateCard(Box::new(NewCard {
            board: board_arg(action, args)?,
            list: string(action, args, "list")?,
            name: string(action, args, "name")?,
            description: optional_text(action, args, "description")?,
            at: at(action, args)?.unwrap_or(ListPosition::Bottom),
            labels: strings(action, args, "labels")?,
            members: strings(action, args, "members")?,
            due: match due(action, args)? {
                Change::Set(due) => Some(due),
                Change::Keep | Change::Clear => None,
            },
        })),
        "update_card" => WriteCall::UpdateCard(Box::new(CardEdit {
            card: card_arg(action, args)?,
            name: optional(action, args, "name", text, "a string")?,
            description: optional_text(action, args, "description")?,
            list: optional(action, args, "list", text, "a string")?,
            at: at(action, args)?,
            due: due(action, args)?,
            due_complete: optional(action, args, "due_complete", Value::as_bool, "a bool")?,
        })),
        "archive_card" => WriteCall::ArchiveCard(card_arg(action, args)?),
        "unarchive_card" => WriteCall::UnarchiveCard(card_arg(action, args)?),
        "delete_card" => WriteCall::DeleteCard(card_arg(action, args)?),
        "create_comment" => WriteCall::CreateComment {
            card: card_arg(action, args)?,
            body: capped(action, args, "body")?,
        },
        "update_comment" => WriteCall::UpdateComment {
            comment: single_arg(action, args, "comment")?,
            body: capped(action, args, "body")?,
        },
        "delete_comment" => WriteCall::DeleteComment(single_arg(action, args, "comment")?),
        "create_checklist" => WriteCall::CreateChecklist {
            card: card_arg(action, args)?,
            name: string(action, args, "name")?,
            items: strings(action, args, "items")?,
        },
        "delete_checklist" => WriteCall::DeleteChecklist(single_arg(action, args, "checklist")?),
        "create_check_item" => WriteCall::CreateCheckItem {
            checklist: single_arg(action, args, "checklist")?,
            name: string(action, args, "name")?,
            at: at(action, args)?.unwrap_or(ListPosition::Bottom),
        },
        "update_check_item" => WriteCall::UpdateCheckItem {
            item: pair_arg(action, args, "item", parse_check_item_key)?,
            name: optional(action, args, "name", text, "a string")?,
            done: optional(action, args, "done", Value::as_bool, "a bool")?,
        },
        "delete_check_item" => {
            WriteCall::DeleteCheckItem(pair_arg(action, args, "item", parse_check_item_key)?)
        }
        "create_attachment" => {
            let card = card_arg(action, args)?;
            let path = string(action, args, "path")?;
            if !std::path::Path::new(&path).is_absolute() {
                return Err(format!("{action}: path must be absolute, not {path:?}"));
            }
            WriteCall::CreateAttachment {
                card,
                path,
                name: optional(action, args, "name", text, "a string")?,
            }
        }
        "delete_attachment" => {
            WriteCall::DeleteAttachment(pair_arg(action, args, "attachment", parse_attachment_key)?)
        }
        "add_label" => WriteCall::AddLabel {
            card: card_arg(action, args)?,
            label: string(action, args, "label")?,
        },
        "remove_label" => WriteCall::RemoveLabel {
            card: card_arg(action, args)?,
            label: string(action, args, "label")?,
        },
        "add_member" => WriteCall::AddMember {
            card: card_arg(action, args)?,
            member: member(action, args)?,
        },
        "remove_member" => WriteCall::RemoveMember {
            card: card_arg(action, args)?,
            member: member(action, args)?,
        },
        _ => return Err(format!("no action `{action}`")),
    };
    Ok((account, call))
}

/// A string value.
fn text(value: &Value) -> Option<String> {
    value.as_str().map(str::to_string)
}

/// The `member` a member write was passed: a username, or `me` when it is left out.
fn member(action: &str, args: &Map<String, Value>) -> Result<String, String> {
    Ok(optional(action, args, "member", text, "a string")?.unwrap_or_else(|| ME.to_string()))
}

/// The string argument `param` of `action`, refused past Trello's [`MAX_TEXT`] characters.
fn capped(action: &str, args: &Map<String, Value>, param: &str) -> Result<String, String> {
    let value = string(action, args, param)?;
    within(action, param, value)
}

/// The string argument `param` of `action`, `None` when it is left out or `nil`, refused past
/// Trello's [`MAX_TEXT`] characters.
fn optional_text(
    action: &str,
    args: &Map<String, Value>,
    param: &str,
) -> Result<Option<String>, String> {
    optional(action, args, param, text, "a string")?
        .map(|value| within(action, param, value))
        .transpose()
}

/// `value`, the argument `param` of `action`, or the sentence refusing it for being past
/// [`MAX_TEXT`] characters: counted as Trello counts, in characters and not bytes.
fn within(action: &str, param: &str, value: String) -> Result<String, String> {
    let length = value.chars().count();
    if length > MAX_TEXT {
        return Err(format!(
            "{action}: {param} is {length} characters, over the {MAX_TEXT} Trello takes"
        ));
    }
    Ok(value)
}

/// The list argument `param` of `action`, none when it is left out, or the sentence refusing
/// one that is not a list of strings.
fn strings(action: &str, args: &Map<String, Value>, param: &str) -> Result<Vec<String>, String> {
    let Some(items) = optional(action, args, param, Value::as_array, "a list of strings")? else {
        return Ok(Vec::new());
    };
    items
        .iter()
        .map(|item| {
            text(item)
                .ok_or_else(|| format!("{action}: parameter {param} must be a list of strings"))
        })
        .collect()
}

/// Where `at` places a card or an item, `None` when it is left out, or the sentence refusing
/// anything but top or bottom.
fn at(action: &str, args: &Map<String, Value>) -> Result<Option<ListPosition>, String> {
    let place = |value: &Value| match value.as_str() {
        Some("top") => Some(ListPosition::Top),
        Some("bottom") => Some(ListPosition::Bottom),
        _ => None,
    };
    optional(action, args, "at", place, "top or bottom")
}

/// The `due` argument of `action`: [`Change::Keep`] when it is left out, [`Change::Clear`] for
/// `nil`, or a time in unix milliseconds, or the sentence refusing anything else.
fn due(action: &str, args: &Map<String, Value>) -> Result<Change<u64>, String> {
    match args.get("due") {
        None => Ok(Change::Keep),
        Some(Value::Null) => Ok(Change::Clear),
        Some(value) => value
            .as_u64()
            .filter(|ms| *ms <= MAX_DUE_MS)
            .map(Change::Set)
            .ok_or_else(|| format!("{action}: parameter due must be a time")),
    }
}

/// The sentence refusing a handle whose key is not of the kind `action` takes as `param`.
fn not_ours(action: &str, param: &str, handle: &HandleArg) -> String {
    format!(
        "{action}: {param} {} has a key this plugin did not hand out: {}",
        handle.id, handle.key
    )
}

/// The handle `action` was passed as `param`, its key one of an object with an id of its own:
/// `trello:<id>`, a comment's or a checklist's.
fn single_arg(action: &str, args: &Map<String, Value>, param: &str) -> Result<HandleArg, String> {
    let handle = handle_arg(action, args, param)?;
    if parse_read_key(&handle.key).is_none() {
        return Err(not_ours(action, param, &handle));
    }
    Ok(handle)
}

/// The handle `action` was passed as `param`, its key one that names two ids,
/// `trello:<card id>/<id>`, as `parse` reads it: a check item's or an attachment's.
fn pair_arg(
    action: &str,
    args: &Map<String, Value>,
    param: &str,
    parse: fn(&str) -> Option<(&str, &str)>,
) -> Result<HandleArg, String> {
    let handle = handle_arg(action, args, param)?;
    if parse(&handle.key).is_none() {
        return Err(not_ours(action, param, &handle));
    }
    Ok(handle)
}

/// The sentence of a request Trello failed.
fn trello(e: BoardError) -> String {
    e.to_string()
}

/// `read`, the read-back of what `action` made or changed, or the sentence saying the write
/// landed and its read-back did not.
fn read_back<T>(action: &str, what: &str, read: Result<T, BoardError>) -> Result<T, String> {
    read.map_err(|e| format!("{action}: the {what} was written, but reading it back failed: {e}"))
}

/// The id of what Trello made, read off its reply to `stage`.
fn made_id(stage: &str, made: &Value, what: &str) -> Result<String, String> {
    match str_of(made, "id") {
        "" => Err(format!(
            "trello {stage}: undecodable response ({what} has no id)"
        )),
        id => Ok(id.to_string()),
    }
}

/// `time` in unix milliseconds as Trello writes a date.
fn rfc3339(millis: u64) -> String {
    format_utc_millis(UNIX_EPOCH + Duration::from_millis(millis))
}

/// The ok reply carrying `handle`.
fn handle_line<F: Serialize>(handle: &WireHandle<F>) -> String {
    json!({ "ok": true, "handle": handle }).to_string()
}

/// The ok reply of a write that gives nothing back.
fn done_line() -> String {
    json!({ "ok": true }).to_string()
}

/// Do `call`, the write `action`, through `client`: its ok reply line, and the line that
/// narrates it.
fn run(action: &str, call: &WriteCall, client: &TrelloClient) -> Result<(String, String), String> {
    let said = |what: String| format!("{ACTION_TAG} {what}");
    Ok(match call {
        WriteCall::CreateCard(new) => {
            let card = create_card(client, action, new)?;
            let said = said(format!(
                "created card {} {:?} in list {:?} of board {}",
                card.id, card.fields.name, card.fields.list, card.fields.board
            ));
            (handle_line(&card), said)
        }
        WriteCall::UpdateCard(edit) => {
            let card = update_card(client, action, edit)?;
            let said = said(format!("updated card {}", card.id));
            (handle_line(&card), said)
        }
        WriteCall::ArchiveCard(card) | WriteCall::UnarchiveCard(card) => {
            let archive = matches!(call, WriteCall::ArchiveCard(_));
            let (stage, did) = if archive {
                ("archive card", "archived")
            } else {
                ("unarchive card", "unarchived")
            };
            client
                .write_json(
                    stage,
                    "PUT",
                    &format!("/cards/{}", card_id(card)),
                    &[("closed", if archive { "true" } else { "false" })],
                    None,
                )
                .map_err(trello)?;
            let read = read_back(
                action,
                "card",
                client.read_card_handle("read card", card_id(card)),
            )?;
            (handle_line(&read), said(format!("{did} card {}", read.id)))
        }
        WriteCall::DeleteCard(card) => {
            client
                .delete("delete card", &format!("/cards/{}", card_id(card)))
                .map_err(trello)?;
            (done_line(), said(format!("deleted card {}", card.id)))
        }
        WriteCall::CreateComment { card, body } => {
            let made = client
                .write_json(
                    "create comment",
                    "POST",
                    &format!("/cards/{}/actions/comments", card_id(card)),
                    &[],
                    Some(&json!({ "text": body })),
                )
                .map_err(trello)?;
            let id = made_id("create comment", &made, "comment")?;
            let comment = read_comment(client, action, &id)?;
            (
                handle_line(&comment),
                said(format!(
                    "commented on card {}, comment {}",
                    comment.fields.card, comment.id
                )),
            )
        }
        WriteCall::UpdateComment { comment, body } => {
            let id = parse_read_key(&comment.key).unwrap_or(&comment.key);
            client
                .write_json(
                    "update comment",
                    "PUT",
                    &format!("/actions/{id}"),
                    &[],
                    Some(&json!({ "text": body })),
                )
                .map_err(trello)?;
            let comment = read_comment(client, action, id)?;
            (
                handle_line(&comment),
                said(format!(
                    "edited comment {} on card {}",
                    comment.id, comment.fields.card
                )),
            )
        }
        WriteCall::DeleteComment(comment) => {
            let id = parse_read_key(&comment.key).unwrap_or(&comment.key);
            client
                .delete("delete comment", &format!("/actions/{id}"))
                .map_err(trello)?;
            (done_line(), said(format!("deleted comment {}", comment.id)))
        }
        WriteCall::CreateChecklist { card, name, items } => {
            let checklist = create_checklist(client, action, card, name, items)?;
            let said = said(format!(
                "made checklist {:?} with {} items on card {}",
                checklist.fields.name,
                checklist.fields.items.len(),
                card.id
            ));
            (handle_line(&checklist), said)
        }
        WriteCall::DeleteChecklist(checklist) => {
            let id = parse_read_key(&checklist.key).unwrap_or(&checklist.key);
            client
                .delete("delete checklist", &format!("/checklists/{id}"))
                .map_err(trello)?;
            (
                done_line(),
                said(format!("deleted checklist {}", checklist.id)),
            )
        }
        WriteCall::CreateCheckItem {
            checklist,
            name,
            at,
        } => {
            let item = create_check_item(client, action, checklist, name, *at)?;
            let said = said(format!(
                "added check item {:?} to checklist {}",
                item.fields.name, item.fields.checklist
            ));
            (handle_line(&item), said)
        }
        WriteCall::UpdateCheckItem { item, name, done } => {
            let item = update_check_item(client, action, item, name.as_deref(), *done)?;
            let said = said(format!("updated check item {}", item.id));
            (handle_line(&item), said)
        }
        WriteCall::DeleteCheckItem(item) => {
            let (card, id) = pair(item)?;
            client
                .delete(
                    "delete check item",
                    &format!("/cards/{card}/checkItem/{id}"),
                )
                .map_err(trello)?;
            (done_line(), said(format!("deleted check item {}", item.id)))
        }
        WriteCall::CreateAttachment { card, path, name } => {
            let attachment = create_attachment(client, action, card, path, name.as_deref())?;
            let said = said(format!(
                "attached {:?} to card {} ({} bytes)",
                attachment.fields.name,
                card.id,
                attachment.fields.bytes.unwrap_or(0)
            ));
            (handle_line(&attachment), said)
        }
        WriteCall::DeleteAttachment(attachment) => {
            let (card, id) = pair(attachment)?;
            client
                .delete(
                    "delete attachment",
                    &format!("/cards/{card}/attachments/{id}"),
                )
                .map_err(trello)?;
            (
                done_line(),
                said(format!("deleted attachment {}", attachment.id)),
            )
        }
        WriteCall::AddLabel { card, label } => {
            let read = tag(client, action, card, Tag::Label, label, true)?;
            (
                handle_line(&read),
                said(format!("added label {label:?} to card {}", read.id)),
            )
        }
        WriteCall::RemoveLabel { card, label } => {
            let read = tag(client, action, card, Tag::Label, label, false)?;
            (
                handle_line(&read),
                said(format!("removed label {label:?} from card {}", read.id)),
            )
        }
        WriteCall::AddMember { card, member } => {
            let read = tag(client, action, card, Tag::Member, member, true)?;
            (
                handle_line(&read),
                said(format!("added member {member:?} to card {}", read.id)),
            )
        }
        WriteCall::RemoveMember { card, member } => {
            let read = tag(client, action, card, Tag::Member, member, false)?;
            (
                handle_line(&read),
                said(format!("removed member {member:?} from card {}", read.id)),
            )
        }
    })
}

/// Put the label or member `name` of the card's board on `card` (`add`), or take it off, and
/// read the card back.
///
/// The name is matched exactly against the card's own board, `me` being the member the token
/// acts as, so one the board lacks is refused naming the ones it has: no label is ever made.
/// Nothing is sent when the card is as asked — it carries the label already, or does not
/// carry the one to remove — and the card is read back all the same.
fn tag(
    client: &TrelloClient,
    action: &str,
    card: &HandleArg,
    what: Tag,
    name: &str,
    add: bool,
) -> Result<WireHandle<CardFields>, String> {
    let id = card_id(card);
    let here = client
        .get_json(
            "read card",
            &format!("/cards/{id}"),
            &[("fields", "idBoard,idLabels,idMembers")],
        )
        .map_err(trello)?;
    let board = str_of(&here, "idBoard");
    if board.is_empty() {
        return Err(format!(
            "{action}: trello read card: undecodable response (card has no board)"
        ));
    }
    // `route` is both the card's field holding the ids it carries and the path under the card
    // Trello adds to and removes from.
    let (route, wanted) = match what {
        Tag::Label => {
            let labels = candidates(&board_labels(client, board)?, "name");
            ("idLabels", one_named(action, "label", name, &labels)?)
        }
        Tag::Member => {
            let username = if name == ME {
                client.me_member("read me").map_err(trello)?.1
            } else {
                name.to_string()
            };
            let members = candidates(&board_members(client, board)?, "username");
            (
                "idMembers",
                one_named(action, "member", &username, &members)?,
            )
        }
    };
    let on = here
        .get(route)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .any(|held| held == wanted);
    if add && !on {
        client
            .write_json(
                if what == Tag::Label {
                    "add label"
                } else {
                    "add member"
                },
                "POST",
                &format!("/cards/{id}/{route}"),
                &[("value", &wanted)],
                None,
            )
            .map_err(trello)?;
    } else if !add && on {
        client
            .delete(
                if what == Tag::Label {
                    "remove label"
                } else {
                    "remove member"
                },
                &format!("/cards/{id}/{route}/{wanted}"),
            )
            .map_err(trello)?;
    }
    read_back(action, "card", client.read_card_handle("read card", id))
}

/// The card id and the other id a decoded check item's or attachment's key holds: the two
/// keys have one shape, so one reading serves both. Decoded already, so the key always parses.
fn pair(handle: &HandleArg) -> Result<(&str, &str), String> {
    parse_check_item_key(&handle.key).ok_or_else(|| not_ours("write", "handle", handle))
}

/// `(id, name)` of each of `items` with a non-empty string `key`: the candidates a name is
/// matched against.
fn candidates(items: &[Value], key: &str) -> Vec<(String, String)> {
    items
        .iter()
        .filter_map(|item| {
            let (id, name) = (str_of(item, "id"), str_of(item, key));
            (!id.is_empty() && !name.is_empty()).then(|| (id.to_string(), name.to_string()))
        })
        .collect()
}

/// The open lists of the board `board_id`, as the candidates a list name is matched against.
fn open_lists(client: &TrelloClient, board_id: &str) -> Result<Vec<(String, String)>, String> {
    let lists = array(
        client,
        "read lists",
        &format!("/boards/{board_id}/lists"),
        &[("filter", "open"), ("fields", "name")],
    )?;
    Ok(candidates(&lists, "name"))
}

/// File the card `new` asks for, and read it back. Every name it holds is resolved against the
/// board before anything is sent: the list, each label and each member, `"me"` being the
/// member the token acts as.
fn create_card(
    client: &TrelloClient,
    action: &str,
    new: &NewCard,
) -> Result<WireHandle<CardFields>, String> {
    let board = read_board(client, action, &new.board)?;
    let lists = open_lists(client, &board.id)?;
    let labels = candidates(&board_labels(client, &board.id)?, "name");
    let members = candidates(&board_members(client, &board.id)?, "username");

    let list = one_named(action, "list", &new.list, &lists)?;
    let mut label_ids: Vec<String> = Vec::new();
    for name in &new.labels {
        let id = one_named(action, "label", name, &labels)?;
        if !label_ids.contains(&id) {
            label_ids.push(id);
        }
    }
    let mut member_ids: Vec<String> = Vec::new();
    for who in &new.members {
        let username = if who == ME {
            client.me_member("read me").map_err(trello)?.1
        } else {
            who.clone()
        };
        let id = one_named(action, "member", &username, &members)?;
        if !member_ids.contains(&id) {
            member_ids.push(id);
        }
    }

    let mut body = Map::new();
    body.insert("idList".into(), json!(list));
    body.insert("name".into(), json!(new.name));
    if let Some(description) = &new.description {
        body.insert("desc".into(), json!(description));
    }
    body.insert("pos".into(), json!(pos_value(new.at)));
    if !label_ids.is_empty() {
        body.insert("idLabels".into(), json!(label_ids));
    }
    if !member_ids.is_empty() {
        body.insert("idMembers".into(), json!(member_ids));
    }
    if let Some(due) = new.due {
        body.insert("due".into(), json!(rfc3339(due)));
    }
    let made = client
        .write_json(
            "create card",
            "POST",
            "/cards",
            &[],
            Some(&Value::Object(body)),
        )
        .map_err(trello)?;
    let id = made_id("create card", &made, "card")?;
    read_back(action, "card", client.read_card_handle("read card", &id))
}

/// Where `update_card` puts a card of the list `current` that is to be in the list `target`,
/// and at what position: `None` when it stays where it is. A card moved to another list lands
/// at the top unless `at` says the bottom; one `at` places in the list it is in goes there.
fn placement(
    at: Option<ListPosition>,
    current: &str,
    target: &str,
) -> Option<(String, ListPosition)> {
    if target != current {
        Some((target.to_string(), at.unwrap_or(ListPosition::Top)))
    } else {
        at.map(|at| (target.to_string(), at))
    }
}

/// The JSON body of the one `PUT` `update_card` sends: only what `edit` gives, and `placed`
/// as the card's list and position when it is placed. Empty when nothing was given, and then
/// no `PUT` is sent.
fn put_body(edit: &CardEdit, placed: Option<&(String, ListPosition)>) -> Map<String, Value> {
    let mut body = Map::new();
    if let Some(name) = &edit.name {
        body.insert("name".into(), json!(name));
    }
    if let Some(description) = &edit.description {
        body.insert("desc".into(), json!(description));
    }
    match edit.due {
        Change::Keep => {}
        Change::Clear => {
            body.insert("due".into(), Value::Null);
        }
        Change::Set(due) => {
            body.insert("due".into(), json!(rfc3339(due)));
        }
    }
    if let Some(done) = edit.due_complete {
        body.insert("dueComplete".into(), json!(done));
    }
    if let Some((list, at)) = placed {
        body.insert("idList".into(), json!(list));
        body.insert("pos".into(), json!(pos_value(*at)));
    }
    body
}

/// Change the card `edit` names as it says, in one `PUT`, and read it back. A new `list` is
/// resolved against the card's own board first.
fn update_card(
    client: &TrelloClient,
    action: &str,
    edit: &CardEdit,
) -> Result<WireHandle<CardFields>, String> {
    let id = card_id(&edit.card);
    let placed = if edit.list.is_some() || edit.at.is_some() {
        let here = client
            .get_json(
                "read card",
                &format!("/cards/{id}"),
                &[("fields", "idBoard,idList")],
            )
            .map_err(trello)?;
        let current = str_of(&here, "idList");
        let target = match &edit.list {
            None => current.to_string(),
            Some(name) => {
                let board = str_of(&here, "idBoard");
                if board.is_empty() {
                    return Err(format!(
                        "{action}: trello read card: undecodable response (card has no board)"
                    ));
                }
                one_named(action, "list", name, &open_lists(client, board)?)?
            }
        };
        placement(edit.at, current, &target)
    } else {
        None
    };
    let body = put_body(edit, placed.as_ref());
    if !body.is_empty() {
        client
            .write_json(
                "update card",
                "PUT",
                &format!("/cards/{id}"),
                &[],
                Some(&Value::Object(body)),
            )
            .map_err(trello)?;
    }
    read_back(action, "card", client.read_card_handle("read card", id))
}

/// The comment action `id` as a handle, the card its short link.
fn read_comment(
    client: &TrelloClient,
    action: &str,
    id: &str,
) -> Result<WireHandle<CommentFields>, String> {
    let found = read_back(
        action,
        "comment",
        client.get_json(
            "read comment",
            &format!("/actions/{id}"),
            &[("memberCreator_fields", "username,fullName")],
        ),
    )?;
    let on = found
        .get("data")
        .and_then(|data| data.get("card"))
        .unwrap_or(&Value::Null);
    let card = match str_of(on, "shortLink") {
        "" => str_of(on, "id"),
        short => short,
    };
    build_comment(&found, card).ok_or_else(|| {
        format!("{action}: the comment was written, but reading it back failed: trello read comment: undecodable response (no text)")
    })
}

/// Make the checklist `name` on `card` and, in the order given, its `items`, and read it back.
/// An item Trello refuses leaves the checklist as far as it got, and the sentence says how far.
fn create_checklist(
    client: &TrelloClient,
    action: &str,
    card: &HandleArg,
    name: &str,
    items: &[String],
) -> Result<WireHandle<ChecklistFields>, String> {
    let made = client
        .write_json(
            "create checklist",
            "POST",
            &format!("/cards/{}/checklists", card_id(card)),
            &[],
            Some(&json!({ "name": name })),
        )
        .map_err(trello)?;
    let id = made_id("create checklist", &made, "checklist")?;
    for (made, item) in items.iter().enumerate() {
        let added = client.write_json(
            "create check item",
            "POST",
            &format!("/checklists/{id}/checkItems"),
            &[("pos", "bottom")],
            Some(&json!({ "name": item })),
        );
        if let Err(e) = added {
            return Err(format!(
                "{action}: checklist {name:?} ({id}) was made on card {} with {made} of its {} items, then: {e}",
                card.id,
                items.len()
            ));
        }
    }
    let found = read_back(
        action,
        "checklist",
        client.get_json(
            "read checklist",
            &format!("/checklists/{id}"),
            &[("checkItems", "all"), ("fields", "name,pos,idCard")],
        ),
    )?;
    build_checklist(&found, &card.id).ok_or_else(|| {
        format!("{action}: the checklist was written, but reading it back failed: trello read checklist: undecodable response (no id)")
    })
}

/// The check item `item_id` of card `card`, read with the card's checklists in the one request,
/// as a handle keyed by its card.
fn read_check_item(
    client: &TrelloClient,
    action: &str,
    card: &str,
    item_id: &str,
) -> Result<WireHandle<CheckItemFields>, String> {
    let found = read_back(
        action,
        "check item",
        client.get_json(
            "read card",
            &format!("/cards/{card}"),
            &[
                ("fields", "shortLink"),
                ("checklists", "all"),
                ("checkItems", "all"),
            ],
        ),
    )?;
    let short = match str_of(&found, "shortLink") {
        "" => card,
        short => short,
    };
    found
        .get("checklists")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .flat_map(|list| build_check_items(list, card, short))
        .find(|item| item.id == item_id)
        .ok_or_else(|| {
            format!("{action}: the check item was written, but reading it back failed: trello read card: undecodable response (no check item {item_id})")
        })
}

/// Add the item `name` to `checklist`, at its top or bottom, and read it back.
fn create_check_item(
    client: &TrelloClient,
    action: &str,
    checklist: &HandleArg,
    name: &str,
    at: ListPosition,
) -> Result<WireHandle<CheckItemFields>, String> {
    let list = parse_read_key(&checklist.key).unwrap_or(&checklist.key);
    let made = client
        .write_json(
            "create check item",
            "POST",
            &format!("/checklists/{list}/checkItems"),
            &[("pos", pos_value(at))],
            Some(&json!({ "name": name })),
        )
        .map_err(trello)?;
    let id = made_id("create check item", &made, "check item")?;
    let owner = read_back(
        action,
        "check item",
        client.get_json(
            "read checklist",
            &format!("/checklists/{list}"),
            &[("fields", "idCard")],
        ),
    )?;
    let card = str_of(&owner, "idCard");
    if card.is_empty() {
        return Err(format!(
            "{action}: the check item was written, but reading it back failed: trello read checklist: undecodable response (no card)"
        ));
    }
    read_check_item(client, action, card, &id)
}

/// Change the check item `item` as `name` and `done` say, in one `PUT` when either is given,
/// and read it back.
fn update_check_item(
    client: &TrelloClient,
    action: &str,
    item: &HandleArg,
    name: Option<&str>,
    done: Option<bool>,
) -> Result<WireHandle<CheckItemFields>, String> {
    let (card, id) = pair(item)?;
    let mut body = Map::new();
    if let Some(name) = name {
        body.insert("name".into(), json!(name));
    }
    if let Some(done) = done {
        body.insert(
            "state".into(),
            json!(if done { "complete" } else { "incomplete" }),
        );
    }
    if !body.is_empty() {
        client
            .write_json(
                "update check item",
                "PUT",
                &format!("/cards/{card}/checkItem/{id}"),
                &[],
                Some(&Value::Object(body)),
            )
            .map_err(trello)?;
    }
    read_check_item(client, action, card, id)
}

/// Put the file at `path`, read now, on `card` as `name` — the file's own name when none is
/// given — and read the attachment back.
fn create_attachment(
    client: &TrelloClient,
    action: &str,
    card: &HandleArg,
    path: &str,
    name: Option<&str>,
) -> Result<WireHandle<AttachmentFields>, String> {
    let own = std::path::Path::new(path)
        .file_name()
        .map(|file| file.to_string_lossy().into_owned())
        .unwrap_or_default();
    // `read_upload` speaks as `attach`, its old name; this call speaks as itself.
    let upload = read_upload(path, name.unwrap_or(&own)).map_err(|refusal| {
        match refusal.strip_prefix("attach: ") {
            Some(rest) => format!("{action}: {rest}"),
            None => refusal,
        }
    })?;
    let card_key = card_id(card);
    let made = client
        .upload(
            "create attachment",
            &format!("/cards/{card_key}/attachments"),
            &upload.name,
            &upload.bytes,
        )
        .map_err(trello)?;
    let id = made_id("create attachment", &made, "attachment")?;
    let found = read_back(
        action,
        "attachment",
        client.get_json(
            "read attachment",
            &format!("/cards/{card_key}/attachments/{id}"),
            &[("fields", ATTACHMENT_FIELDS)],
        ),
    )?;
    build_attachment(&found, card_key, &card.id).ok_or_else(|| {
        format!("{action}: the attachment was written, but reading it back failed: trello read attachment: undecodable response (no id)")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A write's arguments as afkd binds them: an account and `extra`.
    fn args(extra: Value) -> Map<String, Value> {
        let mut args = Map::from_iter([(
            "acct".to_string(),
            json!({"api_key": "k", "token": "t", "base_url": "http://127.0.0.1:9/1"}),
        )]);
        args.extend(extra.as_object().expect("an args object").clone());
        args
    }

    fn call(action: &str, extra: Value) -> Result<WriteCall, String> {
        decode(action, &args(extra)).map(|(_, call)| call)
    }

    const CARD_ID: &str = "6a4dd5de1234abcd5678ef90";

    fn card_json() -> Value {
        json!({"id": "Qb7eLy2w", "key": format!("trello:{CARD_ID}")})
    }

    fn handle(id: &str, key: &str) -> HandleArg {
        HandleArg {
            id: id.into(),
            key: key.into(),
        }
    }

    fn card() -> HandleArg {
        handle("Qb7eLy2w", &format!("trello:{CARD_ID}"))
    }

    fn words(w: &[&str]) -> Vec<String> {
        w.iter().map(|s| s.to_string()).collect()
    }

    /// The writes are the nineteen, in the manifest's order, and share no name with a read.
    #[test]
    fn the_writes_are_nineteen_and_name_nothing_else() {
        assert_eq!(WRITES.len(), 19);
        let mut all: Vec<&str> = crate::reads::READS.iter().chain(WRITES).copied().collect();
        let count = all.len();
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), count);
    }

    /// `create_card` reads the board by address, the three names, and `at`, `labels`,
    /// `members` and `due` as given; left out, the card goes to the bottom with nothing else.
    #[test]
    fn create_card_decodes_its_names_and_options() {
        let bare = call(
            "create_card",
            json!({"board": "1Rkelydw", "list": "Backlog", "name": "Plan 計画"}),
        );
        assert_eq!(
            bare,
            Ok(WriteCall::CreateCard(Box::new(NewCard {
                board: "1Rkelydw".into(),
                list: "Backlog".into(),
                name: "Plan 計画".into(),
                description: None,
                at: ListPosition::Bottom,
                labels: Vec::new(),
                members: Vec::new(),
                due: None,
            })))
        );
        let full = call(
            "create_card",
            json!({
                "board": "https://trello.com/b/BID/afkd", "list": "Backlog", "name": "Plan",
                "description": "## Why", "at": "top", "labels": ["bug", "ready"],
                "members": ["me", "phil"], "due": 1_798_106_400_250u64,
            }),
        );
        assert_eq!(
            full,
            Ok(WriteCall::CreateCard(Box::new(NewCard {
                board: "BID".into(),
                list: "Backlog".into(),
                name: "Plan".into(),
                description: Some("## Why".into()),
                at: ListPosition::Top,
                labels: words(&["bug", "ready"]),
                members: words(&["me", "phil"]),
                due: Some(1_798_106_400_250),
            })))
        );
        // `nil` for the due date is no date, as it is left out.
        let nil = call(
            "create_card",
            json!({"board": "B", "list": "L", "name": "N", "due": null, "description": null}),
        );
        assert!(
            matches!(nil, Ok(WriteCall::CreateCard(n)) if n.due.is_none() && n.description.is_none())
        );
    }

    /// `update_card` gives each field only when it is passed: nothing leaves every one off,
    /// `nil` clears the due date, and an empty description is a description.
    #[test]
    fn update_card_decodes_only_what_is_given() {
        let edit = |extra: Value| {
            let mut full = json!({"card": card_json()});
            full.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            match call("update_card", full) {
                Ok(WriteCall::UpdateCard(edit)) => *edit,
                other => panic!("{other:?}"),
            }
        };
        assert_eq!(
            edit(json!({})),
            CardEdit {
                card: card(),
                name: None,
                description: None,
                list: None,
                at: None,
                due: Change::Keep,
                due_complete: None,
            }
        );
        assert_eq!(
            edit(json!({
                "name": "New", "description": "", "list": "Doing", "at": "bottom",
                "due": null, "due_complete": true,
            })),
            CardEdit {
                card: card(),
                name: Some("New".into()),
                description: Some("".into()),
                list: Some("Doing".into()),
                at: Some(ListPosition::Bottom),
                due: Change::Clear,
                due_complete: Some(true),
            }
        );
        assert_eq!(edit(json!({"due": 0})).due, Change::Set(0));
    }

    /// Every other write reads its handle by the kind of its key and its other arguments.
    #[test]
    fn every_other_write_decodes_its_handle_and_arguments() {
        let list_key = "trello:6a4dd5de1234abcd5678ef91";
        let comment = json!({"id": "6a4dd5de1234abcd5678ef91", "key": list_key});
        let item_key = format!("trello:{CARD_ID}/6a4dd5de1234abcd5678ef92");
        let item = json!({"id": "6a4dd5de1234abcd5678ef92", "key": item_key});
        let attachment = json!({"id": "a1", "key": format!("trello:{CARD_ID}/a1")});
        let comment_arg = handle("6a4dd5de1234abcd5678ef91", list_key);
        let item_arg = handle("6a4dd5de1234abcd5678ef92", &item_key);
        for (action, extra, want) in [
            (
                "archive_card",
                json!({"card": card_json()}),
                WriteCall::ArchiveCard(card()),
            ),
            (
                "unarchive_card",
                json!({"card": card_json()}),
                WriteCall::UnarchiveCard(card()),
            ),
            (
                "delete_card",
                json!({"card": card_json()}),
                WriteCall::DeleteCard(card()),
            ),
            (
                "create_comment",
                json!({"card": card_json(), "body": "看起来不对 🚨"}),
                WriteCall::CreateComment {
                    card: card(),
                    body: "看起来不对 🚨".into(),
                },
            ),
            (
                "update_comment",
                json!({"comment": comment, "body": ""}),
                WriteCall::UpdateComment {
                    comment: comment_arg.clone(),
                    body: "".into(),
                },
            ),
            (
                "delete_comment",
                json!({"comment": comment}),
                WriteCall::DeleteComment(comment_arg.clone()),
            ),
            (
                "create_checklist",
                json!({"card": card_json(), "name": "Acceptance"}),
                WriteCall::CreateChecklist {
                    card: card(),
                    name: "Acceptance".into(),
                    items: Vec::new(),
                },
            ),
            (
                "create_checklist",
                json!({"card": card_json(), "name": "Acceptance", "items": ["one", "二"]}),
                WriteCall::CreateChecklist {
                    card: card(),
                    name: "Acceptance".into(),
                    items: words(&["one", "二"]),
                },
            ),
            (
                "delete_checklist",
                json!({"checklist": comment}),
                WriteCall::DeleteChecklist(comment_arg.clone()),
            ),
            (
                "create_check_item",
                json!({"checklist": comment, "name": "first"}),
                WriteCall::CreateCheckItem {
                    checklist: comment_arg.clone(),
                    name: "first".into(),
                    at: ListPosition::Bottom,
                },
            ),
            (
                "create_check_item",
                json!({"checklist": comment, "name": "first", "at": "top"}),
                WriteCall::CreateCheckItem {
                    checklist: comment_arg,
                    name: "first".into(),
                    at: ListPosition::Top,
                },
            ),
            (
                "update_check_item",
                json!({"item": item}),
                WriteCall::UpdateCheckItem {
                    item: item_arg.clone(),
                    name: None,
                    done: None,
                },
            ),
            (
                "update_check_item",
                json!({"item": item, "name": "renamed", "done": false}),
                WriteCall::UpdateCheckItem {
                    item: item_arg.clone(),
                    name: Some("renamed".into()),
                    done: Some(false),
                },
            ),
            (
                "delete_check_item",
                json!({"item": item}),
                WriteCall::DeleteCheckItem(item_arg),
            ),
            (
                "create_attachment",
                json!({"card": card_json(), "path": "/srv/計画 1/plan.md"}),
                WriteCall::CreateAttachment {
                    card: card(),
                    path: "/srv/計画 1/plan.md".into(),
                    name: None,
                },
            ),
            (
                "create_attachment",
                json!({"card": card_json(), "path": "/srv/plan.md", "name": "the plan"}),
                WriteCall::CreateAttachment {
                    card: card(),
                    path: "/srv/plan.md".into(),
                    name: Some("the plan".into()),
                },
            ),
            (
                "delete_attachment",
                json!({"attachment": attachment}),
                WriteCall::DeleteAttachment(handle("a1", &format!("trello:{CARD_ID}/a1"))),
            ),
            (
                "add_label",
                json!({"card": card_json(), "label": "bug 🐛"}),
                WriteCall::AddLabel {
                    card: card(),
                    label: "bug 🐛".into(),
                },
            ),
            (
                "remove_label",
                json!({"card": card_json(), "label": "bug 🐛"}),
                WriteCall::RemoveLabel {
                    card: card(),
                    label: "bug 🐛".into(),
                },
            ),
            (
                "add_member",
                json!({"card": card_json()}),
                WriteCall::AddMember {
                    card: card(),
                    member: "me".into(),
                },
            ),
            (
                "add_member",
                json!({"card": card_json(), "member": "phil"}),
                WriteCall::AddMember {
                    card: card(),
                    member: "phil".into(),
                },
            ),
            (
                "remove_member",
                json!({"card": card_json(), "member": null}),
                WriteCall::RemoveMember {
                    card: card(),
                    member: "me".into(),
                },
            ),
            (
                "remove_member",
                json!({"card": card_json(), "member": "phil"}),
                WriteCall::RemoveMember {
                    card: card(),
                    member: "phil".into(),
                },
            ),
        ] {
            assert_eq!(call(action, extra.clone()), Ok(want), "{action} {extra}");
        }
    }

    /// Each argument a write cannot use is refused in its own sentence before any request.
    #[test]
    fn a_write_refuses_what_it_cannot_use() {
        let comment = json!({"id": "c1", "key": "trello:6a4dd5de1234abcd5678ef91"});
        let item = json!({"id": "i1", "key": format!("trello:{CARD_ID}/i1")});
        for (action, extra, refusal) in [
            (
                "create_card",
                json!({"list": "L", "name": "N"}),
                "create_card: parameter board is required".to_string(),
            ),
            (
                "create_card",
                json!({"board": " ", "list": "L", "name": "N"}),
                "create_card: board must name a board, not \" \"".to_string(),
            ),
            (
                "create_card",
                json!({"board": "B", "name": "N"}),
                "create_card: parameter list is required".to_string(),
            ),
            (
                "create_card",
                json!({"board": "B", "list": "L"}),
                "create_card: parameter name is required".to_string(),
            ),
            (
                "create_card",
                json!({"board": "B", "list": "L", "name": 7}),
                "create_card: parameter name must be a string".to_string(),
            ),
            (
                "create_card",
                json!({"board": "B", "list": "L", "name": "N", "at": "middle"}),
                "create_card: parameter at must be top or bottom".to_string(),
            ),
            (
                "create_card",
                json!({"board": "B", "list": "L", "name": "N", "labels": "bug"}),
                "create_card: parameter labels must be a list of strings".to_string(),
            ),
            (
                "create_card",
                json!({"board": "B", "list": "L", "name": "N", "members": [1]}),
                "create_card: parameter members must be a list of strings".to_string(),
            ),
            (
                "create_card",
                json!({"board": "B", "list": "L", "name": "N", "due": "tomorrow"}),
                "create_card: parameter due must be a time".to_string(),
            ),
            (
                "create_card",
                json!({"board": "B", "list": "L", "name": "N", "due": -1}),
                "create_card: parameter due must be a time".to_string(),
            ),
            (
                "update_card",
                json!({}),
                "update_card: parameter card is required".to_string(),
            ),
            (
                "update_card",
                json!({"card": "Qb7eLy2w"}),
                "update_card: parameter card must be an item handle".to_string(),
            ),
            (
                "update_card",
                json!({"card": {"id": "Qb7eLy2w", "key": format!("trello:{CARD_ID}/a1")}}),
                format!(
                    "update_card: card Qb7eLy2w has a key this plugin did not hand out: \
                     trello:{CARD_ID}/a1"
                ),
            ),
            (
                "update_card",
                json!({"card": card_json(), "due": true}),
                "update_card: parameter due must be a time".to_string(),
            ),
            (
                "update_card",
                json!({"card": card_json(), "due_complete": "yes"}),
                "update_card: parameter due_complete must be a bool".to_string(),
            ),
            (
                "update_card",
                json!({"card": card_json(), "at": 1}),
                "update_card: parameter at must be top or bottom".to_string(),
            ),
            (
                "archive_card",
                json!({"card": {"id": "x", "key": "6a4dd5de1234abcd5678ef90#6a4dd5ff1234abcd5678ef91"}}),
                "archive_card: card x has a key this plugin did not hand out: \
                 6a4dd5de1234abcd5678ef90#6a4dd5ff1234abcd5678ef91"
                    .to_string(),
            ),
            (
                "create_comment",
                json!({"card": card_json()}),
                "create_comment: parameter body is required".to_string(),
            ),
            (
                "update_comment",
                json!({"comment": {"id": "c1", "key": format!("trello:{CARD_ID}/c1")}, "body": "x"}),
                format!(
                    "update_comment: comment c1 has a key this plugin did not hand out: \
                     trello:{CARD_ID}/c1"
                ),
            ),
            (
                "delete_comment",
                json!({}),
                "delete_comment: parameter comment is required".to_string(),
            ),
            (
                "delete_checklist",
                json!({"checklist": {"id": "x", "key": "x"}}),
                "delete_checklist: checklist x has a key this plugin did not hand out: x"
                    .to_string(),
            ),
            (
                "create_checklist",
                json!({"card": card_json(), "name": "A", "items": ["a", 2]}),
                "create_checklist: parameter items must be a list of strings".to_string(),
            ),
            (
                "create_check_item",
                json!({"checklist": comment}),
                "create_check_item: parameter name is required".to_string(),
            ),
            (
                "update_check_item",
                json!({"item": comment}),
                "update_check_item: item c1 has a key this plugin did not hand out: \
                 trello:6a4dd5de1234abcd5678ef91"
                    .to_string(),
            ),
            (
                "update_check_item",
                json!({"item": item, "done": 1}),
                "update_check_item: parameter done must be a bool".to_string(),
            ),
            (
                "delete_check_item",
                json!({"item": {"id": "i1", "key": "trello:i1"}}),
                "delete_check_item: item i1 has a key this plugin did not hand out: trello:i1"
                    .to_string(),
            ),
            (
                "create_attachment",
                json!({"card": card_json(), "path": "plan.md"}),
                "create_attachment: path must be absolute, not \"plan.md\"".to_string(),
            ),
            (
                "create_attachment",
                json!({"card": card_json()}),
                "create_attachment: parameter path is required".to_string(),
            ),
            (
                "create_attachment",
                json!({"card": card_json(), "path": "/x", "name": 4}),
                "create_attachment: parameter name must be a string".to_string(),
            ),
            (
                "delete_attachment",
                json!({"attachment": {"id": "a1", "key": "trello:a1"}}),
                "delete_attachment: attachment a1 has a key this plugin did not hand out: \
                 trello:a1"
                    .to_string(),
            ),
            (
                "add_label",
                json!({"card": card_json()}),
                "add_label: parameter label is required".to_string(),
            ),
            (
                "remove_label",
                json!({"card": card_json(), "label": 3}),
                "remove_label: parameter label must be a string".to_string(),
            ),
            (
                "add_label",
                json!({"label": "bug"}),
                "add_label: parameter card is required".to_string(),
            ),
            (
                "add_member",
                json!({"card": card_json(), "member": 3}),
                "add_member: parameter member must be a string".to_string(),
            ),
            (
                "remove_member",
                json!({"card": {"id": "x", "key": "x#y"}}),
                "remove_member: card x has a key this plugin did not hand out: x#y".to_string(),
            ),
            ("assign", json!({}), "no action `assign`".to_string()),
            // The old actions are gone: no write is named for one.
            ("archive", json!({}), "no action `archive`".to_string()),
            ("move_to", json!({}), "no action `move_to`".to_string()),
        ] {
            assert_eq!(
                call(action, extra.clone()),
                Err(refusal),
                "{action} {extra}"
            );
        }
        // The account is read first, so its refusal comes before any other.
        let mut no_key = args(json!({}));
        no_key.insert("acct".into(), json!({"api_key": "", "token": "t"}));
        assert_eq!(
            decode("delete_card", &no_key).map(|_| ()),
            Err("delete_card: acct.api_key is empty".to_string())
        );
        no_key.remove("acct");
        assert_eq!(
            decode("create_card", &no_key).map(|_| ()),
            Err("create_card: parameter acct is required".to_string())
        );
    }

    /// A description or a body of 16 384 characters is sent and one of 16 385 refused, counted
    /// in characters and not bytes, whichever write carries it.
    #[test]
    fn a_text_past_trellos_cap_is_refused_before_any_request() {
        let edge = "é".repeat(16_384);
        let over = "é".repeat(16_385);
        let sentence = |action: &str, param: &str| {
            format!("{action}: {param} is 16385 characters, over the 16384 Trello takes")
        };
        let create = |description: &str| {
            call(
                "create_card",
                json!({"board": "B", "list": "L", "name": "N", "description": description}),
            )
        };
        assert!(create(&edge).is_ok());
        assert_eq!(create(&over), Err(sentence("create_card", "description")));
        let update = |description: &str| {
            call(
                "update_card",
                json!({"card": card_json(), "description": description}),
            )
        };
        assert!(update(&edge).is_ok());
        assert_eq!(update(&over), Err(sentence("update_card", "description")));
        let comment =
            |body: &str| call("create_comment", json!({"card": card_json(), "body": body}));
        assert!(comment(&edge).is_ok());
        assert_eq!(comment(&over), Err(sentence("create_comment", "body")));
        let edit = |body: &str| {
            call(
                "update_comment",
                json!({"comment": {"id": "c", "key": "trello:6a4dd5de1234abcd5678ef91"}, "body": body}),
            )
        };
        assert!(edit(&edge).is_ok());
        assert_eq!(edit(&over), Err(sentence("update_comment", "body")));
    }

    /// A card moved to another list lands at the top unless `at` says otherwise; one `at`
    /// places in the list it is in goes there; and one told neither stays.
    #[test]
    fn a_card_is_placed_only_when_it_moves_or_at_says_so() {
        use ListPosition::{Bottom, Top};
        let placed = |at, target: &str| placement(at, "here", target);
        assert_eq!(placed(None, "there"), Some(("there".into(), Top)));
        assert_eq!(
            placed(Some(Bottom), "there"),
            Some(("there".into(), Bottom))
        );
        assert_eq!(placed(Some(Top), "there"), Some(("there".into(), Top)));
        assert_eq!(placed(None, "here"), None);
        assert_eq!(placed(Some(Bottom), "here"), Some(("here".into(), Bottom)));
        assert_eq!(placed(Some(Top), "here"), Some(("here".into(), Top)));
    }

    /// The `PUT` body of `update_card` holds only what was given, `idList` and `pos` only
    /// when the card is placed, and a due date as Trello writes one, to the millisecond;
    /// nothing given is an empty body.
    #[test]
    fn the_update_body_holds_only_what_was_given() {
        let edit = CardEdit {
            card: card(),
            name: None,
            description: None,
            list: None,
            at: None,
            due: Change::Keep,
            due_complete: None,
        };
        assert!(put_body(&edit, None).is_empty());
        let body = |edit: &CardEdit, placed: Option<(String, ListPosition)>| {
            Value::Object(put_body(edit, placed.as_ref()))
        };
        // A list change alone: the list and the top.
        assert_eq!(
            body(&edit, placement(None, "a", "b")),
            json!({"idList": "b", "pos": "top"})
        );
        // `at` alone: the list the card is in, and the place.
        assert_eq!(
            body(&edit, placement(Some(ListPosition::Bottom), "a", "a")),
            json!({"idList": "a", "pos": "bottom"})
        );
        // A list change and `at`.
        assert_eq!(
            body(&edit, placement(Some(ListPosition::Bottom), "a", "b")),
            json!({"idList": "b", "pos": "bottom"})
        );
        // The same list and no `at`: nothing to place.
        assert_eq!(body(&edit, placement(None, "a", "a")), json!({}));
        // Everything given, a description emptied and a due date to the millisecond.
        let all = CardEdit {
            name: Some("New".into()),
            description: Some("".into()),
            due: Change::Set(1_798_106_400_250),
            due_complete: Some(false),
            ..edit.clone()
        };
        assert_eq!(
            body(&all, placement(Some(ListPosition::Top), "a", "b")),
            json!({
                "name": "New", "desc": "", "due": "2026-12-24T10:00:00.250Z",
                "dueComplete": false, "idList": "b", "pos": "top",
            })
        );
        // `nil` clears the due date.
        let cleared = CardEdit {
            due: Change::Clear,
            ..edit
        };
        assert_eq!(body(&cleared, None), json!({"due": null}));
    }
}
