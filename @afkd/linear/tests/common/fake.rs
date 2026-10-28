//! A stateful fake Linear on a loopback socket: the GraphQL endpoint and the token endpoint
//! the plugin's client calls, over a team's states, users, labels, issues, relations and
//! comments the test seeds and then reads back. std and `serde_json` only, and nothing from
//! the plugin crate, so another suite can `#[path]`-include it.
//!
//! **Every request is held to Linear's schema before it is answered.** A `POST /graphql` is
//! checked by `sdl.rs` against `tests/schema.graphql` — its document, and its variables
//! against the variables the document declares — and its document must be, whitespace
//! aside, the one `src/operations.graphql` holds under its `operationName`. One that fails
//! is answered as Linear answers an invalid request, a 400 with a `graphql error`, and is
//! recorded in [`FakeLinear::invalid_requests`], which every leg holds empty. Past that,
//! only the filter paths the plugin means are answered: an `Issues` filter on anything but
//! the team's id, a state's id or the name of *some* label is refused the same way, even
//! where the schema allows it.
//!
//! The rest is what Linear does that the plugin depends on: an API key is sent bare and an
//! app's token as `Bearer`, from the `client_credentials` grant with the plugin's scope, and
//! lasts as long as the fake says; anything else is a 401 `authentication error`. Ids are
//! uuids minted in no order, and times come off the real clock to the millisecond (a claim's
//! liveness is judged against the wall clock). A state's issues come back in their manual
//! order, comments newest first, an issue's inverse relations 20 to the page `Issues` reads
//! and 50 to the page after, an archived issue in no read, an edit stamps `editedAt`, a
//! missing id is Linear's `Entity not found`. An issue filed with no state starts in the
//! team's first backlog state, else its first unstarted one. Every request is recorded, and
//! an operation can be rate limited with a `Retry-After`, made to answer a status, or the
//! whole fake taken out of reach.
//!
//! **Files go up and come back as Linear's upload guide has them.** `FileUpload` mints a
//! signed upload url on the fake itself (`/signed/…`), an asset url under `/uploads/…`, and
//! one header the PUT must carry. The PUT must carry that header, the `Content-Type` and the
//! `Cache-Control` the guide names, exactly `size` bytes and **no** `Authorization`: one that
//! does not is answered 403 and recorded in [`FakeLinear::invalid_requests`], as is a PUT to
//! a url the fake never signed. A `GET` of an asset url takes the credential GraphQL takes
//! (else a 401), and serves the bytes put there, or a 404 before they are. A test can seed a
//! file a person pasted ([`FakeLinear::seed_file`]) and fail the next PUTs with a status
//! ([`FakeLinear::fail_uploads`]). [`png`] is the one file fixture, a valid PNG holding every
//! byte value.

#![allow(dead_code)]

#[path = "sdl.rs"]
mod sdl;

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use sdl::Sdl;

/// The API key every request may carry as it is.
pub const API_KEY: &str = "lin_api_7f3aK9c1eQ2w";

/// The OAuth app's credentials, which the token endpoint trades for a token.
pub const CLIENT_ID: &str = "app-id-7f3a9c1e";
pub const CLIENT_SECRET: &str = "app-secret-4b8d&=/2e";

/// The scope an app's token must be asked for.
pub const SCOPE: &str = "read,write,app:assignable,app:mentionable";

/// The team every issue is in, by key.
pub const TEAM: &str = "ENG";

/// The schema every request is held to.
const SCHEMA: &str = include_str!("../schema.graphql");

/// The operations the plugin sends, as it writes them.
const OPERATIONS: &str = include_str!("../../src/operations.graphql");

/// Who wrote a comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Author {
    /// A user, by id.
    User(String),
    /// A bot actor: an app's comments, when the fake says so ([`FakeLinear::comments_as_bot`]).
    Bot(String),
    /// An external user, by id.
    External(String),
}

impl Author {
    /// The id the plugin reads the author by.
    pub fn id(&self) -> &str {
        match self {
            Author::User(id) | Author::Bot(id) | Author::External(id) => id,
        }
    }
}

/// A comment on an issue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    pub id: String,
    pub issue: String,
    pub author: Author,
    pub body: String,
    /// When it was posted, in epoch milliseconds.
    pub created: u64,
    /// When it was last edited, once edited.
    pub edited: Option<u64>,
}

/// One request as the fake saw it.
#[derive(Debug, Clone)]
pub struct Seen {
    pub method: String,
    pub path: String,
    /// The operation's name, `""` for the token endpoint.
    pub operation: String,
    pub variables: Value,
    pub authorization: Option<String>,
    /// The form body, for the token endpoint.
    pub form: String,
    pub at: Instant,
}

#[derive(Debug, Clone)]
struct User {
    id: String,
    name: String,
    display_name: String,
    email: String,
}

#[derive(Debug, Clone)]
struct State {
    id: String,
    name: String,
    kind: String,
}

#[derive(Debug, Clone)]
struct Label {
    id: String,
    name: String,
    /// `None` for a workspace label.
    team: Option<String>,
}

#[derive(Debug, Clone)]
struct Issue {
    id: String,
    number: u32,
    title: String,
    description: Option<String>,
    state: String,
    labels: Vec<String>,
    assignee: Option<String>,
    delegate: Option<String>,
    parent: Option<String>,
    sort_order: f64,
    /// Archived: in no read, as Linear leaves archived issues out unless asked.
    archived: bool,
}

/// A relation between two issues: `kind` relates `source` to `target`, so a `blocks` one
/// makes `source` a blocker of `target`.
#[derive(Debug, Clone)]
struct Relation {
    id: String,
    source: String,
    target: String,
    kind: String,
}

#[derive(Debug, Clone)]
struct Token {
    value: String,
    expires: Instant,
    revoked: bool,
}

/// One file `FileUpload` signed for, and its bytes once they were PUT.
#[derive(Debug, Clone)]
struct Upload {
    filename: String,
    content_type: String,
    size: usize,
    /// The path the PUT goes to, its signature included.
    upload_path: String,
    /// The path the asset is fetched from.
    asset_path: String,
    /// The header the PUT must carry, and its value.
    header: (String, String),
    bytes: Option<Vec<u8>>,
}

/// One file the fake signed an upload for, as a test reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeenUpload {
    pub filename: String,
    pub content_type: String,
    pub size: usize,
    pub asset_url: String,
    /// Whether its bytes were PUT.
    pub landed: bool,
}

/// One PUT to the storage, as the fake saw it: its path, its headers as sent, its length, and
/// the status it was answered.
#[derive(Debug, Clone)]
pub struct SeenPut {
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub len: usize,
    pub status: u16,
}

impl SeenPut {
    /// The value of the header `name`, in any case.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// The `Cache-Control` Linear's upload guide has the PUT carry.
pub const UPLOAD_CACHE_CONTROL: &str = "public, max-age=31536000";

/// The header the fake signs every upload with.
pub const UPLOAD_HEADER: &str = "x-goog-meta-afkd";

#[derive(Default)]
struct Store {
    counter: u64,
    /// The fake's own root, which its signed and asset urls are under.
    base: String,
    org_id: String,
    team_id: String,
    key_user: String,
    app_user: String,
    users: Vec<User>,
    states: Vec<State>,
    labels: Vec<Label>,
    issues: Vec<Issue>,
    relations: Vec<Relation>,
    comments: Vec<Comment>,
    tokens: Vec<Token>,
    token_lifetime: u64,
    token_fetches: usize,
    /// Whether the app's comments are authored by its bot actor rather than its user.
    bot_comments: bool,
    bot_id: String,
    /// An operation → how many more requests to rate limit, and the wait to ask for.
    rate_limits: HashMap<String, (u32, u64)>,
    /// An operation → the status it answers instead.
    faults: HashMap<String, u16>,
    /// Operations answered as Linear answers a document it refuses.
    refused: Vec<String>,
    /// A comment post whose body contains this answers the status instead.
    post_faults: Vec<(String, u16)>,
    /// Issues on which the next claim post is raced by a rival.
    races: Vec<String>,
    /// The page size of an issue's comments.
    comments_page: usize,
    seen: Vec<Seen>,
    invalid: Vec<String>,
    down: bool,
    dropped: Vec<Seen>,
    uploads: Vec<Upload>,
    puts: Vec<SeenPut>,
    /// How many more PUTs to answer with a status instead, and the status.
    upload_faults: (u32, u16),
}

/// The wall clock now, in epoch milliseconds.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// RFC 3339, UTC, to the millisecond, as Linear writes a time.
pub fn stamp(ms: u64) -> String {
    let (secs, millis) = (ms / 1000, ms % 1000);
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
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        rem / 3_600,
        (rem % 3_600) / 60,
        rem % 60
    )
}

/// RFC 3339, UTC, to the second — the shape a `comments` reply's `at` takes.
pub fn utc(ms: u64) -> String {
    stamp(ms - ms % 1000).replace(".000Z", "Z")
}

impl Store {
    /// A fresh uuid, its bits a hash of a counter, so ids carry no order.
    fn mint(&mut self) -> String {
        self.counter += 1;
        let mut x = self.counter.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let mut word = || {
            x ^= x >> 30;
            x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
            x ^= x >> 27;
            x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
            x ^= x >> 31;
            x
        };
        let (a, b) = (word(), word());
        format!(
            "{:08x}-{:04x}-4{:03x}-{:04x}-{:012x}",
            a >> 32,
            (a >> 16) & 0xffff,
            a & 0xfff,
            0x8000 | ((b >> 48) & 0x3fff),
            b & 0xffff_ffff_ffff
        )
    }

    fn user(&self, id: &str) -> Option<&User> {
        self.users.iter().find(|u| u.id == id)
    }

    fn state(&self, id: &str) -> Option<&State> {
        self.states.iter().find(|s| s.id == id)
    }

    fn issue_at(&self, id: &str) -> Option<usize> {
        self.issues.iter().position(|i| i.id == id)
    }

    fn identifier(&self, issue: &Issue) -> String {
        format!("{TEAM}-{}", issue.number)
    }

    fn state_json(&self, id: &str) -> Value {
        let s = self.state(id).expect("a seeded state");
        json!({"id": s.id, "name": s.name, "type": s.kind})
    }

    /// A sub-issue node, as `Issues` and `IssueChildren` select one.
    fn child_json(&self, c: &Issue) -> Value {
        json!({"id": c.id, "identifier": self.identifier(c), "title": c.title,
               "state": {"name": self.state(&c.state).map_or("", |s| s.name.as_str())}})
    }

    /// The issue's sub-issues, archived ones left out, in the order they were filed.
    fn children(&self, issue: &str) -> Vec<&Issue> {
        self.issues
            .iter()
            .filter(|c| c.parent.as_deref() == Some(issue) && !c.archived)
            .collect()
    }

    /// One page of the relations `target` is the target of, after the cursor `after`, as an
    /// `inverseRelations` connection.
    fn inverse_relations(&self, target: &str, after: Option<&str>, first: usize) -> Value {
        let relations: Vec<&Relation> = self
            .relations
            .iter()
            .filter(|r| r.target == target)
            .collect();
        let (kept, info) = page(relations, |r| r.id.clone(), after, first);
        let nodes: Vec<Value> = kept
            .iter()
            .filter_map(|r| {
                let source = self.issues.iter().find(|i| i.id == r.source)?;
                Some(json!({"type": r.kind, "issue": {
                    "identifier": self.identifier(source),
                    "state": {"type": self.state(&source.state).map_or("", |s| s.kind.as_str())},
                }}))
            })
            .collect();
        json!({"nodes": nodes, "pageInfo": info})
    }

    /// An issue node, every field the `Issues` document selects.
    fn issue_json(&self, issue: &Issue) -> Value {
        let labels: Vec<Value> = issue
            .labels
            .iter()
            .filter_map(|id| self.labels.iter().find(|l| l.id == *id))
            .take(20)
            .map(|l| json!({"name": l.name}))
            .collect();
        let children: Vec<Value> = self
            .children(&issue.id)
            .into_iter()
            .take(20)
            .map(|c| self.child_json(c))
            .collect();
        let parent = issue
            .parent
            .as_deref()
            .and_then(|p| self.issues.iter().find(|i| i.id == p))
            .map(|p| json!({"identifier": self.identifier(p), "title": p.title}));
        let identifier = self.identifier(issue);
        json!({
            "id": issue.id,
            "identifier": identifier,
            "title": issue.title,
            "description": issue.description,
            "url": format!("https://linear.app/acme/issue/{identifier}/{}", slug(&issue.title)),
            "sortOrder": issue.sort_order,
            "state": self.state_json(&issue.state),
            "labels": {"nodes": labels},
            "assignee": issue.assignee.as_ref().map(|id| json!({"id": id})),
            "delegate": issue.delegate.as_ref().map(|id| json!({"id": id})),
            "parent": parent,
            "children": {"nodes": children},
            "inverseRelations": self.inverse_relations(&issue.id, None, 20),
        })
    }

    fn comment_json(&self, c: &Comment) -> Value {
        let (user, bot, external) = match &c.author {
            Author::User(id) => {
                let u = self.user(id);
                (
                    json!({"id": id, "name": u.map_or("", |u| u.name.as_str()),
                           "displayName": u.map_or("", |u| u.display_name.as_str())}),
                    Value::Null,
                    Value::Null,
                )
            }
            Author::Bot(id) => (Value::Null, json!({"id": id, "name": "afkd"}), Value::Null),
            Author::External(id) => (
                Value::Null,
                Value::Null,
                json!({"id": id, "name": "Álvaro (Slack)"}),
            ),
        };
        json!({
            "id": c.id,
            "body": c.body,
            "createdAt": stamp(c.created),
            "editedAt": c.edited.map(stamp),
            "user": user,
            "botActor": bot,
            "externalUser": external,
        })
    }

    /// A state's or a label's issues, as the `Issues` filter the plugin sends selects them:
    /// open, in the team, in manual order — ascending or descending.
    fn select(&self, filter: &Value, descending: bool) -> Result<Vec<&Issue>, String> {
        let filter = filter.as_object().cloned().unwrap_or_default();
        let mut state = None;
        let mut label = None;
        for (key, value) in &filter {
            match key.as_str() {
                "team" if value.as_object().map(|o| o.len()) == Some(1) => {
                    let id = value["id"]["eq"]
                        .as_str()
                        .ok_or("filter.team is not id.eq")?;
                    if id != self.team_id {
                        return Ok(Vec::new());
                    }
                }
                "state" if value.as_object().map(|o| o.len()) == Some(1) => {
                    state = Some(
                        value["id"]["eq"]
                            .as_str()
                            .ok_or("filter.state is not id.eq")?,
                    );
                }
                "labels" if value.as_object().map(|o| o.len()) == Some(1) => {
                    label = Some(
                        value["some"]["name"]["eq"]
                            .as_str()
                            .ok_or("filter.labels is not some.name.eq")?,
                    );
                }
                other => return Err(format!("the fake answers no Issues filter on `{other}`")),
            }
        }
        if !filter.contains_key("team") {
            return Err("the fake answers no Issues filter without the team".into());
        }
        let mut issues: Vec<&Issue> = self
            .issues
            .iter()
            .filter(|i| !i.archived)
            .filter(|i| state.is_none_or(|s| i.state == s))
            .filter(|i| {
                label.is_none_or(|name| {
                    i.labels
                        .iter()
                        .any(|id| self.labels.iter().any(|l| l.id == *id && l.name == name))
                })
            })
            .collect();
        issues.sort_by(|a, b| a.sort_order.total_cmp(&b.sort_order));
        if descending {
            issues.reverse();
        }
        Ok(issues)
    }

    /// A new upload of `size` bytes, signed and with nothing put there yet.
    fn sign(&mut self, filename: &str, content_type: &str, size: usize) -> Upload {
        let (id, sig, asset) = (self.mint(), self.mint(), self.mint());
        Upload {
            filename: filename.to_string(),
            content_type: content_type.to_string(),
            size,
            upload_path: format!("/signed/{id}?sig={sig}"),
            asset_path: format!("/uploads/{}/{id}/{asset}", self.org_id),
            header: (UPLOAD_HEADER.to_string(), format!("upload {sig}")),
            bytes: None,
        }
    }

    /// Who the `Authorization` header authenticates as, if anyone.
    fn viewer(&self, authorization: Option<&str>) -> Option<String> {
        match authorization? {
            key if key == API_KEY => Some(self.key_user.clone()),
            bearer => {
                let token = bearer.strip_prefix("Bearer ")?;
                self.tokens
                    .iter()
                    .any(|t| t.value == token && !t.revoked && Instant::now() < t.expires)
                    .then(|| self.app_user.clone())
            }
        }
    }
}

/// A lowercase ASCII slug of a title, as Linear's issue urls carry one.
fn slug(title: &str) -> String {
    let mut slug = String::new();
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.trim_end_matches('-').to_string()
}

/// The fake. Dropping it leaves the listener thread parked on `accept`; the process ends it.
#[derive(Clone)]
pub struct FakeLinear {
    base: String,
    store: Arc<Mutex<Store>>,
}

fn lock(store: &Mutex<Store>) -> MutexGuard<'_, Store> {
    store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The documents `operations.graphql` holds, by name, whitespace collapsed.
fn documents() -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut current: Option<(String, String)> = None;
    for line in OPERATIONS.lines() {
        if line.starts_with("query ") || line.starts_with("mutation ") {
            if let Some((name, doc)) = current.take() {
                out.insert(name, collapse(&doc));
            }
            let name = line
                .split(|c: char| c.is_whitespace() || c == '(' || c == '{')
                .nth(1)
                .unwrap_or_default()
                .to_string();
            current = Some((name, String::new()));
        }
        if let Some((_, doc)) = current.as_mut() {
            doc.push_str(line);
            doc.push('\n');
        }
    }
    if let Some((name, doc)) = current {
        out.insert(name, collapse(&doc));
    }
    out
}

fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The document `operations.graphql` holds under `name`, as the fake compares it: what a
/// test that reaches Linear the way a run's own script would sends.
pub fn operation_document(name: &str) -> Option<String> {
    documents().remove(name)
}

impl FakeLinear {
    /// A fake with team `ENG`, its API key's user and its app's user, listening on
    /// `127.0.0.1:0`.
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let base = format!("http://{}", listener.local_addr().expect("local addr"));
        let store = Arc::new(Mutex::new(Store {
            base: base.clone(),
            token_lifetime: 2_591_999,
            comments_page: 100,
            ..Store::default()
        }));
        {
            let mut s = lock(&store);
            s.org_id = s.mint();
            s.team_id = s.mint();
            let key_user = s.mint();
            s.users.push(User {
                id: key_user.clone(),
                name: "afkd".into(),
                display_name: "afkd".into(),
                email: "afkd@acme.example".into(),
            });
            s.key_user = key_user;
            let app_user = s.mint();
            s.users.push(User {
                id: app_user.clone(),
                name: "afkd (app)".into(),
                display_name: "afkd-app".into(),
                email: "app@oauthapp.example".into(),
            });
            s.app_user = app_user;
            s.bot_id = s.mint();
        }
        let schema = Arc::new(Sdl::parse(SCHEMA).expect("the excerpt parses"));
        let docs = Arc::new(documents());
        let shared = Arc::clone(&store);
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (store, schema, docs) =
                    (Arc::clone(&shared), Arc::clone(&schema), Arc::clone(&docs));
                thread::spawn(move || serve(stream, &store, &schema, &docs));
            }
        });
        Self { base, store }
    }

    /// The API root, as a config's `base_url` writes it.
    pub fn base_url(&self) -> &str {
        &self.base
    }

    pub fn team_id(&self) -> String {
        lock(&self.store).team_id.clone()
    }

    /// The API key's user: the viewer an API key reads as.
    pub fn key_user(&self) -> String {
        lock(&self.store).key_user.clone()
    }

    /// The app's user: the viewer an app's token reads as.
    pub fn app_user(&self) -> String {
        lock(&self.store).app_user.clone()
    }

    /// The bot actor an app's comments are authored by, when
    /// [`comments_as_bot`](Self::comments_as_bot) says so.
    pub fn bot_id(&self) -> String {
        lock(&self.store).bot_id.clone()
    }

    /// Author the app's comments by its bot actor, not its user.
    pub fn comments_as_bot(&self) {
        lock(&self.store).bot_comments = true;
    }

    /// Add a workflow state of the category `kind`; returns its id.
    pub fn state(&self, name: &str, kind: &str) -> String {
        let mut s = lock(&self.store);
        let id = s.mint();
        s.states.push(State {
            id: id.clone(),
            name: name.to_string(),
            kind: kind.to_string(),
        });
        id
    }

    /// Add a user; returns its id.
    pub fn user(&self, name: &str, display_name: &str, email: &str) -> String {
        let mut s = lock(&self.store);
        let id = s.mint();
        s.users.push(User {
            id: id.clone(),
            name: name.to_string(),
            display_name: display_name.to_string(),
            email: email.to_string(),
        });
        id
    }

    /// Add an issue at the bottom of the state named `state`; returns its id.
    pub fn issue(&self, state: &str, title: &str, description: &str) -> String {
        let mut s = lock(&self.store);
        let state = s
            .states
            .iter()
            .find(|st| st.name == state)
            .map(|st| st.id.clone())
            .expect("a seeded state");
        let id = s.mint();
        let number = s.issues.len() as u32 + 101;
        let sort_order = s
            .issues
            .iter()
            .map(|i| i.sort_order)
            .fold(-1000.0_f64, f64::max)
            + 1000.0;
        s.issues.push(Issue {
            id: id.clone(),
            number,
            title: title.to_string(),
            description: (!description.is_empty()).then(|| description.to_string()),
            state,
            labels: Vec::new(),
            assignee: None,
            delegate: None,
            parent: None,
            sort_order,
            archived: false,
        });
        id
    }

    /// An issue's identifier (`ENG-101`).
    pub fn identifier(&self, issue: &str) -> String {
        let s = lock(&self.store);
        let at = s.issue_at(issue).expect("a seeded issue");
        s.identifier(&s.issues[at])
    }

    /// An issue's url, as the fake serves it.
    pub fn url(&self, issue: &str) -> String {
        let s = lock(&self.store);
        let at = s.issue_at(issue).expect("a seeded issue");
        s.issue_json(&s.issues[at])["url"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    fn edit(&self, issue: &str, f: impl FnOnce(&mut Issue)) {
        let mut s = lock(&self.store);
        let at = s.issue_at(issue).expect("a seeded issue");
        f(&mut s.issues[at]);
    }

    pub fn assign(&self, issue: &str, user: &str) {
        self.edit(issue, |i| i.assignee = Some(user.to_string()));
    }

    pub fn delegate(&self, issue: &str, user: &str) {
        self.edit(issue, |i| i.delegate = Some(user.to_string()));
    }

    pub fn set_parent(&self, child: &str, parent: &str) {
        self.edit(child, |i| i.parent = Some(parent.to_string()));
    }

    /// Relate `source` to `target` as `kind`: `blocks` makes `source` a blocker of `target`.
    pub fn relate(&self, source: &str, target: &str, kind: &str) {
        let mut s = lock(&self.store);
        let id = s.mint();
        s.relations.push(Relation {
            id,
            source: source.to_string(),
            target: target.to_string(),
            kind: kind.to_string(),
        });
    }

    /// Move an issue to the bottom of the state named `state`, as a person does.
    pub fn move_to(&self, issue: &str, state: &str) {
        let mut s = lock(&self.store);
        let state = s
            .states
            .iter()
            .find(|st| st.name == state)
            .map(|st| st.id.clone())
            .expect("a seeded state");
        let bottom = s
            .issues
            .iter()
            .map(|i| i.sort_order)
            .fold(0.0_f64, f64::max);
        let at = s.issue_at(issue).expect("a seeded issue");
        s.issues[at].state = state;
        s.issues[at].sort_order = bottom + 1000.0;
    }

    /// Put the team's label named `name` on an issue, creating it when need be.
    pub fn label(&self, issue: &str, name: &str) {
        let mut s = lock(&self.store);
        let team = s.team_id.clone();
        let label = match s
            .labels
            .iter()
            .find(|l| l.name == name && l.team.as_deref() == Some(&team))
        {
            Some(l) => l.id.clone(),
            None => {
                let id = s.mint();
                s.labels.push(Label {
                    id: id.clone(),
                    name: name.to_string(),
                    team: Some(team),
                });
                id
            }
        };
        let at = s.issue_at(issue).expect("a seeded issue");
        if !s.issues[at].labels.contains(&label) {
            s.issues[at].labels.push(label);
        }
    }

    /// Take the label named `name` off an issue, as a person does.
    pub fn unlabel(&self, issue: &str, name: &str) {
        let mut s = lock(&self.store);
        let ids: Vec<String> = s
            .labels
            .iter()
            .filter(|l| l.name == name)
            .map(|l| l.id.clone())
            .collect();
        let at = s.issue_at(issue).expect("a seeded issue");
        s.issues[at].labels.retain(|l| !ids.contains(l));
    }

    /// Seed a comment by `author`, posted `ago` seconds before now; returns its id.
    pub fn comment(&self, issue: &str, author: Author, body: &str, ago: u64) -> String {
        let mut s = lock(&self.store);
        let id = s.mint();
        s.comments.push(Comment {
            id: id.clone(),
            issue: issue.to_string(),
            author,
            body: body.to_string(),
            created: now_ms() - ago * 1000,
            edited: None,
        });
        id
    }

    /// An issue's comments, in the order they were posted.
    pub fn comments(&self, issue: &str) -> Vec<Comment> {
        let s = lock(&self.store);
        let mut thread: Vec<Comment> = s
            .comments
            .iter()
            .filter(|c| c.issue == issue)
            .cloned()
            .collect();
        thread.sort_by_key(|c| c.created);
        thread
    }

    /// An issue's label names.
    pub fn labels(&self, issue: &str) -> Vec<String> {
        let s = lock(&self.store);
        let at = s.issue_at(issue).expect("a seeded issue");
        s.issues[at]
            .labels
            .iter()
            .filter_map(|id| s.labels.iter().find(|l| l.id == *id))
            .map(|l| l.name.clone())
            .collect()
    }

    /// An issue's title.
    pub fn title_of(&self, issue: &str) -> String {
        let s = lock(&self.store);
        let at = s.issue_at(issue).expect("a seeded issue");
        s.issues[at].title.clone()
    }

    /// An issue's description, `""` for none.
    pub fn description_of(&self, issue: &str) -> String {
        let s = lock(&self.store);
        let at = s.issue_at(issue).expect("a seeded issue");
        s.issues[at].description.clone().unwrap_or_default()
    }

    /// Whether an issue is archived.
    pub fn archived(&self, issue: &str) -> bool {
        let s = lock(&self.store);
        let at = s.issue_at(issue).expect("a seeded issue");
        s.issues[at].archived
    }

    /// The id of an issue's parent, if it has one.
    pub fn parent_of(&self, issue: &str) -> Option<String> {
        let s = lock(&self.store);
        let at = s.issue_at(issue).expect("a seeded issue");
        s.issues[at].parent.clone()
    }

    /// The id of the issue titled `title`, if there is one.
    pub fn find_by_title(&self, title: &str) -> Option<String> {
        lock(&self.store)
            .issues
            .iter()
            .find(|i| i.title == title)
            .map(|i| i.id.clone())
    }

    /// The name of the state an issue sits in.
    pub fn state_of(&self, issue: &str) -> String {
        let s = lock(&self.store);
        let at = s.issue_at(issue).expect("a seeded issue");
        s.state(&s.issues[at].state)
            .map(|st| st.name.clone())
            .unwrap_or_default()
    }

    /// The ids of the issues in the state named `state`, in manual order.
    pub fn in_state(&self, state: &str) -> Vec<String> {
        let s = lock(&self.store);
        let Some(state) = s.states.iter().find(|st| st.name == state) else {
            return Vec::new();
        };
        let mut issues: Vec<&Issue> = s.issues.iter().filter(|i| i.state == state.id).collect();
        issues.sort_by(|a, b| a.sort_order.total_cmp(&b.sort_order));
        issues.into_iter().map(|i| i.id.clone()).collect()
    }

    /// How long a token lasts, in seconds.
    pub fn token_lifetime(&self, secs: u64) {
        lock(&self.store).token_lifetime = secs;
    }

    /// How many tokens the token endpoint has handed out.
    pub fn token_fetches(&self) -> usize {
        lock(&self.store).token_fetches
    }

    /// Revoke every token handed out so far.
    pub fn revoke_tokens(&self) {
        for t in lock(&self.store).tokens.iter_mut() {
            t.revoked = true;
        }
    }

    /// Answer the next `n` requests of `operation` as Linear answers a rate limit: a 429,
    /// `Retry-After` the `wait` in seconds, and the `RATELIMITED` error.
    pub fn rate_limit(&self, operation: &str, n: u32, wait: u64) {
        lock(&self.store)
            .rate_limits
            .insert(operation.to_string(), (n, wait));
    }

    /// Make every request of `operation` answer `status`.
    pub fn fail(&self, operation: &str, status: u16) {
        lock(&self.store)
            .faults
            .insert(operation.to_string(), status);
    }

    /// Make every comment post whose body contains `needle` answer `status`.
    pub fn fail_post_matching(&self, needle: &str, status: u16) {
        lock(&self.store)
            .post_faults
            .push((needle.to_string(), status));
    }

    /// Answer every request of `operation` as Linear answers a document it refuses.
    pub fn refuse_as_invalid(&self, operation: &str) {
        lock(&self.store).refused.push(operation.to_string());
    }

    /// Stop failing `operation`, and every comment post.
    pub fn heal(&self, operation: &str) {
        let mut s = lock(&self.store);
        s.faults.remove(operation);
        s.rate_limits.remove(operation);
        s.refused.retain(|op| op != operation);
        s.post_faults.clear();
    }

    /// Answer an issue's comments this many a page.
    pub fn comments_page(&self, n: usize) {
        lock(&self.store).comments_page = n;
    }

    /// Take the fake out of reach: every request is read and its connection closed with no
    /// reply, until [`restore`](Self::restore).
    pub fn outage(&self) {
        lock(&self.store).down = true;
    }

    pub fn restore(&self) {
        lock(&self.store).down = false;
    }

    /// On the next `[afkd-claim]` posted to `issue`, a rival's claim lands first: posted a
    /// second earlier by another user, so it out-orders ours.
    pub fn race_on_next_claim(&self, issue: &str) {
        lock(&self.store).races.push(issue.to_string());
    }

    /// Hold `bytes` at a new asset url, as a file a person pasted into an issue, and return
    /// the url.
    pub fn seed_file(&self, filename: &str, content_type: &str, bytes: &[u8]) -> String {
        let mut s = lock(&self.store);
        let upload = s.sign(filename, content_type, bytes.len());
        let url = format!("{}{}", s.base, upload.asset_path);
        s.uploads.push(Upload {
            bytes: Some(bytes.to_vec()),
            ..upload
        });
        url
    }

    /// The bytes held at the asset url `url`, once they were put there.
    pub fn file(&self, url: &str) -> Option<Vec<u8>> {
        let s = lock(&self.store);
        let path = url.strip_prefix(&s.base)?;
        s.uploads
            .iter()
            .find(|u| u.asset_path == path)
            .and_then(|u| u.bytes.clone())
    }

    /// Every upload signed, seeded ones included, in order.
    pub fn uploads(&self) -> Vec<SeenUpload> {
        let s = lock(&self.store);
        s.uploads
            .iter()
            .map(|u| SeenUpload {
                filename: u.filename.clone(),
                content_type: u.content_type.clone(),
                size: u.size,
                asset_url: format!("{}{}", s.base, u.asset_path),
                landed: u.bytes.is_some(),
            })
            .collect()
    }

    /// Every PUT to the storage, in order.
    pub fn puts(&self) -> Vec<SeenPut> {
        lock(&self.store).puts.clone()
    }

    /// Answer the next `n` PUTs with `status`, storing nothing.
    pub fn fail_uploads(&self, n: u32, status: u16) {
        lock(&self.store).upload_faults = (n, status);
    }

    /// Every asset url of this fake that `text` names, in order.
    pub fn asset_urls_of(&self, text: &str) -> Vec<String> {
        let prefix = format!("{}/uploads/", self.base);
        text.match_indices(&prefix)
            .map(|(at, _)| {
                let rest = &text[at..];
                let end = rest
                    .find(|c: char| c == ')' || c == '>' || c.is_whitespace())
                    .unwrap_or(rest.len());
                rest[..end].to_string()
            })
            .collect()
    }

    /// Every request answered, in order.
    pub fn seen(&self) -> Vec<Seen> {
        lock(&self.store).seen.clone()
    }

    /// The requests an outage dropped.
    pub fn dropped(&self) -> Vec<Seen> {
        lock(&self.store).dropped.clone()
    }

    /// Every request refused as not conforming to the schema or the operations, each as
    /// the sentence it was refused with.
    pub fn invalid_requests(&self) -> Vec<String> {
        lock(&self.store).invalid.clone()
    }

    /// Forget the refused requests, for a leg that provokes one on purpose.
    pub fn clear_invalid(&self) {
        lock(&self.store).invalid.clear();
    }

    /// POST `body` to the GraphQL endpoint as a client would, with the API key: the status
    /// and the body that came back.
    pub fn post_raw(&self, body: &Value) -> (u16, Value) {
        let addr = self.base.trim_start_matches("http://");
        let mut stream = TcpStream::connect(addr).expect("connect to the fake");
        let body = body.to_string();
        write!(
            stream,
            "POST /graphql HTTP/1.1\r\nHost: {addr}\r\nAuthorization: {API_KEY}\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .expect("write the request");
        let mut reply = String::new();
        stream.read_to_string(&mut reply).expect("read the reply");
        let status = reply
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let json = reply
            .split_once("\r\n\r\n")
            .and_then(|(_, b)| serde_json::from_str(b).ok())
            .unwrap_or(Value::Null);
        (status, json)
    }
}

/// One request as read off the socket.
struct Raw {
    method: String,
    path: String,
    /// Every header but the length, as sent.
    headers: Vec<(String, String)>,
    authorization: Option<String>,
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
    let (mut length, mut authorization, mut headers) = (0usize, None, Vec::new());
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
            } else {
                if name.eq_ignore_ascii_case("authorization") {
                    authorization = Some(value.trim().to_string());
                }
                headers.push((name.trim().to_string(), value.trim().to_string()));
            }
        }
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).ok()?;
    Some(Raw {
        method,
        path,
        headers,
        authorization,
        body,
    })
}

/// One reply: its status, its extra headers, and its JSON body.
type Reply = (u16, Vec<(String, String)>, Value);

/// What a request is answered with: a JSON reply, or a file's bytes and their type.
enum Served {
    Json(Reply),
    Bytes {
        status: u16,
        content_type: String,
        body: Vec<u8>,
    },
}

/// An `errors` body of one error.
fn error(message: &str, code: &str, kind: &str) -> Value {
    json!({"errors": [{"message": message, "extensions": {"code": code, "type": kind}}]})
}

/// Serve one connection: one request, one reply, then close.
fn serve(stream: TcpStream, store: &Mutex<Store>, schema: &Sdl, docs: &HashMap<String, String>) {
    let Some(raw) = read_request(&stream) else {
        return;
    };
    let reply = {
        let mut s = lock(store);
        answer(&mut s, schema, docs, &raw)
    };
    let (status, headers, content_type, body) = match reply {
        // Out of reach: the stream drops here, closed with no reply.
        None => return,
        Some(Served::Json((status, headers, body))) => (
            status,
            headers,
            "application/json".to_string(),
            body.to_string().into_bytes(),
        ),
        Some(Served::Bytes {
            status,
            content_type,
            body,
        }) => (status, Vec::new(), content_type, body),
    };
    let extra: String = headers
        .iter()
        .map(|(k, v)| format!("{k}: {v}\r\n"))
        .collect();
    let mut stream = stream;
    let _ = write!(
        stream,
        "HTTP/1.1 {status} X\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         Connection: close\r\n{extra}\r\n",
        body.len()
    );
    let _ = stream.write_all(&body);
    let _ = stream.flush();
}

/// Answer one request, or `None` to drop it unanswered.
fn answer(
    s: &mut Store,
    schema: &Sdl,
    docs: &HashMap<String, String>,
    raw: &Raw,
) -> Option<Served> {
    let request: Value = serde_json::from_slice(&raw.body).unwrap_or(Value::Null);
    let seen = Seen {
        method: raw.method.clone(),
        path: raw.path.clone(),
        operation: request["operationName"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        variables: request["variables"].clone(),
        authorization: raw.authorization.clone(),
        form: if raw.path == "/oauth/token" {
            String::from_utf8_lossy(&raw.body).into_owned()
        } else {
            String::new()
        },
        at: Instant::now(),
    };
    if s.down {
        s.dropped.push(seen);
        return None;
    }
    s.seen.push(seen);
    if raw.method == "PUT" && raw.path.starts_with("/signed/") {
        return Some(put(s, raw));
    }
    if raw.method == "GET" && raw.path.starts_with("/uploads/") {
        return Some(asset(s, raw));
    }
    Some(Served::Json(match raw.path.as_str() {
        "/oauth/token" => token(s, &String::from_utf8_lossy(&raw.body)),
        "/graphql" => graphql(s, schema, docs, raw, &request),
        _ => (404, Vec::new(), json!({"error": "not found"})),
    }))
}

/// A PUT of a file's bytes to the url `FileUpload` signed: stored when it carries what the
/// upload guide asks for, else a 403 recorded as invalid. A failure the test injected comes
/// first, and stores nothing.
fn put(s: &mut Store, raw: &Raw) -> Served {
    let header = |name: &str| {
        raw.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    };
    let answer = |status: u16| Served::Bytes {
        status,
        content_type: "text/plain".into(),
        body: Vec::new(),
    };
    let mut seen = SeenPut {
        path: raw.path.clone(),
        headers: raw.headers.clone(),
        len: raw.body.len(),
        status: 200,
    };
    if s.upload_faults.0 > 0 {
        s.upload_faults.0 -= 1;
        seen.status = s.upload_faults.1;
        s.puts.push(seen);
        return answer(s.upload_faults.1);
    }
    let problem = match s.uploads.iter().find(|u| u.upload_path == raw.path) {
        None => Some(format!(
            "a PUT to {}, which the fake never signed",
            raw.path
        )),
        Some(_) if raw.authorization.is_some() => {
            Some("a PUT to the storage carried an Authorization header".to_string())
        }
        Some(u) if u.bytes.is_some() => Some(format!("a second PUT of {}", u.filename)),
        Some(u) if header(&u.header.0) != Some(u.header.1.as_str()) => Some(format!(
            "a PUT of {} without the signed header {}",
            u.filename, u.header.0
        )),
        Some(u) if header("content-type") != Some(u.content_type.as_str()) => Some(format!(
            "a PUT of {} as {:?}, signed as {}",
            u.filename,
            header("content-type"),
            u.content_type
        )),
        Some(u) if header("cache-control") != Some(UPLOAD_CACHE_CONTROL) => Some(format!(
            "a PUT of {} with Cache-Control {:?}",
            u.filename,
            header("cache-control")
        )),
        Some(u) if raw.body.len() != u.size => Some(format!(
            "a PUT of {} bytes for {}, signed for {}",
            raw.body.len(),
            u.filename,
            u.size
        )),
        Some(_) => None,
    };
    if let Some(problem) = problem {
        s.invalid.push(problem);
        seen.status = 403;
        s.puts.push(seen);
        return answer(403);
    }
    if let Some(u) = s.uploads.iter_mut().find(|u| u.upload_path == raw.path) {
        u.bytes = Some(raw.body.clone());
    }
    s.puts.push(seen);
    answer(200)
}

/// A GET of an asset url: its bytes for a caller GraphQL would take, once they were put
/// there; a 401 for anyone else, and a 404 for an asset with no bytes.
fn asset(s: &mut Store, raw: &Raw) -> Served {
    if s.viewer(raw.authorization.as_deref()).is_none() {
        return Served::Json((
            401,
            Vec::new(),
            error(
                "Authentication required, not authenticated",
                "AUTHENTICATION_ERROR",
                "authentication error",
            ),
        ));
    }
    match s
        .uploads
        .iter()
        .find(|u| u.asset_path == raw.path && u.bytes.is_some())
    {
        Some(u) => Served::Bytes {
            status: 200,
            content_type: u.content_type.clone(),
            body: u.bytes.clone().unwrap_or_default(),
        },
        None => Served::Bytes {
            status: 404,
            content_type: "text/plain".into(),
            body: b"Not Found".to_vec(),
        },
    }
}

/// The token endpoint: a token for the app's credentials, with the grant and the scope.
fn token(s: &mut Store, form: &str) -> Reply {
    let pairs: HashMap<String, String> = form
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(k, v)| (percent_decode(k), percent_decode(v)))
        .collect();
    let field = |k: &str| pairs.get(k).map(String::as_str);
    let refuse = |error: &str, description: &str| {
        (
            400,
            Vec::new(),
            json!({"error": error, "error_description": description}),
        )
    };
    if field("grant_type") != Some("client_credentials") {
        return refuse("unsupported_grant_type", "The grant type is not supported");
    }
    if field("client_id") != Some(CLIENT_ID) || field("client_secret") != Some(CLIENT_SECRET) {
        return (
            401,
            Vec::new(),
            json!({"error": "invalid_client", "error_description": "Client authentication failed"}),
        );
    }
    if field("scope") != Some(SCOPE) {
        return refuse("invalid_scope", "The requested scope is invalid");
    }
    s.token_fetches += 1;
    let value = format!("lin_oauth_tok-{}", s.token_fetches);
    s.tokens.push(Token {
        value: value.clone(),
        expires: Instant::now() + Duration::from_secs(s.token_lifetime),
        revoked: false,
    });
    (
        200,
        Vec::new(),
        json!({"access_token": value, "token_type": "Bearer", "expires_in": s.token_lifetime,
               "scope": "read write app:assignable app:mentionable"}),
    )
}

/// The GraphQL endpoint: the request checked, the caller authenticated, any injected fault
/// answered, then the operation dispatched.
fn graphql(
    s: &mut Store,
    schema: &Sdl,
    docs: &HashMap<String, String>,
    raw: &Raw,
    request: &Value,
) -> Reply {
    let invalid = |s: &mut Store, sentence: String| {
        s.invalid.push(sentence.clone());
        (
            400,
            Vec::new(),
            error(&sentence, "GRAPHQL_VALIDATION_FAILED", "graphql error"),
        )
    };
    let name = request["operationName"].as_str().unwrap_or_default();
    let query = request["query"].as_str().unwrap_or_default();
    let operation = match schema.check_document(query) {
        Ok(op) => op,
        Err(e) => return invalid(s, e),
    };
    if operation.name != name {
        return invalid(
            s,
            format!(
                "operationName {name} names a document called {}",
                operation.name
            ),
        );
    }
    if docs.get(name) != Some(&collapse(query)) {
        return invalid(
            s,
            format!("{name} is not the document operations.graphql holds"),
        );
    }
    if let Err(e) = schema.check_variables(&operation.variables, &request["variables"]) {
        return invalid(s, e);
    }
    let Some(viewer) = s.viewer(raw.authorization.as_deref()) else {
        return (
            401,
            Vec::new(),
            error(
                "Authentication required, not authenticated",
                "AUTHENTICATION_ERROR",
                "authentication error",
            ),
        );
    };
    if s.refused.iter().any(|op| op == name) {
        return (
            400,
            Vec::new(),
            error(
                &format!("Field \"{name}\" argument is invalid."),
                "GRAPHQL_VALIDATION_FAILED",
                "graphql error",
            ),
        );
    }
    if let Some((left, wait)) = s.rate_limits.get_mut(name) {
        if *left > 0 {
            *left -= 1;
            let wait = *wait;
            return (
                429,
                vec![("Retry-After".into(), wait.to_string())],
                error("Rate limit exceeded", "RATELIMITED", "ratelimited"),
            );
        }
    }
    if let Some(status) = s.faults.get(name) {
        return (
            *status,
            Vec::new(),
            error("injected", "INTERNAL_SERVER_ERROR", "internal error"),
        );
    }
    let vars = &request["variables"];
    match dispatch(s, name, vars, &viewer) {
        Ok(data) => (200, Vec::new(), json!({"data": data})),
        Err(Refusal::NotFound(what)) => (
            200,
            Vec::new(),
            error(
                &format!("Entity not found: {what}"),
                "INVALID_INPUT",
                "invalid input",
            ),
        ),
        Err(Refusal::Invalid(sentence)) => invalid(s, sentence),
        Err(Refusal::Status(status)) => (
            status,
            Vec::new(),
            error("injected", "INTERNAL_SERVER_ERROR", "internal error"),
        ),
    }
}

/// Why an operation was not done.
enum Refusal {
    /// What it names does not exist.
    NotFound(&'static str),
    /// It asks for what the plugin never means.
    Invalid(String),
    /// A fault the test injected.
    Status(u16),
}

/// The page of `items` after the cursor `after`, `first` long, and its `pageInfo`; a cursor
/// is the last item's id.
fn page<T>(
    items: Vec<T>,
    id: impl Fn(&T) -> String,
    after: Option<&str>,
    first: usize,
) -> (Vec<T>, Value) {
    let start = after
        .and_then(|a| items.iter().position(|i| id(i) == a).map(|p| p + 1))
        .unwrap_or(0);
    let rest: Vec<T> = items.into_iter().skip(start).collect();
    let more = rest.len() > first;
    let kept: Vec<T> = rest.into_iter().take(first).collect();
    let end = kept.last().map(&id);
    (kept, json!({"hasNextPage": more, "endCursor": end}))
}

fn dispatch(s: &mut Store, name: &str, vars: &Value, viewer: &str) -> Result<Value, Refusal> {
    let var = |key: &str| vars[key].as_str().unwrap_or_default().to_string();
    match name {
        "Viewer" => Ok(json!({"viewer": {"id": viewer}})),
        "TeamByKey" => {
            let key = vars["filter"]["key"]["eq"].as_str().unwrap_or_default();
            let nodes = if key == TEAM {
                vec![json!({"id": s.team_id, "key": TEAM})]
            } else {
                Vec::new()
            };
            Ok(json!({"teams": {"nodes": nodes}}))
        }
        "States" => {
            if var("teamId") != s.team_id {
                return Err(Refusal::NotFound("Team"));
            }
            let nodes: Vec<Value> = s.states.iter().map(|st| s.state_json(&st.id)).collect();
            Ok(json!({"team": {"states": {"nodes": nodes}}}))
        }
        "Issues" => {
            let descending = vars["sort"][0]["manual"]["order"] == "Descending";
            let issues = s
                .select(&vars["filter"], descending)
                .map_err(Refusal::Invalid)?;
            let first = vars["first"].as_u64().unwrap_or(50) as usize;
            let (kept, info) = page(issues, |i| i.id.clone(), vars["after"].as_str(), first);
            let nodes: Vec<Value> = kept.iter().map(|i| s.issue_json(i)).collect();
            Ok(json!({"issues": {"nodes": nodes, "pageInfo": info}}))
        }
        "IssueRelations" => {
            let issue = var("id");
            if s.issue_at(&issue).is_none() {
                return Err(Refusal::NotFound("Issue"));
            }
            let connection = s.inverse_relations(&issue, vars["after"].as_str(), 50);
            Ok(json!({"issue": {"inverseRelations": connection}}))
        }
        "IssueDescription" => {
            let at = s.issue_at(&var("id")).ok_or(Refusal::NotFound("Issue"))?;
            Ok(json!({"issue": {"description": s.issues[at].description}}))
        }
        "IssueChildren" => {
            let issue = var("id");
            if s.issue_at(&issue).is_none() {
                return Err(Refusal::NotFound("Issue"));
            }
            let children = s.children(&issue);
            let (kept, info) = page(children, |c| c.id.clone(), vars["after"].as_str(), 50);
            let nodes: Vec<Value> = kept.iter().map(|c| s.child_json(c)).collect();
            Ok(json!({"issue": {"children": {"nodes": nodes, "pageInfo": info}}}))
        }
        "IssueComments" => {
            let issue = var("id");
            if s.issue_at(&issue).is_none() {
                return Err(Refusal::NotFound("Issue"));
            }
            let mut thread: Vec<&Comment> =
                s.comments.iter().filter(|c| c.issue == issue).collect();
            thread.sort_by(|a, b| b.created.cmp(&a.created).then(b.id.cmp(&a.id)));
            let (kept, info) = page(
                thread,
                |c| c.id.clone(),
                vars["after"].as_str(),
                s.comments_page,
            );
            let nodes: Vec<Value> = kept.iter().map(|c| s.comment_json(c)).collect();
            Ok(json!({"issue": {"comments": {"nodes": nodes, "pageInfo": info}}}))
        }
        "Users" => {
            let or = vars["filter"]["or"].as_array().cloned().unwrap_or_default();
            let wants: Vec<(String, String)> = or
                .iter()
                .filter_map(|f| {
                    let (k, v) = f.as_object()?.iter().next()?;
                    Some((k.clone(), v["eq"].as_str()?.to_string()))
                })
                .collect();
            let nodes: Vec<Value> = s
                .users
                .iter()
                .filter(|u| {
                    wants.iter().any(|(k, v)| match k.as_str() {
                        "displayName" => u.display_name == *v,
                        "email" => u.email == *v,
                        _ => false,
                    })
                })
                .take(2)
                .map(|u| json!({"id": u.id}))
                .collect();
            Ok(json!({"users": {"nodes": nodes}}))
        }
        "Labels" => {
            let filter = vars["filter"].as_object().cloned().unwrap_or_default();
            if filter.keys().any(|k| k != "name") {
                return Err(Refusal::Invalid(
                    "the fake answers Labels by name only".into(),
                ));
            }
            let name = vars["filter"]["name"]["eq"].as_str().unwrap_or_default();
            let nodes: Vec<Value> = s
                .labels
                .iter()
                .filter(|l| l.name == name)
                .map(|l| json!({"id": l.id, "team": l.team.as_ref().map(|t| json!({"id": t}))}))
                .collect();
            Ok(json!({"issueLabels": {"nodes": nodes}}))
        }
        "CommentCreate" => {
            let issue = vars["input"]["issueId"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let body = vars["input"]["body"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            if s.issue_at(&issue).is_none() {
                return Err(Refusal::NotFound("Issue"));
            }
            if let Some((_, status)) = s.post_faults.iter().find(|(n, _)| body.contains(n)) {
                return Err(Refusal::Status(*status));
            }
            let now = now_ms();
            if body.starts_with("[afkd-claim]") {
                if let Some(at) = s.races.iter().position(|r| *r == issue) {
                    s.races.remove(at);
                    let rival = s
                        .users
                        .iter()
                        .find(|u| u.display_name == "rival-host")
                        .map(|u| u.id.clone())
                        .unwrap_or_default();
                    let id = s.mint();
                    s.comments.push(Comment {
                        id,
                        issue: issue.clone(),
                        author: Author::User(rival),
                        body: "[afkd-claim] owner=afkd-17".into(),
                        created: now - 1000,
                        edited: None,
                    });
                }
            }
            let author = if viewer == s.app_user && s.bot_comments {
                Author::Bot(s.bot_id.clone())
            } else {
                Author::User(viewer.to_string())
            };
            let id = s.mint();
            let comment = Comment {
                id,
                issue,
                author,
                body,
                created: now,
                edited: None,
            };
            let node = s.comment_json(&comment);
            s.comments.push(comment);
            Ok(json!({"commentCreate": {"success": true, "comment": node}}))
        }
        "CommentUpdate" => {
            let id = var("id");
            let Some(c) = s.comments.iter_mut().find(|c| c.id == id) else {
                return Err(Refusal::NotFound("Comment"));
            };
            c.body = vars["input"]["body"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            c.edited = Some(now_ms());
            Ok(json!({"commentUpdate": {"success": true}}))
        }
        "CommentDelete" => {
            let id = var("id");
            let Some(at) = s.comments.iter().position(|c| c.id == id) else {
                return Err(Refusal::NotFound("Comment"));
            };
            s.comments.remove(at);
            Ok(json!({"commentDelete": {"success": true}}))
        }
        "IssueAddLabel" | "IssueRemoveLabel" => {
            let (issue, label) = (var("id"), var("labelId"));
            if !s.labels.iter().any(|l| l.id == label) {
                return Err(Refusal::NotFound("IssueLabel"));
            }
            let at = s.issue_at(&issue).ok_or(Refusal::NotFound("Issue"))?;
            let labels = &mut s.issues[at].labels;
            if name == "IssueAddLabel" {
                if !labels.contains(&label) {
                    labels.push(label);
                }
            } else {
                labels.retain(|l| *l != label);
            }
            let field = if name == "IssueAddLabel" {
                "issueAddLabel"
            } else {
                "issueRemoveLabel"
            };
            Ok(json!({ field: {"success": true} }))
        }
        "IssueLabelCreate" => {
            let id = s.mint();
            s.labels.push(Label {
                id: id.clone(),
                name: vars["input"]["name"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                team: vars["input"]["teamId"].as_str().map(str::to_string),
            });
            Ok(json!({"issueLabelCreate": {"success": true, "issueLabel": {"id": id}}}))
        }
        "IssueUpdate" => {
            let at = s.issue_at(&var("id")).ok_or(Refusal::NotFound("Issue"))?;
            let input = &vars["input"];
            if let Some(state) = input["stateId"].as_str() {
                if s.state(state).is_none() {
                    return Err(Refusal::NotFound("WorkflowState"));
                }
                s.issues[at].state = state.to_string();
            }
            if let Some(order) = input["sortOrder"].as_f64() {
                s.issues[at].sort_order = order;
            }
            if let Some(title) = input["title"].as_str() {
                s.issues[at].title = title.to_string();
            }
            if let Some(text) = input["description"].as_str() {
                s.issues[at].description = (!text.is_empty()).then(|| text.to_string());
            }
            Ok(json!({"issueUpdate": {"success": true}}))
        }
        "IssueCreate" => {
            let input = &vars["input"];
            if input["teamId"].as_str() != Some(s.team_id.as_str()) {
                return Err(Refusal::NotFound("Team"));
            }
            let parent = input["parentId"].as_str().map(str::to_string);
            if parent.as_deref().is_some_and(|p| s.issue_at(p).is_none()) {
                return Err(Refusal::NotFound("Issue"));
            }
            let state = match input["stateId"].as_str() {
                Some(state) if s.state(state).is_none() => {
                    return Err(Refusal::NotFound("WorkflowState"))
                }
                Some(state) => state.to_string(),
                None => ["backlog", "unstarted"]
                    .iter()
                    .find_map(|kind| s.states.iter().find(|st| st.kind == *kind))
                    .or(s.states.first())
                    .map(|st| st.id.clone())
                    .ok_or(Refusal::NotFound("WorkflowState"))?,
            };
            let id = s.mint();
            let number = s.issues.len() as u32 + 101;
            let sort_order = s
                .issues
                .iter()
                .map(|i| i.sort_order)
                .fold(-1000.0_f64, f64::max)
                + 1000.0;
            let issue = Issue {
                id,
                number,
                title: input["title"].as_str().unwrap_or_default().to_string(),
                description: input["description"].as_str().map(str::to_string),
                state,
                labels: Vec::new(),
                assignee: None,
                delegate: None,
                parent,
                sort_order,
                archived: false,
            };
            let identifier = s.identifier(&issue);
            s.issues.push(issue);
            Ok(json!({"issueCreate": {"success": true, "issue": {"identifier": identifier}}}))
        }
        "IssueArchive" => {
            let at = s.issue_at(&var("id")).ok_or(Refusal::NotFound("Issue"))?;
            s.issues[at].archived = true;
            Ok(json!({"issueArchive": {"success": true}}))
        }
        "FileUpload" => {
            let (content_type, filename) = (var("contentType"), var("filename"));
            if content_type.is_empty() || filename.is_empty() {
                return Err(Refusal::Invalid(
                    "the fake signs no upload without a content type and a file name".into(),
                ));
            }
            let size = vars["size"].as_u64().unwrap_or_default() as usize;
            let upload = s.sign(&filename, &content_type, size);
            let node = json!({
                "uploadUrl": format!("{}{}", s.base, upload.upload_path),
                "assetUrl": format!("{}{}", s.base, upload.asset_path),
                "headers": [{"key": upload.header.0, "value": upload.header.1}],
            });
            s.uploads.push(upload);
            Ok(json!({"fileUpload": {"success": true, "uploadFile": node}}))
        }
        other => Err(Refusal::Invalid(format!(
            "the fake answers no operation {other}"
        ))),
    }
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

/// A valid PNG of one black pixel that also holds every byte value: a private ancillary chunk,
/// `afKd`, carries 0 to 255 twice and a multipart-looking `--` line, so the bytes hold NUL,
/// CR LF, `0xFF` and invalid UTF-8. Each chunk carries its real CRC, so a decoder takes it.
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
    // 1×1, 8-bit greyscale, no interlace.
    chunk(&mut png, b"IHDR", &[0, 0, 0, 1, 0, 0, 0, 1, 8, 0, 0, 0, 0]);
    let mut every: Vec<u8> = (0..=255u8).chain(0..=255u8).collect();
    every.extend_from_slice(b"\r\n--afkd-boundary--\r\n");
    chunk(&mut png, b"afKd", &every);
    // The scanline (filter 0, one black pixel) as a zlib stream of one stored block.
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
