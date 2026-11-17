//! Read the `intake` kind's settings block — as afkd lowers it to JSON in `hello` — into a
//! typed [`IntakeConfig`].
//!
//! afkd has already typed the block against the manifest before this plugin is spawned:
//! every key is one the kind declares, `team` and `claim_from` are present, and each value is
//! of its declared type. What is left here is the part a manifest cannot say: a list that
//! names nothing, which credential is written, since exactly one of an API key or an app's
//! `client_id` and `client_secret` is, and an `only_*` filter `linear.issues` would refuse.
//! The cadence and the attempt bound (`every`, `max_attempts`) are afkd's and are not read
//! here, and the slots never arrive here at all.
//!
//! The lowering (afkd's `plugintrigger::settings_json`): a value is a string (an enum member
//! its name, a number its digits), `nil` is `null`, and a `list[…]` is always an array of
//! its items.

use serde_json::{json, Map, Value};

use crate::client::LINEAR_BASE;
use crate::reads::{issue_query, IssueQuery};

/// The member operand naming whoever the credential authenticates as: the API key's user, or
/// the app itself. No display name or email can collide with it in practice, and Linear has
/// no reserved word of its own, so this is the plugin's.
pub(crate) const ME: &str = "me";

/// The `assignee` and `delegate` filter operand for an issue nobody holds that way. It is
/// reserved there only, so `discuss_with` and a write's `delegate` still read it as a name.
pub(crate) const NONE: &str = "none";

/// Each `only_*` setting, and the `linear.issues` filter it is.
const ONLY: &[(&str, &str)] = &[
    ("only_state_type", "state_type"),
    ("only_priority", "priority"),
    ("only_labels", "labels"),
    ("only_assignee", "assignee"),
    ("only_delegate", "delegate"),
    ("only_project", "project"),
    ("only_milestone", "milestone"),
    ("only_cycle", "cycle"),
    ("only_parent", "parent"),
    ("only_text", "text"),
];

/// Who a `discuss_with` entry names: [`ME`], or a workspace user by display name or email.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MemberRef {
    /// Whoever the credential authenticates as, the `viewer`.
    Me,
    /// A user named by their display name or their email, whichever matches.
    Named(String),
}

/// Whose comment drives a `discuss_with` grooming issue: any non-afkd author, or a named
/// allow-list of users.
///
/// The reserved sole operand `anyone` selects any author but afkd's own. A mixed
/// `discuss_with anyone alice` reads as two names, and the literal `anyone` will fail to
/// resolve at poll time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DiscussWith {
    /// Any comment not authored by afkd itself drives the issue.
    Anyone,
    /// Only comments by these named users drive the issue.
    Members(Vec<MemberRef>),
}

/// The credential a service reaches Linear with.
///
/// `Debug` is **hand-written** so no secret reaches a diagnostic.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum AuthConfig {
    /// A personal API key, sent as the `Authorization` header as it is.
    ApiKey(String),
    /// An OAuth app's credentials, traded for the app's own token with the
    /// `client_credentials` grant.
    App {
        client_id: String,
        client_secret: String,
    },
}

impl std::fmt::Debug for AuthConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthConfig::ApiKey(_) => f.debug_struct("ApiKey").finish_non_exhaustive(),
            AuthConfig::App { client_id, .. } => f
                .debug_struct("App")
                .field("client_id", client_id)
                .finish_non_exhaustive(),
        }
    }
}

/// A validated `intake` trigger configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IntakeConfig {
    /// The keys of the teams issues are claimed from (`ENG`), in the order written; never
    /// empty, since the setting is required.
    pub(crate) teams: Vec<String>,
    /// API root the client is pointed at; defaults to Linear's. A testability seam, not a
    /// user-facing knob.
    pub(crate) base_url: String,
    /// The credential.
    pub(crate) auth: AuthConfig,
    /// The workflow states new issues are drawn from, by name, in the order written, in each
    /// team; never empty, since the setting is required.
    pub(crate) claim_from: Vec<String>,
    /// What an issue must pass to be claimed: the `linear.issues` filters the `only_*`
    /// settings and `skip_labels` name, and no open blocker.
    pub(crate) gate: IssueQuery,
    /// The labels no claimed issue carries, which the parked sweep holds a badged issue to
    /// as well.
    pub(crate) skip_labels: Vec<String>,
    /// The comment-driven grooming gate: when set, an issue is claimed only if its tail —
    /// the comments after afkd's own last comment — carries a comment from an allowed
    /// author.
    pub(crate) discuss_with: Option<DiscussWith>,
}

impl IntakeConfig {
    /// The `linear.Workspace` the service's credential settings build, as the slots are
    /// passed it: the credential the service reaches Linear with, every other credential
    /// field `nil`, and the API root.
    pub(crate) fn workspace(&self) -> Value {
        let (api_key, client_id, client_secret) = match &self.auth {
            AuthConfig::ApiKey(key) => (Some(key.as_str()), None, None),
            AuthConfig::App {
                client_id,
                client_secret,
            } => (None, Some(client_id.as_str()), Some(client_secret.as_str())),
        };
        json!({
            "api_key": api_key,
            "client_id": client_id,
            "client_secret": client_secret,
            "base_url": self.base_url,
        })
    }
}

/// A setting afkd let through that this kind cannot use: the key it is about, and the
/// problem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SettingsError {
    /// The setting the problem is about.
    pub(crate) key: String,
    /// What is wrong with it.
    pub(crate) problem: String,
}

impl SettingsError {
    pub(crate) fn new(key: &str, problem: impl Into<String>) -> Self {
        Self {
            key: key.to_string(),
            problem: problem.into(),
        }
    }
}

impl std::fmt::Display for SettingsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "setting `{}`: {}", self.key, self.problem)
    }
}

/// One setting as the manifest declares it: its name, its `type`, and its other keys
/// with their values as the manifest spells them (`("required", "true")`).
#[cfg(test)]
pub(crate) type Declared = (
    &'static str,
    &'static str,
    &'static [(&'static str, &'static str)],
);

/// The `intake` kind's settings as the manifest declares them, in its order: each name, its
/// `type`, and its other keys spelled as the manifest spells their values. afkd reads the
/// manifest, never this: it is what `manifest.rs`'s test holds `afkd-plugin.toml` to, so the
/// two cannot drift apart. afkd's own claim keys are not among them.
#[cfg(test)]
pub(crate) const SETTINGS: &[Declared] = &[
    ("api_key", "str?", &[]),
    ("client_id", "str?", &[]),
    ("client_secret", "str?", &[]),
    (
        "base_url",
        "str",
        &[("default", "\"https://api.linear.app\"")],
    ),
    ("team", "list[str]", &[("required", "true")]),
    ("claim_from", "list[str]", &[("required", "true")]),
    ("only_state_type", "list[State_Type]", &[]),
    ("only_priority", "list[Priority]", &[]),
    ("only_labels", "list[str]", &[]),
    ("only_assignee", "list[str]", &[]),
    ("only_delegate", "list[str]", &[]),
    ("only_project", "list[str]", &[]),
    ("only_milestone", "list[str]", &[]),
    ("only_cycle", "list[int]", &[]),
    ("only_parent", "list[str]", &[]),
    ("only_text", "str", &[]),
    ("skip_labels", "list[str]", &[]),
    ("discuss_with", "list[str]", &[]),
];

/// Read the lowered `settings` into an [`IntakeConfig`], or report the first setting it
/// cannot use.
pub(crate) fn intake_config(settings: &Value) -> Result<IntakeConfig, SettingsError> {
    let empty = Map::new();
    let settings = settings.as_object().unwrap_or(&empty);
    let skip_labels = nonempty_list(
        settings,
        "skip_labels",
        "`skip_labels` expects one or more label names",
    )?;
    Ok(IntakeConfig {
        // afkd refuses a service without either before any `hello`; these refuse a `hello`
        // that arrives without them anyway, so no armed service scans nothing.
        teams: nonempty_list(settings, "team", "`team` expects one or more team keys")?
            .ok_or_else(|| {
                SettingsError::new("team", "`team` is required: name each team by its key")
            })?,
        base_url: opt_scalar(settings, "base_url").unwrap_or_else(|| LINEAR_BASE.to_string()),
        auth: auth_config(settings)?,
        claim_from: nonempty_list(
            settings,
            "claim_from",
            "`claim_from` expects one or more workflow state names",
        )?
        .ok_or_else(|| {
            SettingsError::new(
                "claim_from",
                "`claim_from` is required: name the workflow state or states new issues are \
                 claimed from",
            )
        })?,
        gate: gate(settings, skip_labels.as_deref())?,
        skip_labels: skip_labels.unwrap_or_default(),
        discuss_with: nonempty_list(
            settings,
            "discuss_with",
            "`discuss_with` expects `anyone` or a member list",
        )?
        .map(|ops| {
            if ops == ["anyone"] {
                DiscussWith::Anyone
            } else {
                DiscussWith::Members(ops.iter().map(|s| member_ref(s)).collect())
            }
        }),
    })
}

/// What an issue must pass to be claimed: each `only_*` setting as the `linear.issues`
/// filter it names, `skip_labels` as `without_labels`, and no open blocker. Each setting is
/// held to its filter on its own first, so a refusal names the setting it is about.
fn gate(
    settings: &Map<String, Value>,
    skip_labels: Option<&[String]>,
) -> Result<IssueQuery, SettingsError> {
    let mut args = Map::new();
    for (setting, filter) in ONLY {
        let Some(value) = only_value(settings, setting)? else {
            continue;
        };
        let alone = Map::from_iter([(filter.to_string(), value.clone())]);
        issue_query(&alone).map_err(|problem| {
            let problem = problem.strip_prefix("issues: ").unwrap_or(&problem);
            SettingsError::new(setting, problem.replacen(filter, setting, 1))
        })?;
        args.insert(filter.to_string(), value);
    }
    if let Some(labels) = skip_labels {
        args.insert("without_labels".into(), json!(labels));
    }
    args.insert("blocked".into(), json!(false));
    // Every filter passed on its own, and `skip_labels` names one label or more, so only a
    // label list `without_labels` refuses is left to name.
    issue_query(&args).map_err(|problem| SettingsError::new("skip_labels", problem))
}

/// The `only_*` setting `key` as the argument its filter takes, `None` when it is absent:
/// `only_text` one string, `only_cycle` a list of ints, and every other a list of strings,
/// an enum member by its name.
fn only_value(settings: &Map<String, Value>, key: &str) -> Result<Option<Value>, SettingsError> {
    if key == "only_text" {
        return Ok(required_scalar(settings, key)?.map(Value::String));
    }
    let what = match key {
        "only_state_type" => "state types",
        "only_priority" => "priorities",
        "only_labels" | "only_project" | "only_milestone" => "names",
        "only_cycle" => "cycle numbers",
        "only_parent" => "issues",
        _ => "users",
    };
    let Some(items) = nonempty_list(
        settings,
        key,
        &format!("`{key}` expects one or more {what}"),
    )?
    else {
        return Ok(None);
    };
    if key != "only_cycle" {
        return Ok(Some(json!(items)));
    }
    let numbers = items
        .iter()
        .map(|item| {
            item.parse::<i64>().map_err(|_| {
                SettingsError::new(key, format!("`{key}` expects cycle numbers, not {item:?}"))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Some(json!(numbers)))
}

/// What is wrong with a credential that is not exactly one of an API key or an app's
/// `client_id` and `client_secret`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CredentialProblem {
    /// Neither form is written.
    Missing,
    /// An API key is written beside the app's id, its secret, or both, as each says.
    Doubled {
        client_id: bool,
        client_secret: bool,
    },
    /// The app's id is written without its secret.
    IdWithoutSecret,
    /// The app's secret is written without its id.
    SecretWithoutId,
}

/// The one credential rule a service's block and a `linear.Workspace` are both read by:
/// exactly one of an `api_key`, or a `client_id` with its `client_secret`.
///
/// # Errors
///
/// Returns the problem when the three name no credential, or more than one form of it.
pub(crate) fn credential(
    api_key: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
) -> Result<AuthConfig, CredentialProblem> {
    match (api_key, client_id, client_secret) {
        (Some(key), None, None) => Ok(AuthConfig::ApiKey(key)),
        (None, Some(client_id), Some(client_secret)) => Ok(AuthConfig::App {
            client_id,
            client_secret,
        }),
        (Some(_), id, secret) => Err(CredentialProblem::Doubled {
            client_id: id.is_some(),
            client_secret: secret.is_some(),
        }),
        (None, Some(_), None) => Err(CredentialProblem::IdWithoutSecret),
        (None, None, Some(_)) => Err(CredentialProblem::SecretWithoutId),
        (None, None, None) => Err(CredentialProblem::Missing),
    }
}

/// The credential the block writes: an `api_key`, or a `client_id` with its
/// `client_secret`, and never both forms; one written `nil` or `""` is not written, as a
/// `linear.Workspace` reads it. Each refusal names the setting it is about.
fn auth_config(settings: &Map<String, Value>) -> Result<AuthConfig, SettingsError> {
    let field = |key: &str| {
        required_scalar(settings, key).map(|value| value.filter(|text| !text.is_empty()))
    };
    let key = field("api_key")?;
    let id = field("client_id")?;
    let secret = field("client_secret")?;
    credential(key, id, secret).map_err(|problem| match problem {
        CredentialProblem::Doubled { .. } => SettingsError::new(
            "api_key",
            "write api_key or client_id and client_secret, not both",
        ),
        CredentialProblem::IdWithoutSecret => {
            SettingsError::new("client_id", "client_id needs client_secret")
        }
        CredentialProblem::SecretWithoutId => {
            SettingsError::new("client_secret", "client_secret needs client_id")
        }
        CredentialProblem::Missing => {
            SettingsError::new("api_key", "needs api_key, or client_id and client_secret")
        }
    })
}

/// The one [`ME`]-or-name rule every member operand is read by: `me` (case-sensitively)
/// selects whoever the credential authenticates as, any other text names a user by display
/// name or email.
pub(crate) fn member_ref(who: &str) -> MemberRef {
    match who {
        ME => MemberRef::Me,
        name => MemberRef::Named(name.to_string()),
    }
}

/// A lowered entry's **value** — the entry itself, or the `@value` beside a block — read
/// regardless of any block written with it.
fn inline(entry: &Value) -> Option<&Value> {
    match entry {
        Value::Object(block) => block.get("@value"),
        value => Some(value),
    }
}

/// A single-valued setting's scalar, or `None` when it is absent or not one string.
fn opt_scalar(settings: &Map<String, Value>, key: &str) -> Option<String> {
    match settings.get(key).and_then(inline) {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    }
}

/// An optional setting that, once written, must carry one value: `None` when absent or
/// written `nil`, and a fault when it is a flag, a block or a list — reading it as absent
/// would silently claim what the operator asked to filter.
fn required_scalar(
    settings: &Map<String, Value>,
    key: &str,
) -> Result<Option<String>, SettingsError> {
    if matches!(settings.get(key), None | Some(Value::Null)) {
        return Ok(None);
    }
    opt_scalar(settings, key)
        .map(Some)
        .ok_or_else(|| SettingsError::new(key, format!("setting `{key}` expects a single value")))
}

/// A repeatable list setting: every value its entries carry, flattened in order (afkd's
/// `SettingsTree::list`), `None` when the key is absent or written `nil`, and `problem` when
/// it was written but names nothing.
fn nonempty_list(
    settings: &Map<String, Value>,
    key: &str,
    problem: &str,
) -> Result<Option<Vec<String>>, SettingsError> {
    let Some(entries) = settings.get(key).filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let entries = match entries {
        Value::Array(entries) => entries.as_slice(),
        entry => std::slice::from_ref(entry),
    };
    let mut values = Vec::new();
    for value in entries.iter().filter_map(inline) {
        match value {
            Value::String(s) => values.push(s.clone()),
            Value::Array(items) => {
                values.extend(items.iter().filter_map(Value::as_str).map(str::to_string))
            }
            // A value-less flag contributes nothing.
            _ => {}
        }
    }
    if values.is_empty() {
        return Err(SettingsError::new(key, problem));
    }
    Ok(Some(values))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reads::{issue_filter, IssueRef};
    use crate::tracker::{Priority, StateType};
    use serde_json::json;

    /// The required settings and an API key, plus `extra`, as afkd lowers a block: the
    /// minimal block afkd would let through.
    fn block(extra: Value) -> Value {
        let mut settings = json!({
            "team": ["ENG"],
            "api_key": "lin_api_k",
            "claim_from": ["Todo"],
        });
        for (k, v) in extra.as_object().unwrap() {
            settings[k] = v.clone();
        }
        settings
    }

    fn cfg(extra: Value) -> IntakeConfig {
        intake_config(&block(extra)).expect("valid")
    }

    fn err(extra: Value) -> SettingsError {
        intake_config(&block(extra)).unwrap_err()
    }

    /// The same block with its credential replaced by `auth`'s keys.
    fn with_auth(auth: Value) -> Result<IntakeConfig, SettingsError> {
        let mut settings = block(json!({}));
        settings.as_object_mut().unwrap().remove("api_key");
        for (k, v) in auth.as_object().unwrap() {
            settings[k] = v.clone();
        }
        intake_config(&settings)
    }

    #[test]
    fn reads_the_teams_and_defaults() {
        // afkd's own two keys ride along and are not read here.
        let read = cfg(json!({"every": "30s", "max_attempts": "2"}));
        assert_eq!(read.teams, ["ENG"]);
        assert_eq!(read.base_url, LINEAR_BASE);
        assert_eq!(read.auth, AuthConfig::ApiKey("lin_api_k".into()));
        assert_eq!(read.claim_from, ["Todo"]);
        assert!(read.skip_labels.is_empty());
        assert_eq!(read.discuss_with, None);
        assert_eq!(
            issue_filter(&read.gate),
            json!({"and": [{"hasBlockedByRelations": {"eq": false}}]}),
            "with no filter set, the gate holds back only a blocked issue"
        );
        assert_eq!(
            intake_config(&block(json!({"base_url": "http://127.0.0.1:9"})))
                .unwrap()
                .base_url,
            "http://127.0.0.1:9"
        );
        assert_eq!(
            cfg(json!({"team": ["ENG", "WEB 🌐"], "claim_from": ["Todo", "Väntar på svar"]})).teams,
            ["ENG", "WEB 🌐"]
        );
    }

    /// Exactly one credential: an API key alone, or an app's id with its secret, a field
    /// written `nil` or `""` unwritten. Every other combination is refused in its own
    /// sentence, on the setting it is about.
    #[test]
    fn the_credential_is_an_api_key_or_an_apps_id_and_secret() {
        assert_eq!(
            with_auth(
                json!({"client_id": "app-id-7f3a", "client_secret": "app-secret-9c1e",
                             "api_key": ""})
            )
            .unwrap()
            .auth,
            AuthConfig::App {
                client_id: "app-id-7f3a".into(),
                client_secret: "app-secret-9c1e".into()
            }
        );
        assert_eq!(
            with_auth(json!({"api_key": "k", "client_id": null, "client_secret": ""}))
                .unwrap()
                .auth,
            AuthConfig::ApiKey("k".into())
        );
        for (auth, key, problem) in [
            (
                json!({"api_key": "k", "client_id": "i", "client_secret": "s"}),
                "api_key",
                "write api_key or client_id and client_secret, not both",
            ),
            (
                json!({"api_key": "k", "client_id": "i"}),
                "api_key",
                "write api_key or client_id and client_secret, not both",
            ),
            (
                json!({"api_key": "k", "client_secret": "s"}),
                "api_key",
                "write api_key or client_id and client_secret, not both",
            ),
            (
                json!({}),
                "api_key",
                "needs api_key, or client_id and client_secret",
            ),
            (
                json!({"api_key": ""}),
                "api_key",
                "needs api_key, or client_id and client_secret",
            ),
            (
                json!({"client_id": "i"}),
                "client_id",
                "client_id needs client_secret",
            ),
            (
                json!({"client_secret": "s"}),
                "client_secret",
                "client_secret needs client_id",
            ),
        ] {
            let e = with_auth(auth.clone()).unwrap_err();
            assert_eq!(
                (e.key.as_str(), e.problem.as_str()),
                (key, problem),
                "{auth}"
            );
        }
    }

    /// The slots are passed the workspace the credential settings build: the credential the
    /// service reaches Linear with, the other fields `nil`, and the API root.
    #[test]
    fn the_workspace_is_the_credential_and_the_api_root() {
        assert_eq!(
            cfg(json!({"client_id": null})).workspace(),
            json!({"api_key": "lin_api_k", "client_id": null, "client_secret": null,
                   "base_url": "https://api.linear.app"})
        );
        assert_eq!(
            with_auth(json!({"client_id": "app-id-7f3a", "client_secret": "s/&=",
                             "base_url": "http://127.0.0.1:9"}))
            .unwrap()
            .workspace(),
            json!({"api_key": null, "client_id": "app-id-7f3a", "client_secret": "s/&=",
                   "base_url": "http://127.0.0.1:9"})
        );
    }

    /// A block without `team` or `claim_from` names nothing to claim from, so it is refused
    /// naming the setting rather than armed to scan nothing.
    #[test]
    fn a_block_without_team_or_claim_from_is_refused_naming_it() {
        for (key, problem) in [
            ("team", "`team` is required: name each team by its key"),
            (
                "claim_from",
                "`claim_from` is required: name the workflow state or states new issues are \
                 claimed from",
            ),
        ] {
            let mut settings = block(json!({}));
            settings.as_object_mut().unwrap().remove(key);
            let e = intake_config(&settings).unwrap_err();
            assert_eq!((e.key.as_str(), e.problem.as_str()), (key, problem));
        }
    }

    /// A value beside a block rides as `@value`, and is read as the value.
    #[test]
    fn a_value_beside_a_block_is_read_as_the_value() {
        let cfg = intake_config(&json!({
            "team": {"@value": "ENG"},
            "api_key": "k",
            "claim_from": {"@value": "Todo"},
        }))
        .expect("valid");
        assert_eq!(cfg.teams, ["ENG"]);
        assert_eq!(cfg.claim_from, ["Todo"]);
    }

    /// Each `only_*` setting is the `linear.issues` filter of the name after `only_`, in the
    /// gate beside `skip_labels` as `without_labels` and no open blocker: an enum member by
    /// its name, a cycle number as an int, `me` and `none` as the filters read them, and wide
    /// text as it is.
    #[test]
    fn each_only_setting_is_the_filter_it_names() {
        let gate = cfg(json!({
            "only_state_type": ["unstarted", "started"],
            "only_priority": ["urgent", "high"],
            "only_labels": ["afkd/ready", "Väntar på svar"],
            "only_assignee": ["me", "none"],
            "only_delegate": ["陳大文", "none"],
            "only_project": ["Trackers 🧭"],
            "only_milestone": ["Beta"],
            "only_cycle": ["12", "13"],
            "only_parent": ["ENG-7"],
            "only_text": "retry \"storm\"",
            "skip_labels": ["Blocked", "WIP 🚧"],
        }))
        .gate;
        assert_eq!(
            gate.state_type,
            Some(vec![StateType::Unstarted, StateType::Started])
        );
        assert_eq!(gate.priority, Some(vec![Priority::Urgent, Priority::High]));
        assert_eq!(
            gate.labels,
            Some(vec!["afkd/ready".into(), "Väntar på svar".into()])
        );
        assert_eq!(gate.assignee, Some(vec!["me".into(), "none".into()]));
        assert_eq!(gate.delegate, Some(vec!["陳大文".into(), "none".into()]));
        assert_eq!(gate.project, Some(vec!["Trackers 🧭".into()]));
        assert_eq!(gate.milestone, Some(vec!["Beta".into()]));
        assert_eq!(gate.cycle, Some(vec![12, 13]));
        assert_eq!(
            gate.parent,
            Some(vec![IssueRef::Identifier {
                team: "ENG".into(),
                number: 7
            }])
        );
        assert_eq!(gate.text.as_deref(), Some("retry \"storm\""));
        assert_eq!(
            gate.without_labels,
            Some(vec!["Blocked".into(), "WIP 🚧".into()])
        );
        assert_eq!(gate.blocked, Some(false));
        assert_eq!(gate.team, None, "the scan names the team itself");
        assert_eq!(gate.state, None, "the scan names the state itself");
        assert_eq!(
            cfg(json!({"skip_labels": ["Blocked"]})).skip_labels,
            ["Blocked"]
        );
    }

    /// A filter `linear.issues` would refuse is refused naming the setting, and one written
    /// with no value names nothing, so it is refused rather than read as absent.
    #[test]
    fn an_only_setting_its_filter_refuses_is_refused_naming_it() {
        for (setting, value, problem) in [
            (
                "only_cycle",
                json!(["12", "zwölf"]),
                "`only_cycle` expects cycle numbers, not \"zwölf\"",
            ),
            (
                "only_parent",
                json!(["the epic"]),
                "only_parent \"the epic\" is neither an identifier (ENG-123) nor a uuid",
            ),
            (
                "only_labels",
                json!([true]),
                "`only_labels` expects one or more names",
            ),
            (
                "only_assignee",
                json!([{}]),
                "`only_assignee` expects one or more users",
            ),
            (
                "only_text",
                json!(["a", "b"]),
                "setting `only_text` expects a single value",
            ),
            (
                "skip_labels",
                json!([true]),
                "`skip_labels` expects one or more label names",
            ),
        ] {
            let e = err(json!({ setting: value }));
            assert_eq!(
                (e.key.as_str(), e.problem.as_str()),
                (setting, problem),
                "{setting}"
            );
        }
        assert_eq!(
            cfg(json!({"only_labels": null, "skip_labels": null})).gate,
            cfg(json!({})).gate,
            "a list written nil is not written"
        );
    }

    /// A valueless scalar names nothing. It must fault on its own key rather than read as
    /// absent, which would silently claim any issue.
    #[test]
    fn a_single_valued_setting_without_one_value_faults() {
        for key in ["only_text", "api_key"] {
            for value in [json!(true), json!({}), json!(["a", "b"])] {
                let e = err(json!({ key: value }));
                assert_eq!(e.key, key);
                assert_eq!(e.problem, format!("setting `{key}` expects a single value"));
            }
        }
    }

    /// `claim_from` is a list setting: one name or several, in the shapes afkd lowers, kept
    /// in the order written; written with no value it names nothing.
    #[test]
    fn claim_from_reads_one_state_or_several_in_the_shapes_afkd_lowers() {
        for value in [
            json!("Todo"),
            json!(["Todo"]),
            json!([["Todo"]]),
            json!({"@value": "Todo"}),
        ] {
            assert_eq!(cfg(json!({ "claim_from": value })).claim_from, ["Todo"]);
        }
        assert_eq!(
            cfg(json!({"claim_from": ["Triage 🛎️", ["Todo", "Väntar på svar"]]})).claim_from,
            ["Triage 🛎️", "Todo", "Väntar på svar"]
        );
        for value in [json!(true), json!([true]), json!([{}])] {
            let e = err(json!({ "claim_from": value }));
            assert_eq!(e.key, "claim_from");
            assert_eq!(
                e.problem,
                "`claim_from` expects one or more workflow state names"
            );
        }
    }

    /// `anyone` as the sole operand is anyone; beside a name it is a name; `me` is the
    /// viewer; and a valueless list faults.
    #[test]
    fn discuss_with_reads_anyone_and_members() {
        assert_eq!(
            cfg(json!({"discuss_with": ["anyone"]})).discuss_with,
            Some(DiscussWith::Anyone)
        );
        assert_eq!(
            cfg(json!({"discuss_with": [["anyone", "alice@acme.example"]]})).discuss_with,
            Some(DiscussWith::Members(vec![
                MemberRef::Named("anyone".into()),
                MemberRef::Named("alice@acme.example".into()),
            ]))
        );
        assert_eq!(
            cfg(json!({"discuss_with": ["me", "陳大文"]})).discuss_with,
            Some(DiscussWith::Members(vec![
                MemberRef::Me,
                MemberRef::Named("陳大文".into()),
            ]))
        );
        let e = err(json!({"discuss_with": [{}]}));
        assert_eq!(
            (e.key.as_str(), e.problem.as_str()),
            (
                "discuss_with",
                "`discuss_with` expects `anyone` or a member list"
            )
        );
    }

    /// No secret reaches `Debug`: neither an API key, nor an app's secret.
    #[test]
    fn debug_prints_no_secret() {
        let key = format!("{:?}", cfg(json!({"api_key": "lin_api_SECRET-KEY-abc123"})));
        assert!(!key.contains("SECRET"), "{key}");
        assert!(key.contains("ApiKey"), "{key}");
        let app = format!(
            "{:?}",
            with_auth(json!({"client_id": "app-id-7f3a", "client_secret": "SECRET-def456"}))
                .unwrap()
        );
        assert!(!app.contains("SECRET"), "{app}");
        assert!(app.contains("app-id-7f3a"), "the id is no secret: {app}");
    }
}
