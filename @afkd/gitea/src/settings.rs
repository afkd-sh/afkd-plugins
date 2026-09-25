//! Read the `gitea` kind's settings block — as afkd lowers it to JSON in `hello` — into a
//! typed [`GiteaConfig`], and wire its lifecycle moments to the action vocabulary.
//!
//! afkd has already held the block to the manifest before this plugin is spawned: every
//! key is one the manifest declares, `token` is present, only `discuss_with` is
//! written twice, and only the four `on_*` keys carry a block. What is left here is the
//! part a manifest cannot say — exactly one of `repo`/`org`, a label action naming one
//! label, a `comment` carrying text, a `discuss_with` naming someone, and `@{run:…}`
//! legality per moment — in the built-in trigger's own sentences. The cadence and the
//! attempt bound (`poll_interval`, `max_attempts`, `follow_comments`) are afkd's and are
//! not read here.
//!
//! The lowering (afkd's `pluginworker::settings_json`): a value is a string, several are
//! an array of strings, a bare flag is `true`, a block is an object (a value beside it
//! rides as `@value`), and a repeatable key is always an array.

use serde_json::{Map, Value};

use crate::lifecycle::{parse_block, LifecycleAction};

/// Whose comment counts as a reply on the `discuss_with` tail gate: plain Gitea logins
/// (a comment author *is* a login), or anyone but the bot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DiscussWith {
    /// The reserved sole operand `anyone`: any author but the bot itself.
    Anyone,
    /// Only these logins count as a reply (the bot's own never does).
    Logins(Vec<String>),
}

/// A validated `gitea` trigger configuration.
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
    /// Actions performed when work on an issue begins.
    pub(crate) on_claim: Vec<LifecycleAction>,
    /// Actions performed when an issue's work finishes successfully.
    pub(crate) on_done: Vec<LifecycleAction>,
    /// Actions performed when an issue's work fails.
    pub(crate) on_fail: Vec<LifecycleAction>,
    /// Actions performed when an issue **parks** awaiting human input. The trigger manages
    /// the `afkd/awaiting-reply` label itself; these are optional extras.
    pub(crate) on_park: Vec<LifecycleAction>,
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
            .field("on_claim", &self.on_claim)
            .field("on_done", &self.on_done)
            .field("on_fail", &self.on_fail)
            .field("on_park", &self.on_park)
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

// The built-in's vocabulary, verbatim. afkd reads the manifest, never these: they are
// what `manifest.rs`'s test holds `afkd-plugin.toml` to, so the two cannot drift apart.

/// The `gitea` kind's full key set, exactly the built-in's `ALLOWED_ISSUE_KEYS`.
#[cfg(test)]
pub(crate) const ALLOWED_ISSUE_KEYS: &[&str] = &[
    "base_url",
    "repo",
    "org",
    "token",
    "source_label",
    "discuss_with",
    "follow_comments",
    "max_attempts",
    "poll_interval",
    "on_claim",
    "on_done",
    "on_fail",
    "on_park",
];

/// The keys a `gitea` block may write more than once: the one login list.
#[cfg(test)]
pub(crate) const REPEATABLE_ISSUE_KEYS: &[&str] = &["discuss_with"];

/// The keys afkd reads as a duration, compared by value across a reload.
#[cfg(test)]
pub(crate) const DURATION_KEYS: &[&str] = &["poll_interval", "follow_comments"];

/// The keys the block must carry. The built-in requires the forge `token` (it flows
/// config → child env with no process-env fallback).
#[cfg(test)]
pub(crate) const REQUIRED_ISSUE_KEYS: &[&str] = &["token"];

/// Read the lowered `settings` into a [`GiteaConfig`] for the `gitea` kind, or report
/// the first setting it cannot use.
pub(crate) fn issue_config(settings: &Value) -> Result<GiteaConfig, SettingsError> {
    let empty = Map::new();
    let settings = settings.as_object().unwrap_or(&empty);
    let repo = opt_scalar(settings, "repo");
    let org = opt_scalar(settings, "org");
    require_exactly_one_target(&repo, &org)?;
    Ok(GiteaConfig {
        base_url: opt_scalar(settings, "base_url"),
        repo,
        org,
        token: opt_scalar(settings, "token"),
        source_label: opt_scalar(settings, "source_label"),
        // `on_claim` runs before any fire, so a `@{run:…}` reference is illegal there;
        // the three post-run moments receive the attempt's real facts.
        on_claim: parse_block(settings.get("on_claim"), false)?,
        on_done: parse_block(settings.get("on_done"), true)?,
        on_fail: parse_block(settings.get("on_fail"), true)?,
        on_park: parse_block(settings.get("on_park"), true)?,
        discuss_with: parse_discuss_with(settings)?,
    })
}

/// A lowered entry's **value** — the entry itself, or the `@value` beside a block — as
/// the built-in reads a value regardless of any block written with it.
pub(crate) fn inline(entry: &Value) -> Option<&Value> {
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
        assert!(cfg.on_claim.is_empty() && cfg.on_park.is_empty());
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

    /// Each moment reaches the parser under its own `run_refs_allowed`, into its own
    /// field: `on_claim` before any fire, the three post-run moments after one.
    #[test]
    fn lifecycle_blocks_wire_each_moment_to_its_run_ref_rule() {
        let terminal = "done in @{run:duration} — log @{run:name}";
        let cfg = issue_config(&block(json!({
            "on_claim": {"assign_me": [true], "label_add": ["afkd/claimed"]},
            "on_fail": {"label_remove": ["afkd/claimed"], "unassign": [true]},
            "on_done": {"close": [true], "comment": [terminal]},
            "on_park": {"comment": [terminal]},
        })))
        .expect("valid");
        assert_eq!(
            cfg.on_claim,
            vec![
                LifecycleAction::AssignMe,
                LifecycleAction::LabelAdd("afkd/claimed".into()),
            ]
        );
        assert_eq!(
            cfg.on_fail,
            vec![
                LifecycleAction::LabelRemove("afkd/claimed".into()),
                LifecycleAction::Unassign,
            ]
        );
        // `close` was written first, but the canonical order says something before it
        // closes the issue.
        assert_eq!(
            cfg.on_done,
            vec![
                LifecycleAction::Comment(terminal.into()),
                LifecycleAction::Close,
            ]
        );
        assert_eq!(cfg.on_park, vec![LifecycleAction::Comment(terminal.into())]);

        let err = issue_config(&block(json!({"on_claim": {"comment": [terminal]}}))).unwrap_err();
        assert_eq!(err.key, "comment");
        assert!(
            err.problem.contains("no run happens at claim time"),
            "{}",
            err.problem
        );
    }

    #[test]
    fn a_run_reference_in_a_terminal_comment_parses() {
        let cfg = issue_config(&block(json!({
            "on_done": {"comment": ["done in @{run:duration} — @{run:cost}"]},
        })))
        .expect("valid");
        assert_eq!(
            cfg.on_done,
            vec![LifecycleAction::Comment(
                "done in @{run:duration} — @{run:cost}".into()
            )]
        );
    }

    #[test]
    fn on_park_parses_for_issues() {
        let cfg = issue_config(&block(json!({
            "on_park": {"label_add": ["afkd/awaiting-reply"], "unassign": [true]},
        })))
        .expect("valid");
        assert_eq!(
            cfg.on_park,
            vec![
                LifecycleAction::LabelAdd("afkd/awaiting-reply".into()),
                LifecycleAction::Unassign,
            ]
        );
    }

    #[test]
    fn on_done_performs_two_label_adds_in_source_order() {
        let cfg = issue_config(&block(json!({
            "on_done": {
                "label_add": ["shipped", "reviewed ✅"],
                "comment": ["landed in @{run:duration}", "see the run log"],
                "close": [true],
            },
        })))
        .expect("a repeated lifecycle action is legal");
        assert_eq!(
            cfg.on_done,
            vec![
                LifecycleAction::LabelAdd("shipped".into()),
                LifecycleAction::LabelAdd("reviewed ✅".into()),
                LifecycleAction::Comment("landed in @{run:duration}".into()),
                LifecycleAction::Comment("see the run log".into()),
                LifecycleAction::Close,
            ]
        );
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
