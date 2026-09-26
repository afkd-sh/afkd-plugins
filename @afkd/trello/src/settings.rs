//! Read the `trello` kind's settings block — as afkd lowers it to JSON in `hello` — into
//! a typed [`BoardConfig`], and wire its lifecycle moments to the action vocabulary.
//!
//! afkd has already held the block to the manifest before this plugin is spawned: every
//! key is one the kind's manifest table declares, `board`/`api_key`/`token` are present,
//! no single-valued key is written twice, and only the `on_*` keys (and a `move_to` inside
//! one) carry a block. What is left here is the part a manifest cannot say — a gate that
//! names nothing, a lifecycle action naming no operand, `@{run:…}` legality per moment, a
//! `min_age` that is not a plain duration — in the built-in trigger's own sentences. The
//! cadence, the attempt bound and the mid-run watch (`poll_interval`, `max_attempts`,
//! `follow_comments`) are afkd's and are not read here.
//!
//! The lowering (afkd's `pluginworker::settings_json`): a value is a string, several are
//! an array of strings, a bare flag is `true`, a block is an object (a value beside it
//! rides as `@value`), and a repeatable key is always an array.

use std::time::Duration;

use serde_json::{Map, Value};

use crate::client::TRELLO_BASE;
use crate::lifecycle::{parse_block, LifecycleAction};

/// Who an `add_member` action, or a `require_member` intake gate, names.
///
/// The reserved operand `self` selects the member the credentials authenticate as. The
/// consequence is that a board member whose username is literally `self` cannot be named.
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

/// A validated `trello` trigger configuration.
///
/// `Debug` is **hand-written** so the credential fields never reach a diagnostic.
#[derive(Clone, PartialEq, Eq, Default)]
pub(crate) struct BoardConfig {
    /// The board's web address.
    pub(crate) board_address: String,
    /// Board identity derived from the address (segment after the board marker).
    pub(crate) board_id: String,
    /// API base the client is pointed at; defaults to the public Trello REST base. A
    /// testability seam, not a user-facing knob.
    pub(crate) base_url: String,
    /// API key used to reach the board.
    pub(crate) api_key: String,
    /// Token used to reach the board.
    pub(crate) token: String,
    /// Named list new tasks are drawn from (empty when not given).
    pub(crate) pick_from: String,
    /// The intake gate: when set, only cards this member is already on are claimed.
    pub(crate) require_member: Option<MemberRef>,
    /// The label intake gate: when set, only cards carrying a label of this exact name
    /// are eligible to claim.
    pub(crate) require_label: Option<String>,
    /// The negative label intake gate: a card carrying any of these labels is never
    /// claimed, and deny wins over `require_label`.
    pub(crate) without_label: Vec<String>,
    /// The comment-driven grooming gate: when set, a card is claimed only if its tail —
    /// the comments after afkd's own last comment — carries a comment from an allowed
    /// author.
    pub(crate) discuss_with: Option<DiscussWith>,
    /// The age intake gate, measured from the card's creation. [`Duration::ZERO`] (the
    /// default) filters nothing.
    pub(crate) min_age: Duration,
    /// Actions performed when work on a card begins.
    pub(crate) on_claim: Vec<LifecycleAction>,
    /// Actions performed when a card's work finishes successfully.
    pub(crate) on_done: Vec<LifecycleAction>,
    /// Actions performed when a card's work fails.
    pub(crate) on_fail: Vec<LifecycleAction>,
    /// Optional extras performed when a run parks a card. The `Awaiting Reply` badge is
    /// the kind's own, so the gate holds with no block at all.
    pub(crate) on_park: Vec<LifecycleAction>,
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
            .field("pick_from", &self.pick_from)
            .field("require_member", &self.require_member)
            .field("require_label", &self.require_label)
            .field("without_label", &self.without_label)
            .field("discuss_with", &self.discuss_with)
            .field("min_age", &self.min_age)
            .field("on_claim", &self.on_claim)
            .field("on_done", &self.on_done)
            .field("on_fail", &self.on_fail)
            .field("on_park", &self.on_park)
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

// The built-in's vocabulary, verbatim. afkd reads the manifest, never these: they are
// what `manifest.rs`'s test holds `afkd-plugin.toml` to, so the two cannot drift apart.

/// The `trello` kind's full key set, exactly the built-in's `ALLOWED_KEYS`, in its order.
#[cfg(test)]
pub(crate) const ALLOWED_KEYS: &[&str] = &[
    "board",
    "base_url",
    "api_key",
    "token",
    "pick_from",
    "require_member",
    "require_label",
    "without_label",
    "discuss_with",
    "min_age",
    "follow_comments",
    "max_attempts",
    "poll_interval",
    "on_claim",
    "on_done",
    "on_fail",
    "on_park",
];

/// The keys a `trello` block may write more than once: the two filter lists it reads as
/// sequences.
#[cfg(test)]
pub(crate) const REPEATABLE_KEYS: &[&str] = &["without_label", "discuss_with"];

/// The keys afkd reads as a duration, compared by value across a reload.
#[cfg(test)]
pub(crate) const DURATION_KEYS: &[&str] = &["poll_interval", "min_age", "follow_comments"];

/// The keys the block must carry: the board and its credentials, which flow config →
/// child env with no process-env fallback.
#[cfg(test)]
pub(crate) const REQUIRED_KEYS: &[&str] = &["board", "api_key", "token"];

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
        pick_from: opt_scalar(settings, "pick_from").unwrap_or_default(),
        require_member: required_scalar(settings, "require_member")?.map(|who| member_ref(&who)),
        require_label: required_scalar(settings, "require_label")?,
        without_label: nonempty_list(
            settings,
            "without_label",
            "`without_label` expects one or more label names",
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
        // `on_claim` runs before any fire, so a `@{run:…}` reference is illegal there;
        // the terminal moments — the park among them, which has the ask's run behind
        // it — receive the attempt's real facts.
        on_claim: parse_block(settings.get("on_claim"), false)?,
        on_done: parse_block(settings.get("on_done"), true)?,
        on_fail: parse_block(settings.get("on_fail"), true)?,
        on_park: parse_block(settings.get("on_park"), true)?,
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

/// The one `self`-or-username rule every member operand is read by: the reserved operand
/// `self` (case-sensitively — Trello usernames are lowercase) selects the authed member,
/// any other names a board member by username.
pub(crate) fn member_ref(who: &str) -> MemberRef {
    match who {
        "self" => MemberRef::SelfMember,
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

/// An optional gate that, once written, must carry one value: `None` when absent, and a
/// fault in the built-in's sentence when it is a flag, a block or a list — reading it as
/// absent would silently claim what the operator asked to filter.
fn required_scalar(
    settings: &Map<String, Value>,
    key: &str,
) -> Result<Option<String>, SettingsError> {
    if !settings.contains_key(key) {
        return Ok(None);
    }
    opt_scalar(settings, key)
        .map(Some)
        .ok_or_else(|| SettingsError::new(key, format!("setting `{key}` expects a single value")))
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
    use crate::lifecycle::ListPosition;
    use serde_json::json;

    /// The three required credentials plus `extra`, as afkd lowers a block: the
    /// minimal block afkd would let through.
    fn block(extra: Value) -> Value {
        let mut settings = json!({
            "board": "https://trello.com/b/BID/x",
            "api_key": "k",
            "token": "t",
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
        // afkd's own three keys ride along and are not read here.
        let cfg = cfg(json!({
            "pick_from": "Up for Grabs",
            "max_attempts": "2",
            "poll_interval": "30s",
            "follow_comments": "2m..3m",
        }));
        assert_eq!(cfg.board_address, "https://trello.com/b/BID/x");
        assert_eq!(cfg.board_id, "BID");
        assert_eq!(cfg.api_key, "k");
        assert_eq!(cfg.token, "t");
        assert_eq!(cfg.pick_from, "Up for Grabs");
    }

    #[test]
    fn defaults_apply_when_settings_absent() {
        let cfg = cfg(json!({}));
        assert_eq!(cfg.base_url, TRELLO_BASE);
        assert_eq!(cfg.pick_from, "");
        assert_eq!(cfg.require_member, None);
        assert_eq!(cfg.require_label, None);
        assert!(cfg.without_label.is_empty());
        assert_eq!(cfg.discuss_with, None);
        assert_eq!(cfg.min_age, Duration::ZERO);
        assert!(cfg.on_claim.is_empty() && cfg.on_done.is_empty());
        assert!(cfg.on_fail.is_empty() && cfg.on_park.is_empty());
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
            "pick_from": {"@value": "Up for Grabs"},
        }))
        .expect("valid");
        assert_eq!(cfg.board_id, "1Rkelydw");
        assert_eq!(cfg.pick_from, "Up for Grabs");
    }

    #[test]
    fn require_member_parses_self_and_a_username() {
        assert_eq!(
            cfg(json!({"require_member": "self"})).require_member,
            Some(MemberRef::SelfMember)
        );
        assert_eq!(
            cfg(json!({"require_member": "marisa"})).require_member,
            Some(MemberRef::Username("marisa".into()))
        );
    }

    /// A valueless gate names nothing. It must fault on its own key rather than read as
    /// absent (which would silently claim any card — the opposite of the operator's
    /// ask); a list names more than one and faults the same way.
    #[test]
    fn a_single_valued_gate_without_one_value_faults() {
        for key in ["require_member", "require_label"] {
            for value in [json!(true), json!({}), json!(["a", "b"])] {
                let e = err(json!({ key: value }));
                assert_eq!(e.key, key);
                assert_eq!(e.problem, format!("setting `{key}` expects a single value"));
            }
        }
    }

    #[test]
    fn require_label_parses_a_name() {
        assert_eq!(
            cfg(json!({"require_label": "Redo ✅"})).require_label,
            Some("Redo ✅".to_string())
        );
    }

    /// The negative gate accepts one label or many, across repeated entries, flattened in
    /// written order — the built-in's `SettingsTree::list`.
    #[test]
    fn without_label_parses_one_or_many() {
        assert_eq!(
            cfg(json!({"without_label": ["Hold"]})).without_label,
            ["Hold"]
        );
        assert_eq!(
            cfg(json!({"without_label": [["Hold", "WIP"], "Blockerat / Väntar"]})).without_label,
            ["Hold", "WIP", "Blockerat / Väntar"]
        );
    }

    #[test]
    fn without_label_valueless_faults() {
        for value in [json!([true]), json!([{}]), json!([true, {}])] {
            let e = err(json!({ "without_label": value }));
            assert_eq!(e.key, "without_label");
            assert_eq!(e.problem, "`without_label` expects one or more label names");
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
        // A lone non-`anyone` operand is still a one-member allow-list, `self` is the
        // reserved operand, and two entries flatten into one list.
        assert_eq!(
            cfg(json!({"discuss_with": ["self", "björn"]})).discuss_with,
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

    /// A threshold is not resampled per read, so the `poll_interval` jitter range is not
    /// a legal `min_age`, and neither is a compound, a bare number or an overflow.
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

    /// Each moment reaches the parser under its own `run_refs_allowed`, into its own
    /// field: `on_claim` before any fire, the three terminal moments after one.
    #[test]
    fn lifecycle_blocks_wire_each_moment_to_its_run_ref_rule() {
        let terminal = "done in @{run:duration} — log @{run:name}";
        let cfg = cfg(json!({
            "on_claim": {"add_member": ["self"], "move_to": [{"@value": "In Progress", "at": ["top"]}]},
            "on_done": {"comment": [terminal]},
            "on_fail": {"add_label": ["Problem"]},
            "on_park": {"comment": ["parked after @{run:duration}"]},
        }));
        assert_eq!(
            cfg.on_claim,
            vec![
                LifecycleAction::AddMember(MemberRef::SelfMember),
                LifecycleAction::MoveTo {
                    list: "In Progress".into(),
                    position: ListPosition::Top,
                },
            ]
        );
        assert_eq!(cfg.on_done, vec![LifecycleAction::Comment(terminal.into())]);
        assert_eq!(
            cfg.on_fail,
            vec![LifecycleAction::AddLabel {
                name: "Problem".into()
            }]
        );
        assert_eq!(
            cfg.on_park,
            vec![LifecycleAction::Comment(
                "parked after @{run:duration}".into()
            )]
        );

        // The very same comment is a hard fault at claim time.
        let e = err(json!({"on_claim": {"comment": [terminal]}}));
        assert_eq!(e.key, "comment");
        assert_eq!(
            e.problem,
            "`@{run:duration}` references the run's facts, but no run happens at claim time"
        );
    }

    /// Every lifecycle key is a sequence element, so two of one verb both land, in
    /// written order — while across verbs the canonical order runs (see `lifecycle.rs`),
    /// which puts the built-in's trailing `mark_complete` first.
    #[test]
    fn on_done_performs_two_add_labels_in_source_order() {
        let cfg = cfg(json!({"on_done": {
            "add_label": ["shipped", "reviewed ✅"],
            "comment": ["landed by @{run:name}", "see the run log"],
            "mark_complete": [true],
        }}));
        assert_eq!(
            cfg.on_done,
            vec![
                LifecycleAction::MarkComplete,
                LifecycleAction::AddLabel {
                    name: "shipped".into(),
                },
                LifecycleAction::AddLabel {
                    name: "reviewed ✅".into(),
                },
                LifecycleAction::Comment("landed by @{run:name}".into()),
                LifecycleAction::Comment("see the run log".into()),
            ]
        );
    }

    #[test]
    fn debug_redacts_api_key_and_token() {
        let cfg = board_config(&json!({
            "board": "https://trello.com/b/BID/x",
            "api_key": "SECRET-KEY-abc123",
            "token": "SECRET-TOKEN-def456",
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
