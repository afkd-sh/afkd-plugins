//! The brief's **feedback section**: the human replies a re-claimed issue carries back to
//! the agent, each attributed to its author. Ported from afkd's `afkd_forge::feedback`,
//! so a brief reads byte for byte as the built-in trigger's does.

/// One piece of human feedback delivered to the brief: the commenter's name alongside
/// what they said. `author` is a display handle, not an identity key — nothing routes or
/// gates on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FeedbackItem {
    /// Who said it, as Gitea names them.
    pub(crate) author: String,
    /// What they said, verbatim.
    pub(crate) body: String,
}

/// Render the brief's feedback section: a `## {heading}` rule, then one
/// `**{author}:** {body}` paragraph per item, oldest-first as `items` is ordered. An
/// empty slice renders `""`, so a brief with no new comments is byte-identical to one
/// written before attribution existed.
pub(crate) fn render_feedback_section(heading: &str, items: &[FeedbackItem]) -> String {
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

    fn item(author: &str, body: &str) -> FeedbackItem {
        FeedbackItem {
            author: author.to_string(),
            body: body.to_string(),
        }
    }

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
