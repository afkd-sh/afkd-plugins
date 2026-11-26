//! Read the `card` kind's settings block — as afkd lowers it to JSON in `hello` — into a
//! typed [`BoardConfig`].
//!
//! afkd has already typed the block against the manifest before this plugin is spawned:
//! every key is one the kind declares, `board`/`api_key`/`token` are present, and each
//! value is of its declared type. What is left here is the part a manifest cannot say — a
//! list that names nothing, a `min_age` that is not a plain duration — in the built-in
//! trigger's own sentences. The cadence and the attempt bound (`every`, `max_attempts`) are
//! afkd's own, declared by afkd for every claiming kind, and are not read here; the slots
//! never arrive here at all: afkd runs them, and each read or write they call is one `call`
//! ([`crate::reads`], [`crate::writes`]). The credential settings are also the `trello.Account` every
//! slot is passed ([`BoardConfig::account`]).
//!
//! The lowering (afkd's `pluginworker::settings_json`): a value is a string (a duration
//! its largest whole unit, a range `lo..hi`), and a `list[…]` is always an array — of the
//! one item, or holding the array of several.

use std::time::Duration;

use serde_json::{Map, Value};

use crate::client::TRELLO_BASE;

/// Trello's own alias for the member the token authenticates as, which every member operand
/// reads as that member: `add_member` and `remove_member` default their `member` to it, so a
/// config writes `trello.add_member(card)`, and a setting spells it `"me"`. No username can
/// collide with it, since Trello's are at least three characters long.
pub(crate) const ME: &str = "me";

/// Who an `add_member` / `remove_member` action, an `only_members` intake gate entry or a
/// `discuss_with` entry names: [`ME`], or a board member's username.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MemberRef {
    /// The member the trigger's credentials authenticate as.
    SelfMember,
    /// A board member named by their Trello **username**.
    Username(String),
}

/// Whose comment drives a `discuss_with` grooming card: any non-afkd author, or a named
/// allow-list of board members.
///
/// The reserved sole operand `anyone` selects any author but afkd's own. A mixed
/// `discuss_with anyone alice` reads as two usernames, and the literal `anyone` will fail
/// to resolve at poll time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DiscussWith {
    /// Any comment not authored by afkd itself drives the card.
    Anyone,
    /// Only comments by these named board members drive the card.
    Members(Vec<MemberRef>),
}

/// A validated `card` trigger configuration.
///
/// `Debug` is **hand-written** so the credential fields never reach a diagnostic.
#[derive(Clone, PartialEq, Eq, Default)]
pub(crate) struct BoardConfig {
    /// The board's web address.
    pub(crate) board_address: String,
    /// Board identity derived from the address (segment after the board marker).
    pub(crate) board_id: String,
    /// API base the client is pointed at; defaults to the public Trello REST base.
    pub(crate) base_url: String,
    /// API key used to reach the board.
    pub(crate) api_key: String,
    /// Token used to reach the board.
    pub(crate) token: String,
    /// The named lists new tasks are drawn from, in the order written; never empty, since
    /// the setting is required.
    pub(crate) claim_from: Vec<String>,
    /// The member intake gate: when set, only cards at least one of these members is
    /// already on are claimed. Never empty when set.
    pub(crate) only_members: Option<Vec<MemberRef>>,
    /// The label intake gate: when set, only cards carrying every one of these labels
    /// (by exact name) are eligible to claim. Never empty when set.
    pub(crate) only_labels: Option<Vec<String>>,
    /// The negative label intake gate: a card carrying any of these labels is never
    /// claimed, and skip wins over `only_labels`.
    pub(crate) skip_labels: Vec<String>,
    /// The comment-driven grooming gate: when set, a card is claimed only if its tail —
    /// the comments after afkd's own last comment — carries a comment from an allowed
    /// author.
    pub(crate) discuss_with: Option<DiscussWith>,
    /// The age intake gate, measured from the card's creation. [`Duration::ZERO`] (the
    /// default) filters nothing.
    pub(crate) min_age: Duration,
}

impl BoardConfig {
    /// The `trello.Account` every slot is passed, as the JSON afkd hands a slot's `account`
    /// param: the credentials and API base the service's settings name.
    pub(crate) fn account(&self) -> Value {
        serde_json::json!({
            "api_key": self.api_key,
            "token": self.token,
            "base_url": self.base_url,
        })
    }
}

/// Redact the credential fields so a `{:?}` of a [`BoardConfig`] can never spill
/// `api_key`/`token`; presence is shown as `<set>`/`<unset>` so the config stays
/// debuggable.
impl std::fmt::Debug for BoardConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let redact = |s: &str| if s.is_empty() { "<unset>" } else { "<set>" };
        f.debug_struct("BoardConfig")
            .field("board_address", &self.board_address)
            .field("board_id", &self.board_id)
            .field("base_url", &self.base_url)
            .field("api_key", &redact(&self.api_key))
            .field("token", &redact(&self.token))
            .field("claim_from", &self.claim_from)
            .field("only_members", &self.only_members)
            .field("only_labels", &self.only_labels)
            .field("skip_labels", &self.skip_labels)
            .field("discuss_with", &self.discuss_with)
            .field("min_age", &self.min_age)
            .finish()
    }
}

/// A setting afkd let through that this kind cannot use: the key it is about, and the
/// problem in the built-in trigger's words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SettingsError {
    /// The setting the problem is about.
    pub(crate) key: String,
    /// What is wrong with it.
    pub(crate) problem: String,
}

impl SettingsError {
    pub(crate) fn new(key: &str, problem: impl Into<String>) -> Self {
        Self {
            key: key.to_string(),
            problem: problem.into(),
        }
    }
}

impl std::fmt::Display for SettingsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "setting `{}`: {}", self.key, self.problem)
    }
}

/// One setting as the manifest declares it: its name, its `type`, and its other keys
/// with their values as the manifest spells them (`("default", "\"30s\"")`).
#[cfg(test)]
pub(crate) type Declared = (
    &'static str,
    &'static str,
    &'static [(&'static str, &'static str)],
);

/// The `card` kind's settings as the manifest declares them, in its order: each name,
/// its `type`, and its other keys spelled as the manifest spells their values. afkd reads
/// the manifest, never this: it is what `manifest.rs`'s test holds `afkd-plugin.toml` to,
/// so the two cannot drift apart.
///
/// `every` and `max_attempts` are not here: afkd declares them itself for every claiming
/// kind, so a service writes them beside these.
#[cfg(test)]
pub(crate) const SETTINGS: &[Declared] = &[
    ("api_key", "str", &[("required", "true")]),
    ("token", "str", &[("required", "true")]),
    (
        "base_url",
        "str",
        &[("default", "\"https://api.trello.com/1\"")],
    ),
    ("board", "str", &[("required", "true")]),
    ("claim_from", "list[str]", &[("required", "true")]),
    ("only_labels", "list[str]", &[]),
    ("only_members", "list[str]", &[]),
    ("skip_labels", "list[str]", &[]),
    ("min_age", "duration", &[]),
    ("discuss_with", "list[str]", &[]),
];

/// Read the lowered `settings` into a [`BoardConfig`], or report the first setting it
/// cannot use, in the built-in's order.
pub(crate) fn board_config(settings: &Value) -> Result<BoardConfig, SettingsError> {
    let empty = Map::new();
    let settings = settings.as_object().unwrap_or(&empty);
    let board_address = opt_scalar(settings, "board").unwrap_or_default();
    Ok(BoardConfig {
        board_id: derive_board_id(&board_address),
        board_address,
        base_url: opt_scalar(settings, "base_url").unwrap_or_else(|| TRELLO_BASE.to_string()),
        api_key: opt_scalar(settings, "api_key").unwrap_or_default(),
        token: opt_scalar(settings, "token").unwrap_or_default(),
        // afkd refuses a service without it before any `hello`; this refuses a `hello`
        // that arrives without it anyway, so no armed service has a list-less scan.
        claim_from: nonempty_list(
            settings,
            "claim_from",
            "`claim_from` expects one or more list names",
        )?
        .ok_or_else(|| {
            SettingsError::new(
                "claim_from",
                "`claim_from` is required: name the list or lists new cards are claimed from",
            )
        })?,
        only_members: nonempty_list(
            settings,
            "only_members",
            "`only_members` expects one or more members",
        )?
        .map(|who| who.iter().map(|s| member_ref(s)).collect()),
        only_labels: nonempty_list(
            settings,
            "only_labels",
            "`only_labels` expects one or more label names",
        )?,
        skip_labels: nonempty_list(
            settings,
            "skip_labels",
            "`skip_labels` expects one or more label names",
        )?
        .unwrap_or_default(),
        discuss_with: nonempty_list(
            settings,
            "discuss_with",
            "`discuss_with` expects `anyone` or a member list",
        )?
        .map(|ops| {
            if ops == ["anyone"] {
                DiscussWith::Anyone
            } else {
                DiscussWith::Members(ops.iter().map(|s| member_ref(s)).collect())
            }
        }),
        min_age: parse_min_age(settings)?,
    })
}

/// Derive a board identity from a board address: the segment following the `/b/`
/// marker, falling back to the raw address.
pub(crate) fn derive_board_id(address: &str) -> String {
    if let Some(rest) = address.split("/b/").nth(1) {
        let seg = rest.split('/').next().unwrap_or("");
        if !seg.is_empty() {
            return seg.to_string();
        }
    }
    address.to_string()
}

/// The one [`ME`]-or-username rule every member operand is read by: `me`
/// (case-sensitively — Trello usernames are lowercase) selects the authed member, any
/// other names a board member by username.
pub(crate) fn member_ref(who: &str) -> MemberRef {
    match who {
        ME => MemberRef::SelfMember,
        username => MemberRef::Username(username.to_string()),
    }
}

/// A lowered entry's **value** — the entry itself, or the `@value` beside a block — as
/// the built-in reads a value regardless of any block written with it.
pub(crate) fn inline(entry: &Value) -> Option<&Value> {
    match entry {
        Value::Object(block) => block.get("@value"),
        value => Some(value),
    }
}

/// A single-valued setting's scalar, or `None` when it is absent or not one string — the
/// built-in's `opt_scalar`.
fn opt_scalar(settings: &Map<String, Value>, key: &str) -> Option<String> {
    match settings.get(key).and_then(inline) {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    }
}

/// A repeatable list gate: every value its entries carry, flattened in order (the
/// built-in's `SettingsTree::list`), `None` when the key is absent, and `problem` when it
/// was written but names nothing.
fn nonempty_list(
    settings: &Map<String, Value>,
    key: &str,
    problem: &str,
) -> Result<Option<Vec<String>>, SettingsError> {
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
            Value::String(s) => values.push(s.clone()),
            Value::Array(items) => {
                values.extend(items.iter().filter_map(Value::as_str).map(str::to_string))
            }
            // A value-less flag contributes nothing.
            _ => {}
        }
    }
    if values.is_empty() {
        return Err(SettingsError::new(key, problem));
    }
    Ok(Some(values))
}

/// The `min_age` gate: [`Duration::ZERO`] when absent, else one plain duration. A range
/// is refused — a threshold is not resampled per read — and so is a valueless key.
fn parse_min_age(settings: &Map<String, Value>) -> Result<Duration, SettingsError> {
    const KEY: &str = "min_age";
    if !settings.contains_key(KEY) {
        return Ok(Duration::ZERO);
    }
    let Some(spelling) = opt_scalar(settings, KEY) else {
        return Err(SettingsError::new(
            KEY,
            format!("setting `{KEY}` expects a single duration value"),
        ));
    };
    parse_duration(&spelling).ok_or_else(|| {
        SettingsError::new(
            KEY,
            format!("setting `{KEY}` is not a duration (try `30s`, `5m`, `1h`): `{spelling}`"),
        )
    })
}

/// afkd's own duration reader: a whole count of `ms`, `s`, `m` or `h`, with an
/// un-representable count reading as invalid rather than wrapping.
fn parse_duration(s: &str) -> Option<Duration> {
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit())?);
    let n: u64 = num.parse().ok()?;
    Some(match unit {
        "ms" => Duration::from_millis(n),
        "s" => Duration::from_secs(n),
        "m" => Duration::from_secs(n.checked_mul(60)?),
        "h" => Duration::from_secs(n.checked_mul(3600)?),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The four required settings plus `extra`, as afkd lowers a block: the minimal
    /// block afkd would let through.
    fn block(extra: Value) -> Value {
        let mut settings = json!({
            "board": "https://trello.com/b/BID/x",
            "api_key": "k",
            "token": "t",
            "claim_from": "Up for Grabs",
        });
        for (k, v) in extra.as_object().unwrap() {
            settings[k] = v.clone();
        }
        settings
    }

    fn cfg(extra: Value) -> BoardConfig {
        board_config(&block(extra)).expect("valid")
    }

    fn err(extra: Value) -> SettingsError {
        board_config(&block(extra)).unwrap_err()
    }

    #[test]
    fn derive_board_id_takes_the_segment_after_b() {
        assert_eq!(derive_board_id("https://trello.com/b/ABC/name"), "ABC");
        assert_eq!(derive_board_id("no-marker"), "no-marker");
        // `/b/` immediately followed by `/` leaves an empty segment; rather than yield "",
        // the whole address is kept as the identity.
        assert_eq!(
            derive_board_id("https://trello.com/b//name"),
            "https://trello.com/b//name"
        );
    }

    #[test]
    fn reads_scalars_and_derives_board_id() {
        let cfg = cfg(json!({}));
        assert_eq!(cfg.board_address, "https://trello.com/b/BID/x");
        assert_eq!(cfg.board_id, "BID");
        assert_eq!(cfg.api_key, "k");
        assert_eq!(cfg.token, "t");
        assert_eq!(cfg.claim_from, ["Up for Grabs"]);
    }

    #[test]
    fn defaults_apply_when_settings_absent() {
        let cfg = cfg(json!({}));
        assert_eq!(cfg.base_url, TRELLO_BASE);
        assert_eq!(cfg.claim_from, ["Up for Grabs"]);
        assert_eq!(cfg.only_members, None);
        assert_eq!(cfg.only_labels, None);
        assert!(cfg.skip_labels.is_empty());
        assert_eq!(cfg.discuss_with, None);
        assert_eq!(cfg.min_age, Duration::ZERO);
    }

    /// A block without `claim_from` names no list to claim from, so it is refused naming
    /// the setting rather than armed to sweep parked cards and never claim a new one.
    #[test]
    fn a_block_without_claim_from_is_refused_naming_it() {
        let mut settings = block(json!({}));
        settings.as_object_mut().unwrap().remove("claim_from");
        let e = board_config(&settings).unwrap_err();
        assert_eq!(e.key, "claim_from");
        assert_eq!(
            e.problem,
            "`claim_from` is required: name the list or lists new cards are claimed from"
        );
    }

    /// The old name is no stand-in: a block that writes `pick_from` where `claim_from`
    /// belongs has no `claim_from`, and is refused naming it.
    #[test]
    fn a_block_writing_pick_from_in_place_of_claim_from_is_refused_naming_claim_from() {
        let mut settings = block(json!({"pick_from": "Up for Grabs"}));
        settings.as_object_mut().unwrap().remove("claim_from");
        let e = board_config(&settings).unwrap_err();
        assert_eq!(e.key, "claim_from");
        assert_eq!(
            e.problem,
            "`claim_from` is required: name the list or lists new cards are claimed from"
        );
    }

    /// The account is the three credential settings, with `base_url` defaulting to the
    /// public API.
    #[test]
    fn account_carries_the_key_token_and_base_url() {
        assert_eq!(
            cfg(json!({})).account(),
            json!({"api_key": "k", "token": "t", "base_url": TRELLO_BASE})
        );
        assert_eq!(
            cfg(json!({"base_url": "http://127.0.0.1:9"})).account(),
            json!({"api_key": "k", "token": "t", "base_url": "http://127.0.0.1:9"})
        );
    }

    /// The hooks are afkd's and never cross in `settings`; a proto 1-shaped leftover that
    /// did — an `on_claim` block, a bare `on_done` — is ignored, not a fault.
    #[test]
    fn hook_keys_in_settings_are_ignored() {
        assert_eq!(
            cfg(json!({
                "on_claim": {"add_member": ["self"], "comment": ["claimed at @{run:cost}"]},
                "on_done": true,
                "on_fail": [{"move_to": ["Backlog"]}],
            })),
            cfg(json!({}))
        );
    }

    #[test]
    fn base_url_defaults_to_public_api_and_is_read() {
        assert_eq!(cfg(json!({})).base_url, TRELLO_BASE);
        assert_eq!(
            cfg(json!({"base_url": "http://127.0.0.1:9"})).base_url,
            "http://127.0.0.1:9"
        );
    }

    /// A value beside a block rides as `@value`; the built-in reads the value whatever
    /// block was written with it, and so does this.
    #[test]
    fn a_value_beside_a_block_is_read_as_the_value() {
        let cfg = board_config(&json!({
            "board": {"@value": "https://trello.com/b/1Rkelydw/afkd"},
            "api_key": "k",
            "token": "t",
            "claim_from": {"@value": "Up for Grabs"},
        }))
        .expect("valid");
        assert_eq!(cfg.board_id, "1Rkelydw");
        assert_eq!(cfg.claim_from, ["Up for Grabs"]);
    }

    #[test]
    fn only_members_parses_me_a_username_and_several() {
        assert_eq!(
            cfg(json!({"only_members": "me"})).only_members,
            Some(vec![MemberRef::SelfMember])
        );
        assert_eq!(
            cfg(json!({"only_members": "marisa"})).only_members,
            Some(vec![MemberRef::Username("marisa".into())])
        );
        assert_eq!(
            cfg(json!({"only_members": ["me", ["marisa", "alice"]]})).only_members,
            Some(vec![
                MemberRef::SelfMember,
                MemberRef::Username("marisa".into()),
                MemberRef::Username("alice".into()),
            ])
        );
        // `me` is the one reserved spelling: proto 1's `self` is a username now, and so is
        // a differently-cased `Me`.
        for username in ["self", "Me"] {
            assert_eq!(
                cfg(json!({"only_members": username})).only_members,
                Some(vec![MemberRef::Username(username.into())])
            );
        }
    }

    /// The label gates accept one label or many, across repeated entries, flattened in
    /// written order — the built-in's `SettingsTree::list`.
    #[test]
    fn only_labels_and_skip_labels_parse_one_or_many() {
        assert_eq!(
            cfg(json!({"only_labels": "Redo ✅"})).only_labels,
            Some(vec!["Redo ✅".to_string()])
        );
        assert_eq!(
            cfg(json!({"only_labels": [["ready", "reviewed"], "Klar"]})).only_labels,
            Some(vec![
                "ready".to_string(),
                "reviewed".to_string(),
                "Klar".to_string()
            ])
        );
        assert_eq!(cfg(json!({"skip_labels": ["Hold"]})).skip_labels, ["Hold"]);
        assert_eq!(
            cfg(json!({"skip_labels": [["Hold", "WIP"], "Blockerat / Väntar"]})).skip_labels,
            ["Hold", "WIP", "Blockerat / Väntar"]
        );
    }

    /// `claim_from` is a list setting, so a lone name is a list of one: it reads the same
    /// bare, as afkd's one repeatable entry, or beside a block, and several names keep the
    /// order they were written in.
    #[test]
    fn claim_from_reads_one_name_or_several_in_the_shapes_afkd_lowers() {
        for value in [
            json!("To Do"),
            json!(["To Do"]),
            json!([["To Do"]]),
            json!({"@value": "To Do"}),
        ] {
            assert_eq!(cfg(json!({ "claim_from": value })).claim_from, ["To Do"]);
        }
        assert_eq!(
            cfg(json!({"claim_from": [["Bakgrundsvideor", "Hjälpvideor 🎬"]]})).claim_from,
            ["Bakgrundsvideor", "Hjälpvideor 🎬"]
        );
        assert_eq!(
            cfg(json!({"claim_from": ["Ideas 💡", ["Up for Grabs", "To Do"]]})).claim_from,
            ["Ideas 💡", "Up for Grabs", "To Do"]
        );
        // A `claim_from` written with no value names no list to draw from.
        for value in [json!(true), json!([true]), json!([{}])] {
            let e = err(json!({ "claim_from": value }));
            assert_eq!(e.key, "claim_from");
            assert_eq!(e.problem, "`claim_from` expects one or more list names");
        }
    }

    /// A valueless gate names nothing. It must fault on its own key rather than read as
    /// absent, which would silently claim any card — the opposite of the operator's ask.
    #[test]
    fn a_valueless_gate_faults_on_its_own_key() {
        for (key, problem) in [
            ("only_members", "`only_members` expects one or more members"),
            (
                "only_labels",
                "`only_labels` expects one or more label names",
            ),
            (
                "skip_labels",
                "`skip_labels` expects one or more label names",
            ),
        ] {
            for value in [json!(true), json!([true]), json!([{}]), json!([true, {}])] {
                let e = err(json!({ key: value }));
                assert_eq!(e.key, key);
                assert_eq!(e.problem, problem);
            }
        }
    }

    #[test]
    fn discuss_with_parses_anyone_and_members() {
        assert_eq!(
            cfg(json!({"discuss_with": ["anyone"]})).discuss_with,
            Some(DiscussWith::Anyone)
        );
        assert_eq!(
            cfg(json!({"discuss_with": [["alice", "bob"]]})).discuss_with,
            Some(DiscussWith::Members(vec![
                MemberRef::Username("alice".into()),
                MemberRef::Username("bob".into()),
            ]))
        );
        // A lone non-`anyone` operand is still a one-member allow-list, `me` is the
        // authed member, and two entries flatten into one list.
        assert_eq!(
            cfg(json!({"discuss_with": ["me", "björn"]})).discuss_with,
            Some(DiscussWith::Members(vec![
                MemberRef::SelfMember,
                MemberRef::Username("björn".into()),
            ]))
        );
        // `anyone` beside a name is two usernames, as in the built-in.
        assert_eq!(
            cfg(json!({"discuss_with": [["anyone", "alice"]]})).discuss_with,
            Some(DiscussWith::Members(vec![
                MemberRef::Username("anyone".into()),
                MemberRef::Username("alice".into()),
            ]))
        );
    }

    #[test]
    fn discuss_with_valueless_faults() {
        for value in [json!([true]), json!([{}])] {
            let e = err(json!({ "discuss_with": value }));
            assert_eq!(e.key, "discuss_with");
            assert_eq!(
                e.problem,
                "`discuss_with` expects `anyone` or a member list"
            );
        }
    }

    #[test]
    fn min_age_defaults_to_zero_and_parses_a_plain_duration() {
        assert_eq!(
            cfg(json!({"min_age": "10m"})).min_age,
            Duration::from_secs(600)
        );
        // afkd canonicalizes a declared duration to whole milliseconds; that spelling is
        // one this reader takes too, as the same threshold.
        assert_eq!(
            cfg(json!({"min_age": "600000ms"})).min_age,
            cfg(json!({"min_age": "600s"})).min_age
        );
        assert_eq!(
            cfg(json!({"min_age": "2h"})).min_age,
            Duration::from_secs(7200)
        );
    }

    #[test]
    fn min_age_without_a_duration_faults() {
        for value in [json!(true), json!({}), json!(["1m", "2m"])] {
            let e = err(json!({ "min_age": value }));
            assert_eq!(e.key, "min_age");
            assert_eq!(
                e.problem,
                "setting `min_age` expects a single duration value"
            );
        }
    }

    /// A threshold is not resampled per read, so the `every` jitter range is not a legal
    /// `min_age`, and neither is a compound, a bare number or an overflow.
    #[test]
    fn min_age_rejects_a_range_and_every_other_non_duration() {
        for spelling in [
            "2m..3m",
            "2h30m",
            "10",
            "ten minutes",
            "",
            "99999999999999999h",
        ] {
            let e = err(json!({ "min_age": spelling }));
            assert_eq!(e.key, "min_age");
            assert_eq!(
                e.problem,
                format!(
                    "setting `min_age` is not a duration (try `30s`, `5m`, `1h`): `{spelling}`"
                )
            );
        }
    }

    #[test]
    fn debug_redacts_api_key_and_token() {
        let cfg = board_config(&json!({
            "board": "https://trello.com/b/BID/x",
            "api_key": "SECRET-KEY-abc123",
            "token": "SECRET-TOKEN-def456",
            "claim_from": "Up for Grabs",
        }))
        .expect("valid");
        let shown = format!("{cfg:?}");
        assert!(
            !shown.contains("SECRET-KEY-abc123") && !shown.contains("SECRET-TOKEN-def456"),
            "creds leaked into Debug: {shown}"
        );
        assert!(
            shown.contains("<set>"),
            "presence should still show: {shown}"
        );
        assert!(format!("{:?}", BoardConfig::default()).contains("<unset>"));
    }

    #[test]
    fn a_settings_error_names_its_key() {
        assert_eq!(
            SettingsError::new(
                "min_age",
                "setting `min_age` expects a single duration value"
            )
            .to_string(),
            "setting `min_age`: setting `min_age` expects a single duration value"
        );
    }
}
