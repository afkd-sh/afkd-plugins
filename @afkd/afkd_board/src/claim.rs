//! The **claim decision**: the race-safe way two afkd instances agree on which of them owns a
//! card, as pure functions over the markers they both read, and the filter that says which cards
//! are worth racing for.
//!
//! The board has no claim call of its own, so the scheme is `@afkd/linear`'s: write a marker
//! into the card's append-only comment log, wait [`CLAIM_SETTLE`] so a rival's marker has time
//! to become visible, re-read the log, and win iff a *pure function of that shared list* names
//! you. Both contenders compute the same answer from the same list.
//!
//! The order is `(created_at, id)`: the board stamps a comment to the second, and a comment's id
//! is a uuid, which does not sort by time, so the id breaks only a tie within one second. It is
//! arbitrary, but the same in every process, so exactly one contender wins it.
//!
//! **Markers cannot be deleted or edited** on the board, and so cannot be renewed or released.
//! They age out of the decision instead: only a marker posted no more than [`CLAIM_WINDOW`]
//! before our own can beat it. That is safe because a winner puts `afkd/claimed` on the card
//! within its poll's 45 s budget, long before the window ends, and every contender that reads
//! the card afterwards sees the label and stands down.

use std::time::Duration;

use crate::client::{CardAnswer, CommentAnswer};
use crate::rfc3339::unix_millis;
use crate::settings::ServiceConfig;

/// Marker prefix on a claim comment: the lightweight lock.
pub(crate) const CLAIM_MARKER: &str = "[afkd-claim]";

/// The label a claimed card carries, for people and for other services' `skip_labels`.
pub(crate) const CLAIMED_LABEL: &str = "afkd/claimed";

/// The settle period waited after posting a claim before re-reading, so a rival's
/// concurrently-posted marker is visible in the read the winner is computed from.
///
/// Three seconds, as every plugin that claims through comments waits: against Linear one
/// second was not always enough for a rival's marker to show, and the two seconds more are paid
/// once a claim, and claims are minutes apart.
pub(crate) const CLAIM_SETTLE: Duration = Duration::from_secs(3);

/// How far before our own a marker may be posted and still beat it. A marker cannot be deleted
/// or edited, so this is how a dead run's marker stops keeping others out. A winner puts the
/// label on within its poll's 45 s budget, so 90 s leaves the label on the card well before
/// any marker that raced for it has aged out.
pub(crate) const CLAIM_WINDOW: Duration = Duration::from_secs(90);

/// One claim marker as the decision sees it: the identity ties are broken on, and the time the
/// board says it was posted, in unix milliseconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClaimMarker {
    /// The comment's uuid, unique within the card's log.
    pub(crate) id: String,
    /// When the board says the comment was made.
    pub(crate) posted_at: u64,
}

impl ClaimMarker {
    /// The marker `comment` is, or `None` when it is none: a comment no key of the board made
    /// is a person's, whatever it says, and a time the board wrote that is no instant is one no
    /// decision can order.
    pub(crate) fn from_comment(comment: &CommentAnswer) -> Option<Self> {
        if !comment.by_key || !is_claim(&comment.body) {
            return None;
        }
        Some(Self {
            id: comment.id.clone(),
            posted_at: unix_millis(&comment.created_at)?,
        })
    }
}

/// The text of a claim comment carrying `owner`'s identity.
pub(crate) fn claim_text(owner: &str) -> String {
    format!("{CLAIM_MARKER} owner={owner}")
}

/// Whether a comment's text is a claim comment.
pub(crate) fn is_claim(text: &str) -> bool {
    text.trim_start().starts_with(CLAIM_MARKER)
}

/// The markers `comments` holds: each comment a key of the board made whose text is a claim.
pub(crate) fn markers(comments: &[CommentAnswer]) -> Vec<ClaimMarker> {
    comments
        .iter()
        .filter_map(ClaimMarker::from_comment)
        .collect()
}

/// Whether our claim, the marker `my_id`, is the unique minimum under `(posted_at, id)` among
/// the markers posted no more than `window` before it.
///
/// Liveness is judged relative to our own marker's time (no shared clock is trusted, only the
/// board's own stamps): another marker is a threat iff it is strictly earlier in the order and
/// its age beside ours is at most `window`; a marker exactly `window` older still threatens, and
/// one a millisecond older does not. We win iff nothing threatens. A list we are not in is not a
/// win.
pub(crate) fn won_claim(markers: &[ClaimMarker], my_id: &str, window: Duration) -> bool {
    let Some(mine) = markers.iter().find(|m| m.id == my_id) else {
        return false;
    };
    let window = u64::try_from(window.as_millis()).unwrap_or(u64::MAX);
    !markers.iter().any(|m| {
        let earlier = (m.posted_at, m.id.as_str()) < (mine.posted_at, mine.id.as_str());
        m.id != my_id && earlier && mine.posted_at - m.posted_at <= window
    })
}

/// Whether `card` is worth racing for under `cfg`: it is not archived, not done, does not
/// carry [`CLAIMED_LABEL`], carries every one of `only_labels` and none of `skip_labels`, and
/// its `blocked` is what `only_blocked` asks for when that is set.
pub(crate) fn claimable(card: &CardAnswer, cfg: &ServiceConfig) -> bool {
    let has = |label: &str| card.labels.iter().any(|l| l == label);
    !card.archived
        && !card.done
        && !has(CLAIMED_LABEL)
        && cfg.only_labels.iter().all(|l| has(l))
        && !cfg.skip_labels.iter().any(|l| has(l))
        && cfg.only_blocked.is_none_or(|only| card.blocked == only)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A marker posted `ms` after the epoch.
    fn marker(id: &str, ms: u64) -> ClaimMarker {
        ClaimMarker {
            id: id.into(),
            posted_at: ms,
        }
    }

    /// One marker per id, all posted in the same millisecond: exactly one of the contenders
    /// judges itself the winner, and its id comes back.
    fn sole_winner(ids: &[&str]) -> String {
        let all: Vec<ClaimMarker> = ids.iter().map(|id| marker(id, 100_000)).collect();
        let winners: Vec<&str> = ids
            .iter()
            .copied()
            .filter(|id| won_claim(&all, id, CLAIM_WINDOW))
            .collect();
        assert_eq!(winners.len(), 1, "exactly one winner among {ids:?}");
        winners[0].to_string()
    }

    fn comment(id: &str, body: &str, by_key: bool, created_at: &str) -> CommentAnswer {
        CommentAnswer {
            id: id.into(),
            body: body.into(),
            by: Some("nightly".into()),
            by_key,
            run: None,
            created_at: created_at.into(),
        }
    }

    fn card(labels: &[&str]) -> CardAnswer {
        CardAnswer {
            id: "c-1".into(),
            number: 1,
            title: "A card".into(),
            description: String::new(),
            list: "Todo".into(),
            labels: labels.iter().map(|l| (*l).to_string()).collect(),
            done: false,
            blocked: false,
            blocked_by: Vec::new(),
            blocking: Vec::new(),
            archived: false,
            checklist: Vec::new(),
            url: String::new(),
            created_at: "2026-11-14T09:30:00Z".into(),
            updated_at: "2026-11-14T09:30:00Z".into(),
        }
    }

    fn cfg() -> ServiceConfig {
        ServiceConfig {
            key: "afk_board_k".into(),
            base_url: "http://127.0.0.1:9".into(),
            claim_from: vec!["Todo".into()],
            only_labels: Vec::new(),
            skip_labels: Vec::new(),
            only_blocked: None,
        }
    }

    /// The marker literal, pinned byte for byte: another afkd process reads it off the same
    /// card.
    #[test]
    fn claim_text_carries_the_marker_and_owner() {
        assert_eq!(claim_text("afkd-4242"), "[afkd-claim] owner=afkd-4242");
        for owner in ["me", "björn-öst[bot]", ""] {
            assert!(is_claim(&claim_text(owner)), "round-trips for {owner:?}");
        }
    }

    #[test]
    fn is_claim_ignores_leading_space_and_other_text() {
        assert!(is_claim("[afkd-claim] owner=me"));
        assert!(is_claim("   [afkd-claim] owner=me"));
        assert!(is_claim("\n\n[afkd-claim] owner=me"));
        assert!(!is_claim("[afkd-ran] owner=me"));
        assert!(!is_claim(
            "seen a stray [afkd-claim] on this card — whose is it?"
        ));
        assert!(!is_claim(""));
    }

    /// A marker is a key's comment alone: a person's comment that looks like one is none, and so
    /// is a key's comment that is no claim or whose time is no instant.
    #[test]
    fn a_marker_is_built_only_from_a_comment_a_key_made() {
        let at = "2026-11-14T09:30:05Z";
        let made = comment("k1", &claim_text("afkd-1"), true, at);
        assert_eq!(
            ClaimMarker::from_comment(&made),
            Some(marker("k1", 1_794_648_605_000))
        );
        let person = comment("p1", &claim_text("afkd-1"), false, at);
        assert_eq!(ClaimMarker::from_comment(&person), None);
        let plain = comment("k2", "looks good", true, at);
        assert_eq!(ClaimMarker::from_comment(&plain), None);
        let timeless = comment("k3", &claim_text("afkd-1"), true, "yesterday");
        assert_eq!(ClaimMarker::from_comment(&timeless), None);
        assert_eq!(markers(&[made, person, plain, timeless]).len(), 1);
    }

    /// A person's comment that looks like a marker, posted before ours, never threatens it.
    #[test]
    fn a_persons_marker_look_alike_never_threatens() {
        let comments = [
            comment("p1", &claim_text("someone"), false, "2026-11-14T09:30:00Z"),
            comment("k1", &claim_text("afkd-1"), true, "2026-11-14T09:30:10Z"),
        ];
        assert!(won_claim(&markers(&comments), "k1", CLAIM_WINDOW));
    }

    #[test]
    fn won_claim_when_sole_and_earliest() {
        let mine = marker("c9", 100_000);
        assert!(won_claim(std::slice::from_ref(&mine), "c9", CLAIM_WINDOW));
        let later = marker("c10", 200_000);
        assert!(won_claim(&[mine.clone(), later], "c9", CLAIM_WINDOW));
        // A marker list we are not in at all is not a win: the read lost our claim.
        assert!(!won_claim(&[mine], "c11", CLAIM_WINDOW));
        assert!(!won_claim(&[], "c9", CLAIM_WINDOW));
    }

    #[test]
    fn loses_to_an_earlier_marker_inside_the_window() {
        let earlier = marker("rival", 50_000);
        let mine = marker("mine", 100_000);
        assert!(!won_claim(&[earlier, mine], "mine", CLAIM_WINDOW));
    }

    /// A marker exactly the window older still threatens; one a millisecond older has aged
    /// out.
    #[test]
    fn a_marker_at_the_window_threatens_and_one_just_past_it_does_not() {
        let base = 1_000_000;
        let window = CLAIM_WINDOW.as_millis() as u64;
        let mine = marker("mine", base);
        let at = marker("rival", base - window);
        assert!(!won_claim(&[at, mine.clone()], "mine", CLAIM_WINDOW));
        let past = marker("rival", base - window - 1);
        assert!(won_claim(&[past, mine], "mine", CLAIM_WINDOW));
    }

    /// Later markers never unseat us, however near.
    #[test]
    fn later_markers_never_unseat_us() {
        let mut all = vec![marker("mine", 10_000)];
        for (i, dt) in [1, 1_000, 89_999, 90_000, 7_200_000].iter().enumerate() {
            all.push(marker(&format!("r{i}"), 10_000 + dt));
        }
        assert!(won_claim(&all, "mine", CLAIM_WINDOW));
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

    /// A card is claimable when nothing holds it back, and each thing that does holds it back.
    #[test]
    fn claimable_passes_a_plain_card_and_holds_back_the_rest() {
        assert!(claimable(&card(&[]), &cfg()));
        let mut archived = card(&[]);
        archived.archived = true;
        let mut done = card(&[]);
        done.done = true;
        for held in [archived, done, card(&["afkd/claimed"])] {
            assert!(!claimable(&held, &cfg()), "{held:?}");
        }
    }

    #[test]
    fn only_labels_needs_every_label() {
        let cfg = ServiceConfig {
            only_labels: vec!["ready".into(), "bug".into()],
            ..cfg()
        };
        assert!(claimable(&card(&["bug", "ready", "x"]), &cfg));
        assert!(!claimable(&card(&["ready"]), &cfg));
        assert!(!claimable(&card(&[]), &cfg));
    }

    #[test]
    fn skip_labels_holds_back_a_card_carrying_any_and_wins_over_only_labels() {
        let cfg = ServiceConfig {
            only_labels: vec!["ready".into()],
            skip_labels: vec!["blocked".into(), "hold 🚧".into()],
            ..cfg()
        };
        assert!(claimable(&card(&["ready"]), &cfg));
        assert!(!claimable(&card(&["ready", "blocked"]), &cfg));
        assert!(!claimable(&card(&["ready", "hold 🚧"]), &cfg));
    }

    /// `only_blocked false` passes over a blocked card, `true` over a free one, and left out
    /// neither.
    #[test]
    fn only_blocked_matches_the_cards_blocked_when_set() {
        let mut blocked = card(&[]);
        blocked.blocked = true;
        let free = card(&[]);
        for (only, claims_free, claims_blocked) in [
            (Some(false), true, false),
            (Some(true), false, true),
            (None, true, true),
        ] {
            let cfg = ServiceConfig {
                only_blocked: only,
                ..cfg()
            };
            assert_eq!(claimable(&free, &cfg), claims_free, "{only:?}");
            assert_eq!(claimable(&blocked, &cfg), claims_blocked, "{only:?}");
        }
    }
}
