//! A stateful fake GitHub Enterprise Server on a loopback socket: the `/api/v3` routes the
//! plugin's client calls, over users, issues, pull requests, comments and reviews the test
//! seeds and then reads back. std and `serde_json` only, and nothing from the plugin crate, so another suite
//! can `#[path]`-include it.
//!
//! It models the GitHub behaviours the kind depends on: every route hangs off `/api/v3`
//! (a non-`github.com` host is GHES); the token rides `Authorization: Bearer`; the issue
//! listing filters on `state` and on **every** comma-listed label, and folds pull requests
//! in, each marked by a `pull_request` member; a pull request is an issue — the status
//! writes, the comment thread and a close act on it by its number — and the pulls listing
//! filters on `state`, carrying each PR's author and head branch; a PR's reviews are listed
//! off its `…/pulls/{number}/reviews` path; assignees are added and removed by login
//! through the dedicated `…/assignees` endpoint (a `DELETE` carrying a body); a label is
//! added by name (springing into being on first use) and removed by name on its own path
//! segment, a label the issue does not carry being a `404`; a state `PATCH` closes;
//! comments carry separate `created_at` and `updated_at` stamped from a clock the test
//! moves, an edit moves only the second, and a comment is edited and deleted off a
//! **repo**-scoped path; a comment that does not exist is a `404`. Every request is
//! recorded, and any route can be made to answer a status instead.

#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;

use serde_json::{json, Value};

/// The token every request must carry, as `Authorization: Bearer <TOKEN>`.
pub const TOKEN: &str = "ghp_7f3aFake0000000000000000000000000000";

/// 2026-09-25T09:58:00Z, where the fake's clock starts.
pub const EPOCH: u64 = 1_790_330_280;

/// One request as the fake saw it.
#[derive(Debug, Clone)]
pub struct Seen {
    pub method: String,
    /// The path as it crossed the wire — still percent-encoded — without the query.
    pub path: String,
    /// The decoded query pairs.
    pub query: Vec<(String, String)>,
    /// The `Authorization` header, verbatim.
    pub auth: String,
    pub body: String,
}

/// An issue, as it stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    pub title: String,
    pub body: String,
    pub state: String,
    pub labels: Vec<String>,
    pub assignees: Vec<String>,
    /// Whether this "issue" is a pull request GitHub folds into the issue listing.
    pub pull: bool,
    /// A pull request's author (empty for a plain issue).
    pub author: String,
    /// A pull request's head branch (empty for a plain issue).
    pub head: String,
}

/// A comment on an issue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    pub id: u64,
    pub repo: String,
    pub number: u64,
    pub author: String,
    pub body: String,
    pub created: u64,
    pub updated: u64,
}

/// A submitted review on a pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Review {
    pub id: u64,
    pub repo: String,
    pub number: u64,
    pub author: String,
    pub submitted: u64,
}

#[derive(Default)]
struct State {
    /// The token's own login.
    me: String,
    now: u64,
    next_id: u64,
    /// Keyed by `(owner/name, number)`.
    issues: BTreeMap<(String, u64), Issue>,
    comments: Vec<Comment>,
    reviews: Vec<Review>,
    /// A route name → the status it answers instead.
    faults: HashMap<String, u16>,
    seen: Vec<Seen>,
}

impl State {
    fn issue_json(&self, number: u64, issue: &Issue) -> Value {
        let mut value = json!({
            "number": number,
            "title": issue.title,
            "body": issue.body,
            "state": issue.state,
            "labels": issue.labels.iter().map(|l| json!({"name": l})).collect::<Vec<_>>(),
            "assignees": issue.assignees.iter().map(|a| json!({"login": a})).collect::<Vec<_>>(),
        });
        if issue.pull {
            value["pull_request"] = json!({"url": format!("pulls/{number}")});
        }
        value
    }

    fn pull_json(&self, number: u64, issue: &Issue) -> Value {
        json!({
            "number": number,
            "title": issue.title,
            "body": issue.body,
            "state": issue.state,
            "user": {"login": issue.author},
            "head": {"ref": issue.head},
        })
    }

    fn review_json(&self, review: &Review) -> Value {
        json!({
            "id": review.id,
            "user": {"login": review.author},
            "body": "",
            "state": "COMMENTED",
            "submitted_at": stamp(review.submitted),
        })
    }

    fn comment_json(&self, comment: &Comment) -> Value {
        json!({
            "id": comment.id,
            "body": comment.body,
            "user": {"login": comment.author},
            "created_at": stamp(comment.created),
            "updated_at": stamp(comment.updated),
        })
    }
}

/// The fake. Dropping it leaves the listener thread parked on `accept`; the process ends
/// it.
#[derive(Clone)]
pub struct FakeGithub {
    host: String,
    state: Arc<Mutex<State>>,
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl FakeGithub {
    /// A fake whose token belongs to `me`, listening on `127.0.0.1:0`.
    pub fn start(me: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let host = format!("http://{}", listener.local_addr().expect("local addr"));
        let state = Arc::new(Mutex::new(State {
            me: me.to_string(),
            now: EPOCH,
            next_id: 1_000,
            ..State::default()
        }));
        let shared = Arc::clone(&state);
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let state = Arc::clone(&shared);
                thread::spawn(move || serve(stream, &state));
            }
        });
        Self { host, state }
    }

    /// The server's address, as a config's `host` writes it: a non-`github.com` host, so
    /// the client resolves it to the GHES `/api/v3` root.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The fake's clock, in seconds since the epoch.
    pub fn now(&self) -> u64 {
        lock(&self.state).now
    }

    /// Move the clock on.
    pub fn advance(&self, secs: u64) {
        lock(&self.state).now += secs;
    }

    /// Seed an open issue; each assignee is a login.
    pub fn issue(
        &self,
        repo: &str,
        number: u64,
        title: &str,
        body: &str,
        labels: &[&str],
        assignees: &[&str],
    ) {
        self.seed(
            repo,
            number,
            Issue {
                title: title.to_string(),
                body: body.to_string(),
                ..open(labels, assignees)
            },
        );
    }

    /// Seed an open pull request by `author` on branch `head`: the pulls listing carries
    /// it, and the issue listing folds it in beside the issues.
    pub fn pull(
        &self,
        repo: &str,
        number: u64,
        author: &str,
        head: &str,
        labels: &[&str],
        assignees: &[&str],
    ) {
        self.seed(
            repo,
            number,
            Issue {
                title: format!("Pull request from {head}"),
                pull: true,
                author: author.to_string(),
                head: head.to_string(),
                ..open(labels, assignees)
            },
        );
    }

    fn seed(&self, repo: &str, number: u64, issue: Issue) {
        lock(&self.state)
            .issues
            .insert((repo.to_string(), number), issue);
    }

    /// Close an issue (or merge or close a pull request), as a human does.
    pub fn close(&self, repo: &str, number: u64) {
        if let Some(issue) = lock(&self.state)
            .issues
            .get_mut(&(repo.to_string(), number))
        {
            issue.state = "closed".to_string();
        }
    }

    /// Seed a comment by `author`, created `ago` seconds before now; returns its id.
    pub fn comment(&self, repo: &str, number: u64, author: &str, body: &str, ago: u64) -> u64 {
        let mut s = lock(&self.state);
        let at = s.now - ago;
        push_comment(&mut s, repo, number, author, body, at)
    }

    /// Seed a comment with a fixed id, created `created_ago` and last edited `updated_ago`
    /// seconds before now.
    #[allow(clippy::too_many_arguments)]
    pub fn comment_with_id(
        &self,
        id: u64,
        repo: &str,
        number: u64,
        author: &str,
        body: &str,
        created_ago: u64,
        updated_ago: u64,
    ) {
        let mut s = lock(&self.state);
        let now = s.now;
        s.comments.push(Comment {
            id,
            repo: repo.to_string(),
            number,
            author: author.to_string(),
            body: body.to_string(),
            created: now - created_ago,
            updated: now - updated_ago,
        });
    }

    /// Seed a review on pull request `number` by `author`, submitted `ago` seconds before
    /// now; returns its id, drawn from the comments' sequence.
    pub fn review(&self, repo: &str, number: u64, author: &str, ago: u64) -> u64 {
        let mut s = lock(&self.state);
        s.next_id += 1;
        let (id, submitted) = (s.next_id, s.now - ago);
        s.reviews.push(Review {
            id,
            repo: repo.to_string(),
            number,
            author: author.to_string(),
            submitted,
        });
        id
    }

    /// Make every request to `route` answer `status`.
    pub fn fail(&self, route: &str, status: u16) {
        lock(&self.state).faults.insert(route.to_string(), status);
    }

    /// Stop failing `route`.
    pub fn heal(&self, route: &str) {
        lock(&self.state).faults.remove(route);
    }

    /// An issue as it stands.
    pub fn issue_state(&self, repo: &str, number: u64) -> Issue {
        lock(&self.state).issues[&(repo.to_string(), number)].clone()
    }

    pub fn comments(&self, repo: &str, number: u64) -> Vec<Comment> {
        lock(&self.state)
            .comments
            .iter()
            .filter(|c| c.repo == repo && c.number == number)
            .cloned()
            .collect()
    }

    pub fn seen(&self) -> Vec<Seen> {
        lock(&self.state).seen.clone()
    }
}

/// An open, untitled issue record carrying `labels` and `assignees`.
fn open(labels: &[&str], assignees: &[&str]) -> Issue {
    Issue {
        title: String::new(),
        body: String::new(),
        state: "open".to_string(),
        labels: labels.iter().map(|l| l.to_string()).collect(),
        assignees: assignees.iter().map(|a| a.to_string()).collect(),
        pull: false,
        author: String::new(),
        head: String::new(),
    }
}

fn push_comment(s: &mut State, repo: &str, number: u64, author: &str, body: &str, at: u64) -> u64 {
    s.next_id += 1;
    let id = s.next_id;
    s.comments.push(Comment {
        id,
        repo: repo.to_string(),
        number,
        author: author.to_string(),
        body: body.to_string(),
        created: at,
        updated: at,
    });
    id
}

/// RFC 3339, UTC, to the second — GitHub writes no fractional seconds.
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
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
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
    let mut auth = String::new();
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
            match name.trim().to_ascii_lowercase().as_str() {
                "content-length" => length = value.trim().parse().unwrap_or(0),
                "authorization" => auth = value.trim().to_string(),
                _ => {}
            }
        }
    }
    let mut body = vec![0u8; length];
    if reader.read_exact(&mut body).is_err() {
        return;
    }
    let body = String::from_utf8_lossy(&body).into_owned();
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

    let (status, reply) = {
        let mut s = lock(state);
        s.seen.push(Seen {
            method: method.clone(),
            path: path.clone(),
            query: query.clone(),
            auth: auth.clone(),
            body: body.clone(),
        });
        if auth != format!("Bearer {TOKEN}") {
            (401, json!({ "message": "Bad credentials" }))
        } else {
            route(&mut s, &method, &path, &query, &body)
        }
    };
    let text = if status == 204 {
        String::new()
    } else {
        reply.to_string()
    };
    let reason = match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        401 => "Unauthorized",
        404 => "Not Found",
        _ => "Status",
    };
    let mut stream = stream;
    let _ = write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{text}",
        text.len()
    );
    let _ = stream.flush();
}

/// Split a comma-listed labels value into its names, as GitHub does.
fn names(list: &str) -> Vec<String> {
    list.split(',')
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .collect()
}

fn not_found() -> (u16, Value) {
    (404, json!({ "message": "Not Found" }))
}

/// Answer one authenticated request. The path is split on `/` **before** each segment is
/// decoded, so a label name's `%2F` stays inside its one segment.
fn route(
    s: &mut State,
    method: &str,
    path: &str,
    query: &[(String, String)],
    body: &str,
) -> (u16, Value) {
    let Some(rest) = path.strip_prefix("/api/v3/") else {
        return not_found();
    };
    let segs: Vec<String> = rest.split('/').map(percent_decode).collect();
    let segs: Vec<&str> = segs.iter().map(String::as_str).collect();
    let payload: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    // The repo-scoped comment routes first: their fifth segment is the literal
    // `comments`, where an issue route's is a number.
    let name = match (method, segs.as_slice()) {
        ("GET", ["user"]) => "current user",
        ("PATCH", ["repos", _, _, "issues", "comments", _]) => "edit comment",
        ("DELETE", ["repos", _, _, "issues", "comments", _]) => "delete comment",
        ("GET", ["repos", _, _, "issues"]) => "list issues",
        ("GET", ["repos", _, _, "pulls"]) => "list pulls",
        ("GET", ["repos", _, _, "pulls", _, "reviews"]) => "list reviews",
        ("PATCH", ["repos", _, _, "issues", _]) => "set state",
        ("POST", ["repos", _, _, "issues", _, "assignees"]) => "add assignees",
        ("DELETE", ["repos", _, _, "issues", _, "assignees"]) => "remove assignees",
        ("POST", ["repos", _, _, "issues", _, "labels"]) => "add label",
        ("DELETE", ["repos", _, _, "issues", _, "labels", _]) => "remove label",
        ("GET", ["repos", _, _, "issues", _, "comments"]) => "list comments",
        ("POST", ["repos", _, _, "issues", _, "comments"]) => "post comment",
        _ => return not_found(),
    };
    if let Some(status) = s.faults.get(name) {
        return (*status, json!({ "message": "injected" }));
    }
    let repo = match (segs.get(1), segs.get(2)) {
        (Some(owner), Some(name)) => format!("{owner}/{name}"),
        _ => String::new(),
    };
    let number = |i: usize| segs.get(i).and_then(|n| n.parse::<u64>().ok()).unwrap_or(0);
    let param = |key: &str| query.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
    let logins = |key: &str| -> Vec<String> {
        payload[key]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };
    match name {
        "current user" => (200, json!({ "login": s.me, "id": 7 })),
        "list issues" => {
            let state = param("state").unwrap_or_else(|| "open".to_string());
            let wanted = names(&param("labels").unwrap_or_default());
            let list: Vec<Value> = s
                .issues
                .iter()
                .filter(|((r, _), i)| {
                    *r == repo
                        && (state == "all" || i.state == state)
                        && wanted.iter().all(|w| i.labels.contains(w))
                })
                .map(|((_, n), i)| s.issue_json(*n, i))
                .collect();
            (200, Value::Array(list))
        }
        "list pulls" => {
            let state = param("state").unwrap_or_else(|| "open".to_string());
            let list: Vec<Value> = s
                .issues
                .iter()
                .filter(|((r, _), i)| *r == repo && i.pull && (state == "all" || i.state == state))
                .map(|((_, n), i)| s.pull_json(*n, i))
                .collect();
            (200, Value::Array(list))
        }
        "list reviews" => {
            let n = number(4);
            if !s.issues.get(&(repo.clone(), n)).is_some_and(|i| i.pull) {
                return not_found();
            }
            let list: Vec<Value> = s
                .reviews
                .iter()
                .filter(|r| r.repo == repo && r.number == n)
                .map(|r| s.review_json(r))
                .collect();
            (200, Value::Array(list))
        }
        "set state" | "add assignees" | "remove assignees" | "add label" | "remove label" => {
            let key = (repo, number(4));
            let Some(issue) = s.issues.get_mut(&key) else {
                return not_found();
            };
            match name {
                "set state" => {
                    if let Some(state) = payload["state"].as_str() {
                        issue.state = state.to_string();
                    }
                }
                "add assignees" => {
                    for login in logins("assignees") {
                        if !issue.assignees.contains(&login) {
                            issue.assignees.push(login);
                        }
                    }
                }
                "remove assignees" => {
                    let gone = logins("assignees");
                    issue.assignees.retain(|a| !gone.contains(a));
                }
                "add label" => {
                    for label in logins("labels") {
                        if !issue.labels.contains(&label) {
                            issue.labels.push(label);
                        }
                    }
                    let labels = issue.labels.iter().map(|l| json!({"name": l})).collect();
                    return (200, Value::Array(labels));
                }
                "remove label" => {
                    let label = segs.get(6).copied().unwrap_or("");
                    let Some(at) = issue.labels.iter().position(|l| l == label) else {
                        return (404, json!({ "message": "Label does not exist" }));
                    };
                    issue.labels.remove(at);
                    let labels = issue.labels.iter().map(|l| json!({"name": l})).collect();
                    return (200, Value::Array(labels));
                }
                _ => {}
            }
            let status = if name == "add assignees" { 201 } else { 200 };
            let issue = &s.issues[&key];
            (status, s.issue_json(key.1, issue))
        }
        "list comments" => {
            let n = number(4);
            let list: Vec<Value> = s
                .comments
                .iter()
                .filter(|c| c.repo == repo && c.number == n)
                .map(|c| s.comment_json(c))
                .collect();
            (200, Value::Array(list))
        }
        "post comment" => {
            let n = number(4);
            if !s.issues.contains_key(&(repo.clone(), n)) {
                return not_found();
            }
            let (me, now) = (s.me.clone(), s.now);
            let text = payload["body"].as_str().unwrap_or("").to_string();
            let id = push_comment(s, &repo, n, &me, &text, now);
            let comment = s
                .comments
                .iter()
                .find(|c| c.id == id)
                .cloned()
                .expect("pushed");
            (201, s.comment_json(&comment))
        }
        "edit comment" | "delete comment" => {
            let id = number(5);
            let Some(at) = s.comments.iter().position(|c| c.repo == repo && c.id == id) else {
                return not_found();
            };
            if name == "delete comment" {
                s.comments.remove(at);
                return (204, Value::Null);
            }
            let now = s.now;
            let comment = &mut s.comments[at];
            comment.body = payload["body"].as_str().unwrap_or("").to_string();
            comment.updated = now;
            let comment = comment.clone();
            (200, s.comment_json(&comment))
        }
        _ => not_found(),
    }
}
