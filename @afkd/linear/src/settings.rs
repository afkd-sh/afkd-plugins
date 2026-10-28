//! Read the `issue` kind's settings block — as afkd lowers it to JSON in `hello` — into a
//! typed [`IssueConfig`].
//!
//! afkd has already typed the block against the manifest before this plugin is spawned:
//! every key is one the kind declares, `team` and `pick_from` are present, and each value is
//! of its declared type. What is left here is the part a manifest cannot say: a list that
//! names nothing, and which credential is written, since exactly one of an API key or an
//! app's `client_id` and `client_secret` is. The cadence and the attempt bound (`every`,
//! `max_attempts`) are afkd's and are not read here, and the slots never arrive here at all.
//!
//! The lowering (afkd's `pluginworker::settings_json`): a value is a string (a duration its
//! largest whole unit, a range `lo..hi`), and a `list[…]` is always an array — of the one
//! item, or holding the array of several.

use serde_json::{Map, Value};

use crate::client::LINEAR_BASE;

/// The member operand naming whoever the credential authenticates as: the API key's user, or
/// the app itself. No display name or email can collide with it in practice, and Linear has
/// no reserved word of its own, so this is the plugin's.
pub(crate) const ME: &str = "me";

/// The `require_member` operand for an issue nobody holds: one with neither an assignee nor a
/// delegate. It is reserved there only, so `discuss_with` and `delegate` still read it as a
/// name.
pub(crate) const NONE: &str = "none";

/// Who a `require_member` gate or a `discuss_with` entry names: [`ME`], or a workspace user
/// by display name or email.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MemberRef {
    /// Whoever the credential authenticates as, the `viewer`.
    Me,
    /// A user named by their display name or their email, whichever matches.
    Named(String),
}

/// One `require_member` entry: a member who holds the issue, or nobody holding it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Holder {
    /// The issue is assigned or delegated to this member.
    Member(MemberRef),
    /// The issue has neither an assignee nor a delegate.
    Nobody,
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

/// A validated `issue` trigger configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IssueConfig {
    /// The team's key (`ENG`), which every identifier it holds starts with.
    pub(crate) team_key: String,
    /// API root the client is pointed at; defaults to Linear's. A testability seam, not a
    /// user-facing knob.
    pub(crate) base_url: String,
    /// The credential.
    pub(crate) auth: AuthConfig,
    /// The workflow states new issues are drawn from, by name, in the order written; never
    /// empty, since the setting is required.
    pub(crate) pick_from: Vec<String>,
    /// The intake gate: when set, only issues one of these entries holds are claimed, an issue
    /// passing if any entry matches; never empty.
    pub(crate) require_member: Option<Vec<Holder>>,
    /// The label intake gate: when set, only issues carrying a label of this exact name are
    /// eligible to claim.
    pub(crate) require_label: Option<String>,
    /// The negative label intake gate: an issue carrying any of these labels is never
    /// claimed, and deny wins over `require_label`.
    pub(crate) without_label: Vec<String>,
    /// The comment-driven grooming gate: when set, an issue is claimed only if its tail —
    /// the comments after afkd's own last comment — carries a comment from an allowed
    /// author.
    pub(crate) discuss_with: Option<DiscussWith>,
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

/// The `issue` kind's settings as the manifest declares them, in its order: each name, its
/// `type`, and its other keys spelled as the manifest spells their values. afkd reads the
/// manifest, never this: it is what `manifest.rs`'s test holds `afkd-plugin.toml` to, so the
/// two cannot drift apart. afkd's own claim keys are not among them.
#[cfg(test)]
pub(crate) const SETTINGS: &[Declared] = &[
    ("team", "string", &[("required", "true")]),
    ("base_url", "string", &[]),
    ("api_key", "string", &[]),
    ("client_id", "string", &[]),
    ("client_secret", "string", &[]),
    ("pick_from", "list[string]", &[("required", "true")]),
    ("require_member", "list[string]", &[]),
    ("require_label", "string", &[]),
    ("without_label", "list[string]", &[]),
    ("discuss_with", "list[string]", &[]),
];

/// Read the lowered `settings` into an [`IssueConfig`], or report the first setting it
/// cannot use.
pub(crate) fn issue_config(settings: &Value) -> Result<IssueConfig, SettingsError> {
    let empty = Map::new();
    let settings = settings.as_object().unwrap_or(&empty);
    Ok(IssueConfig {
        // afkd refuses a service without either before any `hello`; these refuse a `hello`
        // that arrives without them anyway, so no armed service scans nothing.
        team_key: opt_scalar(settings, "team")
            .filter(|key| !key.is_empty())
            .ok_or_else(|| {
                SettingsError::new("team", "`team` is required: name the team by its key")
            })?,
        base_url: opt_scalar(settings, "base_url").unwrap_or_else(|| LINEAR_BASE.to_string()),
        auth: auth_config(settings)?,
        pick_from: nonempty_list(
            settings,
            "pick_from",
            "`pick_from` expects one or more workflow state names",
        )?
        .ok_or_else(|| {
            SettingsError::new(
                "pick_from",
                "`pick_from` is required: name the workflow state or states new issues are \
                 claimed from",
            )
        })?,
        require_member: nonempty_list(
            settings,
            "require_member",
            "`require_member` expects one or more members",
        )?
        .map(|entries| entries.iter().map(|s| holder(s)).collect()),
        require_label: required_scalar(settings, "require_label")?,
        without_label: nonempty_list(
            settings,
            "without_label",
            "`without_label` expects one or more label names",
        )?
        .unwrap_or_default(),
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

/// The credential the block writes: an `api_key`, or a `client_id` with its
/// `client_secret`, and never both forms. Each refusal names the setting it is about.
fn auth_config(settings: &Map<String, Value>) -> Result<AuthConfig, SettingsError> {
    let key = required_scalar(settings, "api_key")?;
    let id = required_scalar(settings, "client_id")?;
    let secret = required_scalar(settings, "client_secret")?;
    match (key, id, secret) {
        (Some(key), None, None) => Ok(AuthConfig::ApiKey(key)),
        (None, Some(client_id), Some(client_secret)) => Ok(AuthConfig::App {
            client_id,
            client_secret,
        }),
        (Some(_), _, _) => Err(SettingsError::new(
            "api_key",
            "write api_key or client_id and client_secret, not both",
        )),
        (None, Some(_), None) => Err(SettingsError::new(
            "client_id",
            "client_id needs client_secret",
        )),
        (None, None, Some(_)) => Err(SettingsError::new(
            "client_secret",
            "client_secret needs client_id",
        )),
        (None, None, None) => Err(SettingsError::new(
            "api_key",
            "needs api_key, or client_id and client_secret",
        )),
    }
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

/// One `require_member` entry: [`NONE`] (case-sensitively) is nobody holding the issue, and
/// any other text is a member, read by [`member_ref`].
fn holder(who: &str) -> Holder {
    match who {
        NONE => Holder::Nobody,
        who => Holder::Member(member_ref(who)),
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

/// An optional setting that, once written, must carry one value: `None` when absent, and a
/// fault when it is a flag, a block or a list — reading it as absent would silently claim
/// what the operator asked to filter.
fn required_scalar(
    settings: &Map<String, Value>,
    key: &str,
) -> Result<Option<String>, SettingsError> {
    if !settings.contains_key(key) {
        return Ok(None);
    }
    opt_scalar(settings, key)
        .map(Some)
        .ok_or_else(|| SettingsError::new(key, format!("setting `{key}` expects a single value")))
}

/// A repeatable list setting: every value its entries carry, flattened in order (afkd's
/// `SettingsTree::list`), `None` when the key is absent, and `problem` when it was written
/// but names nothing.
fn nonempty_list(
    settings: &Map<String, Value>,
    key: &str,
    problem: &str,
) -> Result<Option<Vec<String>>, SettingsError> {
    let Some(entries) = settings.get(key) else {
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
    use serde_json::json;

    /// The required settings and an API key, plus `extra`, as afkd lowers a block: the
    /// minimal block afkd would let through.
    fn block(extra: Value) -> Value {
        let mut settings = json!({
            "team": "ENG",
            "api_key": "lin_api_k",
            "pick_from": "Todo",
        });
        for (k, v) in extra.as_object().unwrap() {
            settings[k] = v.clone();
        }
        settings
    }

    fn cfg(extra: Value) -> IssueConfig {
        issue_config(&block(extra)).expect("valid")
    }

    fn err(extra: Value) -> SettingsError {
        issue_config(&block(extra)).unwrap_err()
    }

    /// The same block with its credential replaced by `auth`'s keys.
    fn with_auth(auth: Value) -> Result<IssueConfig, SettingsError> {
        let mut settings = block(json!({}));
        settings.as_object_mut().unwrap().remove("api_key");
        for (k, v) in auth.as_object().unwrap() {
            settings[k] = v.clone();
        }
        issue_config(&settings)
    }

    #[test]
    fn reads_the_team_and_defaults() {
        // afkd's own two keys ride along and are not read here.
        let cfg = cfg(json!({"every": "30s", "max_attempts": "2"}));
        assert_eq!(cfg.team_key, "ENG");
        assert_eq!(cfg.base_url, LINEAR_BASE);
        assert_eq!(cfg.auth, AuthConfig::ApiKey("lin_api_k".into()));
        assert_eq!(cfg.pick_from, ["Todo"]);
        assert_eq!(cfg.require_member, None);
        assert_eq!(cfg.require_label, None);
        assert!(cfg.without_label.is_empty());
        assert_eq!(cfg.discuss_with, None);
        assert_eq!(
            super::issue_config(&block(json!({"base_url": "http://127.0.0.1:9"})))
                .unwrap()
                .base_url,
            "http://127.0.0.1:9"
        );
    }

    /// Exactly one credential: an API key alone, or an app's id with its secret. Every other
    /// combination is refused in its own sentence, on the setting it is about.
    #[test]
    fn the_credential_is_an_api_key_or_an_apps_id_and_secret() {
        assert_eq!(
            with_auth(json!({"client_id": "app-id-7f3a", "client_secret": "app-secret-9c1e"}))
                .unwrap()
                .auth,
            AuthConfig::App {
                client_id: "app-id-7f3a".into(),
                client_secret: "app-secret-9c1e".into()
            }
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

    /// A block without `team` or `pick_from` names nothing to claim from, so it is refused
    /// naming the setting rather than armed to scan nothing.
    #[test]
    fn a_block_without_team_or_pick_from_is_refused_naming_it() {
        for (key, problem) in [
            ("team", "`team` is required: name the team by its key"),
            (
                "pick_from",
                "`pick_from` is required: name the workflow state or states new issues are \
                 claimed from",
            ),
        ] {
            let mut settings = block(json!({}));
            settings.as_object_mut().unwrap().remove(key);
            let e = issue_config(&settings).unwrap_err();
            assert_eq!((e.key.as_str(), e.problem.as_str()), (key, problem));
        }
    }

    /// A value beside a block rides as `@value`, and is read as the value.
    #[test]
    fn a_value_beside_a_block_is_read_as_the_value() {
        let cfg = issue_config(&json!({
            "team": {"@value": "ENG"},
            "api_key": "k",
            "pick_from": {"@value": "Todo"},
        }))
        .expect("valid");
        assert_eq!(cfg.team_key, "ENG");
        assert_eq!(cfg.pick_from, ["Todo"]);
    }

    /// `me` is the viewer; anything else is a user by display name or email, wide text and
    /// all, and a differently-cased `Me` is a name.
    #[test]
    fn require_member_reads_me_a_display_name_and_an_email() {
        for (written, want) in [
            ("me", MemberRef::Me),
            ("björn.öst", MemberRef::Named("björn.öst".into())),
            (
                "chen@acme.example",
                MemberRef::Named("chen@acme.example".into()),
            ),
            ("陳大文", MemberRef::Named("陳大文".into())),
            ("Me", MemberRef::Named("Me".into())),
        ] {
            assert_eq!(
                cfg(json!({ "require_member": [written] })).require_member,
                Some(vec![Holder::Member(want)])
            );
        }
    }

    /// `none` is nobody holding the issue, alone or beside members, in the order written; a
    /// differently-cased `None` is a name, and a valueless list faults.
    #[test]
    fn require_member_reads_none_beside_members() {
        for (value, want) in [
            (
                json!([["me", "none"]]),
                vec![Holder::Member(MemberRef::Me), Holder::Nobody],
            ),
            (json!(["none"]), vec![Holder::Nobody]),
            (
                json!([["none", "陳大文", "None"]]),
                vec![
                    Holder::Nobody,
                    Holder::Member(MemberRef::Named("陳大文".into())),
                    Holder::Member(MemberRef::Named("None".into())),
                ],
            ),
        ] {
            assert_eq!(
                cfg(json!({ "require_member": value })).require_member,
                Some(want),
                "{value}"
            );
        }
        for value in [json!(true), json!([true]), json!([{}])] {
            let e = err(json!({ "require_member": value }));
            assert_eq!(
                (e.key.as_str(), e.problem.as_str()),
                (
                    "require_member",
                    "`require_member` expects one or more members"
                )
            );
        }
    }

    /// A valueless gate names nothing. It must fault on its own key rather than read as
    /// absent, which would silently claim any issue.
    #[test]
    fn a_single_valued_gate_without_one_value_faults() {
        for key in ["require_label", "api_key"] {
            for value in [json!(true), json!({}), json!(["a", "b"])] {
                let e = err(json!({ key: value }));
                assert_eq!(e.key, key);
                assert_eq!(e.problem, format!("setting `{key}` expects a single value"));
            }
        }
    }

    /// `pick_from` is a list setting: one name or several, in the shapes afkd lowers, kept
    /// in the order written; written with no value it names nothing.
    #[test]
    fn pick_from_reads_one_state_or_several_in_the_shapes_afkd_lowers() {
        for value in [
            json!("Todo"),
            json!(["Todo"]),
            json!([["Todo"]]),
            json!({"@value": "Todo"}),
        ] {
            assert_eq!(cfg(json!({ "pick_from": value })).pick_from, ["Todo"]);
        }
        assert_eq!(
            cfg(json!({"pick_from": ["Triage 🛎️", ["Todo", "Väntar på svar"]]})).pick_from,
            ["Triage 🛎️", "Todo", "Väntar på svar"]
        );
        for value in [json!(true), json!([true]), json!([{}])] {
            let e = err(json!({ "pick_from": value }));
            assert_eq!(e.key, "pick_from");
            assert_eq!(
                e.problem,
                "`pick_from` expects one or more workflow state names"
            );
        }
    }

    #[test]
    fn without_label_reads_one_or_many_and_faults_when_valueless() {
        assert_eq!(
            cfg(json!({"without_label": [["Hold", "WIP"], "Blockerat / Väntar"]})).without_label,
            ["Hold", "WIP", "Blockerat / Väntar"]
        );
        let e = err(json!({"without_label": [true]}));
        assert_eq!(
            (e.key.as_str(), e.problem.as_str()),
            (
                "without_label",
                "`without_label` expects one or more label names"
            )
        );
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
