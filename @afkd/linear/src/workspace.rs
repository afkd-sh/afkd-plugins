//! Reaching a workspace: the `linear.Workspace` a read is passed, the client it reads Linear
//! through, and the keys of the handles it hands back.
//!
//! A `linear.Workspace` is the credential a read reaches Linear with, and nothing else is:
//! no read takes a service's settings, and two workspaces are two values used side by side. A
//! handle a read hands back is keyed `linear:<organization uuid>/<object uuid>`
//! ([`read_key`]), so a read passed one checks it is of the workspace it reaches. The uuid
//! is not what a person knows a workspace by, so every read records its workspace's url key
//! in the plugin's own directory ([`record_organization`]), and a refusal names both url keys
//! ([`other_workspace`]).

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use serde_json::Value;

use crate::auth::Auth;
use crate::client::{LinearClient, LINEAR_BASE};
use crate::common::{file_safe, write_whole};
use crate::settings::{credential, AuthConfig, CredentialProblem};
use crate::tracker::{Organization, Tracker};

/// The prefix of every key a read's handle carries.
const KEY_PREFIX: &str = "linear:";

/// A `linear.Workspace`: the credential, and the API root it is sent to.
///
/// `Debug` is **hand-written** so no secret reaches a diagnostic.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Workspace {
    pub(crate) auth: AuthConfig,
    pub(crate) base_url: String,
}

impl std::fmt::Debug for Workspace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Workspace")
            .field("auth", &self.auth)
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl Workspace {
    /// The workspace `ws`, `action`'s argument of that name, writes: exactly one of an API key
    /// or an app's id and secret, an empty string as unset as `nil` is, and the API root,
    /// Linear's when it names none.
    ///
    /// # Errors
    ///
    /// Returns the sentence that refuses it: an argument that is no `linear.Workspace`, or one
    /// that sets no credential, both forms of it, or half an app's.
    pub(crate) fn from_arg(action: &str, ws: Option<&Value>) -> Result<Self, String> {
        let not_one = || format!("{action}: parameter ws must be a linear.Workspace");
        let Some(Value::Object(ws)) = ws else {
            return Err(not_one());
        };
        let field = |name: &str| match ws.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(text)) if text.is_empty() => Ok(None),
            Some(Value::String(text)) => Ok(Some(text.clone())),
            Some(_) => Err(not_one()),
        };
        let auth = credential(
            field("api_key")?,
            field("client_id")?,
            field("client_secret")?,
        )
        .map_err(|problem| {
            let sets = format!("{action}: the linear.Workspace sets");
            match problem {
                CredentialProblem::Missing => format!(
                    "{action}: the linear.Workspace sets no credential; set api_key, or \
                         client_id and client_secret"
                ),
                CredentialProblem::Doubled {
                    client_id,
                    client_secret,
                } => {
                    let beside = match (client_id, client_secret) {
                        (true, true) => "client_id and client_secret",
                        (true, false) => "client_id",
                        _ => "client_secret",
                    };
                    format!("{sets} api_key beside {beside}; set one or the other")
                }
                CredentialProblem::IdWithoutSecret => {
                    format!("{sets} client_id without client_secret")
                }
                CredentialProblem::SecretWithoutId => {
                    format!("{sets} client_secret without client_id")
                }
            }
        })?;
        Ok(Workspace {
            auth,
            base_url: field("base_url")?.unwrap_or_else(|| LINEAR_BASE.to_string()),
        })
    }

    /// A client reaching this workspace for one call: retrying what Linear could not take
    /// within `deadline`, by which every request must be done, and an app's token kept in
    /// `state_dir`, the plugin's own directory, when afkd sent one.
    pub(crate) fn client(&self, state_dir: Option<&Path>, deadline: Instant) -> LinearClient {
        let auth = Arc::new(Auth::cached(&self.auth, state_dir));
        let client = LinearClient::new(&self.base_url, auth).retrying();
        client.set_call_deadline(Some(deadline));
        client
    }
}

/// The key of a handle a read hands back: `linear:<organization uuid>/<object uuid>`, the
/// same however the object is renamed or moved, and the same in every read that hands it.
pub(crate) fn read_key(organization: &str, uuid: &str) -> String {
    format!("{KEY_PREFIX}{organization}/{uuid}")
}

/// The organization's and the object's uuids a [`read_key`] holds, or `None` for any other
/// key: a claimed issue's, one without the `/`, or one with either part empty.
pub(crate) fn parse_read_key(key: &str) -> Option<(&str, &str)> {
    let (organization, uuid) = key.strip_prefix(KEY_PREFIX)?.split_once('/')?;
    (!organization.is_empty() && !uuid.is_empty()).then_some((organization, uuid))
}

/// The file in the plugin's directory that records the url key of the organization `id`.
fn record_file(state_dir: &Path, id: &str) -> std::path::PathBuf {
    state_dir.join(format!("organization-{}.txt", file_safe(id)))
}

/// Record `organization`'s url key in `state_dir`, the plugin's own directory, unless it is
/// recorded already: whether it wrote the record.
///
/// # Errors
///
/// Returns the I/O error of a record that could not be written.
pub(crate) fn record_organization(
    state_dir: &Path,
    organization: &Organization,
) -> std::io::Result<bool> {
    if recorded_url_key(state_dir, &organization.id).as_deref() == Some(&organization.url_key) {
        return Ok(false);
    }
    write_whole(
        &record_file(state_dir, &organization.id),
        organization.url_key.as_bytes(),
        0o644,
    )?;
    Ok(true)
}

/// The url key `state_dir` records for the organization `id`, if it records one.
pub(crate) fn recorded_url_key(state_dir: &Path, id: &str) -> Option<String> {
    std::fs::read_to_string(record_file(state_dir, id))
        .ok()
        .filter(|key| !key.is_empty())
}

/// The sentence refusing `action`'s argument `param`, the handle `id` of the organization
/// `handle_org`, whose url key is `handle_url_key` when it is known, when the
/// `linear.Workspace` reaches the workspace whose url key is `reaches`.
pub(crate) fn other_workspace(
    action: &str,
    param: &str,
    id: &str,
    handle_org: &str,
    handle_url_key: Option<&str>,
    reaches: &str,
) -> String {
    let of = match handle_url_key {
        Some(url_key) => format!("workspace {url_key}"),
        None => format!("the workspace whose organization is {handle_org}"),
    };
    format!("{action}: {param} is {id} of {of}, and the linear.Workspace reaches {reaches}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::TempDir;
    use serde_json::json;

    /// Every combination of the three credential fields reads as exactly one credential or
    /// as the sentence naming the fields: an empty string is as unset as `nil` or a field
    /// left out, and the API root is Linear's unless the workspace names one.
    #[test]
    fn a_workspace_holds_exactly_one_credential() {
        let read = |ws: Value| Workspace::from_arg("issues", Some(&ws));
        assert_eq!(
            read(
                json!({"api_key": "lin_api_k", "client_id": null, "client_secret": "",
                        "base_url": "https://api.linear.app"})
            ),
            Ok(Workspace {
                auth: AuthConfig::ApiKey("lin_api_k".into()),
                base_url: "https://api.linear.app".into(),
            })
        );
        assert_eq!(
            read(json!({"client_id": "app-id 監視", "client_secret": "s&=/", "api_key": ""}))
                .map(|ws| (ws.auth, ws.base_url)),
            Ok((
                AuthConfig::App {
                    client_id: "app-id 監視".into(),
                    client_secret: "s&=/".into()
                },
                LINEAR_BASE.to_string()
            ))
        );
        assert_eq!(
            read(json!({"api_key": "k", "base_url": "http://127.0.0.1:9"})).map(|ws| ws.base_url),
            Ok("http://127.0.0.1:9".to_string())
        );
        let none = "issues: the linear.Workspace sets no credential; set api_key, or client_id \
                    and client_secret";
        for (ws, refusal) in [
            (
                json!({"api_key": null, "client_id": null, "client_secret": null}),
                none,
            ),
            (
                json!({"api_key": "", "client_id": "", "client_secret": ""}),
                none,
            ),
            (json!({}), none),
            (
                json!({"api_key": "k", "client_id": "i", "client_secret": "s"}),
                "issues: the linear.Workspace sets api_key beside client_id and client_secret; \
                 set one or the other",
            ),
            (
                json!({"api_key": "k", "client_id": "i"}),
                "issues: the linear.Workspace sets api_key beside client_id; set one or the \
                 other",
            ),
            (
                json!({"api_key": "k", "client_secret": "s", "client_id": ""}),
                "issues: the linear.Workspace sets api_key beside client_secret; set one or the \
                 other",
            ),
            (
                json!({"client_id": "i", "client_secret": null}),
                "issues: the linear.Workspace sets client_id without client_secret",
            ),
            (
                json!({"client_secret": "s"}),
                "issues: the linear.Workspace sets client_secret without client_id",
            ),
            (
                json!({"api_key": 7}),
                "issues: parameter ws must be a linear.Workspace",
            ),
            (
                json!("lin_api_k"),
                "issues: parameter ws must be a linear.Workspace",
            ),
        ] {
            assert_eq!(read(ws.clone()), Err(refusal.to_string()), "{ws}");
        }
        assert_eq!(
            Workspace::from_arg("teams", None),
            Err("teams: parameter ws must be a linear.Workspace".to_string())
        );
    }

    /// A read's key reads back as the two uuids it was made of; a claimed issue's key, one
    /// without its `/`, and one with an empty part are no read's key.
    #[test]
    fn a_read_key_round_trips() {
        let org = "6f5e4d3c-2b1a-4098-8f7e-6d5c4b3a2918";
        let uuid = "0f6c1d2e-3b4a-4c5d-8e9f-a0b1c2d3e4f5";
        let key = read_key(org, uuid);
        assert_eq!(key, format!("linear:{org}/{uuid}"));
        assert_eq!(parse_read_key(&key), Some((org, uuid)));
        for other in [
            format!("{uuid}#5e2a9c71-0d4b-4f3e-9a8c-7b6d5e4f3a21"),
            format!("linear:{org}"),
            format!("linear:/{uuid}"),
            format!("linear:{org}/"),
            format!("tracker:{org}/{uuid}"),
            String::new(),
        ] {
            assert_eq!(parse_read_key(&other), None, "{other}");
        }
    }

    /// An organization's url key is recorded once, read back, and recorded again only when it
    /// changes; an organization never recorded reads as none.
    #[test]
    fn an_organization_is_recorded_once_and_read_back() {
        let dir = TempDir::new("organization");
        let acme = Organization {
            id: "6f5e4d3c-2b1a-4098-8f7e-6d5c4b3a2918".into(),
            url_key: "acme".into(),
        };
        assert_eq!(recorded_url_key(dir.path(), &acme.id), None);
        assert!(record_organization(dir.path(), &acme).unwrap());
        assert_eq!(
            recorded_url_key(dir.path(), &acme.id).as_deref(),
            Some("acme")
        );
        assert!(
            !record_organization(dir.path(), &acme).unwrap(),
            "not rewritten unchanged"
        );
        let renamed = Organization {
            url_key: "acme-corp".into(),
            ..acme.clone()
        };
        assert!(record_organization(dir.path(), &renamed).unwrap());
        assert_eq!(
            recorded_url_key(dir.path(), &acme.id).as_deref(),
            Some("acme-corp")
        );
        assert!(dir
            .path()
            .join("organization-6f5e4d3c-2b1a-4098-8f7e-6d5c4b3a2918.txt")
            .is_file());
        assert_eq!(recorded_url_key(dir.path(), "../x"), None);
    }

    /// The refusal of another workspace's handle names both url keys, or the organization's
    /// uuid when no read recorded its url key.
    #[test]
    fn another_workspaces_handle_is_refused_naming_both() {
        assert_eq!(
            other_workspace(
                "comments",
                "issue",
                "ACME-101",
                "org-a",
                Some("acme"),
                "globex"
            ),
            "comments: issue is ACME-101 of workspace acme, and the linear.Workspace reaches \
             globex"
        );
        assert_eq!(
            other_workspace("files", "issue", "ENG-7", "org-a", None, "globex"),
            "files: issue is ENG-7 of the workspace whose organization is org-a, and the \
             linear.Workspace reaches globex"
        );
    }

    /// No secret reaches `Debug`: neither an API key, nor an app's secret.
    #[test]
    fn debug_prints_no_secret() {
        for ws in [
            json!({"api_key": "lin_api_SECRET"}),
            json!({"client_id": "app-id-7f3a", "client_secret": "SECRET-9c1e"}),
        ] {
            let shown = format!("{:?}", Workspace::from_arg("me", Some(&ws)).unwrap());
            assert!(!shown.contains("SECRET"), "{shown}");
            assert!(shown.contains("https://api.linear.app"), "{shown}");
        }
    }
}
