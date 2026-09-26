//! The **lifecycle-action vocabulary** and its parser: the eight actions an `on_claim` /
//! `on_done` / `on_fail` / `on_park` block is written in, and the parse that flattens such
//! a block — as afkd lowers it to JSON — into an ordered list of them. Ported from afkd's
//! `crates/trello/src/settings.rs`, with one difference the wire forces.
//!
//! **The order is canonical, not written.** afkd lowers a block to a JSON object, and
//! every lifecycle key is repeatable, so `on_fail { move_to "Backlog"; add_label "x" }`
//! crosses as `{"add_label":["x"],"move_to":["Backlog"]}`: the order *within* one verb
//! survives (it is an array) and the order *across* verbs does not. The actions therefore
//! run in the one fixed order [`ACTION_ORDER`] names — the order every documented block
//! is written in — and, within a verb, in the order the operator wrote them.

use serde_json::Value;

use crate::run_ref::{self, RunRefFault};
use crate::settings::{inline, member_ref, MemberRef, SettingsError};

/// Where a moved card lands in its destination list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum ListPosition {
    /// Land at the top of the list (the default).
    #[default]
    Top,
    /// Land at the bottom of the list.
    Bottom,
}

/// A lifecycle action performed on a card at a moment in its life.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LifecycleAction {
    /// Mark the card complete.
    MarkComplete,
    /// Archive the card.
    Archive,
    /// Move the card into a named list, landing at a [`ListPosition`].
    MoveTo {
        /// Destination list name.
        list: String,
        /// Where in the list the card lands.
        position: ListPosition,
    },
    /// Add a named label to the card (resolving the name on the board, creating it if
    /// absent). Names the label by its Trello **name**, not its id.
    AddLabel {
        /// The label's name on the board.
        name: String,
    },
    /// Add a member to the card. Additive and idempotent.
    AddMember(MemberRef),
    /// Remove a member from the card. The mirror of [`AddMember`](Self::AddMember).
    RemoveMember(MemberRef),
    /// Remove a named label from the card. Idempotent, and it never creates a label.
    RemoveLabel {
        /// The label's name on the board.
        name: String,
    },
    /// Post a literal comment on the card.
    Comment(String),
}

/// The lifecycle actions an `on_*` block recognizes, in the built-in's own order — the
/// order the manifest declares them in, which a test holds it to.
#[cfg(test)]
pub(crate) const LIFECYCLE_KEYS: &[&str] = &[
    "mark_complete",
    "archive",
    "move_to",
    "add_label",
    "remove_label",
    "add_member",
    "remove_member",
    "comment",
];

/// The order a block's actions run in (see the module doc): take the card, finish it,
/// move it, swap its labels, say something, archive it. Every block the operator
/// reference and the live service configs are written in — `on_claim { add_member self;
/// move_to … }`, `on_claim { move_to …; remove_label … }`, `on_done { mark_complete;
/// move_to … }`, `on_done { move_to …; comment … }`, `on_fail { move_to …; add_label … }`
/// — runs in its written order under it.
pub(crate) const ACTION_ORDER: [&str; 8] = [
    "add_member",
    "remove_member",
    "mark_complete",
    "move_to",
    "remove_label",
    "add_label",
    "comment",
    "archive",
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
                // The built-in matches these two on the key alone, whatever is beside it.
                "mark_complete" => LifecycleAction::MarkComplete,
                "archive" => LifecycleAction::Archive,
                "move_to" => move_to(entry)?,
                "add_label" => LifecycleAction::AddLabel {
                    name: named(entry, key, "a label name")?,
                },
                "remove_label" => LifecycleAction::RemoveLabel {
                    name: named(entry, key, "a label name")?,
                },
                "add_member" => {
                    LifecycleAction::AddMember(member_ref(&named(entry, key, "a member name")?))
                }
                "remove_member" => {
                    LifecycleAction::RemoveMember(member_ref(&named(entry, key, "a member name")?))
                }
                _ => LifecycleAction::Comment(comment_text(entry, run_refs_allowed)?),
            });
        }
    }
    Ok(actions)
}

/// Read a `move_to` entry: its own value names the destination list, and the `at` of the
/// block written beside it (if any) gives the position — the **first** `at`, as the
/// built-in's `opt_scalar` reads it. Anything but `bottom` is the top.
fn move_to(entry: &Value) -> Result<LifecycleAction, SettingsError> {
    let list = named(entry, "move_to", "a destination list")?;
    let at = match entry {
        Value::Object(block) => match block.get("at") {
            Some(Value::Array(ats)) => ats.first().and_then(inline),
            at => at.and_then(inline),
        },
        _ => None,
    };
    let position = match at {
        Some(Value::String(at)) if at == "bottom" => ListPosition::Bottom,
        _ => ListPosition::Top,
    };
    Ok(LifecycleAction::MoveTo { list, position })
}

/// Read an entry's value as the one name `key` takes, or fault, in the built-in's
/// sentence, when it carries none — a bare flag, or the folded `add_label "a" "b"` form.
fn named(entry: &Value, key: &str, what: &str) -> Result<String, SettingsError> {
    match inline(entry) {
        Some(Value::String(name)) => Ok(name.clone()),
        _ => Err(SettingsError::new(key, format!("`{key}` expects {what}"))),
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

    fn move_to(list: &str, position: ListPosition) -> LifecycleAction {
        LifecycleAction::MoveTo {
            list: list.into(),
            position,
        }
    }

    /// The order the wire cannot carry is made canonical: however the object's keys
    /// arrive (serde_json orders them alphabetically), all eight verbs run in
    /// [`ACTION_ORDER`], and two of each run in the order they were written.
    #[test]
    fn the_action_order_is_canonical_and_keeps_source_order_within_a_verb() {
        let block = json!({
            "archive": [true],
            "comment": ["två", "一"],
            "add_label": ["b-second", "a-first"],
            "remove_label": ["z", "y"],
            "move_to": ["Review", {"@value": "Done", "at": ["bottom"]}],
            "mark_complete": [true],
            "remove_member": ["self", "marisa"],
            "add_member": ["marisa", "self"],
        });
        assert_eq!(
            parse(block, true).expect("valid"),
            vec![
                LifecycleAction::AddMember(MemberRef::Username("marisa".into())),
                LifecycleAction::AddMember(MemberRef::SelfMember),
                LifecycleAction::RemoveMember(MemberRef::SelfMember),
                LifecycleAction::RemoveMember(MemberRef::Username("marisa".into())),
                LifecycleAction::MarkComplete,
                move_to("Review", ListPosition::Top),
                move_to("Done", ListPosition::Bottom),
                LifecycleAction::RemoveLabel { name: "z".into() },
                LifecycleAction::RemoveLabel { name: "y".into() },
                LifecycleAction::AddLabel {
                    name: "b-second".into()
                },
                LifecycleAction::AddLabel {
                    name: "a-first".into()
                },
                LifecycleAction::Comment("två".into()),
                LifecycleAction::Comment("一".into()),
                LifecycleAction::Archive,
            ]
        );
        assert_eq!(ACTION_ORDER.len(), LIFECYCLE_KEYS.len());
        assert!(ACTION_ORDER.iter().all(|k| LIFECYCLE_KEYS.contains(k)));
    }

    /// Every trello block the operator reference, the gallery and the live service
    /// configs are written in, lowered exactly as afkd lowers it, runs in the order it
    /// is written in.
    #[test]
    fn every_documented_block_runs_in_its_written_order() {
        for (block, written) in [
            // `examples/afkd.conf`, gallery 01 and 25: `on_done { mark_complete; move_to … }`.
            (
                json!({"mark_complete": [true], "move_to": [{"@value": "Done", "at": ["top"]}]}),
                vec![
                    LifecycleAction::MarkComplete,
                    move_to("Done", ListPosition::Top),
                ],
            ),
            // The selfdev service: `on_claim { add_member self; move_to … }`.
            (
                json!({"add_member": ["self"], "move_to": [{"@value": "In Progress", "at": ["top"]}]}),
                vec![
                    LifecycleAction::AddMember(MemberRef::SelfMember),
                    move_to("In Progress", ListPosition::Top),
                ],
            ),
            // …`on_done { move_to …; comment … }`…
            (
                json!({"move_to": [{"@value": "Review", "at": ["top"]}],
                       "comment": ["afkd landed this card in @{run:duration} - @{run:cost}, \
                                    @{run:turns} agent turns."]}),
                vec![
                    move_to("Review", ListPosition::Top),
                    LifecycleAction::Comment(
                        "afkd landed this card in @{run:duration} - @{run:cost}, \
                         @{run:turns} agent turns."
                            .into(),
                    ),
                ],
            ),
            // …and `on_fail { move_to …; add_label … }`.
            (
                json!({"move_to": [{"@value": "Backlog", "at": ["bottom"]}], "add_label": ["Problem"]}),
                vec![
                    move_to("Backlog", ListPosition::Bottom),
                    LifecycleAction::AddLabel {
                        name: "Problem".into(),
                    },
                ],
            ),
            // A ready-flag board: `on_claim { add_member self; move_to …; remove_label … }`.
            (
                json!({"add_member": ["self"], "move_to": [{"@value": "Pågående", "at": ["top"]}],
                       "remove_label": ["Redo"]}),
                vec![
                    LifecycleAction::AddMember(MemberRef::SelfMember),
                    move_to("Pågående", ListPosition::Top),
                    LifecycleAction::RemoveLabel {
                        name: "Redo".into(),
                    },
                ],
            ),
            // `docs/configuration.md`'s park extras: `on_park { move_to …; comment … }`.
            (
                json!({"move_to": ["Discussion"], "comment": ["parked after @{run:duration}"]}),
                vec![
                    move_to("Discussion", ListPosition::Top),
                    LifecycleAction::Comment("parked after @{run:duration}".into()),
                ],
            ),
            // A bounded-concurrency board: `on_fail { archive }`.
            (json!({"archive": [true]}), vec![LifecycleAction::Archive]),
        ] {
            assert_eq!(
                parse(block.clone(), true).expect("valid"),
                written,
                "{block}"
            );
        }
    }

    /// The `move_to` shapes afkd lowers: a bare value, a value beside a block, the block's
    /// first `at` winning (the built-in's `opt_scalar`), and anything but `bottom` — a
    /// flag, an unknown word, a list — reading as the top.
    #[test]
    fn move_to_reads_its_value_and_its_first_at() {
        for (entry, want) in [
            (json!("Done"), move_to("Done", ListPosition::Top)),
            (
                json!({"@value": "In Progress", "at": ["bottom"]}),
                move_to("In Progress", ListPosition::Bottom),
            ),
            (
                json!({"@value": "Review", "at": ["bottom", "top"]}),
                move_to("Review", ListPosition::Bottom),
            ),
            (
                json!({"@value": "Review", "at": ["top", "bottom"]}),
                move_to("Review", ListPosition::Top),
            ),
            (
                json!({"@value": "Review", "at": [true]}),
                move_to("Review", ListPosition::Top),
            ),
            (
                json!({"@value": "Review", "at": ["sideways"]}),
                move_to("Review", ListPosition::Top),
            ),
            (
                json!({"@value": "Review", "at": [["bottom", "top"]]}),
                move_to("Review", ListPosition::Top),
            ),
            (
                json!({"@value": "Review"}),
                move_to("Review", ListPosition::Top),
            ),
            (
                json!({"@value": "Blockerat / Väntar", "at": ["bottom"]}),
                move_to("Blockerat / Väntar", ListPosition::Bottom),
            ),
        ] {
            assert_eq!(
                parse(json!({ "move_to": [entry.clone()] }), true).expect("valid"),
                vec![want],
                "{entry}"
            );
        }
    }

    /// A `move_to` with no destination — a bare flag, a block with no value beside it, a
    /// folded list — faults with the built-in's sentence.
    #[test]
    fn move_to_without_a_destination_list_faults() {
        for entry in [json!(true), json!({"at": ["bottom"]}), json!(["A", "B"])] {
            let err = parse(json!({ "move_to": [entry.clone()] }), true).unwrap_err();
            assert_eq!(err.key, "move_to", "{entry}");
            assert_eq!(err.problem, "`move_to` expects a destination list");
        }
    }

    /// Each named action that names nothing faults on its own key, in the built-in's own
    /// sentence; `mark_complete` and `archive` take no value and accept any.
    #[test]
    fn a_valueless_named_action_faults_with_the_in_tree_text() {
        for (key, problem) in [
            ("add_label", "`add_label` expects a label name"),
            ("remove_label", "`remove_label` expects a label name"),
            ("add_member", "`add_member` expects a member name"),
            ("remove_member", "`remove_member` expects a member name"),
            ("comment", "`comment` expects a comment"),
        ] {
            for entry in [json!(true), json!(["a", "b"]), json!({})] {
                let err = parse(json!({ key: [entry] }), true).unwrap_err();
                assert_eq!((err.key.as_str(), err.problem.as_str()), (key, problem));
            }
        }
        assert_eq!(
            parse(json!({"mark_complete": [true, "x"], "archive": [{}]}), true).expect("valid"),
            vec![
                LifecycleAction::MarkComplete,
                LifecycleAction::MarkComplete,
                LifecycleAction::Archive,
            ]
        );
    }

    /// `self` is the reserved member operand, and any other name is a username, carried
    /// byte for byte.
    #[test]
    fn member_actions_read_self_and_a_username() {
        assert_eq!(
            parse(
                json!({"add_member": ["self", "björn-öst"], "remove_member": ["marisa"]}),
                false
            )
            .expect("valid"),
            vec![
                LifecycleAction::AddMember(MemberRef::SelfMember),
                LifecycleAction::AddMember(MemberRef::Username("björn-öst".into())),
                LifecycleAction::RemoveMember(MemberRef::Username("marisa".into())),
            ]
        );
    }

    #[test]
    fn a_lowered_block_carries_its_names_and_texts_byte_for_byte() {
        // A non-ASCII label, an empty comment, and a multi-line markdown comment with wide
        // glyphs and a run reference.
        let markdown =
            "## 完了 ✅\n\n- duration: @{run:duration}\n- cost: @{run:cost}\n\nsee the run log.";
        assert_eq!(
            parse(
                json!({"add_label": ["reviewed ✅"], "comment": [markdown, ""]}),
                true
            )
            .expect("valid"),
            vec![
                LifecycleAction::AddLabel {
                    name: "reviewed ✅".into()
                },
                LifecycleAction::Comment(markdown.into()),
                LifecycleAction::Comment(String::new()),
            ]
        );
        // The zero-output shapes: an empty block, an absent one, a bare `on_done` flag.
        assert_eq!(parse(json!({}), true).expect("valid"), vec![]);
        assert_eq!(parse_block(None, false).expect("valid"), vec![]);
        assert_eq!(parse(json!(true), false).expect("valid"), vec![]);
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
