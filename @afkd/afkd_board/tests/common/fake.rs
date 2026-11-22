//! A stand-in for one board on app.afkd.sh: a loopback HTTP/1.1 server on a thread of its own,
//! std only, answering the board's API as the site's README ("A board's API") and its
//! `server/src/http/board_api.rs` define it — the four reads, the board, its cards filtered and
//! in the board's order, one card by its number and a card's comments, and every write: a list
//! made, renamed, moved and deleted, a card made, changed, moved, labelled, blocked, ticked,
//! commented on, archived and deleted.
//!
//! It holds one board: its uuid, its name, its one key, its lists in order and its labels, and
//! the cards and comments a test seeds. A request without the key is `401`, as the API answers
//! one. A write changes what the next read answers: the next card number, a label made in the
//! next free colour, a checklist, `done` and the blocks, the archive, a deletion, a comment with
//! the run it was made in, and the lists. Each answer is the API's, a `204` included, and so are
//! its refusals: a blocked card marked done names the cards that are not done, a block that makes
//! a chain back names the chain, a list holding cards that are not archived names the count, and
//! a label the board lacks taken off is `400`.
//!
//! Each request is recorded with its JSON body ([`FakeBoard::seen`]), and each the plugin should
//! never have sent — another method or path, a query key that is no filter, a list the board
//! lacks, a `limit` outside 1 to 250, a filter sent twice, a body field the endpoint does not
//! take, a `run` that is not exactly `{machine, service, instance, id}`, `at` with `after`, a
//! label the board lacks taken off — is recorded too ([`FakeBoard::invalid_requests`]), and
//! answered as the API would.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;

use serde_json::{json, Map, Value};

/// The moment the first seeded card is made, a whole second: 2026-11-14T09:30:00Z.
pub const T0: u64 = 1_794_648_600;

/// What the API answers a request without the board's key.
pub const KEY_REFUSED: &str = "the board key is missing, unknown or revoked";

/// The name of the key, which a comment it made is by.
pub const KEY_NAME: &str = "nightly";

/// The colours a label is made in, as the site lists them.
const LABEL_COLOURS: [&str; 8] = [
    "blue", "amber", "cyan", "pink", "red", "violet", "green", "lime",
];

/// The filters `GET /api/board/cards` takes.
const FILTERS: &[&str] = &[
    "list",
    "labels",
    "without_labels",
    "text",
    "archived",
    "done",
    "blocked",
    "limit",
];

/// A request as the fake saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen {
    pub method: String,
    pub path: String,
    /// The query's pairs, decoded, in order.
    pub query: Vec<(String, String)>,
    pub authorization: Option<String>,
    /// The request's JSON body, read by its `Content-Length`; `None` when it has none.
    pub body: Option<Value>,
}

/// A card to seed: everything but its number, its place and its times, which the fake gives
/// it.
#[derive(Debug, Clone, Default)]
pub struct NewCard {
    pub list: String,
    pub title: String,
    pub description: String,
    pub labels: Vec<String>,
    pub checklist: Vec<(String, bool)>,
    pub done: bool,
    /// The numbers of the cards it is blocked by.
    pub blocked_by: Vec<i64>,
}

impl NewCard {
    /// A card titled `title` in the list `list`, with nothing else set.
    pub fn new(list: &str, title: &str) -> Self {
        Self {
            list: list.to_string(),
            title: title.to_string(),
            ..Self::default()
        }
    }
}

/// A run a comment is made in, as the API names one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub machine: String,
    pub service: String,
    pub instance: i64,
    pub id: String,
}

#[derive(Debug, Clone)]
struct Card {
    id: String,
    number: i64,
    new: NewCard,
    /// When it was archived, the newest first in the board's order; `None` while it is not.
    archived_at: Option<u64>,
    place: i64,
    created_at: u64,
}

#[derive(Debug, Clone)]
struct Comment {
    id: String,
    card: i64,
    body: String,
    by: Option<String>,
    by_key: bool,
    run: Option<Run>,
    created_at: u64,
}

#[derive(Debug, Default)]
struct Store {
    board_id: String,
    name: String,
    key: String,
    lists: Vec<(String, String)>,
    labels: Vec<(String, String)>,
    cards: Vec<Card>,
    comments: Vec<Comment>,
    seen: Vec<Seen>,
    invalid: Vec<String>,
    /// The number the next card is made with, which a deleted card's is never given again.
    next_number: i64,
    /// How many instants a write has been given: its changes are stamped a second apart.
    clock: u64,
}

/// What a test reads of a card: everything the API holds of it but its times.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardState {
    pub title: String,
    pub description: String,
    pub list: String,
    pub labels: Vec<String>,
    pub checklist: Vec<(String, bool)>,
    pub done: bool,
    pub blocked_by: Vec<i64>,
    pub archived: bool,
}

/// What a test reads of a list: its id, and the numbers of the cards in it, not archived ones in
/// their order and then archived ones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListState {
    pub id: String,
    pub cards: Vec<i64>,
    pub archived: Vec<i64>,
}

/// A uuid no other in this test process has: version 4's shape, its digits a counter.
fn mint() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(1);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let pid = u64::from(std::process::id());
    format!(
        "{:08x}-{:04x}-4{:03x}-8{:03x}-{:012x}",
        pid & 0xffff_ffff,
        (n >> 32) & 0xffff,
        (n >> 20) & 0xfff,
        (n >> 8) & 0xfff,
        n
    )
}

fn lock(store: &Mutex<Store>) -> MutexGuard<'_, Store> {
    store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// One board, served on loopback.
pub struct FakeBoard {
    base: String,
    store: Arc<Mutex<Store>>,
}

impl FakeBoard {
    /// A board named `name`, reached with `key`, holding `lists` in order and `labels`, each a
    /// name and its colour.
    pub fn start(name: &str, key: &str, lists: &[&str], labels: &[(&str, &str)]) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let base = format!("http://{}", listener.local_addr().expect("local addr"));
        let store = Arc::new(Mutex::new(Store {
            board_id: mint(),
            name: name.to_string(),
            key: key.to_string(),
            lists: lists.iter().map(|l| (mint(), (*l).to_string())).collect(),
            labels: labels
                .iter()
                .map(|(n, c)| ((*n).to_string(), (*c).to_string()))
                .collect(),
            next_number: 1,
            ..Store::default()
        }));
        let shared = Arc::clone(&store);
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let store = Arc::clone(&shared);
                thread::spawn(move || serve(stream, &store));
            }
        });
        Self { base, store }
    }

    /// The board's address, as an `afkd_board.Board`'s `base_url` writes it.
    pub fn base_url(&self) -> &str {
        &self.base
    }

    /// The board's key.
    pub fn key(&self) -> String {
        lock(&self.store).key.clone()
    }

    /// The board's uuid, which its handles' keys carry.
    pub fn board_id(&self) -> String {
        lock(&self.store).board_id.clone()
    }

    /// The uuid of the list named `name`.
    pub fn list_id(&self, name: &str) -> String {
        let s = lock(&self.store);
        let list = s.lists.iter().find(|(_, n)| n == name).expect("a list");
        list.0.clone()
    }

    /// Seed `card` at the bottom of its list, made a minute after the card before it; its
    /// number comes back.
    pub fn card(&self, card: NewCard) -> i64 {
        let mut s = lock(&self.store);
        assert!(
            s.lists.iter().any(|(_, n)| *n == card.list),
            "no list {}",
            card.list
        );
        let number = s.next_number;
        s.next_number += 1;
        let place = s.order_of(&card.list).len() as i64;
        s.cards.push(Card {
            id: mint(),
            number,
            new: card,
            archived_at: None,
            place,
            created_at: T0 + 60 * (number as u64 - 1),
        });
        number
    }

    /// The uuid of the card `number`.
    pub fn card_id(&self, number: i64) -> String {
        lock(&self.store).card(number).id.clone()
    }

    /// The card `number`'s address.
    pub fn card_url(&self, number: i64) -> String {
        let s = lock(&self.store);
        s.url(number)
    }

    /// Archive the card `number` at `at`, unix seconds.
    pub fn archive(&self, number: i64, at: u64) {
        lock(&self.store).card_mut(number).archived_at = Some(at);
    }

    /// Comment `body` on the card `number`, `seconds` after [`T0`]: by the person `by`, or by
    /// the key named `by` when `by_key`, in `run` when given. Its uuid comes back.
    pub fn comment(
        &self,
        number: i64,
        body: &str,
        by: &str,
        by_key: bool,
        run: Option<Run>,
        seconds: u64,
    ) -> String {
        let mut s = lock(&self.store);
        s.card(number);
        let id = mint();
        s.comments.push(Comment {
            id: id.clone(),
            card: number,
            body: body.to_string(),
            by: Some(by.to_string()),
            by_key,
            run,
            created_at: T0 + seconds,
        });
        id
    }

    /// Delete the account of the person `name`: their comments then name nobody.
    pub fn delete_account(&self, name: &str) {
        let mut s = lock(&self.store);
        for comment in &mut s.comments {
            if !comment.by_key && comment.by.as_deref() == Some(name) {
                comment.by = None;
            }
        }
    }

    /// The card `number` as the board holds it now, or `None` once it is deleted.
    pub fn card_state(&self, number: i64) -> Option<CardState> {
        let s = lock(&self.store);
        let card = s.cards.iter().find(|c| c.number == number)?;
        Some(CardState {
            title: card.new.title.clone(),
            description: card.new.description.clone(),
            list: card.new.list.clone(),
            labels: card.new.labels.clone(),
            checklist: card.new.checklist.clone(),
            done: card.new.done,
            blocked_by: {
                let mut blocked_by = card.new.blocked_by.clone();
                blocked_by.sort_unstable();
                blocked_by
            },
            archived: card.archived_at.is_some(),
        })
    }

    /// The list named `name` as the board holds it now, or `None` when it has none.
    pub fn list_state(&self, name: &str) -> Option<ListState> {
        let s = lock(&self.store);
        let (id, _) = s.lists.iter().find(|(_, n)| n == name)?;
        let mut archived: Vec<&Card> = s
            .cards
            .iter()
            .filter(|c| c.new.list == name && c.archived_at.is_some())
            .collect();
        archived.sort_by_key(|c| std::cmp::Reverse(c.archived_at));
        Some(ListState {
            id: id.clone(),
            cards: s.order_of(name),
            archived: archived.iter().map(|c| c.number).collect(),
        })
    }

    /// The names of the board's lists, in order.
    pub fn list_names(&self) -> Vec<String> {
        lock(&self.store)
            .lists
            .iter()
            .map(|(_, n)| n.clone())
            .collect()
    }

    /// The board's labels, each by name with its colour, in the order they were made.
    pub fn labels(&self) -> Vec<(String, String)> {
        lock(&self.store).labels.clone()
    }

    /// The comments on the card `number`, oldest first, each with who made it and the run it was
    /// made in: its body, its author and whether a key made it, and its run.
    pub fn comments_on(&self, number: i64) -> Vec<(String, Option<String>, bool, Option<Run>)> {
        let s = lock(&self.store);
        let mut comments: Vec<&Comment> = s.comments.iter().filter(|m| m.card == number).collect();
        comments.sort_by_key(|m| m.created_at);
        comments
            .iter()
            .map(|m| (m.body.clone(), m.by.clone(), m.by_key, m.run.clone()))
            .collect()
    }

    /// Every request the fake has seen, in order.
    pub fn seen(&self) -> Vec<Seen> {
        lock(&self.store).seen.clone()
    }

    /// Every request the fake has seen that is no `GET`, in order: the writes the plugin sent.
    pub fn writes(&self) -> Vec<Seen> {
        let mut seen = self.seen();
        seen.retain(|s| s.method != "GET");
        seen
    }

    /// Each request the plugin should never have sent, as what was wrong with it.
    pub fn invalid_requests(&self) -> Vec<String> {
        lock(&self.store).invalid.clone()
    }
}

impl Store {
    fn card(&self, number: i64) -> &Card {
        self.cards
            .iter()
            .find(|c| c.number == number)
            .expect("a card")
    }

    fn card_mut(&mut self, number: i64) -> &mut Card {
        self.cards
            .iter_mut()
            .find(|c| c.number == number)
            .expect("a card")
    }

    fn url(&self, number: i64) -> String {
        format!(
            "https://app.afkd.sh/acme/boards/{}/{number}",
            self.name.replace(' ', "-").to_lowercase()
        )
    }

    /// Whether `card` is blocked: one of the cards it is blocked by is not done.
    fn blocked(&self, card: &Card) -> bool {
        card.new.blocked_by.iter().any(|n| !self.card(*n).new.done)
    }

    fn card_answer(&self, card: &Card) -> Value {
        let mut blocked_by = card.new.blocked_by.clone();
        blocked_by.sort_unstable();
        let blocking: Vec<i64> = self
            .cards
            .iter()
            .filter(|c| c.new.blocked_by.contains(&card.number))
            .map(|c| c.number)
            .collect();
        let updated = self
            .comments
            .iter()
            .filter(|m| m.card == card.number)
            .map(|m| m.created_at)
            .chain([card.created_at])
            .max()
            .unwrap_or(card.created_at);
        let checklist: Vec<Value> = card
            .new
            .checklist
            .iter()
            .map(|(text, done)| json!({"text": text, "done": done}))
            .collect();
        json!({
            "id": card.id,
            "number": card.number,
            "title": card.new.title,
            "description": card.new.description,
            "list": card.new.list,
            "labels": card.new.labels,
            "checklist": checklist,
            "done": card.new.done,
            "blocked": self.blocked(card),
            "blocked_by": blocked_by,
            "blocking": blocking,
            "archived": card.archived_at.is_some(),
            "url": self.url(card.number),
            "created_at": rfc3339(card.created_at),
            "updated_at": rfc3339(updated),
        })
    }

    fn comment_answer(comment: &Comment) -> Value {
        let run = comment.run.as_ref().map(|r| {
            json!({"machine": r.machine, "service": r.service, "instance": r.instance, "id": r.id})
        });
        json!({
            "id": comment.id,
            "body": comment.body,
            "by": comment.by,
            "by_key": comment.by_key,
            "run": run,
            "created_at": rfc3339(comment.created_at),
        })
    }

    /// `GET /api/board/cards` with `query`, filtered and ordered as the site's query does.
    fn cards(&mut self, query: &[(String, String)]) -> Reply {
        let (mut list, mut labels, mut without, mut text) = (None, Vec::new(), Vec::new(), None);
        let (mut archived, mut done, mut blocked, mut limit) = (false, None, None, 50usize);
        let mut once = Vec::new();
        for (name, value) in query {
            if name != "labels" && name != "without_labels" {
                if once.contains(name) {
                    self.invalid.push(format!("`{name}` sent twice"));
                }
                once.push(name.clone());
            }
            match name.as_str() {
                "list" => list = Some(value.clone()),
                "labels" => labels.push(value.clone()),
                "without_labels" => without.push(value.clone()),
                "text" => text = Some(value.to_lowercase()),
                "archived" | "done" | "blocked" => {
                    let yes = match value.as_str() {
                        "true" => true,
                        "false" => false,
                        _ => {
                            self.invalid.push(format!("`{name}` is {value:?}"));
                            return problem(400, &format!("`{name}` is `true` or `false`."));
                        }
                    };
                    match name.as_str() {
                        "archived" => archived = yes,
                        "done" => done = Some(yes),
                        _ => blocked = Some(yes),
                    }
                }
                "limit" => match value.parse::<usize>() {
                    Ok(n) if (1..=250).contains(&n) => limit = n,
                    _ => {
                        self.invalid.push(format!("`limit` is {value:?}"));
                        return problem(400, "`limit` is a number from 1 to 250.");
                    }
                },
                _ => {
                    self.invalid.push(format!("`{name}` is no filter"));
                    return problem(
                        400,
                        &format!(
                            "`{name}` is no filter of the cards; they are {}.",
                            FILTERS.join(", ")
                        ),
                    );
                }
            }
        }
        if let Some(name) = &list {
            if !self.lists.iter().any(|(_, n)| n == name) {
                self.invalid.push(format!("no list `{name}`"));
                let has: Vec<String> = self.lists.iter().map(|(_, n)| format!("`{n}`")).collect();
                return problem(
                    400,
                    &format!("The board has no list `{name}`; it has {}.", and_list(&has)),
                );
            }
        }
        if let Some(name) = labels
            .iter()
            .chain(&without)
            .find(|name| !self.labels.iter().any(|(n, _)| n == *name))
        {
            let has: Vec<String> = self.labels.iter().map(|(n, _)| format!("`{n}`")).collect();
            return problem(
                400,
                &format!(
                    "The board has no label `{name}`; it has {}.",
                    and_list(&has)
                ),
            );
        }
        let position = |card: &Card| {
            self.lists
                .iter()
                .position(|(_, n)| *n == card.new.list)
                .unwrap_or(usize::MAX)
        };
        let mut kept: Vec<&Card> = self
            .cards
            .iter()
            .filter(|c| c.archived_at.is_some() == archived)
            .filter(|c| list.as_ref().is_none_or(|l| c.new.list == *l))
            .filter(|c| {
                text.as_ref().is_none_or(|t| {
                    c.new.title.to_lowercase().contains(t)
                        || c.new.description.to_lowercase().contains(t)
                })
            })
            .filter(|c| labels.iter().all(|l| c.new.labels.contains(l)))
            .filter(|c| !without.iter().any(|l| c.new.labels.contains(l)))
            .filter(|c| done.is_none_or(|d| c.new.done == d))
            .filter(|c| blocked.is_none_or(|b| self.blocked(c) == b))
            .collect();
        // By list place, then not archived first and the last archived first, then place,
        // then the newest number.
        kept.sort_by_key(|c| {
            (
                position(c),
                c.archived_at.map_or(0, |at| u64::MAX - at),
                if c.archived_at.is_none() { c.place } else { 0 },
                -c.number,
            )
        });
        kept.truncate(limit);
        let answers: Vec<Value> = kept.iter().map(|c| self.card_answer(c)).collect();
        (200, Vec::new(), Value::Array(answers))
    }
}

/// A request as the fake read it.
struct Request {
    method: String,
    path: String,
    query: Vec<(String, String)>,
    authorization: Option<String>,
    content_type: Option<String>,
    /// How many bytes its body is, and its JSON when it is any.
    body_len: usize,
    body: Option<Value>,
}

/// The fields of a request's JSON object.
type Fields = Map<String, Value>;

/// Where a card or a list goes among those it is with.
enum Spot<T> {
    Top,
    Bottom,
    After(T),
}

/// The card number a path writes, which is its digits alone, or `None`.
fn card_number(text: &str) -> Option<i64> {
    let canonical =
        !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) && !text.starts_with('0');
    canonical.then(|| text.parse().ok()).flatten()
}

/// The refusal `404` of the card `number`, as a path wrote it.
fn no_card(number: &str) -> Reply {
    problem(404, &format!("The board has no card #{number}."))
}

/// The next colour a label is made in: the one the fewest labels have, the earliest on a tie.
fn next_colour(labels: &[(String, String)]) -> &'static str {
    let mut next = LABEL_COLOURS[0];
    let mut fewest = usize::MAX;
    for colour in LABEL_COLOURS {
        let have = labels.iter().filter(|(_, c)| c == colour).count();
        if have < fewest {
            (next, fewest) = (colour, have);
        }
    }
    next
}

/// Why the card `number` blocked by the cards `waiting`, which are not done, is not marked done.
fn blocked_words(number: i64, waiting: &[i64]) -> String {
    let cards: Vec<String> = waiting.iter().map(|c| format!("#{c}")).collect();
    let are = if waiting.len() == 1 { "is" } else { "are" };
    format!(
        "Card #{number} is blocked by {}, which {are} not done.",
        and_list(&cards)
    )
}

/// Why a block of the card `number` by the first card of `chain`, which leads back to it, is
/// refused.
fn chain_words(number: i64, chain: &[i64]) -> String {
    let Some((first, rest)) = chain.split_first() else {
        return format!("Card #{number} cannot be blocked by itself.");
    };
    let mut words = format!("Card #{number} cannot be blocked by #{first}, since #{first}");
    for (at, card) in rest.iter().enumerate() {
        let then = if at == 0 { " is" } else { ", which is" };
        words.push_str(&format!("{then} blocked by #{card}"));
    }
    words.push('.');
    words
}

/// Why the list `name`, which holds `held` cards that are not archived, was not deleted.
fn list_held(name: &str, held: usize) -> String {
    if held == 1 {
        format!("The list `{name}` holds 1 card that is not archived; move or archive it first.")
    } else {
        format!(
            "The list `{name}` holds {held} cards that are not archived; move or archive them \
             first."
        )
    }
}

impl Store {
    /// The numbers of the cards in the list `list` that are not archived, in their order.
    fn order_of(&self, list: &str) -> Vec<i64> {
        let mut cards: Vec<&Card> = self
            .cards
            .iter()
            .filter(|c| c.new.list == list && c.archived_at.is_none())
            .collect();
        cards.sort_by_key(|c| c.place);
        cards.iter().map(|c| c.number).collect()
    }

    /// The next instant a write is stamped at, a second after the one before.
    fn tick(&mut self) -> u64 {
        self.clock += 1;
        T0 + 86_400 + self.clock
    }

    /// Record `why` as a request the plugin should never have sent, and refuse it `400`.
    fn refuse(&mut self, why: String, error: &str) -> Reply {
        self.invalid.push(why);
        problem(400, error)
    }

    /// The JSON object a write sends: each field of it one of `takes`, each of `required` there;
    /// a request with no body, when `bare`, sends none. Anything else is `400` and recorded.
    fn fields(
        &mut self,
        req: &Request,
        takes: &[&str],
        required: &[&str],
        bare: bool,
    ) -> Result<Fields, Reply> {
        let what = format!("{} {}", req.method, req.path);
        let body = match &req.body {
            Some(Value::Object(fields)) => fields.clone(),
            None if bare && req.body_len == 0 => Fields::new(),
            _ => {
                return Err(self.refuse(
                    format!("{what}: a body that is no JSON object"),
                    "Failed to parse the request body as a JSON object",
                ))
            }
        };
        if let Some(key) = body.keys().find(|k| !takes.contains(&k.as_str())).cloned() {
            return Err(self.refuse(
                format!("{what}: `{key}` is a field it does not take"),
                &format!("unknown field `{key}`"),
            ));
        }
        if let Some(key) = required.iter().find(|k| !body.contains_key(**k)) {
            return Err(self.refuse(
                format!("{what}: `{key}` is missing"),
                &format!("missing field `{key}`"),
            ));
        }
        Ok(body)
    }

    /// The string `key` of `fields`, or `None` when it has none.
    fn text(&mut self, fields: &Fields, key: &str) -> Result<Option<String>, Reply> {
        match fields.get(key) {
            None => Ok(None),
            Some(Value::String(text)) => Ok(Some(text.clone())),
            Some(other) => Err(self.refuse(
                format!("`{key}` is {other}, not a string"),
                &format!("invalid type for `{key}`"),
            )),
        }
    }

    /// The strings `key` of `fields` lists, none when it has none.
    fn texts(&mut self, fields: &Fields, key: &str) -> Result<Vec<String>, Reply> {
        let Some(value) = fields.get(key) else {
            return Ok(Vec::new());
        };
        let items: Option<Vec<String>> = value.as_array().and_then(|items| {
            items
                .iter()
                .map(|item| item.as_str().map(str::to_string))
                .collect()
        });
        items.ok_or_else(|| {
            self.invalid
                .push(format!("`{key}` is {value}, not a list of strings"));
            problem(400, &format!("invalid type for `{key}`"))
        })
    }

    /// The boolean `key` of `fields`, or `None` when it has none.
    fn flag(&mut self, fields: &Fields, key: &str) -> Result<Option<bool>, Reply> {
        match fields.get(key) {
            None => Ok(None),
            Some(Value::Bool(flag)) => Ok(Some(*flag)),
            Some(other) => Err(self.refuse(
                format!("`{key}` is {other}, not a boolean"),
                &format!("invalid type for `{key}`"),
            )),
        }
    }

    /// The whole number `key` of `fields`, or `None` when it has none.
    fn whole(&mut self, fields: &Fields, key: &str) -> Result<Option<i64>, Reply> {
        match fields.get(key) {
            None => Ok(None),
            Some(value) => value.as_i64().map(Some).ok_or_else(|| {
                self.invalid
                    .push(format!("`{key}` is {value}, not a whole number"));
                problem(400, &format!("invalid type for `{key}`"))
            }),
        }
    }

    /// The run `fields` name, `run`, which is exactly `{machine, service, instance, id}`: a
    /// machine, a service and an id of 1 to 100 characters with no control character, and an
    /// instance from 0. Anything else is `400` and recorded.
    fn run_of(&mut self, fields: &Fields, req: &Request) -> Result<Option<Run>, Reply> {
        let Some(run) = fields.get("run") else {
            return Ok(None);
        };
        let parsed = run
            .as_object()
            .filter(|run| run.len() == 4)
            .and_then(|run| {
                let word = |key: &str| {
                    run.get(key)?
                        .as_str()
                        .filter(|t| {
                            !t.is_empty()
                                && t.chars().count() <= 100
                                && !t.chars().any(char::is_control)
                        })
                        .map(str::to_string)
                };
                Some(Run {
                    machine: word("machine")?,
                    service: word("service")?,
                    instance: run.get("instance")?.as_i64().filter(|i| *i >= 0)?,
                    id: word("id")?,
                })
            });
        parsed.map(Some).ok_or_else(|| {
            self.invalid.push(format!(
                "{} {}: `run` is {run}, not exactly the machine, service, instance and id",
                req.method, req.path
            ));
            problem(
                400,
                "`run` is {machine, service, instance, id}: it is not a run.",
            )
        })
    }

    /// The place `fields` name, `at` or `after`: `Some` of `Top` and `Bottom` for `at`, of
    /// `After(())` for `after`, `None` for neither. Both, and an `at` of another word, are `400`
    /// and recorded.
    fn at_after(&mut self, fields: &Fields, req: &Request) -> Result<Option<Spot<()>>, Reply> {
        let at = self.text(fields, "at")?;
        let what = format!("{} {}", req.method, req.path);
        match (at.as_deref(), fields.contains_key("after")) {
            (Some(_), true) => Err(self.refuse(
                format!("{what}: `at` with `after`"),
                "Name a place with `at` or `after`, not both.",
            )),
            (Some("top"), false) => Ok(Some(Spot::Top)),
            (Some("bottom"), false) => Ok(Some(Spot::Bottom)),
            (Some(other), false) => Err(self.refuse(
                format!("{what}: `at` is {other:?}"),
                "`at` is `top` or `bottom`.",
            )),
            (None, true) => Ok(Some(Spot::After(()))),
            (None, false) => Ok(None),
        }
    }

    /// The place of a card in the list `list`, as `fields` name it; `own` is the card being
    /// moved, which cannot go after itself.
    fn card_spot(
        &mut self,
        fields: &Fields,
        req: &Request,
        list: &str,
        own: Option<i64>,
    ) -> Result<Option<Spot<i64>>, Reply> {
        let Some(spot) = self.at_after(fields, req)? else {
            return Ok(None);
        };
        Ok(Some(match spot {
            Spot::Top => Spot::Top,
            Spot::Bottom => Spot::Bottom,
            Spot::After(()) => {
                let after = self.whole(fields, "after")?.unwrap_or_default();
                if Some(after) == own {
                    return Err(problem(400, "A card cannot go after itself."));
                }
                if !self.order_of(list).contains(&after) {
                    return Err(problem(
                        400,
                        &format!("`after` names card #{after}, which is not in `{list}`."),
                    ));
                }
                Spot::After(after)
            }
        }))
    }

    /// The place of a list among the board's, as `fields` name it, `after` by id, which must be a
    /// list of the board; `own` is the id of the list being moved, which cannot go after itself.
    fn list_spot(
        &mut self,
        fields: &Fields,
        req: &Request,
        own: Option<&str>,
    ) -> Result<Option<Spot<String>>, Reply> {
        let Some(spot) = self.at_after(fields, req)? else {
            return Ok(None);
        };
        Ok(Some(match spot {
            Spot::Top => Spot::Top,
            Spot::Bottom => Spot::Bottom,
            Spot::After(()) => {
                let after = self.text(fields, "after")?.unwrap_or_default();
                if Some(after.as_str()) == own {
                    return Err(problem(400, "A list cannot go after itself."));
                }
                if !self.lists.iter().any(|(id, _)| *id == after) {
                    self.invalid
                        .push(format!("{} {}: no list {after:?}", req.method, req.path));
                    let lists: Vec<String> = self
                        .lists
                        .iter()
                        .map(|(id, name)| format!("`{name}` ({id})"))
                        .collect();
                    let has = if lists.is_empty() {
                        "it has none".to_string()
                    } else {
                        format!("its lists are {}", and_list(&lists))
                    };
                    return Err(problem(
                        400,
                        &format!("`after` names no list of the board; {has}."),
                    ));
                }
                Spot::After(after)
            }
        }))
    }

    /// The refusal `400` of the list `name`, which the board lacks, recorded as a request that
    /// should never have been sent.
    fn no_list(&mut self, name: &str) -> Reply {
        self.invalid.push(format!("no list `{name}`"));
        let has: Vec<String> = self.lists.iter().map(|(_, n)| format!("`{n}`")).collect();
        problem(
            400,
            &format!("The board has no list `{name}`; it has {}.", and_list(&has)),
        )
    }

    /// Put the card `number` in the list `list` at `spot`, the others of the list kept in order.
    fn put_card(&mut self, number: i64, list: &str, spot: Spot<i64>) {
        let mut order = self.order_of(list);
        order.retain(|n| *n != number);
        let at = match spot {
            Spot::Top => 0,
            Spot::Bottom => order.len(),
            Spot::After(after) => order
                .iter()
                .position(|n| *n == after)
                .map_or(order.len(), |i| i + 1),
        };
        order.insert(at, number);
        self.card_mut(number).new.list = list.to_string();
        for (place, n) in order.into_iter().enumerate() {
            self.card_mut(n).place = place as i64;
        }
    }

    /// The label `name`, made in the next free colour when the board has none of that name.
    fn ensure_label(&mut self, name: &str) {
        if !self.labels.iter().any(|(n, _)| n == name) {
            let colour = next_colour(&self.labels);
            self.labels.push((name.to_string(), colour.to_string()));
        }
    }

    /// Make every done card the card `number` blocks, and so on down the chain, not done.
    fn undo_blocked(&mut self, number: i64) {
        let blocked: Vec<i64> = self
            .cards
            .iter()
            .filter(|c| c.new.done && c.new.blocked_by.contains(&number))
            .map(|c| c.number)
            .collect();
        for card in blocked {
            self.card_mut(card).new.done = false;
            self.undo_blocked(card);
        }
    }

    /// The cards the card `number` is blocked by that are not done, in order.
    fn waiting_on(&self, number: i64) -> Vec<i64> {
        let mut waiting: Vec<i64> = self
            .card(number)
            .new
            .blocked_by
            .iter()
            .copied()
            .filter(|n| !self.card(*n).new.done)
            .collect();
        waiting.sort_unstable();
        waiting
    }

    /// The shortest chain of blocks from the card `from` to the card `to`, both ends in it, or
    /// `None` when `from` is not blocked, however far down, by `to`.
    fn chain(&self, from: i64, to: i64) -> Option<Vec<i64>> {
        let mut paths: Vec<Vec<i64>> = vec![vec![from]];
        let mut seen = vec![from];
        while !paths.is_empty() {
            let mut next = Vec::new();
            for path in &paths {
                let last = *path.last()?;
                let mut blockers = self.card(last).new.blocked_by.clone();
                blockers.sort_unstable();
                for blocker in blockers {
                    if seen.contains(&blocker) {
                        continue;
                    }
                    let mut longer = path.clone();
                    longer.push(blocker);
                    if blocker == to {
                        return Some(longer);
                    }
                    seen.push(blocker);
                    next.push(longer);
                }
            }
            paths = next;
        }
        None
    }

    /// The card the path's `number` names, or the refusal `404`.
    fn card_of(&self, number: &str) -> Result<i64, Reply> {
        card_number(number)
            .filter(|n| self.cards.iter().any(|c| c.number == *n))
            .ok_or_else(|| no_card(number))
    }

    /// The answer `status` with the card `number` as it is.
    fn card_reply(&self, status: u16, number: i64) -> Reply {
        (status, Vec::new(), self.card_answer(self.card(number)))
    }

    /// `POST /api/board/lists`.
    fn add_list(&mut self, req: &Request) -> Reply {
        let fields = match self.fields(req, &["name", "at", "after"], &["name"], false) {
            Ok(fields) => fields,
            Err(refused) => return refused,
        };
        let name = match self.text(&fields, "name") {
            Ok(name) => name.unwrap_or_default().trim().to_string(),
            Err(refused) => return refused,
        };
        if name.is_empty() {
            return problem(400, "A list needs a name.");
        }
        let spot = match self.list_spot(&fields, req, None) {
            Ok(spot) => spot,
            Err(refused) => return refused,
        };
        if self.lists.iter().any(|(_, n)| *n == name) {
            return problem(409, &self.list_taken(&name));
        }
        let at = match spot {
            None | Some(Spot::Bottom) => self.lists.len(),
            Some(Spot::Top) => 0,
            Some(Spot::After(id)) => self
                .lists
                .iter()
                .position(|(i, _)| *i == id)
                .map_or(0, |i| i + 1),
        };
        let id = mint();
        self.lists.insert(at, (id.clone(), name.clone()));
        (201, Vec::new(), json!({"id": id, "name": name}))
    }

    /// Why a list cannot be named `name`: another list has it.
    fn list_taken(&self, name: &str) -> String {
        format!(
            "The name `{name}` is taken by another list of {}.",
            self.name
        )
    }

    /// The position of the list whose id is `id`, or the refusal `404`, recorded as a request
    /// that should never have been sent.
    fn list_at(&mut self, id: &str) -> Result<usize, Reply> {
        self.lists.iter().position(|(i, _)| i == id).ok_or_else(|| {
            self.invalid.push(format!("no list with the id {id:?}"));
            problem(404, &format!("The board has no list `{id}`."))
        })
    }

    /// `PATCH /api/board/lists/<id>`.
    fn change_list(&mut self, req: &Request, id: &str) -> Reply {
        let at = match self.list_at(id) {
            Ok(at) => at,
            Err(refused) => return refused,
        };
        let fields = match self.fields(req, &["name", "at", "after"], &[], false) {
            Ok(fields) => fields,
            Err(refused) => return refused,
        };
        let name = match self.text(&fields, "name") {
            Ok(name) => name.map(|name| name.trim().to_string()),
            Err(refused) => return refused,
        };
        if name.as_deref() == Some("") {
            return problem(400, "A list needs a name.");
        }
        let spot = match self.list_spot(&fields, req, Some(id)) {
            Ok(spot) => spot,
            Err(refused) => return refused,
        };
        if let Some(name) = &name {
            if self.lists.iter().any(|(i, n)| i != id && n == name) {
                return problem(409, &self.list_taken(name));
            }
        }
        let old = self.lists[at].1.clone();
        if let Some(name) = &name {
            for card in self.cards.iter_mut().filter(|c| c.new.list == old) {
                card.new.list.clone_from(name);
            }
            self.lists[at].1.clone_from(name);
        }
        if let Some(spot) = spot {
            let list = self.lists.remove(at);
            let to = match spot {
                Spot::Top => 0,
                Spot::Bottom => self.lists.len(),
                Spot::After(after) => self
                    .lists
                    .iter()
                    .position(|(i, _)| *i == after)
                    .map_or(0, |i| i + 1),
            };
            self.lists.insert(to, list);
        }
        let list = self.lists.iter().find(|(i, _)| i == id);
        let name = list.map(|(_, n)| n.clone());
        (200, Vec::new(), json!({"id": id, "name": name}))
    }

    /// `DELETE /api/board/lists/<id>`.
    fn delete_list(&mut self, id: &str) -> Reply {
        let at = match self.list_at(id) {
            Ok(at) => at,
            Err(refused) => return refused,
        };
        let name = self.lists[at].1.clone();
        let held = self.order_of(&name).len();
        if held > 0 {
            return problem(409, &list_held(&name, held));
        }
        self.lists.remove(at);
        self.cards.retain(|c| c.new.list != name);
        (204, Vec::new(), Value::Null)
    }

    /// `POST /api/board/cards`.
    fn add_card(&mut self, req: &Request) -> Reply {
        let takes = [
            "list",
            "title",
            "description",
            "labels",
            "checklist",
            "at",
            "after",
            "run",
        ];
        let fields = match self.fields(req, &takes, &["list", "title"], false) {
            Ok(fields) => fields,
            Err(refused) => return refused,
        };
        match self.made_card(req, &fields) {
            Ok(number) => self.card_reply(201, number),
            Err(refused) => refused,
        }
    }

    /// Make the card `fields` describe, and give its number.
    fn made_card(&mut self, req: &Request, fields: &Fields) -> Result<i64, Reply> {
        let list = self.text(fields, "list")?.unwrap_or_default();
        let title = self
            .text(fields, "title")?
            .unwrap_or_default()
            .trim()
            .to_string();
        let description = self.text(fields, "description")?.unwrap_or_default();
        let labels = self.texts(fields, "labels")?;
        let items = self.texts(fields, "checklist")?;
        self.run_of(fields, req)?;
        if title.is_empty() {
            return Err(problem(400, "A title needs some text."));
        }
        if !self.lists.iter().any(|(_, n)| *n == list) {
            return Err(self.no_list(&list));
        }
        let spot = self.card_spot(fields, req, &list, None)?;
        let mut kept: Vec<String> = Vec::new();
        for label in labels.iter().map(|l| l.trim().to_string()) {
            if !kept.contains(&label) {
                kept.push(label);
            }
        }
        for label in &kept {
            self.ensure_label(label);
        }
        let number = self.next_number;
        self.next_number += 1;
        self.cards.push(Card {
            id: mint(),
            number,
            new: NewCard {
                list: list.clone(),
                title,
                description,
                labels: kept,
                checklist: items
                    .iter()
                    .map(|i| (i.trim().to_string(), false))
                    .collect(),
                done: false,
                blocked_by: Vec::new(),
            },
            archived_at: None,
            place: 0,
            created_at: T0 + 60 * (number as u64 - 1),
        });
        self.put_card(number, &list, spot.unwrap_or(Spot::Bottom));
        Ok(number)
    }

    /// `PATCH /api/board/cards/<number>`.
    fn change_card(&mut self, req: &Request, number: &str) -> Reply {
        let number = match self.card_of(number) {
            Ok(number) => number,
            Err(refused) => return refused,
        };
        let takes = ["title", "description", "list", "at", "after", "done", "run"];
        let fields = match self.fields(req, &takes, &[], false) {
            Ok(fields) => fields,
            Err(refused) => return refused,
        };
        match self.changed_card(req, number, &fields) {
            Ok(()) => self.card_reply(200, number),
            Err(refused) => refused,
        }
    }

    /// Change the card `number` as `fields` say.
    fn changed_card(&mut self, req: &Request, number: i64, fields: &Fields) -> Result<(), Reply> {
        let title = self.text(fields, "title")?.map(|t| t.trim().to_string());
        let description = self.text(fields, "description")?;
        let list = self.text(fields, "list")?;
        let done = self.flag(fields, "done")?;
        self.run_of(fields, req)?;
        if title.as_deref() == Some("") {
            return Err(problem(400, "A title needs some text."));
        }
        let own = self.card(number).new.list.clone();
        let target = list.clone().unwrap_or_else(|| own.clone());
        if let Some(list) = &list {
            if !self.lists.iter().any(|(_, n)| n == list) {
                return Err(self.no_list(list));
            }
        }
        let spot = self.card_spot(fields, req, &target, Some(number))?;
        let moves = list.is_some() || spot.is_some();
        if moves && self.card(number).archived_at.is_some() {
            return Err(problem(
                409,
                &format!("Card #{number} is archived; unarchive it before it moves."),
            ));
        }
        let waiting = self.waiting_on(number);
        if done == Some(true) && !waiting.is_empty() {
            return Err(problem(409, &blocked_words(number, &waiting)));
        }
        if let Some(title) = title {
            self.card_mut(number).new.title = title;
        }
        if let Some(description) = description {
            self.card_mut(number).new.description = description;
        }
        match (spot, target != own) {
            (Some(spot), _) => self.put_card(number, &target, spot),
            (None, true) => self.put_card(number, &target, Spot::Bottom),
            (None, false) => {}
        }
        if let Some(done) = done {
            self.card_mut(number).new.done = done;
            if !done {
                self.undo_blocked(number);
            }
        }
        Ok(())
    }

    /// `POST` or `DELETE /api/board/cards/<number>/labels/<name>`.
    fn label(&mut self, req: &Request, number: &str, name: &str) -> Reply {
        let number = match self.card_of(number) {
            Ok(number) => number,
            Err(refused) => return refused,
        };
        let fields = match self.fields(req, &["run"], &[], true) {
            Ok(fields) => fields,
            Err(refused) => return refused,
        };
        if let Err(refused) = self.run_of(&fields, req) {
            return refused;
        }
        if req.method == "POST" {
            self.ensure_label(name);
            let labels = &mut self.card_mut(number).new.labels;
            if !labels.iter().any(|l| l == name) {
                labels.push(name.to_string());
            }
        } else {
            if !self.labels.iter().any(|(n, _)| n == name) {
                self.invalid.push(format!(
                    "DELETE of the label `{name}`, which the board lacks"
                ));
                let has: Vec<String> = self.labels.iter().map(|(n, _)| format!("`{n}`")).collect();
                return problem(
                    400,
                    &format!(
                        "The board has no label `{name}`; it has {}.",
                        and_list(&has)
                    ),
                );
            }
            self.card_mut(number).new.labels.retain(|l| l != name);
        }
        self.card_reply(200, number)
    }

    /// `POST` or `DELETE /api/board/cards/<number>/blocked_by/<other>`.
    fn block(&mut self, req: &Request, number: &str, other: &str) -> Reply {
        let number = match self.card_of(number) {
            Ok(number) => number,
            Err(refused) => return refused,
        };
        let fields = match self.fields(req, &["run"], &[], true) {
            Ok(fields) => fields,
            Err(refused) => return refused,
        };
        if let Err(refused) = self.run_of(&fields, req) {
            return refused;
        }
        let Ok(other) = self.card_of(other) else {
            return problem(400, &format!("The board has no card #{other}."));
        };
        if req.method == "DELETE" {
            self.card_mut(number).new.blocked_by.retain(|n| *n != other);
            return self.card_reply(200, number);
        }
        if other == number {
            return problem(409, &chain_words(number, &[]));
        }
        if let Some(chain) = self.chain(other, number) {
            return problem(409, &chain_words(number, &chain));
        }
        let blocked_by = &mut self.card_mut(number).new.blocked_by;
        if !blocked_by.contains(&other) {
            blocked_by.push(other);
        }
        if self.card(number).new.done && !self.card(other).new.done {
            self.card_mut(number).new.done = false;
            self.undo_blocked(number);
        }
        self.card_reply(200, number)
    }

    /// The place of an item in the checklist of the card `number`, as a path writes it.
    fn item_at(&self, number: i64, index: &str) -> Result<usize, Reply> {
        let count = self.card(number).new.checklist.len();
        let whole = !index.is_empty() && index.bytes().all(|b| b.is_ascii_digit());
        let Some(at) = whole.then(|| index.parse::<usize>().ok()).flatten() else {
            return Err(problem(
                404,
                &format!(
                    "Card #{number} has no item `{index}`; an item is named by its place in the \
                     checklist, from 0."
                ),
            ));
        };
        if at >= count {
            let has = if count == 0 {
                "its checklist has none".to_string()
            } else {
                format!("its checklist has {count}")
            };
            return Err(problem(
                404,
                &format!("Card #{number} has no item {index}; {has}."),
            ));
        }
        Ok(at)
    }

    /// `POST /api/board/cards/<number>/checklist`.
    fn add_item(&mut self, req: &Request, number: &str) -> Reply {
        let number = match self.card_of(number) {
            Ok(number) => number,
            Err(refused) => return refused,
        };
        let fields = match self.fields(req, &["text", "run"], &["text"], false) {
            Ok(fields) => fields,
            Err(refused) => return refused,
        };
        let text = match self.text(&fields, "text") {
            Ok(text) => text.unwrap_or_default().trim().to_string(),
            Err(refused) => return refused,
        };
        if let Err(refused) = self.run_of(&fields, req) {
            return refused;
        }
        if text.is_empty() {
            return problem(400, "An item needs some text.");
        }
        self.card_mut(number).new.checklist.push((text, false));
        self.card_reply(200, number)
    }

    /// `PATCH` or `DELETE /api/board/cards/<number>/checklist/<index>`.
    fn item(&mut self, req: &Request, number: &str, index: &str) -> Reply {
        let number = match self.card_of(number) {
            Ok(number) => number,
            Err(refused) => return refused,
        };
        let patch = req.method == "PATCH";
        let takes: &[&str] = if patch {
            &["text", "done", "run"]
        } else {
            &["run"]
        };
        let fields = match self.fields(req, takes, &[], !patch) {
            Ok(fields) => fields,
            Err(refused) => return refused,
        };
        let (text, done) = match (self.text(&fields, "text"), self.flag(&fields, "done")) {
            (Ok(text), Ok(done)) => (text.map(|t| t.trim().to_string()), done),
            (Err(refused), _) | (_, Err(refused)) => return refused,
        };
        if let Err(refused) = self.run_of(&fields, req) {
            return refused;
        }
        let at = match self.item_at(number, index) {
            Ok(at) => at,
            Err(refused) => return refused,
        };
        let checklist = &mut self.card_mut(number).new.checklist;
        if !patch {
            checklist.remove(at);
        } else {
            if let Some(text) = text {
                checklist[at].0 = text;
            }
            if let Some(done) = done {
                checklist[at].1 = done;
            }
        }
        self.card_reply(200, number)
    }

    /// `POST /api/board/cards/<number>/comments`.
    fn add_comment(&mut self, req: &Request, number: &str) -> Reply {
        let number = match self.card_of(number) {
            Ok(number) => number,
            Err(refused) => return refused,
        };
        let fields = match self.fields(req, &["body", "run"], &["body"], false) {
            Ok(fields) => fields,
            Err(refused) => return refused,
        };
        let body = match self.text(&fields, "body") {
            Ok(body) => body.unwrap_or_default(),
            Err(refused) => return refused,
        };
        let run = match self.run_of(&fields, req) {
            Ok(run) => run,
            Err(refused) => return refused,
        };
        if body.trim().is_empty() || body.chars().count() > 10_000 {
            return problem(400, "A comment is some text, at most 10 000 characters.");
        }
        let created_at = self.tick();
        let comment = Comment {
            id: mint(),
            card: number,
            body,
            by: Some(KEY_NAME.to_string()),
            by_key: true,
            run,
            created_at,
        };
        let answer = Store::comment_answer(&comment);
        self.comments.push(comment);
        (201, Vec::new(), answer)
    }

    /// `POST /api/board/cards/<number>/archive` or `…/unarchive`.
    fn archive(&mut self, req: &Request, number: &str, archive: bool) -> Reply {
        let number = match self.card_of(number) {
            Ok(number) => number,
            Err(refused) => return refused,
        };
        let fields = match self.fields(req, &["run"], &[], true) {
            Ok(fields) => fields,
            Err(refused) => return refused,
        };
        if let Err(refused) = self.run_of(&fields, req) {
            return refused;
        }
        let archived = self.card(number).archived_at.is_some();
        if archive && !archived {
            let at = self.tick();
            self.card_mut(number).archived_at = Some(at);
        } else if !archive && archived {
            self.card_mut(number).archived_at = None;
            let list = self.card(number).new.list.clone();
            self.put_card(number, &list, Spot::Bottom);
        }
        self.card_reply(200, number)
    }

    /// `DELETE /api/board/cards/<number>`: no body, and `204`.
    fn delete_card(&mut self, req: &Request, number: &str) -> Reply {
        let number = match self.card_of(number) {
            Ok(number) => number,
            Err(refused) => return refused,
        };
        if req.body_len > 0 {
            self.invalid
                .push(format!("DELETE {}: it takes no body", req.path));
        }
        self.cards.retain(|c| c.number != number);
        self.comments.retain(|m| m.card != number);
        for card in &mut self.cards {
            card.new.blocked_by.retain(|n| *n != number);
        }
        (204, Vec::new(), Value::Null)
    }
}

/// `items` as a sentence lists them, as the site's `and_list` writes it.
fn and_list(items: &[String]) -> String {
    match items {
        [] => "none".to_string(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// `secs` since the epoch as the API writes an instant, `2026-11-14T09:30:00Z`.
fn rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
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
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3_600,
        (rem % 3_600) / 60,
        rem % 60
    )
}

/// One reply: its status, its extra headers, and its JSON body.
type Reply = (u16, Vec<(String, String)>, Value);

/// A refusal, `{"error": …}`.
fn problem(status: u16, error: &str) -> Reply {
    (status, Vec::new(), json!({ "error": error }))
}

/// `text` with its `%XX` escapes and its `+`s decoded, as a query's value is.
fn decoded(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'+' => out.push(b' '),
            b'%' if at + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[at + 1..at + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(b) => {
                        out.push(b);
                        at += 2;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            b => out.push(b),
        }
        at += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Serve one connection: one request, one reply, then close.
fn serve(stream: TcpStream, store: &Mutex<Store>) {
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    let mut start = String::new();
    if reader.read_line(&mut start).unwrap_or(0) == 0 {
        return;
    }
    let mut words = start.split_whitespace();
    let (method, target) = (
        words.next().unwrap_or("").to_string(),
        words.next().unwrap_or("").to_string(),
    );
    let mut authorization = None;
    let mut content_type = None;
    let mut content_length = 0usize;
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
            if name.eq_ignore_ascii_case("authorization") {
                authorization = Some(value.trim().to_string());
            } else if name.eq_ignore_ascii_case("content-type") {
                content_type = Some(value.trim().to_string());
            } else if name.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut raw = vec![0u8; content_length];
    if reader.read_exact(&mut raw).is_err() {
        return;
    }
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
    let query: Vec<(String, String)> = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (decoded(k), decoded(v))
        })
        .collect();
    let request = Request {
        method,
        path: path.to_string(),
        query,
        authorization,
        content_type,
        body_len: raw.len(),
        body: serde_json::from_slice(&raw).ok(),
    };
    let (status, headers, body) = {
        let mut s = lock(store);
        s.seen.push(Seen {
            method: request.method.clone(),
            path: request.path.clone(),
            query: request.query.clone(),
            authorization: request.authorization.clone(),
            body: request.body.clone(),
        });
        answer(&mut s, &request)
    };
    let extra: String = headers
        .iter()
        .map(|(k, v)| format!("{k}: {v}\r\n"))
        .collect();
    // A `204` has no body, and no type or length to name it by.
    let tail = if status == 204 {
        "\r\n".to_string()
    } else {
        let body = body.to_string();
        format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
    };
    let mut stream = stream;
    let _ = write!(
        stream,
        "HTTP/1.1 {status} X\r\nCache-Control: no-store\r\nConnection: close\r\n{extra}{tail}",
    );
    let _ = stream.flush();
}

/// Answer one request.
fn answer(s: &mut Store, req: &Request) -> Reply {
    let (method, path, query) = (req.method.as_str(), req.path.as_str(), &req.query);
    let bearer = req
        .authorization
        .as_deref()
        .and_then(|a| a.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, key)| key.trim());
    if bearer != Some(s.key.as_str()) || s.key.is_empty() {
        let mut refused = problem(401, KEY_REFUSED);
        refused
            .1
            .push(("WWW-Authenticate".to_string(), "Bearer".to_string()));
        return refused;
    }
    let Some(rest) = path.strip_prefix("/api/board") else {
        s.invalid.push(format!("{method} {path}"));
        return problem(404, "No such page.");
    };
    if !query.is_empty() && !(method == "GET" && rest == "/cards") {
        s.invalid.push(format!("a query on {method} {path}"));
    }
    if req.body_len > 0
        && !req
            .content_type
            .as_deref()
            .is_some_and(|t| t.starts_with("application/json"))
    {
        s.invalid.push(format!(
            "{method} {path}: a body that is not application/json"
        ));
        return problem(
            415,
            "Expected request with `Content-Type: application/json`",
        );
    }
    let segments: Vec<String> = rest
        .split('/')
        .filter(|p| !p.is_empty())
        .map(decoded_path)
        .collect();
    let segments: Vec<&str> = segments.iter().map(String::as_str).collect();
    if method != "GET" {
        return write(s, req, &segments);
    }
    match segments.as_slice() {
        [] => {
            let lists: Vec<Value> = s
                .lists
                .iter()
                .map(|(id, name)| json!({"id": id, "name": name}))
                .collect();
            let labels: Vec<Value> = s
                .labels
                .iter()
                .map(|(name, colour)| json!({"name": name, "colour": colour}))
                .collect();
            (
                200,
                Vec::new(),
                json!({"id": s.board_id, "name": s.name, "lists": lists, "labels": labels}),
            )
        }
        ["cards"] => s.cards(query),
        ["cards", number] | ["cards", number, "comments"] => {
            let canonical = number.bytes().all(|b| b.is_ascii_digit()) && !number.starts_with('0');
            let found = canonical
                .then(|| number.parse::<i64>().ok())
                .flatten()
                .and_then(|n| s.cards.iter().find(|c| c.number == n));
            let Some(card) = found else {
                return problem(404, &format!("The board has no card #{number}."));
            };
            if segments.len() == 2 {
                return (200, Vec::new(), s.card_answer(card));
            }
            let mut comments: Vec<&Comment> = s
                .comments
                .iter()
                .filter(|m| m.card == card.number)
                .collect();
            comments.sort_by_key(|m| m.created_at);
            let answers: Vec<Value> = comments.iter().map(|m| Store::comment_answer(m)).collect();
            (200, Vec::new(), Value::Array(answers))
        }
        _ => {
            s.invalid.push(format!("GET {path}"));
            problem(404, "No such page.")
        }
    }
}

/// Answer one write, `req` split at the board's path into `segments`.
fn write(s: &mut Store, req: &Request, segments: &[&str]) -> Reply {
    match (req.method.as_str(), segments) {
        ("POST", ["lists"]) => s.add_list(req),
        ("PATCH", ["lists", id]) => s.change_list(req, id),
        ("DELETE", ["lists", id]) => s.delete_list(id),
        ("POST", ["cards"]) => s.add_card(req),
        ("PATCH", ["cards", number]) => s.change_card(req, number),
        ("DELETE", ["cards", number]) => s.delete_card(req, number),
        ("POST" | "DELETE", ["cards", number, "labels", name]) => s.label(req, number, name),
        ("POST" | "DELETE", ["cards", number, "blocked_by", other]) => s.block(req, number, other),
        ("POST", ["cards", number, "checklist"]) => s.add_item(req, number),
        ("PATCH" | "DELETE", ["cards", number, "checklist", index]) => s.item(req, number, index),
        ("POST", ["cards", number, "comments"]) => s.add_comment(req, number),
        ("POST", ["cards", number, "archive"]) => s.archive(req, number, true),
        ("POST", ["cards", number, "unarchive"]) => s.archive(req, number, false),
        (method, _) => {
            s.invalid.push(format!("{method} {}", req.path));
            problem(404, "No such page.")
        }
    }
}

/// A path segment with its `%XX` escapes decoded; a `+` in it is a `+`.
fn decoded_path(segment: &str) -> String {
    decoded(&segment.replace('+', "%2B"))
}
