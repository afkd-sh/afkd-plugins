//! The `@{run:…}` reference form: run-fact interpolation in a lifecycle comment, ported
//! from afkd's `afkd_engine::run_ref` (ADR-0064).
//!
//! afkd's load gate passes a `@{run:…}` in a trigger's settings through literally — it
//! cannot tell an `on_claim` comment from an `on_done` one — so, exactly as for the
//! built-in trigger, the kind that owns the lifecycle owns both halves: [`check`] at
//! `hello`, where the plugin knows which moment a comment belongs to, and [`substitute`]
//! at `finish`, against the run's facts as afkd reports them.

use std::time::Duration;

use crate::wire::Facts;

/// The run facts a `@{run:KEY}` reference may name.
pub(crate) const KEYS: [&str; 4] = ["duration", "cost", "turns", "name"];

/// What `@{run:name}` renders when the facts carry no run name — never the empty string,
/// which would collapse the documented `.afkd/runs/@{service}/@{run:name}/run.log` idiom
/// into a `//` path.
const UNSTAMPED_RUN_NAME: &str = "unknown";

/// The `@{run:` opener a genuine reference starts with.
const OPENER: &str = "@{run:";

/// A malformed `@{run:…}` reference found by [`check`] — the **first** fault only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RunRefFault {
    /// A reference in a comment dispatched with no run behind it (`on_claim`).
    ReservedContext {
        /// The referenced key, so the fault names the offending reference.
        key: String,
    },
    /// A reference whose key is none of [`KEYS`].
    UnknownKey {
        /// The unrecognized key.
        key: String,
    },
}

/// Check a comment's text for `@{run:…}` legality, given whether run facts are available
/// at this moment (`true` for `on_done`/`on_fail`, `false` for `on_claim`).
pub(crate) fn check(text: &str, run_available: bool) -> Result<(), RunRefFault> {
    for key in refs(text) {
        if !run_available {
            return Err(RunRefFault::ReservedContext { key });
        }
        if !KEYS.contains(&key.as_str()) {
            return Err(RunRefFault::UnknownKey { key });
        }
    }
    Ok(())
}

/// Substitute each `@{run:KEY}` in `text` with its rendered value from `facts`. A key
/// outside [`KEYS`] cannot occur once [`check`] has passed, so it defensively survives
/// literal here rather than faulting at run end.
pub(crate) fn substitute(text: &str, facts: &Facts) -> String {
    if !text.contains(OPENER) {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(OPENER) {
        out.push_str(&rest[..start]);
        let after = &rest[start + OPENER.len()..];
        match after.find('}') {
            Some(end) => {
                let key = &after[..end];
                match render(key, facts) {
                    Some(value) => out.push_str(&value),
                    None => {
                        out.push_str(OPENER);
                        out.push_str(key);
                        out.push('}');
                    }
                }
                rest = &after[end + 1..];
            }
            // An unterminated `@{run:` has no closing `}`: copy it verbatim and stop.
            None => {
                out.push_str(OPENER);
                rest = after;
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Render one run-fact key against `facts`, as afkd's interpreter renders its finished
/// line: `duration` via [`format_duration`], `cost` as `$<2dp>`, `turns` as a bare
/// integer (`0` when no agent confirmed a count), `name` as the fire's run directory. An
/// unknown key is `None`.
fn render(key: &str, facts: &Facts) -> Option<String> {
    match key {
        "duration" => Some(format_duration(Duration::from_millis(facts.duration_ms))),
        "cost" => Some(format!("${:.2}", facts.cost)),
        "turns" => Some(facts.turns.unwrap_or(0).to_string()),
        "name" => Some(
            facts
                .run_name
                .clone()
                .unwrap_or_else(|| UNSTAMPED_RUN_NAME.to_string()),
        ),
        _ => None,
    }
}

/// A human duration, afkd's `narrate::format_duration`: whole milliseconds under a
/// second, two-decimal seconds under a minute, `XmYs` under an hour, else `XhYm`.
pub(crate) fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs == 0 {
        return format!("{}ms", d.as_millis());
    }
    if secs < 60 {
        return format!("{:.2}s", d.as_secs_f64());
    }
    if secs < 3600 {
        return format!("{}m{}s", secs / 60, secs % 60);
    }
    format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
}

/// Each genuine `@{run:KEY}` reference's key, in order. An unterminated opener yields
/// nothing (it is literal text).
fn refs(text: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(OPENER) {
        let after = &rest[start + OPENER.len()..];
        match after.find('}') {
            Some(end) => {
                keys.push(after[..end].to_string());
                rest = &after[end + 1..];
            }
            None => break,
        }
    }
    keys
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUN_NAME: &str = "260722-141802-card-QBOL9KfN-1";

    fn facts() -> Facts {
        Facts {
            duration_ms: 5_000,
            cost: 1.5,
            turns: Some(3),
            run_name: Some(RUN_NAME.to_string()),
            ..Facts::none()
        }
    }

    #[test]
    fn check_accepts_a_known_key_when_run_available() {
        assert_eq!(check("done in @{run:duration}", true), Ok(()));
        assert_eq!(check("cost @{run:cost}, @{run:turns} turns", true), Ok(()));
        assert_eq!(check("just a literal comment", false), Ok(()));
    }

    #[test]
    fn check_rejects_any_ref_when_run_unavailable() {
        assert_eq!(
            check("log: .afkd/runs/@{service}/@{run:name}/run.log", false),
            Err(RunRefFault::ReservedContext {
                key: "name".to_string()
            })
        );
    }

    #[test]
    fn check_rejects_an_unknown_key() {
        assert_eq!(
            check("@{run:bogus}", true),
            Err(RunRefFault::UnknownKey {
                key: "bogus".to_string()
            })
        );
        // `tokens` is on the facts but deliberately not referenceable.
        assert_eq!(
            check("@{run:tokens}", true),
            Err(RunRefFault::UnknownKey {
                key: "tokens".to_string()
            })
        );
    }

    #[test]
    fn substitute_renders_each_key() {
        assert_eq!(
            substitute(
                "done in @{run:duration} — @{run:cost}, @{run:turns} turns — \
                 log: .afkd/runs/afkd::selfdev/@{run:name}/run.log",
                &facts()
            ),
            "done in 5.00s — $1.50, 3 turns — \
             log: .afkd/runs/afkd::selfdev/260722-141802-card-QBOL9KfN-1/run.log"
        );
    }

    #[test]
    fn substitute_renders_absent_facts_honestly() {
        // No turn count confirmed is `0`, and no run name is `unknown`, never `""`.
        assert_eq!(
            substitute(
                "@{run:turns} turns, log @{run:name}/run.log",
                &Facts::none()
            ),
            "0 turns, log unknown/run.log"
        );
    }

    #[test]
    fn substitute_leaves_everything_else_untouched() {
        let facts = facts();
        assert_eq!(
            substitute("literal @{env:X} text", &facts),
            "literal @{env:X} text"
        );
        assert_eq!(
            substitute("dangling @{run:dur", &facts),
            "dangling @{run:dur"
        );
        assert_eq!(
            substitute("## 完了 ✅\n\nno refs", &facts),
            "## 完了 ✅\n\nno refs"
        );
    }

    #[test]
    fn format_duration_covers_every_band() {
        for (d, text) in [
            (Duration::ZERO, "0ms"),
            (Duration::from_millis(412), "412ms"),
            (Duration::from_millis(5_000), "5.00s"),
            (Duration::from_millis(59_990), "59.99s"),
            (Duration::from_secs(168), "2m48s"),
            (Duration::from_secs(3_599), "59m59s"),
            (Duration::from_secs(7_260), "2h1m"),
        ] {
            assert_eq!(format_duration(d), text, "{d:?}");
        }
    }
}
