//! The **lifecycle-action vocabulary** and its parser: the six actions an `on_claim` /
//! `on_done` / `on_fail` block is written in, and the parse that flattens such a block —
//! as afkd lowers it to JSON — into an ordered list of them. Ported from afkd's
//! `afkd_forge::lifecycle`, with one difference the wire forces.
//!
//! **The order is canonical, not written.** afkd lowers a block to a JSON object, and
//! every lifecycle key is repeatable, so `on_done { label_remove "x"; close }` crosses as
//! `{"close":[true],"label_remove":["x"]}`: the order *within* one verb survives (it is
//! an array) and the order *across* verbs does not. The actions therefore run in the one
//! fixed order [`ACTION_ORDER`] names — the order every documented block is written in —
//! and, within a verb, in the order the operator wrote them.

use serde_json::Value;

use crate::run_ref::{self, RunRefFault};
use crate::settings::{inline, SettingsError};

/// A lifecycle action performed on an issue at a moment in its life.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LifecycleAction {
    /// Assign the issue to the bot (the token's own user).
    AssignMe,
    /// Take the bot off the issue's assignees (release it for a human).
    Unassign,
    /// Add a named label.
    LabelAdd(String),
    /// Remove a named label — by name, never an all-clearing bare path.
    LabelRemove(String),
    /// Close the issue (state → closed).
    Close,
    /// Post a literal comment on the issue.
    Comment(String),
}

/// The lifecycle actions an `on_*` block recognizes, in afkd's own order — the order the
/// manifest declares them in, which a test holds it to.
#[cfg(test)]
pub(crate) const LIFECYCLE_KEYS: &[&str] = &[
    "assign_me",
    "unassign",
    "label_add",
    "label_remove",
    "close",
    "comment",
];

/// The order a block's actions run in (see the module doc): take the issue, swap its
/// labels, say something, hand it back, close it. Every block the operator reference
/// documents — `on_claim { assign_me; label_add … }`, `on_done { label_remove …; close }`,
/// `on_fail { label_remove …; unassign }`, a `comment …; close` — runs in its written
/// order under it.
pub(crate) const ACTION_ORDER: [&str; 6] = [
    "assign_me",
    "label_remove",
    "label_add",
    "comment",
    "unassign",
    "close",
];

/// Parse one lowered `on_*` block into its actions, in [`ACTION_ORDER`] and, within a
/// verb, in written order. An absent block, or a bare `on_done` written with no block, is
/// no actions. `run_refs_allowed` is whether this moment receives the run's facts; it
/// gates a `@{run:…}` reference in a `comment` (ADR-0064).
pub(crate) fn parse_block(
    block: Option<&Value>,
    run_refs_allowed: bool,
) -> Result<Vec<LifecycleAction>, SettingsError> {
    let Some(Value::Object(block)) = block else {
        return Ok(Vec::new());
    };
    let mut actions = Vec::new();
    for key in ACTION_ORDER {
        // Every lifecycle key is declared repeatable, so afkd always sends an array; a
        // lone value is read as a one-element one rather than refused.
        let entries = match block.get(key) {
            None => continue,
            Some(Value::Array(entries)) => entries.as_slice(),
            Some(entry) => std::slice::from_ref(entry),
        };
        for entry in entries {
            actions.push(match key {
                "assign_me" => LifecycleAction::AssignMe,
                "unassign" => LifecycleAction::Unassign,
                "close" => LifecycleAction::Close,
                "label_add" => LifecycleAction::LabelAdd(label_name(entry, key)?),
                "label_remove" => LifecycleAction::LabelRemove(label_name(entry, key)?),
                _ => LifecycleAction::Comment(comment_text(entry, run_refs_allowed)?),
            });
        }
    }
    Ok(actions)
}

/// Read a `label_add`/`label_remove` entry's value as the label name, or fault when it
/// names none — a bare flag, or the folded `label_add "a" "b"` form.
fn label_name(entry: &Value, key: &str) -> Result<String, SettingsError> {
    match inline(entry) {
        Some(Value::String(name)) => Ok(name.clone()),
        _ => Err(SettingsError::new(
            key,
            format!("`{key}` expects a label name"),
        )),
    }
}

/// Read a `comment` entry's value as the comment text, checked for `@{run:…}` legality
/// at this moment. An empty string is accepted (the API rejects an empty body — not our
/// business).
fn comment_text(entry: &Value, run_refs_allowed: bool) -> Result<String, SettingsError> {
    let Some(Value::String(text)) = inline(entry) else {
        return Err(SettingsError::new("comment", "`comment` expects a comment"));
    };
    run_ref::check(text, run_refs_allowed)
        .map_err(|fault| SettingsError::new("comment", run_ref_problem(fault)))?;
    Ok(text.clone())
}

/// Word a [`RunRefFault`] into a `comment` fault, in afkd's own sentence.
fn run_ref_problem(fault: RunRefFault) -> String {
    match fault {
        RunRefFault::ReservedContext { key } => {
            format!("`@{{run:{key}}}` references the run's facts, but no run happens at claim time")
        }
        RunRefFault::UnknownKey { key } => {
            format!(
                "unknown run fact `{key}` (valid: {})",
                run_ref::KEYS.join(", ")
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(block: Value, run_refs_allowed: bool) -> Result<Vec<LifecycleAction>, SettingsError> {
        parse_block(Some(&block), run_refs_allowed)
    }

    #[test]
    fn a_lowered_block_parses_with_its_label_names_and_texts() {
        // Slashed and non-ASCII label names, a repeated key, an empty comment, and a
        // multi-line markdown comment with wide glyphs and a run reference — carried
        // through byte for byte.
        let markdown =
            "## 完了 ✅\n\n- duration: @{run:duration}\n- cost: @{run:cost}\n\nsee the run log.";
        assert_eq!(
            parse(
                json!({
                    "label_add": ["shipped", "reviewed ✅"],
                    "comment": [markdown, ""],
                    "close": [true],
                }),
                true
            )
            .expect("valid"),
            vec![
                LifecycleAction::LabelAdd("shipped".into()),
                LifecycleAction::LabelAdd("reviewed ✅".into()),
                LifecycleAction::Comment(markdown.into()),
                LifecycleAction::Comment(String::new()),
                LifecycleAction::Close,
            ]
        );
        // The zero-output shapes: an empty block, an absent one, a bare `on_done` flag.
        assert_eq!(parse(json!({}), true).expect("valid"), vec![]);
        assert_eq!(parse_block(None, false).expect("valid"), vec![]);
        assert_eq!(parse(json!(true), false).expect("valid"), vec![]);
    }

    /// The order the wire cannot carry is made canonical: however the object's keys
    /// arrive (serde_json orders them alphabetically), all six verbs run in
    /// [`ACTION_ORDER`], and two of each run in the order they were written.
    #[test]
    fn the_action_order_is_canonical_and_keeps_source_order_within_a_verb() {
        let block = json!({
            "unassign": [true, true],
            "label_add": ["b-second", "a-first"],
            "close": [true, true],
            "comment": ["två", "一"],
            "label_remove": ["z", "y"],
            "assign_me": [true, true],
        });
        assert_eq!(
            parse(block, true).expect("valid"),
            vec![
                LifecycleAction::AssignMe,
                LifecycleAction::AssignMe,
                LifecycleAction::LabelRemove("z".into()),
                LifecycleAction::LabelRemove("y".into()),
                LifecycleAction::LabelAdd("b-second".into()),
                LifecycleAction::LabelAdd("a-first".into()),
                LifecycleAction::Comment("två".into()),
                LifecycleAction::Comment("一".into()),
                LifecycleAction::Unassign,
                LifecycleAction::Unassign,
                LifecycleAction::Close,
                LifecycleAction::Close,
            ]
        );
        assert_eq!(ACTION_ORDER.len(), LIFECYCLE_KEYS.len());
        assert!(ACTION_ORDER.iter().all(|k| LIFECYCLE_KEYS.contains(k)));
    }

    /// Every block the operator reference documents, lowered exactly as afkd lowers it,
    /// runs in the order it is written in.
    #[test]
    fn every_documented_block_runs_in_its_written_order() {
        for (block, written) in [
            (
                json!({"assign_me": [true], "label_add": ["afkd/working"]}),
                vec![
                    LifecycleAction::AssignMe,
                    LifecycleAction::LabelAdd("afkd/working".into()),
                ],
            ),
            (
                json!({"label_remove": ["afkd/working"], "close": [true]}),
                vec![
                    LifecycleAction::LabelRemove("afkd/working".into()),
                    LifecycleAction::Close,
                ],
            ),
            (
                json!({"label_remove": ["afkd/working"], "unassign": [true]}),
                vec![
                    LifecycleAction::LabelRemove("afkd/working".into()),
                    LifecycleAction::Unassign,
                ],
            ),
            (
                json!({"comment": ["Fixed in @{run:duration} for @{run:cost}."], "close": [true]}),
                vec![
                    LifecycleAction::Comment("Fixed in @{run:duration} for @{run:cost}.".into()),
                    LifecycleAction::Close,
                ],
            ),
        ] {
            assert_eq!(parse(block, true).expect("valid"), written);
        }
    }

    /// A label action that names no label — the bare flag, and the folded two-operand
    /// form — faults with afkd's own sentence, on its own key.
    #[test]
    fn a_valueless_label_add_or_a_label_list_faults_with_the_in_tree_text() {
        for (block, key) in [
            (json!({"label_add": [true]}), "label_add"),
            (json!({"label_remove": [["a", "b"]]}), "label_remove"),
            (json!({"label_add": ["ok", {}]}), "label_add"),
        ] {
            let err = parse(block, true).unwrap_err();
            assert_eq!(err.key, key);
            assert_eq!(err.problem, format!("`{key}` expects a label name"));
        }
        let err = parse(json!({"comment": [true]}), true).unwrap_err();
        assert_eq!(err.key, "comment");
        assert_eq!(err.problem, "`comment` expects a comment");
    }

    #[test]
    fn a_run_reference_is_checked_against_the_moment() {
        // No run has happened at claim time; the fault names the key it saw.
        let idiom = "log: .afkd/runs/@{service}/@{run:name}/run.log";
        let err = parse(json!({"comment": [idiom]}), false).unwrap_err();
        assert_eq!(err.key, "comment");
        assert_eq!(
            err.problem,
            "`@{run:name}` references the run's facts, but no run happens at claim time"
        );
        assert_eq!(
            parse(json!({"comment": [idiom]}), true).expect("valid"),
            vec![LifecycleAction::Comment(idiom.into())]
        );
        // An unknown key is loud, naming the valid set; at claim time the moment rule
        // preempts it.
        let err = parse(json!({"comment": ["@{run:bogus}"]}), true).unwrap_err();
        assert_eq!(
            err.problem,
            "unknown run fact `bogus` (valid: duration, cost, turns, name)"
        );
        let err = parse(json!({"comment": ["@{run:bogus}"]}), false).unwrap_err();
        assert!(
            err.problem.contains("no run happens at claim time"),
            "{}",
            err.problem
        );
    }
}
