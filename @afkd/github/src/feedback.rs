//! The **feedback decision** and the PR brief's **feedback section**. Ported from afkd's
//! `afkd_forge::feedback`, so a brief reads byte for byte as the built-in trigger's does.
//!
//! The PR kind's rule is one sentence — a PR is eligible when a human has spoken *after
//! the bot's last word* — decided by pure functions over the PR's comments and reviews:
//! the [`watermark`], the [`delta`] after it, and the [`pr_brief`] that renders the delta,
//! each item attributed to its author. The built-in GitHub issue brief carries no
//! feedback section, so the PR kind is this module's only reader.

use std::time::SystemTime;

use crate::claim::is_claim;
use crate::client::{IssueComment, Review};

/// One piece of human feedback delivered to the brief: the commenter's name alongside
/// what they said. `author` is a display handle, not an identity key — nothing routes or
/// gates on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FeedbackItem {
    /// Who said it, as GitHub names them.
    pub(crate) author: String,
    /// What they said, verbatim.
    pub(crate) body: String,
}

/// The watermark: the newest timestamp among the bot's **own** comments (`updated_at`)
/// and reviews (`submitted_at`). `None` when the bot has said nothing yet — every human
/// comment then counts as new feedback (first round).
///
/// A claim marker ([`is_claim`]) is skipped whatever its author: ours is bookkeeping, not
/// a word to measure a reply against, and a *rival's* — authored by a different login —
/// must not read as feedback and re-fire the unit forever.
pub(crate) fn watermark(
    comments: &[IssueComment],
    reviews: &[Review],
    me: &str,
) -> Option<SystemTime> {
    comments
        .iter()
        .filter(|c| c.user.login == me && !is_claim(&c.body))
        .map(|c| c.updated_at)
        .chain(
            reviews
                .iter()
                .filter(|r| r.user.login == me)
                .map(|r| r.submitted_at),
        )
        .max()
}

/// The human-feedback delta to deliver this run: others' comments/reviews with a
/// timestamp strictly after the watermark (or all of them, first round), each carrying
/// its author, **oldest-first** so a review thread reads chronologically. A review
/// collapses to its id, carrying the reviewer's name. Claim markers are dropped on the
/// same grounds as in [`watermark`]: they are the lock's bookkeeping, never conversation.
pub(crate) fn delta(comments: &[IssueComment], reviews: &[Review], me: &str) -> Vec<FeedbackItem> {
    let watermark = watermark(comments, reviews, me);
    let after = |t: SystemTime| watermark.is_none_or(|w| t > w);
    let mut items: Vec<(SystemTime, FeedbackItem)> = Vec::new();
    for c in comments {
        if c.user.login != me && !is_claim(&c.body) && after(c.updated_at) {
            items.push((
                c.updated_at,
                FeedbackItem {
                    author: c.user.login.clone(),
                    body: c.body.clone(),
                },
            ));
        }
    }
    for r in reviews {
        if r.user.login != me && after(r.submitted_at) {
            items.push((
                r.submitted_at,
                FeedbackItem {
                    author: r.user.login.clone(),
                    body: format!("(review {})", r.id),
                },
            ));
        }
    }
    items.sort_by_key(|(t, _)| *t);
    items.into_iter().map(|(_, item)| item).collect()
}

/// Whether a PR carries human feedback newer than the bot's last word.
pub(crate) fn has_new(comments: &[IssueComment], reviews: &[Review], me: &str) -> bool {
    !delta(comments, reviews, me).is_empty()
}

/// The PR-review brief written to `task.md`: a header naming the PR, then the feedback
/// delta (oldest-first, each line attributed to its author). Empty feedback (claimed
/// mid-race) still writes the header.
pub(crate) fn pr_brief(number: u64, feedback: &[FeedbackItem]) -> String {
    let mut s = format!("Address review feedback on PR #{number}.");
    s.push_str(&render_feedback_section("New feedback", feedback));
    s
}

/// One review thread, adversarial on purpose: a multi-line body with an indented code
/// block and trailing whitespace, a non-ASCII handle, and a wide-CJK + emoji body that
/// carries its own `**` markup — afkd's `afkd_forge::testutil::review_thread`, verbatim.
///
/// The brief the PR kind hands over is asserted against [`pr_brief`] over **this** thread
/// rather than against a second hand-written string; its literal shape is pinned once, in
/// this module's tests.
#[cfg(test)]
pub(crate) fn review_thread() -> Vec<FeedbackItem> {
    [
        (
            "alice",
            "this breaks the retry path when the token expires\n\n    let x = 1;\n  ",
        ),
        (
            "bob-döner",
            "disagree — the retry path already handles that, see #412",
        ),
        (
            "陳大文",
            "看起来不对 🚨 — the **bold** claim in §2 is wrong",
        ),
    ]
    .into_iter()
    .map(|(author, body)| FeedbackItem {
        author: author.to_string(),
        body: body.to_string(),
    })
    .collect()
}

/// Render the brief's feedback section: a `## {heading}` rule, then one
/// `**{author}:** {body}` paragraph per item, oldest-first as `items` is ordered. An
/// empty slice renders `""`, so a brief with no new feedback is the bare header.
fn render_feedback_section(heading: &str, items: &[FeedbackItem]) -> String {
    if items.is_empty() {
        return String::new();
    }
    let mut s = format!("\n\n## {heading}\n");
    for item in items {
        s.push('\n');
        s.push_str(&format!("**{}:** {}", item.author, item.body.trim_end()));
        s.push('\n');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claim::{claim_renewal_text, claim_text, CLAIM_MARKER};
    use crate::client::User;
    use std::time::{Duration, UNIX_EPOCH};

    fn item(author: &str, body: &str) -> FeedbackItem {
        FeedbackItem {
            author: author.to_string(),
            body: body.to_string(),
        }
    }

    /// An unedited comment: written and last touched at the same instant.
    fn comment(id: u64, author: &str, secs: u64) -> IssueComment {
        edited_comment(id, author, secs, secs)
    }

    /// A comment written at `created` and last edited at `updated` — the shape that
    /// tells the two times apart.
    fn edited_comment(id: u64, author: &str, created: u64, updated: u64) -> IssueComment {
        IssueComment {
            id,
            body: format!("comment {id}"),
            user: User {
                login: author.into(),
            },
            created_at: UNIX_EPOCH + Duration::from_secs(created),
            updated_at: UNIX_EPOCH + Duration::from_secs(updated),
        }
    }

    fn review(id: u64, author: &str, secs: u64) -> Review {
        Review {
            id,
            user: User {
                login: author.into(),
            },
            submitted_at: UNIX_EPOCH + Duration::from_secs(secs),
        }
    }

    /// The delta's bodies alone.
    fn bodies(delta: &[FeedbackItem]) -> Vec<String> {
        delta.iter().map(|i| i.body.clone()).collect()
    }

    fn authors(delta: &[FeedbackItem]) -> Vec<String> {
        delta.iter().map(|i| i.author.clone()).collect()
    }

    // --- The watermark and the delta ---

    #[test]
    fn watermark_is_the_newest_of_the_bots_own_comments_and_reviews() {
        let comments = [comment(1, "human", 100), comment(2, "me", 200)];
        let reviews = [review(3, "me", 150), review(4, "human", 250)];
        // The bot's own newest is the comment at t=200.
        assert_eq!(
            watermark(&comments, &reviews, "me"),
            Some(UNIX_EPOCH + Duration::from_secs(200))
        );
        // With nothing said by the bot, there is no watermark.
        assert_eq!(watermark(&[comment(1, "human", 100)], &[], "me"), None);
    }

    #[test]
    fn new_feedback_only_after_the_bots_last_word() {
        // bot@200; a human comment at 300 is new, one at 150 is already addressed.
        let comments = [
            comment(1, "human", 150),
            comment(2, "me", 200),
            comment(3, "human", 300),
        ];
        assert!(has_new(&comments, &[], "me"));
        assert_eq!(
            bodies(&delta(&comments, &[], "me")),
            vec!["comment 3".to_string()]
        );

        // No human comment newer than the bot's last word ⇒ idle.
        let addressed = [comment(1, "human", 150), comment(2, "me", 200)];
        assert!(!has_new(&addressed, &[], "me"));
    }

    #[test]
    fn feedback_delta_is_oldest_first_and_excludes_the_bots_own() {
        let comments = [
            comment(2, "human", 300),
            comment(1, "human", 100),
            comment(9, "me", 50),
        ];
        let reviews = [review(5, "human", 200)];
        let delta = delta(&comments, &reviews, "me");
        // Oldest-first across comments + reviews; the bot's own comment is dropped.
        assert_eq!(
            bodies(&delta),
            vec![
                "comment 1".to_string(),
                "(review 5)".to_string(),
                "comment 2".to_string(),
            ]
        );
    }

    #[test]
    fn first_round_keeps_all_human_feedback() {
        // No prior bot comment: every human comment is new.
        let comments = [comment(1, "human", 100), comment(2, "human", 200)];
        assert_eq!(
            bodies(&delta(&comments, &[], "me")),
            vec!["comment 1".to_string(), "comment 2".to_string()]
        );
    }

    /// Two distinct humans in one thread, out of order, with the bot interleaved: the
    /// delta keeps each speaker's name attached to their own body, stays oldest-first,
    /// and still excludes the bot's own word.
    #[test]
    fn the_feedback_delta_carries_each_comments_author() {
        let comments = [
            comment(2, "bob", 300),
            comment(9, "me", 50),
            comment(1, "alice", 100),
        ];
        let reviews = [review(5, "carol", 200)];
        let delta = delta(&comments, &reviews, "me");
        assert_eq!(authors(&delta), vec!["alice", "carol", "bob"]);
        assert_eq!(
            bodies(&delta),
            vec!["comment 1", "(review 5)", "comment 2"],
            "each author keeps their own body"
        );
        assert!(!authors(&delta).contains(&"me".to_string()));
    }

    /// The watermark and the delta read `updated_at`: "has this been touched since the
    /// bot last spoke" is the question, and an edit *is* something new to answer. The
    /// fixture is built so keying on `created_at` would flip every assertion below.
    #[test]
    fn an_edited_comment_is_read_by_updated_at_not_created_at() {
        let comments = [
            edited_comment(1, "me", 100, 500),
            edited_comment(2, "josefandersson", 200, 600),
            edited_comment(3, "björn-öst", 300, 400),
            edited_comment(4, "陳大文", 550, 560),
        ];
        // A bot review older than the bot's edited comment leaves the watermark on the
        // comment — the PR path folds both times through one `max`.
        let reviews = [review(5, "me", 300)];

        // The watermark is the bot's *edit* (500), not when it wrote (100).
        assert_eq!(
            watermark(&comments, &reviews, "me"),
            Some(UNIX_EPOCH + Duration::from_secs(500))
        );
        assert!(has_new(&comments, &reviews, "me"));
        // Only the two humans touched since t=500, oldest *edit* first. Under
        // `created_at` this would be all three humans, ordered 2, 3, 4.
        assert_eq!(
            authors(&delta(&comments, &reviews, "me")),
            vec!["陳大文", "josefandersson"]
        );

        // And the comment written after the bot but not touched since is not new
        // feedback on its own — under `created_at` (300 > 100) it would be.
        let stale = [comments[0].clone(), comments[2].clone()];
        assert!(!has_new(&stale, &[], "me"));
    }

    /// The adversarial thread both marker tests are judged against: multi-line bodies,
    /// non-ASCII and wide handles, an edited comment. Shared so the two read the *same*
    /// conversation and their parity assertions cannot drift apart.
    fn conversation() -> Vec<IssueComment> {
        vec![
            edited_comment(1, "me", 100, 500),
            IssueComment {
                id: 2,
                body: "still broken\n\n    retry(1);\n".into(),
                user: User {
                    login: "bob-döner".into(),
                },
                created_at: UNIX_EPOCH + Duration::from_secs(510),
                updated_at: UNIX_EPOCH + Duration::from_secs(600),
            },
            comment(3, "陳大文", 700),
        ]
    }

    /// The reviews beside [`conversation`]: the bot's own, and a human's newest.
    fn conversation_reviews() -> Vec<Review> {
        vec![review(4, "me", 300), review(5, "alice", 800)]
    }

    /// A claim marker is the lock's bookkeeping, never conversation: interleaving
    /// **ours** and a **rival's** into a real thread must move neither the watermark nor
    /// the delta nor `has_new`. Asserted as a parity of the same three reads over one
    /// fixture with and without the markers. The markers are placed where they would do
    /// damage if counted — ours *after* the bot's own last word, the rival's newest of
    /// all.
    #[test]
    fn a_claim_marker_is_not_feedback() {
        let conversation = conversation();
        let reviews = conversation_reviews();

        let mut with_markers = conversation.to_vec();
        with_markers.push(IssueComment {
            id: 6,
            body: claim_text("me"),
            user: User { login: "me".into() },
            created_at: UNIX_EPOCH + Duration::from_secs(900),
            updated_at: UNIX_EPOCH + Duration::from_secs(900),
        });
        with_markers.push(IssueComment {
            id: 7,
            body: claim_text("björn-öst[bot]"),
            user: User {
                login: "björn-öst[bot]".into(),
            },
            created_at: UNIX_EPOCH + Duration::from_secs(1000),
            updated_at: UNIX_EPOCH + Duration::from_secs(1000),
        });

        assert_eq!(
            watermark(&with_markers, &reviews, "me"),
            watermark(&conversation, &reviews, "me"),
            "a marker is not the bot's last word"
        );
        assert_eq!(
            delta(&with_markers, &reviews, "me"),
            delta(&conversation, &reviews, "me"),
            "a marker is neither delivered nor a boundary"
        );
        assert_eq!(
            has_new(&with_markers, &reviews, "me"),
            has_new(&conversation, &reviews, "me")
        );
        // Not vacuous: the marker-free read really does carry the thread, and the marker
        // text never reaches the brief.
        assert_eq!(
            authors(&delta(&with_markers, &reviews, "me")),
            vec!["bob-döner", "陳大文", "alice"]
        );
        assert!(!bodies(&delta(&with_markers, &reviews, "me"))
            .iter()
            .any(|b| b.contains(CLAIM_MARKER)));
    }

    /// A **renewed** claim marker is still bookkeeping: its fresh `updated_at` is newer
    /// than every word on the thread, exactly the shape that would swallow the newest
    /// reply if it counted as the bot speaking, or re-fire the unit forever if a rival's
    /// counted as feedback. Same parity form as the sibling above, over the same thread.
    #[test]
    fn a_renewed_claim_marker_moves_neither_the_watermark_nor_the_delta() {
        let conversation = conversation();
        let reviews = conversation_reviews();

        let renewed = |id: u64, login: &str, renewal: u64, updated: u64| IssueComment {
            id,
            body: claim_renewal_text(login, renewal),
            user: User {
                login: login.into(),
            },
            // Posted long before the thread's newest word, edited long after it.
            created_at: UNIX_EPOCH + Duration::from_secs(50),
            updated_at: UNIX_EPOCH + Duration::from_secs(updated),
        };
        let mut with_markers = conversation.to_vec();
        with_markers.push(renewed(6, "me", 132, 9_000));
        with_markers.push(renewed(7, "björn-öst[bot]", 7, 9_100));

        assert_eq!(
            watermark(&with_markers, &reviews, "me"),
            watermark(&conversation, &reviews, "me"),
            "a renewal is not the bot's last word"
        );
        assert_eq!(
            delta(&with_markers, &reviews, "me"),
            delta(&conversation, &reviews, "me"),
            "a renewal is neither delivered nor a boundary"
        );
        assert_eq!(
            has_new(&with_markers, &reviews, "me"),
            has_new(&conversation, &reviews, "me")
        );
        // Not vacuous: the renewals really are the newest things on the thread, and
        // none of their text reaches the brief.
        assert!(with_markers
            .iter()
            .all(|c| c.id < 6 || c.updated_at > UNIX_EPOCH + Duration::from_secs(800)));
        assert!(!bodies(&delta(&with_markers, &reviews, "me"))
            .iter()
            .any(|b| b.contains(CLAIM_MARKER)));
    }

    // --- The PR brief ---

    /// The whole PR brief over the shapes a real review thread carries — the shared
    /// [`review_thread`] fixture. This is where that text is pinned; the PR kind asserts
    /// the brief it hands over *equals this renderer's* output over the same fixture.
    #[test]
    fn the_pr_brief_heads_the_thread_and_attributes_every_speaker() {
        assert_eq!(
            pr_brief(412, &review_thread()),
            "Address review feedback on PR #412.\n\n## New feedback\n\
             \n**alice:** this breaks the retry path when the token expires\n\n    let x = 1;\n\
             \n**bob-döner:** disagree — the retry path already handles that, see #412\n\
             \n**陳大文:** 看起来不对 🚨 — the **bold** claim in §2 is wrong\n"
        );
    }

    /// A PR claimed mid-race carries no delta: the brief is the bare header, with no
    /// dangling `## New feedback` rule under it.
    #[test]
    fn an_empty_delta_briefs_the_bare_header() {
        assert_eq!(pr_brief(7, &[]), "Address review feedback on PR #7.");
    }

    /// The delta a real thread produces is what the brief renders — the eligibility
    /// decision and the brief read the same items, so a comment that makes a PR fire is
    /// a comment the agent is shown.
    #[test]
    fn the_brief_renders_exactly_the_delta_that_made_the_pr_eligible() {
        let comments = [
            comment(1, "alice", 100),
            comment(2, "me", 200),
            IssueComment {
                id: 3,
                body: "still broken\n\n    retry(1);\n".into(),
                user: User {
                    login: "bob-döner".into(),
                },
                created_at: UNIX_EPOCH + Duration::from_secs(300),
                updated_at: UNIX_EPOCH + Duration::from_secs(300),
            },
        ];
        let reviews = [review(5, "carol", 400)];
        assert!(has_new(&comments, &reviews, "me"));
        let delta = delta(&comments, &reviews, "me");
        assert_eq!(
            pr_brief(9, &delta),
            "Address review feedback on PR #9.\n\n## New feedback\n\
             \n**bob-döner:** still broken\n\n    retry(1);\n\
             \n**carol:** (review 5)\n"
        );
    }

    // --- The section renderer ---

    /// An empty delta renders nothing at all — not a bare heading.
    #[test]
    fn an_empty_delta_renders_no_section() {
        assert_eq!(render_feedback_section("New comments", &[]), "");
    }

    /// Two speakers in a disagreeing thread: each line is attributed, and the slice
    /// order is the rendered order.
    #[test]
    fn two_commenters_each_get_their_own_attributed_line() {
        let s = render_feedback_section(
            "New comments",
            &[
                item("alice", "this breaks the retry path when the token expires"),
                item(
                    "bob",
                    "disagree, the retry path already handles that — see #412",
                ),
            ],
        );
        assert_eq!(
            s,
            "\n\n## New comments\n\
             \n**alice:** this breaks the retry path when the token expires\n\
             \n**bob:** disagree, the retry path already handles that — see #412\n"
        );
    }

    /// The inputs that break a formatter: a multi-line body (whose interior newlines
    /// must survive), trailing whitespace (normalized away), wide CJK + emoji, a body
    /// carrying its own `**`, a non-ASCII author, and an empty body.
    #[test]
    fn adversarial_bodies_keep_their_shape_and_gain_only_the_prefix() {
        let s = render_feedback_section(
            "New comments",
            &[
                item(
                    "alice",
                    "first line\nsecond line\n\nthird after a gap   \n\n\n",
                ),
                item(
                    "陳大文",
                    "看起来不对 🚨 — the **bold** claim in §2 is wrong",
                ),
                item("bob", ""),
            ],
        );
        assert_eq!(
            s,
            "\n\n## New comments\n\
             \n**alice:** first line\nsecond line\n\nthird after a gap\n\
             \n**陳大文:** 看起来不对 🚨 — the **bold** claim in §2 is wrong\n\
             \n**bob:** \n"
        );
    }
}
