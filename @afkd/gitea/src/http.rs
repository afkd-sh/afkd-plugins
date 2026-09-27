//! The HTTP request/response spine the Gitea client rides: the pooled agent,
//! absolute-URL construction off an already-resolved API root, the credential header, the
//! send-and-decode path, the JSON extraction helpers, and the status/transport/decode
//! error mapping.
//!
//! Ported from the parts of afkd's `afkd_forge::http` the Gitea client reaches. One
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
//! The spine never trims a configured base and never names an endpoint: the caller hands
//! it a finished API root and appends its own `.query(…)` pairs to the request
//! [`HttpClient::request`] hands back. The failure vocabulary is the caller's, through
//! [`HttpError`]. And a transport failure's reason is built URL-free
//! ([`transport_reason`]), so no request line ever reaches a diagnostic.

use std::marker::PhantomData;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::common::{lock, CALL_BUDGET};

/// How long a forge call waits to establish a connection before giving up. A wedged
/// socket must not park a `poll` past afkd's call deadline, so even the "healthy" budget
/// is finite.
pub(crate) const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a forge call waits for the response (read/write) before giving up.
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
pub(crate) enum ForgeRequest {
    /// A verb ureq builds without a request body: GET, DELETE.
    NoBody(ureq::RequestBuilder<ureq::typestate::WithoutBody>),
    /// A verb ureq builds with one: POST, PATCH.
    WithBody(ureq::RequestBuilder<ureq::typestate::WithBody>),
}

impl ForgeRequest {
    /// Append one `key=value` query pair, percent-encoded onto the request target.
    pub(crate) fn query(self, key: &str, value: &str) -> Self {
        match self {
            ForgeRequest::NoBody(r) => ForgeRequest::NoBody(r.query(key, value)),
            ForgeRequest::WithBody(r) => ForgeRequest::WithBody(r.query(key, value)),
        }
    }

    /// Set one header, verbatim — the credential stamp and `send_json`'s content type.
    fn header(self, name: &str, value: &str) -> Self {
        match self {
            ForgeRequest::NoBody(r) => ForgeRequest::NoBody(r.header(name, value)),
            ForgeRequest::WithBody(r) => ForgeRequest::WithBody(r.header(name, value)),
        }
    }

    /// Clip the request's global timeout to `global`, when the call has a deadline. The
    /// agent's per-op timeouts still apply underneath.
    fn clip(self, global: Option<Duration>) -> Self {
        let Some(d) = global else { return self };
        match self {
            ForgeRequest::NoBody(r) => {
                ForgeRequest::NoBody(r.config().timeout_global(Some(d)).build())
            }
            ForgeRequest::WithBody(r) => {
                ForgeRequest::WithBody(r.config().timeout_global(Some(d)).build())
            }
        }
    }

    /// Send the request with no body, whichever typestate it holds.
    fn call(self) -> Result<ureq::http::Response<ureq::Body>, ureq::Error> {
        match self {
            ForgeRequest::NoBody(r) => r.call(),
            ForgeRequest::WithBody(r) => r.send_empty(),
        }
    }

    /// Send the request carrying `body`. A bodyless verb is forced to send one rather
    /// than refused, so the seam is total; no call site reaches that arm.
    fn send_body(self, body: &str) -> Result<ureq::http::Response<ureq::Body>, ureq::Error> {
        match self {
            ForgeRequest::NoBody(r) => r.force_send_body().send(body),
            ForgeRequest::WithBody(r) => r.send(body),
        }
    }
}

/// The failure vocabulary a caller's error type supplies so the spine can report a fault
/// without naming one: three constructors tagged with the **stage** of work the call
/// belonged to, plus the JSON extraction helpers built on them.
pub(crate) trait HttpError: Sized {
    /// The forge answered with a non-success HTTP status.
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

/// A timeout-configured HTTP client bound to one API root, one credential header, and one
/// caller error type `E`.
pub(crate) struct HttpClient<E> {
    base: String,
    auth: (String, String),
    connect: Duration,
    read: Duration,
    /// The current agent generation, built on first use.
    agent: Mutex<Option<Generation>>,
    /// The current call's deadline, `None` between calls.
    deadline: Mutex<Option<Instant>>,
    _err: PhantomData<fn() -> E>,
}

impl<E: HttpError> HttpClient<E> {
    /// A client against an **already-resolved** API root `base` (nothing here trims or
    /// suffixes it), stamping the `auth` header on every request, with explicit
    /// connect/read timeouts. The `read` budget bounds the write side too.
    pub(crate) fn new(
        base: impl Into<String>,
        auth: (String, String),
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
            _err: PhantomData,
        }
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

    /// A `method` request for the base-relative `path`, with the credential already
    /// stamped, handed back unexecuted so the caller appends its own query pairs.
    pub(crate) fn request(&self, method: &str, path: &str) -> ForgeRequest {
        let agent = self.agent();
        let url = format!("{}{path}", self.base);
        let req = match method {
            "POST" => ForgeRequest::WithBody(agent.post(&url)),
            "PATCH" => ForgeRequest::WithBody(agent.patch(&url)),
            "DELETE" => ForgeRequest::NoBody(agent.delete(&url)),
            // GET, and — deliberately, rather than by panic — any other verb. The
            // vocabulary is a literal at every call site.
            _ => ForgeRequest::NoBody(agent.get(&url)),
        };
        req.header(&self.auth.0, &self.auth.1)
    }

    /// GET `path` with the credential, returning the body text.
    pub(crate) fn get(&self, stage: &'static str, path: &str) -> Result<String, E> {
        self.send(stage, self.request("GET", path))
    }

    /// Execute a built request, mapping ureq's outcome onto an `E` and reading the body.
    pub(crate) fn send(&self, stage: &'static str, req: ForgeRequest) -> Result<String, E> {
        let req = req.clip(self.request_budget(stage)?);
        map_outcome(stage, req.call())
    }

    /// Send a JSON `payload` as the request body with the right content type.
    pub(crate) fn send_json(
        &self,
        stage: &'static str,
        req: ForgeRequest,
        payload: &Value,
    ) -> Result<String, E> {
        let result = req
            .clip(self.request_budget(stage)?)
            .header("Content-Type", "application/json")
            .send_body(&payload.to_string());
        map_outcome(stage, result)
    }
}

/// Map ureq's request outcome onto the caller's error vocabulary, reading a successful
/// response to text.
fn map_outcome<E: HttpError>(
    stage: &'static str,
    result: Result<ureq::http::Response<ureq::Body>, ureq::Error>,
) -> Result<String, E> {
    match result {
        Ok(mut resp) => resp
            .body_mut()
            .read_to_string()
            // A premature EOF mid-body is a decode failure: the status line arrived.
            .map_err(|e| E::decode(stage, &transport_reason(&e))),
        Err(ureq::Error::StatusCode(status)) => Err(E::status(stage, status)),
        // Everything else ureq can raise is a request that produced no response.
        // `ureq::Error` is `#[non_exhaustive]`, so this arm is required as well as correct.
        Err(other) => Err(E::transport(stage, transport_reason(&other))),
    }
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

    /// Neither URL-bearing variant renders its URI, whatever it carries.
    #[test]
    fn a_transport_reason_never_renders_the_uri() {
        let uri = "http://127.0.0.1:1/api/v1/user?token=SUPERSECRET";
        let bad = ureq::Error::BadUri(uri.to_string());
        let https = ureq::Error::RequireHttpsOnly(uri.to_string());
        for e in [bad, https] {
            let reason = transport_reason(&e);
            assert!(!reason.contains("SUPERSECRET"), "{reason}");
        }
    }

    /// The spine's failure vocabulary for these tests, so no vendor error type is needed.
    #[derive(Debug, PartialEq)]
    enum TestError {
        Status(u16),
        Transport { stage: &'static str, reason: String },
        Decode(String),
    }

    impl HttpError for TestError {
        fn status(_stage: &'static str, status: u16) -> Self {
            TestError::Status(status)
        }

        fn transport(stage: &'static str, reason: String) -> Self {
            TestError::Transport { stage, reason }
        }

        fn decode(_stage: &'static str, reason: &str) -> Self {
            TestError::Decode(reason.to_string())
        }
    }

    /// A client against `base` with the default connect and read budgets.
    fn client(base: String) -> HttpClient<TestError> {
        HttpClient::new(
            base,
            ("Authorization".into(), "token SUPERSECRET".into()),
            DEFAULT_CONNECT_TIMEOUT,
            DEFAULT_READ_TIMEOUT,
        )
    }

    /// A loopback server that accepts every connection and never answers, holding each
    /// open until the returned sender is dropped or 10 seconds pass.
    fn silent() -> (String, std::sync::mpsc::Sender<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            let until = Instant::now() + Duration::from_secs(10);
            let mut held = Vec::new();
            while Instant::now() < until
                && rx.try_recv() != Err(std::sync::mpsc::TryRecvError::Disconnected)
            {
                if let Ok((stream, _)) = listener.accept() {
                    held.push(stream);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        (base, tx)
    }

    /// A loopback server that answers one request `200 OK` with `body`.
    fn one_shot(body: &'static str) -> String {
        use std::io::{BufRead, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut reader = std::io::BufReader::new(stream.try_clone().expect("clone"));
            let mut line = String::new();
            while reader.read_line(&mut line).is_ok_and(|n| n > 0) && line != "\r\n" {
                line.clear();
            }
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .expect("write reply");
        });
        base
    }

    /// Under the default 35-second request budget, a silent peer is cut off at the call's
    /// deadline, a second away, on both builder typestates — and the failure is ureq's own
    /// timeout, as URL-free as any other transport reason.
    #[test]
    fn a_call_deadline_clips_a_request_to_what_is_left() {
        let (base, _hold) = silent();
        let http = client(base);
        let body = serde_json::json!({"body": "[afkd-claim] 修复 — ✅"});
        for (stage, via) in [("read", "get"), ("post", "send_json")] {
            http.set_deadline(Some(Instant::now() + Duration::from_secs(1)));
            let start = Instant::now();
            let err = match via {
                "get" => http.get(stage, "/repos/acme/widgets/issues").unwrap_err(),
                _ => http
                    .send_json(stage, http.request("POST", "/repos/acme/widgets"), &body)
                    .unwrap_err(),
            };
            assert!(
                start.elapsed() < Duration::from_secs(2),
                "`{via}` should time out near the call's deadline, took {:?}",
                start.elapsed()
            );
            let TestError::Transport { stage: s, reason } = &err else {
                panic!("`{via}`: expected a transport error, got {err:?}");
            };
            assert_eq!(*s, stage);
            assert!(reason.contains("timeout"), "`{via}`: {reason}");
            assert!(!reason.contains("127.0.0.1"), "`{via}`: {reason}");
        }
    }

    /// A call with nothing left sends nothing, on either path: the failure names the
    /// budget, and no connection reaches the peer.
    #[test]
    fn a_spent_call_deadline_sends_nothing() {
        // A listener nobody accepts on: a connection would wait in its queue.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        listener.set_nonblocking(true).unwrap();
        let http = client(format!(
            "http://127.0.0.1:{}",
            listener.local_addr().unwrap().port()
        ));
        http.set_deadline(Some(Instant::now()));

        let spent = |stage| TestError::Transport {
            stage,
            reason: "the call's 45s budget ran out".into(),
        };
        assert_eq!(http.get("read", "/user"), Err(spent("read")));
        assert_eq!(
            http.send_json(
                "post",
                http.request("POST", "/repos/acme/widgets"),
                &serde_json::json!({})
            ),
            Err(spent("post"))
        );
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "no request may reach the forge past the call's budget"
        );
    }

    /// A deadline further off than the request's own budget leaves the request that
    /// budget, and no deadline leaves ureq's agent-wide one untouched.
    #[test]
    fn a_far_call_deadline_leaves_a_request_its_own_budget() {
        let http = client("http://127.0.0.1:1".into());
        assert_eq!(http.request_budget("read"), Ok(None));
        http.set_deadline(Some(Instant::now() + Duration::from_secs(3600)));
        assert_eq!(
            http.request_budget("read"),
            Ok(Some(DEFAULT_CONNECT_TIMEOUT + DEFAULT_READ_TIMEOUT))
        );
    }

    /// A cleared deadline is not sticky: the next request sends as it always did.
    #[test]
    fn a_cleared_call_deadline_is_not_sticky() {
        let http = client(one_shot("[]"));
        http.set_deadline(Some(Instant::now()));
        http.set_deadline(None);
        assert_eq!(http.get("read", "/user"), Ok("[]".to_string()));
    }
}
