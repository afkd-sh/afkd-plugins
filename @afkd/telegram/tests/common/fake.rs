//! A stateful fake Telegram Bot API on a loopback socket: the methods the plugin and its skill
//! call at `/bot<token>/<method>`, and the file downloads at `/file/bot<token>/<path>`, over
//! the chats, messages, reactions and files a test seeds and then reads back. std and
//! `serde_json` only, and nothing from the plugin crate, so another suite can
//! `#[path]`-include it.
//!
//! **The Bot API is recorded, not reached.** Every body the fake sends is one of
//! `tests/fixtures/`: `updates.json` is a `getUpdates` batch, `replies.json` each method's
//! answer, `errors.json` the refusals, each in the shape core.telegram.org/bots/api documents
//! and transcribed from it on 2026-11-09, with the request's own values filled in (the chat,
//! the text, a minted message id, the file sent). No test, and no build step, calls the live
//! API: recording from a live bot would post to a real chat.
//!
//! **Every request is held to the Bot API before it is answered.** The method must be one the
//! page documents, every parameter one it documents for that method, and the required ones
//! present, in a JSON, urlencoded or multipart body. One that is not is answered as Telegram
//! answers it, a `400 Bad Request`, and is recorded in [`FakeTelegram::invalid_requests`],
//! which every leg holds empty. So are the values Telegram refuses that the plugin must never
//! send: an empty text, a text over 4096 UTF-16 units, a caption over 1024, more than one
//! reaction, an upload over 50 MB. Where Telegram checks a value the plugin passes through,
//! the fake checks it too and answers as Telegram does, without recording it: a reaction
//! outside the 73 emoji of `ReactionTypeEmoji` (`reaction_emoji.json`) is
//! `REACTION_INVALID`, a chat it does not know `chat not found`, an edit of a message with no
//! text `there is no text in the message to edit`.
//!
//! The rest is what Telegram does that the plugin depends on: a wrong token is a 401; an
//! update is confirmed, and gone, once a `getUpdates` asks with an offset past it; message
//! ids count up per chat; a sent file gets a file id `getFile` resolves to a path its bytes
//! are downloaded from; a file over 20 MB is `file is too big`. Every request is recorded, and
//! a method can be rate limited with a `retry_after`, made to answer a recorded refusal, or the
//! whole fake taken out of reach.

#![allow(dead_code)]

#[path = "form.rs"]
pub mod form;

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};

/// The bot's token: what every request's path must carry.
pub const TOKEN: &str = "7712345678:AAHfiqksKZ8WmR2zSjiQ7_v4TMAKdiHm9T0";

/// The bot's own user id, which every message it sends is from.
pub const BOT_ID: i64 = 7_712_345_678;

/// The supergroup most legs talk in.
pub const GROUP: i64 = -1_001_234_567_890;

/// A private chat with Anna, a member of the group.
pub const PRIVATE: i64 = 111_111_111;

/// A group no service lists.
pub const ELSEWHERE: i64 = -1_009_876_543_210;

/// The most Telegram takes in one upload from a bot.
pub const UPLOAD_CAP: usize = 50 * 1024 * 1024;

/// The most Telegram hands a bot of a file it received.
pub const FETCH_CAP: u64 = 20 * 1024 * 1024;

const UPDATES: &str = include_str!("../fixtures/updates.json");
const REPLIES: &str = include_str!("../fixtures/replies.json");
const ERRORS: &str = include_str!("../fixtures/errors.json");
const REACTION_EMOJI: &str = include_str!("../fixtures/reaction_emoji.json");

/// Each method the fake answers: its required parameters and every other one the Bot API
/// page documents for it, transcribed on 2026-11-09. `chat_id` and `message_id` are required
/// here where the page makes them optional for the business, inline and ephemeral forms,
/// which nothing of this plugin's sends.
const METHODS: &[(&str, &[&str], &[&str])] = &[
    (
        "getUpdates",
        &[],
        &["offset", "limit", "timeout", "allowed_updates"],
    ),
    (
        "sendMessage",
        &["chat_id", "text"],
        &[
            "business_connection_id",
            "message_thread_id",
            "direct_messages_topic_id",
            "ephemeral_message_parameters",
            "parse_mode",
            "entities",
            "link_preview_options",
            "disable_notification",
            "protect_content",
            "allow_paid_broadcast",
            "message_effect_id",
            "suggested_post_parameters",
            "reply_parameters",
            "reply_markup",
        ],
    ),
    ("sendDocument", &["chat_id", "document"], SEND_FILE_COMMON),
    ("sendPhoto", &["chat_id", "photo"], SEND_FILE_COMMON),
    ("sendVideo", &["chat_id", "video"], SEND_FILE_COMMON),
    ("sendAudio", &["chat_id", "audio"], SEND_FILE_COMMON),
    ("sendVoice", &["chat_id", "voice"], SEND_FILE_COMMON),
    (
        "setMessageReaction",
        &["chat_id", "message_id"],
        &["reaction", "is_big"],
    ),
    (
        "editMessageText",
        &["chat_id", "message_id", "text"],
        &[
            "business_connection_id",
            "inline_message_id",
            "parse_mode",
            "entities",
            "link_preview_options",
            "rich_message",
            "reply_markup",
        ],
    ),
    (
        "editMessageCaption",
        &["chat_id", "message_id"],
        &[
            "business_connection_id",
            "inline_message_id",
            "caption",
            "parse_mode",
            "caption_entities",
            "show_caption_above_media",
            "reply_markup",
        ],
    ),
    ("deleteMessage", &["chat_id", "message_id"], &[]),
    ("getFile", &["file_id"], &[]),
];

/// The optional parameters of the five file sends, the union of what the page documents for
/// each (a video's `width`, an audio's `performer`).
const SEND_FILE_COMMON: &[&str] = &[
    "business_connection_id",
    "message_thread_id",
    "direct_messages_topic_id",
    "ephemeral_message_parameters",
    "thumbnail",
    "caption",
    "parse_mode",
    "caption_entities",
    "disable_content_type_detection",
    "show_caption_above_media",
    "has_spoiler",
    "duration",
    "width",
    "height",
    "cover",
    "start_timestamp",
    "supports_streaming",
    "performer",
    "title",
    "disable_notification",
    "protect_content",
    "allow_paid_broadcast",
    "message_effect_id",
    "suggested_post_parameters",
    "reply_parameters",
    "reply_markup",
];

/// The file field each send takes its bytes in.
fn file_field(method: &str) -> Option<&'static str> {
    match method {
        "sendDocument" => Some("document"),
        "sendPhoto" => Some("photo"),
        "sendVideo" => Some("video"),
        "sendAudio" => Some("audio"),
        "sendVoice" => Some("voice"),
        _ => None,
    }
}

/// The `message` of every recorded update, in order.
pub fn recorded_messages() -> Vec<Value> {
    let updates: Value = serde_json::from_str(UPDATES).expect("updates.json parses");
    updates["result"]
        .as_array()
        .expect("a result array")
        .iter()
        .map(|u| u["message"].clone())
        .collect()
}

/// The recorded message of `id` in `chat`.
pub fn recorded(chat: i64, id: i64) -> Value {
    recorded_messages()
        .into_iter()
        .find(|m| m["chat"]["id"] == chat && m["message_id"] == id)
        .unwrap_or_else(|| panic!("no recorded message {id} in chat {chat}"))
}

/// The 73 emoji a bot may react with.
pub fn reaction_emoji() -> Vec<String> {
    serde_json::from_str(REACTION_EMOJI).expect("reaction_emoji.json parses")
}

/// One file part of a request.
#[derive(Debug, Clone)]
pub struct SeenFile {
    pub field: String,
    pub filename: String,
    pub bytes: Vec<u8>,
}

/// One request as the fake saw it.
#[derive(Debug, Clone)]
pub struct Seen {
    /// The method, or `file` for a download.
    pub method: String,
    /// The parameters, by name: a JSON body's values as sent, a form's as strings.
    pub params: Map<String, Value>,
    /// The file parts of a multipart body.
    pub files: Vec<SeenFile>,
    /// Whether the path carried the bot's token.
    pub token_ok: bool,
    /// The body's `Content-Type`, up to its parameters.
    pub content_type: String,
    pub at: Instant,
}

/// A file Telegram holds: where it is downloaded from, and its bytes or, for one seeded too
/// big to fetch, only its size.
#[derive(Debug, Clone)]
struct StoredFile {
    unique: String,
    path: String,
    size: u64,
    bytes: Vec<u8>,
}

/// A rate limit a test armed on a method.
#[derive(Debug, Clone, Copy)]
struct RateLimit {
    skip: u32,
    n: u32,
    retry_after: u64,
}

#[derive(Default)]
struct Store {
    /// Every chat the bot is in, as its `Chat` object.
    chats: HashMap<i64, Value>,
    /// Every message the bot knows, by chat and id.
    messages: HashMap<(i64, i64), Value>,
    /// The pending updates, oldest first.
    pending: Vec<(i64, Value)>,
    next_update: i64,
    /// The highest offset a `getUpdates` has asked with.
    confirmed: i64,
    files: HashMap<String, StoredFile>,
    minted: u64,
    reactions: HashMap<(i64, i64), Vec<String>>,
    seen: Vec<Seen>,
    dropped: usize,
    invalid: Vec<String>,
    /// After `skip` more calls of a method go through, its next `n` answer 429, asking for
    /// `retry_after` seconds.
    rate_limits: HashMap<String, RateLimit>,
    /// A method answering a recorded refusal, by its `errors.json` name.
    faults: HashMap<String, String>,
    down: bool,
    replies: Value,
    errors: Value,
    emoji: Vec<String>,
}

fn lock(store: &Mutex<Store>) -> MutexGuard<'_, Store> {
    store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Now, in Unix seconds.
fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// How many UTF-16 code units `s` takes, the unit Telegram counts its caps in.
pub fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// A Telegram answer: its status, and its body.
type Answer = (u16, Value);

/// What a method comes to before it is answered: its result, or the answer refusing it.
type Outcome = Result<Value, Answer>;

impl Store {
    /// The recorded refusal of `name`, as Telegram answers it.
    fn refusal(&self, name: &str) -> Answer {
        let body = self.errors[name].clone();
        assert!(body.is_object(), "no refusal `{name}` in errors.json");
        let code = body["error_code"].as_u64().unwrap_or(400) as u16;
        (code, body)
    }

    /// The recorded refusal of `name`, for a request the plugin must never send: recorded as
    /// invalid, with `why`.
    fn invalid(&mut self, method: &str, name: &str, why: &str) -> Answer {
        self.invalid.push(format!("{method}: {why}"));
        self.refusal(name)
    }

    /// The 400 a request not shaped as the Bot API documents gets, recorded as invalid.
    fn malformed(&mut self, method: &str, why: &str) -> Answer {
        self.invalid.push(format!("{method}: {why}"));
        (
            400,
            json!({"ok": false, "error_code": 400, "description": format!("Bad Request: {why}")}),
        )
    }

    /// `replies.json`'s result for `method`, to fill in.
    fn template(&self, method: &str) -> Value {
        self.replies[method]["result"].clone()
    }

    fn register_chat(&mut self, chat: &Value) {
        if let Some(id) = chat["id"].as_i64() {
            self.chats.entry(id).or_insert_with(|| chat.clone());
        }
    }

    /// The next message id in `chat`, one past the highest it holds.
    fn next_message_id(&self, chat: i64) -> i64 {
        self.messages
            .keys()
            .filter(|(c, _)| *c == chat)
            .map(|(_, id)| *id)
            .max()
            .unwrap_or(0)
            + 1
    }

    /// A fresh file id, prefixed as Telegram prefixes a kind's.
    fn mint_file(&mut self, kind: &str) -> (String, String) {
        self.minted += 1;
        let prefix = match kind {
            "photo" => "AgACAgQAAxkDAAI",
            "video" => "BAACAgQAAxkDAAI",
            "audio" => "CQACAgQAAxkDAAI",
            "voice" => "AwACAgQAAxkDAAI",
            _ => "BQACAgQAAxkDAAI",
        };
        (
            format!("{prefix}{:06}Fake", self.minted),
            format!("AgAD{:06}", self.minted),
        )
    }

    /// The chat `chat_id` names: an id the bot is in, or the `@username` of one.
    fn chat(&self, value: &Value) -> Result<i64, Answer> {
        let found = match int(value) {
            Some(id) => self.chats.contains_key(&id).then_some(id),
            None => value.as_str().and_then(|name| {
                let name = name.strip_prefix('@')?;
                self.chats
                    .iter()
                    .find(|(_, c)| c["username"] == name)
                    .map(|(id, _)| *id)
            }),
        };
        found.ok_or_else(|| self.refusal("chat_not_found"))
    }
}

/// An integer parameter: a JSON number, or a form's digits.
fn int(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// A parameter that is a JSON object or array: as sent in a JSON body, or JSON-serialized in a
/// form, as the Bot API takes one.
fn structured(value: &Value) -> Value {
    match value {
        Value::String(s) => serde_json::from_str(s).unwrap_or(Value::Null),
        other => other.clone(),
    }
}

/// A string parameter.
fn text(value: &Value) -> Option<String> {
    value.as_str().map(str::to_string)
}

/// The fake.
pub struct FakeTelegram {
    base: String,
    store: Arc<Mutex<Store>>,
}

impl FakeTelegram {
    /// A fake that knows the recorded chats and holds no update, listening on `127.0.0.1:0`.
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let base = format!("http://{}", listener.local_addr().expect("local addr"));
        let store = Arc::new(Mutex::new(Store {
            next_update: 870_000_001,
            replies: serde_json::from_str(REPLIES).expect("replies.json parses"),
            errors: serde_json::from_str(ERRORS).expect("errors.json parses"),
            emoji: reaction_emoji(),
            ..Store::default()
        }));
        {
            let mut s = lock(&store);
            for m in recorded_messages() {
                s.register_chat(&m["chat"]);
            }
        }
        let shared = Arc::clone(&store);
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let store = Arc::clone(&shared);
                thread::spawn(move || serve(stream, &store));
            }
        });
        Self { base, store }
    }

    /// The API root, as a service's `base_url` writes it.
    pub fn base_url(&self) -> &str {
        &self.base
    }

    /// A message to the bot, pending as an update until a `getUpdates` confirms it. Its chat,
    /// and the message it replies to, become known. Returns the update's id.
    pub fn deliver(&self, message: Value) -> i64 {
        let mut s = lock(&self.store);
        s.register_chat(&message["chat"]);
        let chat = message["chat"]["id"].as_i64().expect("a chat id");
        if let Some(replied) = message.get("reply_to_message") {
            if let Some(id) = replied["message_id"].as_i64() {
                s.messages.insert((chat, id), replied.clone());
            }
        }
        let id = message["message_id"].as_i64().expect("a message id");
        s.messages.insert((chat, id), message.clone());
        let update = s.next_update;
        s.next_update += 1;
        s.pending.push((update, message));
        update
    }

    /// Hold `bytes` as the file of `file_id`, downloadable once `getFile` names its path.
    pub fn seed_file(&self, file_id: &str, bytes: &[u8]) {
        let mut s = lock(&self.store);
        s.minted += 1;
        let n = s.minted;
        s.files.insert(
            file_id.to_string(),
            StoredFile {
                unique: format!("AgAD{n:06}"),
                path: format!("documents/file_{n}.bin"),
                size: bytes.len() as u64,
                bytes: bytes.to_vec(),
            },
        );
    }

    /// Hold a file of `file_id` over the 20 MB a bot may fetch.
    pub fn seed_big_file(&self, file_id: &str) {
        let mut s = lock(&self.store);
        s.files.insert(
            file_id.to_string(),
            StoredFile {
                unique: "AgADbig".into(),
                path: "videos/file_big.mp4".into(),
                size: FETCH_CAP + 1,
                bytes: Vec::new(),
            },
        );
    }

    /// Every request, in order.
    pub fn seen(&self) -> Vec<Seen> {
        lock(&self.store).seen.clone()
    }

    /// The requests of `method`, in order.
    pub fn calls(&self, method: &str) -> Vec<Seen> {
        self.seen()
            .into_iter()
            .filter(|s| s.method == method)
            .collect()
    }

    /// The methods asked for, in order.
    pub fn methods(&self) -> Vec<String> {
        self.seen().into_iter().map(|s| s.method).collect()
    }

    /// The highest offset a `getUpdates` has asked with: every update below it is confirmed.
    pub fn confirmed(&self) -> i64 {
        lock(&self.store).confirmed
    }

    /// The ids of the updates not yet confirmed.
    pub fn pending(&self) -> Vec<i64> {
        lock(&self.store)
            .pending
            .iter()
            .map(|(id, _)| *id)
            .collect()
    }

    /// The message of `id` in `chat`, as the fake holds it now.
    pub fn message(&self, chat: i64, id: i64) -> Option<Value> {
        lock(&self.store).messages.get(&(chat, id)).cloned()
    }

    /// The bot's reaction on the message of `id` in `chat`.
    pub fn reaction(&self, chat: i64, id: i64) -> Vec<String> {
        lock(&self.store)
            .reactions
            .get(&(chat, id))
            .cloned()
            .unwrap_or_default()
    }

    /// The bytes of the file of `file_id`.
    pub fn file(&self, file_id: &str) -> Option<Vec<u8>> {
        lock(&self.store)
            .files
            .get(file_id)
            .map(|f| f.bytes.clone())
    }

    /// Answer the next `n` calls of `method` with a 429 asking for `retry_after` seconds.
    pub fn rate_limit(&self, method: &str, n: u32, retry_after: u64) {
        self.rate_limit_after(method, 0, n, retry_after);
    }

    /// Let the next `skip` calls of `method` through, then answer `n` with a 429 asking for
    /// `retry_after` seconds.
    pub fn rate_limit_after(&self, method: &str, skip: u32, n: u32, retry_after: u64) {
        lock(&self.store).rate_limits.insert(
            method.to_string(),
            RateLimit {
                skip,
                n,
                retry_after,
            },
        );
    }

    /// Answer every call of `method` with the refusal `errors.json` names `refusal`.
    pub fn fail(&self, method: &str, refusal: &str) {
        lock(&self.store)
            .faults
            .insert(method.to_string(), refusal.to_string());
    }

    /// Stop failing and rate limiting `method`.
    pub fn heal(&self, method: &str) {
        let mut s = lock(&self.store);
        s.faults.remove(method);
        s.rate_limits.remove(method);
    }

    /// Take the fake out of reach: every request is read and its connection closed with no
    /// reply, until [`restore`](Self::restore).
    pub fn outage(&self) {
        lock(&self.store).down = true;
    }

    pub fn restore(&self) {
        lock(&self.store).down = false;
    }

    /// How many requests an outage dropped.
    pub fn dropped(&self) -> usize {
        lock(&self.store).dropped
    }

    /// Every request refused as not shaped as the Bot API documents, or carrying a value the
    /// plugin must never send, each as the sentence it was refused with.
    pub fn invalid_requests(&self) -> Vec<String> {
        lock(&self.store).invalid.clone()
    }
}

/// One request as read off the socket.
struct Raw {
    method: String,
    path: String,
    content_type: String,
    body: Vec<u8>,
}

fn read_request(stream: &TcpStream) -> Option<Raw> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut start = String::new();
    if reader.read_line(&mut start).ok()? == 0 {
        return None;
    }
    let method = start.split_whitespace().next()?.to_string();
    let path = start.split_whitespace().nth(1)?.to_string();
    let (mut length, mut content_type) = (0usize, String::new());
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap_or(0);
            } else if name.eq_ignore_ascii_case("content-type") {
                content_type = value.trim().to_string();
            }
        }
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).ok()?;
    Some(Raw {
        method,
        path,
        content_type,
        body,
    })
}

/// Serve one connection: one request, one reply, then close.
fn serve(stream: TcpStream, store: &Mutex<Store>) {
    let Some(raw) = read_request(&stream) else {
        return;
    };
    let reply = {
        let mut s = lock(store);
        answer(&mut s, &raw)
    };
    let Some((status, content_type, body)) = reply else {
        // Out of reach: the stream drops here, closed with no reply.
        return;
    };
    let mut stream = stream;
    let _ = write!(
        stream,
        "HTTP/1.1 {status} X\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(&body);
    let _ = stream.flush();
}

/// `%XX` and `+` decoded, as a form or a query writes them.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 2;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The pairs of a form or a query string, decoded.
fn pairs(s: &str) -> Vec<(String, String)> {
    s.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect()
}

/// A request's parameters and file parts, from its query and its body, or what is wrong
/// with the body.
fn parameters(raw: &Raw, query: &str) -> Result<(Map<String, Value>, Vec<SeenFile>), String> {
    let mut params = Map::new();
    for (k, v) in pairs(query) {
        params.insert(k, Value::String(v));
    }
    let mut files = Vec::new();
    let kind = raw
        .content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match kind.as_str() {
        _ if raw.body.is_empty() => {}
        "application/json" => match serde_json::from_slice(&raw.body) {
            Ok(Value::Object(body)) => params.extend(body),
            _ => return Err("a JSON body that is not an object".into()),
        },
        "application/x-www-form-urlencoded" => {
            for (k, v) in pairs(&String::from_utf8_lossy(&raw.body)) {
                params.insert(k, Value::String(v));
            }
        }
        "multipart/form-data" => {
            for part in form::parse(&raw.content_type, &raw.body)? {
                match part.filename {
                    Some(filename) => files.push(SeenFile {
                        field: part.name,
                        filename,
                        bytes: part.bytes,
                    }),
                    None => {
                        let value = String::from_utf8(part.bytes)
                            .map_err(|_| format!("form field {} is not UTF-8", part.name))?;
                        params.insert(part.name, Value::String(value));
                    }
                }
            }
        }
        other => return Err(format!("a body of type `{other}`")),
    }
    Ok((params, files))
}

/// Answer one request — its status, content type and body — or `None` to drop it unanswered.
fn answer(s: &mut Store, raw: &Raw) -> Option<(u16, String, Vec<u8>)> {
    let (path, query) = raw.path.split_once('?').unwrap_or((&raw.path, ""));
    let (method, token, file_path) = if let Some(rest) = path.strip_prefix("/file/bot") {
        let (token, file) = rest.split_once('/').unwrap_or((rest, ""));
        (
            "file".to_string(),
            token.to_string(),
            Some(file.to_string()),
        )
    } else if let Some(rest) = path.strip_prefix("/bot") {
        let (token, method) = rest.split_once('/').unwrap_or((rest, ""));
        (method.to_string(), token.to_string(), None)
    } else {
        (String::new(), String::new(), None)
    };
    let parsed = parameters(raw, query);
    let (params, files) = parsed.clone().unwrap_or_default();
    let seen = Seen {
        method: method.clone(),
        params: params.clone(),
        files: files.clone(),
        token_ok: token == TOKEN,
        content_type: raw.content_type.split(';').next().unwrap_or("").to_string(),
        at: Instant::now(),
    };
    if s.down {
        s.dropped += 1;
        return None;
    }
    s.seen.push(seen);
    let json = |(status, body): Answer| {
        Some((
            status,
            "application/json".to_string(),
            body.to_string().into_bytes(),
        ))
    };
    if token != TOKEN {
        return json(s.refusal("unauthorized"));
    }
    if let Some(file_path) = file_path {
        return match s.files.values().find(|f| f.path == file_path) {
            Some(f) => Some((200, "application/octet-stream".into(), f.bytes.clone())),
            None => json(s.refusal("not_found")),
        };
    }
    let Some((_, required, optional)) = METHODS.iter().find(|(m, _, _)| *m == method) else {
        s.invalid
            .push(format!("{method}: not a Bot API method this fake knows"));
        return json(s.refusal("not_found"));
    };
    if let Err(e) = parsed {
        return json(s.malformed(&method, &e));
    }
    for name in params.keys().chain(files.iter().map(|f| &f.field)) {
        if !required.contains(&name.as_str()) && !optional.contains(&name.as_str()) {
            return json(s.malformed(&method, &format!("parameter `{name}` is not documented")));
        }
    }
    for name in *required {
        if !params.contains_key(*name) && !files.iter().any(|f| f.field == *name) {
            return json(s.malformed(&method, &format!("parameter `{name}` is required")));
        }
    }
    if let Some(limit) = s.rate_limits.get_mut(&method) {
        if limit.skip > 0 {
            limit.skip -= 1;
        } else if limit.n > 0 {
            limit.n -= 1;
            let wait = limit.retry_after;
            return json((
                429,
                json!({"ok": false, "error_code": 429,
                       "description": format!("Too Many Requests: retry after {wait}"),
                       "parameters": {"retry_after": wait}}),
            ));
        }
    }
    if let Some(refusal) = s.faults.get(&method).cloned() {
        return json(s.refusal(&refusal));
    }
    json(match dispatch(s, &method, &params, &files) {
        Ok(result) => (200, json!({"ok": true, "result": result})),
        Err(refused) => refused,
    })
}

/// Do `method`, its parameters already held to the page.
fn dispatch(s: &mut Store, method: &str, p: &Map<String, Value>, files: &[SeenFile]) -> Outcome {
    match method {
        "getUpdates" => get_updates(s, p),
        "sendMessage" => send_message(s, p),
        "setMessageReaction" => set_reaction(s, p),
        "editMessageText" => edit_text(s, p),
        "editMessageCaption" => edit_caption(s, p),
        "deleteMessage" => {
            let (chat, id) = target(s, method, p)?;
            if s.messages.remove(&(chat, id)).is_none() {
                return Err(s.refusal("delete_not_found"));
            }
            Ok(json!(true))
        }
        "getFile" => {
            let id = text(&p["file_id"]).unwrap_or_default();
            let Some(f) = s.files.get(&id) else {
                return Err(s.refusal("invalid_file_id"));
            };
            if f.size > FETCH_CAP {
                return Err(s.refusal("file_too_big"));
            }
            let mut result = s.template("getFile");
            result["file_id"] = json!(id);
            result["file_unique_id"] = json!(f.unique);
            result["file_size"] = json!(f.size);
            result["file_path"] = json!(f.path);
            Ok(result)
        }
        _ => send_file(s, method, p, files),
    }
}

/// Confirm every update below `offset`, and answer up to `limit` of the rest.
fn get_updates(s: &mut Store, p: &Map<String, Value>) -> Outcome {
    if let Some(offset) = p.get("offset").and_then(int) {
        s.confirmed = s.confirmed.max(offset);
        s.pending.retain(|(id, _)| *id >= offset);
    }
    let limit = p.get("limit").and_then(int).unwrap_or(100).clamp(1, 100) as usize;
    Ok(s.pending
        .iter()
        .take(limit)
        .map(|(id, m)| json!({"update_id": id, "message": m}))
        .collect())
}

/// The chat and the message id a request names, the chat one the bot is in.
fn target(s: &mut Store, method: &str, p: &Map<String, Value>) -> Result<(i64, i64), Answer> {
    let chat = s.chat(&p["chat_id"])?;
    match p.get("message_id").and_then(int) {
        Some(id) => Ok((chat, id)),
        None => Err(s.malformed(method, "message_id is not an integer")),
    }
}

/// A message the bot sends to `chat`, from `method`'s recorded answer, with a fresh id.
fn bot_message(s: &Store, method: &str, chat: i64) -> Value {
    let mut m = s.template(method);
    m["message_id"] = json!(s.next_message_id(chat));
    m["chat"] = s.chats[&chat].clone();
    m["date"] = json!(now_secs());
    if let Some(m) = m.as_object_mut() {
        m.remove("reply_to_message");
        m.remove("caption");
    }
    m
}

/// The message a send replies to, by its `reply_parameters`, without its own reply, as
/// Telegram nests one level.
fn replied(
    s: &mut Store,
    method: &str,
    chat: i64,
    p: &Map<String, Value>,
) -> Result<Option<Value>, Answer> {
    let Some(params) = p.get("reply_parameters") else {
        return Ok(None);
    };
    let Some(id) = structured(params).get("message_id").and_then(int) else {
        return Err(s.malformed(method, "reply_parameters carries no message_id"));
    };
    let Some(mut m) = s.messages.get(&(chat, id)).cloned() else {
        return Err(s.refusal("reply_not_found"));
    };
    if let Some(m) = m.as_object_mut() {
        m.remove("reply_to_message");
    }
    Ok(Some(m))
}

fn send_message(s: &mut Store, p: &Map<String, Value>) -> Outcome {
    let chat = s.chat(&p["chat_id"])?;
    let body = text(&p["text"]).unwrap_or_default();
    if body.trim().is_empty() {
        return Err(s.invalid("sendMessage", "text_empty", "an empty text"));
    }
    if utf16_len(&body) > 4096 {
        let why = format!("a text of {} UTF-16 units", utf16_len(&body));
        return Err(s.invalid("sendMessage", "text_too_long", &why));
    }
    let reply = replied(s, "sendMessage", chat, p)?;
    let mut m = bot_message(s, "sendMessage", chat);
    m["text"] = json!(body);
    if let Some(reply) = reply {
        m["reply_to_message"] = reply;
    }
    let id = m["message_id"].as_i64().unwrap_or_default();
    s.messages.insert((chat, id), m.clone());
    Ok(m)
}

fn send_file(s: &mut Store, method: &str, p: &Map<String, Value>, files: &[SeenFile]) -> Outcome {
    let field = file_field(method).unwrap_or("document");
    let chat = s.chat(&p["chat_id"])?;
    let caption = p.get("caption").and_then(text).unwrap_or_default();
    if utf16_len(&caption) > 1024 {
        let why = format!("a caption of {} UTF-16 units", utf16_len(&caption));
        return Err(s.invalid(method, "caption_too_long", &why));
    }
    let Some(upload) = files.iter().find(|f| f.field == field) else {
        return Err(s.malformed(method, &format!("`{field}` is not an uploaded file")));
    };
    if upload.bytes.len() > UPLOAD_CAP {
        let why = format!("an upload of {} bytes", upload.bytes.len());
        return Err(s.invalid(method, "too_large", &why));
    }
    let reply = replied(s, method, chat, p)?;
    let (file_id, unique) = s.mint_file(field);
    let size = upload.bytes.len() as u64;
    let path = format!("{field}s/file_{}", s.minted);
    s.files.insert(
        file_id.clone(),
        StoredFile {
            unique: unique.clone(),
            path,
            size,
            bytes: upload.bytes.clone(),
        },
    );
    let mut m = bot_message(s, method, chat);
    if field == "photo" {
        let mut sizes = m["photo"].as_array().cloned().unwrap_or_default();
        if let Some(thumb) = sizes.first_mut() {
            thumb["file_id"] = json!(format!("{file_id}_s"));
        }
        if let Some(full) = sizes.last_mut() {
            full["file_id"] = json!(file_id);
            full["file_unique_id"] = json!(unique);
            full["file_size"] = json!(size);
        }
        m["photo"] = Value::Array(sizes);
    } else {
        let f = &mut m[field];
        f["file_id"] = json!(file_id);
        f["file_unique_id"] = json!(unique);
        f["file_size"] = json!(size);
        if field != "voice" {
            f["file_name"] = json!(upload.filename);
        }
    }
    if !caption.is_empty() {
        m["caption"] = json!(caption);
    }
    if let Some(reply) = reply {
        m["reply_to_message"] = reply;
    }
    let id = m["message_id"].as_i64().unwrap_or_default();
    s.messages.insert((chat, id), m.clone());
    Ok(m)
}

fn set_reaction(s: &mut Store, p: &Map<String, Value>) -> Outcome {
    let (chat, id) = target(s, "setMessageReaction", p)?;
    if !s.messages.contains_key(&(chat, id)) {
        return Err(s.refusal("react_not_found"));
    }
    let reaction = p.get("reaction").map(structured).unwrap_or(json!([]));
    let Some(items) = reaction.as_array() else {
        return Err(s.malformed("setMessageReaction", "reaction is not an array"));
    };
    if items.len() > 1 {
        return Err(s.invalid(
            "setMessageReaction",
            "reactions_too_many",
            "more than one reaction",
        ));
    }
    let mut emoji = Vec::new();
    for item in items {
        if item["type"] != "emoji" {
            return Err(s.malformed("setMessageReaction", "a reaction whose type is not emoji"));
        }
        let e = item["emoji"].as_str().unwrap_or_default().to_string();
        if !s.emoji.contains(&e) {
            return Err(s.refusal("reaction_invalid"));
        }
        emoji.push(e);
    }
    s.reactions.insert((chat, id), emoji);
    Ok(json!(true))
}

/// The bot's own message of `id` in `chat`, or the refusal an edit of it gets.
fn editable(s: &Store, chat: i64, id: i64) -> Result<Value, Answer> {
    let Some(m) = s.messages.get(&(chat, id)) else {
        return Err(s.refusal("edit_not_found"));
    };
    if m["from"]["id"] != BOT_ID {
        return Err(s.refusal("cant_be_edited"));
    }
    Ok(m.clone())
}

fn edit_text(s: &mut Store, p: &Map<String, Value>) -> Outcome {
    let (chat, id) = target(s, "editMessageText", p)?;
    let mut m = editable(s, chat, id)?;
    let body = text(&p["text"]).unwrap_or_default();
    if body.trim().is_empty() {
        return Err(s.invalid("editMessageText", "text_empty", "an empty text"));
    }
    if utf16_len(&body) > 4096 {
        let why = format!("a text of {} UTF-16 units", utf16_len(&body));
        return Err(s.invalid("editMessageText", "text_too_long", &why));
    }
    if m.get("text").is_none() {
        return Err(s.refusal("no_text_to_edit"));
    }
    if m["text"] == body {
        return Err(s.refusal("not_modified"));
    }
    m["text"] = json!(body);
    m["edit_date"] = json!(now_secs());
    s.messages.insert((chat, id), m.clone());
    Ok(m)
}

fn edit_caption(s: &mut Store, p: &Map<String, Value>) -> Outcome {
    let (chat, id) = target(s, "editMessageCaption", p)?;
    let mut m = editable(s, chat, id)?;
    let caption = p.get("caption").and_then(text).unwrap_or_default();
    if utf16_len(&caption) > 1024 {
        let why = format!("a caption of {} UTF-16 units", utf16_len(&caption));
        return Err(s.invalid("editMessageCaption", "caption_too_long", &why));
    }
    if m.get("text").is_some() {
        return Err(s.refusal("no_caption_to_edit"));
    }
    if m.get("caption").and_then(Value::as_str).unwrap_or("") == caption {
        return Err(s.refusal("not_modified"));
    }
    match m.as_object_mut() {
        Some(o) if caption.is_empty() => {
            o.remove("caption");
        }
        _ => m["caption"] = json!(caption),
    }
    m["edit_date"] = json!(now_secs());
    s.messages.insert((chat, id), m.clone());
    Ok(m)
}

/// A valid PNG of one black pixel that also holds every byte value: a private ancillary
/// chunk, `afKd`, carries 0 to 255 twice and a multipart-looking `--` line, so the bytes hold
/// NUL, CR LF, `0xFF` and invalid UTF-8. Each chunk carries its real CRC, so a decoder takes
/// it.
pub fn png() -> Vec<u8> {
    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let start = out.len();
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        let crc = crc32(&out[start..]);
        out.extend_from_slice(&crc.to_be_bytes());
    }
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    chunk(&mut png, b"IHDR", &[0, 0, 0, 1, 0, 0, 0, 1, 8, 0, 0, 0, 0]);
    let mut every: Vec<u8> = (0..=255u8).chain(0..=255u8).collect();
    every.extend_from_slice(b"\r\n--afkd-telegram-boundary--\r\n");
    chunk(&mut png, b"afKd", &every);
    chunk(
        &mut png,
        b"IDAT",
        &[
            0x78, 0x01, 0x01, 0x02, 0x00, 0xfd, 0xff, 0x00, 0x00, 0x00, 0x02, 0x00, 0x01,
        ],
    );
    chunk(&mut png, b"IEND", &[]);
    png
}

/// The CRC-32 a PNG chunk carries (ISO 3309, reflected, `0xEDB88320`).
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}
