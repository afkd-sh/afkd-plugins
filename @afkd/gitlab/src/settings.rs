//! Read the `gitlab` kind's settings block — as afkd lowers it to JSON in `hello` — into
//! a typed [`GitlabConfig`], and wire its lifecycle moments to the action vocabulary.
//!
//! afkd has already held the block to the manifest before this plugin is spawned: every
//! key is one the kind's manifest table declares, `token` is present, no key is written
//! twice, and only the `on_*` keys carry a block. What is left here is the part a manifest
//! cannot say — a non-empty `project`, a label action naming one label, a `comment`
//! carrying text, and `@{run:…}` legality per moment — in the built-in trigger's own
//! sentences. The cadence and the attempt bound (`poll_interval`, `max_attempts`,
//! `follow_comments`) are afkd's and are not read here.
//!
//! The lowering (afkd's `pluginworker::settings_json`): a value is a string, several are
//! an array of strings, a bare flag is `true`, a block is an object (a value beside it
//! rides as `@value`), and a repeatable key is always an array.

use serde_json::{Map, Value};

use crate::lifecycle::{parse_block, LifecycleAction};

/// A validated `gitlab` trigger configuration.
///
/// `Debug` is **hand-written** so the `token` never reaches a diagnostic.
#[derive(Clone, PartialEq, Eq, Default)]
pub(crate) struct GitlabConfig {
    /// The GitLab instance base URL (empty → `https://gitlab.com`; the API lives under
    /// `/api/v4` of it for both cloud and self-managed).
    pub(crate) base_url: String,
    /// The project to poll, as a numeric id or a path-with-namespace
    /// (`group/subgroup/widgets`).
    pub(crate) project: String,
    /// The personal/project access token.
    pub(crate) token: String,
    /// The eligibility source label (empty when not given).
    pub(crate) source_label: String,
    /// Actions performed when work on an issue begins.
    pub(crate) on_claim: Vec<LifecycleAction>,
    /// Actions performed when an issue's work finishes successfully.
    pub(crate) on_done: Vec<LifecycleAction>,
    /// Actions performed when an issue's work fails (or parks: GitLab has no `on_park`).
    pub(crate) on_fail: Vec<LifecycleAction>,
}

/// Redact `token` so a `{:?}` of a [`GitlabConfig`] can never spill it; presence is shown
/// as `<set>`/`<unset>` so the config stays debuggable.
impl std::fmt::Debug for GitlabConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let redact = |s: &str| if s.is_empty() { "<unset>" } else { "<set>" };
        f.debug_struct("GitlabConfig")
            .field("base_url", &self.base_url)
            .field("project", &self.project)
            .field("token", &redact(&self.token))
            .field("source_label", &self.source_label)
            .field("on_claim", &self.on_claim)
            .field("on_done", &self.on_done)
            .field("on_fail", &self.on_fail)
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

/// The `gitlab` kind's full key set, exactly the built-in's `ALLOWED_ISSUE_KEYS`.
#[cfg(test)]
pub(crate) const ALLOWED_ISSUE_KEYS: &[&str] = &[
    "base_url",
    "project",
    "token",
    "source_label",
    "follow_comments",
    "max_attempts",
    "poll_interval",
    "on_claim",
    "on_done",
    "on_fail",
];

/// The keys a `gitlab` block may write more than once: none — one project, one token, one
/// `on_done`.
#[cfg(test)]
pub(crate) const REPEATABLE_ISSUE_KEYS: &[&str] = &[];

/// The keys afkd reads as a duration, compared by value across a reload.
#[cfg(test)]
pub(crate) const DURATION_KEYS: &[&str] = &["poll_interval", "follow_comments"];

/// The keys the block must carry. The built-in requires the forge `token` (it flows
/// config → child env with no process-env fallback).
#[cfg(test)]
pub(crate) const REQUIRED_ISSUE_KEYS: &[&str] = &["token"];

/// Read the lowered `settings` into a [`GitlabConfig`] for the `gitlab` kind, or report
/// the first setting it cannot use.
pub(crate) fn issue_config(settings: &Value) -> Result<GitlabConfig, SettingsError> {
    let empty = Map::new();
    let settings = settings.as_object().unwrap_or(&empty);
    let project = opt_scalar(settings, "project");
    require_project(&project)?;
    Ok(GitlabConfig {
        base_url: opt_scalar(settings, "base_url"),
        project,
        token: opt_scalar(settings, "token"),
        source_label: opt_scalar(settings, "source_label"),
        // `on_claim` runs before any fire, so a `@{run:…}` reference is illegal there;
        // the terminal moments receive the attempt's real facts.
        on_claim: parse_block(settings.get("on_claim"), false)?,
        on_done: parse_block(settings.get("on_done"), true)?,
        on_fail: parse_block(settings.get("on_fail"), true)?,
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

/// Enforce that the single `project` addressing target is set and non-empty: the kind
/// polls exactly one project (no group or second target exists).
fn require_project(project: &str) -> Result<(), SettingsError> {
    if project.is_empty() {
        Err(SettingsError::new(
            "project",
            "a gitlab trigger needs a `project` (numeric id or path-with-namespace)",
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
        let mut settings = json!({"project": "acme/sub.group/widgets", "token": "PAT"});
        for (k, v) in extra.as_object().unwrap() {
            settings[k] = v.clone();
        }
        settings
    }

    #[test]
    fn issue_config_reads_scalars_and_defaults() {
        let cfg = issue_config(&json!({
            "base_url": "https://gitlab.example.com",
            "project": "acme/sub.group/widgets",
            "token": "PAT",
            "source_label": "afkd::ready",
            "max_attempts": "2",
            "poll_interval": "1m",
            "follow_comments": "5m",
        }))
        .expect("valid");
        assert_eq!(cfg.base_url, "https://gitlab.example.com");
        assert_eq!(cfg.project, "acme/sub.group/widgets");
        assert_eq!(cfg.token, "PAT");
        assert_eq!(cfg.source_label, "afkd::ready");
        assert!(cfg.on_claim.is_empty() && cfg.on_done.is_empty() && cfg.on_fail.is_empty());

        // The optional keys default to empty; a numeric project id is a project too.
        let cfg = issue_config(&json!({"project": "4242", "token": "PAT"})).expect("valid");
        assert_eq!((cfg.base_url.as_str(), cfg.source_label.as_str()), ("", ""));
        assert_eq!(cfg.project, "4242");
    }

    /// Neither an absent nor an empty `project` is acceptable — it is the whole target —
    /// and a flag-shaped one is no project at all, exactly as the built-in reads it.
    #[test]
    fn a_missing_project_faults_with_the_in_tree_sentence() {
        for settings in [
            json!({"token": "t"}),
            json!({"project": "", "token": "t"}),
            json!({"project": true, "token": "t"}),
            json!({"project": {"assign_me": [true]}, "token": "t"}),
        ] {
            let err = issue_config(&settings).unwrap_err();
            assert_eq!(err.key, "project", "{settings}");
            assert_eq!(
                err.problem,
                "a gitlab trigger needs a `project` (numeric id or path-with-namespace)"
            );
            assert_eq!(
                err.to_string(),
                "setting `project`: a gitlab trigger needs a `project` (numeric id or \
                 path-with-namespace)"
            );
        }
    }

    /// Each moment reaches the parser under its own `run_refs_allowed`, into its own
    /// field: `on_claim` before any fire, the terminal pair after one.
    #[test]
    fn lifecycle_blocks_wire_each_moment_to_its_run_ref_rule() {
        let terminal = "done in @{run:duration} — log @{run:name}";
        let cfg = issue_config(&block(json!({
            "on_claim": {"assign_me": [true], "label_add": ["afkd::working"]},
            "on_fail": {"label_remove": ["afkd::working"], "unassign": [true]},
            "on_done": {"close": [true], "comment": [terminal]},
        })))
        .expect("valid");
        assert_eq!(
            cfg.on_claim,
            vec![
                LifecycleAction::AssignMe,
                LifecycleAction::LabelAdd("afkd::working".into()),
            ]
        );
        assert_eq!(
            cfg.on_fail,
            vec![
                LifecycleAction::LabelRemove("afkd::working".into()),
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

        // The very same comment is a hard fault at claim time.
        let err = issue_config(&block(json!({"on_claim": {"comment": [terminal]}}))).unwrap_err();
        assert_eq!(err.key, "comment");
        assert_eq!(
            err.problem,
            "`@{run:duration}` references the run's facts, but no run happens at claim time"
        );
    }

    /// Two of one verb run in the order they were written, a scoped and a non-ASCII label
    /// name cross byte for byte, and a multi-line comment keeps its lines.
    #[test]
    fn on_done_performs_two_label_adds_in_source_order() {
        let cfg = issue_config(&block(json!({
            "on_done": {
                "label_add": ["shipped", "reviewed::✅"],
                "comment": ["## 完了\n\nlanded in @{run:duration}", "see the run log"],
                "close": [true],
            },
        })))
        .expect("a repeated lifecycle action is legal");
        assert_eq!(
            cfg.on_done,
            vec![
                LifecycleAction::LabelAdd("shipped".into()),
                LifecycleAction::LabelAdd("reviewed::✅".into()),
                LifecycleAction::Comment("## 完了\n\nlanded in @{run:duration}".into()),
                LifecycleAction::Comment("see the run log".into()),
                LifecycleAction::Close,
            ]
        );
    }

    /// A malformed action value faults on its own key, in the built-in's sentence.
    #[test]
    fn a_valueless_label_action_faults() {
        let err = issue_config(&block(json!({"on_fail": {"label_remove": [true]}}))).unwrap_err();
        assert_eq!(
            (err.key.as_str(), err.problem.as_str()),
            ("label_remove", "`label_remove` expects a label name")
        );
    }

    /// A value beside a block rides as `@value`; the built-in reads the value whatever
    /// block was written with it, and so does this.
    #[test]
    fn a_value_beside_a_block_is_read_as_the_value() {
        let cfg = issue_config(&json!({
            "project": {"@value": "acme/sub.group/widgets"},
            "token": "PAT",
            "source_label": {"@value": "afkd::ready"},
        }))
        .expect("valid");
        assert_eq!(cfg.project, "acme/sub.group/widgets");
        assert_eq!(cfg.source_label, "afkd::ready");
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
        assert!(format!("{:?}", GitlabConfig::default()).contains("<unset>"));
    }
}
