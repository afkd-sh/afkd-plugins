//! The **action vocabulary** afkd's slots call: the eight things a slot may do to a card,
//! and the decode of one `call` request into one of them and the card it acts on.
//!
//! afkd runs the slots (`on_claim`, `on_done`, `on_fail`, `on_park`) as code, in the
//! order they are written, and each plugin action a slot calls — `trello.move_to(card,
//! "Review", at=.top)` — crosses as one `call`, its arguments already bound and typed
//! against the manifest and any `#{…}` in a comment already interpolated. The card is the
//! handle the action takes first, never an implied current one. What is left here is
//! reading those arguments back, and refusing a shape only a hand-written wire could send.

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

/// One decoded `call`: the card it acts on, by the key its handle carries, and what to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Call {
    /// afkd's claim key for the card, the handle's `key`.
    pub(crate) key: String,
    /// What to do to it.
    pub(crate) action: LifecycleAction,
}

/// The parameter every action takes first: the card it acts on, as a `trello.Card`
/// handle.
pub(crate) const HANDLE: &str = "card";

/// The actions afkd may `call`, in the order the manifest declares them: what
/// `manifest.rs`'s test holds `afkd-plugin.toml` to, and [`from_call`] decodes.
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
/// card it acts on and the action it asks for, or the sentence that refuses it.
///
/// afkd binds every argument against the manifest before it sends the call, so a
/// missing, mistyped or unknown-word argument only comes from a hand-written wire; it is
/// refused rather than guessed at. The card is read off its handle's `key` alone, the key
/// this plugin handed it over under. An argument the action has no parameter for is
/// ignored, since the wire is additive.
pub(crate) fn from_call(action: &str, args: &Map<String, Value>) -> Result<Call, String> {
    if !ACTIONS.contains(&action) {
        return Err(format!("no action `{action}`"));
    }
    let key = match args.get(HANDLE) {
        Some(Value::Object(handle)) => handle.get("key").and_then(Value::as_str),
        Some(_) => None,
        None => return Err(format!("{action}: parameter {HANDLE} is required")),
    };
    let Some(key) = key else {
        return Err(format!(
            "{action}: parameter {HANDLE} must be an item handle"
        ));
    };
    Ok(Call {
        key: key.to_string(),
        action: decode(action, args)?,
    })
}

/// The action `action` asks for with `args`, its handle aside.
fn decode(action: &str, args: &Map<String, Value>) -> Result<LifecycleAction, String> {
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

    /// The key a card's handle carries: its card and claim ObjectIds.
    const KEY: &str = "6a4dd5de1234abcd5678ef90#6a4dd5ff1234abcd5678ef91";

    /// Decode `action` over `args` as afkd binds them, the card's handle first, and hold
    /// the call to the card the handle names.
    fn call(action: &str, args: Value) -> Result<LifecycleAction, String> {
        let mut bound =
            Map::from_iter([(HANDLE.to_string(), json!({"id": "Qb7eLy2w", "key": KEY}))]);
        bound.extend(args.as_object().expect("an args object").clone());
        from_call(action, &bound).map(|call| {
            assert_eq!(call.key, KEY, "{action}");
            call.action
        })
    }

    /// Every action in [`ACTIONS`] decodes from the arguments afkd binds for it, on the card
    /// its handle names, each name
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

    /// The card is the handle afkd passes first, and a call without a usable one is
    /// refused by name before its other parameters are read: none at all, the short link
    /// alone, a handle with no key or a key that is no string. An unknown action is still
    /// refused as one, whatever it carries.
    #[test]
    fn a_call_without_a_card_handle_is_refused_by_name() {
        let required = "move_to: parameter card is required";
        let malformed = "move_to: parameter card must be an item handle";
        for (args, problem) in [
            (json!({}), required),
            (json!({"list": "Review"}), required),
            (json!({"card": null}), malformed),
            (json!({"card": "Qb7eLy2w", "list": "Review"}), malformed),
            (json!({"card": {}, "list": "Review"}), malformed),
            (json!({"card": {"id": "Qb7eLy2w"}}), malformed),
            (json!({"card": {"id": "Qb7eLy2w", "key": 7}}), malformed),
            (json!({"card": [KEY]}), malformed),
        ] {
            assert_eq!(
                from_call("move_to", args.as_object().unwrap()),
                Err(problem.to_string()),
                "{args}"
            );
        }
        assert_eq!(
            from_call(
                "rename",
                json!({"card": {"id": "x", "key": KEY}})
                    .as_object()
                    .unwrap()
            ),
            Err("no action `rename`".to_string())
        );
        // The id is the plugin's own and not read: the key alone names the card.
        assert_eq!(
            from_call(
                "archive",
                json!({"card": {"key": KEY}}).as_object().unwrap()
            ),
            Ok(Call {
                key: KEY.into(),
                action: LifecycleAction::Archive,
            })
        );
    }
}
