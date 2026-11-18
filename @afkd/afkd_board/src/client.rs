//! The board's HTTP API, as the reads use it: a GET under `<base_url>/api/board` with the
//! `afkd_board.Board`'s key as `Authorization: Bearer <key>`, its JSON answer decoded, and a
//! refusal read from the `{"error": …}` body the API answers every one with (the site's README,
//! "A board's API").
//!
//! The agent is `@afkd/linear`'s cut down to one call: a child answers one call and ends, so
//! there is no pool to keep fresh. afkd ends the call when it misses its reply, so every
//! request's global timeout is clipped to what is left of [`CALL_BUDGET`], and once it is gone
//! no request is sent at all. A redirect never carries the `Authorization` header on (ureq's
//! `RedirectAuthHeaders::Never`, kept), and a failure is told without the URL or the key.

use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

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

    /// GET the path `path` under `/api/board` with `query`, and decode its answer; or the
    /// sentence, behind `action`'s name, saying why there is none.
    fn get<T: DeserializeOwned>(
        &self,
        action: &str,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<T, String> {
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
        let req = self
            .agent
            .get(format!("{}{path}", self.root))
            .header("Authorization", &self.authorization)
            .header("Accept", "application/json")
            .query_pairs(query.iter().map(|(k, v)| (*k, v.as_str())))
            .config()
            .timeout_global(Some(left.min(CONNECT_TIMEOUT + READ_TIMEOUT)))
            .build();
        let mut resp = req.call().map_err(|e| unreachable(transport_reason(&e)))?;
        let status = resp.status().as_u16();
        let body = resp
            .body_mut()
            .with_config()
            .limit(MAX_BODY)
            .read_to_string()
            .map_err(|e| unreachable(transport_reason(&e)))?;
        if !(200..300).contains(&status) {
            return Err(match serde_json::from_str::<Problem>(&body) {
                Ok(problem) => format!("{action}: {}", problem.error),
                Err(_) => format!("{action}: the board answered {status}"),
            });
        }
        serde_json::from_str(&body).map_err(|e| {
            format!("{action}: the board answered {status} with what it does not say: {e}")
        })
    }
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
}
