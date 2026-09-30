//! The **action vocabulary** afkd's slots call: the six things a slot may do to an issue
//! or pull request, and the decode of one `call` request into one of them and the item it acts
//! on.
//!
//! afkd runs the slots (`on_claim`, `on_done`, `on_fail`, and the issue kind's `on_park`)
//! as code, in the order they are written, and each plugin action a slot calls —
//! `gitea.label_remove(issue, "afkd/claimed")` — crosses as one `call`, its arguments
//! already bound and typed against the manifest and any `#{…}` in a comment already
//! interpolated. The item is the handle the action takes first, never an implied current
//! one. What is left here is reading those arguments back, and refusing a shape only a
//! hand-written wire could send.
//!
//! A config's types have no subtyping, so one action cannot take both kinds' handles: the
//! issue kind's actions are the six [`ACTIONS`] as they are, taking a `gitea.Issue`, and
//! the pr kind's are the same six prefixed `pr_`, taking a `gitea.Pull_Request`
//! ([`Vocabulary`]).

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

/// One decoded `call`: the item it acts on, by the key its handle carries, and what to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Call {
    /// afkd's claim key for the item, the handle's `key`.
    pub(crate) key: String,
    /// What to do to it.
    pub(crate) action: LifecycleAction,
}

/// What one kind's services call: the parameter every action takes first, naming the item
/// it acts on, and the prefix on the kind's action names (none on the main kind).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Vocabulary {
    /// The handle parameter's name.
    pub(crate) handle: &'static str,
    /// What each of [`ACTIONS`] is prefixed with.
    pub(crate) prefix: &'static str,
}

/// The issue kind's: `gitea.comment(issue, …)`.
pub(crate) const ISSUE_VOCABULARY: Vocabulary = Vocabulary {
    handle: "issue",
    prefix: "",
};

/// The pr kind's: `gitea.pr_comment(pr, …)`.
pub(crate) const PR_VOCABULARY: Vocabulary = Vocabulary {
    handle: "pr",
    prefix: "pr_",
};

/// The actions afkd may `call`, unprefixed, in the order the manifest declares them for
/// each kind: what `manifest.rs`'s test holds `afkd-plugin.toml` to, and [`from_call`]
/// decodes.
pub(crate) const ACTIONS: &[&str] = &[
    "assign_me",
    "unassign",
    "label_add",
    "label_remove",
    "close",
    "comment",
];

/// Decode one `call` under a kind's `vocabulary` — the action's name and its arguments by
/// parameter name — into the item it acts on and the action it asks for, or the sentence
/// that refuses it.
///
/// afkd binds every argument against the manifest before it sends the call, so a
/// missing or mistyped argument only comes from a hand-written wire; it is refused rather
/// than guessed at. The item is read off its handle's `key` alone, the key this plugin
/// handed it over under. An action of the other kind is no action of this one. An argument
/// the action has no parameter for is ignored, since the wire is additive.
pub(crate) fn from_call(
    vocabulary: &Vocabulary,
    action: &str,
    args: &Map<String, Value>,
) -> Result<Call, String> {
    let Some(verb) = action
        .strip_prefix(vocabulary.prefix)
        .filter(|verb| ACTIONS.contains(verb))
    else {
        return Err(format!("no action `{action}`"));
    };
    let handle = vocabulary.handle;
    let key = match args.get(handle) {
        Some(Value::Object(item)) => item.get("key").and_then(Value::as_str),
        Some(_) => None,
        None => return Err(format!("{action}: parameter {handle} is required")),
    };
    let Some(key) = key else {
        return Err(format!(
            "{action}: parameter {handle} must be an item handle"
        ));
    };
    Ok(Call {
        key: key.to_string(),
        action: decode(action, verb, args)?,
    })
}

/// The action `verb` asks for with `args`, its handle aside, refused as `action`.
fn decode(action: &str, verb: &str, args: &Map<String, Value>) -> Result<LifecycleAction, String> {
    let string = |param: &str| match args.get(param) {
        Some(Value::String(value)) => Ok(value.clone()),
        Some(_) => Err(format!("{action}: parameter {param} must be a string")),
        None => Err(format!("{action}: parameter {param} is required")),
    };
    Ok(match verb {
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

    /// The key an item's handle carries: its repository, number and claim comment.
    const KEY: &str = "acme/widgets#7#123";

    /// `args` as afkd binds them for an action of `vocabulary`'s kind: the item's handle
    /// first.
    fn bound(vocabulary: &Vocabulary, args: Value) -> Map<String, Value> {
        let mut bound = Map::from_iter([(
            vocabulary.handle.to_string(),
            json!({"id": "7", "key": KEY}),
        )]);
        bound.extend(args.as_object().expect("an args object").clone());
        bound
    }

    /// Decode the issue kind's `action` over `args`, and hold the call to the issue the
    /// handle names.
    fn call(action: &str, args: Value) -> Result<LifecycleAction, String> {
        from_call(&ISSUE_VOCABULARY, action, &bound(&ISSUE_VOCABULARY, args)).map(|call| {
            assert_eq!(call.key, KEY, "{action}");
            call.action
        })
    }

    /// Every action in [`ACTIONS`] decodes from the arguments afkd binds for it, on the
    /// issue its handle names, each
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

    /// The pr kind's actions are the same six, prefixed `pr_` and taking the `pr`
    /// handle, and decode to the same actions; neither kind answers the other's names, and
    /// an issue's handle passed under the pr kind's is no `pr` at all.
    #[test]
    fn each_kind_answers_its_own_names_on_its_own_handle() {
        let args = |verb: &str| match verb {
            "label_add" | "label_remove" => json!({"label": "afkd/reviewed ✅"}),
            "comment" => json!({"text": "## 完了 ✅\n\nsee the run log."}),
            _ => json!({}),
        };
        for verb in ACTIONS {
            let prefixed = format!("pr_{verb}");
            let decoded = from_call(
                &PR_VOCABULARY,
                &prefixed,
                &bound(&PR_VOCABULARY, args(verb)),
            );
            assert_eq!(
                decoded,
                call(verb, args(verb)).map(|action| Call {
                    key: KEY.into(),
                    action
                }),
                "{prefixed}"
            );
            assert_eq!(
                from_call(&PR_VOCABULARY, verb, &bound(&PR_VOCABULARY, args(verb))),
                Err(format!("no action `{verb}`"))
            );
            assert_eq!(
                from_call(
                    &ISSUE_VOCABULARY,
                    &prefixed,
                    &bound(&ISSUE_VOCABULARY, args(verb))
                ),
                Err(format!("no action `{prefixed}`"))
            );
        }
        assert_eq!(
            from_call(
                &PR_VOCABULARY,
                "pr_close",
                &bound(&ISSUE_VOCABULARY, json!({}))
            ),
            Err("pr_close: parameter pr is required".to_string())
        );
        assert_eq!(
            from_call(
                &PR_VOCABULARY,
                "pr_comment",
                &bound(&PR_VOCABULARY, json!({}))
            ),
            Err("pr_comment: parameter text is required".to_string()),
            "a prefixed action's refusal names it whole"
        );
    }

    /// The item is the handle afkd passes first, and a call without a usable one is
    /// refused by name before its other parameters are read: none at all, the number
    /// alone, a handle with no key or a key that is no string.
    #[test]
    fn a_call_without_an_item_handle_is_refused_by_name() {
        let required = "comment: parameter issue is required";
        let malformed = "comment: parameter issue must be an item handle";
        for (args, problem) in [
            (json!({}), required),
            (json!({"text": "hej"}), required),
            (json!({"issue": null}), malformed),
            (json!({"issue": "7", "text": "hej"}), malformed),
            (json!({"issue": {}, "text": "hej"}), malformed),
            (json!({"issue": {"id": "7"}}), malformed),
            (json!({"issue": {"id": "7", "key": 7}}), malformed),
            (json!({"issue": [KEY]}), malformed),
        ] {
            assert_eq!(
                from_call(&ISSUE_VOCABULARY, "comment", args.as_object().unwrap()),
                Err(problem.to_string()),
                "{args}"
            );
        }
        // The id is the plugin's own and not read: the key alone names the item.
        assert_eq!(
            from_call(
                &ISSUE_VOCABULARY,
                "close",
                json!({"issue": {"key": KEY}}).as_object().unwrap()
            ),
            Ok(Call {
                key: KEY.into(),
                action: LifecycleAction::Close,
            })
        );
    }
}
