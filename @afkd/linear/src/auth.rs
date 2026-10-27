//! The credential every request to Linear carries: an API key as it is, or an OAuth app's
//! own token, fetched with the `client_credentials` grant and fetched again before it
//! expires.
//!
//! An app token makes the app the actor, so its name shows on what it does, which is how
//! each bot runs as a Linear app of its own. Linear answers the grant with a token valid for
//! 30 days and no refresh token, and asks for a new one on a 401; so a token is renewed
//! ahead of its expiry ([`renew_after`]), and one Linear refuses all the same is dropped
//! ([`Auth::invalidate`]) and fetched again for one retry.
//!
//! The token is fetched on the first request that needs it, never at `hello`: an outage
//! when the service arms is an idle beat, not a refused service.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::common::lock;
use crate::http::{Http, Reply};
use crate::settings::AuthConfig;
use crate::tracker::LinearError;

/// The scopes an app token is asked for: read and write, and the two that let the app be
/// delegated an issue and mentioned. Fixed, since Linear revokes an app's tokens when it is
/// asked for different scopes.
pub(crate) const SCOPE: &str = "read,write,app:assignable,app:mentionable";

/// The most a token is renewed ahead of its expiry: an hour, for Linear's 30-day tokens.
pub(crate) const REFRESH_MARGIN_CAP: Duration = Duration::from_secs(3600);

/// Where Linear's token endpoint sits under the API root.
const TOKEN_PATH: &str = "/oauth/token";

/// The credential, ready to stamp on a request.
pub(crate) enum Auth {
    /// A personal API key, sent as the `Authorization` header with no `Bearer`.
    ApiKey(String),
    /// An app, which sends `Bearer` and a token of its own.
    App(AppAuth),
}

/// An OAuth app's credentials, and the token they last fetched.
pub(crate) struct AppAuth {
    client_id: String,
    client_secret: String,
    token: Mutex<Option<Token>>,
}

/// A fetched app token, and the moment it is renewed at.
struct Token {
    value: String,
    renew_at: Instant,
}

impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Auth::ApiKey(_) => f.debug_struct("ApiKey").finish_non_exhaustive(),
            Auth::App(app) => app.fmt(f),
        }
    }
}

impl std::fmt::Debug for AppAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppAuth")
            .field("client_id", &self.client_id)
            .field("holds_token", &lock(&self.token).is_some())
            .finish_non_exhaustive()
    }
}

impl Auth {
    /// The credential `cfg` writes, holding no token yet.
    pub(crate) fn new(cfg: &AuthConfig) -> Self {
        match cfg {
            AuthConfig::ApiKey(key) => Auth::ApiKey(key.clone()),
            AuthConfig::App {
                client_id,
                client_secret,
            } => Auth::App(AppAuth {
                client_id: client_id.clone(),
                client_secret: client_secret.clone(),
                token: Mutex::new(None),
            }),
        }
    }

    /// The `Authorization` header value for a request sent at `now`: the key as it is, or
    /// `Bearer` and the app's token — fetched through `http` when there is none, or when the
    /// one held is due for renewal.
    pub(crate) fn header(&self, http: &Http, now: Instant) -> Result<String, LinearError> {
        match self {
            Auth::ApiKey(key) => Ok(key.clone()),
            Auth::App(app) => Ok(format!("Bearer {}", app.token(http, now)?)),
        }
    }

    /// Drop the app's token, after Linear refused it: whether a retry with a fresh one can
    /// help, which an API key's refusal cannot.
    pub(crate) fn invalidate(&self) -> bool {
        match self {
            Auth::ApiKey(_) => false,
            Auth::App(app) => {
                *lock(&app.token) = None;
                true
            }
        }
    }
}

impl AppAuth {
    /// The token to send at `now`, fetching a new one when none is held or the one held is
    /// due. The lock is not held across the fetch, so a request on another thread is never
    /// parked behind it; two that race both fetch, and the later token is kept.
    fn token(&self, http: &Http, now: Instant) -> Result<String, LinearError> {
        if let Some(token) = lock(&self.token)
            .as_ref()
            .filter(|token| now < token.renew_at)
        {
            return Ok(token.value.clone());
        }
        let (value, expires_in) = self.fetch(http)?;
        *lock(&self.token) = Some(Token {
            value: value.clone(),
            renew_at: now + renew_after(expires_in),
        });
        Ok(value)
    }

    /// Ask the token endpoint for a token with the `client_credentials` grant: the token,
    /// and how long Linear says it lasts.
    fn fetch(&self, http: &Http) -> Result<(String, Duration), LinearError> {
        let stage = "fetch token";
        let form = form_encode(&[
            ("grant_type", "client_credentials"),
            ("scope", SCOPE),
            ("client_id", &self.client_id),
            ("client_secret", &self.client_secret),
        ]);
        let reply = http.post(
            stage,
            TOKEN_PATH,
            "application/x-www-form-urlencoded",
            None,
            form.as_bytes(),
        )?;
        parse_token(stage, &reply)
    }
}

/// How long after a fetch its token is renewed: an hour short of `expires_in`, or half of
/// it when that is less, so a short-lived token is still used for a while.
pub(crate) fn renew_after(expires_in: Duration) -> Duration {
    expires_in - REFRESH_MARGIN_CAP.min(expires_in / 2)
}

/// Read the token endpoint's reply: `access_token` and `expires_in` from a success, and
/// the OAuth `error` and its description from a refusal.
fn parse_token(stage: &'static str, reply: &Reply) -> Result<(String, Duration), LinearError> {
    let body: Value = serde_json::from_str(&reply.body).unwrap_or(Value::Null);
    if !(200..300).contains(&reply.status) {
        let Some(error) = body.get("error").and_then(Value::as_str) else {
            return Err(LinearError::Status {
                stage,
                status: reply.status,
                retry_after: reply.retry_after,
            });
        };
        let message = body
            .get("error_description")
            .and_then(Value::as_str)
            .unwrap_or(error);
        return Err(LinearError::Refused {
            stage,
            status: reply.status,
            message: message.to_string(),
            kind: error.to_string(),
            retry_after: reply.retry_after,
        });
    }
    let decode = |reason: &str| LinearError::Decode {
        stage,
        reason: reason.to_string(),
    };
    let token = body
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| decode("the token reply carries no access_token"))?;
    let expires_in = body
        .get("expires_in")
        .and_then(Value::as_u64)
        .ok_or_else(|| decode("the token reply carries no expires_in"))?;
    Ok((token.to_string(), Duration::from_secs(expires_in)))
}

/// `pairs` as an `application/x-www-form-urlencoded` body: each key and value
/// percent-encoded, a space as `+`, the pairs joined by `&`.
fn form_encode(pairs: &[(&str, &str)]) -> String {
    let encode = |s: &str| {
        let mut out = String::with_capacity(s.len());
        for b in s.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'*' => {
                    out.push(char::from(b))
                }
                b' ' => out.push('+'),
                b => out.push_str(&format!("%{b:02X}")),
            }
        }
        out
    };
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::{Clock, FakeClock};

    fn app() -> AppAuth {
        AppAuth {
            client_id: "app-id-7f3a".into(),
            client_secret: "SECRET-app-9c1e".into(),
            token: Mutex::new(None),
        }
    }

    fn reply(status: u16, body: &str) -> Reply {
        Reply {
            status,
            retry_after: None,
            body: body.to_string(),
        }
    }

    /// Linear's 30-day token is renewed an hour before it expires, and a 2-second one
    /// halfway; the renewal is never at or past the expiry.
    #[test]
    fn a_token_is_renewed_an_hour_early_or_halfway() {
        assert_eq!(
            renew_after(Duration::from_secs(2_591_999)),
            Duration::from_secs(2_591_999 - 3600)
        );
        assert_eq!(renew_after(Duration::from_secs(2)), Duration::from_secs(1));
        assert_eq!(
            renew_after(Duration::from_secs(7200)),
            Duration::from_secs(3600)
        );
        assert_eq!(renew_after(Duration::ZERO), Duration::ZERO);
    }

    /// A held token is sent until its renewal moment, measured on the clock the request is
    /// stamped with, and not from that moment on.
    #[test]
    fn a_held_token_is_sent_until_its_renewal_moment() {
        let clock = FakeClock::new();
        let app = app();
        let fetched = clock.now();
        *lock(&app.token) = Some(Token {
            value: "tok-1".into(),
            renew_at: fetched + renew_after(Duration::from_secs(2)),
        });
        // Nothing listens there: a fetch would fail, so an answer is the held token.
        let http = Http::new(
            "http://127.0.0.1:9",
            Duration::from_millis(200),
            Duration::from_millis(200),
        );
        clock.advance(Duration::from_millis(999));
        assert_eq!(app.token(&http, clock.now()).unwrap(), "tok-1");
        clock.advance(Duration::from_millis(1));
        let e = app.token(&http, clock.now()).unwrap_err();
        assert_eq!(e.stage(), "fetch token", "due, so it fetched: {e}");
    }

    /// A key is sent bare and never invalidated; an app's token is dropped on a refusal.
    #[test]
    fn a_key_is_sent_bare_and_only_an_apps_token_is_dropped() {
        let http = Http::new(
            "http://127.0.0.1:9",
            Duration::from_millis(200),
            Duration::from_millis(200),
        );
        let key = Auth::new(&AuthConfig::ApiKey("lin_api_k".into()));
        assert_eq!(key.header(&http, Instant::now()).unwrap(), "lin_api_k");
        assert!(!key.invalidate());
        let app = Auth::App(self::app());
        let Auth::App(inner) = &app else {
            unreachable!()
        };
        *lock(&inner.token) = Some(Token {
            value: "tok-1".into(),
            renew_at: Instant::now() + Duration::from_secs(60),
        });
        assert_eq!(app.header(&http, Instant::now()).unwrap(), "Bearer tok-1");
        assert!(app.invalidate());
        assert!(lock(&inner.token).is_none());
    }

    /// The token reply's success and its two kinds of refusal.
    #[test]
    fn the_token_reply_is_read_or_refused_in_linears_words() {
        assert_eq!(
            parse_token(
                "fetch token",
                &reply(
                    200,
                    r#"{"access_token":"lin_oauth_8f2c","token_type":"Bearer","expires_in":2591999,"scope":"read write app:assignable app:mentionable"}"#
                )
            )
            .unwrap(),
            ("lin_oauth_8f2c".to_string(), Duration::from_secs(2_591_999))
        );
        let e = parse_token(
            "fetch token",
            &reply(
                401,
                r#"{"error":"invalid_client","error_description":"Client authentication failed"}"#,
            ),
        )
        .unwrap_err();
        assert_eq!(
            e.to_string(),
            "linear fetch token: Linear refused it (401): Client authentication failed"
        );
        assert!(!e.transient());
        let e = parse_token("fetch token", &reply(503, "upstream down")).unwrap_err();
        assert!(e.transient(), "{e}");
        let e = parse_token("fetch token", &reply(200, r#"{"token_type":"Bearer"}"#)).unwrap_err();
        assert_eq!(
            e.to_string(),
            "linear fetch token: undecodable response (the token reply carries no access_token)"
        );
    }

    /// The form escapes what a secret may hold, and the scope's commas and colons.
    #[test]
    fn the_form_encodes_every_reserved_byte() {
        assert_eq!(
            form_encode(&[("scope", SCOPE), ("client_secret", "a b&c=d/é")]),
            "scope=read%2Cwrite%2Capp%3Aassignable%2Capp%3Amentionable\
             &client_secret=a+b%26c%3Dd%2F%C3%A9"
        );
    }

    /// No secret reaches `Debug`, and neither does a held token.
    #[test]
    fn debug_prints_no_secret() {
        let app = self::app();
        *lock(&app.token) = Some(Token {
            value: "lin_oauth_TOKEN".into(),
            renew_at: Instant::now(),
        });
        let shown = format!("{:?}", Auth::App(app));
        assert!(
            !shown.contains("SECRET") && !shown.contains("TOKEN"),
            "{shown}"
        );
        assert!(shown.contains("app-id-7f3a"), "{shown}");
        let key = format!("{:?}", Auth::ApiKey("lin_api_SECRET".into()));
        assert!(!key.contains("SECRET"), "{key}");
    }
}
