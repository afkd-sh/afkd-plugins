//! A stateful fake Linear on a loopback socket: the GraphQL endpoint and the token endpoint
//! the plugin's client calls, over one workspace's teams, states, users, labels, projects,
//! cycles, issues, relations and comments the test seeds and then reads back. std and
//! `serde_json` only, and nothing from the plugin crate, so another suite can
//! `#[path]`-include it.
//!
//! **Every request is held to Linear's schema before it is answered.** A `POST /graphql` is
//! checked by `sdl.rs` against `tests/schema.graphql` — its document, and its variables
//! against the variables the document declares — and its document must be, whitespace
//! aside, the one `src/operations.graphql` holds under its `operationName`. One that fails
//! is answered as Linear answers an invalid request, a 400 with a `graphql error`, and is
//! recorded in [`FakeLinear::invalid_requests`], which every leg holds empty. Past that,
//! only the filter paths the plugin means are answered: an `Issues` filter on anything but
//! a team's id, a state's id, the name of *some* label and the `and` of a claim's gate, and an
//! `IssueList` filter on anything but the paths its reads send, is refused the same way, even
//! where the schema allows it. A gate's clauses are evaluated as an `IssueList` filter's are.
//!
//! The rest is what Linear does that the plugin depends on: an API key is sent bare and an
//! app's token as `Bearer`, from the `client_credentials` grant with the plugin's scope, and
//! lasts as long as the fake says; anything else is a 401 `authentication error`. Each fake is
//! one workspace, with its own organization, url key and credentials, so two fakes stand for
//! two workspaces ([`FakeLinear::start_as`]). Ids are uuids minted in no order, and times come
//! off the real clock to the millisecond (a claim's liveness is judged against the wall
//! clock). A state's issues come back in their manual order, comments newest first, an
//! issue's nested lists 20 to the page an issue read reads and 50 to the page after, an
//! archived issue in no read but one that asks for archived ones, an edit stamps `editedAt`,
//! a missing id is Linear's `Entity not found`. An issue update sets each field its input
//! holds, clears each `null` one and stamps `updatedAt`; one that moves the issue to another
//! team needs a state of that team, and drops what the team lacks, and Linear's refusal of a
//! value the issue cannot take — an estimate of 0 on a team that does not allow one, a state,
//! a cycle or a milestone of another team or project — is answered as Linear answers it. A
//! thread is one level deep, as Linear keeps it: a reply answers a first comment of its own
//! issue, a thread is resolved and unresolved on its first comment, which carries
//! `resolvedAt`, and deleting a first comment deletes its replies. An issue filed with no state starts in its
//! team's first backlog state, else its first unstarted one. Issue numbers are one count
//! from 101 across the teams, and an issue that moves team takes the next. Every request is
//! recorded, and an operation can be rate limited with a `Retry-After`, made to answer a
//! status, or the whole fake taken out of reach.
//!
//! **`IssueList` filters and sorts as Linear documents it.** Its filter is evaluated clause by
//! clause (`and`, `or`, and each field a read sends), `hasBlockedByRelations` counting only a
//! blocker that is not done. Its sort compares key by key, each breaking the ties of the one
//! before: `manual` by the manual order, `team` by key, `workflowState` by the state's place,
//! `priority` urgent first and none last, the names and numbers as they read, every `null`
//! last and a `Descending` key reversed; what is still tied keeps the order it was seeded in.
//! A `sort` of more than three elements, this operation's or any other's, is refused before
//! anything is read, as Linear refuses one: an `Argument Validation Error` naming the limit,
//! recorded in [`FakeLinear::invalid_requests`].
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
//!
//! **The plugin's `linear` skill is answered the same way.** Its scripts send the documents
//! `skills/linear/lib/operations.graphql` holds, and the fake answers those too, held to the
//! same schema; a name both files hold must be one document, or [`FakeLinear::start`] panics
//! naming it. As Linear does, an issue is found by its identifier (`ENG-101`) as well as by its
//! id, wherever a request names one.

#![allow(dead_code)]

#[path = "sdl.rs"]
pub mod sdl;

use std::cmp::Ordering;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use sdl::Sdl;

/// The API key every request to the `acme` workspace may carry as it is.
pub const API_KEY: &str = "lin_api_7f3aK9c1eQ2w";

/// The OAuth app's credentials, which the `acme` workspace's token endpoint trades for a
/// token.
pub const CLIENT_ID: &str = "app-id-7f3a9c1e";
pub const CLIENT_SECRET: &str = "app-secret-4b8d&=/2e";

/// The url key of the workspace [`FakeLinear::start`] stands for.
pub const ACME: &str = "acme";

/// A second workspace, its url key and its credentials: what [`FakeLinear::start_globex`]
/// stands for.
pub const GLOBEX: &str = "globex";
pub const GLOBEX_API_KEY: &str = "lin_api_gl0b3x-9d2c";
pub const GLOBEX_CLIENT_ID: &str = "app-id-gl0b3x";
pub const GLOBEX_CLIENT_SECRET: &str = "app-secret-gl0b3x/7&";

/// The scope an app's token must be asked for.
pub const SCOPE: &str = "read,write,app:assignable,app:mentionable";

/// The team every issue is in unless a test files it in another, by key.
pub const TEAM: &str = "ENG";

/// The schema every request is held to.
const SCHEMA: &str = include_str!("../schema.graphql");

/// The operations the plugin sends, as it writes them.
const OPERATIONS: &str = include_str!("../../src/operations.graphql");

/// The operations the plugin's `linear` skill sends, as its scripts write them.
const SKILL_OPERATIONS: &str = include_str!("../../skills/linear/lib/operations.graphql");

/// The state categories an issue that blocks another is done in.
const CLOSED_KINDS: &[&str] = &["completed", "canceled", "duplicate"];

/// How long Linear keeps an issue in the trash before it deletes it for good.
const THIRTY_DAYS_MS: u64 = 30 * 24 * 3_600 * 1_000;

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
    /// The comment it answers, if it is a reply.
    pub parent: Option<String>,
    /// When it was resolved, once resolved.
    pub resolved: Option<u64>,
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
struct Team {
    id: String,
    key: String,
    name: String,
    /// Whether its issues may be estimated at 0, which Linear takes only from a team that
    /// allows it.
    zero_estimates: bool,
}

#[derive(Debug, Clone)]
struct User {
    id: String,
    name: String,
    display_name: String,
    email: String,
    /// Whether it is an app's user.
    app: bool,
    /// Whether the account is enabled: Linear lists a disabled one in no read.
    active: bool,
}

#[derive(Debug, Clone)]
struct State {
    id: String,
    /// Its team's id.
    team: String,
    name: String,
    kind: String,
    /// Its place in its team's workflow: the order it was seeded in.
    position: f64,
}

#[derive(Debug, Clone)]
struct Label {
    id: String,
    name: String,
    color: String,
    /// `None` for a workspace label.
    team: Option<String>,
    /// The label group it is in.
    parent: Option<String>,
}

#[derive(Debug, Clone)]
struct Issue {
    id: String,
    /// Its team's id.
    team: String,
    number: u32,
    title: String,
    description: Option<String>,
    state: String,
    labels: Vec<String>,
    assignee: Option<String>,
    delegate: Option<String>,
    parent: Option<String>,
    sort_order: f64,
    /// Archived: in no read but one that asks for archived issues, as Linear leaves them out
    /// unless asked.
    archived: bool,
    /// When it went to the trash, which archives it too, in epoch milliseconds.
    trashed: Option<u64>,
    /// Linear's 0 to 4.
    priority: u8,
    estimate: Option<f64>,
    due: Option<String>,
    /// The user who filed it; `None` for an integration.
    creator: Option<String>,
    project: Option<String>,
    /// The name of its milestone in its project.
    milestone: Option<String>,
    cycle: Option<String>,
    created: u64,
    updated: u64,
    completed: Option<u64>,
    canceled: Option<u64>,
    /// Its links, Linear's attachments: each one's id, url and title.
    links: Vec<(String, String, String)>,
    /// The ids of the users subscribed to it.
    subscribers: Vec<String>,
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
struct Milestone {
    id: String,
    name: String,
    sort_order: f64,
}

#[derive(Debug, Clone)]
struct Project {
    id: String,
    name: String,
    /// Its teams' ids.
    teams: Vec<String>,
    /// Its milestones, in the order they were seeded, which need not be their order.
    milestones: Vec<Milestone>,
}

#[derive(Debug, Clone)]
struct Cycle {
    id: String,
    /// Its team's id.
    team: String,
    number: u32,
    name: Option<String>,
    starts: u64,
    ends: u64,
    active: bool,
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
    url_key: String,
    api_key: String,
    client_id: String,
    client_secret: String,
    /// What each token this workspace hands out starts with, after `lin_oauth_`.
    token_prefix: String,
    /// The teams, `ENG` first.
    teams: Vec<Team>,
    key_user: String,
    app_user: String,
    users: Vec<User>,
    states: Vec<State>,
    labels: Vec<Label>,
    issues: Vec<Issue>,
    /// The number the next issue filed, in any team, takes.
    next_number: u32,
    relations: Vec<Relation>,
    comments: Vec<Comment>,
    projects: Vec<Project>,
    cycles: Vec<Cycle>,
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

/// The epoch milliseconds of an RFC 3339 time in UTC, `Z` and to the second or the
/// millisecond, as the plugin's filters write one: [`stamp`]'s inverse.
pub fn ms_of(text: &str) -> Option<u64> {
    let text = text.strip_suffix('Z')?;
    let (date, time) = text.split_once('T')?;
    let mut date = date.splitn(3, '-').map(|p| p.parse::<i64>().ok());
    let (y, m, d) = (date.next()??, date.next()??, date.next()??);
    let (time, millis) = match time.split_once('.') {
        Some((time, fraction)) => (time, fraction.get(..3)?.parse::<u64>().ok()?),
        None => (time, 0),
    };
    let mut time = time.splitn(3, ':').map(|p| p.parse::<u64>().ok());
    let (h, min, s) = (time.next()??, time.next()??, time.next()??);
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = u64::try_from(era * 146_097 + doe - 719_468).ok()?;
    Some((days * 86_400 + h * 3_600 + min * 60 + s) * 1000 + millis)
}

impl Store {
    /// A fresh uuid, its bits a hash of a counter, so ids carry no order.
    fn mint(&mut self) -> String {
        self.counter += 1;
        let mut x = self
            .counter
            .wrapping_add(u64::from(self.url_key.len() as u32) << 40)
            .wrapping_mul(0x9E37_79B9_7F4A_7C15);
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

    fn team(&self, id: &str) -> Option<&Team> {
        self.teams.iter().find(|t| t.id == id)
    }

    /// The state an issue filed in the team `team` without one lands in: its first backlog
    /// state, else its first unstarted one, else its first; none for a team with no state.
    fn default_state(&self, team: &str) -> Option<String> {
        let of_team: Vec<&State> = self.states.iter().filter(|st| st.team == team).collect();
        ["backlog", "unstarted"]
            .iter()
            .find_map(|kind| of_team.iter().find(|st| st.kind == *kind))
            .or(of_team.first())
            .map(|st| st.id.clone())
    }

    /// The id of the team whose key is `key`.
    fn team_id_of(&self, key: &str) -> Option<String> {
        self.teams
            .iter()
            .find(|t| t.key == key)
            .map(|t| t.id.clone())
    }

    fn team_key(&self, id: &str) -> String {
        self.team(id).map(|t| t.key.clone()).unwrap_or_default()
    }

    /// The id of the state named `name` of the team `team`.
    fn state_named(&self, team: &str, name: &str) -> Option<String> {
        self.states
            .iter()
            .find(|s| s.team == team && s.name == name)
            .map(|s| s.id.clone())
    }

    /// Where the issue named by `id` sits, its id or its identifier.
    fn issue_at(&self, id: &str) -> Option<usize> {
        self.issues
            .iter()
            .position(|i| i.id == id || self.identifier(i) == id)
    }

    /// The id of the issue named by `id`, its id or its identifier.
    fn issue_id(&self, id: &str) -> Option<String> {
        self.issue_at(id).map(|at| self.issues[at].id.clone())
    }

    fn issue(&self, id: &str) -> Option<&Issue> {
        self.issues.iter().find(|i| i.id == id)
    }

    fn identifier(&self, issue: &Issue) -> String {
        format!("{}-{}", self.team_key(&issue.team), issue.number)
    }

    fn url(&self, issue: &Issue) -> String {
        format!(
            "https://linear.app/{}/issue/{}/{}",
            self.url_key,
            self.identifier(issue),
            slug(&issue.title)
        )
    }

    fn state_json(&self, id: &str) -> Value {
        let s = self.state(id).expect("a seeded state");
        json!({"id": s.id, "name": s.name, "type": s.kind, "position": s.position})
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

    /// One page of the relations `source` holds, after the cursor `after`, as a `relations`
    /// connection.
    fn out_relations(&self, source: &str, after: Option<&str>, first: usize) -> Value {
        let relations: Vec<&Relation> = self
            .relations
            .iter()
            .filter(|r| r.source == source)
            .collect();
        let (kept, info) = page(relations, |r| r.id.clone(), after, first);
        let nodes: Vec<Value> = kept
            .iter()
            .filter_map(|r| {
                let target = self.issues.iter().find(|i| i.id == r.target)?;
                Some(json!({"type": r.kind,
                            "relatedIssue": {"identifier": self.identifier(target)}}))
            })
            .collect();
        json!({"nodes": nodes, "pageInfo": info})
    }

    /// One page of an issue's labels, as a `labels` connection.
    fn label_names(&self, issue: &Issue, after: Option<&str>, first: usize) -> Value {
        let labels: Vec<&Label> = issue
            .labels
            .iter()
            .filter_map(|id| self.labels.iter().find(|l| l.id == *id))
            .collect();
        let (kept, info) = page(labels, |l| l.id.clone(), after, first);
        let nodes: Vec<Value> = kept.iter().map(|l| json!({"name": l.name})).collect();
        json!({"nodes": nodes, "pageInfo": info})
    }

    /// One page of an issue's sub-issues, as a `children` connection.
    fn children_page(&self, issue: &str, after: Option<&str>, first: usize) -> Value {
        let (kept, info) = page(self.children(issue), |c| c.id.clone(), after, first);
        let nodes: Vec<Value> = kept.iter().map(|c| self.child_json(c)).collect();
        json!({"nodes": nodes, "pageInfo": info})
    }

    /// One page of an issue's links, as an `attachments` connection.
    fn attachments(&self, issue: &Issue, after: Option<&str>, first: usize) -> Value {
        let (kept, info) = page(issue.links.iter().collect(), |l| l.0.clone(), after, first);
        let nodes: Vec<Value> = kept
            .iter()
            .map(|(id, url, _)| json!({"id": id, "url": url}))
            .collect();
        json!({"nodes": nodes, "pageInfo": info})
    }

    /// One page of an issue's subscribers, as a `subscribers` connection.
    fn subscribers(&self, issue: &Issue, after: Option<&str>, first: usize) -> Value {
        let (kept, info) = page(
            issue.subscribers.iter().collect(),
            |u| u.to_string(),
            after,
            first,
        );
        let nodes: Vec<Value> = kept
            .iter()
            .map(|id| json!({"displayName": self.user(id).map_or("", |u| u.display_name.as_str())}))
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
        json!({
            "id": issue.id,
            "identifier": self.identifier(issue),
            "title": issue.title,
            "description": issue.description,
            "url": self.url(issue),
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

    /// An issue node, every field the `IssueList` and `IssueFull` documents select: each
    /// nested list cut at 20, its `pageInfo` saying whether more follow.
    fn full_issue_json(&self, issue: &Issue) -> Value {
        let mut node = self.issue_json(issue);
        let user = |id: &Option<String>| {
            id.as_deref()
                .and_then(|id| self.user(id))
                .map(|u| json!({"id": u.id, "displayName": u.display_name}))
        };
        let project = issue
            .project
            .as_deref()
            .and_then(|id| self.projects.iter().find(|p| p.id == id));
        let identifier = self.identifier(issue);
        for (key, value) in [
            (
                "branchName",
                json!(format!(
                    "{}-{}",
                    identifier.to_lowercase(),
                    slug(&issue.title)
                )),
            ),
            ("priority", json!(f64::from(issue.priority))),
            ("estimate", json!(issue.estimate)),
            ("dueDate", json!(issue.due)),
            ("createdAt", json!(stamp(issue.created))),
            ("updatedAt", json!(stamp(issue.updated))),
            ("completedAt", json!(issue.completed.map(stamp))),
            ("canceledAt", json!(issue.canceled.map(stamp))),
            (
                "archivedAt",
                json!(issue.archived.then(|| stamp(issue.updated))),
            ),
            ("trashed", json!(issue.trashed.is_some())),
            ("team", json!({"key": self.team_key(&issue.team)})),
            ("assignee", json!(user(&issue.assignee))),
            ("delegate", json!(user(&issue.delegate))),
            (
                "creator",
                json!(user(&issue.creator).map(|u| json!({"displayName": u["displayName"]}))),
            ),
            ("project", json!(project.map(|p| json!({"name": p.name})))),
            (
                "projectMilestone",
                json!(issue.milestone.as_ref().map(|name| json!({"name": name}))),
            ),
            (
                "cycle",
                json!(issue
                    .cycle
                    .as_deref()
                    .and_then(|id| self.cycles.iter().find(|c| c.id == id))
                    .map(|c| json!({"number": f64::from(c.number)}))),
            ),
            ("labels", self.label_names(issue, None, 20)),
            ("children", self.children_page(&issue.id, None, 20)),
            ("relations", self.out_relations(&issue.id, None, 20)),
            ("attachments", self.attachments(issue, None, 20)),
            ("subscribers", self.subscribers(issue, None, 20)),
        ] {
            node[key] = value;
        }
        node
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

    /// A comment node as `IssueThread` selects one, on the issue whose url is `issue_url`.
    fn thread_comment_json(&self, c: &Comment, issue_url: &str) -> Value {
        let mut node = self.comment_json(c);
        node["resolvedAt"] = json!(c.resolved.map(stamp));
        node["url"] = json!(format!("{issue_url}#comment-{}", &c.id[..8]));
        node["parent"] = json!(c.parent.as_ref().map(|id| json!({"id": id})));
        node
    }

    /// A state's or a label's issues, as the `Issues` filter the plugin sends selects them:
    /// open, in the team, kept by the gate when one is sent, `viewer` the user its `isMe`
    /// names, in manual order — ascending or descending.
    fn select(
        &self,
        filter: &Value,
        descending: bool,
        viewer: &str,
    ) -> Result<Vec<&Issue>, String> {
        let filter = filter.as_object().cloned().unwrap_or_default();
        let mut team = None;
        let mut state = None;
        let mut label = None;
        let mut gate = None;
        for (key, value) in &filter {
            match key.as_str() {
                "and" => gate = Some(json!({ "and": value })),
                "team" if value.as_object().map(|o| o.len()) == Some(1) => {
                    team = Some(
                        value["id"]["eq"]
                            .as_str()
                            .ok_or("filter.team is not id.eq")?,
                    );
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
        let Some(team) = team else {
            return Err("the fake answers no Issues filter without the team".into());
        };
        let mut issues: Vec<&Issue> = self
            .issues
            .iter()
            .filter(|i| !i.archived && i.team == team)
            .filter(|i| state.is_none_or(|s| i.state == s))
            .filter(|i| {
                label.is_none_or(|name| {
                    i.labels
                        .iter()
                        .any(|id| self.labels.iter().any(|l| l.id == *id && l.name == name))
                })
            })
            .collect();
        if let Some(gate) = gate {
            let mut kept = Vec::with_capacity(issues.len());
            for issue in issues {
                if self.keeps(&gate, issue, viewer)? {
                    kept.push(issue);
                }
            }
            issues = kept;
        }
        issues.sort_by(|a, b| a.sort_order.total_cmp(&b.sort_order));
        if descending {
            issues.reverse();
        }
        Ok(issues)
    }

    /// Whether `issue` passes the `IssueList` filter `filter`, `viewer` the user `isMe`
    /// names; a clause on a path the plugin's reads never send is refused.
    fn keeps(&self, filter: &Value, issue: &Issue, viewer: &str) -> Result<bool, String> {
        let Some(clauses) = filter.as_object() else {
            return Err(format!("an IssueList filter that is no object: {filter}"));
        };
        for (key, value) in clauses {
            let strings = |at: &Value| -> Vec<String> {
                at.as_array()
                    .map(|v| {
                        v.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default()
            };
            let any = |values: Vec<String>, value: &str| values.iter().any(|v| v == value);
            let held = match key.as_str() {
                "and" => {
                    let mut all = true;
                    for clause in value.as_array().ok_or("`and` is no list")? {
                        all &= self.keeps(clause, issue, viewer)?;
                    }
                    all
                }
                "or" => {
                    let mut one = false;
                    for clause in value.as_array().ok_or("`or` is no list")? {
                        one |= self.keeps(clause, issue, viewer)?;
                    }
                    one
                }
                "id" => any(strings(&value["in"]), &issue.id),
                "number" => value["eq"].as_f64() == Some(f64::from(issue.number)),
                "team" => {
                    let key = self.team_key(&issue.team);
                    match value["key"].as_object() {
                        Some(o) if o.contains_key("in") => any(strings(&o["in"]), &key),
                        Some(o) if o.contains_key("eq") => o["eq"].as_str() == Some(key.as_str()),
                        _ => return Err(format!("the fake answers no team filter {value}")),
                    }
                }
                "state" => {
                    let state = self.state(&issue.state).ok_or("an issue in no state")?;
                    match (value.get("name"), value.get("type")) {
                        (Some(names), None) => any(strings(&names["in"]), &state.name),
                        (None, Some(kinds)) => any(strings(&kinds["in"]), &state.kind),
                        _ => return Err(format!("the fake answers no state filter {value}")),
                    }
                }
                "priority" => value["in"]
                    .as_array()
                    .ok_or("priority is not `in`")?
                    .iter()
                    .any(|p| p.as_f64() == Some(f64::from(issue.priority))),
                "labels" => {
                    let names: Vec<String> = issue
                        .labels
                        .iter()
                        .filter_map(|id| self.labels.iter().find(|l| l.id == *id))
                        .map(|l| l.name.clone())
                        .collect();
                    if let Some(name) = value["some"]["name"]["eq"].as_str() {
                        names.iter().any(|n| n == name)
                    } else if let Some(nin) = value["every"]["name"]["nin"].as_array() {
                        let denied: Vec<&str> = nin.iter().filter_map(Value::as_str).collect();
                        names.iter().all(|n| !denied.contains(&n.as_str()))
                    } else {
                        return Err(format!("the fake answers no labels filter {value}"));
                    }
                }
                "assignee" | "delegate" => {
                    let holder = if key == "assignee" {
                        &issue.assignee
                    } else {
                        &issue.delegate
                    };
                    self.holds(value, holder.as_deref(), viewer)?
                }
                "project" => {
                    let name = issue
                        .project
                        .as_deref()
                        .and_then(|id| self.projects.iter().find(|p| p.id == id))
                        .map(|p| p.name.clone());
                    name.is_some_and(|n| any(strings(&value["name"]["in"]), &n))
                }
                "projectMilestone" => issue
                    .milestone
                    .as_ref()
                    .is_some_and(|n| any(strings(&value["name"]["in"]), n)),
                "cycle" => {
                    let number = issue
                        .cycle
                        .as_deref()
                        .and_then(|id| self.cycles.iter().find(|c| c.id == id))
                        .map(|c| f64::from(c.number));
                    let wanted = value["number"]["in"]
                        .as_array()
                        .ok_or("cycle is not number.in")?;
                    number.is_some_and(|n| wanted.iter().any(|w| w.as_f64() == Some(n)))
                }
                "parent" => match issue.parent.as_deref().and_then(|p| self.issue(p)) {
                    Some(parent) => self.keeps(value, parent, viewer)?,
                    None => false,
                },
                "hasBlockedByRelations" => {
                    let open = self.relations.iter().any(|r| {
                        r.kind == "blocks"
                            && r.target == issue.id
                            && self
                                .issue(&r.source)
                                .and_then(|b| self.state(&b.state))
                                .is_some_and(|s| !CLOSED_KINDS.contains(&s.kind.as_str()))
                    });
                    Some(open) == value["eq"].as_bool()
                }
                "title" | "description" => {
                    let text = if key == "title" {
                        Some(issue.title.as_str())
                    } else {
                        issue.description.as_deref()
                    };
                    let needle = value["containsIgnoreCase"]
                        .as_str()
                        .ok_or("text is not containsIgnoreCase")?
                        .to_lowercase();
                    text.is_some_and(|t| t.to_lowercase().contains(&needle))
                }
                "createdAt" | "updatedAt" | "completedAt" | "canceledAt" => {
                    let at = match key.as_str() {
                        "createdAt" => Some(issue.created),
                        "updatedAt" => Some(issue.updated),
                        "completedAt" => issue.completed,
                        _ => issue.canceled,
                    };
                    self.compares(value, at, |t| {
                        ms_of(t).ok_or(format!("{key}: {t:?} is no time"))
                    })?
                }
                "dueDate" => self.compares(value, issue.due.clone(), |t| {
                    if t.len() == 10 {
                        Ok(t.to_string())
                    } else {
                        Err(format!("dueDate: {t:?} is no date"))
                    }
                })?,
                other => return Err(format!("the fake answers no IssueList filter on `{other}`")),
            };
            if !held {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Whether `at` passes the comparison `value` writes, `gt` or `lt`, each time it names
    /// read by `read`; a `null` never does.
    fn compares<T: PartialOrd>(
        &self,
        value: &Value,
        at: Option<T>,
        read: impl Fn(&str) -> Result<T, String>,
    ) -> Result<bool, String> {
        let Some(at) = at else {
            return Ok(false);
        };
        let mut held = true;
        for (compare, bound) in value.as_object().ok_or("a comparison that is no object")? {
            let bound = read(bound.as_str().ok_or("a bound that is no string")?)?;
            held &= match compare.as_str() {
                "gt" => at > bound,
                "lt" => at < bound,
                other => return Err(format!("the fake answers no comparison `{other}`")),
            };
        }
        Ok(held)
    }

    /// Whether the user `holder` passes the user filter `value`: any of its `or`, each `isMe`,
    /// `null`, or a display name or an email.
    fn holds(&self, value: &Value, holder: Option<&str>, viewer: &str) -> Result<bool, String> {
        let user = holder.and_then(|id| self.user(id));
        let mut one = false;
        for clause in value["or"]
            .as_array()
            .ok_or("a user filter that is not `or`")?
        {
            one |= if clause["isMe"]["eq"] == true {
                holder == Some(viewer)
            } else if clause["null"] == true {
                holder.is_none()
            } else if let Some(name) = clause["displayName"]["eq"].as_str() {
                user.is_some_and(|u| u.display_name == name)
            } else if let Some(email) = clause["email"]["eq"].as_str() {
                user.is_some_and(|u| u.email == email)
            } else {
                return Err(format!("the fake answers no user filter {clause}"));
            };
        }
        Ok(one)
    }

    /// How `a` and `b` compare under the `IssueList` sort `sort`, key by key.
    fn order(&self, sort: &[Value], a: &Issue, b: &Issue) -> Result<Ordering, String> {
        /// One sort key's value of an issue: `None` sorts last.
        #[derive(PartialEq, PartialOrd)]
        enum Key {
            Number(f64),
            Text(String),
        }
        for element in sort {
            let (field, how) = element
                .as_object()
                .and_then(|o| o.iter().next())
                .ok_or("a sort element that is no one key")?;
            let descending = how["order"] == "Descending";
            let key = |i: &Issue| -> Result<Option<Key>, String> {
                Ok(match field.as_str() {
                    "manual" => Some(Key::Number(i.sort_order)),
                    "team" => Some(Key::Text(self.team_key(&i.team))),
                    "workflowState" => self.state(&i.state).map(|s| Key::Number(s.position)),
                    // Urgent first, low after medium, and no priority after every other.
                    "priority" => Some(Key::Number(match i.priority {
                        0 => 5.0,
                        p => f64::from(p),
                    })),
                    "assignee" => i
                        .assignee
                        .as_deref()
                        .and_then(|id| self.user(id))
                        .map(|u| Key::Text(u.display_name.clone())),
                    "project" => i
                        .project
                        .as_deref()
                        .and_then(|id| self.projects.iter().find(|p| p.id == id))
                        .map(|p| Key::Text(p.name.clone())),
                    "cycle" => i
                        .cycle
                        .as_deref()
                        .and_then(|id| self.cycles.iter().find(|c| c.id == id))
                        .map(|c| Key::Number(f64::from(c.number))),
                    "estimate" => i.estimate.map(Key::Number),
                    "title" => Some(Key::Text(i.title.clone())),
                    "dueDate" => i.due.clone().map(Key::Text),
                    "createdAt" => Some(Key::Number(i.created as f64)),
                    "updatedAt" => Some(Key::Number(i.updated as f64)),
                    "completedAt" => i.completed.map(|ms| Key::Number(ms as f64)),
                    other => return Err(format!("the fake sorts by no `{other}`")),
                })
            };
            let ordering = match (key(a)?, key(b)?) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(x), Some(y)) => {
                    let ordering = x.partial_cmp(&y).unwrap_or(Ordering::Equal);
                    if descending {
                        ordering.reverse()
                    } else {
                        ordering
                    }
                }
            };
            if ordering != Ordering::Equal {
                return Ok(ordering);
            }
        }
        Ok(Ordering::Equal)
    }

    /// The issues an `IssueList` request keeps, in the order it sorts them.
    fn list(&self, vars: &Value, viewer: &str) -> Result<Vec<&Issue>, String> {
        let archived = vars["includeArchived"] == true;
        let sort = vars["sort"].as_array().cloned().unwrap_or_default();
        let mut kept = Vec::new();
        for issue in &self.issues {
            if (archived || !issue.archived) && self.keeps(&vars["filter"], issue, viewer)? {
                kept.push(issue);
            }
        }
        let mut failed = None;
        kept.sort_by(|a, b| {
            self.order(&sort, a, b).unwrap_or_else(|e| {
                failed = Some(e);
                Ordering::Equal
            })
        });
        match failed {
            Some(e) => Err(e),
            None => Ok(kept),
        }
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

    /// A new label named `name` in `color`, the team's or, with no `team`, the workspace's;
    /// returns its id.
    fn add_label(&mut self, name: &str, color: &str, team: Option<String>) -> String {
        let id = self.mint();
        self.labels.push(Label {
            id: id.clone(),
            name: name.to_string(),
            color: color.to_string(),
            team,
            parent: None,
        });
        id
    }

    /// A new team; returns its id.
    fn add_team(&mut self, key: &str, name: &str) -> String {
        let id = self.mint();
        self.teams.push(Team {
            id: id.clone(),
            key: key.to_string(),
            name: name.to_string(),
            zero_estimates: false,
        });
        id
    }

    /// A new user; returns its id.
    fn add_user(&mut self, name: &str, display_name: &str, email: &str, app: bool) -> String {
        let id = self.mint();
        self.users.push(User {
            id: id.clone(),
            name: name.to_string(),
            display_name: display_name.to_string(),
            email: email.to_string(),
            app,
            active: true,
        });
        id
    }

    /// A new issue of team `team` in state `state`, at the bottom of every manual order;
    /// returns its id.
    fn add_issue(
        &mut self,
        team: String,
        state: String,
        title: &str,
        description: Option<String>,
        parent: Option<String>,
        sort_order: Option<f64>,
    ) -> String {
        let id = self.mint();
        let number = self.next_number;
        self.next_number += 1;
        let sort_order = sort_order.unwrap_or_else(|| {
            self.issues
                .iter()
                .map(|i| i.sort_order)
                .fold(-1000.0_f64, f64::max)
                + 1000.0
        });
        let now = now_ms();
        self.issues.push(Issue {
            id: id.clone(),
            team,
            number,
            title: title.to_string(),
            description,
            state,
            labels: Vec::new(),
            assignee: None,
            delegate: None,
            parent,
            sort_order,
            archived: false,
            trashed: None,
            priority: 0,
            estimate: None,
            due: None,
            creator: None,
            project: None,
            milestone: None,
            cycle: None,
            created: now,
            updated: now,
            completed: None,
            canceled: None,
            links: Vec::new(),
            subscribers: Vec::new(),
        });
        id
    }

    /// Change `issue` as an `IssueUpdateInput` says, or refuse it as Linear does, leaving
    /// the store as it was: each key it holds set, a `null` one cleared, one left out left
    /// as it is. A `teamId` of another team moves the issue first, so every other field is
    /// the new team's: it needs a state of that team beside it, takes the next number there,
    /// and loses what the new team lacks — the old team's labels, its cycle, and a project the
    /// new team is not in, with its milestone. A state, a cycle or a milestone of another
    /// team or project, and an estimate of 0 on a team that does not allow one, are refused.
    fn update(&mut self, issue: &mut Issue, input: &Value) -> Result<(), Refusal> {
        let refuse = |message: &str| Err(Refusal::Input(message.to_string()));
        if let Some(team) = input["teamId"].as_str() {
            if self.team(team).is_none() {
                return Err(Refusal::NotFound("Team"));
            }
            if team != issue.team {
                let of_team = input["stateId"]
                    .as_str()
                    .and_then(|state| self.state(state))
                    .is_some_and(|state| state.team == team);
                if !of_team {
                    return refuse("An issue moved to another team needs a state of that team");
                }
                self.move_into(issue, team);
            }
        }
        if let Some(state) = input["stateId"].as_str() {
            match self.state(state) {
                None => return Err(Refusal::NotFound("WorkflowState")),
                Some(st) if st.team != issue.team => {
                    return refuse("The workflow state is not one of the issue's team")
                }
                Some(_) => issue.state = state.to_string(),
            }
        }
        if let Some(order) = input["sortOrder"].as_f64() {
            issue.sort_order = order;
        }
        if let Some(title) = input["title"].as_str() {
            issue.title = title.to_string();
        }
        if let Some(text) = input["description"].as_str() {
            issue.description = (!text.is_empty()).then(|| text.to_string());
        }
        if let Some(priority) = input["priority"].as_u64() {
            if priority > 4 {
                return refuse("Priority must be 0 to 4");
            }
            issue.priority = priority as u8;
        }
        match input.get("estimate") {
            Some(Value::Null) => issue.estimate = None,
            Some(estimate) => {
                let estimate = estimate.as_f64().unwrap_or_default();
                let zero = self.team(&issue.team).is_some_and(|t| t.zero_estimates);
                if estimate == 0.0 && !zero {
                    return refuse("Invalid estimate: the team does not use zero estimates");
                }
                issue.estimate = Some(estimate);
            }
            None => {}
        }
        match input.get("dueDate") {
            Some(Value::Null) => issue.due = None,
            Some(due) => issue.due = due.as_str().map(str::to_string),
            None => {}
        }
        // A `null` takes the user off; a key left out leaves it be.
        for (key, field) in [
            ("assigneeId", &mut issue.assignee),
            ("delegateId", &mut issue.delegate),
        ] {
            match input.get(key) {
                Some(Value::Null) => *field = None,
                Some(user) => {
                    let user = user.as_str().unwrap_or_default();
                    if self.user(user).is_none() {
                        return Err(Refusal::NotFound("User"));
                    }
                    *field = Some(user.to_string());
                }
                None => {}
            }
        }
        match input.get("projectId") {
            Some(Value::Null) => {
                issue.project = None;
                issue.milestone = None;
            }
            Some(project) => {
                let project = project.as_str().unwrap_or_default().to_string();
                if !self.projects.iter().any(|p| p.id == project) {
                    return Err(Refusal::NotFound("Project"));
                }
                if issue.project.as_ref() != Some(&project) {
                    issue.milestone = None;
                }
                issue.project = Some(project);
            }
            None => {}
        }
        match input.get("projectMilestoneId") {
            Some(Value::Null) => issue.milestone = None,
            Some(milestone) => {
                let milestone = milestone.as_str().unwrap_or_default();
                let name = issue
                    .project
                    .as_deref()
                    .and_then(|id| self.projects.iter().find(|p| p.id == id))
                    .and_then(|p| p.milestones.iter().find(|m| m.id == milestone))
                    .map(|m| m.name.clone());
                match name {
                    Some(name) => issue.milestone = Some(name),
                    None => return refuse("The milestone is not one of the issue's project"),
                }
            }
            None => {}
        }
        match input.get("cycleId") {
            Some(Value::Null) => issue.cycle = None,
            Some(cycle) => {
                let cycle = cycle.as_str().unwrap_or_default();
                match self.cycles.iter().find(|c| c.id == cycle) {
                    None => return Err(Refusal::NotFound("Cycle")),
                    Some(c) if c.team != issue.team => {
                        return refuse("The cycle is not one of the issue's team")
                    }
                    Some(c) => issue.cycle = Some(c.id.clone()),
                }
            }
            None => {}
        }
        match input.get("parentId") {
            Some(Value::Null) => issue.parent = None,
            Some(parent) => {
                let parent = self
                    .issue_id(parent.as_str().unwrap_or_default())
                    .ok_or(Refusal::NotFound("Issue"))?;
                issue.parent = Some(parent);
            }
            None => {}
        }
        Ok(())
    }

    /// Move `issue` into the team `team`, by id, as Linear does: the next number, and nothing
    /// the new team lacks — the labels of another team, the cycle, and a project the team is
    /// not in, with its milestone.
    fn move_into(&mut self, issue: &mut Issue, team: &str) {
        issue.team = team.to_string();
        issue.number = self.next_number;
        self.next_number += 1;
        let labels = &self.labels;
        issue.labels.retain(|id| {
            labels
                .iter()
                .find(|l| l.id == *id)
                .is_some_and(|l| l.team.as_deref().is_none_or(|t| t == team))
        });
        issue.cycle = None;
        let in_project = issue
            .project
            .as_deref()
            .and_then(|id| self.projects.iter().find(|p| p.id == id))
            .is_some_and(|p| p.teams.iter().any(|t| t == team));
        if !in_project {
            issue.project = None;
            issue.milestone = None;
        }
    }

    /// The ids of `issue`'s sub-issues, theirs, and so on down, archived and trashed ones too.
    fn descendants(&self, issue: &str) -> Vec<String> {
        let mut found: Vec<String> = Vec::new();
        let mut next = vec![issue.to_string()];
        while let Some(parent) = next.pop() {
            for child in self
                .issues
                .iter()
                .filter(|i| i.parent.as_deref() == Some(parent.as_str()))
            {
                if !found.contains(&child.id) && child.id != issue {
                    found.push(child.id.clone());
                    next.push(child.id.clone());
                }
            }
        }
        found
    }

    /// Delete for good, as Linear does, every issue that has been in the trash 30 days.
    fn purge(&mut self) {
        let now = now_ms();
        self.issues.retain(|i| {
            i.trashed
                .is_none_or(|at| now.saturating_sub(at) < THIRTY_DAYS_MS)
        });
    }

    /// Who the `Authorization` header authenticates as, if anyone.
    fn viewer(&self, authorization: Option<&str>) -> Option<String> {
        match authorization? {
            key if key == self.api_key => Some(self.key_user.clone()),
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

/// The documents the plugin's and its skill's `operations.graphql` hold, by name, whitespace
/// collapsed.
///
/// # Panics
///
/// When the two files hold one name as two different documents.
fn documents() -> HashMap<String, String> {
    let mut out = documents_of(OPERATIONS);
    for (name, doc) in documents_of(SKILL_OPERATIONS) {
        if let Some(plugin) = out.get(&name) {
            assert_eq!(
                *plugin, doc,
                "the skill's {name} is not the document src/operations.graphql holds"
            );
        }
        out.insert(name, doc);
    }
    out
}

/// The documents one `operations.graphql` holds, by name, whitespace collapsed.
fn documents_of(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut current: Option<(String, String)> = None;
    for line in text.lines() {
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

/// The documents the skill's `operations.graphql` holds, by name, whitespace collapsed.
pub fn skill_documents() -> HashMap<String, String> {
    documents_of(SKILL_OPERATIONS)
}

/// The documents the plugin's `operations.graphql` holds, by name, whitespace collapsed.
pub fn plugin_documents() -> HashMap<String, String> {
    documents_of(OPERATIONS)
}

impl FakeLinear {
    /// The `acme` workspace: team `ENG`, its API key's user and its app's user, listening on
    /// `127.0.0.1:0`.
    pub fn start() -> Self {
        Self::start_as(ACME, API_KEY, CLIENT_ID, CLIENT_SECRET)
    }

    /// The `globex` workspace, another organization with credentials of its own, built as
    /// [`start`](Self::start) builds `acme`.
    pub fn start_globex() -> Self {
        Self::start_as(
            GLOBEX,
            GLOBEX_API_KEY,
            GLOBEX_CLIENT_ID,
            GLOBEX_CLIENT_SECRET,
        )
    }

    /// A workspace whose url key is `url_key`, reached with the API key `api_key` or the app
    /// `client_id` and `client_secret`: team `ENG`, the key's user and the app's user.
    pub fn start_as(url_key: &str, api_key: &str, client_id: &str, client_secret: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let base = format!("http://{}", listener.local_addr().expect("local addr"));
        let store = Arc::new(Mutex::new(Store {
            base: base.clone(),
            url_key: url_key.to_string(),
            api_key: api_key.to_string(),
            client_id: client_id.to_string(),
            client_secret: client_secret.to_string(),
            token_prefix: if url_key == ACME {
                "tok-".to_string()
            } else {
                format!("{url_key}-tok-")
            },
            next_number: 101,
            token_lifetime: 2_591_999,
            comments_page: 100,
            ..Store::default()
        }));
        {
            let mut s = lock(&store);
            s.org_id = s.mint();
            s.add_team(TEAM, "Engineering");
            s.key_user = s.add_user("afkd", "afkd", &format!("afkd@{url_key}.example"), false);
            s.app_user = s.add_user("afkd (app)", "afkd-app", "app@oauthapp.example", true);
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

    /// The workspace's organization's uuid, which a read's handle keys carry.
    pub fn org_id(&self) -> String {
        lock(&self.store).org_id.clone()
    }

    /// The workspace's url key.
    pub fn url_key(&self) -> String {
        lock(&self.store).url_key.clone()
    }

    /// The API key the workspace takes.
    pub fn api_key(&self) -> String {
        lock(&self.store).api_key.clone()
    }

    /// The client id and secret of the workspace's OAuth app.
    pub fn app_credentials(&self) -> (String, String) {
        let s = lock(&self.store);
        (s.client_id.clone(), s.client_secret.clone())
    }

    /// The `ENG` team's id.
    pub fn team_id(&self) -> String {
        let s = lock(&self.store);
        s.team_id_of(TEAM).expect("the ENG team")
    }

    /// Add a team; returns its id.
    pub fn team(&self, key: &str, name: &str) -> String {
        lock(&self.store).add_team(key, name)
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

    /// Let the team `team`, by key, estimate an issue at 0.
    pub fn allow_zero_estimates(&self, team: &str) {
        let mut s = lock(&self.store);
        if let Some(t) = s.teams.iter_mut().find(|t| t.key == team) {
            t.zero_estimates = true;
        }
    }

    /// Add a workflow state of the category `kind` to `ENG`, after every state seeded there
    /// before it; returns its id.
    pub fn state(&self, name: &str, kind: &str) -> String {
        self.team_state(TEAM, name, kind)
    }

    /// Add a workflow state of the category `kind` to the team `team`, by key, after every
    /// state seeded there before it; returns its id.
    pub fn team_state(&self, team: &str, name: &str, kind: &str) -> String {
        let mut s = lock(&self.store);
        let team = s.team_id_of(team).expect("a seeded team");
        let id = s.mint();
        let position = s.states.iter().filter(|st| st.team == team).count() as f64;
        s.states.push(State {
            id: id.clone(),
            team,
            name: name.to_string(),
            kind: kind.to_string(),
            position,
        });
        id
    }

    /// Put a state at `position` in its team's workflow.
    pub fn set_position(&self, state: &str, position: f64) {
        let mut s = lock(&self.store);
        if let Some(st) = s.states.iter_mut().find(|st| st.id == state) {
            st.position = position;
        }
    }

    /// Add a label of `ENG`'s, named `name` in the hex colour `color`; returns its id.
    pub fn team_label(&self, name: &str, color: &str) -> String {
        let mut s = lock(&self.store);
        let team = s.team_id_of(TEAM);
        s.add_label(name, color, team)
    }

    /// Add a label of the team `team`'s, by key, in the group `group` when one is given;
    /// returns its id.
    pub fn label_of(&self, team: &str, name: &str, group: Option<&str>) -> String {
        let mut s = lock(&self.store);
        let team = s.team_id_of(team);
        let id = s.add_label(name, "#bec2c8", team);
        if let Some(label) = s.labels.iter_mut().find(|l| l.id == id) {
            label.parent = group.map(str::to_string);
        }
        id
    }

    /// Add a workspace label, one no team holds, named `name` in the hex colour `color`;
    /// returns its id.
    pub fn workspace_label(&self, name: &str, color: &str) -> String {
        lock(&self.store).add_label(name, color, None)
    }

    /// The ids of the labels on an issue.
    pub fn label_ids(&self, issue: &str) -> Vec<String> {
        let s = lock(&self.store);
        let at = s.issue_at(issue).expect("a seeded issue");
        s.issues[at].labels.clone()
    }

    /// Put the label `label`, by id, on an issue.
    pub fn put_label(&self, issue: &str, label: &str) {
        self.edit(issue, |i| {
            if !i.labels.iter().any(|l| l == label) {
                i.labels.push(label.to_string());
            }
        });
    }

    /// Add a user; returns its id.
    pub fn user(&self, name: &str, display_name: &str, email: &str) -> String {
        lock(&self.store).add_user(name, display_name, email, false)
    }

    /// Add an app's user; returns its id.
    pub fn app(&self, name: &str, display_name: &str, email: &str) -> String {
        lock(&self.store).add_user(name, display_name, email, true)
    }

    /// Disable a user's account, which Linear then lists in no read.
    pub fn disable(&self, user: &str) {
        let mut s = lock(&self.store);
        if let Some(u) = s.users.iter_mut().find(|u| u.id == user) {
            u.active = false;
        }
    }

    /// Add an issue at the bottom of `ENG`'s state named `state`; returns its id.
    pub fn issue(&self, state: &str, title: &str, description: &str) -> String {
        self.team_issue(TEAM, state, title, description)
    }

    /// Add an issue at the bottom of the state named `state` of the team `team`, by key;
    /// returns its id.
    pub fn team_issue(&self, team: &str, state: &str, title: &str, description: &str) -> String {
        let mut s = lock(&self.store);
        let team = s.team_id_of(team).expect("a seeded team");
        let state = s.state_named(&team, state).expect("a seeded state");
        let description = (!description.is_empty()).then(|| description.to_string());
        s.add_issue(team, state, title, description, None, None)
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
        s.url(&s.issues[at])
    }

    /// An issue's git branch name, as the fake serves it.
    pub fn branch(&self, issue: &str) -> String {
        let s = lock(&self.store);
        let at = s.issue_at(issue).expect("a seeded issue");
        s.full_issue_json(&s.issues[at])["branchName"]
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

    /// Replace an issue's description, as a person does.
    pub fn describe(&self, issue: &str, text: &str) {
        self.edit(issue, |i| {
            i.description = (!text.is_empty()).then(|| text.to_string())
        });
    }

    /// Set an issue's priority, Linear's 0 to 4.
    pub fn set_priority(&self, issue: &str, priority: u8) {
        self.edit(issue, |i| i.priority = priority);
    }

    pub fn set_estimate(&self, issue: &str, estimate: f64) {
        self.edit(issue, |i| i.estimate = Some(estimate));
    }

    /// Set an issue's due date, `YYYY-MM-DD`.
    pub fn set_due(&self, issue: &str, due: &str) {
        self.edit(issue, |i| i.due = Some(due.to_string()));
    }

    /// Set who filed an issue, a user by id.
    pub fn set_creator(&self, issue: &str, user: &str) {
        self.edit(issue, |i| i.creator = Some(user.to_string()));
    }

    /// Put an issue in a project, by id, and in its milestone named `milestone` when one is
    /// given.
    pub fn set_project(&self, issue: &str, project: &str, milestone: Option<&str>) {
        self.edit(issue, |i| {
            i.project = Some(project.to_string());
            i.milestone = milestone.map(str::to_string);
        });
    }

    /// Put an issue in a cycle, by id.
    pub fn set_cycle(&self, issue: &str, cycle: &str) {
        self.edit(issue, |i| i.cycle = Some(cycle.to_string()));
    }

    /// Set when an issue was filed and last changed, in epoch milliseconds.
    pub fn set_times(&self, issue: &str, created: u64, updated: u64) {
        self.edit(issue, |i| {
            i.created = created;
            i.updated = updated;
        });
    }

    /// Mark an issue completed at `ms`.
    pub fn complete_at(&self, issue: &str, ms: u64) {
        self.edit(issue, |i| i.completed = Some(ms));
    }

    /// Mark an issue canceled at `ms`.
    pub fn cancel_at(&self, issue: &str, ms: u64) {
        self.edit(issue, |i| i.canceled = Some(ms));
    }

    /// Put an issue in the trash, which archives it too.
    pub fn trash(&self, issue: &str) {
        self.trash_at(issue, now_ms());
    }

    /// Put an issue in the trash at `ms`, in epoch milliseconds: 30 days after, Linear deletes
    /// it for good, and the fake answers it as it answers an issue there never was.
    pub fn trash_at(&self, issue: &str, ms: u64) {
        self.edit(issue, |i| {
            i.trashed = Some(ms);
            i.archived = true;
        });
    }

    /// Whether an issue is in the trash.
    pub fn trashed(&self, issue: &str) -> bool {
        let s = lock(&self.store);
        let at = s.issue_at(issue).expect("a seeded issue");
        s.issues[at].trashed.is_some()
    }

    /// Archive an issue, as a person does.
    pub fn archive(&self, issue: &str) {
        self.edit(issue, |i| i.archived = true);
    }

    /// Put a link, Linear's attachment, on an issue, titled with its url as Linear titles one
    /// given no title.
    pub fn link(&self, issue: &str, url: &str) {
        let id = lock(&self.store).mint();
        self.edit(issue, |i| {
            i.links.push((id, url.to_string(), url.to_string()));
        });
    }

    /// An issue's links, each one's url and title, in the order they were put on it.
    pub fn links_of(&self, issue: &str) -> Vec<(String, String)> {
        let s = lock(&self.store);
        let at = s.issue_at(issue).expect("a seeded issue");
        s.issues[at]
            .links
            .iter()
            .map(|(_, url, title)| (url.clone(), title.clone()))
            .collect()
    }

    /// Subscribe a user, by id, to an issue.
    pub fn subscribe(&self, issue: &str, user: &str) {
        self.edit(issue, |i| i.subscribers.push(user.to_string()));
    }

    /// The ids of the users subscribed to an issue, in the order they subscribed.
    pub fn subscribers_of(&self, issue: &str) -> Vec<String> {
        let s = lock(&self.store);
        let at = s.issue_at(issue).expect("a seeded issue");
        s.issues[at].subscribers.clone()
    }

    /// How many relations relate one issue to another, both ways and of every kind.
    pub fn relation_count(&self) -> usize {
        lock(&self.store).relations.len()
    }

    /// An issue's place in its state's manual order.
    pub fn sort_order_of(&self, issue: &str) -> f64 {
        let s = lock(&self.store);
        let at = s.issue_at(issue).expect("a seeded issue");
        s.issues[at].sort_order
    }

    /// Set an issue's place in its state's manual order.
    pub fn set_sort_order(&self, issue: &str, sort_order: f64) {
        self.edit(issue, |i| i.sort_order = sort_order);
    }

    /// Move an issue to the team `team`, by key, as a person does: it takes the next number
    /// there, so its identifier changes and its uuid does not, and sits in that team's state
    /// of the same name, else of the same category, else its first.
    pub fn move_team(&self, issue: &str, team: &str) {
        let mut s = lock(&self.store);
        let team = s.team_id_of(team).expect("a seeded team");
        let at = s.issue_at(issue).expect("a seeded issue");
        let old = s.state(&s.issues[at].state).cloned().expect("a state");
        let state = s
            .states
            .iter()
            .filter(|st| st.team == team)
            .find(|st| st.name == old.name)
            .or_else(|| {
                s.states
                    .iter()
                    .find(|st| st.team == team && st.kind == old.kind)
            })
            .or_else(|| s.states.iter().find(|st| st.team == team))
            .map(|st| st.id.clone())
            .expect("a state in the new team");
        let number = s.next_number;
        s.next_number += 1;
        let moved = &mut s.issues[at];
        moved.team = team;
        moved.number = number;
        moved.state = state;
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

    /// Move an issue to the bottom of its team's state named `state`, as a person does.
    pub fn move_to(&self, issue: &str, state: &str) {
        let mut s = lock(&self.store);
        let at = s.issue_at(issue).expect("a seeded issue");
        let team = s.issues[at].team.clone();
        let state = s.state_named(&team, state).expect("a seeded state");
        let bottom = s
            .issues
            .iter()
            .map(|i| i.sort_order)
            .fold(0.0_f64, f64::max);
        s.issues[at].state = state;
        s.issues[at].sort_order = bottom + 1000.0;
    }

    /// Put its team's label named `name` on an issue, creating it when need be.
    pub fn label(&self, issue: &str, name: &str) {
        let mut s = lock(&self.store);
        let at = s.issue_at(issue).expect("a seeded issue");
        let team = s.issues[at].team.clone();
        let label = match s
            .labels
            .iter()
            .find(|l| l.name == name && l.team.as_deref() == Some(&team))
        {
            Some(l) => l.id.clone(),
            None => s.add_label(name, "#bec2c8", Some(team)),
        };
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

    /// Add a project, its teams by key and its milestones with their sort orders, in the
    /// order given; returns its id.
    pub fn project(&self, name: &str, teams: &[&str], milestones: &[(&str, f64)]) -> String {
        let mut s = lock(&self.store);
        let id = s.mint();
        let teams = teams
            .iter()
            .map(|key| s.team_id_of(key).expect("a seeded team"))
            .collect();
        let milestones = milestones
            .iter()
            .map(|(name, sort_order)| Milestone {
                id: s.mint(),
                name: name.to_string(),
                sort_order: *sort_order,
            })
            .collect();
        s.projects.push(Project {
            id: id.clone(),
            name: name.to_string(),
            teams,
            milestones,
        });
        id
    }

    /// Add a cycle of the team `team`, by key; returns its id.
    pub fn cycle(
        &self,
        team: &str,
        number: u32,
        name: Option<&str>,
        starts: u64,
        ends: u64,
        active: bool,
    ) -> String {
        let mut s = lock(&self.store);
        let team = s.team_id_of(team).expect("a seeded team");
        let id = s.mint();
        s.cycles.push(Cycle {
            id: id.clone(),
            team,
            number,
            name: name.map(str::to_string),
            starts,
            ends,
            active,
        });
        id
    }

    /// Seed a comment by `author`, posted `ago` seconds before now; returns its id.
    pub fn comment(&self, issue: &str, author: Author, body: &str, ago: u64) -> String {
        self.comment_at(issue, author, body, now_ms() - ago * 1000, None)
    }

    /// Seed a comment by `author` posted at `created`, in epoch milliseconds, answering the
    /// comment `parent` when one is given; returns its id.
    pub fn comment_at(
        &self,
        issue: &str,
        author: Author,
        body: &str,
        created: u64,
        parent: Option<&str>,
    ) -> String {
        let mut s = lock(&self.store);
        let id = s.mint();
        s.comments.push(Comment {
            id: id.clone(),
            issue: issue.to_string(),
            author,
            body: body.to_string(),
            created,
            edited: None,
            parent: parent.map(str::to_string),
            resolved: None,
        });
        id
    }

    /// Mark a comment edited now, as a person editing it does.
    pub fn mark_edited(&self, comment: &str) {
        let mut s = lock(&self.store);
        if let Some(c) = s.comments.iter_mut().find(|c| c.id == comment) {
            c.edited = Some(now_ms());
        }
    }

    /// Mark a comment's thread resolved at `ms`.
    pub fn resolve(&self, comment: &str, ms: u64) {
        let mut s = lock(&self.store);
        if let Some(c) = s.comments.iter_mut().find(|c| c.id == comment) {
            c.resolved = Some(ms);
        }
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

    /// The id of the user an issue is delegated to, if anyone.
    pub fn delegate_of(&self, issue: &str) -> Option<String> {
        let s = lock(&self.store);
        let at = s.issue_at(issue).expect("a seeded issue");
        s.issues[at].delegate.clone()
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

    /// The ids of the issues in `ENG`'s state named `state`, in manual order.
    pub fn in_state(&self, state: &str) -> Vec<String> {
        self.in_team_state(TEAM, state)
    }

    /// The ids of the issues in the state named `state` of the team `team`, by key, in manual
    /// order.
    pub fn in_team_state(&self, team: &str, state: &str) -> Vec<String> {
        let s = lock(&self.store);
        let Some(team) = s.team_id_of(team) else {
            return Vec::new();
        };
        let Some(state) = s.state_named(&team, state) else {
            return Vec::new();
        };
        let mut issues: Vec<&Issue> = s.issues.iter().filter(|i| i.state == state).collect();
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
        let key = lock(&self.store).api_key.clone();
        write!(
            stream,
            "POST /graphql HTTP/1.1\r\nHost: {addr}\r\nAuthorization: {key}\r\n\
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

/// The token endpoint: a token for the workspace's app's credentials, with the grant and the
/// scope.
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
    if field("client_id") != Some(s.client_id.as_str())
        || field("client_secret") != Some(s.client_secret.as_str())
    {
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
    let value = format!("lin_oauth_{}{}", s.token_prefix, s.token_fetches);
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
        Err(Refusal::Input(message)) => (
            200,
            Vec::new(),
            error(&message, "INVALID_INPUT", "invalid input"),
        ),
        Err(Refusal::Invalid(sentence)) => invalid(s, sentence),
        Err(Refusal::Argument(constraint)) => {
            s.invalid.push(constraint.clone());
            (
                400,
                Vec::new(),
                json!({"errors": [{"message": "Argument Validation Error", "extensions": {
                    "code": "INVALID_INPUT", "type": "invalid input",
                    "validationErrors": [{"constraints": {"arrayMaxSize": constraint}}],
                }}]}),
            )
        }
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
    /// Linear refuses the input as it refuses a person's: a value the issue cannot take.
    Input(String),
    /// It asks for what the plugin never means.
    Invalid(String),
    /// An argument breaks a constraint Linear holds it to, the one named.
    Argument(String),
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

/// The user node `Me` and `UserList` select.
fn user_json(u: &User) -> Value {
    json!({"id": u.id, "name": u.name, "displayName": u.display_name, "email": u.email,
           "app": u.app})
}

fn dispatch(s: &mut Store, name: &str, vars: &Value, viewer: &str) -> Result<Value, Refusal> {
    if vars["sort"].as_array().is_some_and(|sort| sort.len() > 3) {
        return Err(Refusal::Argument(
            "sort must contain no more than 3 elements".into(),
        ));
    }
    let var = |key: &str| vars[key].as_str().unwrap_or_default().to_string();
    let after = vars["after"].as_str();
    s.purge();
    match name {
        "Viewer" => Ok(json!({"viewer": {"id": viewer}})),
        "Organization" => Ok(json!({"organization": {"id": s.org_id, "urlKey": s.url_key}})),
        "Me" => {
            let me = s.user(viewer).ok_or(Refusal::NotFound("User"))?;
            Ok(json!({"viewer": user_json(me)}))
        }
        "TeamByKey" => {
            let key = vars["filter"]["key"]["eq"].as_str().unwrap_or_default();
            let nodes: Vec<Value> = s
                .teams
                .iter()
                .filter(|t| t.key == key)
                .map(|t| json!({"id": t.id, "key": t.key}))
                .collect();
            Ok(json!({"teams": {"nodes": nodes}}))
        }
        "TeamList" => {
            let (kept, info) = page(s.teams.iter().collect(), |t| t.id.clone(), after, 100);
            let nodes: Vec<Value> = kept
                .iter()
                .map(|t| json!({"id": t.id, "key": t.key, "name": t.name}))
                .collect();
            Ok(json!({"teams": {"nodes": nodes, "pageInfo": info}}))
        }
        "UserList" => {
            let active: Vec<&User> = s.users.iter().filter(|u| u.active).collect();
            let (kept, info) = page(active, |u| u.id.clone(), after, 100);
            let nodes: Vec<Value> = kept.iter().map(|u| user_json(u)).collect();
            Ok(json!({"users": {"nodes": nodes, "pageInfo": info}}))
        }
        "States" => {
            let team = var("teamId");
            if s.team(&team).is_none() {
                return Err(Refusal::NotFound("Team"));
            }
            let nodes: Vec<Value> = s
                .states
                .iter()
                .filter(|st| st.team == team)
                .map(|st| s.state_json(&st.id))
                .collect();
            Ok(json!({"team": {"states": {"nodes": nodes}}}))
        }
        "Issues" => {
            let descending = vars["sort"][0]["manual"]["order"] == "Descending";
            let issues = s
                .select(&vars["filter"], descending, viewer)
                .map_err(Refusal::Invalid)?;
            let first = vars["first"].as_u64().unwrap_or(50) as usize;
            let (kept, info) = page(issues, |i| i.id.clone(), after, first);
            let nodes: Vec<Value> = kept.iter().map(|i| s.issue_json(i)).collect();
            Ok(json!({"issues": {"nodes": nodes, "pageInfo": info}}))
        }
        "IssueList" => {
            let issues = s.list(vars, viewer).map_err(Refusal::Invalid)?;
            let first = vars["first"].as_u64().unwrap_or(50) as usize;
            let (kept, info) = page(issues, |i| i.id.clone(), after, first);
            let nodes: Vec<Value> = kept.iter().map(|i| s.full_issue_json(i)).collect();
            Ok(json!({"issues": {"nodes": nodes, "pageInfo": info}}))
        }
        "IssueFull" => {
            let at = s.issue_at(&var("id")).ok_or(Refusal::NotFound("Issue"))?;
            Ok(json!({"issue": s.full_issue_json(&s.issues[at])}))
        }
        "IssueRef" => {
            let at = s.issue_at(&var("id")).ok_or(Refusal::NotFound("Issue"))?;
            let issue = &s.issues[at];
            Ok(json!({"issue": {"id": issue.id, "identifier": s.identifier(issue)}}))
        }
        "IssuePlace" => {
            let at = s.issue_at(&var("id")).ok_or(Refusal::NotFound("Issue"))?;
            let issue = &s.issues[at];
            Ok(json!({"issue": {
                "identifier": s.identifier(issue),
                "sortOrder": issue.sort_order,
                "team": {"id": issue.team, "key": s.team_key(&issue.team)},
                "state": s.state_json(&issue.state),
                "project": issue.project.as_ref().map(|id| json!({"id": id})),
            }}))
        }
        "IssueRelations" => {
            let issue = s.issue_id(&var("id")).ok_or(Refusal::NotFound("Issue"))?;
            let connection = s.inverse_relations(&issue, after, 50);
            Ok(json!({"issue": {"inverseRelations": connection}}))
        }
        "IssueOutRelations" => {
            let issue = s.issue_id(&var("id")).ok_or(Refusal::NotFound("Issue"))?;
            Ok(json!({"issue": {"relations": s.out_relations(&issue, after, 50)}}))
        }
        "IssueLabelNames" => {
            let at = s.issue_at(&var("id")).ok_or(Refusal::NotFound("Issue"))?;
            Ok(json!({"issue": {"labels": s.label_names(&s.issues[at], after, 50)}}))
        }
        "IssueAttachments" => {
            let at = s.issue_at(&var("id")).ok_or(Refusal::NotFound("Issue"))?;
            Ok(json!({"issue": {"attachments": s.attachments(&s.issues[at], after, 50)}}))
        }
        "IssueSubscribers" => {
            let at = s.issue_at(&var("id")).ok_or(Refusal::NotFound("Issue"))?;
            Ok(json!({"issue": {"subscribers": s.subscribers(&s.issues[at], after, 50)}}))
        }
        "IssueDescription" => {
            let at = s.issue_at(&var("id")).ok_or(Refusal::NotFound("Issue"))?;
            Ok(json!({"issue": {"description": s.issues[at].description}}))
        }
        "IssueChildren" => {
            let issue = s.issue_id(&var("id")).ok_or(Refusal::NotFound("Issue"))?;
            Ok(json!({"issue": {"children": s.children_page(&issue, after, 50)}}))
        }
        "IssueComments" | "IssueThread" => {
            let at = s.issue_at(&var("id")).ok_or(Refusal::NotFound("Issue"))?;
            let issue = &s.issues[at];
            let mut thread: Vec<&Comment> =
                s.comments.iter().filter(|c| c.issue == issue.id).collect();
            thread.sort_by(|a, b| b.created.cmp(&a.created).then(b.id.cmp(&a.id)));
            let (kept, info) = page(thread, |c| c.id.clone(), after, s.comments_page);
            if name == "IssueComments" {
                let nodes: Vec<Value> = kept.iter().map(|c| s.comment_json(c)).collect();
                return Ok(json!({"issue": {"comments": {"nodes": nodes, "pageInfo": info}}}));
            }
            let url = s.url(issue);
            let nodes: Vec<Value> = kept
                .iter()
                .map(|c| s.thread_comment_json(c, &url))
                .collect();
            Ok(json!({"issue": {"identifier": s.identifier(issue),
                                "comments": {"nodes": nodes, "pageInfo": info}}}))
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
        "LabelList" => {
            let workspace = json!({"team": {"null": true}});
            let filter = &vars["filter"];
            let team = if *filter == workspace {
                None
            } else {
                let key = filter["or"][1]["team"]["key"]["eq"].as_str();
                let wanted = key.map(|k| json!({"or": [workspace, {"team": {"key": {"eq": k}}}]}));
                if wanted.as_ref() != Some(filter) {
                    return Err(Refusal::Invalid(format!(
                        "the fake answers LabelList for the workspace and one team only, \
                         not {filter}"
                    )));
                }
                key.and_then(|k| s.team_id_of(k))
            };
            let labels: Vec<&Label> = s
                .labels
                .iter()
                .filter(|l| l.team.is_none() || (team.is_some() && l.team == team))
                .collect();
            let (kept, info) = page(labels, |l| l.id.clone(), after, 100);
            let nodes: Vec<Value> = kept
                .iter()
                .map(|l| {
                    let group = l
                        .parent
                        .as_deref()
                        .and_then(|p| s.labels.iter().find(|g| g.id == p));
                    json!({"id": l.id, "name": l.name,
                           "team": l.team.as_deref().map(|t| json!({"key": s.team_key(t)})),
                           "parent": group.map(|g| json!({"name": g.name}))})
                })
                .collect();
            Ok(json!({"issueLabels": {"nodes": nodes, "pageInfo": info}}))
        }
        "ProjectList" => {
            let filter = &vars["filter"];
            let team = match filter["accessibleTeams"]["some"]["key"]["eq"].as_str() {
                Some(key) if filter.as_object().map(|o| o.len()) == Some(1) => {
                    Some(s.team_id_of(key).unwrap_or_default())
                }
                None if *filter == json!({}) => None,
                _ => {
                    return Err(Refusal::Invalid(format!(
                        "the fake answers ProjectList for every project or one team's, not {filter}"
                    )))
                }
            };
            let projects: Vec<&Project> = s
                .projects
                .iter()
                .filter(|p| team.as_ref().is_none_or(|t| p.teams.contains(t)))
                .collect();
            let (kept, info) = page(projects, |p| p.id.clone(), after, 25);
            let nodes: Vec<Value> = kept
                .iter()
                .map(|p| {
                    let mut node = json!({"id": p.id, "name": p.name,
                                          "url": format!("https://linear.app/{}/project/{}-{}",
                                                         s.url_key, slug(&p.name), &p.id[..8])});
                    node["teams"] = project_teams(s, p, None, 50);
                    node["projectMilestones"] = project_milestones(p, None, 50);
                    node
                })
                .collect();
            Ok(json!({"projects": {"nodes": nodes, "pageInfo": info}}))
        }
        "ProjectTeams" | "ProjectMilestones" | "MilestoneList" => {
            let id = var("id");
            let project = s
                .projects
                .iter()
                .find(|p| p.id == id)
                .ok_or(Refusal::NotFound("Project"))?;
            Ok(if name == "ProjectTeams" {
                json!({"project": {"teams": project_teams(s, project, after, 50)}})
            } else {
                json!({"project": {"projectMilestones": project_milestones(project, after, 50)}})
            })
        }
        "CycleList" => {
            let team = vars["filter"]["team"]["id"]["eq"]
                .as_str()
                .unwrap_or_default();
            let cycles: Vec<&Cycle> = s.cycles.iter().filter(|c| c.team == team).collect();
            let (kept, info) = page(cycles, |c| c.id.clone(), after, 100);
            let nodes: Vec<Value> = kept
                .iter()
                .map(|c| {
                    json!({"id": c.id, "number": f64::from(c.number), "name": c.name,
                           "startsAt": stamp(c.starts), "endsAt": stamp(c.ends),
                           "isActive": c.active, "team": {"key": s.team_key(&c.team)}})
                })
                .collect();
            Ok(json!({"cycles": {"nodes": nodes, "pageInfo": info}}))
        }
        "CommentCreate" | "CommentPost" => {
            let issue = s
                .issue_id(vars["input"]["issueId"].as_str().unwrap_or_default())
                .ok_or(Refusal::NotFound("Issue"))?;
            let parent = match vars["input"]["parentId"].as_str() {
                None => None,
                Some(id) => {
                    let parent = s
                        .comments
                        .iter()
                        .find(|c| c.id == id)
                        .ok_or(Refusal::NotFound("Comment"))?;
                    if parent.issue != issue || parent.parent.is_some() {
                        return Err(Refusal::Input(
                            "parent must be a first comment on the same issue".into(),
                        ));
                    }
                    Some(parent.id.clone())
                }
            };
            let body = vars["input"]["body"]
                .as_str()
                .unwrap_or_default()
                .to_string();
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
                        parent: None,
                        resolved: None,
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
                parent,
                resolved: None,
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
            if !s.comments.iter().any(|c| c.id == id) {
                return Err(Refusal::NotFound("Comment"));
            }
            // A first comment takes its replies with it.
            s.comments
                .retain(|c| c.id != id && c.parent.as_deref() != Some(id.as_str()));
            Ok(json!({"commentDelete": {"success": true}}))
        }
        "CommentOne" => {
            let id = var("id");
            let c = s
                .comments
                .iter()
                .find(|c| c.id == id)
                .ok_or(Refusal::NotFound("Comment"))?;
            let issue = s.issue(&c.issue).ok_or(Refusal::NotFound("Issue"))?;
            let mut node = s.thread_comment_json(c, &s.url(issue));
            node["issue"] = json!({"id": issue.id, "identifier": s.identifier(issue)});
            Ok(json!({ "comment": node }))
        }
        "CommentResolve" | "CommentUnresolve" => {
            let id = var("id");
            let resolving = vars["resolvingCommentId"].as_str();
            let c = s
                .comments
                .iter()
                .find(|c| c.id == id)
                .ok_or(Refusal::NotFound("Comment"))?;
            if c.parent.is_some() {
                return Err(Refusal::Input(
                    "a thread is resolved on its first comment".into(),
                ));
            }
            if let Some(reply) = resolving {
                if !s
                    .comments
                    .iter()
                    .any(|r| r.id == reply && r.parent.as_deref() == Some(id.as_str()))
                {
                    return Err(Refusal::Input(
                        "the resolving comment must be a reply in the thread".into(),
                    ));
                }
            }
            let resolved = (name == "CommentResolve").then(now_ms);
            if let Some(c) = s.comments.iter_mut().find(|c| c.id == id) {
                c.resolved = resolved;
            }
            let field = if resolved.is_some() {
                "commentResolve"
            } else {
                "commentUnresolve"
            };
            Ok(json!({ field: {"success": true} }))
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
            } else if labels.contains(&label) {
                labels.retain(|l| *l != label);
            } else {
                return Err(Refusal::Invalid("Label not on issue".into()));
            }
            let field = if name == "IssueAddLabel" {
                "issueAddLabel"
            } else {
                "issueRemoveLabel"
            };
            Ok(json!({ field: {"success": true} }))
        }
        "IssueLabelCreate" => {
            let id = s.add_label(
                vars["input"]["name"].as_str().unwrap_or_default(),
                "#bec2c8",
                vars["input"]["teamId"].as_str().map(str::to_string),
            );
            Ok(json!({"issueLabelCreate": {"success": true, "issueLabel": {"id": id}}}))
        }
        "IssueUpdate" | "IssueWrite" => {
            let at = s.issue_at(&var("id")).ok_or(Refusal::NotFound("Issue"))?;
            let mut issue = s.issues[at].clone();
            s.update(&mut issue, &vars["input"])?;
            issue.updated = now_ms();
            s.issues[at] = issue;
            Ok(json!({"issueUpdate": {"success": true}}))
        }
        "IssueCreate" => {
            let input = &vars["input"];
            let team = input["teamId"].as_str().unwrap_or_default().to_string();
            if s.team(&team).is_none() {
                return Err(Refusal::NotFound("Team"));
            }
            let parent = match input["parentId"].as_str() {
                Some(p) => Some(s.issue_id(p).ok_or(Refusal::NotFound("Issue"))?),
                None => None,
            };
            let state = match input["stateId"].as_str() {
                Some(state) if s.state(state).is_none() => {
                    return Err(Refusal::NotFound("WorkflowState"))
                }
                Some(state) => state.to_string(),
                None => s
                    .default_state(&team)
                    .ok_or(Refusal::NotFound("WorkflowState"))?,
            };
            let id = s.add_issue(
                team,
                state,
                input["title"].as_str().unwrap_or_default(),
                input["description"].as_str().map(str::to_string),
                parent,
                input["sortOrder"].as_f64(),
            );
            // Set the rest of the input as an update sets it, and file nothing it refuses.
            let mut issue = s.issues.pop().expect("the issue just filed");
            let mut rest = input.as_object().cloned().unwrap_or_default();
            for key in [
                "teamId",
                "stateId",
                "title",
                "description",
                "parentId",
                "sortOrder",
            ] {
                rest.remove(key);
            }
            let labels = rest.remove("labelIds").unwrap_or_default();
            s.update(&mut issue, &Value::Object(rest))?;
            for label in labels.as_array().into_iter().flatten() {
                let label = label.as_str().unwrap_or_default();
                let of = s
                    .labels
                    .iter()
                    .find(|l| l.id == label)
                    .ok_or(Refusal::NotFound("IssueLabel"))?;
                if of.team.as_ref().is_some_and(|team| *team != issue.team) {
                    return Err(Refusal::Input(
                        "The label is not one of the issue's team".into(),
                    ));
                }
                if !issue.labels.iter().any(|l| l == label) {
                    issue.labels.push(label.to_string());
                }
            }
            issue.creator = Some(viewer.to_string());
            s.issues.push(issue);
            let issue = s.issue(&id).expect("the issue just filed");
            let node = json!({
                "id": issue.id,
                "identifier": s.identifier(issue),
                "url": s.url(issue),
            });
            Ok(json!({"issueCreate": {"success": true, "issue": node}}))
        }
        "IssueArchive" | "IssueUnarchive" | "IssueDelete" => {
            // An issue's sub-issues go, and come back, with it.
            let issue = s.issue_id(&var("id")).ok_or(Refusal::NotFound("Issue"))?;
            let mut affected = s.descendants(&issue);
            affected.push(issue);
            let now = now_ms();
            for i in s.issues.iter_mut().filter(|i| affected.contains(&i.id)) {
                match name {
                    "IssueArchive" => i.archived = true,
                    "IssueDelete" => {
                        i.archived = true;
                        i.trashed = Some(now);
                    }
                    _ => {
                        i.archived = false;
                        i.trashed = None;
                    }
                }
            }
            let field = match name {
                "IssueArchive" => "issueArchive",
                "IssueDelete" => "issueDelete",
                _ => "issueUnarchive",
            };
            Ok(json!({ field: {"success": true} }))
        }
        "RelationsOut" | "RelationsIn" => {
            let issue = s.issue_id(&var("id")).ok_or(Refusal::NotFound("Issue"))?;
            let out = name == "RelationsOut";
            let relations: Vec<&Relation> = s
                .relations
                .iter()
                .filter(|r| {
                    if out {
                        r.source == issue
                    } else {
                        r.target == issue
                    }
                })
                .collect();
            let (kept, info) = page(relations, |r| r.id.clone(), after, 50);
            let nodes: Vec<Value> = kept
                .iter()
                .map(|r| {
                    if out {
                        json!({"id": r.id, "type": r.kind, "relatedIssue": {"id": r.target}})
                    } else {
                        json!({"id": r.id, "type": r.kind, "issue": {"id": r.source}})
                    }
                })
                .collect();
            let field = if out { "relations" } else { "inverseRelations" };
            Ok(json!({"issue": { field: {"nodes": nodes, "pageInfo": info} }}))
        }
        "RelationCreate" => {
            let input = &vars["input"];
            let id = |key: &str| input[key].as_str().and_then(|id| s.issue_id(id));
            let (source, target) = (
                id("issueId").ok_or(Refusal::NotFound("Issue"))?,
                id("relatedIssueId").ok_or(Refusal::NotFound("Issue"))?,
            );
            if source == target {
                return Err(Refusal::Input(
                    "An issue cannot be related to itself".into(),
                ));
            }
            let id = s.mint();
            s.relations.push(Relation {
                id,
                source,
                target,
                kind: input["type"].as_str().unwrap_or_default().to_string(),
            });
            Ok(json!({"issueRelationCreate": {"success": true}}))
        }
        "RelationDelete" => {
            let id = var("id");
            let at = s
                .relations
                .iter()
                .position(|r| r.id == id)
                .ok_or(Refusal::NotFound("IssueRelation"))?;
            s.relations.remove(at);
            Ok(json!({"issueRelationDelete": {"success": true}}))
        }
        "IssueLinks" => {
            let at = s.issue_at(&var("id")).ok_or(Refusal::NotFound("Issue"))?;
            Ok(json!({"issue": {"attachments": s.attachments(&s.issues[at], after, 50)}}))
        }
        "LinkUrl" => {
            // Linear links a url once on an issue: linking it again retitles that link.
            let at = s
                .issue_at(&var("issueId"))
                .ok_or(Refusal::NotFound("Issue"))?;
            let (url, title) = (var("url"), vars["title"].as_str().map(str::to_string));
            let id = s.mint();
            let links = &mut s.issues[at].links;
            match links.iter_mut().find(|(_, linked, _)| *linked == url) {
                Some(link) => {
                    if let Some(title) = title {
                        link.2 = title;
                    }
                }
                None => links.push((id, url.clone(), title.unwrap_or(url))),
            }
            Ok(json!({"attachmentLinkURL": {"success": true}}))
        }
        "LinkUpdate" | "LinkDelete" => {
            let id = var("id");
            let found = s.issues.iter().enumerate().find_map(|(at, i)| {
                let link = i.links.iter().position(|(link, _, _)| *link == id)?;
                Some((at, link))
            });
            let (at, link) = found.ok_or(Refusal::NotFound("Attachment"))?;
            if name == "LinkUpdate" {
                s.issues[at].links[link].2 = vars["input"]["title"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                return Ok(json!({"attachmentUpdate": {"success": true}}));
            }
            s.issues[at].links.remove(link);
            Ok(json!({"attachmentDelete": {"success": true}}))
        }
        "IssueSubscribe" | "IssueUnsubscribe" => {
            let at = s.issue_at(&var("id")).ok_or(Refusal::NotFound("Issue"))?;
            let user = var("userId");
            if s.user(&user).is_none() {
                return Err(Refusal::NotFound("User"));
            }
            let subscribers = &mut s.issues[at].subscribers;
            if name == "IssueSubscribe" {
                if !subscribers.contains(&user) {
                    subscribers.push(user);
                }
                return Ok(json!({"issueSubscribe": {"success": true}}));
            }
            subscribers.retain(|u| *u != user);
            Ok(json!({"issueUnsubscribe": {"success": true}}))
        }
        "TeamDefaultState" => {
            let team = var("id");
            if s.team(&team).is_none() {
                return Err(Refusal::NotFound("Team"));
            }
            let state = s.default_state(&team).map(|id| {
                let state = s.state(&id).expect("a state of the team");
                json!({"id": state.id, "name": state.name})
            });
            Ok(json!({"team": {"defaultIssueState": state}}))
        }
        "IssueLabelIds" => {
            let at = s.issue_at(&var("id")).ok_or(Refusal::NotFound("Issue"))?;
            let issue = &s.issues[at];
            let nodes: Vec<Value> = issue.labels.iter().map(|id| json!({"id": id})).collect();
            Ok(json!({"issue": {"identifier": s.identifier(issue), "labels": {"nodes": nodes}}}))
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

/// One page of a project's teams, as a `teams` connection.
fn project_teams(s: &Store, project: &Project, after: Option<&str>, first: usize) -> Value {
    let (kept, info) = page(
        project.teams.iter().collect(),
        |t| t.to_string(),
        after,
        first,
    );
    let nodes: Vec<Value> = kept.iter().map(|t| json!({"key": s.team_key(t)})).collect();
    json!({"nodes": nodes, "pageInfo": info})
}

/// One page of a project's milestones, as a `projectMilestones` connection, in the order they
/// were seeded: every field `ProjectList`, `ProjectMilestones` and `MilestoneList` select.
fn project_milestones(project: &Project, after: Option<&str>, first: usize) -> Value {
    let (kept, info) = page(
        project.milestones.iter().collect(),
        |m| m.id.clone(),
        after,
        first,
    );
    let nodes: Vec<Value> = kept
        .iter()
        .map(|m| json!({"id": m.id, "name": m.name, "sortOrder": m.sort_order}))
        .collect();
    json!({"nodes": nodes, "pageInfo": info})
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
