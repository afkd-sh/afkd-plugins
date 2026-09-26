//! The **claim decision**: the race-safe way two afkd instances agree on which of them
//! owns a shared unit, as pure functions over the markers they both read.
//!
//! The scheme is one sentence — write a marker into the unit's append-only comment log,
//! wait [`CLAIM_SETTLE`] so a rival's marker has time to become visible, re-read the log,
//! and win iff a *pure function of that shared list* names you — and its whole safety
//! argument is that both contenders compute the same answer from the same list.
//!
//! Ported verbatim from afkd's `afkd_forge::claim`. The texts and the key shape cross a
//! boundary someone else reads: a marker the built-in trigger left on a live issue, and a
//! claim-journal entry it wrote, must read here exactly as they read there, and the other
//! way round, so the switch between the two strands no claim.

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
    /// The note's GitLab id, unique within the unit's log.
    pub(crate) id: u64,
    /// When GitLab says the note was posted — the **order** half: an edit must never
    /// reshuffle who spoke first.
    pub(crate) posted_at: SystemTime,
    /// The last moment GitLab reports the note as touched — the **liveness** half,
    /// which a fire holding the claim moves forward on every renewal.
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
/// The counter is in the body deliberately: a forge asked to edit a comment to the text
/// it already holds is entitled to leave its last-touched stamp where it was, and that
/// stamp is the whole liveness signal.
pub(crate) fn claim_renewal_text(owner: &str, renewal: u64) -> String {
    format!("{CLAIM_MARKER} owner={owner} renewal={renewal}")
}

/// Whether a comment's text is a claim comment.
pub(crate) fn is_claim(text: &str) -> bool {
    text.trim_start().starts_with(CLAIM_MARKER)
}

/// Whether our claim is the unique minimum among still-live claims under the total order
/// `(posted_at, id)`.
///
/// Post times are second-granular, so two claims posted in the same second carry equal
/// `posted_at`; the id breaks the tie, giving a total order both processes compute
/// identically. Liveness is judged relative to our own claim's post time `t0` (no shared
/// clock is trusted) and off the **renewal**: a different claim is a threat iff it is
/// strictly earlier in the order and its last renewal is no more than `lifetime` before
/// `t0`. A renewal at or after `t0` reads live. We win iff nothing threatens.
///
/// `markers` is the *whole* mapped comment list: the claim filter lives here, welded to
/// the decision.
pub(crate) fn won_claim(markers: &[ClaimMarker], my_id: u64, lifetime: Duration) -> bool {
    let claims: Vec<&ClaimMarker> = markers.iter().filter(|m| is_claim(&m.text)).collect();
    let Some(mine) = claims.iter().find(|m| m.id == my_id) else {
        return false;
    };
    let t0 = mine.posted_at;
    !claims.iter().any(|m| {
        let earlier_in_order = m.posted_at < t0 || (m.posted_at == t0 && m.id < my_id);
        let stale = t0
            .duration_since(m.renewed_at)
            .is_ok_and(|age| age > lifetime);
        m.id != my_id && earlier_in_order && !stale
    })
}

/// The claim-journal key for an in-flight claim: `<location>#<number>#<marker-id>` — the
/// unit's own coordinate plus the claim marker's comment id, which the reaper needs to
/// delete a crashed run's marker.
pub(crate) fn claim_key(location: &str, number: u64, marker_id: u64) -> String {
    format!("{location}#{number}#{marker_id}")
}

/// Split a [`claim_key`] back into its `(location, number, marker_id)`. `None` when the
/// key is not that shape — no `#`, an empty location, or a non-numeric tail — including
/// the plain `<location>#<number>` a key had before it carried a marker, which names
/// nothing releasable.
///
/// The marker id comes off the *last* `#`, and the number off the last `#` of what
/// remains (afkd's `claimjournal::split_unit_key`).
pub(crate) fn split_claim_key(key: &str) -> Option<(&str, u64, u64)> {
    let (head, marker_id) = key.rsplit_once('#')?;
    let marker_id: u64 = marker_id.parse().ok()?;
    let (location, number) = head.rsplit_once('#')?;
    if location.is_empty() {
        return None;
    }
    let number: u64 = number.parse().ok()?;
    Some((location, number, marker_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    /// A marker posted `secs` after the epoch — decades from any wall clock, so an
    /// implementation that consulted `SystemTime::now()` would judge every fixture aged
    /// out. Unrenewed (`renewed_at == posted_at`).
    fn marker(id: u64, text: &str, secs: u64) -> ClaimMarker {
        ClaimMarker {
            id,
            posted_at: UNIX_EPOCH + Duration::from_secs(secs),
            renewed_at: UNIX_EPOCH + Duration::from_secs(secs),
            text: text.into(),
        }
    }

    /// A claim marker posted `secs` after the epoch, by `owner`.
    fn claim(id: u64, owner: &str, secs: u64) -> ClaimMarker {
        marker(id, &claim_text(owner), secs)
    }

    /// A claim marker posted at `posted` and last **renewed** at `renewed`.
    fn claim_renewed(id: u64, owner: &str, posted: u64, renewed: u64) -> ClaimMarker {
        ClaimMarker {
            renewed_at: UNIX_EPOCH + Duration::from_secs(renewed),
            ..claim(id, owner, posted)
        }
    }

    /// Build one claim per id, **all posted in the same second**, assert exactly one of
    /// the contenders judges itself the winner, and return that winner's id.
    fn sole_winner(ids: &[u64]) -> u64 {
        let claims: Vec<ClaimMarker> = ids.iter().map(|id| claim(*id, "inst", 100)).collect();
        let winners: Vec<u64> = ids
            .iter()
            .copied()
            .filter(|id| won_claim(&claims, *id, CLAIM_LIFETIME))
            .collect();
        assert_eq!(winners.len(), 1, "exactly one winner among {ids:?}");
        winners[0]
    }

    /// The cross-boundary literals, pinned byte for byte: the built-in trigger wrote
    /// these onto live issues, and the plugin must recognise, renew and release them.
    #[test]
    fn claim_text_carries_the_marker_and_owner() {
        assert_eq!(claim_text("me"), "[afkd-claim] owner=me");
        assert_eq!(
            claim_text("björn-öst[bot]"),
            "[afkd-claim] owner=björn-öst[bot]"
        );
        assert_eq!(
            claim_renewal_text("björn-öst[bot]", 3),
            "[afkd-claim] owner=björn-öst[bot] renewal=3"
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
        assert!(!is_claim("[afkd-ran] upto=1970-01-01T00:01:40Z"));
        assert!(!is_claim("[afkd-attempt] 2/3: agent exited 1"));
        assert!(!is_claim(
            "seen a stray [afkd-claim] on this card — whose is it?"
        ));
        assert!(!is_claim(""));
        assert!(!is_claim("   \n "));
    }

    #[test]
    fn non_claim_markers_never_threaten() {
        let markers = [
            marker(1, "[afkd-ran] upto=1970-01-01T00:00:10Z", 10),
            marker(
                2,
                "looks stuck?\n\n> [afkd-claim] owner=other\n\nwhose claim is that?",
                20,
            ),
            marker(3, "[afkd-attempt] 1/3: agent exited 1", 30),
            claim(9, "me", 100),
        ];
        assert!(won_claim(&markers, 9, CLAIM_LIFETIME));
    }

    #[test]
    fn won_claim_when_sole_live_and_earliest() {
        let mine = claim(9, "me", 100);
        assert!(won_claim(std::slice::from_ref(&mine), 9, CLAIM_LIFETIME));
        let later = claim(10, "rival", 200);
        assert!(won_claim(&[mine.clone(), later], 9, CLAIM_LIFETIME));
        // A marker list we are not in at all is not a win (the read lost our claim).
        assert!(!won_claim(&[mine], 11, CLAIM_LIFETIME));
    }

    #[test]
    fn loses_claim_to_an_earlier_still_live_claim() {
        let earlier = claim(10, "rival", 50);
        let mine = claim(9, "me", 100);
        assert!(!won_claim(&[earlier, mine], 9, CLAIM_LIFETIME));
    }

    #[test]
    fn earlier_claim_past_its_lifetime_does_not_block() {
        let stale = claim(10, "rival", 100);
        let mine = claim(9, "me", 100 + 7200);
        assert!(won_claim(&[stale, mine], 9, CLAIM_LIFETIME));
    }

    #[test]
    fn rival_exactly_at_the_lifetime_boundary_still_threatens() {
        let base = 100_000;
        let mine = claim(9, "me", base);
        let boundary = claim(10, "rival", base - CLAIM_LIFETIME.as_secs());
        assert!(!won_claim(&[boundary, mine.clone()], 9, CLAIM_LIFETIME));
        let past = claim(10, "rival", base - CLAIM_LIFETIME.as_secs() - 1);
        assert!(won_claim(&[past, mine], 9, CLAIM_LIFETIME));
    }

    #[test]
    fn a_renewed_rival_blocks_us_however_old_it_is() {
        let base = 100_000;
        let mine = claim(9, "me", base);
        let held = claim_renewed(10, "rival", base - 36_000, base - 60);
        assert!(!won_claim(&[held, mine.clone()], 9, CLAIM_LIFETIME));
        let dropped = claim_renewed(
            10,
            "rival",
            base - 36_000,
            base - CLAIM_LIFETIME.as_secs() - 1,
        );
        assert!(won_claim(&[dropped, mine.clone()], 9, CLAIM_LIFETIME));
        // A rival renewing *while we were posting*: the live case, not the stale one.
        let concurrent = claim_renewed(10, "rival", base - 36_000, base + 1);
        assert!(!won_claim(&[concurrent, mine], 9, CLAIM_LIFETIME));
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
        let mut markers = vec![claim(1, "me", base)];
        for (i, dt) in [1, 60, 3599, 3600, 7200].iter().enumerate() {
            markers.push(claim(2 + i as u64, "rival", base + dt));
        }
        assert!(won_claim(&markers, 1, CLAIM_LIFETIME));
    }

    #[test]
    fn same_second_ties_are_broken_by_id_numerically() {
        assert_eq!(sole_winner(&[7, 9]), 7);
        assert_eq!(sole_winner(&[30, 9, 10, 200, 4]), 4);
        // As numbers 9 is the minimum; a string compare would hand it to "10".
        assert_eq!(sole_winner(&[9, 10, 100]), 9);
    }

    #[test]
    fn claim_key_round_trips_through_split() {
        for (location, number, marker) in [
            ("acme/widgets", 7u64, 4242u64),
            ("björn-öst/verktyg", 412, 1_000_001),
            ("a", 0, 0),
        ] {
            let key = claim_key(location, number, marker);
            assert_eq!(split_claim_key(&key), Some((location, number, marker)));
        }
        assert_eq!(claim_key("acme/widgets", 7, 4242), "acme/widgets#7#4242");
    }

    #[test]
    fn split_claim_key_rejects_every_other_shape() {
        for key in [
            "garbage",
            "acme/widgets",
            // The pre-marker two-part key: it names nothing releasable.
            "acme/widgets#7",
            "#7#4242",
            "acme/widgets#seven#4242",
            "acme/widgets#7#claim",
            "6a7f118d9a663d521d85c645#6a7f118d9a663d521d85c646",
            "",
        ] {
            assert_eq!(split_claim_key(key), None, "{key:?} is not a claim key");
        }
    }
}
