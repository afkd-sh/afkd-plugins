//! A stateful fake Gitea on a loopback socket: the `/api/v1` routes the plugin's client
//! calls, over repositories, issues, labels and comments the test seeds and then reads
//! back. std and `serde_json` only, and nothing from the plugin crate, so another suite
//! can `#[path]`-include it.
//!
//! It models the Gitea behaviours the kind depends on: a label add naming a label the
//! repository does not define is **dropped** and answered `200` with the issue's
//! resulting labels; adding an **exclusive** scoped label strips its scope siblings; an
//! assignee `PATCH` replaces the whole set; a comment carries separate `created_at` and
//! `updated_at`, stamped from a clock the test moves. Every request is recorded, and any
//! route can be made to answer a status instead.

#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;

use serde_json::{json, Value};

/// The token every request must carry, as `Authorization: token <TOKEN>`.
pub const TOKEN: &str = "PAT-7f3a-fake";

/// 2026-09-25T09:58:00Z, where the fake's clock starts.
pub const EPOCH: u64 = 1_790_330_280;

/// One request as the fake saw it.
#[derive(Debug, Clone)]
pub struct Seen {
    pub method: String,
    /// The path, without the query.
    pub path: String,
    /// The decoded query pairs.
    pub query: Vec<(String, String)>,
    pub body: String,
}

/// A label a repository defines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Label {
    pub id: u64,
    pub name: String,
    pub exclusive: bool,
}

/// An issue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    pub title: String,
    pub body: String,
    pub state: String,
    pub labels: Vec<String>,
    pub assignees: Vec<String>,
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

#[derive(Default)]
struct State {
    /// The token's own login.
    me: String,
    now: u64,
    next_id: u64,
    orgs: BTreeMap<String, Vec<String>>,
    /// Keyed by `(owner/name, number)`.
    issues: BTreeMap<(String, u64), Issue>,
    labels: BTreeMap<String, Vec<Label>>,
    comments: Vec<Comment>,
    /// A route name → the status it answers instead.
    faults: HashMap<String, u16>,
    seen: Vec<Seen>,
}

/// The fake. Dropping it leaves the listener thread parked on `accept`; the process
/// ends it.
#[derive(Clone)]
pub struct FakeGitea {
    base: String,
    state: Arc<Mutex<State>>,
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl FakeGitea {
    /// A fake whose token belongs to `me`, listening on `127.0.0.1:0`.
    pub fn start(me: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let base = format!("http://{}", listener.local_addr().expect("local addr"));
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
        Self { base, state }
    }

    /// The instance root, as a config's `base_url` writes it.
    pub fn base_url(&self) -> &str {
        &self.base
    }

    /// The fake's clock, in seconds since the epoch.
    pub fn now(&self) -> u64 {
        lock(&self.state).now
    }

    /// Move the clock on.
    pub fn advance(&self, secs: u64) {
        lock(&self.state).now += secs;
    }

    /// Put `repos` (`owner/name`) under `org`.
    pub fn org(&self, org: &str, repos: &[&str]) {
        lock(&self.state).orgs.insert(
            org.to_string(),
            repos.iter().map(|r| r.to_string()).collect(),
        );
    }

    /// Define `name` in `repo`.
    pub fn define_label(&self, repo: &str, name: &str, exclusive: bool) {
        define(&mut lock(&self.state), repo, name, exclusive);
    }

    /// Seed an open issue; its labels are defined in the repository as they would be.
    pub fn issue(
        &self,
        repo: &str,
        number: u64,
        title: &str,
        body: &str,
        labels: &[&str],
        assignees: &[&str],
    ) {
        let mut s = lock(&self.state);
        for label in labels {
            define(&mut s, repo, label, false);
        }
        s.issues.insert(
            (repo.to_string(), number),
            Issue {
                title: title.to_string(),
                body: body.to_string(),
                state: "open".to_string(),
                labels: labels.iter().map(|l| l.to_string()).collect(),
                assignees: assignees.iter().map(|a| a.to_string()).collect(),
            },
        );
    }

    /// Take `label` off an issue, as a human does.
    pub fn unlabel(&self, repo: &str, number: u64, label: &str) {
        if let Some(issue) = lock(&self.state)
            .issues
            .get_mut(&(repo.to_string(), number))
        {
            issue.labels.retain(|l| l != label);
        }
    }

    /// Close an issue.
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

    /// Seed a comment with a fixed id.
    pub fn comment_with_id(
        &self,
        id: u64,
        repo: &str,
        number: u64,
        author: &str,
        body: &str,
        ago: u64,
    ) {
        let mut s = lock(&self.state);
        let at = s.now - ago;
        s.comments.push(Comment {
            id,
            repo: repo.to_string(),
            number,
            author: author.to_string(),
            body: body.to_string(),
            created: at,
            updated: at,
        });
    }

    /// Make every request to `route` answer `status`.
    pub fn fail(&self, route: &str, status: u16) {
        lock(&self.state).faults.insert(route.to_string(), status);
    }

    /// Stop failing `route`.
    pub fn heal(&self, route: &str) {
        lock(&self.state).faults.remove(route);
    }

    pub fn issue_state(&self, repo: &str, number: u64) -> Issue {
        lock(&self.state).issues[&(repo.to_string(), number)].clone()
    }

    pub fn labels(&self, repo: &str) -> Vec<Label> {
        lock(&self.state)
            .labels
            .get(repo)
            .cloned()
            .unwrap_or_default()
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

fn define(s: &mut State, repo: &str, name: &str, exclusive: bool) {
    let labels = s.labels.entry(repo.to_string()).or_default();
    if labels.iter().any(|l| l.name == name) {
        return;
    }
    let id = labels.len() as u64 + 1;
    labels.push(Label {
        id,
        name: name.to_string(),
        exclusive,
    });
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

/// Gitea's `Label.ExclusiveScope()`: the text before the last `/` of an exclusive label.
fn scope(label: &Label) -> Option<&str> {
    if !label.exclusive {
        return None;
    }
    let cut = label.name.rfind('/')?;
    (cut > 0 && cut + 1 < label.name.len()).then(|| &label.name[..cut])
}

/// RFC 3339, UTC, to the second.
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

fn user(login: &str) -> Value {
    json!({ "login": login })
}

fn label_json(l: &Label) -> Value {
    json!({ "id": l.id, "name": l.name, "exclusive": l.exclusive, "color": "7057ff" })
}

fn issue_json(s: &State, repo: &str, number: u64, issue: &Issue) -> Value {
    let defined = s.labels.get(repo).cloned().unwrap_or_default();
    json!({
        "number": number,
        "title": issue.title,
        "body": issue.body,
        "state": issue.state,
        "labels": issue.labels.iter()
            .filter_map(|n| defined.iter().find(|l| &l.name == n))
            .map(label_json)
            .collect::<Vec<_>>(),
        "assignees": issue.assignees.iter().map(|a| user(a)).collect::<Vec<_>>(),
    })
}

fn comment_json(c: &Comment) -> Value {
    json!({
        "id": c.id,
        "body": c.body,
        "user": user(&c.author),
        "created_at": stamp(c.created),
        "updated_at": stamp(c.updated),
    })
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
    let path = percent_decode(&path);

    let (status, reply) = {
        let mut s = lock(state);
        s.seen.push(Seen {
            method: method.clone(),
            path: path.clone(),
            query: query.clone(),
            body: body.clone(),
        });
        if auth != format!("token {TOKEN}") {
            (401, json!({ "message": "token is required" }))
        } else {
            route(&mut s, &method, &path, &body)
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

/// Answer one authenticated request.
fn route(s: &mut State, method: &str, path: &str, body: &str) -> (u16, Value) {
    let Some(rest) = path.strip_prefix("/api/v1/") else {
        return (404, json!({}));
    };
    let segs: Vec<&str> = rest.split('/').collect();
    let payload: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let name = match (method, segs.as_slice()) {
        ("GET", ["user"]) => "current user",
        ("GET", ["orgs", _, "repos"]) => "list org repos",
        ("GET", ["repos", _, _, "issues"]) => "list issues",
        ("GET", ["repos", _, _, "issues", "comments", _]) => "get comment",
        ("DELETE", ["repos", _, _, "issues", "comments", _]) => "delete comment",
        ("PATCH", ["repos", _, _, "issues", "comments", _]) => "edit comment",
        ("GET", ["repos", _, _, "issues", _]) => "get issue",
        ("PATCH", ["repos", _, _, "issues", _]) => {
            if payload.get("state").is_some() {
                "set state"
            } else {
                "patch assignees"
            }
        }
        ("GET", ["repos", _, _, "labels"]) => "list labels",
        ("POST", ["repos", _, _, "labels"]) => "create label",
        ("POST", ["repos", _, _, "issues", _, "labels"]) => "add label",
        ("DELETE", ["repos", _, _, "issues", _, "labels", _]) => "remove label",
        ("GET", ["repos", _, _, "issues", _, "comments"]) => "list comments",
        ("POST", ["repos", _, _, "issues", _, "comments"]) => "post comment",
        _ => return (404, json!({ "message": "no such route" })),
    };
    if let Some(status) = s.faults.get(name) {
        return (*status, json!({ "message": "injected" }));
    }
    let repo = || format!("{}/{}", segs[1], segs[2]);
    let number = |i: usize| segs.get(i).and_then(|n| n.parse::<u64>().ok()).unwrap_or(0);
    match name {
        "current user" => (200, user(&s.me)),
        "list org repos" => {
            let repos = s.orgs.get(segs[1]).cloned().unwrap_or_default();
            (
                200,
                Value::Array(repos.iter().map(|r| json!({ "full_name": r })).collect()),
            )
        }
        "list issues" => {
            let repo = repo();
            let list: Vec<Value> = s
                .issues
                .iter()
                .filter(|((r, _), i)| *r == repo && i.state == "open")
                .map(|((r, n), i)| issue_json(s, r, *n, i))
                .collect();
            (200, Value::Array(list))
        }
        "get issue" | "patch assignees" | "set state" => {
            let (repo, n) = (repo(), number(4));
            if !s.issues.contains_key(&(repo.clone(), n)) {
                return (404, json!({ "message": "issue not found" }));
            }
            let issue = s.issues.get_mut(&(repo.clone(), n)).expect("checked");
            if name == "patch assignees" {
                issue.assignees = payload["assignees"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
            } else if name == "set state" {
                issue.state = payload["state"].as_str().unwrap_or("open").to_string();
            }
            let issue = issue.clone();
            (200, issue_json(s, &repo, n, &issue))
        }
        "list labels" => {
            let labels = s.labels.get(&repo()).cloned().unwrap_or_default();
            (200, Value::Array(labels.iter().map(label_json).collect()))
        }
        "create label" => {
            let name = payload["name"].as_str().unwrap_or("").to_string();
            let exclusive = payload["exclusive"].as_bool().unwrap_or(false);
            define(s, &repo(), &name, exclusive);
            let label = s.labels[&repo()]
                .iter()
                .find(|l| l.name == name)
                .cloned()
                .expect("defined");
            (201, label_json(&label))
        }
        "add label" => {
            let (repo, n) = (repo(), number(4));
            let defined = s.labels.get(&repo).cloned().unwrap_or_default();
            let Some(issue) = s.issues.get_mut(&(repo.clone(), n)) else {
                return (404, json!({ "message": "issue not found" }));
            };
            for wanted in payload["labels"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                // An undefined name is dropped under a 200, as Gitea does.
                let Some(label) = defined.iter().find(|l| l.name == wanted) else {
                    continue;
                };
                if let Some(sc) = scope(label) {
                    issue.labels.retain(|have| {
                        have == wanted
                            || defined.iter().find(|l| &l.name == have).and_then(scope) != Some(sc)
                    });
                }
                if !issue.labels.iter().any(|l| l == wanted) {
                    issue.labels.push(wanted.to_string());
                }
            }
            let labels: Vec<Value> = issue
                .labels
                .iter()
                .filter_map(|n| defined.iter().find(|l| &l.name == n))
                .map(label_json)
                .collect();
            (200, Value::Array(labels))
        }
        "remove label" => {
            let (repo, n, id) = (repo(), number(4), number(6));
            let name = s
                .labels
                .get(&repo)
                .and_then(|ls| ls.iter().find(|l| l.id == id))
                .map(|l| l.name.clone());
            match (s.issues.get_mut(&(repo, n)), name) {
                (Some(issue), Some(name)) => {
                    issue.labels.retain(|l| *l != name);
                    (204, Value::Null)
                }
                _ => (404, json!({ "message": "not found" })),
            }
        }
        "list comments" => {
            let (repo, n) = (repo(), number(4));
            let list: Vec<Value> = s
                .comments
                .iter()
                .filter(|c| c.repo == repo && c.number == n)
                .map(comment_json)
                .collect();
            (200, Value::Array(list))
        }
        "post comment" => {
            let (repo, n) = (repo(), number(4));
            if !s.issues.contains_key(&(repo.clone(), n)) {
                return (404, json!({ "message": "issue not found" }));
            }
            let (me, now) = (s.me.clone(), s.now);
            let id = push_comment(
                s,
                &repo,
                n,
                &me,
                payload["body"].as_str().unwrap_or(""),
                now,
            );
            let c = s
                .comments
                .iter()
                .find(|c| c.id == id)
                .cloned()
                .expect("pushed");
            (201, comment_json(&c))
        }
        "delete comment" | "edit comment" | "get comment" => {
            let (repo, id) = (repo(), number(5));
            let Some(at) = s.comments.iter().position(|c| c.repo == repo && c.id == id) else {
                return (404, json!({ "message": "comment not found" }));
            };
            match name {
                "delete comment" => {
                    s.comments.remove(at);
                    (204, Value::Null)
                }
                "edit comment" => {
                    let now = s.now;
                    let c = &mut s.comments[at];
                    c.body = payload["body"].as_str().unwrap_or("").to_string();
                    c.updated = now;
                    (200, comment_json(c))
                }
                _ => (200, comment_json(&s.comments[at])),
            }
        }
        _ => (404, json!({})),
    }
}
