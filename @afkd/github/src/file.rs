//! The files an item links, downloaded into the task folder: the reader that finds each
//! link GitHub hosts in an item's body and comments ([`files_linked_by`]), the download
//! that puts each in a folder of its own ([`fetch_files`]), and the rewrite that points the
//! brief at the copies ([`brief_with_files`]).
//!
//! GitHub hosts what a person pastes into an issue, a pull request or a comment: an image
//! as `<img … src=…>` HTML or as `![alt](url)`, any other file as `[name](url)`. So a read
//! of an item's files is a read of the links its text carries, which the client then fetches
//! with the credential the REST API takes ([`GithubClient::download`]). The reader and the
//! rewrite are `@afkd/linear`'s, with the one tag GitHub's own editor writes added.

use std::ops::Range;
use std::path::{Path, PathBuf};

use crate::claim::is_claim;
use crate::client::{Download, GithubClient, IssueComment};
use crate::common::{write_whole, Diag};

/// The longest link text [`linked_files`] reads before it gives the bracket up: what keeps a
/// body of a thousand unclosed `[` from costing a scan per bracket of the whole text.
const MAX_LINK_TEXT: usize = 1024;

/// The most bytes [`linked_files`] reads after a link's `](` for its destination, its title
/// and the `)` that closes it, before it gives the link up: what bounds the scan one bracket
/// costs however its destination runs, so no text costs a scan per bracket of the whole of
/// it. A GitHub attachment url is a tenth of it, so a longer destination is no GitHub file.
const MAX_LINK_DESTINATION: usize = 2048;

/// The most parentheses a bare link destination nests before [`linked_files`] gives the link
/// up, a limit CommonMark leaves to the reader. A run of `[a](` that never closes then costs
/// each bracket a few dozen bytes of scan, not [`MAX_LINK_DESTINATION`].
const MAX_LINK_PARENS: usize = 32;

/// The most bytes of one `<img` tag [`linked_files`] reads, from the tag's name to its `>`,
/// before it gives the tag up: what bounds the scan one `<` costs. GitHub writes an image
/// tag in well under a quarter of it.
const MAX_TAG: usize = 4096;

/// A file the item links, downloaded to be handed over in the task folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FetchedFile {
    /// Its name in the task folder.
    pub(crate) name: String,
    /// The GitHub url it was linked at.
    pub(crate) url: String,
    /// Where it was written.
    pub(crate) path: PathBuf,
}

/// One link the reader found: where its destination is, and how to write another there.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Link {
    /// Where the link opens: its `[`, or the `<` of its `<img` tag. What the order written is
    /// read off.
    at: usize,
    /// The link's text unescaped, or an image tag's `alt`: what the file is called.
    text: String,
    url: String,
    /// The byte range of the destination in the text: a markdown `<url>` with its brackets,
    /// a bare url, or the inside of an HTML `src`'s quotes.
    destination: Range<usize>,
    /// Whether the destination is an HTML attribute value rather than a markdown one.
    html: bool,
}

/// Every link and image in `markdown` whose url `is_github` takes, in the order written, as
/// its text unescaped and its url: `[t](url)`, `![t](url)`, a `<url>` destination, a
/// destination followed by a title, and an `<img src=…>` tag, quoted either way in any
/// case, its text its `alt`. A bare url is no link, an escaped `\[` opens none, and a
/// bracket whose text runs past a paragraph or [`MAX_LINK_TEXT`], or whose destination and
/// title run past [`MAX_LINK_DESTINATION`], is no link either. Nor, as CommonMark reads them,
/// is a `<url>` holding a `<`, a `(title)` holding a `(`, or a url nesting more than
/// [`MAX_LINK_PARENS`] parentheses. A link inside another's text — an image in a link — is
/// found too.
pub(crate) fn linked_files(
    markdown: &str,
    is_github: &dyn Fn(&str) -> bool,
) -> Vec<(String, String)> {
    links_to(markdown, is_github)
        .into_iter()
        .map(|link| (link.text, link.url))
        .collect()
}

/// Every link [`linked_files`] reads, with where its destination is.
fn links_to(markdown: &str, is_github: &dyn Fn(&str) -> bool) -> Vec<Link> {
    let mut found = markdown_links(markdown);
    found.extend(img_links(markdown));
    found.retain(|link| is_github(&link.url));
    found.sort_by_key(|link| link.at);
    found
}

/// The `[t](url)` links of `markdown`, whatever their url.
fn markdown_links(markdown: &str) -> Vec<Link> {
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
            found.push(Link {
                at: start,
                text,
                url,
                destination,
                html: false,
            });
        }
        from = start + 1;
    }
    found
}

/// The `<img … src="url" …>` tags of `html`, whatever their url.
fn img_links(html: &str) -> Vec<Link> {
    let bytes = html.as_bytes();
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(open) = html[from..].find('<') {
        let start = from + open;
        from = start + 1;
        let name = &bytes[from..bytes.len().min(from + 4)];
        let is_img = name.len() == 4
            && name[..3].eq_ignore_ascii_case(b"img")
            && matches!(name[3], b' ' | b'\t' | b'\n' | b'\r' | b'\x0c' | b'/');
        if !is_img {
            continue;
        }
        if let Some((text, url, destination)) = img_at(html, from + 4) {
            found.push(Link {
                at: start,
                text,
                url,
                destination,
                html: true,
            });
        }
    }
    found
}

/// The `src` of the `<img` tag whose attributes open at byte `from` of `html`: the tag's
/// `alt` with its entities undone (empty with none), its `src` the same, and the byte range
/// of the `src` value inside its quotes. The first `src` and the first `alt` are the tag's.
/// A value with no quotes is not read, and a tag that runs past [`MAX_TAG`], meets a `<`
/// outside a quoted value, or never closes has none.
fn img_at(html: &str, from: usize) -> Option<(String, String, Range<usize>)> {
    let b = html.as_bytes();
    let end = b.len().min(from + MAX_TAG);
    let blank = |i: usize| b[i].is_ascii_whitespace();
    let (mut src, mut alt): (Option<Range<usize>>, Option<Range<usize>>) = (None, None);
    let mut i = from;
    loop {
        while i < end && (blank(i) || b[i] == b'/') {
            i += 1;
        }
        if i >= end || b[i] == b'<' {
            return None;
        }
        if b[i] == b'>' {
            break;
        }
        let name_at = i;
        while i < end && !blank(i) && !matches!(b[i], b'=' | b'/' | b'>' | b'<') {
            i += 1;
        }
        if i == name_at {
            // A stray `=`: no name to give it to.
            i += 1;
            continue;
        }
        if i >= end {
            return None;
        }
        let name = &html[name_at..i];
        while i < end && blank(i) {
            i += 1;
        }
        if i >= end || b[i] != b'=' {
            continue;
        }
        i += 1;
        while i < end && blank(i) {
            i += 1;
        }
        match b.get(i) {
            Some(&quote @ (b'"' | b'\'')) if i < end => {
                let value = i + 1;
                let close = value + b[value..end].iter().position(|&c| c == quote)?;
                if name.eq_ignore_ascii_case("src") {
                    src.get_or_insert(value..close);
                } else if name.eq_ignore_ascii_case("alt") {
                    alt.get_or_insert(value..close);
                }
                i = close + 1;
            }
            _ => {
                while i < end && !blank(i) && b[i] != b'>' {
                    i += 1;
                }
            }
        }
    }
    let src = src?;
    let text = alt.map_or_else(String::new, |alt| unescape_html(&html[alt]));
    Some((text, unescape_html(&html[src.clone()]), src))
}

/// `value` with the five entities an attribute carries undone.
fn unescape_html(value: &str) -> String {
    value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

/// `name` as an HTML attribute value, in either quote.
fn escape_html(name: &str) -> String {
    name.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// `name` as a link's text: every `\`, `[` and `]` escaped, so no name can end the link early.
fn escape_link_text(name: &str) -> String {
    let mut text = String::with_capacity(name.len());
    for c in name.chars() {
        if matches!(c, '\\' | '[' | ']') {
            text.push('\\');
        }
        text.push(c);
    }
    text
}

/// `text` with the destination of every link [`linked_files`] reads whose url `to` maps — to
/// a file name — written `<name>` in its place, or `name` as an attribute value inside the
/// quotes of an `<img>`'s `src`, `&` and the quotes escaped. Every other byte stays as it
/// was written: a link `to` does not map, a title, the text of a link and an image inside
/// it. A destination inside another's (a `<url>` that holds a whole link) is left as
/// written.
pub(crate) fn relink(text: &str, to: &dyn Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut done = 0;
    let mut links = links_to(text, &|url| to(url).is_some());
    links.sort_by_key(|link| link.destination.start);
    for link in links {
        let (Some(name), true) = (to(&link.url), link.destination.start >= done) else {
            continue;
        };
        out.push_str(&text[done..link.destination.start]);
        if link.html {
            out.push_str(&escape_html(&name));
        } else {
            out.push('<');
            out.push_str(&name);
            out.push('>');
        }
        done = link.destination.end;
    }
    out.push_str(&text[done..]);
    out
}

/// The longest file name [`task_file_name`] gives, in bytes, before a duplicate's suffix:
/// well under the 255 a file system takes, with the temp name a write adds.
const MAX_NAME_BYTES: usize = 100;

/// The longest extension [`task_file_name`] keeps when it cuts a name, in bytes.
const MAX_EXTENSION_BYTES: usize = 16;

/// Whether `name` ends in an extension: a `.` after a stem, then letters and digits.
fn has_extension(name: &str) -> bool {
    extension_of(name).is_some()
}

/// The extension `name` ends in, if it has one: a `.` after a stem, then one to
/// [`MAX_EXTENSION_BYTES`] letters and digits.
fn extension_of(name: &str) -> Option<&str> {
    let (stem, extension) = name.rsplit_once('.')?;
    (!stem.is_empty()
        && !extension.is_empty()
        && extension.len() <= MAX_EXTENSION_BYTES
        && extension.bytes().all(|b| b.is_ascii_alphanumeric()))
    .then_some(extension)
}

/// The file name a link to `url` with the text `text` is downloaded under: the text, or with
/// none the url's last path segment, with `/`, `\`, `<`, `>`, every control character and a
/// leading `.` written `_`, so it is one plain file that links safely as `<name>`. A name
/// with no extension — a pasted image's `alt` is just "Image" — gets `extension`, as the
/// response named it, when there is one. It is cut at a char boundary, keeping an extension,
/// and is never `README.md` in any case, the one file afkd frames. A name in `taken`
/// (compared without case) gets `-2`, `-3` before its extension, the first free.
pub(crate) fn task_file_name(
    text: &str,
    url: &str,
    taken: &[String],
    extension: Option<&str>,
) -> String {
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
    if let Some(extension) = extension.filter(|e| {
        !e.is_empty()
            && e.len() <= MAX_EXTENSION_BYTES
            && e.bytes().all(|b| b.is_ascii_alphanumeric())
    }) {
        if !has_extension(&name) {
            let trimmed = name.trim_end_matches('.').len();
            name.truncate(trimmed);
            name.push('.');
            name.push_str(extension);
        }
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

/// The extension a download's response names its file with, lowercased: the one on the
/// `filename` of its `Content-Disposition`, else the one its `Content-Type` is known by.
fn extension_hint(download: &Download) -> Option<String> {
    download
        .content_disposition
        .as_deref()
        .and_then(disposition_filename)
        .and_then(|name| extension_of(&name).map(str::to_ascii_lowercase))
        .or_else(|| {
            download
                .content_type
                .as_deref()
                .and_then(extension_for_type)
                .map(str::to_string)
        })
}

/// The `filename` a `Content-Disposition` gives, the `filename*` form (RFC 5987, percent
/// decoded) before the plain one, quotes taken off.
fn disposition_filename(disposition: &str) -> Option<String> {
    let mut plain = None;
    for part in disposition.split(';').skip(1) {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim().to_ascii_lowercase().as_str() {
            "filename*" => {
                // `charset'language'percent-encoded`.
                let encoded = value.splitn(3, '\'').nth(2).unwrap_or(value);
                return Some(percent_decode(encoded));
            }
            "filename" if plain.is_none() => {
                plain = Some(value.trim_matches('"').to_string());
            }
            _ => {}
        }
    }
    plain
}

/// `text` with each `%XX` undone, as UTF-8.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes
            .get(i + 1..i + 3)
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match (bytes[i], hex) {
            (b'%', Some(byte)) => {
                out.push(byte);
                i += 3;
            }
            (byte, _) => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The extension a `Content-Type` is known by, parameters ignored: the image types GitHub
/// shows inline, the documents and logs a run reads, and the video and archives it takes.
fn extension_for_type(content_type: &str) -> Option<&'static str> {
    let essence = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    Some(match essence.as_str() {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        "image/bmp" => "bmp",
        "image/tiff" => "tiff",
        "image/heic" => "heic",
        "image/avif" => "avif",
        "application/pdf" => "pdf",
        "text/plain" => "txt",
        "text/markdown" => "md",
        "text/csv" => "csv",
        "text/html" => "html",
        "application/json" => "json",
        "application/zip" => "zip",
        "application/gzip" => "gz",
        "application/x-tar" => "tar",
        "video/mp4" => "mp4",
        "video/quicktime" => "mov",
        "video/webm" => "webm",
        _ => return None,
    })
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

/// The texts of an item that can link a file, in the order they were written: its `body`,
/// then its `comments` oldest first, the claim markers left out — a marker is the lock's
/// bookkeeping, never something a person wrote.
pub(crate) fn linked_texts<'a>(body: &'a str, comments: &'a [IssueComment]) -> Vec<&'a str> {
    let mut thread: Vec<&IssueComment> = comments.iter().filter(|c| !is_claim(&c.body)).collect();
    thread.sort_by_key(|c| (c.created_at, c.id));
    std::iter::once(body)
        .chain(thread.into_iter().map(|c| c.body.as_str()))
        .collect()
}

/// Every file `is_github` takes that `texts` embed or link, as its name and its url, in the
/// order of the texts and, within one, as written: each url once.
pub(crate) fn files_linked_by(
    texts: &[&str],
    is_github: &dyn Fn(&str) -> bool,
) -> Vec<(String, String)> {
    let mut files: Vec<(String, String)> = Vec::new();
    for text in texts {
        for (name, url) in linked_files(text, is_github) {
            if !files.iter().any(|(_, seen)| *seen == url) {
                files.push((name, url));
            }
        }
    }
    files
}

/// `url` as a log line may carry it: without its query and fragment, which a private image's
/// signed url keeps its credential in.
fn loggable(url: &str) -> &str {
    url.split(['?', '#']).next().unwrap_or_default()
}

/// Download every file GitHub hosts that `texts` link ([`files_linked_by`]) into a fresh
/// `dir`, each through `client` under the call's deadline, and return what landed: the name,
/// the url and the path of each. A file that cannot be downloaded or written is said, with
/// `item`, its url and why, and left out: the brief keeps its GitHub url, which the skill's
/// `fetch_attachment.py` fetches. A failure never fails the claim.
pub(crate) fn fetch_files(
    client: &dyn GithubClient,
    item: &str,
    texts: &[&str],
    dir: &Path,
    diag: &dyn Diag,
) -> Vec<FetchedFile> {
    let links = files_linked_by(texts, &|url| client.is_github_file(url));
    // Whatever a crashed child left under this key is not this claim's.
    let _ = std::fs::remove_dir_all(dir);
    if links.is_empty() {
        return Vec::new();
    }
    if let Err(e) = std::fs::create_dir_all(dir) {
        diag.err(&format!(
            "{item}: no file it links is downloaded, {} cannot be made — {e}",
            dir.display()
        ));
        return Vec::new();
    }
    let mut fetched: Vec<FetchedFile> = Vec::new();
    for (text, url) in links {
        let downloaded = client.download(&url).map_err(|e| e.to_string());
        let written = downloaded.and_then(|download| {
            let names: Vec<String> = fetched.iter().map(|file| file.name.clone()).collect();
            let name = task_file_name(&text, &url, &names, extension_hint(&download).as_deref());
            let path = dir.join(&name);
            write_whole(&path, &download.bytes, 0o600)
                .map(|()| FetchedFile {
                    name,
                    url: url.clone(),
                    path: path.clone(),
                })
                .map_err(|e| format!("{} cannot be written — {e}", path.display()))
        });
        match written {
            Ok(file) => fetched.push(file),
            Err(e) => diag.err(&format!(
                "{item}: the file at {} is not downloaded, and stays a GitHub link in the \
                 brief for the skill's fetch_attachment.py — {e}",
                loggable(&url)
            )),
        }
    }
    fetched
}

/// The brief `brief` with each link to a file in `fetched` written as its name
/// ([`relink`]), and, when `fetched` holds a file the brief does not link — one only the
/// item's description or comments link, which the brief does not carry — a closing section
/// that links each of those by its relative name.
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
        .map(|link| link.url)
        .collect();
    let elsewhere: Vec<&FetchedFile> = fetched
        .iter()
        .filter(|file| !carried.contains(&file.url))
        .collect();
    if elsewhere.is_empty() {
        return out;
    }
    out.truncate(out.trim_end_matches('\n').len());
    out.push_str(
        "\n\n## Files linked elsewhere on the item\n\nThese are linked from the item's \
         description or its comments, which this brief does not carry, and are downloaded \
         beside it:\n\n",
    );
    for file in elsewhere {
        out.push_str(&format!(
            "- [{}](<{}>)\n",
            escape_link_text(&file.name),
            file.name
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claim::claim_text;
    use crate::client::{MockClient, User};
    use crate::common::{CaptureDiag, TempDir};
    use std::time::{Duration, UNIX_EPOCH};

    /// Whether a url is one of the tests' GitHub files.
    fn github(url: &str) -> bool {
        url.starts_with("https://github.com/user-attachments/")
    }

    const UUID: &str = "0f6c1d2e-3b4a-4c5d-8e9f-a0b1c2d3e4f5";

    fn asset(n: u32) -> String {
        format!("https://github.com/user-attachments/assets/{UUID}-{n}")
    }

    /// The reader finds each GitHub-hosted link in the order written, and only those: two on
    /// one line, a `<url>` destination, brackets nested in the text, an escaped bracket, a
    /// title, an image inside a link. A lookalike host, plain http, a link elsewhere, a bare
    /// url, an escaped bracket, a bracket with no destination, text broken by a paragraph
    /// and one that never closes are no GitHub file.
    #[test]
    fn linked_files_reads_every_github_link_and_no_other() {
        let u = |n: u32| format!("https://github.com/user-attachments/files/{n}/x");
        let markdown = format!(
            "Before ![a.png]({}) and [b.log]({}) on one line.\n\n\
             ![修复 截图](<{}>)\n\
             [a [nested] name]({} \"a title\")\n\
             [esc \\] aped]({} 'single')\n\
             [![badge]({})]({})\n\
             ![evil](https://github.com.evil.example/user-attachments/files/9/x)\n\
             ![plain](http://github.com/user-attachments/files/9/x)\n\
             [elsewhere](https://example.com/x.png)\n\
             bare https://github.com/user-attachments/files/9/bare\n\
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
        let found = linked_files(&markdown, &github);
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

    /// GitHub's editor pastes an image as an `<img>` tag: its `src` is the file and its
    /// `alt` the name, quoted either way, in any case, in any attribute order, over a line
    /// break, its entities undone. A tag with no `src`, an unquoted one, one for another
    /// host, one that never closes and another element are no file; the first `src` of a
    /// tag is its own.
    #[test]
    fn linked_files_reads_the_img_tags_github_pastes() {
        let html = format!(
            "<p>See <img width=\"1280\" height=\"720\" alt=\"Image\" src=\"{a}\" /></p>\n\
             <IMG SRC='{b}' ALT='a &amp; b &quot;c&quot;'>\n\
             <img\n  src=\"{c}&amp;x=1\"\n  alt=\"multi\nline\"\n>\n\
             <img alt=\"no src\">\n\
             <img src={d}>\n\
             <img src=\"https://example.com/x.png\" alt=\"elsewhere\">\n\
             <imgx src=\"{e}\">\n\
             <image src=\"{e}\">\n\
             <img src=\"{f}\" src=\"{e}\" alt=\"first\" alt=\"second\">\n\
             <img src=\"{g}\"\n\
             ![md]({h})",
            a = asset(1),
            b = asset(2),
            c = asset(3),
            d = asset(4),
            e = asset(5),
            f = asset(6),
            g = asset(7),
            h = asset(8),
        );
        assert_eq!(
            linked_files(&html, &github),
            [
                ("Image".to_string(), asset(1)),
                ("a & b \"c\"".to_string(), asset(2)),
                ("multi\nline".to_string(), format!("{}&x=1", asset(3))),
                ("first".to_string(), asset(6)),
                // The tag that never closes is none, and the markdown after it is read.
                ("md".to_string(), asset(8)),
            ]
        );
    }

    /// A megabyte of links that never close — an unclosed destination, an unclosed `<url>`,
    /// an unclosed title, an unclosed tag or quote — is read in one pass, not a pass of the
    /// whole text per bracket. The bounded read takes milliseconds where a scan to the end
    /// per bracket takes minutes, so the bound cannot flake.
    #[test]
    fn links_that_never_close_are_read_in_linear_time() {
        let padded = format!("<img a=\"{}", "x".repeat(5000));
        let spaced = format!("<img{}", " ".repeat(5000));
        for unit in [
            "[a](",
            "[a](<",
            "[a](u (",
            "[a](u \"",
            "[a](u '",
            "[a](()",
            "![a](",
            "<img ",
            "<img src=\"",
            "<img src='",
            "<img a=\"'",
            "<img a='\"",
            "<img src=\"x\" ",
            padded.as_str(),
            spaced.as_str(),
        ] {
            let markdown = unit.repeat(1024 * 1024 / unit.len());
            let started = std::time::Instant::now();
            assert!(
                linked_files(&markdown, &|_: &str| true).is_empty(),
                "{:.20}",
                unit
            );
            let took = started.elapsed();
            assert!(took < Duration::from_secs(5), "{:.20}: {took:?}", unit);
        }
    }

    /// A relink writes `<name>` over the destination of each link whose url is mapped, in
    /// every form a destination takes — bare, `<url>`, before a title, an image inside a
    /// link — and leaves each other byte as it was written: a url not in the map, an
    /// external link, a bare url, the titles and the text.
    #[test]
    fn relink_rewrites_each_mapped_destination_and_nothing_else() {
        let (a, b) = (
            "https://github.com/user-attachments/files/1/a",
            "https://github.com/user-attachments/files/2/b",
        );
        let to = |url: &str| match url {
            "https://github.com/user-attachments/files/1/a" => Some("a 修复.png".to_string()),
            "https://github.com/user-attachments/files/2/b" => Some("b.log".to_string()),
            _ => None,
        };
        let markdown = format!(
            "![a]({a}) and [b](<{b}>)\n\
             [c]({a} \"a title\")\n\
             [d](<{b}> 'single' )\n\
             [![badge]({a})]({b} (paren))\n\
             [other](https://github.com/user-attachments/files/3/c) [ext](https://example.com/x.png)\n\
             bare {a}, [no destination] ({a})\n\
             修复 [esc \\] aped]({a}) end"
        );
        assert_eq!(
            relink(&markdown, &to),
            "![a](<a 修复.png>) and [b](<b.log>)\n\
             [c](<a 修复.png> \"a title\")\n\
             [d](<b.log> 'single' )\n\
             [![badge](<a 修复.png>)](<b.log> (paren))\n\
             [other](https://github.com/user-attachments/files/3/c) [ext](https://example.com/x.png)\n\
             bare https://github.com/user-attachments/files/1/a, [no destination] (https://github.com/user-attachments/files/1/a)\n\
             修复 [esc \\] aped](<a 修复.png>) end"
        );
        assert_eq!(relink(&markdown, &|_: &str| None), markdown);
        assert_eq!(relink("", &to), "");
        assert_eq!(relink("no links 修复", &to), "no links 修复");
    }

    /// An `<img>`'s `src` is rewritten inside its own quotes, whichever they are, and the
    /// name goes in as an attribute value: `&` and both quotes escaped. The rest of the tag,
    /// an unmapped tag and a markdown link beside it are untouched.
    #[test]
    fn relink_rewrites_an_img_src_as_an_attribute_value() {
        let to = |url: &str| match url {
            u if u == asset(1) => Some("Image.png".to_string()),
            u if u == asset(2) => Some("a&b \"c\" 'd'.png".to_string()),
            u if u == asset(3) => Some("doc.pdf".to_string()),
            _ => None,
        };
        let html = format!(
            "<img width=\"1\" alt=\"Image\" src=\"{a}\" />\n\
             <IMG ALT='x' SRC='{b}'>\n\
             <img src=\"{z}\">\n\
             [doc]({c})",
            a = asset(1),
            b = asset(2),
            c = asset(3),
            z = asset(9),
        );
        assert_eq!(
            relink(&html, &to),
            format!(
                "<img width=\"1\" alt=\"Image\" src=\"Image.png\" />\n\
                 <IMG ALT='x' SRC='a&amp;b &quot;c&quot; &#39;d&#39;.png'>\n\
                 <img src=\"{z}\">\n\
                 [doc](<doc.pdf>)",
                z = asset(9)
            )
        );
    }

    /// A `<url>` destination that itself holds a whole link is one destination: the relink
    /// writes the outer one's name and leaves the link inside it out, never a corrupt mix.
    #[test]
    fn relink_leaves_a_destination_inside_another_alone() {
        let markdown = "[a](<x [b](https://github.com/user-attachments/files/1/a)>) tail";
        assert_eq!(
            relink(markdown, &|_: &str| Some("n".to_string())),
            "[a](<n>) tail"
        );
    }

    /// A name is one plain file name: the link's text with `/ \ < >`, control characters and
    /// a leading `.` written `_`, the url's last segment when the text is empty, cut at a
    /// char boundary with its extension kept, never `README.md` in any case, and a duplicate
    /// numbered from 2 before its extension, whatever its case.
    #[test]
    fn a_task_file_name_is_one_safe_name_and_never_a_duplicate() {
        let url = "https://github.com/user-attachments/files/1/asset.bin";
        let name = |text: &str, taken: &[&str]| {
            let taken: Vec<String> = taken.iter().map(|t| t.to_string()).collect();
            task_file_name(text, url, &taken, None)
        };
        assert_eq!(name("shot — 修复.png", &[]), "shot — 修复.png");
        assert_eq!(name("a/b\\c<d>e\nf\u{7}g.png", &[]), "a_b_c_d_e_f_g.png");
        assert_eq!(name("../../etc/passwd", &[]), "_._.._etc_passwd");
        assert_eq!(name(".env", &[]), "_env");
        assert_eq!(name("  report.pdf  ", &[]), "report.pdf");
        assert_eq!(name("", &[]), "asset.bin");
        assert_eq!(
            task_file_name(
                "",
                "https://github.com/user-attachments/files/1/rep%20ort.pdf?sig=x#y",
                &[],
                None
            ),
            "rep%20ort.pdf"
        );
        assert_eq!(task_file_name("", "", &[], None), "file");

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

    /// A name with no extension — a pasted image's alt text, a bare uuid — gets the one the
    /// response named, and one that has an extension keeps its own; the hint is no way to
    /// smuggle a path or a second extension in, and a duplicate is numbered before it.
    #[test]
    fn a_name_with_no_extension_takes_the_responses() {
        let url = format!("https://github.com/user-attachments/assets/{UUID}");
        let name = |text: &str, hint: Option<&str>, taken: &[&str]| {
            let taken: Vec<String> = taken.iter().map(|t| t.to_string()).collect();
            task_file_name(text, &url, &taken, hint)
        };
        assert_eq!(name("Image", Some("png"), &[]), "Image.png");
        assert_eq!(name("", Some("png"), &[]), format!("{UUID}.png"));
        assert_eq!(name("Image", None, &[]), "Image");
        assert_eq!(name("shot.PNG", Some("jpg"), &[]), "shot.PNG");
        assert_eq!(name("Image", Some("png"), &["Image.png"]), "Image-2.png");
        assert_eq!(name("Image.", Some("png"), &[]), "Image.png");
        assert_eq!(
            name("Screenshot at 10.30.45 AM", Some("png"), &[]),
            "Screenshot at 10.30.45 AM.png"
        );
        assert_eq!(name("Image", Some("../x"), &[]), "Image");
        assert_eq!(name("Image", Some("p ng"), &[]), "Image");
        assert_eq!(name("Image", Some(""), &[]), "Image");
        assert_eq!(name("README", Some("md"), &[]), "README-2.md");
    }

    fn download(content_type: Option<&str>, disposition: Option<&str>) -> Download {
        Download {
            bytes: Vec::new(),
            content_type: content_type.map(str::to_string),
            content_disposition: disposition.map(str::to_string),
        }
    }

    /// The hint is the extension of the disposition's `filename` (the `filename*` form
    /// first, percent decoded), else the one the content type is known by, parameters and
    /// case ignored; neither, or a filename with none, is no hint.
    #[test]
    fn the_extension_hint_comes_from_the_disposition_then_the_type() {
        for (content_type, disposition, want) in [
            (None, Some("attachment; filename=\"Report.PDF\""), Some("pdf")),
            (None, Some("inline; filename=plain.png"), Some("png")),
            (
                None,
                Some("attachment; filename=\"fallback.txt\"; filename*=UTF-8''%E4%BF%AE%E5%A4%8D.log"),
                Some("log"),
            ),
            (
                Some("image/png"),
                Some("attachment; filename=\"clip.mov\""),
                Some("mov"),
            ),
            (Some("image/png"), Some("attachment; filename=\"noext\""), Some("png")),
            (Some("image/png"), Some("attachment"), Some("png")),
            (Some("Image/JPEG; charset=binary"), None, Some("jpg")),
            (Some("application/pdf"), None, Some("pdf")),
            (Some("text/plain; charset=utf-8"), None, Some("txt")),
            (Some("application/octet-stream"), None, None),
            (Some("application/octet-stream"), Some("attachment; filename=\"a.b c\""), None),
            (None, None, None),
        ] {
            assert_eq!(
                extension_hint(&download(content_type, disposition)).as_deref(),
                want,
                "{content_type:?} {disposition:?}"
            );
        }
    }

    fn comment(id: u64, body: &str, secs: u64) -> IssueComment {
        IssueComment {
            id,
            body: body.to_string(),
            user: User {
                login: "alice".into(),
            },
            created_at: UNIX_EPOCH + Duration::from_secs(secs),
            updated_at: UNIX_EPOCH + Duration::from_secs(secs + 1000),
        }
    }

    /// The texts are the body, then the comments oldest first by when each was written (an
    /// edit reorders none), a claim marker left out whoever wrote it, and one url is one
    /// file wherever it is linked.
    #[test]
    fn the_texts_are_the_body_then_the_comments_oldest_first() {
        let comments = [
            comment(3, "third", 300),
            comment(1, &claim_text("me"), 100),
            comment(2, "second", 200),
            comment(4, "  [afkd-claim] owner=rival renewal=2", 250),
            comment(5, "same second, later id", 300),
        ];
        assert_eq!(
            linked_texts("the body", &comments),
            ["the body", "second", "third", "same second, later id"]
        );
        assert_eq!(linked_texts("", &[]), [""]);

        let (a, b) = (asset(1), asset(2));
        let texts = [
            format!("![one]({a})"),
            format!("[two]({b}) and [again]({a})"),
        ];
        let texts: Vec<&str> = texts.iter().map(String::as_str).collect();
        assert_eq!(
            files_linked_by(&texts, &github),
            [("one".to_string(), a), ("two".to_string(), b)]
        );
    }

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\xff\xfe--afkd--\r\n";
    const PDF: &[u8] = b"%PDF-1.7\n\0\xe2\xe3\xcf\xd3\r\n%%EOF";

    /// Each linked file is downloaded into a fresh folder, byte for byte, under its own
    /// name: the pasted image takes its extension from the response, a duplicate is
    /// numbered, `README.md` is never taken, a link elsewhere is left alone — and a file
    /// that cannot be downloaded is said, its url without a signed query, and left out
    /// while the rest land.
    #[test]
    fn fetch_files_downloads_each_file_and_skips_one_that_fails() {
        let client = MockClient::new("me");
        let (img, gone, pdf, readme, twin) = (asset(1), asset(2), asset(3), asset(4), asset(5));
        let pdf_url = "https://github.com/user-attachments/files/1234/report.pdf";
        let signed = "https://private-user-images.githubusercontent.com/1/x.png?jwt=SECRET";
        client.add_file(&img, PNG, Some("image/png"), None);
        client.add_file(
            &pdf,
            PDF,
            Some("application/octet-stream"),
            Some("attachment; filename=\"Final.pdf\""),
        );
        client.add_file(pdf_url, PDF, Some("application/pdf"), None);
        client.add_file(&readme, b"# not the brief", None, None);
        client.add_file(&twin, PNG, Some("image/png"), None);
        // `gone` and `signed` are never seeded: GitHub answers 404.
        let body = format!(
            "<img alt=\"Image\" src=\"{img}\">\n![gone]({gone})\n![x]({signed})\n[elsewhere](https://example.com/a.png)"
        );
        let comment = format!(
            "[report.pdf]({pdf_url}) [Image]({pdf}) [README.md]({readme}) [Image]({twin}) \
             [again]({img})"
        );
        let dir = TempDir::new("fetch");
        let task_files = dir.path().join("task-files").join("k");
        std::fs::create_dir_all(&task_files).unwrap();
        std::fs::write(task_files.join("stale.png"), b"left by a crash").unwrap();
        let diag = CaptureDiag::default();

        let fetched = fetch_files(
            &client,
            "issue \"T\" (acme/widgets#7)",
            &[&body, &comment],
            &task_files,
            &diag,
        );

        let names: Vec<&str> = fetched.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "Image.png",
                "report.pdf",
                "Image.pdf",
                "README-2.md",
                "Image-2.png"
            ]
        );
        let urls: Vec<&str> = fetched.iter().map(|f| f.url.as_str()).collect();
        assert_eq!(urls, [img.as_str(), pdf_url, &pdf, &readme, &twin]);
        for (file, bytes) in fetched.iter().zip([PNG, PDF, PDF, b"# not the brief", PNG]) {
            assert_eq!(file.path, task_files.join(&file.name));
            assert_eq!(std::fs::read(&file.path).unwrap(), bytes, "{}", file.name);
        }
        assert!(
            !task_files.join("stale.png").exists(),
            "the folder is fresh"
        );
        let mut on_disk: Vec<String> = std::fs::read_dir(&task_files)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        on_disk.sort();
        let mut want: Vec<String> = names.iter().map(|n| n.to_string()).collect();
        want.sort();
        assert_eq!(on_disk, want, "no temp file is left");
        assert_eq!(
            client.downloads().len(),
            7,
            "each GitHub url once, the link elsewhere never: {:?}",
            client.downloads()
        );
        assert_eq!(
            diag.lines(),
            [
                format!(
                    "issue \"T\" (acme/widgets#7): the file at {gone} is not downloaded, and stays \
                     a GitHub link in the brief for the skill's fetch_attachment.py — github \
                     fetch file: forge returned status 404"
                ),
                "issue \"T\" (acme/widgets#7): the file at \
                 https://private-user-images.githubusercontent.com/1/x.png is not downloaded, and \
                 stays a GitHub link in the brief for the skill's fetch_attachment.py — github \
                 fetch file: forge returned status 404"
                    .to_string(),
            ]
        );
    }

    /// A transport failure is the same one line and not a failure of the claim, and an item
    /// that links nothing leaves no folder at all, whatever was there.
    #[test]
    fn fetch_files_says_a_transport_failure_and_makes_no_folder_for_no_link() {
        let client = MockClient::new("me");
        client.add_file(&asset(1), PNG, Some("image/png"), None);
        client.fail("fetch file");
        let dir = TempDir::new("fetch-down");
        let task_files = dir.path().join("k");
        let diag = CaptureDiag::default();
        let text = format!("![a]({})", asset(1));
        assert!(fetch_files(&client, "issue", &[&text], &task_files, &diag).is_empty());
        assert_eq!(
            diag.lines(),
            [format!(
                "issue: the file at {} is not downloaded, and stays a GitHub link in the brief \
                 for the skill's fetch_attachment.py — github fetch file: no response (mock \
                 failure)",
                asset(1)
            )]
        );

        std::fs::create_dir_all(&task_files).unwrap();
        std::fs::write(task_files.join("stale.png"), b"x").unwrap();
        assert!(fetch_files(&client, "issue", &["no links"], &task_files, &diag).is_empty());
        assert!(!task_files.exists());
        assert_eq!(diag.lines().len(), 1, "no link, no line");
    }

    /// A folder that cannot be made is one line and no file; the claim goes on.
    #[test]
    fn fetch_files_says_a_folder_it_cannot_make() {
        let client = MockClient::new("me");
        client.add_file(&asset(1), PNG, Some("image/png"), None);
        let dir = TempDir::new("fetch-blocked");
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"a file, not a folder").unwrap();
        let task_files = blocker.join("k");
        let diag = CaptureDiag::default();
        let text = format!("![a]({})", asset(1));
        assert!(fetch_files(&client, "issue", &[&text], &task_files, &diag).is_empty());
        let lines = diag.lines();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].starts_with(&format!(
                "issue: no file it links is downloaded, {} cannot be made — ",
                task_files.display()
            )),
            "{}",
            lines[0]
        );
        assert!(client.downloads().is_empty());
    }

    fn fetched(name: &str, url: &str) -> FetchedFile {
        FetchedFile {
            name: name.to_string(),
            url: url.to_string(),
            path: PathBuf::from("/state/task-files/k").join(name),
        }
    }

    /// The brief points each file it links at its relative name, and a file only the
    /// comments link is listed in a closing section by its relative name too, in the order
    /// the files were fetched; a brief that carries every file gets no section, and a
    /// brief that carries none is all section.
    #[test]
    fn a_brief_links_the_files_it_carries_and_closes_with_the_rest() {
        let (shot, report, log) = (asset(1), asset(2), asset(3));
        let files = [
            fetched("Image.png", &shot),
            fetched("report [final].pdf", &report),
            fetched("run.log", &log),
        ];
        let brief = format!("Title\n\nSee <img alt=\"Image\" src=\"{shot}\"> and ![a]({shot}).");
        assert_eq!(
            brief_with_files(&brief, &files),
            "Title\n\nSee <img alt=\"Image\" src=\"Image.png\"> and ![a](<Image.png>).\n\n\
             ## Files linked elsewhere on the item\n\n\
             These are linked from the item's description or its comments, which this brief \
             does not carry, and are downloaded beside it:\n\n\
             - [report \\[final\\].pdf](<report [final].pdf>)\n\
             - [run.log](<run.log>)\n"
        );

        // A brief that ends in a newline, as a pull request's does, joins the section after
        // one blank line, not two.
        let pr = format!("Address review feedback.\n\n**alice:** [r]({report})\n");
        assert_eq!(
            brief_with_files(&pr, &files),
            "Address review feedback.\n\n**alice:** [r](<report [final].pdf>)\n\n\
             ## Files linked elsewhere on the item\n\n\
             These are linked from the item's description or its comments, which this brief \
             does not carry, and are downloaded beside it:\n\n\
             - [Image.png](<Image.png>)\n\
             - [run.log](<run.log>)\n"
        );

        let all = format!("![a]({shot}) [b]({report}) [c]({log})");
        assert_eq!(
            brief_with_files(&all, &files),
            "![a](<Image.png>) [b](<report [final].pdf>) [c](<run.log>)"
        );
        assert_eq!(brief_with_files("Just a title", &[]), "Just a title");
        let none = brief_with_files("Just a title", &files[..1]);
        assert!(
            none.starts_with("Just a title\n\n## Files linked elsewhere on the item"),
            "{none}"
        );
        assert!(!none.contains("github.com"), "{none}");
    }
}
