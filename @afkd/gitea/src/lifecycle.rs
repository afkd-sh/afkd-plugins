//! The **action vocabulary** afkd's hooks call: the six things a hook may do to the
//! claimed issue or pull request, and the decode of one `call` request into one of them.
//!
//! afkd runs the hooks (`on_claim`, `on_done`, `on_fail`, and the issue kind's `on_park`)
//! as code, in the order they are written, and each plugin action a hook calls —
//! `gitea.label_remove("afkd/claimed")` — crosses as one `call`, its arguments already
//! bound and typed against the manifest and any `#{…}` in a comment already interpolated.
//! What is left here is reading those arguments back, and refusing a shape only a
//! hand-written wire could send.

use serde_json::{Map, Value};

/// A lifecycle action performed on an issue or pull request at a moment in its life.
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

/// The actions afkd may `call`, in the order the manifest declares them: what
/// `manifest.rs`'s test holds `afkd-plugin.toml` to, and [`from_call`] decodes.
#[cfg(test)]
pub(crate) const ACTIONS: &[&str] = &[
    "assign_me",
    "unassign",
    "label_add",
    "label_remove",
    "close",
    "comment",
];

/// Decode one `call` — the action's name and its arguments by parameter name — into the
/// action it asks for, or the sentence that refuses it.
///
/// afkd binds every argument against the manifest before it sends the call, so a
/// missing or mistyped argument only comes from a hand-written wire; it is refused rather
/// than guessed at. An argument the action has no parameter for is ignored, since the wire
/// is additive.
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
        "assign_me" => LifecycleAction::AssignMe,
        "unassign" => LifecycleAction::Unassign,
        "label_add" => LifecycleAction::LabelAdd(string("label")?),
        "label_remove" => LifecycleAction::LabelRemove(string("label")?),
        "close" => LifecycleAction::Close,
        "comment" => LifecycleAction::Comment(string("text")?),
        _ => return Err(format!("no action `{action}`")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(action: &str, args: Value) -> Result<LifecycleAction, String> {
        from_call(action, args.as_object().expect("an args object"))
    }

    /// Every action in [`ACTIONS`] decodes from the arguments afkd binds for it, each
    /// label and text byte for byte: a slashed emoji label, and a multi-line markdown
    /// comment carrying CJK and a literal `#{run.x}` that afkd did not interpolate.
    #[test]
    fn every_action_decodes_from_its_bound_arguments() {
        let markdown = "## 完了 ✅\n\n- took 3m\n- log: #{run.x}\n\nsee the run log.";
        let decoded: Vec<LifecycleAction> = [
            ("assign_me", json!({})),
            ("unassign", json!({})),
            ("label_add", json!({"label": "afkd/reviewed ✅"})),
            ("label_remove", json!({"label": "afkd/claimed"})),
            ("close", json!({})),
            ("comment", json!({"text": markdown})),
        ]
        .into_iter()
        .map(|(action, args)| call(action, args).expect(action))
        .collect();
        assert_eq!(
            decoded,
            [
                LifecycleAction::AssignMe,
                LifecycleAction::Unassign,
                LifecycleAction::LabelAdd("afkd/reviewed ✅".into()),
                LifecycleAction::LabelRemove("afkd/claimed".into()),
                LifecycleAction::Close,
                LifecycleAction::Comment(markdown.into()),
            ]
        );
        assert_eq!(decoded.len(), ACTIONS.len());
    }

    /// An empty comment is a comment (the forge refusing an empty body is not ours to
    /// pre-empt), and an argument the action has no parameter for is ignored.
    #[test]
    fn an_empty_comment_decodes_and_extra_args_are_ignored() {
        assert_eq!(
            call("comment", json!({"text": ""})),
            Ok(LifecycleAction::Comment(String::new()))
        );
        assert_eq!(
            call("close", json!({"label": "afkd/claimed"})),
            Ok(LifecycleAction::Close)
        );
        assert_eq!(
            call("label_add", json!({"label": "shipped", "later": 1})),
            Ok(LifecycleAction::LabelAdd("shipped".into()))
        );
    }

    /// Each shape afkd would never bind is refused with a sentence naming the action and
    /// the parameter: an unknown action, a missing parameter, and one of the wrong type.
    #[test]
    fn a_call_afkd_would_not_bind_is_refused_by_name() {
        for (action, args, problem) in [
            ("reopen", json!({}), "no action `reopen`"),
            ("", json!({}), "no action ``"),
            (
                "label_add",
                json!({}),
                "label_add: parameter label is required",
            ),
            (
                "label_add",
                json!({"label": ["a", "b"]}),
                "label_add: parameter label must be a string",
            ),
            (
                "label_remove",
                json!({"name": "afkd/claimed"}),
                "label_remove: parameter label is required",
            ),
            (
                "label_remove",
                json!({"label": null}),
                "label_remove: parameter label must be a string",
            ),
            ("comment", json!({}), "comment: parameter text is required"),
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
