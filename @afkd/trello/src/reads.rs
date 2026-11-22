//! The **account reads** a config calls with a `trello.Account` first: a board's cards, a
//! card's comments, checklists, check items and attachments, one attachment saved to a path,
//! the member the token acts as, the boards it reaches, and a board's lists, labels and
//! members.
//!
//! A read reaches Trello only through the account it is passed ([`Account`]): it reads no
//! service's settings, and the kind's own client is never used for it. So it answers the same
//! in a child afkd starts for the one call as in a `service(trello)`'s trigger child. Each read
//!
//! 1. decodes its arguments, a refusal sent before any request ([`decode`]);
//! 2. reads what it reads from Trello through the account's retrying client, each list to its
//!    end — the board's cards past Trello's 1000, a card's comments to the `limit`;
//! 3. answers with handles keyed `trello:<Trello's id>` (an attachment's `trello:<card
//!    id>/<attachment id>`), or a value, or nothing, in one reply under afkd's 8 MiB for a
//!    call ([`fit_call`]).
//!
//! A read is never queued: it is done now, a request Trello answered "later" tried again
//! within the call's deadline, or refused with the reason.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Instant;

use serde::Serialize;
use serde_json::{json, Map, Value};

use crate::account::{Account, Clients};
use crate::attachment::{write_atomic, MAX_ATTACHMENT_BYTES};
use crate::board::{BoardClient, BoardError};
use crate::card::ACTION_TAG;
use crate::client::{parse_labels_field, TrelloClient};
use crate::common::Diag;
use crate::handles::{
    build_attachment, build_board, build_card, build_check_items, build_checklist, build_comment,
    build_label, build_list, build_member, parse_attachment_key, parse_read_key, position,
    read_key, str_of, CardFields, Lookups, MemberFields,
};
use crate::settings::{derive_board_id, ME};
use crate::wire::{WireHandle, MAX_CALL_REPLY};

/// The reads, in the order the manifest declares them, after today's actions
/// ([`ACTIONS`](crate::lifecycle::ACTIONS)).
pub(crate) const READS: &[&str] = &[
    "cards",
    "comments",
    "checklists",
    "check_items",
    "attachments",
    "download_attachment",
    "me",
    "boards",
    "lists",
    "labels",
    "members",
];

/// How many cards `cards` reads when its `limit` is left out.
const DEFAULT_CARDS: i64 = 100;

/// How many comments `comments` reads when its `limit` is left out.
const DEFAULT_COMMENTS: i64 = 200;

/// The most either read returns, and Trello's own page: it answers no more than 1000 cards
/// or actions, and refuses a `limit` of 1001.
const MAX_LIMIT: i64 = 1000;

/// [`MAX_LIMIT`] as a count.
const PAGE: usize = 1000;

/// The card fields a board read asks for: every one a `trello.Card` carries, no more, so a
/// page of a thousand cards stays as small as it can.
const CARD_FIELDS: &str = "name,desc,idList,idMembers,labels,shortLink,url,closed,due,dueComplete,\
                           idShort,dateLastActivity,pos";

/// The attachment fields a read asks for: every one a `trello.Attachment` carries.
const ATTACHMENT_FIELDS: &str = "id,name,date,url,isUpload,bytes,mimeType";

/// A handle a read was passed: the id and the key it carries.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct HandleArg {
    pub(crate) id: String,
    pub(crate) key: String,
}

/// What `cards` keeps and how it reads them, as its arguments say. A filter left out is
/// `None`, and keeps every card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CardQuery {
    /// The board, by address or short link, reduced to the id its address carries.
    pub(crate) board: String,
    pub(crate) list: Option<Vec<String>>,
    pub(crate) labels: Option<Vec<String>>,
    pub(crate) without_labels: Option<Vec<String>>,
    pub(crate) members: Option<Vec<String>>,
    pub(crate) include_archived: bool,
    pub(crate) limit: usize,
}

/// One read, decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReadCall {
    Cards(Box<CardQuery>),
    Comments { card: HandleArg, limit: usize },
    Checklists(HandleArg),
    CheckItems(HandleArg),
    Attachments(HandleArg),
    DownloadAttachment { attachment: HandleArg, to: String },
    Me,
    Boards,
    Lists { board: String },
    Labels { board: String },
    Members { board: String },
}

/// Do the read `action` with `args`, and answer it: the reply line, `ok` with what it read,
/// or `ok:false` with the sentence refusing it, which is diagnosed too. A read that lands is
/// narrated. Every request of the call ends by `deadline`.
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

/// `line`, the ok reply to `action`, when it fits the 8 MiB afkd reads of a call's reply; or
/// the sentence saying it does not, and how to read less.
///
/// # Errors
///
/// Returns the sentence when the line runs past [`MAX_CALL_REPLY`]: for `cards` and
/// `comments` it names the `limit` to lower, and for every other read says the reply is too
/// large.
pub(crate) fn fit_call(action: &str, line: String) -> Result<String, String> {
    if line.len() <= MAX_CALL_REPLY {
        return Ok(line);
    }
    let size = line.len();
    Err(match action {
        "cards" | "comments" => format!(
            "{action}: the reply comes to {size} bytes, over afkd's 8 MiB for one call; lower \
             limit"
        ),
        _ => format!(
            "{action}: the reply is too large: {size} bytes, over afkd's 8 MiB for one call"
        ),
    })
}

/// The account and the read `args` write for `action`, or the sentence refusing them.
///
/// # Errors
///
/// Returns the sentence for an `acct` that is no account or holds an empty key or token, an
/// argument missing or of another type than the manifest's, an empty filter list, a `limit`
/// outside 1 to 1000, a handle whose key is none of the kind the read takes, and a
/// `download_attachment` to a path that is not absolute.
pub(crate) fn decode(
    action: &str,
    args: &Map<String, Value>,
) -> Result<(Account, ReadCall), String> {
    let account = Account::from_arg(action, args.get("acct"))?;
    let call = match action {
        "cards" => ReadCall::Cards(Box::new(card_query(args)?)),
        "comments" => {
            let limit = optional(action, args, "limit", Value::as_i64, "an int")?
                .unwrap_or(DEFAULT_COMMENTS);
            ReadCall::Comments {
                card: card_arg(action, args)?,
                limit: limit_of(action, limit)?,
            }
        }
        "checklists" => ReadCall::Checklists(card_arg(action, args)?),
        "check_items" => ReadCall::CheckItems(card_arg(action, args)?),
        "attachments" => ReadCall::Attachments(card_arg(action, args)?),
        "download_attachment" => {
            let attachment = handle_arg(action, args, "attachment")?;
            if parse_attachment_key(&attachment.key).is_none() {
                return Err(format!(
                    "{action}: attachment {} has a key this plugin did not hand out: {}",
                    attachment.id, attachment.key
                ));
            }
            let to = string(action, args, "to")?;
            if !Path::new(&to).is_absolute() {
                return Err(format!("{action}: to must be absolute, not {to:?}"));
            }
            ReadCall::DownloadAttachment { attachment, to }
        }
        "me" => ReadCall::Me,
        "boards" => ReadCall::Boards,
        "lists" => ReadCall::Lists {
            board: board_arg(action, args)?,
        },
        "labels" => ReadCall::Labels {
            board: board_arg(action, args)?,
        },
        "members" => ReadCall::Members {
            board: board_arg(action, args)?,
        },
        _ => return Err(format!("no action `{action}`")),
    };
    Ok((account, call))
}

/// The board `action` was passed, by its address or its short link, as the id the address
/// carries.
fn board_arg(action: &str, args: &Map<String, Value>) -> Result<String, String> {
    let address = string(action, args, "board")?;
    if address.trim().is_empty() {
        return Err(format!(
            "{action}: board must name a board, not {address:?}"
        ));
    }
    Ok(derive_board_id(address.trim()))
}

/// A `limit` of `action` as a count, or the sentence refusing one outside 1 to 1000.
fn limit_of(action: &str, limit: i64) -> Result<usize, String> {
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(format!(
            "{action}: limit {limit} is outside 1 to {MAX_LIMIT}"
        ));
    }
    Ok(usize::try_from(limit).unwrap_or(1))
}

/// What `cards` keeps and how it reads them, as `args` say.
fn card_query(args: &Map<String, Value>) -> Result<CardQuery, String> {
    let action = "cards";
    let limit = optional(action, args, "limit", Value::as_i64, "an int")?.unwrap_or(DEFAULT_CARDS);
    Ok(CardQuery {
        board: board_arg(action, args)?,
        list: names(args, "list")?,
        labels: names(args, "labels")?,
        without_labels: names(args, "without_labels")?,
        members: names(args, "members")?,
        include_archived: optional(action, args, "include_archived", Value::as_bool, "a bool")?
            .unwrap_or(false),
        limit: limit_of(action, limit)?,
    })
}

/// The list of names `cards`'s filter `param` holds: `None` when it is left out, and a
/// refusal for one of another type and for an empty one, which names nothing to keep.
fn names(args: &Map<String, Value>, param: &str) -> Result<Option<Vec<String>>, String> {
    let Some(items) = optional("cards", args, param, Value::as_array, "a list of strings")? else {
        return Ok(None);
    };
    if items.is_empty() {
        return Err(format!(
            "cards: `{param}` is empty; leave it out to keep every card"
        ));
    }
    items
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("cards: parameter {param} must be a list of strings"))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

/// The handle `action` was passed as its argument `param`, as afkd sends one,
/// `{"id":…,"key":…}`, or the sentence refusing an argument that is missing or no handle.
fn handle_arg(action: &str, args: &Map<String, Value>, param: &str) -> Result<HandleArg, String> {
    let handle = match args.get(param) {
        Some(Value::Object(handle)) => Some(handle),
        Some(_) => None,
        None => return Err(format!("{action}: parameter {param} is required")),
    };
    let key = handle.and_then(|h| h.get("key")).and_then(Value::as_str);
    let Some(key) = key else {
        return Err(format!(
            "{action}: parameter {param} must be an item handle"
        ));
    };
    Ok(HandleArg {
        id: handle
            .and_then(|h| h.get("id"))
            .and_then(Value::as_str)
            .unwrap_or(key)
            .to_string(),
        key: key.to_string(),
    })
}

/// The card `action` was passed, its key one of a card's: `trello:<card id>`, whether the
/// card was claimed or read.
fn card_arg(action: &str, args: &Map<String, Value>) -> Result<HandleArg, String> {
    let card = handle_arg(action, args, "card")?;
    if parse_read_key(&card.key).is_none() {
        return Err(format!(
            "{action}: card {} has a key this plugin did not hand out: {}",
            card.id, card.key
        ));
    }
    Ok(card)
}

/// The string argument `param` of `action`, or the sentence refusing its absence or type.
fn string(action: &str, args: &Map<String, Value>, param: &str) -> Result<String, String> {
    match args.get(param) {
        Some(Value::String(value)) => Ok(value.clone()),
        Some(_) => Err(format!("{action}: parameter {param} must be a string")),
        None => Err(format!("{action}: parameter {param} is required")),
    }
}

/// The argument `param` of `action` read by `read`, `None` when it is left out or `null`, or
/// the sentence refusing a value that is not `what`.
fn optional<'a, T>(
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

/// The card id a card handle's key holds. Decoded already, so the key always parses.
fn card_id(card: &HandleArg) -> &str {
    parse_read_key(&card.key).unwrap_or(&card.key)
}

/// The ok reply carrying `handles`.
fn handles_line<F: Serialize>(handles: &[WireHandle<F>]) -> String {
    json!({ "ok": true, "handles": handles }).to_string()
}

/// The sentence of a request Trello failed.
fn trello(e: BoardError) -> String {
    e.to_string()
}

/// A board as a read finds it: Trello's id for it, which every path after the first read
/// uses, and its short link, which a handle's `board` field is.
struct Board {
    id: String,
    short_link: String,
}

/// Read the board `address` names, or say it is none: an address or a short link a call
/// named, as [`derive_board_id`] reduced it.
fn read_board(client: &TrelloClient, action: &str, address: &str) -> Result<Board, String> {
    let board = client
        .get_json(
            "read board",
            &format!("/boards/{address}"),
            &[("fields", "name,url,shortLink,closed")],
        )
        .map_err(|e| match e {
            BoardError::Status { status: 404, .. } => {
                format!("{action}: no board {address:?} on this account: {e}")
            }
            e => e.to_string(),
        })?;
    let id = str_of(&board, "id");
    if id.is_empty() {
        return Err(format!("{action}: trello read board: board has no id"));
    }
    let short = str_of(&board, "shortLink");
    Ok(Board {
        id: id.to_string(),
        short_link: if short.is_empty() { id } else { short }.to_string(),
    })
}

/// `path` read as a JSON array: what a list read asks of Trello, in `pairs`.
fn array(
    client: &TrelloClient,
    stage: &'static str,
    path: &str,
    pairs: &[(&str, &str)],
) -> Result<Vec<Value>, String> {
    match client.get_json(stage, path, pairs).map_err(trello)? {
        Value::Array(items) => Ok(items),
        _ => Err(format!(
            "trello {stage}: undecodable response (expected an array)"
        )),
    }
}

/// `items` ordered by Trello's `pos`, an item with none last and Trello's own order among
/// equals.
fn by_position(mut items: Vec<Value>) -> Vec<Value> {
    items.sort_by(|a, b| position(a).total_cmp(&position(b)));
    items
}

/// Do `call`, the read `action`, through `client`: its ok reply line, and the line that
/// narrates it.
fn run(action: &str, call: &ReadCall, client: &TrelloClient) -> Result<(String, String), String> {
    let read = |what: String| format!("{ACTION_TAG} read {what}");
    Ok(match call {
        ReadCall::Cards(query) => {
            let handles = cards(client, query)?;
            (
                handles_line(&handles),
                read(format!("{} cards of board {}", handles.len(), query.board)),
            )
        }
        ReadCall::Comments { card, limit } => {
            let actions = comment_actions(client, card_id(card), *limit)?;
            let handles: Vec<_> = actions
                .iter()
                .filter_map(|action| build_comment(action, &card.id))
                .collect();
            (
                handles_line(&handles),
                read(format!("{} comments on card {}", handles.len(), card.id)),
            )
        }
        ReadCall::Checklists(card) => {
            let checklists = checklists_of(client, card_id(card))?;
            let handles: Vec<_> = checklists
                .iter()
                .filter_map(|list| build_checklist(list, &card.id))
                .collect();
            (
                handles_line(&handles),
                read(format!("{} checklists on card {}", handles.len(), card.id)),
            )
        }
        ReadCall::CheckItems(card) => {
            let checklists = checklists_of(client, card_id(card))?;
            let handles: Vec<_> = checklists
                .iter()
                .flat_map(|list| build_check_items(list, &card.id))
                .collect();
            (
                handles_line(&handles),
                read(format!("{} check items on card {}", handles.len(), card.id)),
            )
        }
        ReadCall::Attachments(card) => {
            let listed = array(
                client,
                "read attachments",
                &format!("/cards/{}/attachments", card_id(card)),
                &[("fields", ATTACHMENT_FIELDS)],
            )?;
            let mut handles: Vec<_> = listed
                .iter()
                .filter_map(|a| build_attachment(a, card_id(card), &card.id))
                .collect();
            handles.sort_by(|a, b| (a.fields.created_at, &a.id).cmp(&(b.fields.created_at, &b.id)));
            (
                handles_line(&handles),
                read(format!("{} attachments of card {}", handles.len(), card.id)),
            )
        }
        ReadCall::DownloadAttachment { attachment, to } => {
            let saved = download(client, action, attachment, to)?;
            (json!({ "ok": true }).to_string(), saved)
        }
        ReadCall::Me => {
            let (id, username, full_name) = client.me_member("read me").map_err(trello)?;
            let me = WireHandle {
                id: username,
                key: read_key(&id),
                fields: MemberFields { full_name },
            };
            (
                json!({ "ok": true, "handle": me }).to_string(),
                read("the member the token acts as".to_string()),
            )
        }
        ReadCall::Boards => {
            let boards = array(
                client,
                "read boards",
                "/members/me/boards",
                &[("filter", "open"), ("fields", "name,url,shortLink,closed")],
            )?;
            let handles: Vec<_> = boards.iter().filter_map(build_board).collect();
            (
                handles_line(&handles),
                read(format!("{} boards", handles.len())),
            )
        }
        ReadCall::Lists { board } => {
            let found = read_board(client, action, board)?;
            let lists = array(
                client,
                "read lists",
                &format!("/boards/{}/lists", found.id),
                &[("filter", "open"), ("fields", "name,closed,pos")],
            )?;
            let handles: Vec<_> = by_position(lists)
                .iter()
                .filter_map(|list| build_list(list, &found.short_link))
                .collect();
            (
                handles_line(&handles),
                read(format!("{} lists of board {board}", handles.len())),
            )
        }
        ReadCall::Labels { board } => {
            let found = read_board(client, action, board)?;
            let labels = board_labels(client, &found.id)?;
            let handles: Vec<_> = labels
                .iter()
                .filter_map(|label| build_label(label, &found.short_link))
                .collect();
            (
                handles_line(&handles),
                read(format!("{} labels of board {board}", handles.len())),
            )
        }
        ReadCall::Members { board } => {
            let found = read_board(client, action, board)?;
            let members = board_members(client, &found.id)?;
            let handles: Vec<_> = members.iter().filter_map(build_member).collect();
            (
                handles_line(&handles),
                read(format!("{} members of board {board}", handles.len())),
            )
        }
    })
}

/// Every label of a board: Trello gives 50 unless asked, so the read asks for the 1000 it
/// allows.
fn board_labels(client: &TrelloClient, board_id: &str) -> Result<Vec<Value>, String> {
    array(
        client,
        "read labels",
        &format!("/boards/{board_id}/labels"),
        &[("limit", "1000"), ("fields", "name,color")],
    )
}

/// A board's members.
fn board_members(client: &TrelloClient, board_id: &str) -> Result<Vec<Value>, String> {
    array(
        client,
        "read members",
        &format!("/boards/{board_id}/members"),
        &[("fields", "username,fullName")],
    )
}

/// A card's checklists with their items, in the card's order.
fn checklists_of(client: &TrelloClient, card_id: &str) -> Result<Vec<Value>, String> {
    let lists = array(
        client,
        "read checklists",
        &format!("/cards/{card_id}/checklists"),
        &[("checkItems", "all"), ("fields", "name,pos")],
    )?;
    Ok(by_position(lists))
}

/// The newest `limit` comment actions of card `card_id`, oldest first, paged with Trello's
/// `before`: each page the newest the action log has below the oldest the last one held.
fn comment_actions(
    client: &TrelloClient,
    card_id: &str,
    limit: usize,
) -> Result<Vec<Value>, String> {
    let path = format!("/cards/{card_id}/actions");
    let mut actions: Vec<Value> = Vec::new();
    let mut before: Option<String> = None;
    while actions.len() < limit {
        let want = (limit - actions.len()).min(PAGE);
        let want_text = want.to_string();
        let mut pairs = vec![
            ("filter", "commentCard"),
            ("memberCreator_fields", "username,fullName"),
            ("limit", want_text.as_str()),
        ];
        if let Some(before) = &before {
            pairs.push(("before", before));
        }
        let page = array(client, "read comments", &path, &pairs)?;
        let oldest = page
            .iter()
            .filter_map(|a| a.get("id").and_then(Value::as_str))
            .min()
            .map(str::to_string);
        let full = page.len() >= want;
        actions.extend(page);
        match oldest {
            Some(oldest) if full => before = Some(oldest),
            _ => break,
        }
    }
    // Oldest first, by the ObjectId, which orders as the time it holds does.
    actions.sort_by(|a, b| str_of(a, "id").cmp(str_of(b, "id")));
    actions.dedup_by(|a, b| str_of(a, "id") == str_of(b, "id"));
    let extra = actions.len().saturating_sub(limit);
    actions.drain(..extra);
    Ok(actions)
}

/// The names `have` lists, each quoted, for a sentence naming what a board has.
fn quoted(have: &[String]) -> String {
    if have.is_empty() {
        return "none".to_string();
    }
    have.iter()
        .map(|name| format!("{name:?}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The sentence refusing a `name` of `kind` the board has none of, naming the ones it has.
fn missing(kind: &str, name: &str, have: &[String]) -> String {
    format!(
        "cards: the board has no {kind} named {name:?}; its {kind}s are {}",
        quoted(have)
    )
}

/// One list of a board, as `cards` reads it.
struct ListInfo {
    id: String,
    name: String,
    closed: bool,
}

/// The ids of the lists `names` name, in the lists `considered`: each name must be that of
/// exactly one.
///
/// # Errors
///
/// Returns the sentence for a name no list has, naming the lists there are, and for one two
/// lists share, listing their ids.
fn list_ids(names: &[String], considered: &[&ListInfo]) -> Result<HashSet<String>, String> {
    let mut have: Vec<String> = Vec::new();
    for list in considered {
        if !have.contains(&list.name) {
            have.push(list.name.clone());
        }
    }
    let mut ids = HashSet::new();
    for name in names {
        let named: Vec<&&ListInfo> = considered.iter().filter(|l| &l.name == name).collect();
        match named.as_slice() {
            [] => return Err(missing("list", name, &have)),
            [only] => {
                ids.insert(only.id.clone());
            }
            several => {
                let shared: Vec<&str> = several.iter().map(|l| l.id.as_str()).collect();
                return Err(format!(
                    "cards: {} lists are named {name:?} (ids {}); name one of them by renaming \
                     the other",
                    several.len(),
                    shared.join(", ")
                ));
            }
        }
    }
    Ok(ids)
}

/// The cards of the board `query` names that every filter of it keeps, as handles: list by
/// list in the board's order, each list top to bottom, cut at the `limit`.
///
/// Every name a filter holds is checked against the board before any card is read, so a name
/// the board lacks costs four requests and not a board's worth of pages.
fn cards(client: &TrelloClient, query: &CardQuery) -> Result<Vec<WireHandle<CardFields>>, String> {
    let action = "cards";
    let board = read_board(client, action, &query.board)?;
    let lists: Vec<ListInfo> = by_position(array(
        client,
        "read lists",
        &format!("/boards/{}/lists", board.id),
        &[("filter", "all"), ("fields", "name,closed,pos")],
    )?)
    .iter()
    .filter_map(|l| {
        Some(ListInfo {
            id: l.get("id")?.as_str()?.to_string(),
            name: str_of(l, "name").to_string(),
            closed: l.get("closed").and_then(Value::as_bool).unwrap_or(false),
        })
    })
    .collect();
    let members = board_members(client, &board.id)?;
    let labels = board_labels(client, &board.id)?;

    let mut lookups = Lookups {
        lists: lists
            .iter()
            .map(|l| (l.id.clone(), l.name.clone()))
            .collect(),
        members: HashMap::new(),
        board: board.short_link.clone(),
    };
    let mut usernames: Vec<String> = Vec::new();
    for member in &members {
        let (id, username) = (str_of(member, "id"), str_of(member, "username"));
        if !id.is_empty() && !username.is_empty() {
            lookups.members.insert(id.to_string(), username.to_string());
            usernames.push(username.to_string());
        }
    }
    let mut label_names: Vec<String> = Vec::new();
    for label in &labels {
        let name = str_of(label, "name");
        if !name.is_empty() && !label_names.iter().any(|n| n == name) {
            label_names.push(name.to_string());
        }
    }

    let considered: Vec<&ListInfo> = lists
        .iter()
        .filter(|l| query.include_archived || !l.closed)
        .collect();
    let in_lists = query
        .list
        .as_deref()
        .map(|names| list_ids(names, &considered))
        .transpose()?;
    for name in query.labels.iter().chain(&query.without_labels).flatten() {
        if !label_names.contains(name) {
            return Err(missing("label", name, &label_names));
        }
    }
    let on_cards: Option<HashSet<String>> = match &query.members {
        None => None,
        Some(wanted) => {
            let mut ids = HashSet::new();
            for who in wanted {
                if who == ME {
                    ids.insert(client.me_member("read me").map_err(trello)?.0);
                } else if let Some((id, _)) = lookups.members.iter().find(|(_, u)| *u == who) {
                    ids.insert(id.clone());
                } else {
                    return Err(missing("member", who, &usernames));
                }
            }
            Some(ids)
        }
    };

    let closed_lists: HashSet<&str> = lists
        .iter()
        .filter(|l| l.closed)
        .map(|l| l.id.as_str())
        .collect();
    let order: HashMap<&str, usize> = lists
        .iter()
        .enumerate()
        .map(|(at, l)| (l.id.as_str(), at))
        .collect();
    let mut kept: Vec<Value> = board_cards(client, &board.id, query.include_archived)?
        .into_iter()
        .filter(|card| {
            let list = str_of(card, "idList");
            let closed = card.get("closed").and_then(Value::as_bool).unwrap_or(false);
            if !query.include_archived && (closed || closed_lists.contains(list)) {
                return false;
            }
            if in_lists.as_ref().is_some_and(|ids| !ids.contains(list)) {
                return false;
            }
            let carried = parse_labels_field(card);
            if query
                .labels
                .iter()
                .flatten()
                .any(|name| !carried.contains(name))
            {
                return false;
            }
            if query
                .without_labels
                .iter()
                .flatten()
                .any(|name| carried.contains(name))
            {
                return false;
            }
            on_cards.as_ref().is_none_or(|ids| {
                card.get("idMembers")
                    .and_then(Value::as_array)
                    .is_some_and(|on| on.iter().filter_map(Value::as_str).any(|m| ids.contains(m)))
            })
        })
        .collect();
    kept.sort_by(|a, b| {
        let place = |card: &Value| {
            order
                .get(str_of(card, "idList"))
                .copied()
                .unwrap_or(usize::MAX)
        };
        place(a)
            .cmp(&place(b))
            .then_with(|| position(a).total_cmp(&position(b)))
    });
    Ok(kept
        .iter()
        .take(query.limit)
        .filter_map(|card| build_card(card, &lookups))
        .collect())
}

/// Every card of the board, open ones or — `all` — archived ones too, paged with Trello's
/// `before` past its 1000: each page the cards below the lowest id the last one held.
fn board_cards(client: &TrelloClient, board_id: &str, all: bool) -> Result<Vec<Value>, String> {
    let path = if all {
        format!("/boards/{board_id}/cards/all")
    } else {
        format!("/boards/{board_id}/cards")
    };
    let mut seen: HashSet<String> = HashSet::new();
    let mut cards: Vec<Value> = Vec::new();
    let mut before: Option<String> = None;
    loop {
        let mut pairs = vec![("limit", "1000"), ("fields", CARD_FIELDS)];
        if let Some(before) = &before {
            pairs.push(("before", before));
        }
        let page = array(client, "read cards", &path, &pairs)?;
        let full = page.len() >= PAGE;
        let lowest = page
            .iter()
            .filter_map(|c| c.get("id").and_then(Value::as_str))
            .min()
            .map(str::to_string);
        let mut fresh = 0;
        for card in page {
            if let Some(id) = card.get("id").and_then(Value::as_str) {
                if seen.insert(id.to_string()) {
                    cards.push(card);
                    fresh += 1;
                }
            }
        }
        match lowest {
            Some(lowest) if full && fresh > 0 => before = Some(lowest),
            _ => break,
        }
    }
    Ok(cards)
}

/// Save the file `attachment` names to the absolute path `to`, and say so: a file Trello
/// holds, of at most 10 MiB, fetched with the `Authorization` header Trello's download asks
/// for and written through a temp file beside `to`.
///
/// # Errors
///
/// Returns the sentence refusing a link, a file over the cap, a card or attachment Trello
/// does not have, and a path that cannot be written.
fn download(
    client: &TrelloClient,
    action: &str,
    attachment: &HandleArg,
    to: &str,
) -> Result<String, String> {
    let Some((card, id)) = parse_attachment_key(&attachment.key) else {
        return Err(format!("{action}: {} is no attachment key", attachment.key));
    };
    let listed = client
        .get_json(
            "read attachment",
            &format!("/cards/{card}/attachments/{id}"),
            &[("fields", ATTACHMENT_FIELDS)],
        )
        .map_err(|e| match e {
            BoardError::Status { status: 404, .. } => {
                format!(
                    "{action}: no attachment {} on card {card}: {e}",
                    attachment.id
                )
            }
            e => e.to_string(),
        })?;
    let Some(found) = build_attachment(&listed, card, "") else {
        return Err(format!(
            "{action}: trello read attachment: undecodable response (no id)"
        ));
    };
    let fields = &found.fields;
    if !fields.is_upload {
        return Err(format!(
            "{action}: attachment {:?} is a link to {}, not a file Trello holds",
            fields.name, fields.url
        ));
    }
    if let Some(size) = fields.bytes.filter(|&n| n > MAX_ATTACHMENT_BYTES) {
        return Err(format!(
            "{action}: attachment {:?} is {size} bytes, over the {} MiB the plugin fetches",
            fields.name,
            MAX_ATTACHMENT_BYTES / (1024 * 1024)
        ));
    }
    let bytes = client.download_attachment(&fields.url).map_err(trello)?;
    write_atomic(Path::new(to), &bytes).map_err(|e| format!("{action}: cannot write {to}: {e}"))?;
    Ok(format!(
        "{ACTION_TAG} downloading attachment {:?} into {to} ({} bytes)",
        fields.name,
        bytes.len()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A read's arguments as afkd binds them: an account and `extra`.
    fn args(extra: Value) -> Map<String, Value> {
        let mut args = Map::from_iter([(
            "acct".to_string(),
            json!({"api_key": "k", "token": "t", "base_url": "http://127.0.0.1:9/1"}),
        )]);
        args.extend(extra.as_object().expect("an args object").clone());
        args
    }

    fn call(action: &str, extra: Value) -> Result<ReadCall, String> {
        decode(action, &args(extra)).map(|(_, call)| call)
    }

    fn query(extra: Value) -> CardQuery {
        let mut full = json!({"board": "https://trello.com/b/BID/afkd"});
        full.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        match call("cards", full) {
            Ok(ReadCall::Cards(query)) => *query,
            other => panic!("{other:?}"),
        }
    }

    /// `cards` reads the board by address or short link, every filter as given, and the
    /// defaults when they are left out: archived ones kept out, the first hundred.
    #[test]
    fn cards_decodes_its_board_and_filters() {
        assert_eq!(
            query(json!({})),
            CardQuery {
                board: "BID".into(),
                list: None,
                labels: None,
                without_labels: None,
                members: None,
                include_archived: false,
                limit: 100,
            }
        );
        let by_link = call("cards", json!({"board": "1Rkelydw"})).unwrap();
        assert!(matches!(by_link, ReadCall::Cards(q) if q.board == "1Rkelydw"));
        let full = query(json!({
            "list": ["Up for Grabs", "In Progress 進行中"],
            "labels": ["afkd/ready"], "without_labels": ["Hold"], "members": ["me", "phil"],
            "include_archived": true, "limit": 1000,
        }));
        assert_eq!(
            full,
            CardQuery {
                board: "BID".into(),
                list: Some(vec!["Up for Grabs".into(), "In Progress 進行中".into()]),
                labels: Some(vec!["afkd/ready".into()]),
                without_labels: Some(vec!["Hold".into()]),
                members: Some(vec!["me".into(), "phil".into()]),
                include_archived: true,
                limit: 1000,
            }
        );
        assert_eq!(
            query(json!({"limit": 1, "include_archived": null})).limit,
            1
        );
    }

    /// The other reads decode their arguments: a card handle by the key of a card, the
    /// attachment by its card-qualified key, a board by address, the limit's default.
    #[test]
    fn every_read_decodes_its_arguments() {
        let card = json!({"id": "Qb7eLy2w", "key": "trello:6a4dd5de1234abcd5678ef90"});
        let handle = HandleArg {
            id: "Qb7eLy2w".into(),
            key: "trello:6a4dd5de1234abcd5678ef90".into(),
        };
        assert_eq!(
            call("comments", json!({"card": card})),
            Ok(ReadCall::Comments {
                card: handle.clone(),
                limit: 200
            })
        );
        assert_eq!(
            call("comments", json!({"card": card, "limit": 1})),
            Ok(ReadCall::Comments {
                card: handle.clone(),
                limit: 1
            })
        );
        assert_eq!(
            call("checklists", json!({"card": card})),
            Ok(ReadCall::Checklists(handle.clone()))
        );
        assert_eq!(
            call("check_items", json!({"card": card})),
            Ok(ReadCall::CheckItems(handle.clone()))
        );
        assert_eq!(
            call("attachments", json!({"card": card})),
            Ok(ReadCall::Attachments(handle))
        );
        let attachment = json!({"id": "a1", "key": "trello:6a4dd5de1234abcd5678ef90/a1"});
        assert_eq!(
            call(
                "download_attachment",
                json!({"attachment": attachment, "to": "/srv/計画 1/plan.md"})
            ),
            Ok(ReadCall::DownloadAttachment {
                attachment: HandleArg {
                    id: "a1".into(),
                    key: "trello:6a4dd5de1234abcd5678ef90/a1".into()
                },
                to: "/srv/計画 1/plan.md".into()
            })
        );
        assert_eq!(call("me", json!({})), Ok(ReadCall::Me));
        assert_eq!(call("boards", json!({})), Ok(ReadCall::Boards));
        let board = |b: &str| b.to_string();
        for (action, expected) in [
            (
                "lists",
                ReadCall::Lists {
                    board: board("BID"),
                },
            ),
            (
                "labels",
                ReadCall::Labels {
                    board: board("BID"),
                },
            ),
            (
                "members",
                ReadCall::Members {
                    board: board("BID"),
                },
            ),
        ] {
            assert_eq!(
                call(action, json!({"board": "https://trello.com/b/BID/afkd"})),
                Ok(expected),
                "{action}"
            );
        }
    }

    /// Each argument a read cannot use is refused in its own sentence before any request: a
    /// missing or empty account field, an empty filter, a limit outside 1 to 1000, a value of
    /// another type, a handle that is none or of another kind of key, and a relative path.
    #[test]
    fn a_read_refuses_what_it_cannot_use() {
        let card = json!({"id": "Qb7eLy2w", "key": "trello:6a4dd5de1234abcd5678ef90"});
        let board = "https://trello.com/b/BID/afkd";
        let old_key = "6a4dd5de1234abcd5678ef90#6a4dd5ff1234abcd5678ef91";
        let old_refusal =
            format!("checklists: card Qb7eLy2w has a key this plugin did not hand out: {old_key}");
        for (action, extra, refusal) in [
            ("cards", json!({}), "cards: parameter board is required"),
            (
                "cards",
                json!({"board": 7}),
                "cards: parameter board must be a string",
            ),
            (
                "cards",
                json!({"board": " "}),
                "cards: board must name a board, not \" \"",
            ),
            (
                "cards",
                json!({"board": board, "list": []}),
                "cards: `list` is empty; leave it out to keep every card",
            ),
            (
                "cards",
                json!({"board": board, "labels": []}),
                "cards: `labels` is empty; leave it out to keep every card",
            ),
            (
                "cards",
                json!({"board": board, "members": "me"}),
                "cards: parameter members must be a list of strings",
            ),
            (
                "cards",
                json!({"board": board, "list": [1]}),
                "cards: parameter list must be a list of strings",
            ),
            (
                "cards",
                json!({"board": board, "limit": 0}),
                "cards: limit 0 is outside 1 to 1000",
            ),
            (
                "cards",
                json!({"board": board, "limit": 1001}),
                "cards: limit 1001 is outside 1 to 1000",
            ),
            (
                "cards",
                json!({"board": board, "limit": "all"}),
                "cards: parameter limit must be an int",
            ),
            (
                "cards",
                json!({"board": board, "include_archived": "yes"}),
                "cards: parameter include_archived must be a bool",
            ),
            (
                "comments",
                json!({}),
                "comments: parameter card is required",
            ),
            (
                "comments",
                json!({"card": "Qb7eLy2w"}),
                "comments: parameter card must be an item handle",
            ),
            (
                "comments",
                json!({"card": {"id": "x"}}),
                "comments: parameter card must be an item handle",
            ),
            (
                "comments",
                json!({"card": card, "limit": 1001}),
                "comments: limit 1001 is outside 1 to 1000",
            ),
            (
                "comments",
                json!({"card": card, "limit": -5}),
                "comments: limit -5 is outside 1 to 1000",
            ),
            (
                "checklists",
                json!({"card": {"id": "Qb7eLy2w", "key": old_key}}),
                old_refusal.as_str(),
            ),
            (
                "attachments",
                json!({"card": {"id": "Qb7eLy2w", "key": "trello:card1/att1"}}),
                "attachments: card Qb7eLy2w has a key this plugin did not hand out: \
                 trello:card1/att1",
            ),
            (
                "download_attachment",
                json!({"attachment": {"id": "a1", "key": "trello:a1"}, "to": "/x"}),
                "download_attachment: attachment a1 has a key this plugin did not hand out: \
                 trello:a1",
            ),
            (
                "download_attachment",
                json!({"attachment": {"id": "a1", "key": "trello:c1/a1"}, "to": "plan.md"}),
                "download_attachment: to must be absolute, not \"plan.md\"",
            ),
            (
                "download_attachment",
                json!({"attachment": {"id": "a1", "key": "trello:c1/a1"}}),
                "download_attachment: parameter to is required",
            ),
            ("lists", json!({}), "lists: parameter board is required"),
            ("assign", json!({}), "no action `assign`"),
        ] {
            assert_eq!(
                call(action, extra.clone()),
                Err(refusal.to_string()),
                "{action} {extra}"
            );
        }
        // The account is read first, so its refusal comes before any other.
        let mut no_key = args(json!({}));
        no_key.insert("acct".into(), json!({"api_key": "", "token": "t"}));
        assert_eq!(
            decode("me", &no_key).map(|_| ()),
            Err("me: acct.api_key is empty".to_string())
        );
        let mut no_token = args(json!({}));
        no_token.insert("acct".into(), json!({"api_key": "k", "token": ""}));
        assert_eq!(
            decode("boards", &no_token).map(|_| ()),
            Err("boards: acct.token is empty".to_string())
        );
        no_token.remove("acct");
        assert_eq!(
            decode("boards", &no_token).map(|_| ()),
            Err("boards: parameter acct is required".to_string())
        );
    }

    /// A reply of exactly afkd's 8 MiB is sent, and one byte more is refused: `cards` and
    /// `comments` name the `limit` to lower, every other read says the reply is too large.
    #[test]
    fn a_reply_past_eight_mib_is_refused() {
        for action in READS {
            let at_cap = "x".repeat(MAX_CALL_REPLY);
            assert_eq!(fit_call(action, at_cap.clone()), Ok(at_cap), "{action}");
        }
        let over = "x".repeat(MAX_CALL_REPLY + 1);
        for action in ["cards", "comments"] {
            assert_eq!(
                fit_call(action, over.clone()),
                Err(format!(
                    "{action}: the reply comes to 8388608 bytes, over afkd's 8 MiB for one call; \
                     lower limit"
                ))
            );
        }
        for action in ["labels", "members", "attachments", "check_items", "boards"] {
            assert_eq!(
                fit_call(action, over.clone()),
                Err(format!(
                    "{action}: the reply is too large: 8388608 bytes, over afkd's 8 MiB for one \
                     call"
                ))
            );
        }
    }

    /// The lists a `list` filter names: a name two lists share is refused listing their ids,
    /// and one none has naming the lists there are, each name once.
    #[test]
    fn list_names_must_each_name_one_list() {
        let list = |id: &str, name: &str, closed: bool| ListInfo {
            id: id.into(),
            name: name.into(),
            closed,
        };
        let all = [
            list("l1", "Backlog", false),
            list("l2", "Doing", false),
            list("l3", "Doing", false),
            list("l4", "Done", true),
        ];
        let open: Vec<&ListInfo> = all.iter().filter(|l| !l.closed).collect();
        let every: Vec<&ListInfo> = all.iter().collect();
        let name = |n: &str| vec![n.to_string()];
        assert_eq!(
            list_ids(&name("Backlog"), &open),
            Ok(HashSet::from(["l1".to_string()]))
        );
        assert_eq!(
            list_ids(&["Backlog".to_string(), "Done".to_string()], &every),
            Ok(HashSet::from(["l1".to_string(), "l4".to_string()]))
        );
        assert_eq!(
            list_ids(&name("Doing"), &open),
            Err(
                "cards: 2 lists are named \"Doing\" (ids l2, l3); name one of them by renaming \
                 the other"
                    .to_string()
            )
        );
        assert_eq!(
            list_ids(&name("Done"), &open),
            Err(
                "cards: the board has no list named \"Done\"; its lists are \"Backlog\", \
                 \"Doing\""
                    .to_string()
            )
        );
        assert_eq!(
            list_ids(&name("Nope"), &[]),
            Err("cards: the board has no list named \"Nope\"; its lists are none".to_string())
        );
    }

    /// A name the board lacks is refused naming the ones it has, in the order it has them.
    #[test]
    fn a_missing_name_lists_what_there_is() {
        assert_eq!(
            missing("label", "Hold", &["afkd/ready".into(), "bug 🐛".into()]),
            "cards: the board has no label named \"Hold\"; its labels are \"afkd/ready\", \
             \"bug 🐛\""
        );
        assert_eq!(
            missing("member", "ghost", &[]),
            "cards: the board has no member named \"ghost\"; its members are none"
        );
    }
}
