//! A stateful fake Trello on a loopback socket: the REST routes the plugin's client calls,
//! over lists, cards, labels, members and comment actions the test seeds and then reads
//! back. std and `serde_json` only, and nothing from the plugin crate, so another suite
//! can `#[path]`-include it.
//!
//! It models the Trello behaviours the kind depends on: every id is a 24-hex ObjectId
//! whose first eight digits are its creation second, minted off the **real** clock (the
//! plugin judges a claim's liveness against the wall clock, so a fixture near the epoch
//! would read as stale and pass vacuously); every request authenticates with `key` and
//! `token` query parameters; comment actions come back newest first and nest into a card
//! read when `actions=commentCard` is asked for; an edited comment carries
//! `data.dateLastEdited`; the board-wide card read answers only open cards, narrowed to
//! `fields=name,labels`; a card is addressed by its id or its short link, and a card read
//! carries the board's `idBoard` and its `idLabels`; a list's cards carry a `pos` of 16384 × their place,
//! counted from one; a card created or moved (its `idList` and `pos` in the query or a
//! JSON body) lands at the top or bottom of its list, or before the first other card
//! whose `pos` is greater than the number it names, as those positions stood before the
//! move; adding a member already on the card is Trello's `400`; a comment or card that
//! does not exist is a `404`. An attachment is uploaded as a multipart form whose `name`
//! part names it, listed — or read alone by its id — with its date, size and a download
//! `url` on this fake, and downloaded from there with the OAuth `Authorization` header and
//! **no** query credential — either of those wrong is a `401`, as on Trello's download
//! host. Every request is recorded, and any route can be made to answer a status instead —
//! or the whole fake taken out of reach, every connection closed unanswered.
//!
//! For the reads a `trello.Account` makes it holds **several boards** (the default is
//! [`BOARD`]; [`FakeTrello::board`] adds more, each with the token's member on it), whose
//! lists are open or archived — two of one name allowed — and whose labels are named or not,
//! coloured or not, and as many as a test seeds, 50 given unless `limit` asks, up to 1000. A
//! card has an `idShort`, a due date, a `dateLastActivity` every write to it moves, and may
//! be archived. A board's cards are read from `/boards/{b}/cards[/{filter}]`, paged at
//! Trello's 1000 newest first with `before` when a `limit` or a `before` is asked, and a
//! card's comment actions from `/cards/{c}/actions`, newest first, 50 unless `limit` asks, up
//! to 1000, from below `before`. A card read nests its list, its board and its members when
//! asked. Checklists come back in `pos` order. A route can be told to answer a 429 some
//! number of times, with the rate-limit headers or without.
//!
//! For the writes a `trello.Account` makes it takes every one. A card read serves an
//! archived card as Trello does, with `closed: true`, and answers `404` only for a card it
//! does not have; it honours `fields` (keeping what it nests, and the checklists when
//! `checklists=all` asks) beside the nesting. `PUT /cards/{id}` with a JSON body is an
//! `update card`: it applies `name`, `desc`, `closed`, `dueComplete`, `idList` with `pos`
//! (placed as a move is; a `pos` alone places the card in the list it is in) and `due` —
//! an RFC 3339 string, kept to the millisecond, or `null` to clear it — while the
//! query-parameter `move card`, `archive card` and `complete card` forms stay as they were.
//! `POST /cards` also takes `idLabels`, `idMembers` and `due` from its body, and
//! `DELETE /cards/{id}` removes a card with its comments and attachments. A comment action
//! is read, edited and deleted by its id alone (`/actions/{id}`), and every comment action
//! carries `data.card`, the card's `id` and `shortLink`; an edit or a delete of a comment
//! another member wrote is Trello's `401 unauthorized permission requested`. `POST
//! /cards/{id}/checklists` makes a checklist, `GET /checklists/{id}` reads one — honouring
//! `fields`, `idCard` among them, and `checkItems` — `DELETE /checklists/{id}` removes it,
//! and `POST /checklists/{id}/checkItems` adds an item at `pos` top or bottom. An item is
//! changed (`name`, `state`) and removed by `/cards/{card}/checkItem/{item}`, and an
//! attachment removed by `DELETE /cards/{card}/attachments/{id}`. Each write is a route of
//! its own to fail or throttle: `update card`, `delete card`, `read action`, `update
//! action`, `delete action`, `create checklist`, `read checklist`, `delete checklist`,
//! `create check item`, `update check item`, `delete check item` and `delete attachment`.

#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

/// The API key every request must carry as `?key=`.
pub const KEY: &str = "trello-key-7f3a";

/// The token every request must carry as `?token=`.
pub const TOKEN: &str = "trello-token-9c1e";

/// The board every path names, as a board address's `/b/<id>/` segment spells it.
pub const BOARD: &str = "1Rkelydw";

/// The member the key and token authenticate as — afkd's own.
pub const ME: &str = "afkd-bot";

/// A check item as `(id, name, complete)`.
pub type ItemRow = (String, String, bool);

/// A checklist as `(id, name, items)`.
pub type ChecklistRow = (String, String, Vec<ItemRow>);

/// One request as the fake saw it.
#[derive(Debug, Clone)]
pub struct Seen {
    pub method: String,
    /// The path as it crossed the wire, without the query.
    pub path: String,
    /// The decoded query pairs.
    pub query: Vec<(String, String)>,
    /// The body as text, any invalid UTF-8 replaced.
    pub body: String,
    /// The body as it crossed the wire.
    pub raw: Vec<u8>,
    pub content_type: Option<String>,
    pub authorization: Option<String>,
}

/// One part of a multipart form, as the fake reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    /// The `name` its Content-Disposition gives.
    pub name: String,
    /// The `filename` its Content-Disposition gives, if any.
    pub filename: Option<String>,
    pub bytes: Vec<u8>,
}

/// An attachment on a card: an uploaded file, or a link.
#[derive(Debug, Clone)]
struct Attachment {
    id: String,
    card: String,
    name: String,
    /// The creation second, as the ObjectId carries it.
    posted: u64,
    /// The millisecond within it, which the listing's `date` carries and the id does not.
    millis: u32,
    /// The file's bytes; empty for a link.
    bytes: Vec<u8>,
    /// Where a link points; `None` for an upload, which this fake serves.
    link: Option<String>,
}

/// A comment action on a card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    pub id: String,
    pub card: String,
    /// The member id of whoever posted it.
    pub author: String,
    pub text: String,
    /// The creation second, as the ObjectId carries it.
    pub posted: u64,
    /// The last edit's second, once edited.
    pub edited: Option<u64>,
}

#[derive(Debug, Clone)]
struct Board {
    id: String,
    short_link: String,
    name: String,
    closed: bool,
    /// The last `idShort` a card of the board was given.
    cards: u32,
}

#[derive(Debug, Clone)]
struct Member {
    id: String,
    username: String,
    full_name: String,
    /// The ids of the boards the member is on.
    boards: Vec<String>,
}

#[derive(Debug, Clone)]
struct List {
    id: String,
    name: String,
    board: String,
    closed: bool,
}

#[derive(Debug, Clone)]
struct Label {
    id: String,
    name: String,
    color: Option<String>,
    board: String,
}

#[derive(Debug, Clone)]
struct CheckItem {
    id: String,
    name: String,
    done: bool,
    pos: f64,
}

#[derive(Debug, Clone)]
struct Checklist {
    id: String,
    name: String,
    items: Vec<CheckItem>,
    pos: f64,
}

#[derive(Debug, Clone)]
struct Card {
    id: String,
    short_link: String,
    name: String,
    desc: String,
    list: String,
    board: String,
    /// Trello's `idShort`, counted from one on each board.
    number: u32,
    labels: Vec<String>,
    members: Vec<String>,
    checklists: Vec<Checklist>,
    closed: bool,
    due_complete: bool,
    /// The due date, in seconds.
    due: Option<u64>,
    /// The millisecond within the due second, which a due date written to the millisecond
    /// keeps.
    due_millis: u32,
    /// The second `dateLastActivity` carries: when the card was made, and then when a write
    /// last touched it.
    last_activity: u64,
}

/// Where a created or moved card lands in its list.
#[derive(Debug, Clone, Copy)]
enum Pos {
    Top,
    Bottom,
    /// Before the first other card whose `pos` is greater.
    At(f64),
}

impl Pos {
    /// Read `pos` as a query parameter (a string) or a body field (a string or a number);
    /// anything else is `default`.
    fn read(value: Option<&Value>, default: Self) -> Self {
        match value {
            Some(Value::String(word)) if word == "top" => Self::Top,
            Some(Value::String(word)) if word == "bottom" => Self::Bottom,
            Some(Value::String(word)) => word.parse().map_or(default, Self::At),
            Some(Value::Number(n)) => n.as_f64().map_or(default, Self::At),
            _ => default,
        }
    }
}

#[derive(Default)]
struct State {
    me: String,
    counter: u64,
    /// The id of the default board, [`BOARD`].
    board: String,
    boards: Vec<Board>,
    members: Vec<Member>,
    lists: Vec<List>,
    labels: Vec<Label>,
    /// In board order: a list's cards are the ones naming it, in this order.
    cards: Vec<Card>,
    comments: Vec<Comment>,
    attachments: Vec<Attachment>,
    /// An attachment id → the size its listing says, for a file too large to hold.
    sizes: HashMap<String, u64>,
    /// The API root, which an upload's download `url` is built on.
    base: String,
    /// A route name → the status it answers instead.
    faults: HashMap<String, u16>,
    /// A route name → how many more requests answer a 429, and the rate-limit interval, in
    /// milliseconds, its headers carry — none when they carry none.
    throttles: HashMap<String, (u32, Option<u64>)>,
    /// The headers the reply being built carries beside the usual ones.
    reply_headers: Vec<(String, String)>,
    /// A comment post whose text contains this answers the status instead.
    post_faults: Vec<(String, u16)>,
    /// Cards on which the next claim post is raced by a rival.
    races: HashSet<String>,
    seen: Vec<Seen>,
    /// Whether the fake is out of reach: every request is read, recorded in `dropped`,
    /// and its connection closed with no reply.
    down: bool,
    dropped: Vec<Seen>,
}

/// The current wall-clock second.
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl State {
    /// A fresh ObjectId stamped with `secs`.
    fn mint(&mut self, secs: u64) -> String {
        self.counter += 1;
        format!("{secs:08x}{:016x}", self.counter)
    }

    fn member(&self, id: &str) -> Option<&Member> {
        self.members.iter().find(|m| m.id == id)
    }

    fn member_json(&self, id: &str) -> Value {
        let m = self.member(id);
        json!({
            "id": id,
            "username": m.map_or("", |m| m.username.as_str()),
            "fullName": m.map_or("", |m| m.full_name.as_str()),
        })
    }

    fn label_json(&self, id: &str) -> Value {
        let label = self.labels.iter().find(|l| l.id == id);
        json!({
            "id": id,
            "name": label.map_or("", |l| l.name.as_str()),
            "color": label.and_then(|l| l.color.clone()),
            "idBoard": label.map_or("", |l| l.board.as_str()),
        })
    }

    /// The card and the place on it of the checklist `id`.
    fn checklist_at(&self, id: &str) -> Option<(usize, usize)> {
        self.cards.iter().enumerate().find_map(|(card, c)| {
            c.checklists
                .iter()
                .position(|cl| cl.id == id)
                .map(|list| (card, list))
        })
    }

    /// The board named by its id or its short link.
    fn board_at(&self, which: &str) -> Option<usize> {
        self.boards
            .iter()
            .position(|b| b.id == which || b.short_link == which)
    }

    /// A board as Trello's board read gives it.
    fn board_json(&self, b: &Board) -> Value {
        json!({
            "id": b.id,
            "name": b.name,
            "shortLink": b.short_link,
            "url": format!("https://trello.com/b/{}/{}", b.short_link, slug(&b.name)),
            "closed": b.closed,
        })
    }

    /// A list as Trello's list read gives it: its `pos` 16384 × its place among its board's
    /// lists, archived ones counted.
    fn list_json(&self, list: &List) -> Value {
        let place = self
            .lists
            .iter()
            .filter(|l| l.board == list.board)
            .position(|l| l.id == list.id)
            .unwrap_or(0);
        json!({
            "id": list.id,
            "name": list.name,
            "closed": list.closed,
            "idBoard": list.board,
            "pos": 16_384.0 * (place as f64 + 1.0),
        })
    }

    fn comment_json(&self, c: &Comment) -> Value {
        // Every comment action names the card it is on, as Trello's does.
        let card = self.cards.iter().find(|card| card.id == c.card).map_or(
            json!({ "id": c.card }),
            |card| json!({ "id": card.id, "shortLink": card.short_link }),
        );
        let mut data = json!({ "text": c.text, "card": card });
        if let Some(edited) = c.edited {
            data["dateLastEdited"] = json!(stamp(edited));
        }
        json!({
            "id": c.id,
            "type": "commentCard",
            "idMemberCreator": c.author,
            "date": stamp(c.posted),
            "data": data,
            "memberCreator": self.member_json(&c.author),
        })
    }

    /// A card's comment actions, newest first, as Trello answers them.
    fn thread(&self, card: &str) -> Vec<Value> {
        self.actions_page(card, None, usize::MAX)
    }

    /// At most `limit` of a card's comment actions, newest first, those below the action id
    /// `before` when one is named.
    fn actions_page(&self, card: &str, before: Option<&str>, limit: usize) -> Vec<Value> {
        let mut thread: Vec<&Comment> = self
            .comments
            .iter()
            .filter(|c| c.card == card && before.is_none_or(|b| c.id.as_str() < b))
            .collect();
        thread.sort_by(|a, b| b.id.cmp(&a.id));
        thread
            .into_iter()
            .take(limit)
            .map(|c| self.comment_json(c))
            .collect()
    }

    /// Mark the write that just touched card `card` (its id or short link): its
    /// `dateLastActivity` is now.
    fn touch(&mut self, card: &str) {
        let now = now();
        if let Some(card) = self
            .cards
            .iter_mut()
            .find(|c| c.id == card || c.short_link == card)
        {
            card.last_activity = card.last_activity.max(now);
        }
    }

    /// The open cards of `list`, each as its index into `cards` and the `pos` the fake
    /// derives for it: 16384 × its place in the list, counted from one.
    fn positions(&self, list: &str) -> Vec<(usize, f64)> {
        self.cards
            .iter()
            .enumerate()
            .filter(|(_, c)| c.list == list && !c.closed)
            .zip(1u32..)
            .map(|((at, _), n)| (at, 16_384.0 * f64::from(n)))
            .collect()
    }

    /// Put `card` into its list at `pos`, taking it out of `cards` at `from` first when it
    /// is already there. A numeric `pos` is resolved against the positions from before the
    /// card is taken out, so a card moved down its own list lands where the number says.
    fn place(&mut self, card: Card, from: Option<usize>, pos: Pos) {
        let others: Vec<(String, f64)> = self
            .positions(&card.list)
            .into_iter()
            .filter(|&(at, _)| Some(at) != from)
            .map(|(at, p)| (self.cards[at].id.clone(), p))
            .collect();
        if let Some(from) = from {
            self.cards.remove(from);
        }
        let index_of = |s: &Self, id: &str| s.cards.iter().position(|c| c.id == id);
        match pos {
            Pos::Top => self.cards.insert(0, card),
            Pos::Bottom => self.cards.push(card),
            Pos::At(n) => match others.iter().find(|(_, p)| *p > n) {
                Some((next, _)) => {
                    let at = index_of(self, next).expect("a card in the list");
                    self.cards.insert(at, card);
                }
                None => match others.last() {
                    Some((last, _)) => {
                        let at = index_of(self, last).expect("a card in the list");
                        self.cards.insert(at + 1, card);
                    }
                    None => self.cards.push(card),
                },
            },
        }
    }

    /// An attachment as Trello lists it: an upload's `url` is its download on this fake,
    /// named by a slug of its name, and a link's is where it points.
    fn attachment_json(&self, a: &Attachment) -> Value {
        let url = a.link.clone().unwrap_or_else(|| {
            let file: String = a
                .name
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '.' {
                        c
                    } else {
                        '_'
                    }
                })
                .collect();
            format!(
                "{}/cards/{}/attachments/{}/download/{file}",
                self.base, a.card, a.id
            )
        });
        json!({
            "id": a.id,
            "name": a.name,
            "date": stamp(a.posted).replace(".000Z", &format!(".{:03}Z", a.millis)),
            "url": url,
            "isUpload": a.link.is_none(),
            "bytes": if a.link.is_none() {
                json!(self.sizes.get(&a.id).copied().unwrap_or(a.bytes.len() as u64))
            } else {
                Value::Null
            },
            "mimeType": if a.link.is_none() { "application/octet-stream" } else { "" },
        })
    }

    /// A checklist as Trello gives it, its items in the order they were put on it.
    fn checklist_json(&self, cl: &Checklist, card: &str) -> Value {
        json!({
            "id": cl.id,
            "idCard": card,
            "name": cl.name,
            "pos": cl.pos,
            "checkItems": cl.items.iter().map(|item| json!({
                "id": item.id,
                "name": item.name,
                "pos": item.pos,
                "state": if item.done { "complete" } else { "incomplete" },
            })).collect::<Vec<_>>(),
        })
    }

    fn card_json(&self, card: &Card, with_actions: bool) -> Value {
        let mut value = json!({
            "id": card.id,
            "idBoard": card.board,
            "shortLink": card.short_link,
            "idShort": card.number,
            "name": card.name,
            "url": card_url(card),
            "desc": card.desc,
            "idList": card.list,
            "idMembers": card.members,
            "idLabels": card.labels,
            "labels": card.labels.iter().map(|l| self.label_json(l)).collect::<Vec<_>>(),
            "closed": card.closed,
            "due": card.due.map(|secs| {
                stamp(secs).replace(".000Z", &format!(".{:03}Z", card.due_millis))
            }),
            "dueComplete": card.due_complete,
            "dateLastActivity": stamp(card.last_activity),
            "checklists": card.checklists.iter().map(|cl| self.checklist_json(cl, &card.id)).collect::<Vec<_>>(),
        });
        if with_actions {
            value["actions"] = Value::Array(self.thread(&card.id));
        }
        value
    }

    /// A card with its `pos` among the cards of its list, as a board read gives it: 16384 ×
    /// its place in the list, counted from one, archived cards counted too.
    fn positioned_json(&self, card: &Card) -> Value {
        let place = self
            .cards
            .iter()
            .filter(|c| c.list == card.list)
            .position(|c| c.id == card.id)
            .unwrap_or(0);
        let mut value = self.card_json(card, false);
        value["pos"] = json!(16_384.0 * (place as f64 + 1.0));
        value
    }

    /// `value` cut to the fields `fields` names (comma separated) and its `id`, as a
    /// `fields=` parameter asks; whole for none, or `all`.
    fn only(value: Value, fields: Option<&str>) -> Value {
        let Some(fields) = fields.filter(|f| *f != "all") else {
            return value;
        };
        let wanted: HashSet<&str> = fields.split(',').collect();
        match value {
            Value::Object(map) => Value::Object(
                map.into_iter()
                    .filter(|(key, _)| key == "id" || wanted.contains(key.as_str()))
                    .collect(),
            ),
            other => other,
        }
    }

    /// The card read nested with its list, its board and its members when asked, each cut
    /// to the `*_fields` asked.
    fn nested_json(&self, card: &Card, query: &dyn Fn(&str) -> Option<String>) -> Value {
        let mut value = self.card_json(card, query("actions").as_deref() == Some("commentCard"));
        let yes = |key: &str| query(key).as_deref() == Some("true");
        if yes("list") {
            let list = self.lists.iter().find(|l| l.id == card.list);
            value["list"] = list.map_or(Value::Null, |l| {
                Self::only(self.list_json(l), query("list_fields").as_deref())
            });
        }
        if yes("board") {
            let board = self.boards.iter().find(|b| b.id == card.board);
            value["board"] = board.map_or(Value::Null, |b| {
                Self::only(self.board_json(b), query("board_fields").as_deref())
            });
        }
        if yes("members") {
            value["members"] = card
                .members
                .iter()
                .map(|m| Self::only(self.member_json(m), query("member_fields").as_deref()))
                .collect();
        }
        value
    }
}

/// `name` as a URL slug: lowercased ASCII letters and digits, every other run a single `-`.
fn slug(name: &str) -> String {
    let mut slug = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.trim_end_matches('-').to_string()
}

/// A card's address as Trello's default card fields carry it: the short link, then a slug
/// of the name — lowercased ASCII letters and digits, every other run a single `-`.
fn card_url(card: &Card) -> String {
    let mut slug = String::new();
    for c in card.name.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    format!(
        "https://trello.com/c/{}/{}",
        card.short_link,
        slug.trim_end_matches('-')
    )
}

/// The fake. Dropping it leaves the listener thread parked on `accept`; the process ends
/// it.
#[derive(Clone)]
pub struct FakeTrello {
    base: String,
    state: Arc<Mutex<State>>,
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl FakeTrello {
    /// A fake whose key and token authenticate as [`ME`], listening on `127.0.0.1:0`.
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let base = format!("http://{}/1", listener.local_addr().expect("local addr"));
        let state = Arc::new(Mutex::new(State::default()));
        {
            let mut s = lock(&state);
            s.base = base.clone();
            let board = s.mint(now() - 500 * 86_400);
            s.boards.push(Board {
                id: board.clone(),
                short_link: BOARD.to_string(),
                name: "afkd".to_string(),
                closed: false,
                cards: 0,
            });
            let me = s.mint(now() - 400 * 86_400);
            s.members.push(Member {
                id: me.clone(),
                username: ME.to_string(),
                full_name: "afkd".to_string(),
                boards: vec![board.clone()],
            });
            s.me = me;
            s.board = board;
        }
        let shared = Arc::clone(&state);
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let state = Arc::clone(&shared);
                thread::spawn(move || serve(stream, &state));
            }
        });
        Self { base, state }
    }

    /// The API root, as a config's `base_url` writes it.
    pub fn base_url(&self) -> &str {
        &self.base
    }

    /// The `trello.Account` that reaches this fake with its key and token, as afkd binds the
    /// struct a read is passed.
    pub fn account(&self) -> Value {
        json!({ "api_key": KEY, "token": TOKEN, "base_url": self.base })
    }

    /// afkd's own member id.
    pub fn me(&self) -> String {
        lock(&self.state).me.clone()
    }

    /// Add a board `name` addressed by `short_link`, with the token's member on it; returns
    /// its id. [`BOARD`] is the one a fake starts with, and the one every method that names no
    /// board means.
    pub fn board(&self, short_link: &str, name: &str) -> String {
        let mut s = lock(&self.state);
        let id = s.mint(now() - 450 * 86_400);
        s.boards.push(Board {
            id: id.clone(),
            short_link: short_link.to_string(),
            name: name.to_string(),
            closed: false,
            cards: 0,
        });
        let me = s.me.clone();
        if let Some(me) = s.members.iter_mut().find(|m| m.id == me) {
            me.boards.push(id.clone());
        }
        id
    }

    /// The id of the board addressed by `short_link`, which the paths after the first read name.
    pub fn board_id(&self, short_link: &str) -> String {
        let s = lock(&self.state);
        let at = s.board_at(short_link).expect("a seeded board");
        s.boards[at].id.clone()
    }

    /// Rewrite the comment `comment` (its action id) to `text`, as an edit does: it is stamped
    /// with its last edit, now.
    pub fn edit_comment(&self, comment: &str, text: &str) {
        let mut s = lock(&self.state);
        let at = s
            .comments
            .iter()
            .position(|c| c.id == comment)
            .expect("a seeded comment");
        s.comments[at].text = text.to_string();
        s.comments[at].edited = Some(now());
        let card = s.comments[at].card.clone();
        s.touch(&card);
    }

    /// Close the board addressed by `short_link`: Trello lists no closed board among the
    /// ones a member can reach.
    pub fn close_board(&self, short_link: &str) {
        let mut s = lock(&self.state);
        let at = s.board_at(short_link).expect("a seeded board");
        s.boards[at].closed = true;
    }

    /// Register a member of the default board; returns its id.
    pub fn member(&self, username: &str, full_name: &str) -> String {
        self.member_on(BOARD, username, full_name)
    }

    /// Register the member `username`, or find them, and put them on the board addressed by
    /// `board`; returns their id.
    pub fn member_on(&self, board: &str, username: &str, full_name: &str) -> String {
        let mut s = lock(&self.state);
        let at = s.board_at(board).expect("a seeded board");
        let board = s.boards[at].id.clone();
        if let Some(m) = s.members.iter_mut().find(|m| m.username == username) {
            if !m.boards.contains(&board) {
                m.boards.push(board);
            }
            return m.id.clone();
        }
        let id = s.mint(now() - 300 * 86_400);
        s.members.push(Member {
            id: id.clone(),
            username: username.to_string(),
            full_name: full_name.to_string(),
            boards: vec![board],
        });
        id
    }

    /// Add a list to the default board; returns its id.
    pub fn list(&self, name: &str) -> String {
        self.list_on(BOARD, name)
    }

    /// Add a list to the board addressed by `board`, after the lists it has, archived ones
    /// among them; returns its id. Two lists may share a name.
    pub fn list_on(&self, board: &str, name: &str) -> String {
        let mut s = lock(&self.state);
        let at = s.board_at(board).expect("a seeded board");
        let board = s.boards[at].id.clone();
        let id = s.mint(now() - 200 * 86_400);
        s.lists.push(List {
            id: id.clone(),
            name: name.to_string(),
            board,
            closed: false,
        });
        id
    }

    /// The id of the first open list named `name` on the default board.
    pub fn list_id(&self, name: &str) -> String {
        let s = lock(&self.state);
        s.lists
            .iter()
            .find(|l| l.name == name && !l.closed && l.board == s.board)
            .map(|l| l.id.clone())
            .expect("a seeded list")
    }

    /// Archive the list `list` (its id): its cards stay open, and Trello lists it no more
    /// among the open lists.
    pub fn archive_list(&self, list: &str) {
        let mut s = lock(&self.state);
        s.lists
            .iter_mut()
            .find(|l| l.id == list)
            .expect("a seeded list")
            .closed = true;
    }

    /// Add a card, created a week ago, at the bottom of the open list named `list` on the
    /// default board; returns its id.
    pub fn card(&self, list: &str, short_link: &str, name: &str, desc: &str) -> String {
        let list = {
            let s = lock(&self.state);
            s.lists
                .iter()
                .find(|l| l.name == list && !l.closed && l.board == s.board)
                .map(|l| l.id.clone())
                .expect("a seeded list")
        };
        self.card_in(&list, short_link, name, desc)
    }

    /// Add a card, created a week ago, at the bottom of the list `list` (its id), on that
    /// list's board; returns its id.
    pub fn card_in(&self, list: &str, short_link: &str, name: &str, desc: &str) -> String {
        let mut s = lock(&self.state);
        let board = s
            .lists
            .iter()
            .find(|l| l.id == list)
            .map(|l| l.board.clone())
            .expect("a seeded list");
        let at = s.board_at(&board).expect("the list's board");
        s.boards[at].cards += 1;
        let number = s.boards[at].cards;
        let created = now() - 7 * 86_400;
        let id = s.mint(created);
        s.cards.push(Card {
            id: id.clone(),
            short_link: short_link.to_string(),
            name: name.to_string(),
            desc: desc.to_string(),
            list: list.to_string(),
            board,
            number,
            labels: Vec::new(),
            members: Vec::new(),
            checklists: Vec::new(),
            closed: false,
            due_complete: false,
            due: None,
            due_millis: 0,
            last_activity: created,
        });
        id
    }

    /// Archive a card, as Trello's Archive does: it is `closed`, and no list holds it.
    pub fn archive(&self, card: &str) {
        let mut s = lock(&self.state);
        s.cards
            .iter_mut()
            .find(|c| c.id == card)
            .expect("a seeded card")
            .closed = true;
    }

    /// Move a card to the bottom of the open list named `list` on its board.
    pub fn move_card(&self, card: &str, list: &str) {
        let mut s = lock(&self.state);
        let at = s
            .cards
            .iter()
            .position(|c| c.id == card)
            .expect("a seeded card");
        let board = s.cards[at].board.clone();
        let list = s
            .lists
            .iter()
            .find(|l| l.name == list && !l.closed && l.board == board)
            .map(|l| l.id.clone())
            .expect("a seeded list");
        let mut moved = s.cards[at].clone();
        moved.list = list;
        s.place(moved, Some(at), Pos::Bottom);
        s.touch(card);
    }

    /// The second a card is due, when it has a due date.
    pub fn due_of(&self, card: &str) -> Option<u64> {
        let s = lock(&self.state);
        s.cards
            .iter()
            .find(|c| c.id == card)
            .expect("a seeded card")
            .due
    }

    /// The second a card's `dateLastActivity` is, which every write to the card moves.
    pub fn last_activity(&self, card: &str) -> u64 {
        let s = lock(&self.state);
        s.cards
            .iter()
            .find(|c| c.id == card)
            .expect("a seeded card")
            .last_activity
    }

    /// Give a card a due date `secs` seconds from now, a negative number being past, and
    /// say whether it is marked complete.
    pub fn due(&self, card: &str, secs: i64, complete: bool) {
        let mut s = lock(&self.state);
        let card = s
            .cards
            .iter_mut()
            .find(|c| c.id == card)
            .expect("a seeded card");
        card.due = Some(now().saturating_add_signed(secs));
        card.due_millis = 0;
        card.due_complete = complete;
    }

    /// The millisecond a card is due, when it has a due date: the second and the fraction
    /// of it a write kept.
    pub fn due_millis_of(&self, card: &str) -> Option<u64> {
        let s = lock(&self.state);
        let card = s
            .cards
            .iter()
            .find(|c| c.id == card)
            .expect("a seeded card");
        card.due
            .map(|secs| secs * 1000 + u64::from(card.due_millis))
    }

    /// Whether the fake still holds the card `card` (its id), archived or not.
    pub fn has_card(&self, card: &str) -> bool {
        lock(&self.state).cards.iter().any(|c| c.id == card)
    }

    /// Whether the fake still holds the comment `comment` (its action id).
    pub fn has_comment(&self, comment: &str) -> bool {
        lock(&self.state).comments.iter().any(|c| c.id == comment)
    }

    /// Whether the fake still holds the attachment `attachment` (its id).
    pub fn has_attachment(&self, attachment: &str) -> bool {
        lock(&self.state)
            .attachments
            .iter()
            .any(|a| a.id == attachment)
    }

    /// A card's checklists as `(id, name, items)`, each item `(id, name, complete)`, in
    /// the order they were put on the card, items in the order they were put on theirs.
    pub fn checklists_on(&self, card: &str) -> Vec<ChecklistRow> {
        let s = lock(&self.state);
        let card = s
            .cards
            .iter()
            .find(|c| c.id == card)
            .expect("a seeded card");
        card.checklists
            .iter()
            .map(|cl| {
                (
                    cl.id.clone(),
                    cl.name.clone(),
                    cl.items
                        .iter()
                        .map(|i| (i.id.clone(), i.name.clone(), i.done))
                        .collect(),
                )
            })
            .collect()
    }

    /// Put a member (their id) on a card.
    pub fn card_member(&self, card: &str, member: &str) {
        let mut s = lock(&self.state);
        let card = s
            .cards
            .iter_mut()
            .find(|c| c.id == card)
            .expect("a seeded card");
        if !card.members.iter().any(|m| m == member) {
            card.members.push(member.to_string());
        }
    }

    /// Put a checklist on a card, its items as `(text, complete)`; returns the item ids.
    pub fn checklist(&self, card: &str, name: &str, items: &[(&str, bool)]) -> Vec<String> {
        let next = {
            let s = lock(&self.state);
            let card = s
                .cards
                .iter()
                .find(|c| c.id == card)
                .expect("a seeded card");
            16_384.0 * (card.checklists.len() as f64 + 1.0)
        };
        self.checklist_at(card, name, next, items)
    }

    /// Put a checklist at `pos` on a card, its items as `(text, complete)` at 16384 × their
    /// place; returns the item ids. Trello lists checklists by `pos`, whatever order they
    /// were made in.
    pub fn checklist_at(
        &self,
        card: &str,
        name: &str,
        pos: f64,
        items: &[(&str, bool)],
    ) -> Vec<String> {
        let mut s = lock(&self.state);
        let secs = now();
        let id = s.mint(secs);
        let items: Vec<CheckItem> = items
            .iter()
            .zip(1u32..)
            .map(|((text, done), n)| CheckItem {
                id: s.mint(secs),
                name: text.to_string(),
                done: *done,
                pos: 16_384.0 * f64::from(n),
            })
            .collect();
        let ids = items.iter().map(|item| item.id.clone()).collect();
        s.cards
            .iter_mut()
            .find(|c| c.id == card)
            .expect("a seeded card")
            .checklists
            .push(Checklist {
                id,
                name: name.to_string(),
                items,
                pos,
            });
        s.touch(card);
        ids
    }

    /// Move the item `item` of a card's checklist to `pos`, so its place in the checklist is
    /// not the one it was made in.
    pub fn move_check_item(&self, item: &str, pos: f64) {
        let mut s = lock(&self.state);
        for card in &mut s.cards {
            for list in &mut card.checklists {
                for it in list.items.iter_mut().filter(|i| i.id == item) {
                    it.pos = pos;
                }
            }
        }
    }

    /// The address the fake serves as the card's `url`.
    pub fn url(&self, card: &str) -> String {
        let s = lock(&self.state);
        card_url(
            s.cards
                .iter()
                .find(|c| c.id == card)
                .expect("a seeded card"),
        )
    }

    /// Rewrite a card's description, as a person does once the files it will link are on the
    /// card and their urls known.
    pub fn describe(&self, card: &str, desc: &str) {
        let mut s = lock(&self.state);
        s.cards
            .iter_mut()
            .find(|c| c.id == card)
            .expect("a seeded card")
            .desc = desc.to_string();
        s.touch(card);
    }

    /// The url the fake serves an upload's bytes at — the one its attachments list names,
    /// which a description or a comment can link as well.
    pub fn download_url(&self, attachment: &str) -> String {
        let s = lock(&self.state);
        let a = s
            .attachments
            .iter()
            .find(|a| a.id == attachment)
            .expect("a seeded attachment");
        s.attachment_json(a)["url"]
            .as_str()
            .expect("a url")
            .to_string()
    }

    /// Label a card with the label of its board named `name`, creating it if need be. An
    /// empty `name` is a colour-only label, as Trello sends one, and is green.
    pub fn label(&self, card: &str, name: &str) {
        let mut s = lock(&self.state);
        let board = s
            .cards
            .iter()
            .find(|c| c.id == card)
            .map(|c| c.board.clone())
            .expect("a seeded card");
        let label = match s.labels.iter().find(|l| l.name == name && l.board == board) {
            Some(l) => l.id.clone(),
            None => {
                let id = s.mint(now());
                s.labels.push(Label {
                    id: id.clone(),
                    name: name.to_string(),
                    color: name.is_empty().then(|| "green".to_string()),
                    board,
                });
                id
            }
        };
        let card = s
            .cards
            .iter_mut()
            .find(|c| c.id == card)
            .expect("a seeded card");
        if !card.labels.contains(&label) {
            card.labels.push(label);
        }
    }

    /// Add a label to the board addressed by `board`, named or not (`""`) and coloured or not
    /// (a Trello colour such as `green`), without putting it on a card; returns its id.
    pub fn board_label(&self, board: &str, name: &str, color: Option<&str>) -> String {
        let mut s = lock(&self.state);
        let at = s.board_at(board).expect("a seeded board");
        let board = s.boards[at].id.clone();
        let id = s.mint(now());
        s.labels.push(Label {
            id: id.clone(),
            name: name.to_string(),
            color: color.map(str::to_string),
            board,
        });
        id
    }

    /// Put the label `label` (its id) on a card.
    pub fn label_card(&self, card: &str, label: &str) {
        let mut s = lock(&self.state);
        let card = s
            .cards
            .iter_mut()
            .find(|c| c.id == card)
            .expect("a seeded card");
        if !card.labels.iter().any(|l| l == label) {
            card.labels.push(label.to_string());
        }
    }

    /// Seed a comment by member `author`, posted `ago` seconds before now; returns its id.
    pub fn comment(&self, card: &str, author: &str, text: &str, ago: u64) -> String {
        let mut s = lock(&self.state);
        let posted = now() - ago;
        let id = s.mint(posted);
        s.comments.push(Comment {
            id: id.clone(),
            card: card.to_string(),
            author: author.to_string(),
            text: text.to_string(),
            posted,
            edited: None,
        });
        s.touch(card);
        id
    }

    /// Put an uploaded file named `name` holding `bytes` on a card, `ago` seconds before
    /// now; returns its id.
    pub fn attachment(&self, card: &str, name: &str, bytes: &[u8], ago: u64) -> String {
        self.attachment_at(card, name, bytes, ago, 0)
    }

    /// [`attachment`](Self::attachment), at `millis` into its second. Ids are minted in
    /// the order seeded, so seeding the later millisecond first gives two uploads in one
    /// second whose ids run against their times — as Trello's, minted on different
    /// servers, can.
    pub fn attachment_at(
        &self,
        card: &str,
        name: &str,
        bytes: &[u8],
        ago: u64,
        millis: u32,
    ) -> String {
        let mut s = lock(&self.state);
        let posted = now() - ago;
        let id = s.mint(posted);
        s.attachments.push(Attachment {
            id: id.clone(),
            card: card.to_string(),
            name: name.to_string(),
            posted,
            millis,
            bytes: bytes.to_vec(),
            link: None,
        });
        id
    }

    /// Put a link named `name` to `url` on a card, `ago` seconds before now; returns its id.
    pub fn link_attachment(&self, card: &str, name: &str, url: &str, ago: u64) -> String {
        let mut s = lock(&self.state);
        let posted = now() - ago;
        let id = s.mint(posted);
        s.attachments.push(Attachment {
            id: id.clone(),
            card: card.to_string(),
            name: name.to_string(),
            posted,
            millis: 0,
            bytes: Vec::new(),
            link: Some(url.to_string()),
        });
        id
    }

    /// A card's uploaded files, as `(name, bytes)` in the order they were put on it.
    pub fn attachments(&self, card: &str) -> Vec<(String, Vec<u8>)> {
        let s = lock(&self.state);
        let mut files: Vec<&Attachment> = s
            .attachments
            .iter()
            .filter(|a| a.card == card && a.link.is_none())
            .collect();
        files.sort_by(|a, b| a.id.cmp(&b.id));
        files
            .into_iter()
            .map(|a| (a.name.clone(), a.bytes.clone()))
            .collect()
    }

    /// Put an uploaded file named `name` on a card that the listing says holds `size` bytes,
    /// with none behind it — a file too large to hold; returns its id. Downloading it
    /// answers no bytes.
    pub fn attachment_sized(&self, card: &str, name: &str, size: u64, ago: u64) -> String {
        let id = self.attachment(card, name, b"", ago);
        let mut s = lock(&self.state);
        s.sizes.insert(id.clone(), size);
        id
    }

    /// Make every request to `route` answer `status`.
    pub fn fail(&self, route: &str, status: u16) {
        lock(&self.state).faults.insert(route.to_string(), status);
    }

    /// Make the next `times` requests to `route` answer `429 Too Many Requests`, each with
    /// `x-rate-limit-api-key-interval-ms` and `x-rate-limit-api-token-interval-ms` headers
    /// naming `interval_ms` when given, and with no rate-limit header at all when not.
    pub fn throttle(&self, route: &str, times: u32, interval_ms: Option<u64>) {
        lock(&self.state)
            .throttles
            .insert(route.to_string(), (times, interval_ms));
    }

    /// How many requests the fake has seen for the path `path`, whatever the method.
    pub fn requests_to(&self, path: &str) -> usize {
        lock(&self.state)
            .seen
            .iter()
            .filter(|r| r.path == path)
            .count()
    }

    /// Make every comment post whose text contains `needle` answer `status`.
    pub fn fail_post_matching(&self, needle: &str, status: u16) {
        lock(&self.state)
            .post_faults
            .push((needle.to_string(), status));
    }

    /// Stop failing `route`, and every comment post.
    pub fn heal(&self, route: &str) {
        let mut s = lock(&self.state);
        s.faults.remove(route);
        s.post_faults.clear();
    }

    /// Take the fake out of reach, as a network outage does: every request from now on is
    /// read and its connection closed with no reply, so the client sees no response at
    /// all. Nothing is answered, and nothing changes, until [`restore`](Self::restore).
    pub fn outage(&self) {
        lock(&self.state).down = true;
    }

    /// Bring the fake back after an [`outage`](Self::outage).
    pub fn restore(&self) {
        lock(&self.state).down = false;
    }

    /// The requests an outage dropped, in order.
    pub fn dropped(&self) -> Vec<Seen> {
        lock(&self.state).dropped.clone()
    }

    /// On the next `[afkd-claim]` posted to `card`, a rival's claim lands first: posted a
    /// second earlier by another member, so it out-orders ours.
    pub fn race_on_next_claim(&self, card: &str) {
        lock(&self.state).races.insert(card.to_string());
    }

    /// A card's comments, in the order they were posted.
    pub fn comments(&self, card: &str) -> Vec<Comment> {
        let s = lock(&self.state);
        let mut thread: Vec<Comment> = s
            .comments
            .iter()
            .filter(|c| c.card == card)
            .cloned()
            .collect();
        thread.sort_by(|a, b| a.id.cmp(&b.id));
        thread
    }

    /// A card's label names.
    pub fn labels(&self, card: &str) -> Vec<String> {
        let s = lock(&self.state);
        let card = s
            .cards
            .iter()
            .find(|c| c.id == card)
            .expect("a seeded card");
        card.labels
            .iter()
            .filter_map(|id| s.labels.iter().find(|l| l.id == *id))
            .map(|l| l.name.clone())
            .collect()
    }

    /// A card's member usernames.
    pub fn members(&self, card: &str) -> Vec<String> {
        let s = lock(&self.state);
        let card = s
            .cards
            .iter()
            .find(|c| c.id == card)
            .expect("a seeded card");
        card.members
            .iter()
            .filter_map(|id| s.member(id))
            .map(|m| m.username.clone())
            .collect()
    }

    /// The name of the list a card sits in.
    pub fn list_of(&self, card: &str) -> String {
        let s = lock(&self.state);
        let card = s
            .cards
            .iter()
            .find(|c| c.id == card)
            .expect("a seeded card");
        s.lists
            .iter()
            .find(|l| l.id == card.list)
            .map(|l| l.name.clone())
            .unwrap_or_default()
    }

    /// The open cards in the list named `list`, in order.
    pub fn cards_in(&self, list: &str) -> Vec<String> {
        let s = lock(&self.state);
        let Some(list) = s.lists.iter().find(|l| l.name == list) else {
            return Vec::new();
        };
        s.cards
            .iter()
            .filter(|c| c.list == list.id && !c.closed)
            .map(|c| c.id.clone())
            .collect()
    }

    /// Whether a card is marked complete.
    pub fn complete(&self, card: &str) -> bool {
        let s = lock(&self.state);
        s.cards.iter().any(|c| c.id == card && c.due_complete)
    }

    /// Whether a card is archived.
    pub fn archived(&self, card: &str) -> bool {
        let s = lock(&self.state);
        s.cards.iter().any(|c| c.id == card && c.closed)
    }

    pub fn seen(&self) -> Vec<Seen> {
        lock(&self.state).seen.clone()
    }
}

/// RFC 3339, UTC, to the second — the shape a `comments` reply's `at` takes.
pub fn utc(secs: u64) -> String {
    stamp(secs).replace(".000Z", "Z")
}

/// RFC 3339, UTC, to the millisecond — Trello writes fractional seconds.
fn stamp(secs: u64) -> String {
    let (days, rem) = ((secs / 86_400) as i64, secs % 86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.000Z",
        rem / 3_600,
        (rem % 3_600) / 60,
        rem % 60
    )
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = |b: u8| (b as char).to_digit(16);
                match (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                    (Some(hi), Some(lo)) => {
                        out.push((hi * 16 + lo) as u8);
                        i += 3;
                    }
                    _ => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Serve one connection: one request, one reply, then close.
fn serve(stream: TcpStream, state: &Mutex<State>) {
    let Ok(read_half) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(read_half);
    let mut start = String::new();
    if reader.read_line(&mut start).unwrap_or(0) == 0 {
        return;
    }
    let mut parts = start.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("").to_string();
    let mut length = 0usize;
    let (mut content_type, mut authorization) = (None, None);
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            let (name, value) = (name.trim(), value.trim().to_string());
            if name.eq_ignore_ascii_case("content-length") {
                length = value.parse().unwrap_or(0);
            } else if name.eq_ignore_ascii_case("content-type") {
                content_type = Some(value);
            } else if name.eq_ignore_ascii_case("authorization") {
                authorization = Some(value);
            }
        }
    }
    let mut raw = vec![0u8; length];
    if reader.read_exact(&mut raw).is_err() {
        return;
    }
    let body = String::from_utf8_lossy(&raw).into_owned();
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target.clone(), String::new()),
    };
    let query: Vec<(String, String)> = query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect();

    let (status, mime, reply, headers) = {
        let mut s = lock(state);
        let seen = Seen {
            method: method.clone(),
            path: path.clone(),
            query: query.clone(),
            body: body.clone(),
            raw: raw.clone(),
            content_type: content_type.clone(),
            authorization: authorization.clone(),
        };
        if s.down {
            // Out of reach: the stream drops here, closed with no reply.
            s.dropped.push(seen);
            return;
        }
        s.seen.push(seen);
        let param = |key: &str| {
            query
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
        };
        s.reply_headers.clear();
        if let Some((status, mime, reply)) =
            download(&s, &method, &path, &query, authorization.as_deref())
        {
            (status, mime, reply, Vec::new())
        } else {
            let (status, reply) = if param("key") != Some(KEY) || param("token") != Some(TOKEN) {
                (401, json!("invalid key"))
            } else {
                let request = Request {
                    method: &method,
                    path: &path,
                    query: &query,
                    body: &body,
                    raw: &raw,
                    content_type: content_type.as_deref(),
                };
                route(&mut s, &request)
            };
            let headers = std::mem::take(&mut s.reply_headers);
            (
                status,
                "application/json",
                reply.to_string().into_bytes(),
                headers,
            )
        }
    };
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        429 => "Too Many Requests",
        _ => "Status",
    };
    let extra: String = headers
        .iter()
        .map(|(name, value)| format!("{name}: {value}\r\n"))
        .collect();
    let mut stream = stream;
    let _ = write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {mime}\r\nContent-Length: {}\r\n{extra}\
         Connection: close\r\n\r\n",
        reply.len()
    );
    let _ = stream.write_all(&reply);
    let _ = stream.flush();
}

/// One authenticated request, as [`route`] reads it.
struct Request<'a> {
    method: &'a str,
    path: &'a str,
    query: &'a [(String, String)],
    body: &'a str,
    raw: &'a [u8],
    content_type: Option<&'a str>,
}

/// The OAuth header a download must carry, as Trello's download host takes it.
pub fn oauth_header() -> String {
    format!(r#"OAuth oauth_consumer_key="{KEY}", oauth_token="{TOKEN}""#)
}

/// Answer a download of an upload's bytes, or `None` when the request is no download: a
/// `GET /1/cards/{card}/attachments/{id}/download/{file}`, authenticated by the OAuth
/// header alone — a query credential beside it is refused, as Trello's host refuses it.
/// A route made to fail answers its status, as every other does.
fn download(
    s: &State,
    method: &str,
    path: &str,
    query: &[(String, String)],
    authorization: Option<&str>,
) -> Option<(u16, &'static str, Vec<u8>)> {
    let segs: Vec<&str> = path.strip_prefix("/1/")?.split('/').collect();
    let ["cards", card, "attachments", id, "download", _] = segs.as_slice() else {
        return None;
    };
    if method != "GET" {
        return None;
    }
    let json = |status: u16, reply: Value| {
        Some((status, "application/json", reply.to_string().into_bytes()))
    };
    if let Some(status) = s.faults.get("download attachment") {
        return json(*status, json!({ "message": "injected" }));
    }
    let carries = |key: &str| query.iter().any(|(k, _)| k == key);
    if authorization != Some(oauth_header().as_str()) || carries("key") || carries("token") {
        return json(401, json!("unauthorized permission requested"));
    }
    match s
        .attachments
        .iter()
        .find(|a| a.id == *id && a.card == *card && a.link.is_none())
    {
        Some(a) => Some((200, "application/octet-stream", a.bytes.clone())),
        None => json(404, json!("The requested resource was not found.")),
    }
}

/// The parts of a multipart form whose content type is `content_type`, in order; empty
/// when it is no multipart form.
pub fn multipart_parts(content_type: &str, raw: &[u8]) -> Vec<Part> {
    let Some(boundary) = content_type
        .split(';')
        .find_map(|p| p.trim().strip_prefix("boundary="))
    else {
        return Vec::new();
    };
    let delimiter = format!("--{boundary}");
    let find = |hay: &[u8], needle: &[u8]| hay.windows(needle.len()).position(|w| w == needle);
    let mut segments = Vec::new();
    let mut rest = raw;
    while let Some(at) = find(rest, delimiter.as_bytes()) {
        segments.push(&rest[..at]);
        rest = &rest[at + delimiter.len()..];
    }
    segments
        .iter()
        .skip(1)
        .filter_map(|segment| {
            let part = segment.strip_prefix(b"\r\n")?.strip_suffix(b"\r\n")?;
            let split = find(part, b"\r\n\r\n")?;
            let headers = String::from_utf8_lossy(&part[..split]).into_owned();
            let field = |key: &str| {
                let start = headers.find(&format!("{key}=\""))? + key.len() + 2;
                let end = headers[start..].find('"')? + start;
                Some(headers[start..end].to_string())
            };
            Some(Part {
                name: field(" name").or_else(|| field(";name"))?,
                filename: field("filename"),
                bytes: part[split + 4..].to_vec(),
            })
        })
        .collect()
}

/// Answer one authenticated request.
fn route(s: &mut State, request: &Request) -> (u16, Value) {
    let Request {
        method,
        path,
        query,
        body,
        ..
    } = *request;
    let not_found = (404, json!("The requested resource was not found."));
    let Some(rest) = path.strip_prefix("/1/") else {
        return not_found;
    };
    let segs: Vec<String> = rest.split('/').map(percent_decode).collect();
    let segs: Vec<&str> = segs.iter().map(String::as_str).collect();
    let param = |key: &str| {
        query
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.to_string())
    };
    let payload: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let name = match (method, segs.as_slice()) {
        ("GET", ["members", "me"]) => "me",
        ("GET", ["members", "me", "boards"]) => "my boards",
        ("GET", ["boards", _]) => "read board",
        ("GET", ["boards", _, "lists"]) => "lists",
        ("GET", ["boards", _, "members"]) => "members",
        ("GET", ["boards", _, "labels"]) => "labels",
        ("POST", ["boards", _, "labels"]) => "create label",
        ("GET", ["boards", _, "cards"] | ["boards", _, "cards", _]) => "board cards",
        ("GET", ["lists", _, "cards"]) => "list cards",
        ("GET", ["cards", _]) => "read card",
        ("GET", ["cards", _, "checklists"]) => "list checklists",
        ("GET", ["cards", _, "actions"]) => match param("filter").as_deref() {
            Some("createCard") => "created",
            _ => "read comments",
        },
        ("POST", ["cards"]) => "create card",
        ("DELETE", ["cards", _]) => "delete card",
        ("POST", ["cards", _, "actions", "comments"]) => "post comment",
        ("PUT", ["cards", _, "actions", _, "comments"]) => "edit comment",
        ("DELETE", ["cards", _, "actions", _, "comments"]) => "delete comment",
        ("GET", ["actions", _]) => "read action",
        ("PUT", ["actions", _]) => "update action",
        ("DELETE", ["actions", _]) => "delete action",
        ("POST", ["cards", _, "checklists"]) => "create checklist",
        ("GET", ["checklists", _]) => "read checklist",
        ("DELETE", ["checklists", _]) => "delete checklist",
        ("POST", ["checklists", _, "checkItems"]) => "create check item",
        ("PUT", ["cards", _, "checkItem", _]) => "update check item",
        ("DELETE", ["cards", _, "checkItem", _]) => "delete check item",
        ("DELETE", ["cards", _, "attachments", _]) => "delete attachment",
        // A PUT with a JSON body is an update of the card; the query-parameter forms below
        // are today's actions'.
        ("PUT", ["cards", _]) => {
            if payload.is_object() {
                "update card"
            } else if param("idList").is_some() || payload.get("idList").is_some() {
                "move card"
            } else if param("closed").is_some() {
                "archive card"
            } else {
                "complete card"
            }
        }
        ("POST", ["cards", _, "idLabels"]) => "add label",
        ("DELETE", ["cards", _, "idLabels", _]) => "remove label",
        ("POST", ["cards", _, "idMembers"]) => "add member",
        ("DELETE", ["cards", _, "idMembers", _]) => "remove member",
        ("POST", ["cards", _, "attachments"]) => "post attachment",
        ("GET", ["cards", _, "attachments"]) => "list attachments",
        ("GET", ["cards", _, "attachments", _]) => "get attachment",
        _ => return not_found,
    };
    if let Some(status) = s.faults.get(name) {
        return (*status, json!({ "message": "injected" }));
    }
    if let Some((left, interval)) = s.throttles.get_mut(name).filter(|(left, _)| *left > 0) {
        *left -= 1;
        let interval = *interval;
        s.reply_headers = interval.map_or_else(Vec::new, |ms| {
            vec![
                (
                    "x-rate-limit-api-key-interval-ms".to_string(),
                    ms.to_string(),
                ),
                (
                    "x-rate-limit-api-token-interval-ms".to_string(),
                    ms.to_string(),
                ),
                (
                    "x-rate-limit-api-key-remaining".to_string(),
                    "0".to_string(),
                ),
            ]
        });
        return (429, json!("API_TOKEN_LIMIT_EXCEEDED"));
    }
    // The board a `/boards/{b}/…` path names, by its id or its short link.
    let board_at = if segs[0] == "boards" {
        match s.board_at(segs[1]) {
            Some(at) => Some(at),
            None => return not_found,
        }
    } else {
        None
    };
    let card_at = |s: &State| {
        segs.get(1).and_then(|id| {
            s.cards
                .iter()
                .position(|c| c.id == *id || c.short_link == *id)
        })
    };
    // A field read from the query, else from the JSON body.
    let field = |key: &str| {
        param(key)
            .map(Value::String)
            .or_else(|| payload.get(key).cloned())
    };
    // The numeric query parameter `key`, when it reads as a count.
    let count = |key: &str| param(key).and_then(|v| v.parse::<usize>().ok());
    match name {
        "me" => (200, s.member_json(&s.me.clone())),
        "my boards" => {
            let me = s.me.clone();
            let open_only = param("filter").as_deref() == Some("open");
            let on: Vec<&String> = s
                .member(&me)
                .map(|m| m.boards.iter().collect())
                .unwrap_or_default();
            (
                200,
                s.boards
                    .iter()
                    .filter(|b| on.contains(&&b.id) && !(open_only && b.closed))
                    .map(|b| State::only(s.board_json(b), param("fields").as_deref()))
                    .collect(),
            )
        }
        "read board" => {
            let at = board_at.expect("a board path");
            (
                200,
                State::only(s.board_json(&s.boards[at]), param("fields").as_deref()),
            )
        }
        "lists" => {
            let board = s.boards[board_at.expect("a board path")].id.clone();
            let filter = param("filter").unwrap_or_else(|| "open".to_string());
            (
                200,
                s.lists
                    .iter()
                    .filter(|l| l.board == board)
                    .filter(|l| match filter.as_str() {
                        "all" => true,
                        "closed" => l.closed,
                        _ => !l.closed,
                    })
                    .map(|l| s.list_json(l))
                    .collect(),
            )
        }
        "members" => {
            let board = s.boards[board_at.expect("a board path")].id.clone();
            (
                200,
                s.members
                    .iter()
                    .filter(|m| m.boards.contains(&board))
                    .map(|m| s.member_json(&m.id))
                    .collect(),
            )
        }
        "labels" => {
            // Trello gives 50 labels unless asked for more, and no more than 1000.
            let board = s.boards[board_at.expect("a board path")].id.clone();
            let limit = count("limit").unwrap_or(50).min(1000);
            (
                200,
                s.labels
                    .iter()
                    .filter(|l| l.board == board)
                    .take(limit)
                    .map(|l| s.label_json(&l.id))
                    .collect(),
            )
        }
        "create label" => {
            let board = s.boards[board_at.expect("a board path")].id.clone();
            let id = s.mint(now());
            let label_name = param("name").unwrap_or_default();
            let color = param("color").filter(|c| c != "null");
            s.labels.push(Label {
                id: id.clone(),
                name: label_name.clone(),
                color: color.clone(),
                board,
            });
            (200, json!({ "id": id, "name": label_name, "color": color }))
        }
        "board cards" => {
            let board = s.boards[board_at.expect("a board path")].id.clone();
            // `/cards` is the open cards; `/cards/{filter}` names which.
            let filter = segs.get(3).copied().unwrap_or("open");
            let matching: Vec<&Card> = s
                .cards
                .iter()
                .filter(|c| c.board == board)
                .filter(|c| match filter {
                    "all" => true,
                    "closed" => c.closed,
                    "visible" => !c.closed && s.lists.iter().any(|l| l.id == c.list && !l.closed),
                    _ => !c.closed,
                })
                .collect();
            let fields = param("fields");
            // Asked a `limit` or a `before`, the board answers a page of its cards, the newest
            // first, from below the card id `before`, 1000 at most; asked neither, all of them
            // in the board's order.
            let paged = param("limit").is_some() || param("before").is_some();
            let page: Vec<&Card> = if paged {
                let before = param("before");
                let mut page: Vec<&Card> = matching
                    .into_iter()
                    .filter(|c| before.as_deref().is_none_or(|b| c.id.as_str() < b))
                    .collect();
                page.sort_by(|a, b| b.id.cmp(&a.id));
                page.truncate(count("limit").unwrap_or(1000).min(1000));
                page
            } else {
                matching
            };
            (
                200,
                page.into_iter()
                    .map(|c| State::only(s.positioned_json(c), fields.as_deref()))
                    .collect(),
            )
        }
        "list cards" => {
            let with_actions = param("actions").as_deref() == Some("commentCard");
            (
                200,
                s.positions(segs[1])
                    .into_iter()
                    .map(|(at, pos)| {
                        let mut card = s.card_json(&s.cards[at], with_actions);
                        card["pos"] = json!(pos);
                        card
                    })
                    .collect(),
            )
        }
        // An archived card is served as an open one is, with `closed: true`.
        "read card" => match card_at(s) {
            Some(at) => {
                let value = s.nested_json(&s.cards[at], &|key: &str| param(key));
                let checklists = param("checklists").as_deref() == Some("all");
                (200, cut_card(value, param("fields").as_deref(), checklists))
            }
            None => not_found,
        },
        "list checklists" => match card_at(s) {
            Some(at) => {
                let card = &s.cards[at];
                let mut lists: Vec<&Checklist> = card.checklists.iter().collect();
                lists.sort_by(|a, b| a.pos.total_cmp(&b.pos));
                (
                    200,
                    lists
                        .into_iter()
                        .map(|cl| {
                            let mut value = s.checklist_json(cl, &card.id);
                            if let Some(items) = value["checkItems"].as_array_mut() {
                                items.sort_by(|a, b| {
                                    let pos = |v: &Value| v["pos"].as_f64().unwrap_or(f64::MAX);
                                    pos(a).total_cmp(&pos(b))
                                });
                            }
                            value
                        })
                        .collect(),
                )
            }
            None => not_found,
        },
        "read comments" => match card_at(s) {
            // Trello gives the newest 50 comment actions unless asked for more, up to 1000,
            // from below the action id `before`.
            Some(at) => (
                200,
                Value::Array(s.actions_page(
                    &s.cards[at].id,
                    param("before").as_deref(),
                    count("limit").unwrap_or(50).min(1000),
                )),
            ),
            None => not_found,
        },
        "created" => match card_at(s) {
            Some(at) => {
                let born = u64::from_str_radix(&s.cards[at].id[..8], 16).unwrap_or(0);
                (
                    200,
                    json!([{ "id": s.cards[at].id, "type": "createCard", "date": stamp(born) }]),
                )
            }
            None => not_found,
        },
        "create card" => {
            let list = payload["idList"].as_str().unwrap_or("").to_string();
            let Some(board) = s
                .lists
                .iter()
                .find(|l| l.id == list)
                .map(|l| l.board.clone())
            else {
                return (400, json!("invalid value for idList"));
            };
            let ids = |key: &str| -> Vec<String> {
                payload[key]
                    .as_array()
                    .map(|ids| {
                        ids.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default()
            };
            let (labels, members) = (ids("idLabels"), ids("idMembers"));
            let due = match payload.get("due") {
                None | Some(Value::Null) => None,
                Some(due) => match due.as_str().and_then(parse_stamp) {
                    Some(due) => Some(due),
                    None => return (400, json!("invalid value for due")),
                },
            };
            let at = s.board_at(&board).expect("the list's board");
            s.boards[at].cards += 1;
            let number = s.boards[at].cards;
            let created = now();
            let id = s.mint(created);
            let card = Card {
                short_link: id[id.len() - 8..].to_string(),
                id,
                name: payload["name"].as_str().unwrap_or("").to_string(),
                desc: payload["desc"].as_str().unwrap_or("").to_string(),
                list,
                board,
                number,
                labels,
                members,
                checklists: Vec::new(),
                closed: false,
                due_complete: false,
                due: due.map(|(secs, _)| secs),
                due_millis: due.map_or(0, |(_, millis)| millis),
                last_activity: created,
            };
            let mut reply = s.card_json(&card, false);
            reply["shortUrl"] = json!(format!("https://trello.com/c/{}", card.short_link));
            s.place(card, None, Pos::read(payload.get("pos"), Pos::Bottom));
            (200, reply)
        }
        "post comment" => {
            let Some(at) = card_at(s) else {
                return not_found;
            };
            let card = s.cards[at].id.clone();
            // The text rides the JSON body, or the query as `ask.py` sends it.
            let text = payload["text"]
                .as_str()
                .map(str::to_string)
                .or_else(|| param("text"))
                .unwrap_or_default();
            if let Some((_, status)) = s.post_faults.iter().find(|(n, _)| text.contains(n)) {
                return (*status, json!({ "message": "injected" }));
            }
            let posted = now();
            if text.starts_with("[afkd-claim]") && s.races.remove(&card) {
                let rival = s
                    .members
                    .iter()
                    .find(|m| m.username == "rival-host")
                    .map(|m| m.id.clone())
                    .unwrap_or_default();
                let id = s.mint(posted - 1);
                s.comments.push(Comment {
                    id,
                    card: card.clone(),
                    author: rival,
                    text: "[afkd-claim] owner=afkd-17".to_string(),
                    posted: posted - 1,
                    edited: None,
                });
            }
            let id = s.mint(posted);
            let me = s.me.clone();
            let comment = Comment {
                id,
                card,
                author: me,
                text,
                posted,
                edited: None,
            };
            let reply = s.comment_json(&comment);
            let card = comment.card.clone();
            s.comments.push(comment);
            s.touch(&card);
            (200, reply)
        }
        "edit comment" | "delete comment" => {
            let (card, action) = (segs[1], segs[3]);
            let Some(at) = s
                .comments
                .iter()
                .position(|c| c.card == card && c.id == action)
            else {
                return not_found;
            };
            if name == "delete comment" {
                s.comments.remove(at);
                s.touch(card);
                return (200, json!({ "_value": null }));
            }
            let comment = &mut s.comments[at];
            comment.text = payload["text"].as_str().unwrap_or("").to_string();
            comment.edited = Some(now());
            let comment = comment.clone();
            s.touch(card);
            (200, s.comment_json(&comment))
        }
        "move card" | "archive card" | "complete card" => {
            let Some(at) = card_at(s) else {
                return not_found;
            };
            match name {
                "move card" => {
                    let mut card = s.cards[at].clone();
                    card.list = field("idList")
                        .and_then(|v| v.as_str().map(str::to_string))
                        .unwrap_or_default();
                    if let Some(list) = s.lists.iter().find(|l| l.id == card.list) {
                        card.board = list.board.clone();
                    }
                    s.place(card, Some(at), Pos::read(field("pos").as_ref(), Pos::Top));
                }
                "archive card" => s.cards[at].closed = param("closed").as_deref() == Some("true"),
                _ => s.cards[at].due_complete = param("dueComplete").as_deref() == Some("true"),
            }
            s.touch(segs[1]);
            let card = s
                .cards
                .iter()
                .find(|c| c.id == segs[1] || c.short_link == segs[1])
                .cloned();
            (200, card.map_or(Value::Null, |c| s.card_json(&c, false)))
        }
        "add label" | "remove label" => {
            let Some(at) = card_at(s) else {
                return not_found;
            };
            if name == "add label" {
                let label = param("value").unwrap_or_default();
                if !s.cards[at].labels.contains(&label) {
                    s.cards[at].labels.push(label);
                }
            } else {
                s.cards[at].labels.retain(|l| l != segs[3]);
            }
            s.touch(segs[1]);
            (200, json!(s.cards[at].labels))
        }
        "add member" | "remove member" => {
            let Some(at) = card_at(s) else {
                return not_found;
            };
            if name == "add member" {
                let member = param("value").unwrap_or_default();
                if s.cards[at].members.contains(&member) {
                    return (400, json!({ "message": "member is already on the card" }));
                }
                s.cards[at].members.push(member);
            } else {
                s.cards[at].members.retain(|m| m != segs[3]);
            }
            s.touch(segs[1]);
            (200, json!(s.cards[at].members))
        }
        "post attachment" => {
            let Some(at) = card_at(s) else {
                return not_found;
            };
            let parts = multipart_parts(request.content_type.unwrap_or(""), request.raw);
            let Some(file) = parts.iter().find(|p| p.name == "file") else {
                return (400, json!({ "message": "no file part" }));
            };
            // Trello stores the `name` part as the attachment's name, else the file's.
            let name = parts
                .iter()
                .find(|p| p.name == "name")
                .map(|p| String::from_utf8_lossy(&p.bytes).into_owned())
                .or_else(|| file.filename.clone())
                .unwrap_or_default();
            let clock = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default();
            let posted = clock.as_secs();
            let attachment = Attachment {
                id: s.mint(posted),
                card: s.cards[at].id.clone(),
                name,
                posted,
                millis: clock.subsec_millis(),
                bytes: file.bytes.clone(),
                link: None,
            };
            let reply = s.attachment_json(&attachment);
            s.attachments.push(attachment);
            s.touch(segs[1]);
            (200, reply)
        }
        "list attachments" => match card_at(s) {
            Some(at) => {
                let card = s.cards[at].id.clone();
                (
                    200,
                    s.attachments
                        .iter()
                        .filter(|a| a.card == card)
                        .map(|a| s.attachment_json(a))
                        .collect(),
                )
            }
            None => not_found,
        },
        "get attachment" => {
            let card = card_at(s).map(|at| s.cards[at].id.clone());
            match s
                .attachments
                .iter()
                .find(|a| Some(&a.card) == card.as_ref() && a.id == segs[3])
            {
                Some(a) => (200, s.attachment_json(a)),
                None => not_found,
            }
        }
        "update card" => {
            let Some(at) = card_at(s) else {
                return not_found;
            };
            let id = s.cards[at].id.clone();
            let moved = payload.get("idList").is_some() || payload.get("pos").is_some();
            let list = match payload.get("idList") {
                None => s.cards[at].list.clone(),
                Some(list) => list.as_str().unwrap_or("").to_string(),
            };
            let Some(board) = s
                .lists
                .iter()
                .find(|l| l.id == list)
                .map(|l| l.board.clone())
            else {
                return (400, json!("invalid value for idList"));
            };
            let due = match payload.get("due") {
                None => None,
                Some(Value::Null) => Some(None),
                Some(due) => match due.as_str().and_then(parse_stamp) {
                    Some(due) => Some(Some(due)),
                    None => return (400, json!("invalid value for due")),
                },
            };
            let card = &mut s.cards[at];
            if let Some(name) = payload.get("name").and_then(Value::as_str) {
                card.name = name.to_string();
            }
            if let Some(desc) = payload.get("desc").and_then(Value::as_str) {
                card.desc = desc.to_string();
            }
            if let Some(done) = payload.get("dueComplete").and_then(Value::as_bool) {
                card.due_complete = done;
            }
            if let Some(closed) = payload.get("closed").and_then(Value::as_bool) {
                card.closed = closed;
            }
            if let Some(due) = due {
                card.due = due.map(|(secs, _)| secs);
                card.due_millis = due.map_or(0, |(_, millis)| millis);
            }
            if moved {
                let mut card = s.cards[at].clone();
                card.list = list;
                card.board = board;
                s.place(card, Some(at), Pos::read(payload.get("pos"), Pos::Top));
            }
            s.touch(&id);
            let card = s.cards.iter().find(|c| c.id == id).cloned();
            (200, card.map_or(Value::Null, |c| s.card_json(&c, false)))
        }
        "delete card" => {
            let Some(at) = card_at(s) else {
                return not_found;
            };
            let id = s.cards.remove(at).id;
            s.comments.retain(|c| c.card != id);
            s.attachments.retain(|a| a.card != id);
            (200, json!({ "limits": {} }))
        }
        "read action" => match s.comments.iter().find(|c| c.id == segs[1]) {
            Some(c) => {
                let mut value = s.comment_json(c);
                if let Some(fields) = param("memberCreator_fields") {
                    value["memberCreator"] =
                        State::only(value["memberCreator"].clone(), Some(&fields));
                }
                (200, value)
            }
            None => not_found,
        },
        "update action" | "delete action" => {
            let Some(at) = s.comments.iter().position(|c| c.id == segs[1]) else {
                return not_found;
            };
            // Trello lets a token edit and delete only its own comments.
            if s.comments[at].author != s.me {
                return (401, json!("unauthorized permission requested"));
            }
            let card = s.comments[at].card.clone();
            if name == "delete action" {
                s.comments.remove(at);
                s.touch(&card);
                return (200, json!({ "_value": null }));
            }
            let text = field("text")
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default();
            let comment = &mut s.comments[at];
            comment.text = text;
            comment.edited = Some(now());
            let comment = comment.clone();
            s.touch(&card);
            (200, s.comment_json(&comment))
        }
        "create checklist" => {
            let Some(at) = card_at(s) else {
                return not_found;
            };
            let pos = next_pos(
                s.cards[at].checklists.iter().map(|cl| cl.pos),
                Pos::read(field("pos").as_ref(), Pos::Bottom),
            );
            let list_name = field("name")
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default();
            let checklist = Checklist {
                id: s.mint(now()),
                name: list_name,
                items: Vec::new(),
                pos,
            };
            let card = s.cards[at].id.clone();
            let reply = s.checklist_json(&checklist, &card);
            s.cards[at].checklists.push(checklist);
            s.touch(&card);
            (200, reply)
        }
        "read checklist" | "delete checklist" | "create check item" => {
            let Some((on, of)) = s.checklist_at(segs[1]) else {
                return not_found;
            };
            let card = s.cards[on].id.clone();
            match name {
                "delete checklist" => {
                    s.cards[on].checklists.remove(of);
                    s.touch(&card);
                    (200, json!({ "limits": {} }))
                }
                "create check item" => {
                    let item_id = s.mint(now());
                    let list = &mut s.cards[on].checklists[of];
                    let pos = Pos::read(field("pos").as_ref(), Pos::Bottom);
                    let item = CheckItem {
                        id: item_id,
                        name: field("name")
                            .and_then(|v| v.as_str().map(str::to_string))
                            .unwrap_or_default(),
                        done: false,
                        pos: next_pos(list.items.iter().map(|i| i.pos), pos),
                    };
                    let reply = check_item_json(&item, &list.id, &card);
                    match pos {
                        Pos::Top => list.items.insert(0, item),
                        _ => list.items.push(item),
                    }
                    s.touch(&card);
                    (200, reply)
                }
                _ => {
                    // Trello's `fields` cut the checklist, and `checkItems` — all, unless
                    // `none` — is a nest of its own.
                    let list = &s.cards[on].checklists[of];
                    let mut value = s.checklist_json(list, &card);
                    let mut items = value["checkItems"].take();
                    if let Some(items) = items.as_array_mut() {
                        items.sort_by(|a, b| {
                            let pos = |v: &Value| v["pos"].as_f64().unwrap_or(f64::MAX);
                            pos(a).total_cmp(&pos(b))
                        });
                    }
                    let mut value = State::only(value, param("fields").as_deref());
                    if param("checkItems").as_deref() != Some("none") {
                        value["checkItems"] = items;
                    }
                    (200, value)
                }
            }
        }
        "update check item" | "delete check item" => {
            let Some(at) = card_at(s) else {
                return not_found;
            };
            let card = s.cards[at].id.clone();
            let found = s.cards[at]
                .checklists
                .iter()
                .enumerate()
                .find_map(|(l, cl)| {
                    cl.items
                        .iter()
                        .position(|i| i.id == segs[3])
                        .map(|i| (l, i))
                });
            let Some((list_at, item_at)) = found else {
                return not_found;
            };
            let list = &mut s.cards[at].checklists[list_at];
            if name == "delete check item" {
                list.items.remove(item_at);
                s.touch(&card);
                return (200, json!({ "limits": {} }));
            }
            let state = match field("state").as_ref().and_then(Value::as_str) {
                None => None,
                Some("complete") => Some(true),
                Some("incomplete") => Some(false),
                Some(_) => return (400, json!("invalid value for state")),
            };
            let item = &mut list.items[item_at];
            if let Some(new_name) = field("name").as_ref().and_then(Value::as_str) {
                item.name = new_name.to_string();
            }
            if let Some(done) = state {
                item.done = done;
            }
            let reply = check_item_json(item, &list.id, &card);
            s.touch(&card);
            (200, reply)
        }
        "delete attachment" => {
            let card = card_at(s).map(|at| s.cards[at].id.clone());
            let Some(at) = s
                .attachments
                .iter()
                .position(|a| Some(&a.card) == card.as_ref() && a.id == segs[3])
            else {
                return not_found;
            };
            s.attachments.remove(at);
            s.touch(segs[1]);
            (200, json!({ "limits": {} }))
        }
        _ => not_found,
    }
}

/// A check item as Trello gives one.
fn check_item_json(item: &CheckItem, checklist: &str, card: &str) -> Value {
    json!({
        "id": item.id,
        "idChecklist": checklist,
        "idCard": card,
        "name": item.name,
        "pos": item.pos,
        "state": if item.done { "complete" } else { "incomplete" },
    })
}

/// The `pos` a checklist or check item is given among `others`: half the first at the top,
/// 16384 past the last at the bottom, or the number named.
fn next_pos(others: impl Iterator<Item = f64>, pos: Pos) -> f64 {
    let others: Vec<f64> = others.collect();
    match pos {
        Pos::Top => others
            .iter()
            .copied()
            .reduce(f64::min)
            .map_or(16_384.0, |p| p / 2.0),
        Pos::Bottom => others
            .iter()
            .copied()
            .reduce(f64::max)
            .map_or(16_384.0, |p| p + 16_384.0),
        Pos::At(n) => n,
    }
}

/// A card read cut to the `fields` it names and its `id`, with what it nests — its list, its
/// board, its members and its actions, and its checklists when they were asked for — kept.
fn cut_card(value: Value, fields: Option<&str>, checklists: bool) -> Value {
    let Some(fields) = fields.filter(|f| *f != "all") else {
        return value;
    };
    let wanted: HashSet<&str> = fields.split(',').collect();
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .filter(|(key, _)| {
                    let key = key.as_str();
                    key == "id"
                        || wanted.contains(key)
                        || ["list", "board", "members", "actions"].contains(&key)
                        || (checklists && key == "checklists")
                })
                .collect(),
        ),
        other => other,
    }
}

/// An RFC 3339 UTC stamp, `YYYY-MM-DDTHH:MM:SS[.fff]Z`, as seconds since the epoch and the
/// milliseconds within the second; `None` for any other shape.
fn parse_stamp(text: &str) -> Option<(u64, u32)> {
    let (date, time) = text.strip_suffix('Z')?.split_once('T')?;
    let mut d = date.splitn(3, '-');
    let year: i64 = d.next()?.parse().ok()?;
    let month: i64 = d.next()?.parse().ok()?;
    let day: i64 = d.next()?.parse().ok()?;
    let (clock, fraction) = time.split_once('.').unwrap_or((time, ""));
    let mut t = clock.splitn(3, ':');
    let hour: i64 = t.next()?.parse().ok()?;
    let minute: i64 = t.next()?.parse().ok()?;
    let second: i64 = t.next()?.parse().ok()?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
        || !fraction.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let millis: String = fraction.chars().chain("000".chars()).take(3).collect();
    // Days from the civil date (Howard Hinnant's `days_from_civil`).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + hour * 3_600 + minute * 60 + second;
    Some((u64::try_from(secs).ok()?, millis.parse().ok()?))
}
