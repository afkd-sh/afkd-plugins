//! A stand-in for one board on app.afkd.sh: a loopback HTTP/1.1 server on a thread of its own,
//! std only, answering the four reads of the board's API as the site's README ("A board's
//! API") and its `server/src/db/board_api.rs` define them — the board, its cards filtered and
//! in the board's order, one card by its number, and a card's comments.
//!
//! It holds one board: its uuid, its name, its one key, its lists in order and its labels, and
//! the cards and comments a test seeds. A request without the key is `401`, as the API answers
//! one. Each request is recorded ([`FakeBoard::seen`]), and each the plugin should never have
//! sent — another method or path, a query key that is no filter, a list the board lacks, a
//! `limit` outside 1 to 250, a filter sent twice — is recorded too
//! ([`FakeBoard::invalid_requests`]), and answered as the API would.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;

use serde_json::{json, Value};

/// The moment the first seeded card is made, a whole second: 2026-11-14T09:30:00Z.
pub const T0: u64 = 1_794_648_600;

/// What the API answers a request without the board's key.
pub const KEY_REFUSED: &str = "the board key is missing, unknown or revoked";

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
#[derive(Debug, Clone)]
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

    /// The board's address, as a `board.Board`'s `base_url` writes it.
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
        let number = s.cards.len() as i64 + 1;
        let place = s.cards.iter().filter(|c| c.new.list == card.list).count() as i64;
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

    /// Every request the fake has seen, in order.
    pub fn seen(&self) -> Vec<Seen> {
        lock(&self.store).seen.clone()
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
            }
        }
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
    let (status, headers, body) = {
        let mut s = lock(store);
        s.seen.push(Seen {
            method: method.clone(),
            path: path.to_string(),
            query: query.clone(),
            authorization: authorization.clone(),
        });
        answer(&mut s, &method, path, &query, authorization.as_deref())
    };
    let extra: String = headers
        .iter()
        .map(|(k, v)| format!("{k}: {v}\r\n"))
        .collect();
    let body = body.to_string();
    let mut stream = stream;
    let _ = write!(
        stream,
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nCache-Control: no-store\r\n\
         Content-Length: {}\r\nConnection: close\r\n{extra}\r\n{body}",
        body.len()
    );
    let _ = stream.flush();
}

/// Answer one request.
fn answer(
    s: &mut Store,
    method: &str,
    path: &str,
    query: &[(String, String)],
    authorization: Option<&str>,
) -> Reply {
    let bearer = authorization
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
    if method != "GET" {
        s.invalid.push(format!("{method} {path}"));
        return problem(405, "Only GET is served here.");
    }
    let Some(rest) = path.strip_prefix("/api/board") else {
        s.invalid.push(format!("GET {path}"));
        return problem(404, "No such page.");
    };
    if !query.is_empty() && rest != "/cards" {
        s.invalid.push(format!("a query on GET {path}"));
    }
    let segments: Vec<&str> = rest.split('/').filter(|p| !p.is_empty()).collect();
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
