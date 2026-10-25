//! The **action vocabulary** afkd's slots call: the ten things a slot may do to a card,
//! and the decode of one `call` request into one of them and the card it acts on.
//!
//! afkd runs the slots (`on_claim`, `on_done`, `on_fail`, `on_park`) as code, in the
//! order they are written, and each plugin action a slot calls — `trello.move_to(card,
//! "Review", at=.top)` — crosses as one `call`, its arguments already bound and typed
//! against the manifest and any `#{…}` in a comment already interpolated. The card is the
//! handle the action takes first, never an implied current one. What is left here is
//! reading those arguments back, and refusing a shape only a hand-written wire could send.

use serde_json::{Map, Value};

use crate::attachment::Upload;
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
    /// Upload a file to the card, its bytes read when the slot called for it.
    Attach(Upload),
}

/// What one decoded `call` asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Op {
    /// A write to the card, delivered — or queued — like every other.
    Act(LifecycleAction),
    /// Upload the file at `path` as `name`: a write once the file is read, which the
    /// decode, reading arguments alone, leaves to its caller.
    Attach {
        /// The file's absolute path.
        path: String,
        /// The attachment's name on the card.
        name: String,
    },
    /// Write the newest upload named `name` to `to`: a read, never queued.
    Fetch {
        /// The attachment's name on the card.
        name: String,
        /// The absolute path to write it to.
        to: String,
    },
}

/// One decoded `call`: the card it acts on, by the key its handle carries, and what to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Call {
    /// afkd's claim key for the card, the handle's `key`.
    pub(crate) key: String,
    /// What to do to it.
    pub(crate) op: Op,
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
    "attach",
    "fetch_attachment",
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
        op: decode(action, args)?,
    })
}

/// What `action` asks for with `args`, its handle aside.
fn decode(action: &str, args: &Map<String, Value>) -> Result<Op, String> {
    let string = |param: &str| string(action, args, param);
    Ok(match action {
        "attach" => Op::Attach {
            path: string("path")?,
            name: string("name")?,
        },
        "fetch_attachment" => Op::Fetch {
            name: string("name")?,
            to: string("to")?,
        },
        _ => Op::Act(decode_act(action, args)?),
    })
}

/// The write `action` asks for with `args`.
fn decode_act(action: &str, args: &Map<String, Value>) -> Result<LifecycleAction, String> {
    let string = |param: &str| string(action, args, param);
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

/// The string argument `param` of `action`, or the sentence refusing its absence or type.
fn string(action: &str, args: &Map<String, Value>, param: &str) -> Result<String, String> {
    match args.get(param) {
        Some(Value::String(value)) => Ok(value.clone()),
        Some(_) => Err(format!("{action}: parameter {param} must be a string")),
        None => Err(format!("{action}: parameter {param} is required")),
    }
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
    fn call(action: &str, args: Value) -> Result<Op, String> {
        let mut bound =
            Map::from_iter([(HANDLE.to_string(), json!({"id": "Qb7eLy2w", "key": KEY}))]);
        bound.extend(args.as_object().expect("an args object").clone());
        from_call(action, &bound).map(|call| {
            assert_eq!(call.key, KEY, "{action}");
            call.op
        })
    }

    /// Every action in [`ACTIONS`] decodes from the arguments afkd binds for it, on the card
    /// its handle names, each name, path and text byte for byte: a wide list name, an emoji
    /// label, a username with diacritics, the member `me`, a multi-line markdown comment
    /// carrying CJK and a literal `@{run:x}` that afkd did not interpolate, and a path and
    /// an attachment name with spaces and wide glyphs.
    #[test]
    fn every_action_decodes_from_its_bound_arguments() {
        let markdown = "## 完了 ✅\n\n- took 3m\n- log: @{run:x}\n\nsee the run log.";
        let decoded: Vec<Op> = [
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
            (
                "attach",
                json!({"path": "/srv/runs/計画 1/work.patch", "name": "work – 計画.patch"}),
            ),
            (
                "fetch_attachment",
                json!({"name": "plan.md", "to": "/srv/acme/earlier plan.md"}),
            ),
        ]
        .into_iter()
        .map(|(action, args)| call(action, args).expect(action))
        .collect();
        assert_eq!(
            decoded,
            [
                Op::Act(LifecycleAction::MarkComplete),
                Op::Act(LifecycleAction::Archive),
                Op::Act(LifecycleAction::MoveTo {
                    list: "Blockerat / Väntar".into(),
                    position: ListPosition::Bottom,
                }),
                Op::Act(LifecycleAction::AddLabel {
                    name: "reviewed ✅".into()
                }),
                Op::Act(LifecycleAction::RemoveLabel {
                    name: "Redo".into()
                }),
                Op::Act(LifecycleAction::AddMember(MemberRef::SelfMember)),
                Op::Act(LifecycleAction::RemoveMember(MemberRef::Username(
                    "björn-öst".into()
                ))),
                Op::Act(LifecycleAction::Comment(markdown.into())),
                Op::Attach {
                    path: "/srv/runs/計画 1/work.patch".into(),
                    name: "work – 計画.patch".into(),
                },
                Op::Fetch {
                    name: "plan.md".into(),
                    to: "/srv/acme/earlier plan.md".into(),
                },
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
            Ok(Op::Act(LifecycleAction::MoveTo {
                list: "In Progress".into(),
                position: ListPosition::Top,
            }))
        );
        assert_eq!(
            call(
                "move_to",
                json!({"list": "Review", "at": "top", "later": 1})
            ),
            Ok(Op::Act(LifecycleAction::MoveTo {
                list: "Review".into(),
                position: ListPosition::Top,
            }))
        );
        assert_eq!(
            call("comment", json!({"text": ""})),
            Ok(Op::Act(LifecycleAction::Comment(String::new())))
        );
        assert_eq!(
            call("archive", json!({"list": "Done"})),
            Ok(Op::Act(LifecycleAction::Archive))
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
            (
                "attach",
                json!({"name": "work.patch"}),
                "attach: parameter path is required",
            ),
            (
                "attach",
                json!({"path": "/tmp/work.patch"}),
                "attach: parameter name is required",
            ),
            (
                "attach",
                json!({"path": ["/tmp/a"], "name": "a"}),
                "attach: parameter path must be a string",
            ),
            (
                "fetch_attachment",
                json!({"to": "/tmp/plan.md"}),
                "fetch_attachment: parameter name is required",
            ),
            (
                "fetch_attachment",
                json!({"name": "plan.md"}),
                "fetch_attachment: parameter to is required",
            ),
            (
                "fetch_attachment",
                json!({"name": "plan.md", "to": 7}),
                "fetch_attachment: parameter to must be a string",
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
                op: Op::Act(LifecycleAction::Archive),
            })
        );
    }
}
