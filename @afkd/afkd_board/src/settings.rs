//! The `board` kind's settings block, as afkd lowers it to JSON in `hello`, read into a typed
//! [`ServiceConfig`].
//!
//! afkd has already typed the block against the manifest before this plugin is spawned: every
//! key is one the kind declares, `key` and `claim_from` are present, and each value is of its
//! declared type. What is left here is the part a manifest cannot say: a `key` that is empty,
//! a `claim_from` that names no list or names one twice. The cadence and the attempt bound
//! (`every`, `max_attempts`) are afkd's own, declared by afkd for every claiming kind, and are
//! not read here; the slots never arrive here at all, since afkd runs them.
//!
//! The lowering (afkd's `pluginworker::settings_json`): a value is a string, a `bool` its word,
//! `"true"` or `"false"`, and a `list[…]` is an array, of the one item or of several, which a
//! block written beside the value carries as `@value`.

use std::fmt;

use serde_json::{Map, Value};

use crate::board::Board;

/// A validated `board` kind configuration. Its `Debug` leaves the key out, so nothing that
/// prints one prints the credential.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ServiceConfig {
    /// One of the board's API keys, which is never empty.
    pub(crate) key: String,
    /// The board's address.
    pub(crate) base_url: String,
    /// The lists cards are claimed from, in the order written; never empty, each named once.
    pub(crate) claim_from: Vec<String>,
    /// A card is claimed only when it carries every one of these labels; none is no gate.
    pub(crate) only_labels: Vec<String>,
    /// A card carrying any of these labels is never claimed, and skip wins over `only_labels`.
    pub(crate) skip_labels: Vec<String>,
    /// `Some(false)` claims only a card no open card blocks, `Some(true)` only a blocked one,
    /// and `None` either.
    pub(crate) only_blocked: Option<bool>,
}

impl fmt::Debug for ServiceConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServiceConfig")
            .field("base_url", &self.base_url)
            .field("claim_from", &self.claim_from)
            .field("only_labels", &self.only_labels)
            .field("skip_labels", &self.skip_labels)
            .field("only_blocked", &self.only_blocked)
            .finish_non_exhaustive()
    }
}

/// The address the board is served at when `base_url` is left out.
pub(crate) const DEFAULT_BASE_URL: &str = "https://app.afkd.sh";

/// One setting as the manifest declares it: its name, its `type`, and its other keys with their
/// values as the manifest spells them (`("default", "\"https://app.afkd.sh\"")`).
#[cfg(test)]
pub(crate) type Declared = (
    &'static str,
    &'static str,
    &'static [(&'static str, &'static str)],
);

/// The `board` kind's settings as the manifest declares them, in its order: each name, its
/// `type`, and its other keys spelled as the manifest spells their values. afkd reads the
/// manifest, never this: it is what `manifest.rs`'s test holds `afkd-plugin.toml` to, so the
/// two cannot drift apart.
///
/// `every` and `max_attempts` are not here: afkd declares them itself for every claiming kind.
#[cfg(test)]
pub(crate) const SETTINGS: &[Declared] = &[
    ("key", "str", &[("required", "true")]),
    ("base_url", "str", &[("default", "\"https://app.afkd.sh\"")]),
    ("claim_from", "list[str]", &[("required", "true")]),
    ("only_labels", "list[str]", &[]),
    ("skip_labels", "list[str]", &[]),
    ("only_blocked", "bool", &[]),
];

impl ServiceConfig {
    /// Read the lowered `settings`, or the sentence naming the first setting it cannot use.
    ///
    /// # Errors
    ///
    /// Returns the sentence for an empty `key`, a `claim_from` that is left out, names no list
    /// or names one twice, a label list of a shape other than strings, and an `only_blocked`
    /// that is neither `true` nor `false`.
    pub(crate) fn from_settings(settings: &Value) -> Result<Self, String> {
        let empty = Map::new();
        let settings = settings.as_object().unwrap_or(&empty);
        let key = scalar(settings, "key")?.unwrap_or_default();
        if key.trim().is_empty() {
            return Err(
                "setting `key`: `key` is empty; set it to one of the board's API keys, made in \
                 its Board settings"
                    .to_string(),
            );
        }
        let claim_from = strings(settings, "claim_from")?.unwrap_or_default();
        if claim_from.is_empty() {
            return Err(
                "setting `claim_from`: `claim_from` is required: name the list or lists cards \
                 are claimed from"
                    .to_string(),
            );
        }
        for (at, list) in claim_from.iter().enumerate() {
            if claim_from[..at].contains(list) {
                return Err(format!(
                    "setting `claim_from`: the list `{list}` is named twice"
                ));
            }
        }
        Ok(Self {
            key,
            base_url: scalar(settings, "base_url")?
                .filter(|url| !url.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            claim_from,
            only_labels: strings(settings, "only_labels")?.unwrap_or_default(),
            skip_labels: strings(settings, "skip_labels")?.unwrap_or_default(),
            only_blocked: flag(settings, "only_blocked")?,
        })
    }

    /// The `afkd_board.Board` every slot is passed, which the `key` and `base_url` settings
    /// build.
    pub(crate) fn board(&self) -> Board {
        Board {
            key: self.key.clone(),
            base_url: self.base_url.clone(),
        }
    }
}

/// A lowered entry's **value**: the entry itself, or the `@value` beside a block.
fn inline(entry: &Value) -> Option<&Value> {
    match entry {
        Value::Object(block) => block.get("@value"),
        value => Some(value),
    }
}

/// The scalar `key` holds, `None` when it is absent, or the sentence refusing a value that is
/// no one string.
fn scalar(settings: &Map<String, Value>, key: &str) -> Result<Option<String>, String> {
    match settings.get(key).and_then(inline) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(format!("setting `{key}`: `{key}` expects one string")),
    }
}

/// The strings `key` lists, every one its entries carry flattened in order, `None` when it is
/// absent, or the sentence refusing a value that is no string.
fn strings(settings: &Map<String, Value>, key: &str) -> Result<Option<Vec<String>>, String> {
    let Some(entries) = settings.get(key) else {
        return Ok(None);
    };
    let entries = match entries {
        Value::Array(entries) => entries.as_slice(),
        entry => std::slice::from_ref(entry),
    };
    let mut values = Vec::new();
    for value in entries.iter().filter_map(inline) {
        match value {
            Value::String(text) => values.push(text.clone()),
            Value::Array(items) => {
                for item in items {
                    let Some(text) = item.as_str() else {
                        return Err(format!("setting `{key}`: `{key}` expects names, as text"));
                    };
                    values.push(text.to_string());
                }
            }
            _ => return Err(format!("setting `{key}`: `{key}` expects names, as text")),
        }
    }
    Ok(Some(values))
}

/// The `bool` `key` holds, as afkd lowers one to its word, `None` when it is absent.
fn flag(settings: &Map<String, Value>, key: &str) -> Result<Option<bool>, String> {
    match settings.get(key).and_then(inline) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(flag)) => Ok(Some(*flag)),
        Some(Value::String(word)) if word == "true" => Ok(Some(true)),
        Some(Value::String(word)) if word == "false" => Ok(Some(false)),
        Some(_) => Err(format!("setting `{key}`: `{key}` is `true` or `false`")),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// The two required settings plus `extra`, as afkd lowers a block.
    fn block(extra: Value) -> Value {
        let mut settings = json!({"key": "afk_board_k", "claim_from": "Todo"});
        for (k, v) in extra.as_object().unwrap() {
            settings[k] = v.clone();
        }
        settings
    }

    fn config(extra: Value) -> ServiceConfig {
        ServiceConfig::from_settings(&block(extra)).unwrap()
    }

    fn refused(extra: Value) -> String {
        ServiceConfig::from_settings(&block(extra)).unwrap_err()
    }

    /// The two required settings read, and every other one defaults: the public address, no
    /// label gate and no blocked gate.
    #[test]
    fn the_required_settings_read_and_the_rest_default() {
        let cfg = config(json!({}));
        assert_eq!(cfg.key, "afk_board_k");
        assert_eq!(cfg.base_url, "https://app.afkd.sh");
        assert_eq!(cfg.claim_from, ["Todo"]);
        assert!(cfg.only_labels.is_empty() && cfg.skip_labels.is_empty());
        assert_eq!(cfg.only_blocked, None);
    }

    /// `claim_from` keeps the order written, whether afkd lowers it as one string, an array of
    /// strings, or an array holding an array, and a block's `@value` is its value.
    #[test]
    fn claim_from_keeps_the_order_in_every_lowering() {
        for lowered in [
            json!(["In review", "Todo"]),
            json!([["In review", "Todo"]]),
            json!([{"@value": "In review"}, "Todo"]),
        ] {
            assert_eq!(
                config(json!({ "claim_from": lowered })).claim_from,
                ["In review", "Todo"]
            );
        }
        assert_eq!(
            config(json!({"claim_from": "待办 🚧"})).claim_from,
            ["待办 🚧"]
        );
    }

    /// An empty key, a blank one and a left-out one are refused, and the sentence names the
    /// setting and never carries the key.
    #[test]
    fn an_empty_key_is_refused() {
        for key in [json!(""), json!("  ")] {
            assert_eq!(
                refused(json!({ "key": key })),
                "setting `key`: `key` is empty; set it to one of the board's API keys, made in \
                 its Board settings"
            );
        }
        let mut without = block(json!({}));
        without.as_object_mut().unwrap().remove("key");
        assert!(ServiceConfig::from_settings(&without)
            .unwrap_err()
            .starts_with("setting `key`: `key` is empty"));
    }

    /// A `claim_from` left out or naming nothing is refused, and one naming a list twice names
    /// the list.
    #[test]
    fn a_claim_from_that_names_no_list_or_one_twice_is_refused() {
        let mut without = block(json!({}));
        without.as_object_mut().unwrap().remove("claim_from");
        for settings in [without, block(json!({"claim_from": []}))] {
            assert_eq!(
                ServiceConfig::from_settings(&settings).unwrap_err(),
                "setting `claim_from`: `claim_from` is required: name the list or lists cards \
                 are claimed from"
            );
        }
        assert_eq!(
            refused(json!({"claim_from": ["Todo", "Doing", "Todo"]})),
            "setting `claim_from`: the list `Todo` is named twice"
        );
    }

    /// The label gates read as lists, and a value that is no name is refused naming the setting.
    #[test]
    fn the_label_gates_read_as_lists() {
        let cfg = config(json!({"only_labels": ["ready", "bug"], "skip_labels": "blocked"}));
        assert_eq!(cfg.only_labels, ["ready", "bug"]);
        assert_eq!(cfg.skip_labels, ["blocked"]);
        assert_eq!(
            refused(json!({"only_labels": [true]})),
            "setting `only_labels`: `only_labels` expects names, as text"
        );
        assert_eq!(
            refused(json!({"skip_labels": [[1]]})),
            "setting `skip_labels`: `skip_labels` expects names, as text"
        );
    }

    /// `only_blocked` is a gate only when it is written: its word, or a JSON bool, sets it, and
    /// left out it is none.
    #[test]
    fn only_blocked_is_a_gate_only_when_written() {
        for (written, gate) in [
            (json!("true"), Some(true)),
            (json!("false"), Some(false)),
            (json!(true), Some(true)),
            (json!(false), Some(false)),
            (json!({"@value": "false"}), Some(false)),
            (json!(null), None),
        ] {
            assert_eq!(
                config(json!({ "only_blocked": written })).only_blocked,
                gate
            );
        }
        assert_eq!(
            refused(json!({"only_blocked": "maybe"})),
            "setting `only_blocked`: `only_blocked` is `true` or `false`"
        );
    }

    /// The board every slot is passed is the `key` and `base_url` settings, and a debug print
    /// never shows the key.
    #[test]
    fn the_board_is_the_key_and_the_base_url() {
        let cfg = config(json!({"base_url": "http://127.0.0.1:9"}));
        assert_eq!(
            cfg.board(),
            Board {
                key: "afk_board_k".to_string(),
                base_url: "http://127.0.0.1:9".to_string(),
            }
        );
        assert!(!format!("{cfg:?}").contains("afk_board_k"));
    }
}
