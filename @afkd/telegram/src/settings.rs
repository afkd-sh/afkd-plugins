//! Read the plugin's two settings blocks: the `message` kind's, as afkd lowers it to JSON in
//! `hello`, into a [`MessageConfig`]; and the plugin's own settings file, as a child started
//! for one call gets it, into the [`Credentials`] that call is made with.
//!
//! afkd has already typed both against the manifest: every key is one it declares, `token`
//! and `chats` are present in a service, and each value is of its declared type. What is left
//! here is what a manifest cannot say: a token or a chat list that is empty, and a call made
//! where no token is written at all.
//!
//! The lowering (afkd's `plugintrigger::settings_json`): a scalar is a string, and a
//! `List[…]` is an array — of the one item, or holding the array of several.

use serde_json::{Map, Value};

use crate::client::TELEGRAM_BASE;

/// What a request reaches the Bot API with: the bot's token and the API root.
///
/// `Debug` is **hand-written** so the token reaches no diagnostic.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Credentials {
    pub(crate) token: String,
    /// The API root, Telegram's unless `base_url` names another. A testability seam, not a
    /// user-facing knob.
    pub(crate) base_url: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

/// A validated `message` trigger configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MessageConfig {
    pub(crate) credentials: Credentials,
    /// The chat ids whose messages start a run, as written; never empty.
    pub(crate) chats: Vec<String>,
}

/// One setting as the manifest declares it: its name, its `type`, and its other keys
/// with their values as the manifest spells them (`("required", "true")`).
#[cfg(test)]
pub(crate) type Declared = (
    &'static str,
    &'static str,
    &'static [(&'static str, &'static str)],
);

/// The `message` kind's settings as the manifest declares them, in its order. afkd reads the
/// manifest, never this: it is what `manifest.rs`'s test holds `afkd-plugin.toml` to.
#[cfg(test)]
pub(crate) const SETTINGS: &[Declared] = &[
    ("token", "str", &[("required", "true")]),
    ("chats", "List[str]", &[("required", "true")]),
    ("base_url", "str", &[]),
];

/// The plugin's own settings, its `[config]`, as the manifest declares them: neither
/// required, so a config that imports the plugin needs no file.
#[cfg(test)]
pub(crate) const CONFIG_SETTINGS: &[Declared] = &[("token", "str", &[]), ("base_url", "str", &[])];

/// The sentence refusing a call that finds no token.
pub(crate) const NO_TOKEN: &str =
    "no bot token: set token in plugins/@afkd/telegram.afkd, or call from a service(telegram)";

/// Read the lowered `settings` into a [`MessageConfig`], or say which setting it cannot use.
pub(crate) fn message_config(settings: &Value) -> Result<MessageConfig, String> {
    let empty = Map::new();
    let settings = settings.as_object().unwrap_or(&empty);
    let token = scalar(settings, "token").filter(|t| !t.is_empty()).ok_or(
        "setting `token`: `token` is required: write the bot's token, as BotFather gave it",
    )?;
    let chats = list(settings, "chats");
    if chats.is_empty() {
        return Err(
            "setting `chats`: `chats` is required: list the chat ids whose messages start a run"
                .into(),
        );
    }
    Ok(MessageConfig {
        credentials: Credentials {
            token,
            base_url: base_url(settings),
        },
        chats,
    })
}

/// The credentials a call made by a child started for it reaches Telegram with: the
/// plugin's settings file's `token` and `base_url`, or [`NO_TOKEN`].
pub(crate) fn call_credentials(settings: Option<&Value>) -> Result<Credentials, String> {
    let empty = Map::new();
    let settings = settings.and_then(Value::as_object).unwrap_or(&empty);
    let token = scalar(settings, "token")
        .filter(|t| !t.is_empty())
        .ok_or(NO_TOKEN)?;
    Ok(Credentials {
        token,
        base_url: base_url(settings),
    })
}

/// The block's `base_url`, else Telegram's.
fn base_url(settings: &Map<String, Value>) -> String {
    scalar(settings, "base_url")
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| TELEGRAM_BASE.to_string())
}

/// A single-valued setting's string, or `None` when it is absent or not one.
fn scalar(settings: &Map<String, Value>, key: &str) -> Option<String> {
    settings
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// A list setting's strings, flattened in order: the one item, or every item of the arrays
/// it holds. An empty string names no chat and is dropped.
fn list(settings: &Map<String, Value>, key: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut push = |v: &Value| match v {
        Value::String(s) if !s.is_empty() => out.push(s.clone()),
        Value::Array(items) => out.extend(
            items
                .iter()
                .filter_map(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
        ),
        _ => {}
    };
    match settings.get(key) {
        Some(Value::Array(items)) => items.iter().for_each(&mut push),
        Some(v) => push(v),
        None => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_service_block_reads_every_setting() {
        let cfg = message_config(&json!({
            "token": "7712345678:AAH",
            "chats": [["-1001234567890", "111111111"]],
            "base_url": "http://127.0.0.1:9",
            "every": "5s",
        }))
        .unwrap();
        assert_eq!(cfg.chats, ["-1001234567890", "111111111"]);
        assert_eq!(cfg.credentials.token, "7712345678:AAH");
        assert_eq!(cfg.credentials.base_url, "http://127.0.0.1:9");
        // One chat, as a bare item or an array of it, and no base_url: Telegram's.
        for chats in [json!("42"), json!(["42"])] {
            let cfg = message_config(&json!({"token": "t", "chats": chats})).unwrap();
            assert_eq!(cfg.chats, ["42"]);
            assert_eq!(cfg.credentials.base_url, "https://api.telegram.org");
        }
    }

    #[test]
    fn a_block_without_a_token_or_a_chat_is_refused_naming_it() {
        for (block, setting) in [
            (json!({"chats": ["42"]}), "token"),
            (json!({"token": "", "chats": ["42"]}), "token"),
            (json!({"token": "t"}), "chats"),
            (json!({"token": "t", "chats": [[]]}), "chats"),
            (json!({"token": "t", "chats": [""]}), "chats"),
        ] {
            let e = message_config(&block).unwrap_err();
            assert!(
                e.starts_with(&format!("setting `{setting}`")),
                "{block}: {e}"
            );
        }
    }

    #[test]
    fn a_call_reads_the_settings_file_or_names_it() {
        let creds = call_credentials(Some(&json!({"token": "t", "base_url": "http://x"}))).unwrap();
        assert_eq!(
            (creds.token.as_str(), creds.base_url.as_str()),
            ("t", "http://x")
        );
        for settings in [None, Some(json!({})), Some(json!({"token": ""}))] {
            assert_eq!(call_credentials(settings.as_ref()).unwrap_err(), NO_TOKEN);
        }
    }

    #[test]
    fn credentials_never_debug_the_token() {
        let creds = Credentials {
            token: "7712345678:AAHsecret".into(),
            base_url: TELEGRAM_BASE.into(),
        };
        assert!(!format!("{creds:?}").contains("AAHsecret"));
    }
}
