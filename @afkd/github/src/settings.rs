//! Read a kind's settings block — as afkd lowers it to JSON in `hello` — into a typed
//! [`GithubConfig`]. The two kinds share one config struct where their keys overlap, as the
//! built-in's do.
//!
//! afkd has already typed the block against the manifest before this plugin is spawned:
//! every key is one the kind declares, `token` is present, and each value is of its
//! declared type. What is left here is the part a manifest cannot say — a non-empty
//! `repo` — in the built-in trigger's own sentences. The cadence and the attempt bound
//! (`poll_interval`, `max_attempts`, `follow_comments`) are afkd's and are not read here,
//! and the hooks never arrive here at all: afkd runs them, and each action they call is
//! one `call` ([`crate::lifecycle`]).
//!
//! The lowering (afkd's `pluginworker::settings_json`): a value is a string — a `bool` its
//! word `true` or `false`, a duration its largest whole unit.

use serde_json::{Map, Value};

/// A validated `issue` or `pr` trigger configuration: one struct covers
/// the union of both kinds' keys.
///
/// `Debug` is **hand-written** so the `token` never reaches a diagnostic.
#[derive(Clone, PartialEq, Eq, Default)]
pub(crate) struct GithubConfig {
    /// The GitHub host (empty / `github.com` → the cloud API; any other host is a GitHub
    /// Enterprise Server base resolved to `…/api/v3`).
    pub(crate) host: String,
    /// The single repository to poll, as `owner/name`.
    pub(crate) repo: String,
    /// The personal access token.
    pub(crate) token: String,
    /// The eligibility source label (issue kind only; empty when not given).
    pub(crate) source_label: String,
    /// Restrict the PR kind to the bot's own PRs (the `author_me` setting; PR kind only).
    pub(crate) author_me: bool,
}

/// Redact `token` so a `{:?}` of a [`GithubConfig`] can never spill it; presence is shown
/// as `<set>`/`<unset>` so the config stays debuggable.
impl std::fmt::Debug for GithubConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let redact = |s: &str| if s.is_empty() { "<unset>" } else { "<set>" };
        f.debug_struct("GithubConfig")
            .field("host", &self.host)
            .field("repo", &self.repo)
            .field("token", &redact(&self.token))
            .field("source_label", &self.source_label)
            .field("author_me", &self.author_me)
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
    ("host", "string", &[]),
    ("repo", "string", &[]),
    ("token", "string", &[("required", "true")]),
    ("source_label", "string", &[]),
    ("follow_comments", "duration", &[("jitter", "true")]),
    ("max_attempts", "int", &[("default", "1")]),
    (
        "poll_interval",
        "duration",
        &[("jitter", "true"), ("default", "\"30s\"")],
    ),
];

/// The `pr` kind's settings, as [`ISSUE_SETTINGS`] lists the issue kind's: the shared keys
/// with `author_me` in place of `source_label`.
#[cfg(test)]
pub(crate) const PR_SETTINGS: &[Declared] = &[
    ("host", "string", &[]),
    ("repo", "string", &[]),
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

/// Read the lowered `settings` into a [`GithubConfig`] for the `issue` kind, or report
/// the first setting it cannot use.
pub(crate) fn issue_config(settings: &Value) -> Result<GithubConfig, SettingsError> {
    let empty = Map::new();
    let settings = settings.as_object().unwrap_or(&empty);
    let mut cfg = shared_config(settings)?;
    cfg.source_label = opt_scalar(settings, "source_label");
    Ok(cfg)
}

/// Read the lowered `settings` into a [`GithubConfig`] for the `pr` kind, or report the
/// first setting it cannot use. `author_me` is a `bool`, which afkd lowers to its word:
/// `"true"` turns it on, and `"false"` or its absence leaves it off.
pub(crate) fn pr_review_config(settings: &Value) -> Result<GithubConfig, SettingsError> {
    let empty = Map::new();
    let settings = settings.as_object().unwrap_or(&empty);
    let mut cfg = shared_config(settings)?;
    cfg.author_me = opt_scalar(settings, "author_me") == "true";
    Ok(cfg)
}

/// The keys both kinds carry: the single `repo` target and the forge coordinates.
fn shared_config(settings: &Map<String, Value>) -> Result<GithubConfig, SettingsError> {
    let repo = opt_scalar(settings, "repo");
    require_repo(&repo)?;
    Ok(GithubConfig {
        host: opt_scalar(settings, "host"),
        repo,
        token: opt_scalar(settings, "token"),
        ..GithubConfig::default()
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

/// Enforce that the single `repo` addressing target is set and non-empty: the kind polls
/// exactly one repository (no org or second target exists). A `repo` that is set but not
/// `owner/name` is not refused, as the built-in does not refuse it: it claims nothing.
fn require_repo(repo: &str) -> Result<(), SettingsError> {
    if repo.is_empty() {
        Err(SettingsError::new(
            "repo",
            "a github trigger needs a `repo` (`owner/name`)",
        ))
    } else {
        Ok(())
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
            "host": "ghe.example.com",
            "repo": "acme/widgets",
            "token": "PAT",
            "source_label": "afkd/ready",
            "max_attempts": "2",
            "poll_interval": "1m",
            "follow_comments": "5m",
        }))
        .expect("valid");
        assert_eq!(cfg.host, "ghe.example.com");
        assert_eq!(cfg.repo, "acme/widgets");
        assert_eq!(cfg.token, "PAT");
        assert_eq!(cfg.source_label, "afkd/ready");

        // The optional keys default to empty: an empty host is the cloud.
        let cfg = issue_config(&json!({"repo": "acme/widgets", "token": "PAT"})).expect("valid");
        assert_eq!((cfg.host.as_str(), cfg.source_label.as_str()), ("", ""));
    }

    /// Neither an absent nor an empty `repo` is acceptable — it is the whole target — and a
    /// flag-shaped one is no repo at all, exactly as the built-in reads it, on both kinds
    /// in the one sentence.
    #[test]
    fn a_missing_repo_faults_on_both_kinds() {
        for (settings, config) in [
            json!({"token": "t"}),
            json!({"repo": "", "token": "t"}),
            json!({"repo": true, "token": "t"}),
            json!({"repo": {"assign_me": [true]}, "token": "t"}),
        ]
        .into_iter()
        .flat_map(|s| {
            [
                (s.clone(), issue_config as fn(&Value) -> _),
                (s, pr_review_config),
            ]
        }) {
            let err = config(&settings).unwrap_err();
            assert_eq!(err.key, "repo", "{settings}");
            assert_eq!(
                err.problem,
                "a github trigger needs a `repo` (`owner/name`)"
            );
            assert_eq!(
                err.to_string(),
                "setting `repo`: a github trigger needs a `repo` (`owner/name`)"
            );
        }
        // A malformed but non-empty `repo` arms, as the built-in's does: it claims nothing.
        for config in [issue_config, pr_review_config] {
            assert_eq!(
                config(&json!({"repo": "not-a-repo", "token": "t"}))
                    .expect("arms")
                    .repo,
                "not-a-repo"
            );
        }
    }

    /// A value beside a block rides as `@value`; the built-in reads the value whatever
    /// block was written with it, and so does this.
    #[test]
    fn a_value_beside_a_block_is_read_as_the_value() {
        let cfg = issue_config(&json!({
            "repo": {"@value": "acme/widgets"},
            "token": "PAT",
            "source_label": {"@value": "afkd/ready"},
        }))
        .expect("valid");
        assert_eq!(cfg.repo, "acme/widgets");
        assert_eq!(cfg.source_label, "afkd/ready");
    }

    #[test]
    fn debug_redacts_the_token() {
        let cfg = issue_config(&block(json!({"token": "SECRET-PAT-abc123"}))).expect("valid");
        let shown = format!("{cfg:?}");
        assert!(
            !shown.contains("SECRET-PAT-abc123"),
            "token leaked: {shown}"
        );
        assert!(shown.contains("<set>"), "presence should show: {shown}");
        assert!(format!("{:?}", GithubConfig::default()).contains("<unset>"));

        let cfg = pr_review_config(&block(json!({"author_me": "true"}))).expect("valid");
        assert!(format!("{cfg:?}").contains("author_me: true"), "{cfg:?}");
    }

    /// `author_me` is a typed `bool`, which afkd lowers to its word: `"true"` is on, and
    /// `"false"` — the manifest's default — or an absent key is off. Presence alone no
    /// longer turns it on, and the issue kind never reads it.
    #[test]
    fn author_me_reads_the_lowered_bool() {
        let author_me = |extra: Value| pr_review_config(&block(extra)).unwrap().author_me;
        assert!(author_me(json!({"author_me": "true"})));
        assert!(author_me(json!({"author_me": {"@value": "true"}})));
        assert!(!author_me(json!({"author_me": "false"})));
        assert!(!author_me(json!({})));
        assert!(
            !issue_config(&block(json!({"author_me": "true"})))
                .unwrap()
                .author_me
        );
    }

    /// The hooks are afkd's and never cross in `settings`; a proto 1-shaped leftover that
    /// did — an `on_claim` block, a bare `on_done`, a `@{run:…}` comment — is ignored, not
    /// a fault, on either kind.
    #[test]
    fn hook_keys_in_settings_are_ignored() {
        let hooks = json!({
            "on_claim": {"assign_me": [true], "comment": ["claimed at @{run:cost}"]},
            "on_done": true,
            "on_fail": [{"label_remove": [true]}],
        });
        for config in [issue_config, pr_review_config] {
            assert_eq!(config(&block(hooks.clone())), config(&block(json!({}))));
        }
    }

    /// The PR kind reads the keys it shares with the issue kind the same way, and not the
    /// key only the issue kind declares.
    #[test]
    fn pr_review_config_reads_the_shared_keys() {
        let cfg = pr_review_config(&json!({
            "host": "ghe.example.com",
            "repo": "acme/widgets",
            "token": "PAT",
            "author_me": "true",
            // afkd's own keys ride along unread.
            "max_attempts": "2",
            "poll_interval": "1m",
            "follow_comments": "5m",
            // Not the PR kind's key: afkd's manifest check refuses it before a `hello`,
            // and were one to reach here it is not read.
            "source_label": "afkd/ready",
        }))
        .expect("valid");
        assert_eq!(cfg.host, "ghe.example.com");
        assert_eq!(cfg.repo, "acme/widgets");
        assert_eq!(cfg.token, "PAT");
        assert!(cfg.author_me);
        assert_eq!(cfg.source_label, "");
    }
}
