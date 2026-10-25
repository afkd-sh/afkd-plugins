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
//! carries the board's `idBoard`; a list's cards carry a `pos` of 16384 × their place,
//! counted from one; a card created or moved (its `idList` and `pos` in the query or a
//! JSON body) lands at the top or bottom of its list, or before the first other card
//! whose `pos` is greater than the number it names, as those positions stood before the
//! move; adding a member already on the card is Trello's `400`; a comment or card that
//! does not exist is a `404`. An attachment is uploaded as a multipart form whose `name`
//! part names it, listed with its date, size and a download `url` on this fake, and
//! downloaded from there with the OAuth `Authorization` header and **no** query
//! credential — either of those wrong is a `401`, as on Trello's download host. Every
//! request is recorded, and any route can be made to answer a status instead — or the
//! whole fake taken out of reach, every connection closed unanswered.

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
struct Member {
    id: String,
    username: String,
    full_name: String,
}

#[derive(Debug, Clone)]
struct List {
    id: String,
    name: String,
}

#[derive(Debug, Clone)]
struct Label {
    id: String,
    name: String,
}

#[derive(Debug, Clone)]
struct Checklist {
    id: String,
    name: String,
    items: Vec<(String, String, bool)>,
}

#[derive(Debug, Clone)]
struct Card {
    id: String,
    short_link: String,
    name: String,
    desc: String,
    list: String,
    labels: Vec<String>,
    members: Vec<String>,
    checklists: Vec<Checklist>,
    closed: bool,
    due_complete: bool,
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
    members: Vec<Member>,
    lists: Vec<List>,
    labels: Vec<Label>,
    /// In board order: a list's cards are the ones naming it, in this order.
    cards: Vec<Card>,
    comments: Vec<Comment>,
    attachments: Vec<Attachment>,
    /// The API root, which an upload's download `url` is built on.
    base: String,
    /// A route name → the status it answers instead.
    faults: HashMap<String, u16>,
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
        let name = self
            .labels
            .iter()
            .find(|l| l.id == id)
            .map_or("", |l| l.name.as_str());
        json!({ "id": id, "name": name, "color": null })
    }

    fn comment_json(&self, c: &Comment) -> Value {
        let mut data = json!({ "text": c.text });
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
        let mut thread: Vec<&Comment> = self.comments.iter().filter(|c| c.card == card).collect();
        thread.sort_by(|a, b| b.id.cmp(&a.id));
        thread.into_iter().map(|c| self.comment_json(c)).collect()
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
            "bytes": if a.link.is_none() { json!(a.bytes.len()) } else { Value::Null },
            "mimeType": if a.link.is_none() { "application/octet-stream" } else { "" },
        })
    }

    fn card_json(&self, card: &Card, with_actions: bool) -> Value {
        let mut value = json!({
            "id": card.id,
            "idBoard": BOARD,
            "shortLink": card.short_link,
            "name": card.name,
            "url": card_url(card),
            "desc": card.desc,
            "idList": card.list,
            "idMembers": card.members,
            "labels": card.labels.iter().map(|l| self.label_json(l)).collect::<Vec<_>>(),
            "closed": card.closed,
            "dueComplete": card.due_complete,
            "checklists": card.checklists.iter().map(|cl| json!({
                "id": cl.id,
                "name": cl.name,
                "checkItems": cl.items.iter().map(|(id, name, done)| json!({
                    "id": id,
                    "name": name,
                    "state": if *done { "complete" } else { "incomplete" },
                })).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
        });
        if with_actions {
            value["actions"] = Value::Array(self.thread(&card.id));
        }
        value
    }
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
            let me = s.mint(now() - 400 * 86_400);
            s.members.push(Member {
                id: me.clone(),
                username: ME.to_string(),
                full_name: "afkd".to_string(),
            });
            s.me = me;
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

    /// afkd's own member id.
    pub fn me(&self) -> String {
        lock(&self.state).me.clone()
    }

    /// Register a board member; returns its id.
    pub fn member(&self, username: &str, full_name: &str) -> String {
        let mut s = lock(&self.state);
        let id = s.mint(now() - 300 * 86_400);
        s.members.push(Member {
            id: id.clone(),
            username: username.to_string(),
            full_name: full_name.to_string(),
        });
        id
    }

    /// Add a list; returns its id.
    pub fn list(&self, name: &str) -> String {
        let mut s = lock(&self.state);
        let id = s.mint(now() - 200 * 86_400);
        s.lists.push(List {
            id: id.clone(),
            name: name.to_string(),
        });
        id
    }

    /// Add a card, created a week ago, at the bottom of the list named `list`; returns
    /// its id.
    pub fn card(&self, list: &str, short_link: &str, name: &str, desc: &str) -> String {
        let mut s = lock(&self.state);
        let list = s
            .lists
            .iter()
            .find(|l| l.name == list)
            .map(|l| l.id.clone())
            .expect("a seeded list");
        let id = s.mint(now() - 7 * 86_400);
        s.cards.push(Card {
            id: id.clone(),
            short_link: short_link.to_string(),
            name: name.to_string(),
            desc: desc.to_string(),
            list,
            labels: Vec::new(),
            members: Vec::new(),
            checklists: Vec::new(),
            closed: false,
            due_complete: false,
        });
        id
    }

    /// Put a checklist on a card, its items as `(text, complete)`; returns the item ids.
    pub fn checklist(&self, card: &str, name: &str, items: &[(&str, bool)]) -> Vec<String> {
        let mut s = lock(&self.state);
        let secs = now();
        let id = s.mint(secs);
        let items: Vec<(String, String, bool)> = items
            .iter()
            .map(|(text, done)| (s.mint(secs), text.to_string(), *done))
            .collect();
        let ids = items.iter().map(|(id, _, _)| id.clone()).collect();
        s.cards
            .iter_mut()
            .find(|c| c.id == card)
            .expect("a seeded card")
            .checklists
            .push(Checklist {
                id,
                name: name.to_string(),
                items,
            });
        ids
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

    /// Label a card with the board label named `name`, creating it if need be. An empty
    /// `name` is a colour-only label, as Trello sends one.
    pub fn label(&self, card: &str, name: &str) {
        let mut s = lock(&self.state);
        let label = match s.labels.iter().find(|l| l.name == name) {
            Some(l) => l.id.clone(),
            None => {
                let id = s.mint(now());
                s.labels.push(Label {
                    id: id.clone(),
                    name: name.to_string(),
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

    /// Make every request to `route` answer `status`.
    pub fn fail(&self, route: &str, status: u16) {
        lock(&self.state).faults.insert(route.to_string(), status);
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

    let (status, mime, reply) = {
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
        if let Some(reply) = download(&s, &method, &path, &query, authorization.as_deref()) {
            reply
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
            (status, "application/json", reply.to_string().into_bytes())
        }
    };
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        _ => "Status",
    };
    let mut stream = stream;
    let _ = write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {mime}\r\nContent-Length: {}\r\n\
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
        ("GET", ["boards", _, "lists"]) => "lists",
        ("GET", ["boards", _, "members"]) => "members",
        ("GET", ["boards", _, "labels"]) => "labels",
        ("POST", ["boards", _, "labels"]) => "create label",
        ("GET", ["boards", _, "cards"]) => "board cards",
        ("GET", ["lists", _, "cards"]) => "list cards",
        ("GET", ["cards", _]) => "read card",
        ("GET", ["cards", _, "actions"]) => match param("filter").as_deref() {
            Some("createCard") => "created",
            _ => "read comments",
        },
        ("POST", ["cards"]) => "create card",
        ("POST", ["cards", _, "actions", "comments"]) => "post comment",
        ("PUT", ["cards", _, "actions", _, "comments"]) => "edit comment",
        ("DELETE", ["cards", _, "actions", _, "comments"]) => "delete comment",
        ("PUT", ["cards", _]) => {
            if param("idList").is_some() || payload.get("idList").is_some() {
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
        _ => return not_found,
    };
    if let Some(status) = s.faults.get(name) {
        return (*status, json!({ "message": "injected" }));
    }
    if segs[0] == "boards" && segs[1] != BOARD {
        return not_found;
    }
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
    match name {
        "me" => (200, s.member_json(&s.me.clone())),
        "lists" => (
            200,
            s.lists
                .iter()
                .map(|l| json!({ "id": l.id, "name": l.name }))
                .collect(),
        ),
        "members" => (
            200,
            s.members.iter().map(|m| s.member_json(&m.id)).collect(),
        ),
        "labels" => (200, s.labels.iter().map(|l| s.label_json(&l.id)).collect()),
        "create label" => {
            let id = s.mint(now());
            let name = param("name").unwrap_or_default();
            s.labels.push(Label {
                id: id.clone(),
                name: name.clone(),
            });
            (200, json!({ "id": id, "name": name, "color": null }))
        }
        "board cards" => (
            200,
            s.cards
                .iter()
                .filter(|c| !c.closed)
                .map(|c| {
                    json!({
                        "id": c.id,
                        "name": c.name,
                        "labels": c.labels.iter().map(|l| s.label_json(l)).collect::<Vec<_>>(),
                    })
                })
                .collect(),
        ),
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
        "read card" => match card_at(s) {
            Some(at) if !s.cards[at].closed => {
                let with_actions = param("actions").as_deref() == Some("commentCard");
                (200, s.card_json(&s.cards[at], with_actions))
            }
            _ => not_found,
        },
        "read comments" => match card_at(s) {
            Some(at) => (200, Value::Array(s.thread(&s.cards[at].id))),
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
            if !s.lists.iter().any(|l| l.id == list) {
                return (400, json!("invalid value for idList"));
            }
            let id = s.mint(now());
            let card = Card {
                short_link: id[id.len() - 8..].to_string(),
                id,
                name: payload["name"].as_str().unwrap_or("").to_string(),
                desc: payload["desc"].as_str().unwrap_or("").to_string(),
                list,
                labels: Vec::new(),
                members: Vec::new(),
                checklists: Vec::new(),
                closed: false,
                due_complete: false,
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
            let text = payload["text"].as_str().unwrap_or("").to_string();
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
            s.comments.push(comment);
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
                return (200, json!({ "_value": null }));
            }
            let comment = &mut s.comments[at];
            comment.text = payload["text"].as_str().unwrap_or("").to_string();
            comment.edited = Some(now());
            let comment = comment.clone();
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
                    s.place(card, Some(at), Pos::read(field("pos").as_ref(), Pos::Top));
                }
                "archive card" => s.cards[at].closed = param("closed").as_deref() == Some("true"),
                _ => s.cards[at].due_complete = param("dueComplete").as_deref() == Some("true"),
            }
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
        _ => not_found,
    }
}
