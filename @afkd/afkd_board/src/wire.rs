//! The plugin wire, as this plugin speaks it: the request afkd sends, the replies written back,
//! the handles and the unit they carry, and the budgets a reply line lives under.
//!
//! The protocol is afkd's (`docs/plugins.md`, "The trigger protocol"): newline-delimited JSON,
//! one request at a time on stdin and one reply line each on stdout. The `board` kind's child is
//! armed by a `hello` and then sent `poll`, `finish`, `release` and `call`; every other call a
//! config makes is a child afkd starts for that one `call`, sent no `hello`, with the `proto` a
//! `hello` would have carried. Nothing here is `deny_unknown_fields`, because the wire is
//! additive within a `proto`: a newer afkd's extra field must not break this plugin.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::reads::CardFields;

/// The plugin protocol version this plugin speaks.
pub(crate) const PROTO: u32 = 2;

/// The longest reply line afkd reads, **before** its newline: afkd caps a line at 64 KiB
/// including the `\n`, and a line of exactly 64 KiB not ending in one is over the cap.
pub(crate) const MAX_REPLY: usize = 64 * 1024 - 1;

/// The longest reply to a `call` afkd reads, before its newline: afkd takes a call's reply
/// up to 8 MiB including the `\n`, so a page of handles with their fields fits.
pub(crate) const MAX_CALL_REPLY: usize = 8 * 1024 * 1024 - 1;

/// One request, every field any request carries: `call` names what afkd asks. A `hello` carries
/// the protocol, the kind, the settings block and who is asking (the configured service, this
/// afkd process's claim owner, and the plugin's own directory); a `finish` and a `release` the
/// journal `key`, the first with the run's `outcome`; and a `call` the action, the arguments afkd
/// bound to its parameters by name, and, for a child started for that one call, the protocol.
#[derive(Debug, Deserialize)]
pub(crate) struct Request {
    pub(crate) call: String,
    #[serde(default)]
    pub(crate) proto: Option<u32>,
    #[serde(default)]
    pub(crate) kind: String,
    #[serde(default)]
    pub(crate) settings: Value,
    #[serde(default)]
    pub(crate) service: String,
    #[serde(default)]
    pub(crate) owner: String,
    #[serde(default)]
    pub(crate) state_dir: Option<String>,
    #[serde(default)]
    pub(crate) key: String,
    #[serde(default)]
    pub(crate) outcome: Option<String>,
    #[serde(default)]
    pub(crate) action: String,
    #[serde(default)]
    pub(crate) args: Map<String, Value>,
}

/// A handle as a reply carries it: its `id`, its key `afkd_board:<board uuid>/<object uuid>`, and
/// its fields.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct WireHandle<F> {
    pub(crate) id: String,
    pub(crate) key: String,
    pub(crate) fields: F,
}

/// One work item handed over in a `poll` reply: the claimed card as a handle, its thread, and
/// what afkd lays down for the run.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct WireUnit {
    /// The card's number as its digits.
    pub(crate) id: String,
    /// The card handle's key, which afkd journals the claim under.
    pub(crate) key: String,
    /// The card handle's fields, every one the manifest's `Card` declares.
    pub(crate) fields: CardFields,
    /// The arguments every slot is passed beside the card: `b`, the `afkd_board.Board` built from
    /// the service's `key` and `base_url` settings, shaped as a `call`'s `args` carry it.
    pub(crate) args: Map<String, Value>,
    pub(crate) thread: String,
    pub(crate) seen: Vec<String>,
    #[serde(rename = "self")]
    pub(crate) me: String,
    pub(crate) env: BTreeMap<String, String>,
    pub(crate) files: Vec<WireFile>,
}

/// One file of the attempt's scratch layout, as data: afkd writes it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct WireFile {
    pub(crate) path: String,
    pub(crate) text: String,
}

/// The `poll` reply that hands `unit` over, serialized.
pub(crate) fn fire_line(unit: &WireUnit) -> String {
    json!({ "fire": true, "unit": unit }).to_string()
}

/// The `poll` reply that hands nothing over.
pub(crate) fn idle_line() -> String {
    json!({ "fire": false }).to_string()
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

/// The ok reply carrying a value: a list of the plugin's structs, or one.
pub(crate) fn ok_value<T: Serialize>(value: &T) -> String {
    json!({ "ok": true, "value": value }).to_string()
}

/// The ok reply of an action that returns nothing.
pub(crate) fn ok_nothing() -> String {
    json!({ "ok": true }).to_string()
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

/// A `poll` reply [`fit_poll`] made fit: the line, and what it cut.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Fitted {
    pub(crate) line: String,
    /// Whether the handle's `description` field was cut.
    pub(crate) description_cut: bool,
    /// Whether the `task/README.md` brief was cut.
    pub(crate) brief_cut: bool,
}

/// `text`'s first `n` bytes and the trailer saying that `what` was cut there, of how many
/// bytes, and where the whole card is.
fn cut_text(text: &str, n: usize, what: &str) -> String {
    format!(
        "{}\n\n[afkd_board: this {what} was cut at {n} of {} bytes to fit afkd's 64 KiB plugin \
         line — read the whole card with the afkd_board skill]",
        &text[..n],
        text.len()
    )
}

/// The longest prefix of `text`, in bytes and at a char boundary, whose `line` is no longer than
/// [`MAX_REPLY`], or `None` when not even the empty prefix's is. The line only grows with the
/// prefix, so the search is a binary search over byte offsets, each rounded down to a char
/// boundary.
fn longest_fitting(text: &str, line: impl Fn(usize) -> String) -> Option<usize> {
    let floor = |mut n: usize| {
        while !text.is_char_boundary(n) {
            n -= 1;
        }
        n
    };
    if line(0).len() > MAX_REPLY {
        return None;
    }
    let (mut fits, mut over) = (0, text.len());
    while over - fits > 1 {
        let mid = floor(fits + (over - fits) / 2);
        if mid == fits {
            break;
        }
        if line(mid).len() <= MAX_REPLY {
            fits = mid;
        } else {
            over = mid;
        }
    }
    Some(fits)
}

/// Fit a `poll` reply under [`MAX_REPLY`], and return the line and what was cut: nothing when it
/// fits as it is, else the handle's `description` field, the brief whole beside it if it can be,
/// and only then the unit's `task/README.md` text too.
///
/// Each cut keeps the longest prefix, at a char boundary, that still fits beside a trailer
/// saying what was cut and where the whole card is. `None` when not even both trailers alone
/// fit, which only a unit whose *other* fields overflow can reach.
pub(crate) fn fit_poll(unit: &WireUnit) -> Option<Fitted> {
    let line = fire_line(unit);
    if line.len() <= MAX_REPLY {
        return Some(Fitted {
            line,
            description_cut: false,
            brief_cut: false,
        });
    }
    let description = &unit.fields.description;
    let with_description = |n: usize| {
        let mut cut = unit.clone();
        cut.fields.description = cut_text(description, n, "description");
        cut
    };
    let mut unit = unit.clone();
    let mut description_cut = false;
    if !description.is_empty() {
        if let Some(n) = longest_fitting(description, |n| fire_line(&with_description(n))) {
            return Some(Fitted {
                line: fire_line(&with_description(n)),
                description_cut: true,
                brief_cut: false,
            });
        }
        // Not even the description's trailer alone fits beside the brief: keep the shorter of
        // the two, and cut the brief.
        let bare = with_description(0);
        if bare.fields.description.len() < description.len() {
            unit = bare;
            description_cut = true;
        }
    }
    let brief = unit.files.iter().position(|f| f.path == crate::TASK_FILE)?;
    let text = unit.files[brief].text.clone();
    let with_brief = |n: usize| {
        let mut cut = unit.clone();
        cut.files[brief].text = cut_text(&text, n, "brief");
        fire_line(&cut)
    };
    let n = longest_fitting(&text, with_brief)?;
    Some(Fitted {
        line: with_brief(n),
        description_cut,
        brief_cut: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(value: Value) -> Request {
        serde_json::from_value(value).unwrap()
    }

    fn fields(description: &str) -> CardFields {
        CardFields {
            number: 12,
            title: "修复 the retry storm 🚨".into(),
            description: description.into(),
            list: "Todo".into(),
            labels: vec!["afkd/claimed".into()],
            archived: false,
            done: false,
            blocked_by: vec![],
            blocking: vec![],
            blocked: false,
            url: "https://app.afkd.sh/acme/boards/train/12".into(),
            created_at: 1_794_648_600_000,
            updated_at: 1_794_648_660_000,
        }
    }

    fn board_args() -> Map<String, Value> {
        let Value::Object(args) = json!({"b": {"key": "k", "base_url": "http://x"}}) else {
            unreachable!()
        };
        args
    }

    /// A fired unit carries `args.b`, the board, which afkd refuses a unit without.
    #[test]
    fn a_fired_unit_carries_the_board_in_its_args() {
        let line = fire_line(&unit_with("D", "brief"));
        let reply: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(
            reply["unit"]["args"],
            json!({"b": {"key": "k", "base_url": "http://x"}})
        );
    }

    /// A unit as the plugin hands one over, the card's description `description` beside the
    /// brief `text`.
    fn unit_with(description: &str, text: &str) -> WireUnit {
        WireUnit {
            id: "12".into(),
            key: "afkd_board:b-1/c-12".into(),
            fields: fields(description),
            args: board_args(),
            thread: "afkd_board:b-1/c-12".into(),
            seen: vec![],
            me: "afkd-4242".into(),
            env: BTreeMap::from([("AFKD_BOARD_URL".to_string(), "http://x".to_string())]),
            files: vec![WireFile {
                path: crate::TASK_FILE.into(),
                text: text.into(),
            }],
        }
    }

    /// A call decodes with its protocol, its action and its arguments, and a newer afkd's
    /// extra fields are ignored.
    #[test]
    fn a_call_decodes_with_its_action_and_arguments() {
        let r = request(json!({"call": "call", "proto": 2, "action": "lists",
                               "args": {"b": {"key": "k"}}, "settings": {}, "state_dir": "/s",
                               "later": 1}));
        assert_eq!(r.call, "call");
        assert_eq!(r.proto, Some(2));
        assert_eq!(r.action, "lists");
        assert_eq!(r.args["b"]["key"], "k");
    }

    /// The `hello` afkd writes decodes with its identity and its directory, and one that
    /// leaves them out decodes to the empty ones, which the arming refuses by name.
    #[test]
    fn a_hello_decodes_with_and_without_its_identity() {
        let hello = request(json!({"call": "hello", "proto": 2, "kind": "board",
                                   "settings": {"claim_from": "Todo"}, "service": "develop",
                                   "owner": "afkd-4242", "state_dir": "/s/afkd_board"}));
        assert_eq!(hello.kind, "board");
        assert_eq!(hello.settings["claim_from"], "Todo");
        assert_eq!(
            (hello.service.as_str(), hello.owner.as_str()),
            ("develop", "afkd-4242")
        );
        assert_eq!(hello.state_dir.as_deref(), Some("/s/afkd_board"));
        let bare = request(json!({"call": "hello", "proto": 2, "kind": "board"}));
        assert!(bare.service.is_empty() && bare.owner.is_empty() && bare.state_dir.is_none());
        assert!(bare.settings.is_null());
    }

    /// A `finish` and a `release` carry the journal key, and the first the run's outcome.
    #[test]
    fn a_finish_and_a_release_decode_their_key() {
        let finish = request(
            json!({"call": "finish", "id": "12", "key": "afkd_board:b/c",
                                    "outcome": "clean", "facts": {"signal": "proceed"}}),
        );
        assert_eq!(
            (finish.key.as_str(), finish.outcome.as_deref()),
            ("afkd_board:b/c", Some("clean"))
        );
        let release = request(json!({"call": "release", "key": "afkd_board:b/c"}));
        assert_eq!(
            (release.key.as_str(), release.outcome),
            ("afkd_board:b/c", None)
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

    #[test]
    fn a_reply_that_fits_is_left_alone() {
        let u = unit_with("It retries forever.", "# #12\n\nIt retries forever.");
        assert_eq!(
            fit_poll(&u),
            Some(Fitted {
                line: fire_line(&u),
                description_cut: false,
                brief_cut: false
            })
        );
    }

    /// A brief far over the line, made of multi-byte text so a byte cut would split a character:
    /// the line fits, decodes, ends in the trailer, and keeps as much of the brief as the budget
    /// allows (within one character of the limit).
    #[test]
    fn an_oversized_brief_is_cut_at_a_char_boundary_to_fit() {
        let body = "看起来不对 🚨 — ".repeat(8_000);
        let u = unit_with("It retries forever.", &body);
        let fitted = fit_poll(&u).expect("the cut fits");
        assert!(!fitted.description_cut && fitted.brief_cut);
        assert!(fitted.line.len() <= MAX_REPLY, "{}", fitted.line.len());
        assert!(fitted.line.len() > MAX_REPLY - 8, "the cut wastes budget");
        let reply: Value = serde_json::from_str(&fitted.line).unwrap();
        let text = reply["unit"]["files"][0]["text"].as_str().unwrap();
        assert!(
            text.ends_with("read the whole card with the afkd_board skill]"),
            "{}",
            &text[text.len() - 120..]
        );
        assert!(text.contains(&format!("of {} bytes", body.len())));
        assert!(body.starts_with(text.split("\n\n[afkd_board:").next().unwrap()));
        assert_eq!(reply["unit"]["key"], "afkd_board:b-1/c-12");
        assert_eq!(reply["unit"]["env"]["AFKD_BOARD_URL"], "http://x");
        assert_eq!(
            reply["unit"]["fields"]["description"],
            "It retries forever."
        );
    }

    /// A description over the line beside a short brief is the field cut, with its trailer, and
    /// the brief is left whole.
    #[test]
    fn an_oversized_description_is_cut_and_the_brief_left_whole() {
        let description = "看起来不对 🚨 — ".repeat(8_000);
        let u = unit_with(&description, "short brief");
        let fitted = fit_poll(&u).expect("the cut fits");
        assert!(fitted.description_cut && !fitted.brief_cut);
        assert!(fitted.line.len() <= MAX_REPLY && fitted.line.len() > MAX_REPLY - 8);
        let reply: Value = serde_json::from_str(&fitted.line).unwrap();
        let cut = reply["unit"]["fields"]["description"].as_str().unwrap();
        assert!(cut.contains("this description was cut at"), "{cut}");
        assert_eq!(reply["unit"]["files"][0]["text"], "short brief");
    }

    /// A description repeated whole in the brief, over the line together: the description is cut
    /// to its trailer, and the brief to what is left.
    #[test]
    fn a_description_repeated_in_the_brief_cuts_both() {
        let description = "看起来不对 🚨 — ".repeat(8_000);
        let u = unit_with(&description, &description);
        let fitted = fit_poll(&u).expect("the cut fits");
        assert!(fitted.description_cut && fitted.brief_cut);
        assert!(fitted.line.len() <= MAX_REPLY);
        let reply: Value = serde_json::from_str(&fitted.line).unwrap();
        assert!(reply["unit"]["files"][0]["text"].as_str().unwrap().len() > 60_000);
    }

    /// A unit whose fields besides the brief overflow the line cannot be handed over at all.
    #[test]
    fn a_unit_over_the_line_without_its_brief_does_not_fit() {
        let mut u = unit_with("", "short");
        u.seen = (0..4_000).map(|i| format!("c-{i:040}")).collect();
        assert_eq!(fit_poll(&u), None);
    }
}
