//! The **action vocabulary** afkd's hooks call: the eight things a hook may do to the
//! claimed card, and the decode of one `call` request into one of them.
//!
//! afkd runs the hooks (`on_claim`, `on_done`, `on_fail`, `on_park`) as code, in the
//! order they are written, and each plugin action a hook calls — `trello.move_to("Review",
//! at=top)` — crosses as one `call`, its arguments already bound and typed against the
//! manifest and any `#{…}` in a comment already interpolated. What is left here is reading
//! those arguments back, and refusing a shape only a hand-written wire could send.

use serde_json::{Map, Value};

use crate::settings::{member_ref, MemberRef};

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

/// The actions afkd may `call`, in the order the manifest declares them: what
/// `manifest.rs`'s test holds `afkd-plugin.toml` to, and [`from_call`] decodes.
#[cfg(test)]
pub(crate) const ACTIONS: &[&str] = &[
    "mark_complete",
    "archive",
    "move_to",
    "add_label",
    "remove_label",
    "add_member",
    "remove_member",
    "comment",
];

/// Decode one `call` — the action's name and its arguments by parameter name — into the
/// action it asks for, or the sentence that refuses it.
///
/// afkd binds every argument against the manifest before it sends the call, so a
/// missing, mistyped or unknown-word argument only comes from a hand-written wire; it is
/// refused rather than guessed at. An argument the action has no parameter for is
/// ignored, since the wire is additive.
pub(crate) fn from_call(
    action: &str,
    args: &Map<String, Value>,
) -> Result<LifecycleAction, String> {
    let string = |param: &str| match args.get(param) {
        Some(Value::String(value)) => Ok(value.clone()),
        Some(_) => Err(format!("{action}: parameter {param} must be a string")),
        None => Err(format!("{action}: parameter {param} is required")),
    };
    Ok(match action {
        "mark_complete" => LifecycleAction::MarkComplete,
        "archive" => LifecycleAction::Archive,
        "move_to" => LifecycleAction::MoveTo {
            list: string("list")?,
            position: match args.get("at") {
                None => ListPosition::Top,
                Some(Value::String(at)) if at == "top" => ListPosition::Top,
                Some(Value::String(at)) if at == "bottom" => ListPosition::Bottom,
                Some(at) => {
                    return Err(format!("{action}: parameter at is top or bottom, not {at}"))
                }
            },
        },
        "add_label" => LifecycleAction::AddLabel {
            name: string("label")?,
        },
        "remove_label" => LifecycleAction::RemoveLabel {
            name: string("label")?,
        },
        "add_member" => LifecycleAction::AddMember(member_ref(&string("member")?)),
        "remove_member" => LifecycleAction::RemoveMember(member_ref(&string("member")?)),
        "comment" => LifecycleAction::Comment(string("text")?),
        _ => return Err(format!("no action `{action}`")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::ME;
    use serde_json::json;

    fn call(action: &str, args: Value) -> Result<LifecycleAction, String> {
        from_call(action, args.as_object().expect("an args object"))
    }

    /// Every action in [`ACTIONS`] decodes from the arguments afkd binds for it, each name
    /// and text byte for byte: a wide list name, an emoji label, a username with
    /// diacritics, the value `me`, and a multi-line markdown comment carrying CJK and a
    /// literal `@{run:x}` that afkd did not interpolate.
    #[test]
    fn every_action_decodes_from_its_bound_arguments() {
        let markdown = "## 完了 ✅\n\n- took 3m\n- log: @{run:x}\n\nsee the run log.";
        let decoded: Vec<LifecycleAction> = [
            ("mark_complete", json!({})),
            ("archive", json!({})),
            (
                "move_to",
                json!({"list": "Blockerat / Väntar", "at": "bottom"}),
            ),
            ("add_label", json!({"label": "reviewed ✅"})),
            ("remove_label", json!({"label": "Redo"})),
            ("add_member", json!({"member": ME})),
            ("remove_member", json!({"member": "björn-öst"})),
            ("comment", json!({"text": markdown})),
        ]
        .into_iter()
        .map(|(action, args)| call(action, args).expect(action))
        .collect();
        assert_eq!(
            decoded,
            [
                LifecycleAction::MarkComplete,
                LifecycleAction::Archive,
                LifecycleAction::MoveTo {
                    list: "Blockerat / Väntar".into(),
                    position: ListPosition::Bottom,
                },
                LifecycleAction::AddLabel {
                    name: "reviewed ✅".into()
                },
                LifecycleAction::RemoveLabel {
                    name: "Redo".into()
                },
                LifecycleAction::AddMember(MemberRef::SelfMember),
                LifecycleAction::RemoveMember(MemberRef::Username("björn-öst".into())),
                LifecycleAction::Comment(markdown.into()),
            ]
        );
        assert_eq!(decoded.len(), ACTIONS.len());
    }

    /// `at` is the top when afkd leaves it out (the manifest's default) or says so, and the
    /// bottom only when it says `bottom`; an empty comment is a comment, and an argument
    /// the action has no parameter for is ignored.
    #[test]
    fn move_to_defaults_to_the_top_and_extra_args_are_ignored() {
        assert_eq!(
            call("move_to", json!({"list": "In Progress"})),
            Ok(LifecycleAction::MoveTo {
                list: "In Progress".into(),
                position: ListPosition::Top,
            })
        );
        assert_eq!(
            call(
                "move_to",
                json!({"list": "Review", "at": "top", "later": 1})
            ),
            Ok(LifecycleAction::MoveTo {
                list: "Review".into(),
                position: ListPosition::Top,
            })
        );
        assert_eq!(
            call("comment", json!({"text": ""})),
            Ok(LifecycleAction::Comment(String::new()))
        );
        assert_eq!(
            call("archive", json!({"list": "Done"})),
            Ok(LifecycleAction::Archive)
        );
    }

    /// Each shape afkd would never bind is refused with a sentence naming the action and
    /// the parameter: an unknown action, a missing parameter, one of the wrong type, and an
    /// `at` outside its two words.
    #[test]
    fn a_call_afkd_would_not_bind_is_refused_by_name() {
        for (action, args, problem) in [
            ("rename", json!({"to": "x"}), "no action `rename`"),
            ("", json!({}), "no action ``"),
            ("move_to", json!({}), "move_to: parameter list is required"),
            (
                "move_to",
                json!({"at": "top"}),
                "move_to: parameter list is required",
            ),
            (
                "move_to",
                json!({"list": ["A", "B"]}),
                "move_to: parameter list must be a string",
            ),
            (
                "move_to",
                json!({"list": "Review", "at": "sideways"}),
                "move_to: parameter at is top or bottom, not \"sideways\"",
            ),
            (
                "move_to",
                json!({"list": "Review", "at": null}),
                "move_to: parameter at is top or bottom, not null",
            ),
            (
                "add_label",
                json!({}),
                "add_label: parameter label is required",
            ),
            (
                "add_label",
                json!({"label": 7}),
                "add_label: parameter label must be a string",
            ),
            (
                "remove_label",
                json!({"name": "x"}),
                "remove_label: parameter label is required",
            ),
            (
                "add_member",
                json!({"member": true}),
                "add_member: parameter member must be a string",
            ),
            (
                "remove_member",
                json!({}),
                "remove_member: parameter member is required",
            ),
            (
                "comment",
                json!({"text": {"@value": "x"}}),
                "comment: parameter text must be a string",
            ),
        ] {
            assert_eq!(
                call(action, args.clone()),
                Err(problem.to_string()),
                "{action} {args}"
            );
        }
    }
}
