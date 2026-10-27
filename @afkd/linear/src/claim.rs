//! The **claim decision**: the race-safe way two afkd instances agree on which of them
//! owns a shared issue, as pure functions over the markers they both read.
//!
//! The scheme is one sentence — write a marker into the issue's append-only comment log,
//! wait [`CLAIM_SETTLE`] so a rival's marker has time to become visible, re-read the log,
//! and win iff a *pure function of that shared list* names you — and its whole safety
//! argument is that both contenders compute the same answer from the same list.
//!
//! `@afkd/trello`'s decision, at Linear's id type: a comment id is a UUID, which does not
//! sort by time, so the order is `(createdAt, id)` with `createdAt` to the millisecond, and
//! the id breaks only a tie within one. The marker texts are trello's byte for byte.
//!
//! Linear keeps a comment as markdown and may hand a marker back with its punctuation
//! escaped (`\[afkd-claim\] owner=afkd\_x`), so every marker predicate reads a body
//! through [`unescape`] first.

use std::borrow::Cow;
use std::time::{Duration, SystemTime};

/// Marker prefix on a claim comment (the lightweight lock).
pub(crate) const CLAIM_MARKER: &str = "[afkd-claim]";

/// A claim's lifetime, measured from its **last renewal** — after it, the claim is stale
/// and no longer keeps a rival out. afkd renews a live run's claim every five minutes, so
/// this only says how long a **dead** run's marker keeps others out.
pub(crate) const CLAIM_LIFETIME: Duration = Duration::from_secs(3600);

/// The settle period waited after posting a claim before re-reading, so a rival's
/// concurrently-posted marker is visible in the read the winner is computed from.
pub(crate) const CLAIM_SETTLE: Duration = Duration::from_secs(1);

/// One comment as the claim decision sees it: the identity ties are broken on, the two
/// times the order and the age are measured with, and the text the marker predicate
/// reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClaimMarker {
    /// The comment's uuid, unique within the issue's log.
    pub(crate) id: String,
    /// When Linear says the comment was posted (`createdAt`) — the **order** half: an edit
    /// must never reshuffle who spoke first.
    pub(crate) posted_at: SystemTime,
    /// The last moment Linear reports the comment as edited (`editedAt`, else `createdAt`)
    /// — the **liveness** half, which a fire holding the claim moves forward on every
    /// renewal.
    pub(crate) renewed_at: SystemTime,
    /// The comment's text, verbatim.
    pub(crate) text: String,
}

/// The text of a claim comment carrying `owner`'s identity.
pub(crate) fn claim_text(owner: &str) -> String {
    format!("{CLAIM_MARKER} owner={owner}")
}

/// The text of a claim comment's `renewal`th **renewal** — still a claim
/// ([`is_claim`]), and still carrying `owner`.
///
/// The counter is in the body deliberately: a tracker asked to edit a comment to the text
/// it already holds is entitled to leave its last-edited stamp where it was, and that
/// stamp is the whole liveness signal.
pub(crate) fn claim_renewal_text(owner: &str, renewal: u64) -> String {
    format!("{CLAIM_MARKER} owner={owner} renewal={renewal}")
}

/// Whether a comment's text is a claim comment.
pub(crate) fn is_claim(text: &str) -> bool {
    unescape(text).trim_start().starts_with(CLAIM_MARKER)
}

/// `text` with CommonMark's backslash escapes taken out: a backslash before an ASCII
/// punctuation character is dropped, and every other character is kept as it is.
///
/// Whether Linear escapes a comment it stores is a guess this plugin does not lean on:
/// read through this, a marker reads the same escaped or as written.
pub(crate) fn unescape(text: &str) -> Cow<'_, str> {
    if !text.contains('\\') {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match chars.peek() {
            Some(&next) if c == '\\' && next.is_ascii_punctuation() => {
                out.push(next);
                chars.next();
            }
            _ => out.push(c),
        }
    }
    Cow::Owned(out)
}

/// Whether our claim is the unique minimum among still-live claims under the total order
/// `(posted_at, id)`.
///
/// Two claims posted in the same millisecond carry equal `posted_at`; the id breaks the
/// tie, giving a total order both processes compute identically. Liveness is judged relative to our
/// own claim's post time `t0` (no shared clock is trusted) and off the **renewal**: a
/// different claim is a threat iff it is strictly earlier in the order and its last
/// renewal is no more than `lifetime` before `t0`. A renewal at or after `t0` reads live.
/// We win iff nothing threatens.
///
/// `markers` is the *whole* mapped comment list: the claim filter lives here, welded to
/// the decision.
pub(crate) fn won_claim(markers: &[ClaimMarker], my_id: &str, lifetime: Duration) -> bool {
    let claims: Vec<&ClaimMarker> = markers.iter().filter(|m| is_claim(&m.text)).collect();
    let Some(mine) = claims.iter().find(|m| m.id == my_id) else {
        return false;
    };
    let t0 = mine.posted_at;
    !claims.iter().any(|m| {
        let earlier_in_order = m.posted_at < t0 || (m.posted_at == t0 && m.id.as_str() < my_id);
        let stale = t0
            .duration_since(m.renewed_at)
            .is_ok_and(|age| age > lifetime);
        m.id != my_id && earlier_in_order && !stale
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    /// A marker posted `secs` after the epoch — decades from any wall clock, so an
    /// implementation that consulted `SystemTime::now()` would judge every fixture aged
    /// out. Unrenewed (`renewed_at == posted_at`).
    fn marker(id: &str, text: &str, secs: u64) -> ClaimMarker {
        ClaimMarker {
            id: id.into(),
            posted_at: UNIX_EPOCH + Duration::from_secs(secs),
            renewed_at: UNIX_EPOCH + Duration::from_secs(secs),
            text: text.into(),
        }
    }

    /// A claim marker posted `secs` after the epoch, by `owner`.
    fn claim(id: &str, owner: &str, secs: u64) -> ClaimMarker {
        marker(id, &claim_text(owner), secs)
    }

    /// A claim marker posted at `posted` and last **renewed** at `renewed`.
    fn claim_renewed(id: &str, owner: &str, posted: u64, renewed: u64) -> ClaimMarker {
        ClaimMarker {
            renewed_at: UNIX_EPOCH + Duration::from_secs(renewed),
            ..claim(id, owner, posted)
        }
    }

    /// Build one claim per id, **all posted in the same second**, assert exactly one of
    /// the contenders judges itself the winner, and return that winner's id.
    fn sole_winner(ids: &[&str]) -> String {
        let claims: Vec<ClaimMarker> = ids.iter().map(|id| claim(id, "inst", 100)).collect();
        let winners: Vec<&str> = ids
            .iter()
            .copied()
            .filter(|id| won_claim(&claims, id, CLAIM_LIFETIME))
            .collect();
        assert_eq!(winners.len(), 1, "exactly one winner among {ids:?}");
        winners[0].to_string()
    }

    /// The marker literals, pinned byte for byte: trello's, which another afkd process
    /// reads off the same issue.
    #[test]
    fn claim_text_carries_the_marker_and_owner() {
        assert_eq!(claim_text("afkd-4242"), "[afkd-claim] owner=afkd-4242");
        assert_eq!(
            claim_text("björn-öst[bot]"),
            "[afkd-claim] owner=björn-öst[bot]"
        );
        assert_eq!(
            claim_renewal_text("afkd-4242", 3),
            "[afkd-claim] owner=afkd-4242 renewal=3"
        );
        for owner in ["me", "björn-öst[bot]", ""] {
            assert!(is_claim(&claim_text(owner)), "round-trips for {owner:?}");
        }
    }

    #[test]
    fn is_claim_ignores_leading_space_and_other_markers() {
        assert!(is_claim("[afkd-claim] owner=me"));
        assert!(is_claim("   [afkd-claim] owner=me"));
        assert!(is_claim("\n\n[afkd-claim] owner=me"));
        assert!(!is_claim("[afkd-ran] owner=me upto=100"));
        assert!(!is_claim("[afkd-attempt] 2/3: agent exited 1"));
        assert!(!is_claim("[afkd-park] service=afkd::develop"));
        assert!(!is_claim(
            "seen a stray [afkd-claim] on this issue — whose is it?"
        ));
        assert!(!is_claim(""));
        assert!(!is_claim("   \n "));
    }

    #[test]
    fn non_claim_markers_never_threaten() {
        let markers = [
            marker("a1", "[afkd-ran] owner=me upto=10", 10),
            marker(
                "a2",
                "looks stuck?\n\n> [afkd-claim] owner=other\n\nwhose claim is that?",
                20,
            ),
            marker("a3", "[afkd-attempt] 1/3: agent exited 1", 30),
            claim("a9", "me", 100),
        ];
        assert!(won_claim(&markers, "a9", CLAIM_LIFETIME));
    }

    #[test]
    fn won_claim_when_sole_live_and_earliest() {
        let mine = claim("c9", "me", 100);
        assert!(won_claim(std::slice::from_ref(&mine), "c9", CLAIM_LIFETIME));
        let later = claim("c10", "rival", 200);
        assert!(won_claim(&[mine.clone(), later], "c9", CLAIM_LIFETIME));
        // A marker list we are not in at all is not a win (the read lost our claim).
        assert!(!won_claim(&[mine], "c11", CLAIM_LIFETIME));
    }

    #[test]
    fn loses_claim_to_an_earlier_still_live_claim() {
        let earlier = claim("rival1", "rival", 50);
        let mine = claim("mine", "me", 100);
        assert!(!won_claim(&[earlier, mine], "mine", CLAIM_LIFETIME));
    }

    #[test]
    fn earlier_claim_past_its_lifetime_does_not_block() {
        let stale = claim("rival1", "rival", 100);
        let mine = claim("mine", "me", 100 + 7200);
        assert!(won_claim(&[stale, mine], "mine", CLAIM_LIFETIME));
    }

    #[test]
    fn rival_exactly_at_the_lifetime_boundary_still_threatens() {
        let base = 100_000;
        let mine = claim("mine", "me", base);
        let boundary = claim("rival1", "rival", base - CLAIM_LIFETIME.as_secs());
        assert!(!won_claim(
            &[boundary, mine.clone()],
            "mine",
            CLAIM_LIFETIME
        ));
        let past = claim("rival1", "rival", base - CLAIM_LIFETIME.as_secs() - 1);
        assert!(won_claim(&[past, mine], "mine", CLAIM_LIFETIME));
    }

    #[test]
    fn a_renewed_rival_blocks_us_however_old_it_is() {
        let base = 100_000;
        let mine = claim("mine", "me", base);
        let held = claim_renewed("rival1", "rival", base - 36_000, base - 60);
        assert!(!won_claim(&[held, mine.clone()], "mine", CLAIM_LIFETIME));
        let dropped = claim_renewed(
            "rival1",
            "rival",
            base - 36_000,
            base - CLAIM_LIFETIME.as_secs() - 1,
        );
        assert!(won_claim(&[dropped, mine.clone()], "mine", CLAIM_LIFETIME));
        // A rival renewing *while we were posting*: the live case, not the stale one.
        let concurrent = claim_renewed("rival1", "rival", base - 36_000, base + 1);
        assert!(!won_claim(&[concurrent, mine], "mine", CLAIM_LIFETIME));
    }

    #[test]
    fn renewal_text_is_still_a_claim_and_changes_per_renewal() {
        for owner in ["me", "björn-öst[bot]", ""] {
            assert!(is_claim(&claim_renewal_text(owner, 1)));
            assert_ne!(claim_renewal_text(owner, 1), claim_renewal_text(owner, 2));
            assert_ne!(claim_renewal_text(owner, 1), claim_text(owner));
        }
    }

    #[test]
    fn later_markers_never_unseat_us() {
        let base = 10_000;
        let mut markers = vec![claim("mine", "me", base)];
        for (i, dt) in [1, 60, 3599, 3600, 7200].iter().enumerate() {
            markers.push(claim(&format!("r{i}"), "rival", base + dt));
        }
        assert!(won_claim(&markers, "mine", CLAIM_LIFETIME));
    }

    /// A tie within one millisecond is broken by the uuid's string order: arbitrary, since a
    /// uuid carries no time, but the same in every process, so exactly one wins.
    #[test]
    fn same_millisecond_ties_are_broken_by_the_uuid() {
        assert_eq!(
            sole_winner(&[
                "f3b1c2d4-0e5f-4a6b-8c7d-9e0f1a2b3c4d",
                "0a9b8c7d-6e5f-4d3c-a2b1-c0d9e8f7a6b5",
            ]),
            "0a9b8c7d-6e5f-4d3c-a2b1-c0d9e8f7a6b5"
        );
        assert_eq!(
            sole_winner(&[
                "7c1e9a3b-2d4f-4b6a-8e0c-1f3a5b7d9e2c",
                "7c1e9a3b-2d4f-4b6a-8e0c-1f3a5b7d9e2b",
                "e4d3c2b1-a0f9-4e8d-b7c6-a5f4e3d2c1b0",
            ]),
            "7c1e9a3b-2d4f-4b6a-8e0c-1f3a5b7d9e2b"
        );
    }

    /// A marker Linear hands back with its punctuation escaped is still a claim, and still
    /// not one when the escaped text is not a marker at the start.
    #[test]
    fn an_escaped_claim_marker_is_a_claim() {
        assert!(is_claim(r"\[afkd-claim\] owner=afkd\-4242"));
        assert!(is_claim(r"  \[afkd-claim\] owner=afkd\_x renewal=3"));
        assert!(!is_claim(r"\[afkd-ran\] owner=afkd upto=1"));
        assert!(!is_claim(r"see \[afkd-claim\] above"));
    }

    /// A backslash before ASCII punctuation is dropped; one before anything else, a trailing
    /// one and a doubled one are CommonMark's literal backslash, and wide text is untouched.
    #[test]
    fn unescape_drops_only_commonmark_escapes() {
        assert_eq!(
            unescape(r"\[afkd\-park\] service=afkd\:\:develop"),
            "[afkd-park] service=afkd::develop"
        );
        assert_eq!(unescape(r"C:\Users\björn"), r"C:\Users\björn");
        assert_eq!(unescape(r"ends \"), r"ends \");
        assert_eq!(unescape(r"two \\ backslashes"), r"two \ backslashes");
        assert_eq!(unescape("修复 🚨 — \"x\""), "修复 🚨 — \"x\"");
        assert!(matches!(unescape("plain"), Cow::Borrowed("plain")));
    }
}
