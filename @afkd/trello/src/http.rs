//! The HTTP request/response spine the Trello client rides: the pooled agent,
//! absolute-URL construction off an already-resolved API root, the `key`+`token`
//! credential, the send-and-decode path, the JSON extraction helpers, and the
//! status/transport/decode error mapping.
//!
//! Ported from the parts of afkd's `afkd_forge::http` the Trello client reaches. One
//! difference: afkd shares one agent across a whole daemon's services, while this child
//! serves exactly one service, so the agent is the client's own. Its expiry rules are
//! afkd's — idle past [`AGENT_MAX_IDLE`] or alive past [`AGENT_MAX_AGE`], and the pool is
//! rebuilt rather than kept warm forever.
//!
//! Trello authenticates with `?key=…&token=…`, so the credential rides the **URL**, which
//! is what makes [`transport_reason`]'s URL-free rule load-bearing rather than merely
//! tidy: the two `ureq::Error` variants that render the request URI would fold a live key
//! and token into whatever the caller logs.

use std::marker::PhantomData;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::common::lock;

/// How long a board call waits to establish a connection before giving up. A wedged
/// socket must not park a `poll` past afkd's call deadline, so even the "healthy" budget
/// is finite.
pub(crate) const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a board call waits for the response (read/write) before giving up.
pub(crate) const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Idle longer than this and the pooled sockets are assumed stale: the peer, a NAT or a
/// tunnel exit may have dropped them without ever sending a FIN. Handed to ureq as its
/// pool's `max_idle_age` and read by the recycle rule, so the two agree on one number.
const AGENT_MAX_IDLE: Duration = Duration::from_secs(60);

/// And never hand out one generation's sockets longer than this, however busy it is.
const AGENT_MAX_AGE: Duration = Duration::from_secs(300);

/// One generation of the client's agent: the agent itself plus the two instants the
/// recycle rule reads.
struct Generation {
    agent: ureq::Agent,
    created: Instant,
    last_used: Instant,
}

/// Whether a generation must be rebuilt: idle past [`AGENT_MAX_IDLE`], or alive past
/// [`AGENT_MAX_AGE`] however busy it has been. A pure function of three instants, so the
/// policy is testable without a clock.
fn agent_expired(created: Instant, last_used: Instant, now: Instant) -> bool {
    now.duration_since(last_used) > AGENT_MAX_IDLE || now.duration_since(created) > AGENT_MAX_AGE
}

/// Build one pooled, timeout-configured agent.
///
/// `timeout_global` is what makes pooling safe: it is the only budget measured from the
/// request's *start*, so a recycled connection to a peer that accepted and then went
/// silent trips at `connect + read` instead of parking the call forever. The four per-op
/// budgets under it bound each leg of the exchange separately.
fn build_agent(connect: Duration, read: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(connect))
        .timeout_send_request(Some(read))
        .timeout_send_body(Some(read))
        .timeout_recv_response(Some(read))
        .timeout_recv_body(Some(read))
        .timeout_global(Some(connect + read))
        .max_idle_age(AGENT_MAX_IDLE)
        .build()
        .new_agent()
}

/// A request [`HttpClient::request`] built and handed back unexecuted, over whichever of
/// ureq's two builder typestates the verb takes.
pub(crate) enum BoardRequest {
    /// A verb ureq builds without a request body: GET, DELETE.
    NoBody(ureq::RequestBuilder<ureq::typestate::WithoutBody>),
    /// A verb ureq builds with one: POST, PUT.
    WithBody(ureq::RequestBuilder<ureq::typestate::WithBody>),
}

impl BoardRequest {
    /// Append one `key=value` query pair, percent-encoded onto the request target.
    ///
    /// Appends rather than replaces, so the credential's pairs, stamped first, always
    /// lead.
    pub(crate) fn query(self, key: &str, value: &str) -> Self {
        match self {
            BoardRequest::NoBody(r) => BoardRequest::NoBody(r.query(key, value)),
            BoardRequest::WithBody(r) => BoardRequest::WithBody(r.query(key, value)),
        }
    }

    /// Set one header, verbatim — `send_json`'s content type.
    fn header(self, name: &str, value: &str) -> Self {
        match self {
            BoardRequest::NoBody(r) => BoardRequest::NoBody(r.header(name, value)),
            BoardRequest::WithBody(r) => BoardRequest::WithBody(r.header(name, value)),
        }
    }

    /// Send the request with no body, whichever typestate it holds.
    fn call(self) -> Result<ureq::http::Response<ureq::Body>, ureq::Error> {
        match self {
            BoardRequest::NoBody(r) => r.call(),
            BoardRequest::WithBody(r) => r.send_empty(),
        }
    }

    /// Send the request carrying `body`. A bodyless verb is forced to send one rather
    /// than refused, so the seam is total; no call site reaches that arm.
    fn send_body(self, body: &str) -> Result<ureq::http::Response<ureq::Body>, ureq::Error> {
        match self {
            BoardRequest::NoBody(r) => r.force_send_body().send(body),
            BoardRequest::WithBody(r) => r.send(body),
        }
    }

    /// Send the request with no body, with the status-to-error translation turned
    /// **off** for this one request — so a 4xx/5xx arrives as an `Ok` response whose
    /// body is still readable.
    fn call_reading_any_status(self) -> Result<ureq::http::Response<ureq::Body>, ureq::Error> {
        match self {
            BoardRequest::NoBody(r) => r.config().http_status_as_error(false).build().call(),
            BoardRequest::WithBody(r) => {
                r.config().http_status_as_error(false).build().send_empty()
            }
        }
    }
}

/// The failure vocabulary a caller's error type supplies so the spine can report a fault
/// without naming one: three constructors tagged with the **stage** of work the call
/// belonged to, plus the JSON extraction helpers built on them.
pub(crate) trait HttpError: Sized {
    /// The board answered with a non-success HTTP status.
    fn status(stage: &'static str, status: u16) -> Self;

    /// The request never produced a response. `reason` is built URL-free.
    fn transport(stage: &'static str, reason: String) -> Self;

    /// The response body could not be decoded into the expected shape.
    fn decode(stage: &'static str, reason: &str) -> Self;

    /// Parse a response body as JSON, mapping a syntax error onto [`decode`](Self::decode).
    fn decode_json(stage: &'static str, body: &str) -> Result<Value, Self> {
        serde_json::from_str(body).map_err(|e| Self::decode(stage, &e.to_string()))
    }

    /// Read `value` as a JSON array, or fail naming `what` was expected.
    fn as_array<'a>(
        stage: &'static str,
        value: &'a Value,
        what: &str,
    ) -> Result<&'a Vec<Value>, Self> {
        value
            .as_array()
            .ok_or_else(|| Self::decode(stage, &format!("expected an array of {what}")))
    }
}

/// A timeout-configured HTTP client bound to one API root, one query-pair credential,
/// and one caller error type `E`.
pub(crate) struct HttpClient<E> {
    base: String,
    /// Folded onto every request's URL, in order: Trello's `key` and `token`.
    auth: Vec<(String, String)>,
    connect: Duration,
    read: Duration,
    /// The current agent generation, built on first use.
    agent: Mutex<Option<Generation>>,
    _err: PhantomData<fn() -> E>,
}

impl<E: HttpError> HttpClient<E> {
    /// A client against an **already-resolved** API root `base` (nothing here trims or
    /// suffixes it), folding the `auth` pairs onto every request, with explicit
    /// connect/read timeouts. The `read` budget bounds the write side too.
    pub(crate) fn new(
        base: impl Into<String>,
        auth: Vec<(String, String)>,
        connect: Duration,
        read: Duration,
    ) -> Self {
        Self {
            base: base.into(),
            auth,
            connect,
            read,
            agent: Mutex::new(None),
            _err: PhantomData,
        }
    }

    /// The API root every request is built off, as the caller handed it over.
    #[cfg(test)]
    pub(crate) fn base(&self) -> &str {
        &self.base
    }

    /// The agent for the next request, rebuilt past either expiry bound and stamped as
    /// used. The critical section is two instant compares and an `Arc` clone.
    fn agent(&self) -> ureq::Agent {
        let mut slot = lock(&self.agent);
        let now = Instant::now();
        let generation = match slot.take() {
            Some(g) if !agent_expired(g.created, g.last_used, now) => g,
            _ => Generation {
                agent: build_agent(self.connect, self.read),
                created: now,
                last_used: now,
            },
        };
        let agent = generation.agent.clone();
        *slot = Some(Generation {
            last_used: now,
            ..generation
        });
        agent
    }

    /// A `method` request for the base-relative `path`, with the credential already
    /// stamped as the leading query pairs, handed back unexecuted so the caller appends
    /// its own.
    pub(crate) fn request(&self, method: &str, path: &str) -> BoardRequest {
        let agent = self.agent();
        let url = format!("{}{path}", self.base);
        let req = match method {
            "POST" => BoardRequest::WithBody(agent.post(&url)),
            "PUT" => BoardRequest::WithBody(agent.put(&url)),
            "DELETE" => BoardRequest::NoBody(agent.delete(&url)),
            // GET, and — deliberately, rather than by panic — any other verb. The
            // vocabulary is a literal at every call site.
            _ => BoardRequest::NoBody(agent.get(&url)),
        };
        self.auth.iter().fold(req, |req, (k, v)| req.query(k, v))
    }

    /// GET `path` with the credential, returning the body text.
    pub(crate) fn get(&self, stage: &'static str, path: &str) -> Result<String, E> {
        self.send(stage, self.request("GET", path))
    }

    /// Execute a built request, mapping ureq's outcome onto an `E` and reading the body.
    pub(crate) fn send(&self, stage: &'static str, req: BoardRequest) -> Result<String, E> {
        map_outcome(stage, req.call())
    }

    /// Send a JSON `payload` as the request body with the right content type.
    pub(crate) fn send_json(
        &self,
        stage: &'static str,
        req: BoardRequest,
        payload: &Value,
    ) -> Result<String, E> {
        let result = req
            .header("Content-Type", "application/json")
            .send_body(&payload.to_string());
        map_outcome(stage, result)
    }

    /// Execute a built request, letting the caller **judge** a non-success status off
    /// its body before it becomes an error: when `tolerate(status, body)` holds, the body
    /// is returned as `Ok` instead.
    ///
    /// The shape of "this board faults an idempotent re-do rather than no-op-ing it"
    /// (a re-added member), and of "this status is an answer" (a card that is gone).
    /// Every other outcome — success, transport failure, a status the predicate
    /// rejects — goes through the same mapping [`send`](Self::send) does.
    pub(crate) fn send_tolerating(
        &self,
        stage: &'static str,
        req: BoardRequest,
        tolerate: impl Fn(u16, &str) -> bool,
    ) -> Result<String, E> {
        // With the status translation off, a 4xx/5xx arrives as `Ok` with its body
        // intact, and every `Err` left is a transport failure, which the predicate must
        // never see because there is no status to judge.
        match req.call_reading_any_status() {
            Err(e) => Err(E::transport(stage, transport_reason(&e))),
            Ok(mut resp) => {
                let status = resp.status();
                if !(status.is_client_error() || status.is_server_error()) {
                    read_body(stage, &mut resp)
                } else {
                    // An unreadable body is an empty one: a fault the predicate cannot
                    // recognize, so it stays an error.
                    let body = resp.body_mut().read_to_string().unwrap_or_default();
                    let status = status.as_u16();
                    if tolerate(status, &body) {
                        Ok(body)
                    } else {
                        Err(E::status(stage, status))
                    }
                }
            }
        }
    }
}

/// Map ureq's request outcome onto the caller's error vocabulary, reading a successful
/// response to text.
fn map_outcome<E: HttpError>(
    stage: &'static str,
    result: Result<ureq::http::Response<ureq::Body>, ureq::Error>,
) -> Result<String, E> {
    match result {
        Ok(mut resp) => read_body(stage, &mut resp),
        Err(ureq::Error::StatusCode(status)) => Err(E::status(stage, status)),
        // Everything else ureq can raise is a request that produced no response.
        // `ureq::Error` is `#[non_exhaustive]`, so this arm is required as well as correct.
        Err(other) => Err(E::transport(stage, transport_reason(&other))),
    }
}

/// Read an arrived response to text: a premature EOF mid-body is a **decode** failure,
/// not a transport one — the status line already arrived.
fn read_body<E: HttpError>(
    stage: &'static str,
    resp: &mut ureq::http::Response<ureq::Body>,
) -> Result<String, E> {
    resp.body_mut()
        .read_to_string()
        .map_err(|e| E::decode(stage, &transport_reason(&e)))
}

/// A failure's reason, built **without** the request URL: the two ureq variants whose
/// `Display` renders the request URI are answered with a fixed string, and every other
/// one passes through with its diagnostic intact ("io: Connection refused").
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

    /// The recycle policy over its two bounds, each at and past its edge.
    #[test]
    fn an_agent_expires_when_idle_or_old() {
        let born = Instant::now();
        let at = |secs: u64| born + Duration::from_secs(secs);
        assert!(!agent_expired(born, born, born));
        assert!(
            !agent_expired(born, at(0), at(60)),
            "idle exactly at the bound"
        );
        assert!(agent_expired(born, at(0), at(61)), "idle past it");
        assert!(
            !agent_expired(born, at(299), at(300)),
            "busy, at the age bound"
        );
        assert!(
            agent_expired(born, at(300), at(301)),
            "busy, past the age bound"
        );
    }

    /// Neither URL-bearing variant renders its URI, whatever it carries — and Trello's
    /// URI carries the live key and token.
    #[test]
    fn a_transport_reason_never_renders_the_uri() {
        let uri = "http://127.0.0.1:1/1/members/me?key=SUPERSECRET-KEY&token=SUPERSECRET-TOKEN";
        let bad = ureq::Error::BadUri(uri.to_string());
        let https = ureq::Error::RequireHttpsOnly(uri.to_string());
        for e in [bad, https] {
            let reason = transport_reason(&e);
            assert!(!reason.contains("SUPERSECRET"), "{reason}");
        }
    }
}
