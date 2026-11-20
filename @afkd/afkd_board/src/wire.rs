//! The plugin wire, as this plugin speaks it: the one request afkd sends a child, the reply
//! written back, the handles it carries, and the budget a reply line lives under.
//!
//! The protocol is afkd's (`docs/plugins.md`, "The trigger protocol"). A plugin that declares
//! no kind is never sent a `hello`: each call a config makes is a child of its own, sent one
//! `call` that carries the `proto` a `hello` would have. Nothing here is
//! `deny_unknown_fields`, because the wire is additive within a `proto`: a newer afkd's extra
//! field must not break this plugin.

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::reads::{self, READS};

/// The plugin protocol version this plugin speaks.
pub(crate) const PROTO: u32 = 2;

/// The longest reply to a `call` afkd reads, before its newline: afkd takes a call's reply
/// up to 8 MiB including the `\n`, so a page of handles with their fields fits.
pub(crate) const MAX_CALL_REPLY: usize = 8 * 1024 * 1024 - 1;

/// One request: `call` names what afkd asks, and a `call` request carries the action, the
/// arguments afkd bound to its parameters by name, and the protocol.
#[derive(Debug, Deserialize)]
pub(crate) struct Request {
    pub(crate) call: String,
    #[serde(default)]
    pub(crate) proto: Option<u32>,
    #[serde(default)]
    pub(crate) action: String,
    #[serde(default)]
    pub(crate) args: Map<String, Value>,
}

/// What a request gets: a reply line, or the reason the child ends without one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Answer {
    Reply(String),
    Fatal(String),
}

/// A handle as a reply carries it: its `id`, its key `afkd_board:<board uuid>/<object uuid>`, and
/// its fields.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct WireHandle<F> {
    pub(crate) id: String,
    pub(crate) key: String,
    pub(crate) fields: F,
}

/// Answer `request`. Only a `call` is served: anything else is a call of a kind, and this
/// plugin declares none, so afkd sending one is a bug the child ends on. A call of another
/// protocol, or of an action the manifest does not declare, is refused.
pub(crate) fn answer(request: Request) -> Answer {
    if request.call != "call" {
        return Answer::Fatal(format!(
            "afkd sent `{}`, and @afkd/afkd_board provides no kind",
            request.call
        ));
    }
    if request.proto != Some(PROTO) {
        let proto = request
            .proto
            .map_or_else(|| "no protocol".to_string(), |p| format!("protocol {p}"));
        return Answer::Reply(refusal(&format!(
            "afkd sent a call of {proto}, and this plugin speaks plugin protocol {PROTO}"
        )));
    }
    if !READS.contains(&request.action.as_str()) {
        return Answer::Reply(refusal(&format!(
            "no action `{}`; @afkd/afkd_board has card, cards, comments, checklist, lists and labels",
            request.action
        )));
    }
    Answer::Reply(reads::answer(&request.action, &request.args))
}

/// The `ok:false` reply refusing a call with `error`, which is diagnosed too.
pub(crate) fn refusal(error: &str) -> String {
    eprintln!("afkd-afkd_board: {error}");
    json!({ "ok": false, "error": error }).to_string()
}

/// The ok reply carrying one handle.
pub(crate) fn ok_handle<F: Serialize>(handle: &WireHandle<F>) -> String {
    json!({ "ok": true, "handle": handle }).to_string()
}

/// The ok reply carrying a list of handles.
pub(crate) fn ok_handles<F: Serialize>(handles: &[WireHandle<F>]) -> String {
    json!({ "ok": true, "handles": handles }).to_string()
}

/// The ok reply carrying a value: a list of the plugin's structs.
pub(crate) fn ok_value<T: Serialize>(value: &T) -> String {
    json!({ "ok": true, "value": value }).to_string()
}

/// `line`, the ok reply to `action`, when it fits the 8 MiB afkd reads of a call's reply; or
/// the sentence saying it does not, and how to read less.
///
/// # Errors
///
/// Returns the sentence when the line runs past [`MAX_CALL_REPLY`].
pub(crate) fn fit_call(action: &str, line: String) -> Result<String, String> {
    if line.len() <= MAX_CALL_REPLY {
        return Ok(line);
    }
    let fewer = if action == "cards" {
        "ask for fewer with limit"
    } else {
        "read fewer"
    };
    Err(format!(
        "{action}: the reply comes to {} bytes, over afkd's 8 MiB for one call; {fewer}",
        line.len()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(value: Value) -> Request {
        serde_json::from_value(value).unwrap()
    }

    /// A call decodes with its protocol, its action and its arguments, and a newer afkd's
    /// extra fields are ignored.
    #[test]
    fn a_call_decodes_with_its_action_and_arguments() {
        let r = request(json!({"call": "call", "proto": 2, "action": "lists",
                               "args": {"b": {"key": "k"}}, "settings": {}, "state_dir": "/s"}));
        assert_eq!(r.call, "call");
        assert_eq!(r.proto, Some(2));
        assert_eq!(r.action, "lists");
        assert_eq!(r.args["b"]["key"], "k");
    }

    /// A kind's call ends the child, and a call of another protocol or another action is
    /// refused with a reply.
    #[test]
    fn only_a_call_of_protocol_2_and_a_declared_action_is_served() {
        assert_eq!(
            answer(request(
                json!({"call": "hello", "proto": 2, "kind": "board"})
            )),
            Answer::Fatal("afkd sent `hello`, and @afkd/afkd_board provides no kind".to_string())
        );
        let refused = |r: Answer| match r {
            Answer::Reply(line) => serde_json::from_str::<Value>(&line).unwrap(),
            Answer::Fatal(reason) => panic!("fatal: {reason}"),
        };
        let old = refused(answer(request(
            json!({"call": "call", "proto": 1, "action": "lists"}),
        )));
        assert_eq!(
            old,
            json!({"ok": false, "error": "afkd sent a call of protocol 1, and this plugin speaks plugin protocol 2"})
        );
        let none = refused(answer(request(json!({"call": "call", "action": "lists"}))));
        assert_eq!(
            none["error"],
            "afkd sent a call of no protocol, and this plugin speaks plugin protocol 2"
        );
        let unknown = refused(answer(request(
            json!({"call": "call", "proto": 2, "action": "create_card"}),
        )));
        assert_eq!(
            unknown["error"],
            "no action `create_card`; @afkd/afkd_board has card, cards, comments, checklist, lists and labels"
        );
    }

    /// A reply at the cap fits, and one byte past it is refused with how to read less.
    #[test]
    fn a_reply_over_8_mib_is_refused() {
        let at_cap = "x".repeat(MAX_CALL_REPLY);
        assert_eq!(fit_call("lists", at_cap.clone()), Ok(at_cap));
        let over = "x".repeat(MAX_CALL_REPLY + 1);
        assert_eq!(
            fit_call("cards", over.clone()),
            Err(format!(
                "cards: the reply comes to {} bytes, over afkd's 8 MiB for one call; ask for \
                 fewer with limit",
                MAX_CALL_REPLY + 1
            ))
        );
        assert_eq!(
            fit_call("comments", over),
            Err(format!(
                "comments: the reply comes to {} bytes, over afkd's 8 MiB for one call; read \
                 fewer",
                MAX_CALL_REPLY + 1
            ))
        );
    }
}
