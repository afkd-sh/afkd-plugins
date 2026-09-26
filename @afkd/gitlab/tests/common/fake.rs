//! A stateful fake GitLab on a loopback socket: the `/api/v4` routes the plugin's client
//! calls, over users, issues and notes the test seeds and then reads back. std and
//! `serde_json` only, and nothing from the plugin crate, so another suite can
//! `#[path]`-include it.
//!
//! It models the GitLab behaviours the kind depends on: a project is addressed by its
//! path-with-namespace percent-encoded into one `:id` segment; the issue listing filters on
//! `state` and on **every** comma-listed label; an issue `PUT` replaces the whole assignee
//! set (`assignee_ids`), adds and removes labels by name (`add_labels`/`remove_labels`, a
//! label springing into being on first use) and closes on `state_event=close`; notes are
//! scoped to their issue, carry separate `created_at` and `updated_at` stamped from a clock
//! the test moves, and an edit moves only the second; a note that does not exist is a
//! `404`. Every request is recorded, and any route can be made to answer a status instead.

#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;

use serde_json::{json, Value};

/// The token every request must carry, as `PRIVATE-TOKEN: <TOKEN>`.
pub const TOKEN: &str = "glpat-7f3a-fake";

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
    pub body: String,
}

/// An issue, its assignees by username.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    pub title: String,
    pub body: String,
    pub state: String,
    pub labels: Vec<String>,
    pub assignees: Vec<String>,
}

/// A note on an issue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub id: u64,
    pub project: String,
    pub iid: u64,
    pub author: String,
    pub body: String,
    pub created: u64,
    pub updated: u64,
}

/// An issue as the fake keeps it: assignees by user id, as GitLab writes them.
struct Stored {
    title: String,
    body: String,
    state: String,
    labels: Vec<String>,
    assignees: Vec<u64>,
}

#[derive(Default)]
struct State {
    /// The token's own username.
    me: String,
    now: u64,
    next_id: u64,
    /// Every user the fake knows, id → username.
    users: BTreeMap<u64, String>,
    /// Keyed by `(project path, iid)`.
    issues: BTreeMap<(String, u64), Stored>,
    notes: Vec<Note>,
    /// A route name → the status it answers instead.
    faults: HashMap<String, u16>,
    seen: Vec<Seen>,
}

impl State {
    /// The id of `username`, registering a user for a name the fake has not seen.
    fn user_id(&mut self, username: &str) -> u64 {
        if let Some((id, _)) = self.users.iter().find(|(_, u)| *u == username) {
            return *id;
        }
        self.next_id += 1;
        let id = self.next_id;
        self.users.insert(id, username.to_string());
        id
    }

    fn user_json(&self, id: u64) -> Value {
        json!({ "id": id, "username": self.users.get(&id).cloned().unwrap_or_default() })
    }

    fn issue_json(&self, iid: u64, issue: &Stored) -> Value {
        json!({
            "iid": iid,
            "title": issue.title,
            "description": issue.body,
            "state": issue.state,
            "labels": issue.labels,
            "assignees": issue.assignees.iter().map(|id| self.user_json(*id)).collect::<Vec<_>>(),
        })
    }

    fn note_json(&self, note: &Note) -> Value {
        let id = self
            .users
            .iter()
            .find(|(_, u)| **u == note.author)
            .map_or(0, |(id, _)| *id);
        json!({
            "id": note.id,
            "body": note.body,
            "author": self.user_json(id),
            "created_at": stamp(note.created),
            "updated_at": stamp(note.updated),
            "system": false,
        })
    }
}

/// The fake. Dropping it leaves the listener thread parked on `accept`; the process ends
/// it.
#[derive(Clone)]
pub struct FakeGitlab {
    base: String,
    state: Arc<Mutex<State>>,
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl FakeGitlab {
    /// A fake whose token belongs to user `me_id` / `me`, listening on `127.0.0.1:0`.
    pub fn start(me_id: u64, me: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let base = format!("http://{}", listener.local_addr().expect("local addr"));
        let state = Arc::new(Mutex::new(State {
            me: me.to_string(),
            now: EPOCH,
            next_id: 1_000,
            users: BTreeMap::from([(me_id, me.to_string())]),
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

    /// Register user `id` as `username`.
    pub fn user(&self, id: u64, username: &str) {
        lock(&self.state).users.insert(id, username.to_string());
    }

    /// Seed an open issue; each assignee is a username, registered if it is new.
    pub fn issue(
        &self,
        project: &str,
        iid: u64,
        title: &str,
        body: &str,
        labels: &[&str],
        assignees: &[&str],
    ) {
        let mut s = lock(&self.state);
        let assignees = assignees.iter().map(|a| s.user_id(a)).collect();
        s.issues.insert(
            (project.to_string(), iid),
            Stored {
                title: title.to_string(),
                body: body.to_string(),
                state: "opened".to_string(),
                labels: labels.iter().map(|l| l.to_string()).collect(),
                assignees,
            },
        );
    }

    /// Close an issue, as a human does.
    pub fn close(&self, project: &str, iid: u64) {
        if let Some(issue) = lock(&self.state)
            .issues
            .get_mut(&(project.to_string(), iid))
        {
            issue.state = "closed".to_string();
        }
    }

    /// Seed a note by `author`, created `ago` seconds before now; returns its id.
    pub fn note(&self, project: &str, iid: u64, author: &str, body: &str, ago: u64) -> u64 {
        let mut s = lock(&self.state);
        s.user_id(author);
        let at = s.now - ago;
        push_note(&mut s, project, iid, author, body, at)
    }

    /// Seed a note with a fixed id.
    pub fn note_with_id(
        &self,
        id: u64,
        project: &str,
        iid: u64,
        author: &str,
        body: &str,
        ago: u64,
    ) {
        let mut s = lock(&self.state);
        s.user_id(author);
        let at = s.now - ago;
        s.notes.push(Note {
            id,
            project: project.to_string(),
            iid,
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

    /// An issue as it stands, its assignees by username.
    pub fn issue_state(&self, project: &str, iid: u64) -> Issue {
        let s = lock(&self.state);
        let issue = &s.issues[&(project.to_string(), iid)];
        Issue {
            title: issue.title.clone(),
            body: issue.body.clone(),
            state: issue.state.clone(),
            labels: issue.labels.clone(),
            assignees: issue
                .assignees
                .iter()
                .map(|id| s.users.get(id).cloned().unwrap_or_default())
                .collect(),
        }
    }

    pub fn notes(&self, project: &str, iid: u64) -> Vec<Note> {
        lock(&self.state)
            .notes
            .iter()
            .filter(|n| n.project == project && n.iid == iid)
            .cloned()
            .collect()
    }

    pub fn seen(&self) -> Vec<Seen> {
        lock(&self.state).seen.clone()
    }
}

fn push_note(s: &mut State, project: &str, iid: u64, author: &str, body: &str, at: u64) -> u64 {
    s.next_id += 1;
    let id = s.next_id;
    s.notes.push(Note {
        id,
        project: project.to_string(),
        iid,
        author: author.to_string(),
        body: body.to_string(),
        created: at,
        updated: at,
    });
    id
}

/// RFC 3339, UTC, to the millisecond — GitLab writes fractional seconds.
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
    let mut token = String::new();
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
                "private-token" => token = value.trim().to_string(),
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
            body: body.clone(),
        });
        if token != TOKEN {
            (401, json!({ "message": "401 Unauthorized" }))
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

/// Split a comma-listed labels value into its names, as GitLab does.
fn names(list: &str) -> Vec<String> {
    list.split(',')
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .collect()
}

/// Answer one authenticated request. The path is split on `/` **before** each segment is
/// decoded, so a project's `%2F` stays inside its one `:id` segment.
fn route(
    s: &mut State,
    method: &str,
    path: &str,
    query: &[(String, String)],
    body: &str,
) -> (u16, Value) {
    let Some(rest) = path.strip_prefix("/api/v4/") else {
        return (404, json!({ "message": "404 Not Found" }));
    };
    let segs: Vec<String> = rest.split('/').map(percent_decode).collect();
    let segs: Vec<&str> = segs.iter().map(String::as_str).collect();
    let payload: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let name = match (method, segs.as_slice()) {
        ("GET", ["user"]) => "current user",
        ("GET", ["projects", _, "issues"]) => "list issues",
        ("GET", ["projects", _, "issues", _]) => "get item",
        ("PUT", ["projects", _, "issues", _]) => {
            if payload.get("assignee_ids").is_some() {
                "set assignees"
            } else if payload.get("add_labels").is_some() {
                "add label"
            } else if payload.get("remove_labels").is_some() {
                "remove label"
            } else {
                "set state"
            }
        }
        ("GET", ["projects", _, "issues", _, "notes"]) => "list notes",
        ("POST", ["projects", _, "issues", _, "notes"]) => "post comment",
        ("PUT", ["projects", _, "issues", _, "notes", _]) => "edit comment",
        ("DELETE", ["projects", _, "issues", _, "notes", _]) => "delete comment",
        _ => return (404, json!({ "message": "404 Not Found" })),
    };
    if let Some(status) = s.faults.get(name) {
        return (*status, json!({ "message": "injected" }));
    }
    let project = segs.get(1).map_or(String::new(), |p| p.to_string());
    let number = |i: usize| segs.get(i).and_then(|n| n.parse::<u64>().ok()).unwrap_or(0);
    let param = |key: &str| query.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
    match name {
        "current user" => {
            let id = s.user_id(&s.me.clone());
            (200, s.user_json(id))
        }
        "list issues" => {
            let state = param("state").unwrap_or_else(|| "all".to_string());
            let wanted = names(&param("labels").unwrap_or_default());
            let list: Vec<Value> = s
                .issues
                .iter()
                .filter(|((p, _), i)| {
                    *p == project
                        && (state == "all" || i.state == state)
                        && wanted.iter().all(|w| i.labels.contains(w))
                })
                .map(|((_, iid), i)| s.issue_json(*iid, i))
                .collect();
            (200, Value::Array(list))
        }
        "get item" | "set assignees" | "add label" | "remove label" | "set state" => {
            let key = (project, number(3));
            let Some(issue) = s.issues.get_mut(&key) else {
                return (404, json!({ "message": "404 Issue Not Found" }));
            };
            match name {
                "set assignees" => {
                    issue.assignees = payload["assignee_ids"]
                        .as_array()
                        .map(|a| a.iter().filter_map(Value::as_u64).collect())
                        .unwrap_or_default();
                }
                "add label" => {
                    for label in names(payload["add_labels"].as_str().unwrap_or("")) {
                        if !issue.labels.contains(&label) {
                            issue.labels.push(label);
                        }
                    }
                }
                "remove label" => {
                    let gone = names(payload["remove_labels"].as_str().unwrap_or(""));
                    issue.labels.retain(|l| !gone.contains(l));
                }
                "set state" if payload["state_event"] == "close" => {
                    issue.state = "closed".to_string();
                }
                _ => {}
            }
            let issue = &s.issues[&key];
            (200, s.issue_json(key.1, issue))
        }
        "list notes" => {
            let iid = number(3);
            let list: Vec<Value> = s
                .notes
                .iter()
                .filter(|n| n.project == project && n.iid == iid)
                .map(|n| s.note_json(n))
                .collect();
            (200, Value::Array(list))
        }
        "post comment" => {
            let iid = number(3);
            if !s.issues.contains_key(&(project.clone(), iid)) {
                return (404, json!({ "message": "404 Issue Not Found" }));
            }
            let (me, now) = (s.me.clone(), s.now);
            let text = payload["body"].as_str().unwrap_or("").to_string();
            let id = push_note(s, &project, iid, &me, &text, now);
            let note = s
                .notes
                .iter()
                .find(|n| n.id == id)
                .cloned()
                .expect("pushed");
            (201, s.note_json(&note))
        }
        "edit comment" | "delete comment" => {
            let (iid, id) = (number(3), number(5));
            let Some(at) = s
                .notes
                .iter()
                .position(|n| n.project == project && n.iid == iid && n.id == id)
            else {
                return (404, json!({ "message": "404 Note Not Found" }));
            };
            if name == "delete comment" {
                s.notes.remove(at);
                return (204, Value::Null);
            }
            let now = s.now;
            let note = &mut s.notes[at];
            note.body = payload["body"].as_str().unwrap_or("").to_string();
            note.updated = now;
            let note = note.clone();
            (200, s.note_json(&note))
        }
        _ => (404, json!({})),
    }
}
