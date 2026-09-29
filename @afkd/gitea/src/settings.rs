//! Read the `issue` and `pr` kinds' settings blocks — as afkd lowers them to JSON in
//! `hello` — into a typed [`GiteaConfig`].
//!
//! afkd has already typed the block against the manifest before this plugin is spawned:
//! every key is one the kind declares, `token` is present, and each value is of its
//! declared type. What is left here is the part a manifest cannot say — exactly one of
//! `repo`/`org`, and a `discuss_with` naming someone — in the built-in trigger's own
//! sentences. The cadence and the attempt bound (`poll_interval`, `max_attempts`,
//! `follow_comments`) are afkd's and are not read here, and the hooks never arrive here at
//! all: afkd runs them, and each action they call is one `call` ([`crate::lifecycle`]).
//!
//! The lowering (afkd's `pluginworker::settings_json`): a value is a string (a `bool` its
//! word `true` or `false`, a duration its largest whole unit), and a `list[…]` its items —
//! an array of them, possibly nested one deep, or the one item alone.

use serde_json::{Map, Value};

/// Whose comment counts as a reply on the `discuss_with` tail gate: plain Gitea logins
/// (a comment author *is* a login), or anyone but the bot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DiscussWith {
    /// The reserved sole operand `anyone`: any author but the bot itself.
    Anyone,
    /// Only these logins count as a reply (the bot's own never does).
    Logins(Vec<String>),
}

/// A validated `issue` or `pr` trigger configuration. The keys one kind
/// does not declare stay at their defaults for the other.
///
/// `Debug` is **hand-written** so the `token` never reaches a diagnostic.
#[derive(Clone, PartialEq, Eq, Default)]
pub(crate) struct GiteaConfig {
    /// The Gitea instance base URL (e.g. `https://gitea.example.com`).
    pub(crate) base_url: String,
    /// The single repository to poll, as `owner/name` (empty when polling an org).
    pub(crate) repo: String,
    /// The org to poll every repo of (empty when polling a single repo).
    pub(crate) org: String,
    /// The personal access token.
    pub(crate) token: String,
    /// The eligibility source label (empty when not given).
    pub(crate) source_label: String,
    /// Restrict the PR kind to the bot's own PRs (the `author_me` setting).
    pub(crate) author_me: bool,
    /// The comment-tail claim gate. `None` is the unset key — the claim path reads no
    /// comments outside the awaiting-reply re-arm.
    pub(crate) discuss_with: Option<DiscussWith>,
}

/// Redact `token` so a `{:?}` of a [`GiteaConfig`] can never spill it; presence is shown
/// as `<set>`/`<unset>` so the config stays debuggable.
impl std::fmt::Debug for GiteaConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let redact = |s: &str| if s.is_empty() { "<unset>" } else { "<set>" };
        f.debug_struct("GiteaConfig")
            .field("base_url", &self.base_url)
            .field("repo", &self.repo)
            .field("org", &self.org)
            .field("token", &redact(&self.token))
            .field("source_label", &self.source_label)
            .field("author_me", &self.author_me)
            .field("discuss_with", &self.discuss_with)
            .finish()
    }
}

/// A setting afkd let through that this kind cannot use: the key it is about, and the
/// problem in the built-in trigger's words.
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
/// with their values as the manifest spells them (`("default", "\"30s\"")`).
#[cfg(test)]
pub(crate) type Declared = (
    &'static str,
    &'static str,
    &'static [(&'static str, &'static str)],
);

/// The `issue` kind's settings as the manifest declares them, in its order: each name, its
/// `type`, and its other keys spelled as the manifest spells their values. afkd reads the
/// manifest, never this: it is what `manifest.rs`'s test holds `afkd-plugin.toml` to, so
/// the two cannot drift apart.
///
/// The last three are afkd's own claim keys, declared exactly as afkd declares them for
/// every claiming kind; afkd's declaration is the one that applies.
#[cfg(test)]
pub(crate) const ISSUE_SETTINGS: &[Declared] = &[
    ("base_url", "string", &[]),
    ("repo", "string", &[]),
    ("org", "string", &[]),
    ("token", "string", &[("required", "true")]),
    ("source_label", "string", &[]),
    ("discuss_with", "list[string]", &[]),
    ("follow_comments", "duration", &[("jitter", "true")]),
    ("max_attempts", "int", &[("default", "1")]),
    (
        "poll_interval",
        "duration",
        &[("jitter", "true"), ("default", "\"30s\"")],
    ),
];

/// The `pr` kind's settings, as [`ISSUE_SETTINGS`] lists the issue kind's: the shared keys
/// with `author_me` in place of `source_label` and `discuss_with`.
#[cfg(test)]
pub(crate) const PR_SETTINGS: &[Declared] = &[
    ("base_url", "string", &[]),
    ("repo", "string", &[]),
    ("org", "string", &[]),
    ("token", "string", &[("required", "true")]),
    ("author_me", "bool", &[("default", "false")]),
    ("follow_comments", "duration", &[("jitter", "true")]),
    ("max_attempts", "int", &[("default", "1")]),
    (
        "poll_interval",
        "duration",
        &[("jitter", "true"), ("default", "\"30s\"")],
    ),
];

/// Read the lowered `settings` into a [`GiteaConfig`] for the `issue` kind, or report the
/// first setting it cannot use.
pub(crate) fn issue_config(settings: &Value) -> Result<GiteaConfig, SettingsError> {
    let empty = Map::new();
    let settings = settings.as_object().unwrap_or(&empty);
    let mut cfg = shared_config(settings)?;
    cfg.source_label = opt_scalar(settings, "source_label");
    cfg.discuss_with = parse_discuss_with(settings)?;
    Ok(cfg)
}

/// Read the lowered `settings` into a [`GiteaConfig`] for the `pr` kind, or report the
/// first setting it cannot use. `author_me` is a `bool`, which afkd lowers to its word:
/// `"true"` turns it on, and `"false"` or its absence leaves it off.
pub(crate) fn pr_config(settings: &Value) -> Result<GiteaConfig, SettingsError> {
    let empty = Map::new();
    let settings = settings.as_object().unwrap_or(&empty);
    let mut cfg = shared_config(settings)?;
    cfg.author_me = opt_scalar(settings, "author_me") == "true";
    Ok(cfg)
}

/// The keys both kinds carry: the target (exactly one of `repo`/`org`) and the forge
/// coordinates.
fn shared_config(settings: &Map<String, Value>) -> Result<GiteaConfig, SettingsError> {
    let repo = opt_scalar(settings, "repo");
    let org = opt_scalar(settings, "org");
    require_exactly_one_target(&repo, &org)?;
    Ok(GiteaConfig {
        base_url: opt_scalar(settings, "base_url"),
        repo,
        org,
        token: opt_scalar(settings, "token"),
        ..GiteaConfig::default()
    })
}

/// A lowered entry's **value** — the entry itself, or the `@value` beside a block — as
/// the built-in reads a value regardless of any block written with it.
fn inline(entry: &Value) -> Option<&Value> {
    match entry {
        Value::Object(block) => block.get("@value"),
        value => Some(value),
    }
}

/// A single-valued setting's scalar, or `""` when it is absent or not one string — the
/// built-in's `opt_scalar(..).unwrap_or_default()`.
fn opt_scalar(settings: &Map<String, Value>, key: &str) -> String {
    match settings.get(key).and_then(inline) {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

/// Every operand of every `key` entry, flattened — the built-in's `SettingsTree::list`. A
/// bare flag or a block-only entry contributes nothing.
fn list(settings: &Map<String, Value>, key: &str) -> Vec<String> {
    let entries = match settings.get(key) {
        None => return Vec::new(),
        Some(Value::Array(entries)) => entries.as_slice(),
        Some(entry) => std::slice::from_ref(entry),
    };
    let mut out = Vec::new();
    for entry in entries {
        match inline(entry) {
            Some(Value::String(s)) => out.push(s.clone()),
            Some(Value::Array(items)) => {
                out.extend(items.iter().filter_map(Value::as_str).map(str::to_string))
            }
            _ => {}
        }
    }
    out
}

/// Parse the optional `discuss_with` tail gate. Absent is `None`; the sole operand
/// `anyone` is [`DiscussWith::Anyone`]; any other operand list is the logins. A present
/// but valueless key names no author and faults, rather than reading as absent — a gate
/// the operator asked for must never silently degrade into no gate.
fn parse_discuss_with(settings: &Map<String, Value>) -> Result<Option<DiscussWith>, SettingsError> {
    if !settings.contains_key("discuss_with") {
        return Ok(None);
    }
    let ops = list(settings, "discuss_with");
    if ops.is_empty() {
        return Err(SettingsError::new(
            "discuss_with",
            "`discuss_with` expects `anyone` or a login list",
        ));
    }
    if ops == ["anyone"] {
        Ok(Some(DiscussWith::Anyone))
    } else {
        Ok(Some(DiscussWith::Logins(ops)))
    }
}

/// Enforce that **exactly one** of `repo`/`org` is set.
fn require_exactly_one_target(repo: &str, org: &str) -> Result<(), SettingsError> {
    match (repo.is_empty(), org.is_empty()) {
        (false, true) | (true, false) => Ok(()),
        (true, true) => Err(SettingsError::new(
            "repo",
            "a gitea trigger needs exactly one of `repo` or `org` (neither was set)",
        )),
        (false, false) => Err(SettingsError::new(
            "org",
            "a gitea trigger takes exactly one of `repo` or `org` (both were set)",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The minimal block afkd would let through, plus `extra`.
    fn block(extra: Value) -> Value {
        let mut settings = json!({"repo": "acme/widgets", "token": "PAT"});
        for (k, v) in extra.as_object().unwrap() {
            settings[k] = v.clone();
        }
        settings
    }

    #[test]
    fn issue_config_reads_scalars_and_defaults() {
        let cfg = issue_config(&json!({
            "base_url": "https://gitea.example.com",
            "repo": "acme/widgets",
            "token": "PAT",
            "source_label": "afkd/ready",
            "max_attempts": "2",
            "poll_interval": "1m",
        }))
        .expect("valid");
        assert_eq!(cfg.base_url, "https://gitea.example.com");
        assert_eq!(cfg.repo, "acme/widgets");
        assert_eq!(cfg.org, "");
        assert_eq!(cfg.token, "PAT");
        assert_eq!(cfg.source_label, "afkd/ready");
        assert_eq!(cfg.discuss_with, None);
    }

    /// The hooks are afkd's and never cross in `settings`; a proto 1-shaped leftover that
    /// did — an `on_claim` block, a bare `on_done`, a `@{run:…}` comment — is ignored, not
    /// a fault, on either kind.
    #[test]
    fn hook_keys_in_settings_are_ignored() {
        let hooks = json!({
            "on_claim": {"assign_me": [true], "comment": ["claimed at @{run:cost}"]},
            "on_done": true,
            "on_fail": [{"label_remove": ["afkd/claimed"]}],
            "on_park": {"label_add": [true]},
        });
        assert_eq!(
            issue_config(&block(hooks.clone())),
            issue_config(&block(json!({})))
        );
        assert_eq!(pr_config(&block(hooks)), pr_config(&block(json!({}))));
    }

    #[test]
    fn discuss_with_parses_anyone_and_logins() {
        let parsed = |dw: Value| {
            issue_config(&block(json!({ "discuss_with": dw })))
                .expect("valid")
                .discuss_with
        };
        assert_eq!(parsed(json!(["anyone"])), Some(DiscussWith::Anyone));
        // `discuss_with alice josefandersson` is one entry of two operands.
        assert_eq!(
            parsed(json!([["alice", "josefandersson"]])),
            Some(DiscussWith::Logins(vec![
                "alice".into(),
                "josefandersson".into()
            ]))
        );
        // Two entries flatten, a list entry among them, in order.
        assert_eq!(
            parsed(json!(["björn-öst", ["alice", "陳大文"]])),
            Some(DiscussWith::Logins(vec![
                "björn-öst".into(),
                "alice".into(),
                "陳大文".into()
            ]))
        );
        // A lone non-`anyone` operand is still a one-login allow-list.
        assert_eq!(
            parsed(json!(["alice"])),
            Some(DiscussWith::Logins(vec!["alice".into()]))
        );
    }

    #[test]
    fn discuss_with_absent_is_none() {
        assert_eq!(issue_config(&block(json!({}))).unwrap().discuss_with, None);
    }

    #[test]
    fn discuss_with_valueless_faults() {
        // A bare flag, and a block-only entry, name no author.
        for dw in [json!([true]), json!([{}])] {
            let err = issue_config(&block(json!({ "discuss_with": dw }))).unwrap_err();
            assert_eq!(err.key, "discuss_with");
            assert_eq!(
                err.problem,
                "`discuss_with` expects `anyone` or a login list"
            );
        }
    }

    #[test]
    fn exactly_one_of_repo_or_org_is_required() {
        let err = issue_config(&json!({"token": "t"})).unwrap_err();
        assert_eq!(err.key, "repo");
        assert_eq!(
            err.problem,
            "a gitea trigger needs exactly one of `repo` or `org` (neither was set)"
        );
        let err = issue_config(&json!({"repo": "acme/widgets", "org": "acme", "token": "t"}))
            .unwrap_err();
        assert_eq!(err.key, "org");
        assert_eq!(
            err.problem,
            "a gitea trigger takes exactly one of `repo` or `org` (both were set)"
        );
        let cfg = issue_config(&json!({"org": "acme", "token": "PAT"})).expect("org alone");
        assert_eq!(cfg.org, "acme");
        // A flag-shaped `repo` is no repo at all, exactly as the built-in reads it.
        assert!(issue_config(&json!({"repo": true, "token": "t"})).is_err());
    }

    /// A value beside a block rides as `@value`; the built-in reads the value whatever
    /// block was written with it, and so does this.
    #[test]
    fn a_value_beside_a_block_is_read_as_the_value() {
        let cfg = issue_config(&json!({
            "repo": {"@value": "acme/widgets"},
            "token": "PAT",
            "discuss_with": [{"@value": "anyone"}],
        }))
        .expect("valid");
        assert_eq!(cfg.repo, "acme/widgets");
        assert_eq!(cfg.discuss_with, Some(DiscussWith::Anyone));
    }

    /// `author_me` is a typed `bool`, which afkd lowers to its word: `"true"` is on, and
    /// `"false"` — the manifest's default — or an absent key is off. Presence alone no
    /// longer turns it on.
    #[test]
    fn author_me_reads_the_lowered_bool() {
        let author_me = |extra: Value| pr_config(&block(extra)).unwrap().author_me;
        assert!(author_me(json!({"author_me": "true"})));
        assert!(author_me(json!({"author_me": {"@value": "true"}})));
        assert!(!author_me(json!({"author_me": "false"})));
        assert!(!author_me(json!({})));
    }

    /// The PR kind holds the rules it shares with the issue kind in the same sentences,
    /// and reads none of the keys only the issue kind declares.
    #[test]
    fn pr_config_holds_the_shared_rules() {
        let err = pr_config(&json!({"token": "t"})).unwrap_err();
        assert_eq!(
            (err.key.as_str(), err.problem.as_str()),
            (
                "repo",
                "a gitea trigger needs exactly one of `repo` or `org` (neither was set)"
            )
        );
        let err =
            pr_config(&json!({"repo": "acme/widgets", "org": "acme", "token": "t"})).unwrap_err();
        assert_eq!(
            (err.key.as_str(), err.problem.as_str()),
            (
                "org",
                "a gitea trigger takes exactly one of `repo` or `org` (both were set)"
            )
        );

        let cfg = pr_config(&json!({
            "base_url": "https://gitea.example.com",
            "org": "acme",
            "token": "PAT",
            // Not the PR kind's keys: afkd's manifest check refuses them before a
            // `hello`, and were one to reach here it is not read.
            "source_label": "afkd/ready",
            "discuss_with": ["anyone"],
        }))
        .expect("valid");
        assert_eq!(
            (cfg.base_url.as_str(), cfg.org.as_str(), cfg.repo.as_str()),
            ("https://gitea.example.com", "acme", "")
        );
        assert_eq!(cfg.source_label, "");
        assert_eq!(cfg.discuss_with, None);
        assert!(!cfg.author_me);
    }

    #[test]
    fn debug_redacts_the_token() {
        let cfg = issue_config(&json!({"repo": "acme/widgets", "token": "SECRET-PAT-abc123"}))
            .expect("valid");
        let shown = format!("{cfg:?}");
        assert!(
            !shown.contains("SECRET-PAT-abc123"),
            "token leaked: {shown}"
        );
        assert!(shown.contains("<set>"), "presence should show: {shown}");
        assert!(format!("{:?}", GiteaConfig::default()).contains("<unset>"));
    }
}
