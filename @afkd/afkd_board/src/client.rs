//! The board's HTTP API, as the reads and the writes use it: a request under
//! `<base_url>/api/board` with the `afkd_board.Board`'s key as `Authorization: Bearer <key>`, its
//! JSON answer decoded, and a refusal read from the `{"error": …}` body the API answers every
//! one with (the site's README, "A board's API"). A write's body is JSON, and a write of a card,
//! its checklist or a comment names the run it is made in as `"run"` only when it has one.
//!
//! The agent is `@afkd/linear`'s cut down to one call: a child answers one call and ends, so
//! there is no pool to keep fresh. afkd ends the call when it misses its reply, so every
//! request's global timeout is clipped to what is left of [`CALL_BUDGET`], and once it is gone
//! no request is sent at all. A redirect never carries the `Authorization` header on (ureq's
//! `RedirectAuthHeaders::Never`, kept), and a failure is told without the URL or the key.

use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use ureq::typestate::WithBody;
use ureq::RequestBuilder;

use crate::board::Board;

/// How long one call may take, short of afkd's own 60 s deadline so a refusal still reaches
/// it in time.
pub(crate) const CALL_BUDGET: Duration = Duration::from_secs(45);

/// How long a request waits to establish a connection before giving up.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a request waits for the response (read/write) before giving up.
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// The most of an answer's body read: past afkd's 8 MiB for a call's reply, so a page that
/// does not fit is refused by [`fit_call`](crate::wire::fit_call) with how to read less,
/// rather than cut here.
const MAX_BODY: u64 = 32 * 1024 * 1024;

/// What `GET /api/board` answers: the board's id, its name, its lists in order and its
/// labels.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct BoardAnswer {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) lists: Vec<ListAnswer>,
    pub(crate) labels: Vec<LabelAnswer>,
}

/// A list as the API names it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct ListAnswer {
    pub(crate) id: String,
    pub(crate) name: String,
}

/// A label as the API names it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct LabelAnswer {
    pub(crate) name: String,
    pub(crate) colour: String,
}

/// A card as the API answers it. Its `checklist` is left unread: a handle's field is never a
/// struct, so `afkd_board.Card` has none.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct CardAnswer {
    pub(crate) id: String,
    pub(crate) number: i64,
    pub(crate) title: String,
    pub(crate) description: String,
    pub(crate) list: String,
    pub(crate) labels: Vec<String>,
    pub(crate) done: bool,
    pub(crate) blocked: bool,
    pub(crate) blocked_by: Vec<i64>,
    pub(crate) blocking: Vec<i64>,
    pub(crate) archived: bool,
    pub(crate) url: String,
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
}

/// A comment as the API answers it: `by` the person's name or the key's, `null` once the
/// person's account is deleted, and `by_key` whether a key of the board made it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct CommentAnswer {
    pub(crate) id: String,
    pub(crate) body: String,
    pub(crate) by: Option<String>,
    pub(crate) by_key: bool,
    pub(crate) run: Option<RunAnswer>,
    pub(crate) created_at: String,
}

/// The run a comment was made in, as the API names it. Only its id is read.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct RunAnswer {
    pub(crate) id: String,
}

/// A refusal's body, `{"error": "<what is wrong>"}`.
#[derive(Debug, Deserialize)]
struct Problem {
    error: String,
}

/// The methods the board's API is called with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Method {
    Get,
    Post,
    Patch,
    Delete,
}

/// Where a card or a list goes: at the `top` or the `bottom` of its list or the board, or right
/// `after` another, a card by its number or a list by its id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Place<A> {
    Top,
    Bottom,
    After(A),
}

impl<A: Serialize> Place<A> {
    /// Name the place in `body`, an object: its `at`, or its `after`.
    fn put(&self, body: &mut Value) {
        let Some(fields) = body.as_object_mut() else {
            return;
        };
        match self {
            Self::Top => fields.insert("at".to_string(), json!("top")),
            Self::Bottom => fields.insert("at".to_string(), json!("bottom")),
            Self::After(after) => fields.insert("after".to_string(), json!(after)),
        };
    }
}

/// What `POST /api/board/cards` makes a card of.
#[derive(Debug, Clone, Copy)]
pub(crate) struct NewCard<'a> {
    /// The list it goes in, by name.
    pub(crate) list: &'a str,
    pub(crate) title: &'a str,
    pub(crate) description: Option<&'a str>,
    pub(crate) labels: &'a [String],
    /// The text of each item, none ticked.
    pub(crate) checklist: &'a [String],
    pub(crate) place: Option<&'a Place<i64>>,
}

/// The board's API, reached with one `afkd_board.Board` for one call.
pub(crate) struct BoardClient {
    agent: ureq::Agent,
    /// `<base_url>/api/board`, with no `/` after it.
    root: String,
    /// The host the board is served at, which a transport failure names.
    host: String,
    authorization: String,
    deadline: Instant,
}

impl std::fmt::Debug for BoardClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoardClient")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

impl BoardClient {
    /// A client of the board `board` reaches, for a call answered by `deadline`.
    pub(crate) fn new(board: &Board, deadline: Instant) -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .timeout_send_request(Some(READ_TIMEOUT))
            .timeout_recv_response(Some(READ_TIMEOUT))
            .timeout_recv_body(Some(READ_TIMEOUT))
            .timeout_global(Some(CONNECT_TIMEOUT + READ_TIMEOUT))
            .http_status_as_error(false)
            .build()
            .new_agent();
        let base = board.base_url.trim_end_matches('/');
        Self {
            agent,
            root: format!("{base}/api/board"),
            host: host_of(base).to_string(),
            authorization: format!("Bearer {}", board.key),
            deadline,
        }
    }

    /// `GET /api/board`: the board, its id, its lists and its labels.
    pub(crate) fn board(&self, action: &str) -> Result<BoardAnswer, String> {
        self.get(action, "", &[])
    }

    /// `GET /api/board/cards` with the query `query`: the cards it keeps, in board order.
    pub(crate) fn cards(
        &self,
        action: &str,
        query: &[(&str, String)],
    ) -> Result<Vec<CardAnswer>, String> {
        self.get(action, "/cards", query)
    }

    /// `GET /api/board/cards/<number>`: one card, archived or not.
    pub(crate) fn card(&self, action: &str, number: i64) -> Result<CardAnswer, String> {
        self.get(action, &format!("/cards/{number}"), &[])
    }

    /// `GET /api/board/cards/<number>/comments`: its comments, the oldest first.
    pub(crate) fn comments(&self, action: &str, number: i64) -> Result<Vec<CommentAnswer>, String> {
        self.get(action, &format!("/cards/{number}/comments"), &[])
    }

    /// `POST /api/board/lists`: a list named `name` at `place`, the bottom when it has none.
    pub(crate) fn create_list(
        &self,
        action: &str,
        name: &str,
        place: Option<&Place<String>>,
    ) -> Result<ListAnswer, String> {
        let mut body = json!({ "name": name });
        if let Some(place) = place {
            place.put(&mut body);
        }
        self.made(action, Method::Post, "/lists", Some(&body))
    }

    /// `PATCH /api/board/lists/<id>`: the list `id` renamed to `name`, moved to `place`, or
    /// both.
    pub(crate) fn update_list(
        &self,
        action: &str,
        id: &str,
        name: Option<&str>,
        place: Option<&Place<String>>,
    ) -> Result<ListAnswer, String> {
        let mut body = json!({});
        if let (Some(name), Some(fields)) = (name, body.as_object_mut()) {
            fields.insert("name".to_string(), json!(name));
        }
        if let Some(place) = place {
            place.put(&mut body);
        }
        let path = format!("/lists/{}", segment(id));
        self.made(action, Method::Patch, &path, Some(&body))
    }

    /// `DELETE /api/board/lists/<id>`: the list `id` with its archived cards.
    pub(crate) fn delete_list(&self, action: &str, id: &str) -> Result<(), String> {
        let path = format!("/lists/{}", segment(id));
        self.send::<Value>(action, Method::Delete, &path, None)
            .map(|_| ())
    }

    /// `POST /api/board/cards`: a card made of `card`, in `run` when it has one.
    pub(crate) fn create_card(
        &self,
        action: &str,
        card: &NewCard<'_>,
        run: Option<&Value>,
    ) -> Result<CardAnswer, String> {
        let mut body = json!({ "list": card.list, "title": card.title });
        if let Some(fields) = body.as_object_mut() {
            if let Some(description) = card.description {
                fields.insert("description".to_string(), json!(description));
            }
            if !card.labels.is_empty() {
                fields.insert("labels".to_string(), json!(card.labels));
            }
            if !card.checklist.is_empty() {
                fields.insert("checklist".to_string(), json!(card.checklist));
            }
        }
        if let Some(place) = card.place {
            place.put(&mut body);
        }
        self.made(action, Method::Post, "/cards", Some(&with_run(body, run)))
    }

    /// `PATCH /api/board/cards/<number>`: its title, its description or both.
    pub(crate) fn update_card(
        &self,
        action: &str,
        number: i64,
        title: Option<&str>,
        description: Option<&str>,
        run: Option<&Value>,
    ) -> Result<CardAnswer, String> {
        let mut body = json!({});
        if let Some(fields) = body.as_object_mut() {
            if let Some(title) = title {
                fields.insert("title".to_string(), json!(title));
            }
            if let Some(description) = description {
                fields.insert("description".to_string(), json!(description));
            }
        }
        self.patch_card(action, number, with_run(body, run))
    }

    /// `PATCH /api/board/cards/<number>`: it moved to the list `list`, at `place`.
    pub(crate) fn move_card(
        &self,
        action: &str,
        number: i64,
        list: &str,
        place: &Place<i64>,
        run: Option<&Value>,
    ) -> Result<CardAnswer, String> {
        let mut body = json!({ "list": list });
        place.put(&mut body);
        self.patch_card(action, number, with_run(body, run))
    }

    /// `PATCH /api/board/cards/<number>`: it marked done, or not.
    pub(crate) fn mark_done(
        &self,
        action: &str,
        number: i64,
        done: bool,
        run: Option<&Value>,
    ) -> Result<CardAnswer, String> {
        self.patch_card(action, number, with_run(json!({ "done": done }), run))
    }

    /// `POST /api/board/cards/<number>/labels/<name>`: the label put on, made in the next free
    /// colour when the board has none of that name.
    pub(crate) fn add_label(
        &self,
        action: &str,
        number: i64,
        name: &str,
        run: Option<&Value>,
    ) -> Result<CardAnswer, String> {
        let path = format!("/cards/{number}/labels/{}", segment(name));
        self.made(action, Method::Post, &path, Some(&with_run(json!({}), run)))
    }

    /// `DELETE /api/board/cards/<number>/labels/<name>`: the label taken off; a label the
    /// board lacks is `400`.
    pub(crate) fn remove_label(
        &self,
        action: &str,
        number: i64,
        name: &str,
        run: Option<&Value>,
    ) -> Result<CardAnswer, String> {
        let path = format!("/cards/{number}/labels/{}", segment(name));
        self.made(
            action,
            Method::Delete,
            &path,
            Some(&with_run(json!({}), run)),
        )
    }

    /// `POST /api/board/cards/<number>/checklist`: an item added to the end of its checklist.
    pub(crate) fn add_item(
        &self,
        action: &str,
        number: i64,
        text: &str,
        run: Option<&Value>,
    ) -> Result<CardAnswer, String> {
        let path = format!("/cards/{number}/checklist");
        let body = with_run(json!({ "text": text }), run);
        self.made(action, Method::Post, &path, Some(&body))
    }

    /// `PATCH /api/board/cards/<number>/checklist/<index>`: the item ticked or unticked.
    pub(crate) fn check_item(
        &self,
        action: &str,
        number: i64,
        index: i64,
        done: bool,
        run: Option<&Value>,
    ) -> Result<CardAnswer, String> {
        let path = format!("/cards/{number}/checklist/{index}");
        let body = with_run(json!({ "done": done }), run);
        self.made(action, Method::Patch, &path, Some(&body))
    }

    /// `DELETE /api/board/cards/<number>/checklist/<index>`: the item removed.
    pub(crate) fn remove_item(
        &self,
        action: &str,
        number: i64,
        index: i64,
        run: Option<&Value>,
    ) -> Result<CardAnswer, String> {
        let path = format!("/cards/{number}/checklist/{index}");
        let body = with_run(json!({}), run);
        self.made(action, Method::Delete, &path, Some(&body))
    }

    /// `POST` or `DELETE /api/board/cards/<number>/blocked_by/<other>`: the card blocked by
    /// `other`, or that block taken off.
    pub(crate) fn block(
        &self,
        action: &str,
        number: i64,
        other: i64,
        blocked: bool,
        run: Option<&Value>,
    ) -> Result<CardAnswer, String> {
        let path = format!("/cards/{number}/blocked_by/{other}");
        let method = if blocked {
            Method::Post
        } else {
            Method::Delete
        };
        self.made(action, method, &path, Some(&with_run(json!({}), run)))
    }

    /// `POST /api/board/cards/<number>/comments`: a comment of `text`, markdown.
    pub(crate) fn create_comment(
        &self,
        action: &str,
        number: i64,
        text: &str,
        run: Option<&Value>,
    ) -> Result<CommentAnswer, String> {
        let path = format!("/cards/{number}/comments");
        let body = with_run(json!({ "body": text }), run);
        self.made(action, Method::Post, &path, Some(&body))
    }

    /// `POST /api/board/cards/<number>/archive`, or `…/unarchive`.
    pub(crate) fn archive(
        &self,
        action: &str,
        number: i64,
        archived: bool,
        run: Option<&Value>,
    ) -> Result<CardAnswer, String> {
        let verb = if archived { "archive" } else { "unarchive" };
        let path = format!("/cards/{number}/{verb}");
        self.made(action, Method::Post, &path, Some(&with_run(json!({}), run)))
    }

    /// `DELETE /api/board/cards/<number>`: the card deleted for good, with its checklist, its
    /// comments and its activity. The call takes no body, so no run.
    pub(crate) fn delete_card(&self, action: &str, number: i64) -> Result<(), String> {
        let path = format!("/cards/{number}");
        self.send::<Value>(action, Method::Delete, &path, None)
            .map(|_| ())
    }

    /// `PATCH /api/board/cards/<number>` with `body`.
    fn patch_card(&self, action: &str, number: i64, body: Value) -> Result<CardAnswer, String> {
        let path = format!("/cards/{number}");
        self.made(action, Method::Patch, &path, Some(&body))
    }

    /// A write whose answer is a body: [`send`](Self::send) with the one it must carry.
    fn made<T: DeserializeOwned>(
        &self,
        action: &str,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<T, String> {
        self.send(action, method, path, body)?
            .ok_or_else(|| format!("{action}: the board answered with no body"))
    }

    /// GET the path `path` under `/api/board` with `query`, and decode its answer; or the
    /// sentence, behind `action`'s name, saying why there is none.
    fn get<T: DeserializeOwned>(
        &self,
        action: &str,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<T, String> {
        let (status, body) = self.request(action, Method::Get, path, query, None)?;
        decode(action, status, &body)
    }

    /// Call `method` on the path `path` under `/api/board` with `body`, JSON, when it has one,
    /// and decode its answer; `None` for a `204` or an empty body, which a delete answers.
    /// Or the sentence, behind `action`'s name, saying why there is none.
    pub(crate) fn send<T: DeserializeOwned>(
        &self,
        action: &str,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Option<T>, String> {
        let (status, text) = self.request(action, method, path, &[], body)?;
        if status == 204 || text.trim().is_empty() {
            return Ok(None);
        }
        decode(action, status, &text).map(Some)
    }

    /// One request: its answer's status and body when the board answers `2xx`, and otherwise
    /// the sentence, behind `action`'s name, saying why not: the board unreachable, or its own
    /// `{"error"}`.
    fn request(
        &self,
        action: &str,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
    ) -> Result<(u16, String), String> {
        let unreachable = |reason: String| {
            format!(
                "{action}: cannot reach the board at {}: {reason}",
                self.host
            )
        };
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(unreachable(format!(
                "the call's {}s budget ran out",
                CALL_BUDGET.as_secs()
            )));
        }
        let url = format!("{}{path}", self.root);
        let json = body.map(Value::to_string);
        let called = match (method, json.as_deref()) {
            (Method::Get, _) => self.dress(self.agent.get(url), query, left).call(),
            (Method::Delete, None) => self.dress(self.agent.delete(url), query, left).call(),
            (Method::Delete, Some(text)) => {
                let builder = self.agent.delete(url).force_send_body();
                json_body(self.dress(builder, query, left), text)
            }
            (Method::Post, text) => text.map_or_else(
                || {
                    self.dress(self.agent.post(url.clone()), query, left)
                        .send_empty()
                },
                |text| json_body(self.dress(self.agent.post(url.clone()), query, left), text),
            ),
            (Method::Patch, text) => text.map_or_else(
                || {
                    self.dress(self.agent.patch(url.clone()), query, left)
                        .send_empty()
                },
                |text| json_body(self.dress(self.agent.patch(url.clone()), query, left), text),
            ),
        };
        let mut resp = called.map_err(|e| unreachable(transport_reason(&e)))?;
        let status = resp.status().as_u16();
        let text = resp
            .body_mut()
            .with_config()
            .limit(MAX_BODY)
            .read_to_string()
            .map_err(|e| unreachable(transport_reason(&e)))?;
        if !(200..300).contains(&status) {
            return Err(match serde_json::from_str::<Problem>(&text) {
                Ok(problem) => format!("{action}: {}", problem.error),
                Err(_) => format!("{action}: the board answered {status}"),
            });
        }
        Ok((status, text))
    }

    /// `builder` with the key, `Accept`, `query` and a global timeout of what is left of the
    /// call's budget, `left`.
    fn dress<B>(
        &self,
        builder: RequestBuilder<B>,
        query: &[(&str, String)],
        left: Duration,
    ) -> RequestBuilder<B> {
        builder
            .header("Authorization", &self.authorization)
            .header("Accept", "application/json")
            .query_pairs(query.iter().map(|(k, v)| (*k, v.as_str())))
            .config()
            .timeout_global(Some(left.min(CONNECT_TIMEOUT + READ_TIMEOUT)))
            .build()
    }
}

/// Send `text` as the JSON body of `builder`.
fn json_body(
    builder: RequestBuilder<WithBody>,
    text: &str,
) -> Result<ureq::http::Response<ureq::Body>, ureq::Error> {
    builder
        .header("Content-Type", "application/json")
        .send(text)
}

/// The answer `body` of status `status`, decoded as the `T` the call gives; or the sentence
/// saying the board answered what it does not say.
fn decode<T: DeserializeOwned>(action: &str, status: u16, body: &str) -> Result<T, String> {
    serde_json::from_str(body).map_err(|e| {
        format!("{action}: the board answered {status} with what it does not say: {e}")
    })
}

/// `body`, an object, with the run it is made in under `"run"` when it has one. A write with no
/// run sends no `run` at all, so the board shows its change under the key's name.
fn with_run(mut body: Value, run: Option<&Value>) -> Value {
    if let (Some(run), Some(fields)) = (run, body.as_object_mut()) {
        fields.insert("run".to_string(), run.clone());
    }
    body
}

/// `text` as one segment of a path: everything but a letter, a digit and `-._~` as `%XX` bytes.
fn segment(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The host of the address `base`, its scheme and its path left off.
fn host_of(base: &str) -> &str {
    let rest = base.split_once("://").map_or(base, |(_, rest)| rest);
    rest.split('/').next().unwrap_or(rest)
}

/// A failure's reason, built **without** the request URL: the two ureq variants whose
/// `Display` renders the request URI are answered with a fixed string, and every other one
/// passes through with its diagnostic intact ("io: Connection refused").
fn transport_reason(e: &ureq::Error) -> String {
    match e {
        ureq::Error::BadUri(_) => "bad uri".to_string(),
        ureq::Error::RequireHttpsOnly(_) => "https required".to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A host is read off an address with or without its scheme, port and path.
    #[test]
    fn a_host_is_the_address_without_its_scheme_and_path() {
        assert_eq!(host_of("https://app.afkd.sh"), "app.afkd.sh");
        assert_eq!(host_of("http://127.0.0.1:8080/board"), "127.0.0.1:8080");
        assert_eq!(host_of("app.afkd.sh"), "app.afkd.sh");
    }

    /// Neither URL-bearing variant renders its URI, whatever it carries.
    #[test]
    fn a_transport_reason_never_renders_the_uri() {
        let uri = "http://127.0.0.1:1/api/board?SUPERSECRET";
        for e in [
            ureq::Error::BadUri(uri.to_string()),
            ureq::Error::RequireHttpsOnly(uri.to_string()),
        ] {
            let reason = transport_reason(&e);
            assert!(!reason.contains("SUPERSECRET"), "{reason}");
        }
    }

    /// A spent call deadline sends nothing, and says which budget ran out; a refused
    /// connection names the host and never the key.
    #[test]
    fn a_failure_names_the_host_and_never_the_key() {
        let board = Board {
            key: "afk_board_SUPERSECRET".to_string(),
            base_url: "http://127.0.0.1:9/".to_string(),
        };
        let spent = BoardClient::new(&board, Instant::now());
        assert_eq!(
            spent.board("lists").unwrap_err(),
            "lists: cannot reach the board at 127.0.0.1:9: the call's 45s budget ran out"
        );
        let client = BoardClient::new(&board, Instant::now() + CALL_BUDGET);
        let e = client.board("lists").unwrap_err();
        assert!(
            e.starts_with("lists: cannot reach the board at 127.0.0.1:9: "),
            "{e}"
        );
        assert!(!e.contains("SUPERSECRET"), "{e}");
        assert!(!format!("{client:?}").contains("SUPERSECRET"));
    }

    /// A spent call deadline refuses a write before it reaches the host, whatever its method.
    #[test]
    fn a_spent_deadline_refuses_a_send_without_reaching_the_host() {
        let board = Board {
            key: "afk_board_SUPERSECRET".to_string(),
            base_url: "http://127.0.0.1:9/".to_string(),
        };
        let spent = BoardClient::new(&board, Instant::now());
        for method in [Method::Post, Method::Patch, Method::Delete] {
            let body = json!({"list": "Todo"});
            assert_eq!(
                spent
                    .send::<Value>("create_card", method, "/cards", Some(&body))
                    .unwrap_err(),
                "create_card: cannot reach the board at 127.0.0.1:9: the call's 45s budget ran out"
            );
        }
        assert_eq!(
            spent.delete_list("delete_list", "l-1").unwrap_err(),
            "delete_list: cannot reach the board at 127.0.0.1:9: the call's 45s budget ran out"
        );
    }

    /// A run is put in a body when there is one, and a body without one has no `run` key.
    #[test]
    fn a_run_is_in_the_body_only_when_there_is_one() {
        let run = json!({"machine": "m", "service": "s", "instance": 0, "id": "i"});
        assert_eq!(
            with_run(json!({"text": "x"}), Some(&run)),
            json!({"text": "x", "run": run})
        );
        assert_eq!(with_run(json!({"text": "x"}), None), json!({"text": "x"}));
    }

    /// A path segment keeps its unreserved bytes and escapes the rest, a `/` and a space and
    /// a multi-byte letter among them.
    #[test]
    fn a_path_segment_escapes_all_but_the_unreserved() {
        assert_eq!(segment("bug-1.x_y~z"), "bug-1.x_y~z");
        assert_eq!(segment("a b/c+d?"), "a%20b%2Fc%2Bd%3F");
        assert_eq!(segment("媒体"), "%E5%AA%92%E4%BD%93");
    }

    /// A place names `at` or `after`, never both.
    #[test]
    fn a_place_names_at_or_after() {
        let mut body = json!({});
        Place::<i64>::Top.put(&mut body);
        assert_eq!(body, json!({"at": "top"}));
        let mut body = json!({});
        Place::After(7).put(&mut body);
        assert_eq!(body, json!({"after": 7}));
        let mut body = json!({});
        Place::<String>::After("l-1".to_string()).put(&mut body);
        assert_eq!(body, json!({"after": "l-1"}));
    }
}
