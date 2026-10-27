//! The HTTP spine the Linear client rides: the pooled agent, the call deadline every request
//! is clipped to, and one POST that hands back the status, the `Retry-After` and the body
//! whatever the status was, since Linear answers a refused request with a JSON `errors`
//! body on a 4xx as well as on a 200.
//!
//! The agent and its expiry are `@afkd/trello`'s: idle past [`AGENT_MAX_IDLE`] or alive
//! past [`AGENT_MAX_AGE`], and the pool is rebuilt rather than kept warm forever. So is the
//! deadline: afkd ends the service when a call misses its reply, so while a call is under
//! way every request's global timeout is clipped to what is left of [`CALL_BUDGET`], and
//! once it is gone no request is sent at all.
//!
//! The credential rides a header here, not the URL, but [`transport_reason`] stays URL-free
//! all the same: a failure line is logged, and a URL is no part of what it needs to say.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::common::{lock, CALL_BUDGET};
use crate::tracker::LinearError;

/// How long a request waits to establish a connection before giving up. A wedged socket
/// must not park a `poll` past afkd's call deadline, so even the "healthy" budget is finite.
pub(crate) const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a request waits for the response (read/write) before giving up.
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

/// Build one pooled, timeout-configured agent that reads a 4xx or 5xx as an answer, so its
/// body reaches the caller.
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
        .http_status_as_error(false)
        .build()
        .new_agent()
}

/// What a request came back with: its status, the wait a `Retry-After` header asked for,
/// and its body as text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Reply {
    pub(crate) status: u16,
    pub(crate) retry_after: Option<Duration>,
    pub(crate) body: String,
}

/// A timeout-configured HTTP client bound to one API root.
pub(crate) struct Http {
    base: String,
    connect: Duration,
    read: Duration,
    /// The current agent generation, built on first use.
    agent: Mutex<Option<Generation>>,
    /// The current call's deadline, `None` between calls.
    deadline: Mutex<Option<Instant>>,
}

impl Http {
    /// A client against the API root `base`, with explicit connect/read timeouts. The `read`
    /// budget bounds the write side too.
    pub(crate) fn new(base: impl Into<String>, connect: Duration, read: Duration) -> Self {
        Self {
            base: base.into(),
            connect,
            read,
            agent: Mutex::new(None),
            deadline: Mutex::new(None),
        }
    }

    /// The absolute URL of the base-relative `path`.
    pub(crate) fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// Set (or, with `None`, clear) the current call's deadline, which every request sent
    /// until the next set is clipped to. Measured on the real monotonic clock, the one the
    /// plugin stamps it with.
    pub(crate) fn set_deadline(&self, deadline: Option<Instant>) {
        *lock(&self.deadline) = deadline;
    }

    /// The global timeout the next request gets: `None` outside a call, else the request's
    /// own `connect + read` or what is left of the call, whichever is shorter. A call with
    /// nothing left sends nothing: that is a transport failure naming the budget.
    fn request_budget(&self, stage: &'static str) -> Result<Option<Duration>, LinearError> {
        let Some(deadline) = *lock(&self.deadline) else {
            return Ok(None);
        };
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(LinearError::Transport {
                stage,
                reason: format!("the call's {}s budget ran out", CALL_BUDGET.as_secs()),
            });
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

    /// POST `body` to the base-relative `path` under `content_type`, with `authorization` as
    /// its `Authorization` header when there is one, and read the reply whatever its status.
    /// Only a request that produced no response is an error, and a body cut off after the
    /// status line arrived is an undecodable one.
    pub(crate) fn post(
        &self,
        stage: &'static str,
        path: &str,
        content_type: &str,
        authorization: Option<&str>,
        body: &[u8],
    ) -> Result<Reply, LinearError> {
        let mut req = self
            .agent()
            .post(self.url(path))
            .header("Content-Type", content_type);
        if let Some(authorization) = authorization {
            req = req.header("Authorization", authorization);
        }
        if let Some(global) = self.request_budget(stage)? {
            req = req.config().timeout_global(Some(global)).build();
        }
        let mut resp = req.send(body).map_err(|e| LinearError::Transport {
            stage,
            reason: transport_reason(&e),
        })?;
        let status = resp.status().as_u16();
        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(parse_retry_after);
        let body = resp
            .body_mut()
            .read_to_string()
            .map_err(|e| LinearError::Decode {
                stage,
                reason: transport_reason(&e),
            })?;
        Ok(Reply {
            status,
            retry_after,
            body,
        })
    }
}

/// A `Retry-After` header's wait, in the delta-seconds form Linear sends. An HTTP date, or
/// anything else, reads as no wait asked for.
pub(crate) fn parse_retry_after(value: &str) -> Option<Duration> {
    value.trim().parse().ok().map(Duration::from_secs)
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
        let uri = "http://127.0.0.1:1/graphql?SUPERSECRET";
        let bad = ureq::Error::BadUri(uri.to_string());
        let https = ureq::Error::RequireHttpsOnly(uri.to_string());
        for e in [bad, https] {
            let reason = transport_reason(&e);
            assert!(!reason.contains("SUPERSECRET"), "{reason}");
        }
    }

    /// Delta-seconds are read, padded or not; a date or a negative or fractional number is
    /// no wait at all.
    #[test]
    fn retry_after_reads_delta_seconds_only() {
        assert_eq!(parse_retry_after("7"), Some(Duration::from_secs(7)));
        assert_eq!(parse_retry_after(" 60 "), Some(Duration::from_secs(60)));
        assert_eq!(parse_retry_after("0"), Some(Duration::ZERO));
        for other in ["Wed, 21 Oct 2026 07:28:00 GMT", "-1", "1.5", ""] {
            assert_eq!(parse_retry_after(other), None, "{other}");
        }
    }

    /// A spent call deadline sends nothing, and says which budget ran out.
    #[test]
    fn a_spent_call_deadline_sends_nothing() {
        let http = Http::new(
            "http://127.0.0.1:9",
            DEFAULT_CONNECT_TIMEOUT,
            DEFAULT_READ_TIMEOUT,
        );
        http.set_deadline(Some(Instant::now()));
        let e = http
            .post("post comment", "/graphql", "application/json", None, b"{}")
            .unwrap_err();
        assert_eq!(
            e.to_string(),
            "linear post comment: no response (the call's 45s budget ran out)"
        );
    }
}
