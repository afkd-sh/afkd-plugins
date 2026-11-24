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
//! One addition afkd's spine has no need of: the current call's deadline. afkd ends the
//! service when a call misses its reply, so while a call is under way every request's
//! global timeout is clipped to what is left of [`CALL_BUDGET`], and once it is gone no
//! request is sent at all.
//!
//! Trello authenticates with `?key=…&token=…`, so the credential rides the **URL**, which
//! is what makes [`transport_reason`]'s URL-free rule load-bearing rather than merely
//! tidy: the two `ureq::Error` variants that render the request URI would fold a live key
//! and token into whatever the caller logs.
//!
//! A client is **retrying** when the caller asks ([`HttpClient::retrying`]): a read or a write
//! through a `trello.Account` is done or refused, so a 429 is tried again — on every verb — and a 5xx
//! on every verb but `POST`, which Trello may already have carried out. Each wait is what
//! Trello's `x-rate-limit-*-interval-ms` headers say, or else one second doubling, and one
//! that would pass the call's deadline is not taken: the call is refused with the answer it
//! last got. A request is therefore a *description* ([`BoardRequest`]) built anew for every
//! attempt, and every status is read here, never turned into an error by `ureq`.

use std::marker::PhantomData;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::common::{lock, CALL_BUDGET};

/// How long a board call waits to establish a connection before giving up. A wedged
/// socket must not park a `poll` past afkd's call deadline, so even the "healthy" budget
/// is finite.
pub(crate) const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a board call waits for the response (read/write) before giving up.
pub(crate) const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// The wait before the first retry of an answer that names none, doubling after each one.
pub(crate) const RETRY_FIRST: Duration = Duration::from_secs(1);

/// The most times one request is tried again. The call's deadline is what ends retries in
/// practice; this ends them for a client with none.
const RETRY_MAX: u32 = 8;

/// The most a response body may hold, past which it is a decode fault: far over what one
/// reply can be (afkd takes 8 MiB of a call's), so a page of cards is never cut short by
/// `ureq`'s own default of 10 MB.
const MAX_BODY: u64 = 64 * 1024 * 1024;

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

/// The HTTP verbs the client sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verb {
    Get,
    Post,
    Put,
    Delete,
}

impl Verb {
    /// The verb `method` names — GET, and deliberately rather than by panic, any other
    /// word. The vocabulary is a literal at every call site.
    fn of(method: &str) -> Self {
        match method {
            "POST" => Verb::Post,
            "PUT" => Verb::Put,
            "DELETE" => Verb::Delete,
            _ => Verb::Get,
        }
    }
}

/// A request [`HttpClient::request`] described and handed back unsent: built into one of
/// `ureq`'s requests anew for every attempt, so a retry sends what the first did.
pub(crate) struct BoardRequest {
    verb: Verb,
    /// The whole address, without any query.
    url: String,
    /// Whether the client's credential pairs lead the query.
    credentialed: bool,
    pairs: Vec<(String, String)>,
    headers: Vec<(String, String)>,
}

impl BoardRequest {
    /// Append one `key=value` query pair, percent-encoded onto the request target.
    ///
    /// Appends rather than replaces, so the credential's pairs, stamped first, always
    /// lead.
    pub(crate) fn query(mut self, key: &str, value: &str) -> Self {
        self.pairs.push((key.to_string(), value.to_string()));
        self
    }

    /// Set one header, verbatim — a download's `Authorization`.
    fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }
}

/// What a request carries as its body.
#[derive(Clone, Copy)]
pub(crate) enum Payload<'a> {
    Empty,
    /// A JSON document, under its content type.
    Json(&'a Value),
    /// Bytes verbatim, under `content_type` — an upload's multipart form.
    Bytes {
        content_type: &'a str,
        body: &'a [u8],
    },
}

/// One attempt of a [`BoardRequest`] built, over whichever of ureq's two builder typestates
/// the verb takes.
enum Built {
    /// A verb ureq builds without a request body: GET, DELETE.
    NoBody(ureq::RequestBuilder<ureq::typestate::WithoutBody>),
    /// A verb ureq builds with one: POST, PUT.
    WithBody(ureq::RequestBuilder<ureq::typestate::WithBody>),
}

impl Built {
    /// Set one header, verbatim.
    fn header(self, name: &str, value: &str) -> Self {
        match self {
            Built::NoBody(r) => Built::NoBody(r.header(name, value)),
            Built::WithBody(r) => Built::WithBody(r.header(name, value)),
        }
    }

    /// Read every status here rather than have `ureq` turn a 4xx or 5xx into an error —
    /// so such a response arrives as an `Ok` whose headers and body are still readable —
    /// and clip the request's global timeout to `global`, when the call has a deadline. The
    /// agent's per-op timeouts still apply underneath.
    fn configure(self, global: Option<Duration>) -> Self {
        match self {
            Built::NoBody(r) => {
                let mut config = r.config().http_status_as_error(false);
                if let Some(d) = global {
                    config = config.timeout_global(Some(d));
                }
                Built::NoBody(config.build())
            }
            Built::WithBody(r) => {
                let mut config = r.config().http_status_as_error(false);
                if let Some(d) = global {
                    config = config.timeout_global(Some(d));
                }
                Built::WithBody(config.build())
            }
        }
    }

    /// Send the request carrying `body`. A bodyless verb is forced to send one rather
    /// than refused, so the seam is total; no call site reaches that arm.
    fn send_body(
        self,
        body: impl ureq::AsSendBody,
    ) -> Result<ureq::http::Response<ureq::Body>, ureq::Error> {
        match self {
            Built::NoBody(r) => r.force_send_body().send(body),
            Built::WithBody(r) => r.send(body),
        }
    }

    /// Send the request with `payload` as its body.
    fn send(self, payload: Payload<'_>) -> Result<ureq::http::Response<ureq::Body>, ureq::Error> {
        match payload {
            Payload::Empty => match self {
                Built::NoBody(r) => r.call(),
                Built::WithBody(r) => r.send_empty(),
            },
            Payload::Json(value) => self
                .header("Content-Type", "application/json")
                .send_body(&value.to_string()),
            Payload::Bytes { content_type, body } => {
                self.header("Content-Type", content_type).send_body(body)
            }
        }
    }
}

/// Whether an answer of `status` to a request of `verb` is tried again: a 429 whatever the
/// verb, and a 5xx unless the verb is a `POST`, which Trello may already have carried out.
fn retryable(verb: Verb, status: u16) -> bool {
    status == 429 || (status >= 500 && verb != Verb::Post)
}

/// What Trello's rate-limit headers ask a client to wait: the largest of its
/// `x-rate-limit-*-interval-ms` values, or `None` when it names none.
fn rate_wait(headers: &ureq::http::HeaderMap) -> Option<Duration> {
    headers
        .iter()
        .filter(|(name, _)| {
            let name = name.as_str();
            name.starts_with("x-rate-limit-") && name.ends_with("-interval-ms")
        })
        .filter_map(|(_, value)| value.to_str().ok()?.trim().parse::<u64>().ok())
        .max()
        .map(Duration::from_millis)
}

/// Whether `status` is one `ureq` would have made an error of.
fn is_failure(status: ureq::http::StatusCode) -> bool {
    status.is_client_error() || status.is_server_error()
}

/// The failure vocabulary a caller's error type supplies so the spine can report a fault
/// without naming one: three constructors tagged with the **stage** of work the call
/// belonged to, plus the JSON extraction helpers built on them.
pub(crate) trait HttpError: Sized {
    /// The board answered with a non-success HTTP status.
    fn status(stage: &'static str, status: u16) -> Self;

    /// The board answered with a non-success HTTP status and said why: `message` is its own
    /// text for the refusal.
    fn answered(stage: &'static str, status: u16, message: String) -> Self;

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
    /// The current call's deadline, `None` between calls.
    deadline: Mutex<Option<Instant>>,
    /// Whether an answer that says try again later is tried again ([`Self::retrying`]).
    retrying: bool,
    /// How a retry waits: the thread sleeps, except in a test that records the waits.
    sleep: Arc<dyn Fn(Duration) + Send + Sync>,
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
            deadline: Mutex::new(None),
            retrying: false,
            sleep: Arc::new(std::thread::sleep),
            _err: PhantomData,
        }
    }

    /// This client, retrying: a 429 is tried again on every verb, and a 5xx on every verb
    /// but `POST`, each after the wait Trello's rate-limit headers name or else one second
    /// doubling, for as long as the wait ends before the call's deadline.
    pub(crate) fn retrying(mut self) -> Self {
        self.retrying = true;
        self
    }

    /// This client, waiting through `sleep` rather than the thread's.
    #[cfg(test)]
    pub(crate) fn sleeping(mut self, sleep: impl Fn(Duration) + Send + Sync + 'static) -> Self {
        self.sleep = Arc::new(sleep);
        self
    }

    /// The API root every request is built off, as the caller handed it over.
    #[cfg(test)]
    pub(crate) fn base(&self) -> &str {
        &self.base
    }

    /// Set (or, with `None`, clear) the current call's deadline, which every request
    /// sent until the next set is clipped to. Measured on the real monotonic clock, the
    /// one the plugin stamps it with.
    pub(crate) fn set_deadline(&self, deadline: Option<Instant>) {
        *lock(&self.deadline) = deadline;
    }

    /// The global timeout the next request gets: `None` outside a call, else the request's
    /// own `connect + read` or what is left of the call, whichever is shorter. A call
    /// with nothing left sends nothing: that is a transport failure naming the budget.
    fn request_budget(&self, stage: &'static str) -> Result<Option<Duration>, E> {
        let Some(deadline) = *lock(&self.deadline) else {
            return Ok(None);
        };
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(E::transport(
                stage,
                format!("the call's {}s budget ran out", CALL_BUDGET.as_secs()),
            ));
        }
        Ok(Some(left.min(self.connect + self.read)))
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

    /// One attempt of `req` built: the credential pairs first if it carries them, then its
    /// own pairs, then its headers.
    fn build(&self, req: &BoardRequest) -> Built {
        let agent = self.agent();
        let built = match req.verb {
            Verb::Post => Built::WithBody(agent.post(&req.url)),
            Verb::Put => Built::WithBody(agent.put(&req.url)),
            Verb::Delete => Built::NoBody(agent.delete(&req.url)),
            Verb::Get => Built::NoBody(agent.get(&req.url)),
        };
        let auth: &[(String, String)] = if req.credentialed { &self.auth } else { &[] };
        let built = auth
            .iter()
            .chain(&req.pairs)
            .fold(built, |built, (k, v)| match built {
                Built::NoBody(r) => Built::NoBody(r.query(k, v)),
                Built::WithBody(r) => Built::WithBody(r.query(k, v)),
            });
        req.headers
            .iter()
            .fold(built, |built, (name, value)| built.header(name, value))
    }

    /// A `method` request for the base-relative `path`, with the credential to lead its
    /// query, handed back unsent so the caller appends its own pairs.
    pub(crate) fn request(&self, method: &str, path: &str) -> BoardRequest {
        BoardRequest {
            verb: Verb::of(method),
            url: format!("{}{path}", self.base),
            credentialed: true,
            pairs: Vec::new(),
            headers: Vec::new(),
        }
    }

    /// GET `path` with the credential, returning the body text.
    pub(crate) fn get(&self, stage: &'static str, path: &str) -> Result<String, E> {
        self.send(stage, self.request("GET", path))
    }

    /// Send `req` with `payload` as its body, and read what came back: the response — any
    /// status, with its headers and its body unread — or a transport failure.
    ///
    /// A retrying client ([`Self::retrying`]) sends it again when the answer says to, after
    /// the wait the answer names or else [`RETRY_FIRST`] doubling, until the answer is one
    /// that stands, the wait would end past the call's deadline, or [`RETRY_MAX`] retries
    /// are spent: then the answer last got is returned, for the caller to refuse the call
    /// with.
    fn exchange(
        &self,
        stage: &'static str,
        req: &BoardRequest,
        payload: Payload<'_>,
    ) -> Result<ureq::http::Response<ureq::Body>, E> {
        let mut backoff = RETRY_FIRST;
        let mut retries = 0;
        loop {
            let budget = self.request_budget(stage)?;
            let resp = self
                .build(req)
                .configure(budget)
                .send(payload)
                .map_err(|e| E::transport(stage, transport_reason(&e)))?;
            let status = resp.status();
            if !self.retrying
                || !is_failure(status)
                || !retryable(req.verb, status.as_u16())
                || retries >= RETRY_MAX
            {
                return Ok(resp);
            }
            let wait = rate_wait(resp.headers()).unwrap_or_else(|| {
                let wait = backoff;
                backoff *= 2;
                wait
            });
            if lock(&self.deadline).is_some_and(|deadline| Instant::now() + wait >= deadline) {
                return Ok(resp);
            }
            (self.sleep)(wait);
            retries += 1;
        }
    }

    /// Execute a built request, mapping its outcome onto an `E` and reading the body.
    pub(crate) fn send(&self, stage: &'static str, req: BoardRequest) -> Result<String, E> {
        into_text(stage, self.exchange(stage, &req, Payload::Empty)?)
    }

    /// Send a JSON `payload` as the request body with the right content type.
    pub(crate) fn send_json(
        &self,
        stage: &'static str,
        req: BoardRequest,
        payload: &Value,
    ) -> Result<String, E> {
        into_text(stage, self.exchange(stage, &req, Payload::Json(payload))?)
    }

    /// Send `req` with `payload` as its body, like [`send`](Self::send), but keep what the
    /// board says when it refuses: on a 4xx or 5xx the body is read and the failure is
    /// [`HttpError::answered`] with Trello's text — a JSON string unquoted, an object's
    /// `message`, else the body as it came — and plain [`HttpError::status`] when the body
    /// is empty. A write that may be another member's to make needs the words.
    pub(crate) fn send_answering(
        &self,
        stage: &'static str,
        req: BoardRequest,
        payload: Payload<'_>,
    ) -> Result<String, E> {
        let mut resp = self.exchange(stage, &req, payload)?;
        let status = resp.status();
        if !is_failure(status) {
            return read_body(stage, &mut resp);
        }
        // An unreadable body is an empty one, so the refusal stands as its status.
        let body = resp.body_mut().read_to_string().unwrap_or_default();
        let status = status.as_u16();
        match answer_text(&body) {
            Some(message) => Err(E::answered(stage, status, message)),
            None => Err(E::status(stage, status)),
        }
    }

    /// GET the absolute `url` verbatim — off no API root, with none of the credential's
    /// query pairs — authenticated by `authorization` as its `Authorization` header, and
    /// read its body as bytes, no more than `limit` of them. ureq strips the header from
    /// any redirect it follows.
    pub(crate) fn get_bytes_at(
        &self,
        stage: &'static str,
        url: &str,
        authorization: &str,
        limit: u64,
    ) -> Result<Vec<u8>, E> {
        let req = BoardRequest {
            verb: Verb::Get,
            url: url.to_string(),
            credentialed: false,
            pairs: Vec::new(),
            headers: Vec::new(),
        }
        .header("Authorization", authorization);
        let mut resp = self.exchange(stage, &req, Payload::Empty)?;
        if is_failure(resp.status()) {
            return Err(E::status(stage, resp.status().as_u16()));
        }
        resp.body_mut()
            .with_config()
            .limit(limit)
            .read_to_vec()
            .map_err(|e| match e {
                ureq::Error::BodyExceedsLimit(_) => {
                    E::decode(stage, &format!("the body is over {limit} bytes"))
                }
                e => E::decode(stage, &transport_reason(&e)),
            })
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
        // A 4xx/5xx arrives as `Ok` with its body intact, and every `Err` left is a
        // transport failure, which the predicate must never see because there is no status
        // to judge.
        let mut resp = self.exchange(stage, &req, Payload::Empty)?;
        let status = resp.status();
        if !is_failure(status) {
            return read_body(stage, &mut resp);
        }
        // An unreadable body is an empty one: a fault the predicate cannot recognize, so
        // it stays an error.
        let body = resp.body_mut().read_to_string().unwrap_or_default();
        let status = status.as_u16();
        if tolerate(status, &body) {
            Ok(body)
        } else {
            Err(E::status(stage, status))
        }
    }
}

/// Trello's words in a refusal's `body`: a JSON string unquoted, a JSON object's `message`,
/// else the body as it came, trimmed. `None` when it holds none.
fn answer_text(body: &str) -> Option<String> {
    let text = match serde_json::from_str::<Value>(body) {
        Ok(Value::String(text)) => text,
        Ok(Value::Object(object)) => match object.get("message").and_then(Value::as_str) {
            Some(message) => message.to_string(),
            None => body.to_string(),
        },
        _ => body.to_string(),
    };
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// Read a response that arrived to text, or fail with its status when it is a failure: the
/// translation `ureq` would have made, made here since every status is read here.
fn into_text<E: HttpError>(
    stage: &'static str,
    mut resp: ureq::http::Response<ureq::Body>,
) -> Result<String, E> {
    if is_failure(resp.status()) {
        return Err(E::status(stage, resp.status().as_u16()));
    }
    read_body(stage, &mut resp)
}

/// Read an arrived response to text: a premature EOF mid-body is a **decode** failure,
/// not a transport one — the status line already arrived.
fn read_body<E: HttpError>(
    stage: &'static str,
    resp: &mut ureq::http::Response<ureq::Body>,
) -> Result<String, E> {
    resp.body_mut()
        .with_config()
        .limit(MAX_BODY)
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

    /// A 429 is retried whatever the verb; a 5xx on every verb but the `POST`, which Trello
    /// may already have carried out; and nothing else, a 4xx included.
    #[test]
    fn only_a_429_and_a_5xx_off_a_post_are_retried() {
        for verb in [Verb::Get, Verb::Put, Verb::Delete, Verb::Post] {
            assert!(retryable(verb, 429), "{verb:?}");
            for status in [400, 401, 404, 409] {
                assert!(!retryable(verb, status), "{verb:?} {status}");
            }
        }
        for verb in [Verb::Get, Verb::Put, Verb::Delete] {
            for status in [500, 502, 503, 599] {
                assert!(retryable(verb, status), "{verb:?} {status}");
            }
        }
        for status in [500, 502, 503] {
            assert!(!retryable(Verb::Post, status), "POST {status}");
        }
        assert_eq!(Verb::of("PUT"), Verb::Put);
        assert_eq!(Verb::of("anything else"), Verb::Get);
    }

    /// The wait a 429 asks is the largest of its `x-rate-limit-*-interval-ms` headers,
    /// whatever their case; other rate-limit headers and values that are no number name
    /// none.
    #[test]
    fn the_wait_is_the_largest_rate_limit_interval() {
        let headers = |pairs: &[(&str, &str)]| {
            let mut map = ureq::http::HeaderMap::new();
            for (name, value) in pairs {
                map.append(
                    ureq::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                    ureq::http::HeaderValue::from_str(value).unwrap(),
                );
            }
            map
        };
        assert_eq!(
            rate_wait(&headers(&[
                ("X-Rate-Limit-Api-Key-Interval-Ms", "10000"),
                ("x-rate-limit-api-token-interval-ms", "2000"),
                ("x-rate-limit-api-key-max", "300"),
                ("x-rate-limit-api-key-remaining", "0"),
            ])),
            Some(Duration::from_secs(10))
        );
        assert_eq!(
            rate_wait(&headers(&[("x-rate-limit-api-token-interval-ms", " 250 ")])),
            Some(Duration::from_millis(250))
        );
        assert_eq!(
            rate_wait(&headers(&[
                ("x-rate-limit-api-key-max", "300"),
                ("x-rate-limit-api-key-interval-ms", "soon"),
                ("retry-after", "3"),
            ])),
            None
        );
        assert_eq!(rate_wait(&headers(&[])), None);
    }

    /// Trello's words come out of a refusal's body whichever shape it has, and an empty
    /// body has none.
    #[test]
    fn trellos_words_are_read_off_a_refusals_body() {
        assert_eq!(
            answer_text("\"unauthorized permission requested\""),
            Some("unauthorized permission requested".to_string())
        );
        assert_eq!(
            answer_text(r#"{"message":"invalid id","error":"ERROR"}"#),
            Some("invalid id".to_string())
        );
        assert_eq!(
            answer_text(r#"{"error":"ERROR"}"#),
            Some(r#"{"error":"ERROR"}"#.to_string())
        );
        assert_eq!(
            answer_text("invalid value for idList\n"),
            Some("invalid value for idList".to_string())
        );
        assert_eq!(answer_text(""), None);
        assert_eq!(answer_text("  \n"), None);
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
