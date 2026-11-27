//! The files a card links, read out of its description and comments, and the brief that
//! points at their copies.
//!
//! Trello serves a file of a card at `…/cards/<id>/attachments/<id>/download/<name>`; a
//! description or a comment links one as markdown, an image embedded `![name](url)`, any
//! other file linked `[name](url)`. The files a card's text carries are so the links its
//! description and thread hold ([`files_linked_by`]). The brief is then the card's own text
//! with each such link written as the file's relative name in the task folder ([`relink`]),
//! and the files no link reaches listed at its end ([`brief_with_files`]).

use std::ops::Range;

use crate::board::Comment;
use crate::card::{is_control_marker, FetchedFile};

/// The longest link text [`linked_files`] reads before it gives the bracket up: what keeps a
/// description of a thousand unclosed `[` from costing a scan per bracket of the whole text.
const MAX_LINK_TEXT: usize = 1024;

/// The most bytes [`linked_files`] reads after a link's `](` for its destination, its title
/// and the `)` that closes it, before it gives the link up: what bounds the scan one bracket
/// costs however its destination runs, so no text costs a scan per bracket of the whole of
/// it. A Trello download url is a tenth of it, so a longer destination is no Trello file.
const MAX_LINK_DESTINATION: usize = 2048;

/// The most parentheses a bare link destination nests before [`linked_files`] gives the link
/// up, a limit CommonMark leaves to the reader. A run of `[a](` that never closes then costs
/// each bracket a few dozen bytes of scan, not [`MAX_LINK_DESTINATION`].
const MAX_LINK_PARENS: usize = 32;

/// Whether a file named `name` is an image, by its extension in any case: what a brief
/// embeds with a `!` rather than links.
pub(crate) fn is_image(name: &str) -> bool {
    match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => matches!(
            extension.to_ascii_lowercase().as_str(),
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg"
        ),
        _ => false,
    }
}

/// `name` as a link's text: every `\`, `[` and `]` escaped, so no name can end the link early.
pub(crate) fn escape_link_text(name: &str) -> String {
    let mut text = String::with_capacity(name.len());
    for c in name.chars() {
        if matches!(c, '\\' | '[' | ']') {
            text.push('\\');
        }
        text.push(c);
    }
    text
}

/// Every link and image in `markdown` whose url `is_file` takes, in the order written, as
/// its text unescaped and its url: `[t](url)`, `![t](url)`, a `<url>` destination, and a
/// destination followed by a title. A bare url is no link, an escaped `\[` opens none, and a
/// bracket whose text runs past a paragraph or [`MAX_LINK_TEXT`], or whose destination and
/// title run past [`MAX_LINK_DESTINATION`], is no link either. Nor, as CommonMark reads them,
/// is a `<url>` holding a `<`, a `(title)` holding a `(`, or a url nesting more than
/// [`MAX_LINK_PARENS`] parentheses. A link inside another's text — an image in a link — is
/// found too.
pub(crate) fn linked_files(
    markdown: &str,
    is_file: &dyn Fn(&str) -> bool,
) -> Vec<(String, String)> {
    links_to(markdown, is_file)
        .into_iter()
        .map(|(text, url, _)| (text, url))
        .collect()
}

/// Every link [`linked_files`] reads, as its text, its url and the byte range of its
/// destination in `markdown`, a `<url>`'s brackets included.
pub(crate) fn links_to(
    markdown: &str,
    is_file: &dyn Fn(&str) -> bool,
) -> Vec<(String, String, Range<usize>)> {
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(open) = markdown[from..].find('[') {
        let start = from + open;
        let escapes = markdown[..start]
            .bytes()
            .rev()
            .take_while(|&b| b == b'\\')
            .count();
        if escapes % 2 == 1 {
            from = start + 1;
            continue;
        }
        if let Some((text, url, destination)) = link_at(markdown, start) {
            if is_file(&url) {
                found.push((text, url, destination));
            }
        }
        from = start + 1;
    }
    found
}

/// `markdown` with the destination of every link [`linked_files`] reads whose url `to` maps
/// — to a file name — written `<name>` in its place. Every other byte stays as it was
/// written: a link `to` does not map, a title, the text of a link and an image inside it.
/// A destination inside another's (a `<url>` that holds a whole link) is left as written.
pub(crate) fn relink(markdown: &str, to: &dyn Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(markdown.len());
    let mut done = 0;
    let mut links = links_to(markdown, &|url| to(url).is_some());
    links.sort_by_key(|(_, _, destination)| destination.start);
    for (_, url, destination) in links {
        let (Some(name), true) = (to(&url), destination.start >= done) else {
            continue;
        };
        out.push_str(&markdown[done..destination.start]);
        out.push('<');
        out.push_str(&name);
        out.push('>');
        done = destination.end;
    }
    out.push_str(&markdown[done..]);
    out
}

/// The longest file name [`task_file_name`] gives, in bytes, before a duplicate's suffix:
/// well under the 255 a file system takes, with the temp name a write adds.
const MAX_NAME_BYTES: usize = 100;

/// The longest extension [`task_file_name`] keeps when it cuts a name, in bytes.
const MAX_EXTENSION_BYTES: usize = 16;

/// The file name a link to `url` with the text `text` is downloaded under: the text, or with
/// none the url's last path segment, with `/`, `\`, `<`, `>`, every control character and a
/// leading `.` written `_`, so it is one plain file that links safely as `<name>`. It is cut
/// at a char boundary, keeping an extension, and is never `README.md` in any case, the one
/// file afkd frames. A name in `taken` (compared without case) gets `-2`, `-3` before its
/// extension, the first free.
pub(crate) fn task_file_name(text: &str, url: &str, taken: &[String]) -> String {
    let text = text.trim();
    let from_url = || {
        let path = url.split(['?', '#']).next().unwrap_or_default();
        path.rsplit('/').find(|s| !s.is_empty()).unwrap_or_default()
    };
    let named = if text.is_empty() { from_url() } else { text };
    let named = if named.is_empty() { "file" } else { named };
    let mut name: String = named
        .chars()
        .map(|c| match c {
            '/' | '\\' | '<' | '>' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    if name.starts_with('.') {
        name.replace_range(..1, "_");
    }
    let (stem, extension) = match name.rsplit_once('.') {
        Some((stem, extension)) if extension.len() <= MAX_EXTENSION_BYTES => {
            (stem.to_string(), format!(".{extension}"))
        }
        _ => (name, String::new()),
    };
    let mut cap = MAX_NAME_BYTES
        .saturating_sub(extension.len())
        .min(stem.len());
    while !stem.is_char_boundary(cap) {
        cap -= 1;
    }
    let stem = &stem[..cap];
    let is_taken = |candidate: &str| {
        candidate.eq_ignore_ascii_case("README.md")
            || taken.iter().any(|t| t.eq_ignore_ascii_case(candidate))
    };
    let mut candidate = format!("{stem}{extension}");
    let mut n = 1;
    while is_taken(&candidate) {
        n += 1;
        candidate = format!("{stem}-{n}{extension}");
    }
    candidate
}

/// The link whose text opens at the `[` at byte `start` of `md`: its text with escapes
/// undone, its url, and the byte range of its destination — the `<url>` with its brackets, or
/// the bare url.
fn link_at(md: &str, start: usize) -> Option<(String, String, Range<usize>)> {
    let mut text = String::new();
    let (mut depth, mut escaped, mut close) = (0usize, false, None);
    let mut last = '[';
    for (i, c) in md[start + 1..].char_indices() {
        if i > MAX_LINK_TEXT || (c == '\n' && last == '\n') {
            return None;
        }
        last = c;
        if escaped {
            if !c.is_ascii_punctuation() {
                text.push('\\');
            }
            text.push(c);
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            ']' if depth == 0 => {
                close = Some(start + 1 + i);
                break;
            }
            '[' => depth += 1,
            ']' => depth -= 1,
            _ => {}
        }
        if c != '\\' {
            text.push(c);
        }
    }
    let rest = md[close? + 1..].strip_prefix('(')?;
    let rest_at = close? + 2;
    let mut cap = rest.len().min(MAX_LINK_DESTINATION);
    while !rest.is_char_boundary(cap) {
        cap -= 1;
    }
    let rest = &rest[..cap];
    // Stop each scan where CommonMark ends the part it reads: a `<url>` at a second `<`, a
    // bare url past `MAX_LINK_PARENS` open parentheses, a `(title)` at a second `(`. So the
    // scan from one bracket never runs on through the brackets after it.
    let (url, after, destination) = match rest.strip_prefix('<') {
        Some(inner) => {
            let end = inner.find(['>', '<', '\n'])?;
            if !inner[end..].starts_with('>') {
                return None;
            }
            (&inner[..end], &inner[end + 1..], rest_at..rest_at + end + 2)
        }
        None => {
            let mut depth = 0usize;
            let mut end = rest.len();
            for (i, c) in rest.char_indices() {
                match c {
                    '(' if depth == MAX_LINK_PARENS => return None,
                    '(' => depth += 1,
                    ')' if depth == 0 => {
                        end = i;
                        break;
                    }
                    ')' => depth -= 1,
                    c if c.is_whitespace() => {
                        end = i;
                        break;
                    }
                    _ => {}
                }
            }
            (&rest[..end], &rest[end..], rest_at..rest_at + end)
        }
    };
    let after = after.trim_start();
    let after = match after.chars().next()? {
        ')' => after,
        quote @ ('"' | '\'') => {
            let body = &after[1..];
            body[body.find(quote)? + 1..].trim_start()
        }
        '(' => {
            let body = &after[1..];
            let end = body.find(['(', ')'])?;
            body[end..].strip_prefix(')')?.trim_start()
        }
        _ => return None,
    };
    after
        .starts_with(')')
        .then(|| (text, url.to_string(), destination))
}

/// Every file `is_file` takes that `description` embeds or links, then `comments` oldest
/// first, as its name and its url: control markers left out, each url once.
pub(crate) fn files_linked_by(
    description: &str,
    comments: &[Comment],
    is_file: &dyn Fn(&str) -> bool,
) -> Vec<(String, String)> {
    let mut thread: Vec<&Comment> = comments
        .iter()
        .filter(|c| !is_control_marker(&c.text))
        .collect();
    thread.sort_by(|a, b| (a.posted_at, &a.id).cmp(&(b.posted_at, &b.id)));
    let mut files: Vec<(String, String)> = Vec::new();
    for text in std::iter::once(description).chain(thread.iter().map(|c| c.text.as_str())) {
        for (name, url) in linked_files(text, is_file) {
            if !files.iter().any(|(_, seen)| *seen == url) {
                files.push((name, url));
            }
        }
    }
    files
}

/// The brief `brief` with each link to a file in `fetched` written as its name ([`relink`]),
/// and, when `fetched` holds a file the brief does not link — an attachment only, or one
/// only a comment outside the brief links — a closing `Files` section that links each of
/// those by its relative name, an image embedded.
pub(crate) fn brief_with_files(brief: &str, fetched: &[FetchedFile]) -> String {
    let name_of = |url: &str| {
        fetched
            .iter()
            .find(|file| file.url == url)
            .map(|file| file.name.clone())
    };
    let mut out = relink(brief, &name_of);
    let carried: Vec<String> = links_to(brief, &|url| name_of(url).is_some())
        .into_iter()
        .map(|(_, url, _)| url)
        .collect();
    let elsewhere: Vec<&FetchedFile> = fetched
        .iter()
        .filter(|file| !carried.contains(&file.url))
        .collect();
    if elsewhere.is_empty() {
        return out;
    }
    let kept = out.trim_end_matches('\n').len();
    out.drain(kept..);
    out.push_str(
        "\n\n## Files\n\nThese files are attached to the card, and are downloaded beside \
         this brief:\n\n",
    );
    for file in elsewhere {
        let bang = if is_image(&file.name) { "!" } else { "" };
        out.push_str(&format!(
            "- {bang}[{}](<{}>)\n",
            escape_link_text(&file.name),
            file.name
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{Duration, UNIX_EPOCH};

    /// A Trello download url of attachment `n`.
    fn url(n: u32) -> String {
        format!("https://trello.com/1/cards/c1/attachments/a{n}/download/x")
    }

    /// Whether a url is Trello's, for these tests.
    fn trello(url: &str) -> bool {
        url.starts_with("https://trello.com/")
    }

    /// The reader finds each Trello-hosted link in the order written, and only those: two on
    /// one line, a `<url>` destination, brackets nested in the text, an escaped bracket, a
    /// title, an image inside a link. A lookalike host, plain http, a link elsewhere, a bare
    /// url, an escaped bracket, a bracket with no destination, text broken by a paragraph
    /// and one that never closes are no Trello file.
    #[test]
    fn linked_files_reads_every_trello_link_and_no_other() {
        let u = url;
        let markdown = format!(
            "Before ![a.png]({}) and [b.log]({}) on one line.\n\n\
             ![修复 截图](<{}>)\n\
             [a [nested] name]({} \"a title\")\n\
             [esc \\] aped]({} 'single')\n\
             [![badge]({})]({})\n\
             ![evil](https://trello.com.evil.example/1/cards/c1/attachments/a9/download/x)\n\
             ![plain](http://trello.com/1/cards/c1/attachments/a9/download/x)\n\
             [elsewhere](https://example.com/x.png)\n\
             bare https://trello.com/1/cards/c1/attachments/a9/download/bare\n\
             \\[escaped]({})\n\
             [no destination] ({})\n\
             [broken\n\nparagraph]({})\n\
             ![long]({}{})\n\
             ![too long]({}{})\n\
             [parens]({}(1)(2))\n\
             [too deep]({}{})\n\
             [pointy](<{}<x>)\n\
             [title]({} (a (b))\n\
             [never closes",
            u(1),
            u(2),
            u(3),
            u(4),
            u(5),
            u(6),
            u(7),
            u(8),
            u(9),
            u(10),
            u(11),
            "a".repeat(MAX_LINK_DESTINATION / 2),
            u(12),
            "a".repeat(MAX_LINK_DESTINATION),
            u(13),
            u(14),
            "(".repeat(MAX_LINK_PARENS + 1) + &")".repeat(MAX_LINK_PARENS + 1),
            u(15),
            u(16),
        );
        let found = linked_files(&markdown, &trello);
        assert_eq!(
            found,
            [
                ("a.png".to_string(), u(1)),
                ("b.log".to_string(), u(2)),
                ("修复 截图".to_string(), u(3)),
                ("a [nested] name".to_string(), u(4)),
                ("esc ] aped".to_string(), u(5)),
                (format!("![badge]({})", u(6)), u(7)),
                ("badge".to_string(), u(6)),
                (
                    "long".to_string(),
                    u(11) + &"a".repeat(MAX_LINK_DESTINATION / 2)
                ),
                ("parens".to_string(), u(13) + "(1)(2)"),
            ]
        );
        assert!(linked_files(&"[".repeat(50_000), &|_: &str| true).is_empty());
        assert!(linked_files("", &|_: &str| true).is_empty());
    }

    /// A megabyte of links that never close — an unclosed destination, an unclosed `<url>`,
    /// an unclosed title — is read in one pass, not a pass of the whole text per bracket. The
    /// bounded read takes milliseconds where a scan to the end per bracket takes minutes, so
    /// the bound cannot flake.
    #[test]
    fn links_that_never_close_are_read_in_linear_time() {
        for unit in [
            "[a](", "[a](<", "[a](u (", "[a](u \"", "[a](u '", "[a](()", "![a](",
        ] {
            let markdown = unit.repeat(1024 * 1024 / unit.len());
            let started = std::time::Instant::now();
            assert!(
                linked_files(&markdown, &|_: &str| true).is_empty(),
                "{unit}"
            );
            let took = started.elapsed();
            assert!(took < Duration::from_secs(5), "{unit}: {took:?}");
        }
    }

    /// A relink writes `<name>` over the destination of each link whose url is mapped, in
    /// every form a destination takes — bare, `<url>`, before a title, an image inside a
    /// link — and leaves each other byte as it was written: a url not in the map, an
    /// external link, a bare url, the titles and the text.
    #[test]
    fn relink_rewrites_each_mapped_destination_and_nothing_else() {
        let (a, b) = (url(1), url(2));
        let to = |u: &str| {
            if u == url(1) {
                Some("a 修复.png".to_string())
            } else if u == url(2) {
                Some("b.log".to_string())
            } else {
                None
            }
        };
        let other = url(3);
        let markdown = format!(
            "![a]({a}) and [b](<{b}>)\n\
             [c]({a} \"a title\")\n\
             [d](<{b}> 'single' )\n\
             [![badge]({a})]({b} (paren))\n\
             [other]({other}) [ext](https://example.com/x.png)\n\
             bare {a}, [no destination] ({a})\n\
             修复 [esc \\] aped]({a}) end"
        );
        assert_eq!(
            relink(&markdown, &to),
            format!(
                "![a](<a 修复.png>) and [b](<b.log>)\n\
                 [c](<a 修复.png> \"a title\")\n\
                 [d](<b.log> 'single' )\n\
                 [![badge](<a 修复.png>)](<b.log> (paren))\n\
                 [other]({other}) [ext](https://example.com/x.png)\n\
                 bare {a}, [no destination] ({a})\n\
                 修复 [esc \\] aped](<a 修复.png>) end"
            )
        );
        assert_eq!(relink(&markdown, &|_: &str| None), markdown);
        assert_eq!(relink("", &to), "");
        assert_eq!(relink("no links 修复", &to), "no links 修复");
    }

    /// A `<url>` destination that itself holds a whole link is one destination: the relink
    /// writes the outer one's name and leaves the link inside it out, never a corrupt mix.
    #[test]
    fn relink_leaves_a_destination_inside_another_alone() {
        let markdown = format!("[a](<x [b]({})>) tail", url(1));
        assert_eq!(
            relink(&markdown, &|_: &str| Some("n".to_string())),
            "[a](<n>) tail"
        );
    }

    /// A name is one plain file name: the link's text with `/ \ < >`, control characters and
    /// a leading `.` written `_`, the url's last segment when the text is empty, cut at a
    /// char boundary with its extension kept, never `README.md` in any case, and a duplicate
    /// numbered from 2 before its extension, whatever its case.
    #[test]
    fn a_task_file_name_is_one_safe_name_and_never_a_duplicate() {
        let url = "https://trello.com/1/cards/c1/attachments/a1/download/asset";
        let name = |text: &str, taken: &[&str]| {
            let taken: Vec<String> = taken.iter().map(|t| t.to_string()).collect();
            task_file_name(text, url, &taken)
        };
        assert_eq!(name("shot — 修复.png", &[]), "shot — 修复.png");
        assert_eq!(name("a/b\\c<d>e\nf\u{7}g.png", &[]), "a_b_c_d_e_f_g.png");
        assert_eq!(name("../../etc/passwd", &[]), "_._.._etc_passwd");
        assert_eq!(name(".env", &[]), "_env");
        assert_eq!(name("  report.pdf  ", &[]), "report.pdf");
        assert_eq!(name("", &[]), "asset");
        assert_eq!(
            task_file_name(
                "",
                "https://trello.com/1/cards/c1/attachments/a1/download/rep%20ort.pdf?x=1#y",
                &[]
            ),
            "rep%20ort.pdf"
        );
        assert_eq!(task_file_name("", "", &[]), "file");

        let long = format!("a{}.png", "字".repeat(100));
        let cut = name(&long, &[]);
        assert_eq!(cut, format!("a{}.png", "字".repeat(31)));
        assert!(cut.len() <= MAX_NAME_BYTES);
        let no_extension = name(&"é".repeat(200), &[]);
        assert_eq!(no_extension, "é".repeat(50));

        assert_eq!(name("README.md", &[]), "README-2.md");
        assert_eq!(name("readme.MD", &[]), "readme-2.MD");
        assert_eq!(name("README.md", &["README-2.md"]), "README-3.md");
        assert_eq!(name("shot.png", &["shot.png"]), "shot-2.png");
        assert_eq!(name("shot.png", &["shot.png", "shot-2.png"]), "shot-3.png");
        assert_eq!(name("shot.png", &["SHOT.PNG"]), "shot-2.png");
        assert_eq!(name("log", &["log"]), "log-2");
        assert_eq!(name("a.tar.gz", &["a.tar.gz"]), "a.tar-2.gz");
    }

    /// A comment `id` of `text`, posted `secs` after the epoch.
    fn comment(id: &str, text: &str, secs: u64) -> Comment {
        let at = UNIX_EPOCH + Duration::from_secs(secs);
        Comment {
            id: id.into(),
            text: text.into(),
            author: "m1".into(),
            author_name: "Alice".into(),
            posted_at: at,
            renewed_at: at,
        }
    }

    /// The files a card links are its description's, then its comments' oldest first, each
    /// url once, with a control marker's links left out and foreign links ignored.
    #[test]
    fn files_linked_by_reads_the_description_then_the_thread_oldest_first() {
        let comments = [
            comment("c3", &format!("[late.pdf]({})", url(3)), 300),
            comment("c1", &format!("again ![shot.png]({})", url(1)), 100),
            comment("c2", &format!("[mid.log]({})", url(2)), 200),
            comment(
                "c0",
                &format!("[afkd-claim] owner=x [marker.png]({})", url(9)),
                50,
            ),
            comment("c4", "[elsewhere](https://example.com/x.png)", 400),
        ];
        let description = format!("![shot.png]({}) and [d.txt]({})", url(1), url(4));
        assert_eq!(
            files_linked_by(&description, &comments, &trello),
            [
                ("shot.png".to_string(), url(1)),
                ("d.txt".to_string(), url(4)),
                ("mid.log".to_string(), url(2)),
                ("late.pdf".to_string(), url(3)),
            ]
        );
        assert!(files_linked_by("", &[], &trello).is_empty());
    }

    /// A fetched file of `name` from `url`.
    fn fetched(name: &str, url: String) -> FetchedFile {
        FetchedFile {
            name: name.into(),
            url,
            path: PathBuf::from("/state/task-files/k").join(name),
        }
    }

    /// A brief that links every file it holds gets each link as a relative name and no
    /// closing section; one with a file it does not link lists that file at the end, an
    /// image embedded and anything else linked, a name's brackets escaped.
    #[test]
    fn a_brief_links_each_file_by_name_and_lists_the_unlinked_ones() {
        let files = [
            fetched("shot.png", url(1)),
            fetched("report.pdf", url(2)),
            fetched("log [1].PNG", url(3)),
        ];
        let linked = format!("T\n\n![shot.png]({}) and [r]({})\n", url(1), url(2));
        assert_eq!(
            brief_with_files(&linked, &files[..2]),
            "T\n\n![shot.png](<shot.png>) and [r](<report.pdf>)\n"
        );
        assert_eq!(
            brief_with_files(&linked, &files),
            "T\n\n![shot.png](<shot.png>) and [r](<report.pdf>)\n\n\
             ## Files\n\n\
             These files are attached to the card, and are downloaded beside this brief:\n\n\
             - ![log \\[1\\].PNG](<log [1].PNG>)\n"
        );
        assert_eq!(
            brief_with_files("Just a title", &files[..1]),
            "Just a title\n\n## Files\n\n\
             These files are attached to the card, and are downloaded beside this brief:\n\n\
             - ![shot.png](<shot.png>)\n"
        );
        assert_eq!(brief_with_files("Just a title", &[]), "Just a title");
        assert_eq!(
            brief_with_files("[a](https://example.com/a.pdf)", &[]),
            "[a](https://example.com/a.pdf)"
        );
    }
}
