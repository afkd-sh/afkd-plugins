//! The **account** a read reaches Trello through: `trello.Account`, the struct a config
//! writes (`trello.Account{ api_key: …, token: … }`) and passes every call first.
//!
//! A read reaches Trello only through the account it is passed: it reads no service's
//! settings, and the kind's own client is never used for it. So it answers the same in a
//! child afkd starts for the one call as in a `service(trello)`'s trigger child, and a call
//! may be made with two accounts side by side.
//!
//! An account is its credential and the address it is sent to; the client built from one
//! ([`Account::client`]) is a retrying [`TrelloClient`], which sends the credential only to
//! the base's origin and, for a download, to trello.com. A trigger child keeps the clients it
//! has built ([`Clients`]), so `"me"` is resolved once per account for the child's life:
//! Trello allows only 100 requests to `/1/members` per 900 seconds.

use std::fmt;
use std::time::Instant;

use serde_json::Value;

use crate::board::BoardClient;
use crate::client::{TrelloClient, TRELLO_BASE};

/// The most accounts a child keeps a client for; the one longest unused makes room.
const CLIENTS_MAX: usize = 8;

/// The credentials a read reaches Trello with, and where it sends them.
///
/// `Debug` is **hand-written**, so neither secret ever reaches a diagnostic.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Account {
    /// The Trello app's key.
    api_key: String,
    /// The token the calls act with.
    token: String,
    /// The API root; a stand-in's address in tests.
    base_url: String,
}

impl fmt::Debug for Account {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let redact = |s: &str| if s.is_empty() { "<unset>" } else { "<set>" };
        f.debug_struct("Account")
            .field("api_key", &redact(&self.api_key))
            .field("token", &redact(&self.token))
            .field("base_url", &self.base_url)
            .finish()
    }
}

impl Account {
    /// The account `action`'s argument `acct` holds, as afkd binds a struct: an object with
    /// `api_key`, `token` and `base_url`, the last defaulting to Trello's own.
    ///
    /// # Errors
    ///
    /// Returns the sentence refusing an argument that is missing or no struct, a field of
    /// another type, an empty `api_key` or `token` — each naming the field — and an empty
    /// `base_url`.
    pub(crate) fn from_arg(action: &str, arg: Option<&Value>) -> Result<Self, String> {
        let Some(arg) = arg else {
            return Err(format!("{action}: parameter acct is required"));
        };
        let Some(fields) = arg.as_object() else {
            return Err(format!(
                "{action}: parameter acct must be a trello.Account struct"
            ));
        };
        let field = |name: &str| match fields.get(name) {
            Some(Value::String(value)) => Ok(Some(value.clone())),
            None | Some(Value::Null) => Ok(None),
            Some(_) => Err(format!("{action}: acct.{name} must be a string")),
        };
        let required = |name: &str| match field(name)? {
            Some(value) if !value.is_empty() => Ok(value),
            _ => Err(format!("{action}: acct.{name} is empty")),
        };
        let (api_key, token) = (required("api_key")?, required("token")?);
        let base_url = match field("base_url")? {
            Some(url) if url.is_empty() => {
                return Err(format!("{action}: acct.base_url is empty"));
            }
            Some(url) => url,
            None => TRELLO_BASE.to_string(),
        };
        Ok(Self {
            api_key,
            token,
            base_url,
        })
    }

    /// A retrying client for this account, whose requests all end by `deadline`.
    pub(crate) fn client(&self, deadline: Instant) -> TrelloClient {
        let client = TrelloClient::with_base(&self.base_url, &self.api_key, &self.token).retrying();
        client.set_call_deadline(Some(deadline));
        client
    }
}

/// The clients a child has built for the accounts it was called with, for as long as it
/// lives: the first call with an account builds its client, and each later one reuses it,
/// with what it has memoized.
#[derive(Default)]
pub(crate) struct Clients {
    /// Least recently used first. Boxed, so a client keeps its address as the list is
    /// reordered.
    entries: Vec<(Account, Box<TrelloClient>)>,
}

impl Clients {
    /// The client for `account`, built on first use, its deadline set to `deadline`.
    pub(crate) fn client(&mut self, account: &Account, deadline: Instant) -> &TrelloClient {
        let entry = match self.entries.iter().position(|(held, _)| held == account) {
            Some(at) => self.entries.remove(at),
            None => {
                if self.entries.len() == CLIENTS_MAX {
                    self.entries.remove(0);
                }
                (account.clone(), Box::new(account.client(deadline)))
            }
        };
        self.entries.push(entry);
        let (_, client) = &self.entries[self.entries.len() - 1];
        client.set_call_deadline(Some(deadline));
        client
    }

    /// Clear every client's deadline, once the call is answered, so none inherits it.
    pub(crate) fn clear_deadlines(&self) {
        for (_, client) in &self.entries {
            client.set_call_deadline(None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn account(extra: Value) -> Value {
        let mut fields = json!({"api_key": "KEY-SECRET-1", "token": "TOKEN-SECRET-2"});
        fields
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        fields
    }

    /// A struct with both secrets reads; `base_url` is Trello's own unless named, and a
    /// `null` one reads as left out.
    #[test]
    fn an_account_reads_its_fields_and_defaults_the_base() {
        let acct = Account::from_arg("cards", Some(&account(json!({})))).unwrap();
        assert_eq!(
            (
                acct.api_key.as_str(),
                acct.token.as_str(),
                acct.base_url.as_str()
            ),
            ("KEY-SECRET-1", "TOKEN-SECRET-2", "https://api.trello.com/1")
        );
        let acct = Account::from_arg(
            "cards",
            Some(&account(json!({"base_url": "http://127.0.0.1:9/1"}))),
        )
        .unwrap();
        assert_eq!(acct.base_url, "http://127.0.0.1:9/1");
        let acct = Account::from_arg("cards", Some(&account(json!({"base_url": null})))).unwrap();
        assert_eq!(acct.base_url, TRELLO_BASE);
    }

    /// Each refusal says which call and which field: a missing or non-struct account, an
    /// empty or missing key or token, a field of another type and an empty base.
    #[test]
    fn an_account_refuses_what_it_cannot_reach_trello_with() {
        for (arg, refusal) in [
            (None, "cards: parameter acct is required"),
            (
                Some(json!("trello")),
                "cards: parameter acct must be a trello.Account struct",
            ),
            (
                Some(json!([1])),
                "cards: parameter acct must be a trello.Account struct",
            ),
            (
                Some(account(json!({"api_key": ""}))),
                "cards: acct.api_key is empty",
            ),
            (
                Some(account(json!({"token": ""}))),
                "cards: acct.token is empty",
            ),
            (Some(json!({"token": "t"})), "cards: acct.api_key is empty"),
            (
                Some(json!({"api_key": "k", "token": null})),
                "cards: acct.token is empty",
            ),
            (
                Some(account(json!({"token": 7}))),
                "cards: acct.token must be a string",
            ),
            (
                Some(account(json!({"base_url": ""}))),
                "cards: acct.base_url is empty",
            ),
            (
                Some(account(json!({"base_url": false}))),
                "cards: acct.base_url must be a string",
            ),
        ] {
            assert_eq!(
                Account::from_arg("cards", arg.as_ref()).map(|_| ()),
                Err(refusal.to_string()),
                "{arg:?}"
            );
        }
    }

    /// The `Debug` form shows that a secret is set, never the secret.
    #[test]
    fn debug_hides_both_secrets() {
        let acct = Account::from_arg("cards", Some(&account(json!({})))).unwrap();
        let shown = format!("{acct:?} {:#?}", Some(&acct));
        assert!(!shown.contains("SECRET"), "{shown}");
        assert!(shown.contains("api_key: \"<set>\""), "{shown}");
        assert!(shown.contains("token: \"<set>\""), "{shown}");
        assert!(shown.contains("https://api.trello.com/1"), "{shown}");
    }

    /// One client per account: asking again returns the same client, so what it memoized
    /// is kept, another account gets its own, and the least recently used makes room past
    /// [`CLIENTS_MAX`].
    #[test]
    fn a_client_is_kept_per_account() {
        let mut clients = Clients::default();
        let deadline = Instant::now() + std::time::Duration::from_secs(60);
        let acct = |n: usize| {
            Account::from_arg(
                "cards",
                Some(&json!({"api_key": format!("k{n}"), "token": "t"})),
            )
            .unwrap()
        };
        let first = std::ptr::from_ref(clients.client(&acct(0), deadline));
        let again = std::ptr::from_ref(clients.client(&acct(0), deadline));
        assert_eq!(first, again, "the same account, the same client");
        let other = std::ptr::from_ref(clients.client(&acct(1), deadline));
        assert_ne!(first, other);
        for n in 2..CLIENTS_MAX + 2 {
            clients.client(&acct(n), deadline);
        }
        assert_eq!(clients.entries.len(), CLIENTS_MAX);
        assert!(
            clients.entries.iter().all(|(held, _)| *held != acct(0)),
            "the longest unused went first"
        );
        clients.clear_deadlines();
    }
}
