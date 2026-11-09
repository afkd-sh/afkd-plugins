//! The HTTP spine the Bot API client rides: the pooled agent, the call deadline every request
//! is clipped to, one POST that hands back the status and the body whatever the status was,
//! since the Bot API answers a refusal with a JSON body on a 4xx, and one capped GET of a
//! file's bytes.
//!
//! The agent and its expiry are `@afkd/linear`'s: idle past [`AGENT_MAX_IDLE`] or alive past
//! [`AGENT_MAX_AGE`], and the pool is rebuilt rather than kept warm forever. So is the
//! deadline: afkd ends the service when a call misses its reply, so while a call is under
//! way every request's global timeout is clipped to what is left of it, and once it is gone
//! no request is sent at all.
//!
//! **The bot's token is in every URL** (`/bot<token>/<method>`), so no failure says the
//! URL: [`transport_reason`] answers the variants that render it with a fixed string, and
//! the token is taken out of every other reason as well, since a reason is logged.

use std::io::Read;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::botapi::ApiError;
use crate::common::CALL_BUDGET;

/// How long a request waits to establish a connection before giving up. A wedged socket
/// must not park a call past afkd's deadline, so even the "healthy" budget is finite.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a request waits for the response (read/write) before giving up. An upload of
/// 50 MB rides this too.
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Idle longer than this and the pooled sockets are assumed stale.
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
/// [`AGENT_MAX_AGE`] however busy it has been.
fn agent_expired(created: Instant, last_used: Instant, now: Instant) -> bool {
    now.duration_since(last_used) > AGENT_MAX_IDLE || now.duration_since(created) > AGENT_MAX_AGE
}

/// One pooled, timeout-configured agent that reads a 4xx or 5xx as an answer, so its body
/// reaches the caller.
fn build_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_send_request(Some(READ_TIMEOUT))
        .timeout_send_body(Some(READ_TIMEOUT))
        .timeout_recv_response(Some(READ_TIMEOUT))
        .timeout_recv_body(Some(READ_TIMEOUT))
        .timeout_global(Some(CONNECT_TIMEOUT + READ_TIMEOUT))
        .max_idle_age(AGENT_MAX_IDLE)
        .http_status_as_error(false)
        .build()
        .new_agent()
}

/// Lock `m`, recovering the data from a poisoned lock rather than panicking.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// What a request came back with: its status, and its body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Reply {
    pub(crate) status: u16,
    pub(crate) body: Vec<u8>,
}

/// A timeout-configured HTTP client that keeps `secret` out of everything it says.
pub(crate) struct Http {
    secret: String,
    /// The current agent generation, built on first use.
    agent: Mutex<Option<Generation>>,
    /// The current call's deadline, `None` between calls.
    deadline: Mutex<Option<Instant>>,
}

impl Http {
    pub(crate) fn new(secret: impl Into<String>) -> Self {
        Self {
            secret: secret.into(),
            agent: Mutex::new(None),
            deadline: Mutex::new(None),
        }
    }

    /// Set (or, with `None`, clear) the current call's deadline, which every request sent
    /// until the next set is clipped to.
    pub(crate) fn set_deadline(&self, deadline: Option<Instant>) {
        *lock(&self.deadline) = deadline;
    }

    /// The global timeout the next request gets: `None` outside a call, else what is left of
    /// it, at most a request's own budget. A call with nothing left sends nothing.
    fn request_budget(&self) -> Result<Option<Duration>, ApiError> {
        let Some(deadline) = *lock(&self.deadline) else {
            return Ok(None);
        };
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(ApiError::Transport(format!(
                "the call's {}s budget ran out",
                CALL_BUDGET.as_secs()
            )));
        }
        Ok(Some(left.min(CONNECT_TIMEOUT + READ_TIMEOUT)))
    }

    /// The agent for the next request, rebuilt past either expiry bound and stamped as used.
    fn agent(&self) -> ureq::Agent {
        let mut slot = lock(&self.agent);
        let now = Instant::now();
        let generation = match slot.take() {
            Some(g) if !agent_expired(g.created, g.last_used, now) => g,
            _ => Generation {
                agent: build_agent(),
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

    /// A failure's reason, with neither the URL nor the secret in it.
    fn reason(&self, e: &impl std::fmt::Display) -> String {
        let reason = e.to_string();
        if self.secret.is_empty() {
            reason
        } else {
            reason.replace(&self.secret, "<token>")
        }
    }

    /// POST `body` to `url` under `content_type`, and read the reply whatever its status.
    /// Only a request that produced no response is an error.
    pub(crate) fn post(
        &self,
        url: &str,
        content_type: &str,
        body: &[u8],
    ) -> Result<Reply, ApiError> {
        let mut req = self.agent().post(url).header("Content-Type", content_type);
        if let Some(global) = self.request_budget()? {
            req = req.config().timeout_global(Some(global)).build();
        }
        let mut resp = req
            .send(body)
            .map_err(|e| ApiError::Transport(self.reason(&transport_reason(&e))))?;
        let status = resp.status().as_u16();
        let mut body = Vec::new();
        resp.body_mut()
            .as_reader()
            .read_to_end(&mut body)
            .map_err(|e| ApiError::Decode(self.reason(&e)))?;
        Ok(Reply { status, body })
    }

    /// GET `url` and read the reply's bytes whatever its status, up to `cap`: a body past it
    /// is refused, read no further.
    pub(crate) fn get_bytes(&self, url: &str, cap: u64) -> Result<Reply, ApiError> {
        let mut req = self.agent().get(url);
        if let Some(global) = self.request_budget()? {
            req = req.config().timeout_global(Some(global)).build();
        }
        let mut resp = req
            .call()
            .map_err(|e| ApiError::Transport(self.reason(&transport_reason(&e))))?;
        let mut body = Vec::new();
        resp.body_mut()
            .as_reader()
            .take(cap + 1)
            .read_to_end(&mut body)
            .map_err(|e| ApiError::Decode(self.reason(&e)))?;
        if body.len() as u64 > cap {
            return Err(ApiError::Decode(format!(
                "the file is over the {cap} bytes a bot may fetch"
            )));
        }
        Ok(Reply {
            status: resp.status().as_u16(),
            body,
        })
    }
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

    #[test]
    fn an_agent_expires_when_idle_or_old() {
        let born = Instant::now();
        let at = |secs: u64| born + Duration::from_secs(secs);
        assert!(!agent_expired(born, born, born));
        assert!(!agent_expired(born, at(0), at(60)));
        assert!(agent_expired(born, at(0), at(61)));
        assert!(!agent_expired(born, at(299), at(300)));
        assert!(agent_expired(born, at(300), at(301)));
    }

    /// Neither URL-bearing variant renders its URI, and the token is taken out of any reason
    /// that holds it.
    #[test]
    fn a_reason_never_says_the_url_or_the_token() {
        let token = "7712345678:AAHfiqksKZ8WmR2zSjiQ7_v4TMAKdiHm9T0";
        let uri = format!("http://127.0.0.1:1/bot{token}/sendMessage");
        for e in [
            ureq::Error::BadUri(uri.clone()),
            ureq::Error::RequireHttpsOnly(uri.clone()),
        ] {
            assert!(!transport_reason(&e).contains(token));
        }
        let http = Http::new(token);
        assert_eq!(
            http.reason(&format!("could not reach {uri}")),
            "could not reach http://127.0.0.1:1/bot<token>/sendMessage"
        );
    }

    /// A spent call deadline sends nothing, and says which budget ran out.
    #[test]
    fn a_spent_call_deadline_sends_nothing() {
        let http = Http::new("t");
        http.set_deadline(Some(Instant::now()));
        let e = http
            .post(
                "http://127.0.0.1:9/bott/sendMessage",
                "application/json",
                b"{}",
            )
            .unwrap_err();
        assert_eq!(
            e.to_string(),
            "Telegram is out of reach: the call's 45s budget ran out"
        );
    }
}
