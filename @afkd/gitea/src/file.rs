//! The files an issue links, found: the reader that finds each attachment link in an
//! issue's body and comments ([`files_linked_by`]), the matcher that says which links are
//! this instance's attachments ([`AttachmentBase`]), the rewrite that points a brief at the
//! downloaded copies ([`relink`]), and the name each copy is given ([`task_file_name`]).
//!
//! Gitea stores what a person drops into an issue or a comment as an attachment and leaves
//! a markdown link to it in the text: an image embedded `![name](/attachments/<uuid>)`, any
//! other file linked `[name](/attachments/<uuid>)`. So a read of an issue's files is a read
//! of the links its body and thread carry, each fetched from the instance's
//! `/attachments/<uuid>` with the token the API takes
//! ([`GiteaClient::download_attachment`](crate::client::GiteaClient::download_attachment)).
//! The reader and the rewrite are `@afkd/linear`'s.

use std::ops::Range;

use crate::claim::is_claim;
use crate::client::{trim_base, IssueComment};

/// The longest link text [`linked_files`] reads before it gives the bracket up: what keeps a
/// body of a thousand unclosed `[` from costing a scan per bracket of the whole text.
const MAX_LINK_TEXT: usize = 1024;

/// The most bytes [`linked_files`] reads after a link's `](` for its destination, its title
/// and the `)` that closes it, before it gives the link up: what bounds the scan one bracket
/// costs however its destination runs, so no text costs a scan per bracket of the whole of
/// it. A Gitea attachment url is a tenth of it, so a longer destination is no Gitea file.
const MAX_LINK_DESTINATION: usize = 2048;

/// The most parentheses a bare link destination nests before [`linked_files`] gives the link
/// up, a limit CommonMark leaves to the reader. A run of `[a](` that never closes then costs
/// each bracket a few dozen bytes of scan, not [`MAX_LINK_DESTINATION`].
const MAX_LINK_PARENS: usize = 32;

/// Which urls are this instance's attachments, and what uuid each names: built from the
/// configured `base_url`, it holds the base's origin and its path prefix, the sub-path the
/// instance is served under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AttachmentBase {
    /// `scheme://host[:port]` in lower case, a scheme's default port left out; `None` when
    /// the base is no absolute url, which no absolute link then matches.
    origin: Option<String>,
    /// The base's path with no trailing `/`: `""` for an instance at its host's root,
    /// `/gitea` for one under a sub-path.
    prefix: String,
}

/// `url` split at its `://`, as the lower-case scheme, the lower-case authority with a
/// default port taken off, and the rest from the path on. `None` for a url with no scheme
/// or an authority that names a user, which is no host this instance is at.
fn split_origin(url: &str) -> Option<(String, String, &str)> {
    let (scheme, rest) = url.split_once("://")?;
    if scheme.is_empty()
        || !scheme
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+')
    {
        return None;
    }
    let at = rest.find('/').unwrap_or(rest.len());
    let (authority, path) = rest.split_at(at);
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    let scheme = scheme.to_ascii_lowercase();
    let authority = authority.to_ascii_lowercase();
    let default = match scheme.as_str() {
        "http" => ":80",
        "https" => ":443",
        _ => "",
    };
    let authority = match authority.strip_suffix(default) {
        Some(host) if !default.is_empty() => host.to_string(),
        _ => authority,
    };
    Some((scheme, authority, path))
}

impl AttachmentBase {
    /// The matcher for an instance at `base_url`, trimmed as the client trims it
    /// ([`trim_base`]).
    pub(crate) fn new(base_url: &str) -> Self {
        let base = trim_base(base_url);
        match split_origin(base) {
            Some((scheme, authority, path)) => Self {
                origin: Some(format!("{scheme}://{authority}")),
                prefix: path.trim_end_matches('/').to_string(),
            },
            None => Self {
                origin: None,
                prefix: String::new(),
            },
        }
    }

    /// The uuid of the attachment `url` links, or `None` when it is not one of this
    /// instance's. A url is a relative one, from the root (`/attachments/<uuid>`), or an
    /// absolute one at this instance's origin — the scheme and port equal, the host in any
    /// case. Its query and fragment are dropped, then the instance's path prefix is
    /// stripped (a link without it is no link into this instance), and what is left must be
    /// `/attachments/<uuid>` or `/<owner>/<repo>/attachments/<uuid>`, the uuid 36
    /// characters of hex and hyphens. A bare url, a url with more segments, a foreign host
    /// and the other scheme are refused.
    pub(crate) fn attachment_uuid(&self, url: &str) -> Option<String> {
        let url = url.split('#').next().unwrap_or_default();
        let url = url.split('?').next().unwrap_or_default();
        let path = if url.starts_with("//") {
            return None;
        } else if url.starts_with('/') {
            url
        } else {
            let (scheme, authority, path) = split_origin(url)?;
            let origin = format!("{scheme}://{authority}");
            if self.origin.as_deref() != Some(origin.as_str()) {
                return None;
            }
            path
        };
        let path = match path.strip_prefix(self.prefix.as_str()) {
            Some(rest) if rest.starts_with('/') => rest,
            _ => return None,
        };
        let segments: Vec<&str> = path[1..].split('/').collect();
        let uuid = match segments.as_slice() {
            ["attachments", uuid] => uuid,
            [owner, repo, "attachments", uuid] if !owner.is_empty() && !repo.is_empty() => uuid,
            _ => return None,
        };
        let is_uuid = uuid.len() == 36 && uuid.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-');
        is_uuid.then(|| (*uuid).to_string())
    }
}

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

/// Every link and image in `markdown` whose url `is_gitea` takes, in the order written, as
/// its text unescaped and its url: `[t](url)`, `![t](url)`, a `<url>` destination, and a
/// destination followed by a title. A bare url is no link, an escaped `\[` opens none, and a
/// bracket whose text runs past a paragraph or [`MAX_LINK_TEXT`], or whose destination and
/// title run past [`MAX_LINK_DESTINATION`], is no link either. Nor, as CommonMark reads them,
/// is a `<url>` holding a `<`, a `(title)` holding a `(`, or a url nesting more than
/// [`MAX_LINK_PARENS`] parentheses. A link inside another's text — an image in a link — is
/// found too.
pub(crate) fn linked_files(
    markdown: &str,
    is_gitea: &dyn Fn(&str) -> bool,
) -> Vec<(String, String)> {
    links_to(markdown, is_gitea)
        .into_iter()
        .map(|(text, url, _)| (text, url))
        .collect()
}

/// Every link [`linked_files`] reads, as its text, its url and the byte range of its
/// destination in `markdown`, a `<url>`'s brackets included.
pub(crate) fn links_to(
    markdown: &str,
    is_gitea: &dyn Fn(&str) -> bool,
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
            if is_gitea(&url) {
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
/// at a char boundary, keeping an extension, and is never `README.md` in any case, nor the
/// `issue` and `pr` folders `task/` holds the numbers in. A name in `taken` (compared
/// without case) gets `-2`, `-3` before its extension, the first free.
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
            || candidate.eq_ignore_ascii_case("issue")
            || candidate.eq_ignore_ascii_case("pr")
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

/// Every file `is_gitea` takes that `body` embeds or links, then `comments` oldest first, as
/// its name and its url: claim markers left out, each url once.
pub(crate) fn files_linked_by(
    body: &str,
    comments: &[IssueComment],
    is_gitea: &dyn Fn(&str) -> bool,
) -> Vec<(String, String)> {
    let mut thread: Vec<&IssueComment> = comments.iter().filter(|c| !is_claim(&c.body)).collect();
    thread.sort_by_key(|c| (c.created_at, c.id));
    let mut files: Vec<(String, String)> = Vec::new();
    for text in std::iter::once(body).chain(thread.iter().map(|c| c.body.as_str())) {
        for (name, url) in linked_files(text, is_gitea) {
            if !files.iter().any(|(_, seen)| *seen == url) {
                files.push((name, url));
            }
        }
    }
    files
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claim::claim_text;
    use crate::client::User;
    use std::time::{Duration, UNIX_EPOCH};

    const UUID: &str = "0a1b2c3d-4e5f-6789-abcd-ef0123456789";

    fn root() -> AttachmentBase {
        AttachmentBase::new("https://gitea.example.com")
    }

    fn is_file(url: &str) -> bool {
        root().attachment_uuid(url).is_some()
    }

    fn comment(id: u64, at: u64, body: &str) -> IssueComment {
        IssueComment {
            id,
            body: body.to_string(),
            user: User {
                login: "alice".into(),
            },
            created_at: UNIX_EPOCH + Duration::from_secs(at),
            updated_at: UNIX_EPOCH + Duration::from_secs(at),
        }
    }

    /// With a root base: a relative link, an absolute one at the same origin (the host in any
    /// case, the scheme's default port spelled out) and a repo-scoped one match, with a query
    /// or fragment dropped; and a foreign host, the other scheme, another port, a user in
    /// the authority, a protocol-relative url, a non-uuid, extra segments, a bare url and a
    /// bare `/attachments/` do not.
    #[test]
    fn a_root_base_matches_this_instances_attachments_only() {
        let base = root();
        let yes = [
            format!("/attachments/{UUID}"),
            format!("https://gitea.example.com/attachments/{UUID}"),
            format!("https://GITEA.Example.com/attachments/{UUID}"),
            format!("HTTPS://gitea.example.com:443/attachments/{UUID}"),
            format!("/acme/widgets/attachments/{UUID}"),
            format!("https://gitea.example.com/acme/widgets/attachments/{UUID}"),
            format!("/attachments/{UUID}?download=1#top"),
            format!("/attachments/{}", UUID.to_uppercase()),
        ];
        for url in &yes {
            assert!(base.attachment_uuid(url).is_some(), "{url}");
        }
        assert_eq!(
            base.attachment_uuid(&yes[4]).as_deref(),
            Some(UUID),
            "the uuid is what the link names"
        );
        let no = [
            format!("https://evil.example.com/attachments/{UUID}"),
            format!("https://gitea.example.com.evil.example/attachments/{UUID}"),
            format!("http://gitea.example.com/attachments/{UUID}"),
            format!("https://gitea.example.com:8443/attachments/{UUID}"),
            format!("https://user@gitea.example.com/attachments/{UUID}"),
            format!("//gitea.example.com/attachments/{UUID}"),
            "/attachments/not-a-uuid".to_string(),
            format!("/attachments/{}x", &UUID[..35]),
            format!("/attachments/{}g", &UUID[..35]),
            format!("/attachments/{UUID}/extra"),
            format!("/attachments/{UUID}/"),
            format!("/a/b/c/attachments/{UUID}"),
            format!("//attachments/{UUID}"),
            format!("/acme//attachments/{UUID}"),
            format!("/acme/widgets/files/{UUID}"),
            format!("attachments/{UUID}"),
            UUID.to_string(),
            "/attachments/".to_string(),
            "/attachments".to_string(),
            "/".to_string(),
            String::new(),
            "https://gitea.example.com".to_string(),
        ];
        for url in &no {
            assert_eq!(base.attachment_uuid(url), None, "{url}");
        }
    }

    /// Under a sub-path base `https://host/gitea/`, the editor's relative link and the
    /// absolute `browser_download_url` both carry the prefix and match, repo-scoped too; a
    /// link under another prefix, or with none, is refused, and so is the prefix's
    /// lookalike.
    #[test]
    fn a_sub_path_base_matches_its_prefix_only() {
        let base = AttachmentBase::new("  https://host/gitea/  ");
        for url in [
            format!("/gitea/attachments/{UUID}"),
            format!("https://host/gitea/attachments/{UUID}"),
            format!("/gitea/acme/widgets/attachments/{UUID}"),
            format!("https://HOST/gitea/acme/widgets/attachments/{UUID}?x=1"),
        ] {
            assert_eq!(base.attachment_uuid(&url).as_deref(), Some(UUID), "{url}");
        }
        for url in [
            format!("/other/attachments/{UUID}"),
            format!("/attachments/{UUID}"),
            format!("/acme/widgets/attachments/{UUID}"),
            format!("/gitea2/attachments/{UUID}"),
            format!("/giteaattachments/{UUID}"),
            format!("https://host/other/attachments/{UUID}"),
            format!("https://host/attachments/{UUID}"),
            format!("https://other/gitea/attachments/{UUID}"),
            format!("http://host/gitea/attachments/{UUID}"),
            "/gitea".to_string(),
            "/gitea/".to_string(),
        ] {
            assert_eq!(base.attachment_uuid(&url), None, "{url}");
        }
    }

    /// A base with a port keeps it in the origin; a base that is no absolute url matches
    /// relative links only.
    #[test]
    fn a_base_with_a_port_or_no_origin() {
        let base = AttachmentBase::new("http://127.0.0.1:3000");
        assert!(base
            .attachment_uuid(&format!("http://127.0.0.1:3000/attachments/{UUID}"))
            .is_some());
        assert_eq!(
            base.attachment_uuid(&format!("http://127.0.0.1:3001/attachments/{UUID}")),
            None
        );
        assert_eq!(
            base.attachment_uuid(&format!("http://127.0.0.1/attachments/{UUID}")),
            None
        );
        let broken = AttachmentBase::new("gitea.example.com");
        assert!(broken
            .attachment_uuid(&format!("/attachments/{UUID}"))
            .is_some());
        assert_eq!(
            broken.attachment_uuid(&format!("https://gitea.example.com/attachments/{UUID}")),
            None
        );
    }

    /// The reader finds each attachment link in the order written, and only those: two on
    /// one line, a `<url>` destination, brackets nested in the text, an escaped bracket, a
    /// title, an image inside a link, parentheses in a url. A lookalike host, plain http, a
    /// link elsewhere, a bare url, an escaped bracket, a bracket with no destination, text
    /// broken by a paragraph and one that never closes are no Gitea file.
    #[test]
    fn linked_files_reads_every_gitea_link_and_no_other() {
        let u = |n: u32| format!("/attachments/00000000-0000-0000-0000-{n:012}");
        let markdown = format!(
            "Before ![a.png]({}) and [b.log]({}) on one line.\n\n\
             ![修复 截图](<{}>)\n\
             [a [nested] name]({} \"a title\")\n\
             [esc \\] aped]({} 'single')\n\
             [![badge]({})]({})\n\
             ![evil](https://evil.example/x/00000000-0000-0000-0000-000000000099)\n\
             [elsewhere](https://example.com/x.png)\n\
             bare /attachments/00000000-0000-0000-0000-000000000098\n\
             \\[escaped]({})\n\
             [no destination] ({})\n\
             [broken\n\nparagraph]({})\n\
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
            u(12),
            "(".repeat(MAX_LINK_PARENS + 1) + &")".repeat(MAX_LINK_PARENS + 1),
            u(13),
            u(14),
        );
        let found = linked_files(&markdown, &|url: &str| url.starts_with("/attachments/"));
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
                ("parens".to_string(), u(11) + "(1)(2)"),
            ]
        );
        assert!(linked_files(&"[".repeat(50_000), &|_: &str| true).is_empty());
        assert!(linked_files("", &|_: &str| true).is_empty());
    }

    /// The reader finds the links Gitea's own editor writes when the instance's own base
    /// decides which are files: an image relative, a file repo-scoped; a link at another
    /// host and one with a malformed uuid are not.
    #[test]
    fn linked_files_reads_the_links_the_editor_writes() {
        let base = root();
        let md = format!(
            "![shot.png](/attachments/{UUID})\n\
             [short.pdf](https://gitea.example.com/attachments/{UUID}-x)\n\
             [other](https://example.com/attachments/{UUID})\n\
             [report.pdf](/acme/widgets/attachments/{UUID})"
        );
        let found = linked_files(&md, &|url| base.attachment_uuid(url).is_some());
        assert_eq!(
            found,
            [
                ("shot.png".to_string(), format!("/attachments/{UUID}")),
                (
                    "report.pdf".to_string(),
                    format!("/acme/widgets/attachments/{UUID}")
                ),
            ]
        );
    }

    /// A megabyte of links that never close — an unclosed destination, an unclosed `<url>`,
    /// an unclosed title — is read in one pass, not a pass of the whole text per bracket.
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
        let (a, b) = ("/attachments/a", "https://host/attachments/b");
        let to = |url: &str| match url {
            "/attachments/a" => Some("a 修复.png".to_string()),
            "https://host/attachments/b" => Some("b.log".to_string()),
            _ => None,
        };
        let markdown = format!(
            "![a]({a}) and [b](<{b}>)\n\
             [c]({a} \"a title\")\n\
             [d](<{b}> 'single' )\n\
             [![badge]({a})]({b} (paren))\n\
             [other](/attachments/c) [ext](https://example.com/x.png)\n\
             bare {a}, [no destination] ({a})\n\
             修复 [esc \\] aped]({a}) end"
        );
        assert_eq!(
            relink(&markdown, &to),
            "![a](<a 修复.png>) and [b](<b.log>)\n\
             [c](<a 修复.png> \"a title\")\n\
             [d](<b.log> 'single' )\n\
             [![badge](<a 修复.png>)](<b.log> (paren))\n\
             [other](/attachments/c) [ext](https://example.com/x.png)\n\
             bare /attachments/a, [no destination] (/attachments/a)\n\
             修复 [esc \\] aped](<a 修复.png>) end"
        );
        assert_eq!(relink(&markdown, &|_: &str| None), markdown);
        assert_eq!(relink("", &to), "");
        assert_eq!(relink("no links 修复", &to), "no links 修复");
    }

    /// A `<url>` destination that itself holds a whole link is one destination: the relink
    /// writes the outer one's name and leaves the link inside it out, never a corrupt mix.
    #[test]
    fn relink_leaves_a_destination_inside_another_alone() {
        let markdown = "[a](<x [b](/attachments/a)>) tail";
        assert_eq!(
            relink(markdown, &|_: &str| Some("n".to_string())),
            "[a](<n>) tail"
        );
    }

    /// A name is one plain file name: the link's text with `/ \ < >`, control characters and
    /// a leading `.` written `_`, the url's last segment when the text is empty, cut at a
    /// char boundary with its extension kept, never `README.md` in any case — nor `issue` or
    /// `pr`, the folders beside it — and a duplicate numbered from 2 before its extension,
    /// whatever its case.
    #[test]
    fn a_task_file_name_is_one_safe_name_and_never_a_duplicate() {
        let url = format!("/attachments/{UUID}");
        let name = |text: &str, taken: &[&str]| {
            let taken: Vec<String> = taken.iter().map(|t| t.to_string()).collect();
            task_file_name(text, &url, &taken)
        };
        assert_eq!(name("shot — 修复.png", &[]), "shot — 修复.png");
        assert_eq!(name("a/b\\c<d>e\nf\u{7}g.png", &[]), "a_b_c_d_e_f_g.png");
        assert_eq!(name("../../etc/passwd", &[]), "_._.._etc_passwd");
        assert_eq!(name(".env", &[]), "_env");
        assert_eq!(name("  report.pdf  ", &[]), "report.pdf");
        assert_eq!(name("", &[]), UUID);
        assert_eq!(
            task_file_name("", "/attachments/rep%20ort.pdf?sig=x#y", &[]),
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
        assert_eq!(name("issue", &[]), "issue-2");
        assert_eq!(name("PR", &[]), "PR-2");
        assert_eq!(name("shot.png", &["shot.png"]), "shot-2.png");
        assert_eq!(name("shot.png", &["shot.png", "shot-2.png"]), "shot-3.png");
        assert_eq!(name("shot.png", &["SHOT.PNG"]), "shot-2.png");
        assert_eq!(name("log", &["log"]), "log-2");
        assert_eq!(name("a.tar.gz", &["a.tar.gz"]), "a.tar-2.gz");
    }

    /// An image is told by its extension in any case; a dotfile and no extension are not.
    #[test]
    fn an_image_is_told_by_its_extension() {
        for name in [
            "a.png",
            "A.PNG",
            "x.jpg",
            "x.JPEG",
            "y.gif",
            "修复.webp",
            "d.svg",
        ] {
            assert!(is_image(name), "{name}");
        }
        for name in ["a.pdf", "png", ".png", "", "a.png.txt", "trailing."] {
            assert!(!is_image(name), "{name}");
        }
    }

    /// The files an issue links are its body's, then its comments' oldest first whatever
    /// order they were listed in, each url once, and a claim marker is read as no text.
    #[test]
    fn files_linked_by_reads_the_body_then_the_thread_oldest_first() {
        let a = format!("/attachments/{UUID}");
        let b = "/attachments/11111111-1111-1111-1111-111111111111";
        let c = "/attachments/22222222-2222-2222-2222-222222222222";
        let foreign = "https://example.com/x.png";
        let body = format!("![a.png]({a}) ![x.png]({foreign})");
        let thread = [
            comment(3, 300, &format!("[late.log]({c})")),
            comment(1, 100, &format!("[b.pdf]({b}) and again [a2.png]({a})")),
            comment(
                2,
                200,
                &format!("{}\n[claimed.png]({b}x)", claim_text("me")),
            ),
        ];
        let found = files_linked_by(&body, &thread, &is_file);
        assert_eq!(
            found,
            [
                ("a.png".to_string(), a),
                ("b.pdf".to_string(), b.to_string()),
                ("late.log".to_string(), c.to_string()),
            ]
        );
        assert!(files_linked_by("", &[], &is_file).is_empty());
    }
}
