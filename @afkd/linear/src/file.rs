//! Files on an issue, both ways: the files `update_issue` and `create_comment` upload and
//! embed, and the files an issue links ([`linked_issue_files`]), which the workspace read
//! `linear.files` hands back as urls.
//!
//! Linear stores a file in two steps: `fileUpload` signs an upload url and names the asset
//! url the file will live at, and the bytes are PUT to the signed url. The file is then
//! nothing but a markdown link to its asset url — an image embedded `![name](url)`, any
//! other file linked `[name](url)` — in a comment or a description ([`embed`]). So a read of
//! an issue's files is a read of the links its description and thread carry
//! ([`linked_files`]), each fetched with the credential the GraphQL API takes.
//!
//! An upload reads its file when the slot calls, before any request, and the bytes ride the
//! [`Upload`] from then on.

use std::fmt;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use crate::issue::is_control_marker;
use crate::tracker::{LinearError, Tracker};

/// The most one file may hold, either way: what `@afkd/trello` takes, and ureq's own cap on
/// a body.
pub(crate) const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;

/// The longest link text [`linked_files`] reads before it gives the bracket up: what keeps a
/// description of a thousand unclosed `[` from costing a scan per bracket of the whole text.
const MAX_LINK_TEXT: usize = 1024;

/// The most bytes [`linked_files`] reads after a link's `](` for its destination, its title
/// and the `)` that closes it, before it gives the link up: what bounds the scan one bracket
/// costs however its destination runs, so no text costs a scan per bracket of the whole of
/// it. A Linear asset url is a tenth of it, so a longer destination is no Linear file.
const MAX_LINK_DESTINATION: usize = 2048;

/// The most parentheses a bare link destination nests before [`linked_files`] gives the link
/// up, a limit CommonMark leaves to the reader. A run of `[a](` that never closes then costs
/// each bracket a few dozen bytes of scan, not [`MAX_LINK_DESTINATION`].
const MAX_LINK_PARENS: usize = 32;

/// A file to put on an issue: the name it is linked under, its content type, and its bytes
/// as they were read.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Upload {
    /// The file's name, the last part of the path the slot gave: what Linear stores and
    /// the link's text.
    pub(crate) name: String,
    pub(crate) content_type: String,
    pub(crate) bytes: Vec<u8>,
}

/// The name, the type and the length, never the bytes: an upload can be a whole screenshot,
/// and a log line may print it.
impl fmt::Debug for Upload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Upload")
            .field("name", &self.name)
            .field("content_type", &self.content_type)
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

/// An upload of `name` as [`read_upload`] reads one, its bytes those of a binary file: a PNG
/// signature, a NUL, a CR LF, an `0xFF` and a `--` run, then the name.
#[cfg(test)]
pub(crate) fn upload(name: &str) -> Upload {
    let mut bytes = b"\x89PNG\r\n\x1a\n\0\xff--afkd--\r\n".to_vec();
    bytes.extend_from_slice(name.as_bytes());
    Upload {
        name: name.to_string(),
        content_type: content_type_of(name).to_string(),
        bytes,
    }
}

/// Read the file at `path` for `action` to put up, or the sentence that refuses it: a
/// relative path, one that names no file, a path with nothing there or no regular file, one
/// that cannot be read, or a file over [`MAX_FILE_BYTES`].
pub(crate) fn read_upload(action: &str, path: &str) -> Result<Upload, String> {
    let at = Path::new(path);
    if !at.is_absolute() {
        return Err(format!("{action}: files must be absolute, not {path:?}"));
    }
    let Some(name) = at.file_name() else {
        return Err(format!("{action}: {path} names no file"));
    };
    let name = name.to_string_lossy().into_owned();
    let cannot = |e: std::io::Error| format!("{action}: cannot read {path}: {e}");
    let file = File::open(at).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => format!("{action}: no file at {path}"),
        _ => cannot(e),
    })?;
    if !file.metadata().map_err(cannot)?.is_file() {
        return Err(format!("{action}: {path} is not a regular file"));
    }
    // Read one byte past the cap rather than trust the size the metadata gave: a file
    // still being written can grow between the two.
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(cannot)?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(format!(
            "{action}: {path} is over the {} MiB a file may hold",
            MAX_FILE_BYTES / (1024 * 1024)
        ));
    }
    Ok(Upload {
        content_type: content_type_of(&name).to_string(),
        name,
        bytes,
    })
}

/// The content type of a file named `name`, by its extension in any case: the image types
/// Linear shows inline, the text a run writes, and `application/octet-stream` for the rest.
pub(crate) fn content_type_of(name: &str) -> &'static str {
    let extension = match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => extension.to_ascii_lowercase(),
        _ => String::new(),
    };
    match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "txt" | "log" => "text/plain",
        "md" => "text/markdown",
        "json" => "application/json",
        "patch" | "diff" => "text/x-diff",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
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

/// The markdown a comment or a description is sent as: `text` verbatim, then each upload on
/// a paragraph of its own at the url beside it — an image embedded, any other file linked.
/// With no text, the files alone.
pub(crate) fn embed(text: &str, uploads: &[Upload], urls: &[String]) -> String {
    let links = uploads.iter().zip(urls).map(|(upload, url)| {
        let bang = if upload.content_type.starts_with("image/") {
            "!"
        } else {
            ""
        };
        format!("{bang}[{}]({url})", escape_link_text(&upload.name))
    });
    let parts: Vec<String> = (!text.is_empty())
        .then(|| text.to_string())
        .into_iter()
        .chain(links)
        .collect();
    parts.join("\n\n")
}

/// Put each upload up, and return the markdown `text` is sent as with every file in it
/// ([`embed`]). An upload that fails is the caller's to refuse.
pub(crate) fn embedded(
    tracker: &dyn Tracker,
    text: &str,
    uploads: &[Upload],
) -> Result<String, LinearError> {
    let urls = uploads
        .iter()
        .map(|upload| tracker.upload_file(upload))
        .collect::<Result<Vec<_>, LinearError>>()?;
    Ok(embed(text, uploads, &urls))
}

/// Every link and image in `markdown` whose url `is_linear` takes, in the order written, as
/// its text unescaped and its url: `[t](url)`, `![t](url)`, a `<url>` destination, and a
/// destination followed by a title. A bare url is no link, an escaped `\[` opens none, and a
/// bracket whose text runs past a paragraph or [`MAX_LINK_TEXT`], or whose destination and
/// title run past [`MAX_LINK_DESTINATION`], is no link either. Nor, as CommonMark reads them,
/// is a `<url>` holding a `<`, a `(title)` holding a `(`, or a url nesting more than
/// [`MAX_LINK_PARENS`] parentheses. A link inside another's text — an image in a link — is
/// found too.
pub(crate) fn linked_files(
    markdown: &str,
    is_linear: &dyn Fn(&str) -> bool,
) -> Vec<(String, String)> {
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
        if let Some((text, url)) = link_at(markdown, start) {
            if is_linear(&url) {
                found.push((text, url));
            }
        }
        from = start + 1;
    }
    found
}

/// The link whose text opens at the `[` at byte `start` of `md`: its text with escapes
/// undone, and its url.
fn link_at(md: &str, start: usize) -> Option<(String, String)> {
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
    let mut cap = rest.len().min(MAX_LINK_DESTINATION);
    while !rest.is_char_boundary(cap) {
        cap -= 1;
    }
    let rest = &rest[..cap];
    // Stop each scan where CommonMark ends the part it reads: a `<url>` at a second `<`, a
    // bare url past `MAX_LINK_PARENS` open parentheses, a `(title)` at a second `(`. So the
    // scan from one bracket never runs on through the brackets after it.
    let (url, after) = match rest.strip_prefix('<') {
        Some(inner) => {
            let end = inner.find(['>', '<', '\n'])?;
            if !inner[end..].starts_with('>') {
                return None;
            }
            (&inner[..end], &inner[end + 1..])
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
            (&rest[..end], &rest[end..])
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
    after.starts_with(')').then(|| (text, url.to_string()))
}

/// Every file Linear hosts that the issue `issue_id` embeds or links, as its name and its url:
/// in its description, then in its thread oldest first, control markers left out, each url
/// once.
///
/// # Errors
///
/// Returns the sentence of the read of the description or the thread Linear refused.
pub(crate) fn linked_issue_files(
    tracker: &dyn Tracker,
    issue_id: &str,
) -> Result<Vec<(String, String)>, String> {
    let description = tracker
        .issue_description(issue_id)
        .map_err(|e| e.to_string())?;
    let mut thread = tracker
        .issue_comments(issue_id)
        .map_err(|e| e.to_string())?;
    thread.retain(|c| !is_control_marker(&c.text));
    thread.sort_by(|a, b| (a.posted_at, &a.id).cmp(&(b.posted_at, &b.id)));
    let is_linear = |url: &str| tracker.is_linear_file(url);
    let mut files: Vec<(String, String)> = Vec::new();
    for text in std::iter::once(&description).chain(thread.iter().map(|c| &c.text)) {
        for (name, url) in linked_files(text, &is_linear) {
            if !files.iter().any(|(_, seen)| *seen == url) {
                files.push((name, url));
            }
        }
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::TempDir;
    use crate::tracker::MOCK_UPLOADS;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;

    /// Whether a url is the mock's.
    fn mock_file(url: &str) -> bool {
        url.starts_with(MOCK_UPLOADS)
    }

    /// A file is read whole, named by its path's last part, its bytes as they are on disk —
    /// a NUL, a CR LF and invalid UTF-8 among them — and its type by its extension.
    #[test]
    fn an_upload_is_read_whole_and_typed_by_its_name() {
        let dir = TempDir::new("upload");
        let path = dir.path().join("screen shot — 修复.PNG");
        let bytes = b"\x89PNG\r\n\x1a\n\0\xff\xfe--afkd--\r\n".to_vec();
        std::fs::write(&path, &bytes).unwrap();
        let upload = read_upload("create_comment", &path.display().to_string()).unwrap();
        assert_eq!(upload.name, "screen shot — 修复.PNG");
        assert_eq!(upload.content_type, "image/png");
        assert_eq!(upload.bytes, bytes);
    }

    /// Each path an upload cannot be read from is refused in the action's words: relative,
    /// naming no file, nothing there, a directory, unreadable, and one byte over the cap.
    #[test]
    fn an_upload_refuses_what_it_cannot_read() {
        let dir = TempDir::new("refuse");
        let at = |name: &str| dir.path().join(name).display().to_string();
        let big = at("big.bin");
        std::fs::write(&big, vec![0u8; MAX_FILE_BYTES as usize + 1]).unwrap();
        let exact = at("exact.bin");
        std::fs::write(&exact, vec![0xffu8; MAX_FILE_BYTES as usize]).unwrap();
        for (path, refusal) in [
            (
                "shot.png".to_string(),
                "create_comment: files must be absolute, not \"shot.png\"".to_string(),
            ),
            (
                "/".to_string(),
                "create_comment: / names no file".to_string(),
            ),
            (
                at("gone.png"),
                format!("create_comment: no file at {}", at("gone.png")),
            ),
            (
                dir.path().display().to_string(),
                format!(
                    "create_comment: {} is not a regular file",
                    dir.path().display()
                ),
            ),
            (
                big.clone(),
                format!("create_comment: {big} is over the 10 MiB a file may hold"),
            ),
        ] {
            assert_eq!(read_upload("create_comment", &path).map(drop), Err(refusal));
        }
        assert_eq!(
            read_upload("update_issue", &exact).unwrap().bytes.len() as u64,
            MAX_FILE_BYTES,
            "the cap itself is taken"
        );
        let locked = at("locked.png");
        std::fs::write(&locked, b"x").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        if File::open(&locked).is_err() {
            assert_eq!(
                read_upload("update_issue", &locked).map(drop),
                Err(format!(
                    "update_issue: cannot read {locked}: Permission denied (os error 13)"
                ))
            );
        }
    }

    /// The type comes from the extension in any case; a dotfile, no extension and an
    /// unknown one are bytes.
    #[test]
    fn a_content_type_comes_from_the_extension() {
        for (name, content_type) in [
            ("shot.png", "image/png"),
            ("SHOT.PNG", "image/png"),
            ("photo.JPG", "image/jpeg"),
            ("photo.jpeg", "image/jpeg"),
            ("anim.gif", "image/gif"),
            ("修复.webp", "image/webp"),
            ("diagram.svg", "image/svg+xml"),
            ("run.log", "text/plain"),
            ("notes.txt", "text/plain"),
            ("plan.md", "text/markdown"),
            ("trace.json", "application/json"),
            ("fix.patch", "text/x-diff"),
            ("fix.diff", "text/x-diff"),
            ("report.pdf", "application/pdf"),
            ("archive.tar.gz", "application/octet-stream"),
            ("Makefile", "application/octet-stream"),
            (".png", "application/octet-stream"),
            ("", "application/octet-stream"),
        ] {
            assert_eq!(content_type_of(name), content_type, "{name}");
        }
    }

    /// The text goes out verbatim and each file on a paragraph after it, an image embedded
    /// and anything else linked, a name's brackets and backslashes escaped; with no text,
    /// the files alone, and with no file, the text alone.
    #[test]
    fn embed_puts_each_file_after_the_text() {
        let files = [upload("screen shot — 修复.png"), upload(r"run [1]\2.log")];
        let urls = [
            "https://uploads.linear.app/o/a/1".to_string(),
            "https://uploads.linear.app/o/b/2".to_string(),
        ];
        assert_eq!(
            embed("## 背景\n\nBefore:", &files, &urls),
            "## 背景\n\nBefore:\n\n![screen shot — 修复.png](https://uploads.linear.app/o/a/1)\
             \n\n[run \\[1\\]\\\\2.log](https://uploads.linear.app/o/b/2)"
        );
        assert_eq!(
            embed("", &files[..1], &urls[..1]),
            "![screen shot — 修复.png](https://uploads.linear.app/o/a/1)"
        );
        assert_eq!(embed("Just text ✅\n", &[], &[]), "Just text ✅\n");
        assert_eq!(embed("", &[], &[]), "");
    }

    /// The reader finds each Linear-hosted link in the order written, and only those: two on
    /// one line, a `<url>` destination, brackets nested in the text, an escaped bracket, a
    /// title, an image inside a link. A lookalike host, plain http, a link elsewhere, a bare
    /// url, an escaped bracket, a bracket with no destination, text broken by a paragraph
    /// and one that never closes are no Linear file.
    #[test]
    fn linked_files_reads_every_linear_link_and_no_other() {
        let u = |n: u32| format!("https://uploads.linear.app/o/{n}/x");
        let markdown = format!(
            "Before ![a.png]({}) and [b.log]({}) on one line.\n\n\
             ![修复 截图](<{}>)\n\
             [a [nested] name]({} \"a title\")\n\
             [esc \\] aped]({} 'single')\n\
             [![badge]({})]({})\n\
             ![evil](https://uploads.linear.app.evil.example/o/9/x)\n\
             ![plain](http://uploads.linear.app/o/9/x)\n\
             [elsewhere](https://example.com/x.png)\n\
             bare https://uploads.linear.app/o/9/bare\n\
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
        let found = linked_files(&markdown, &|url: &str| {
            url.starts_with("https://uploads.linear.app/")
        });
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
            assert!(took < std::time::Duration::from_secs(5), "{unit}: {took:?}");
        }
    }

    /// The writer and the reader of the embed format agree: what `embed` writes for any
    /// names — brackets, backslashes, wide, an emoji, a leading `!` — `linked_files` reads
    /// back as exactly those names at exactly those urls, in order.
    #[test]
    fn embed_and_linked_files_round_trip() {
        let names = [
            "screen shot — 修复.png",
            r"run [1]\2.log",
            "]].md",
            "!bang.gif",
            "a\\",
            "截图 🚨 (final).PNG",
        ];
        let files: Vec<Upload> = names.iter().map(|n| upload(n)).collect();
        let urls: Vec<String> = (0..names.len())
            .map(|n| format!("{MOCK_UPLOADS}acme/{n}/asset-{n}"))
            .collect();
        for text in ["", "## 背景\n\nSee [the log] below ✅", "trailing [bracket"] {
            let read = linked_files(&embed(text, &files, &urls), &mock_file);
            let want: Vec<(String, String)> = names
                .iter()
                .map(|n| n.to_string())
                .zip(urls.iter().cloned())
                .collect();
            assert_eq!(read, want, "{text:?}");
        }
    }

    /// An upload prints its name, type and length, and never a byte.
    #[test]
    fn an_upload_never_prints_its_bytes() {
        let upload = upload("shot.png");
        let printed = format!("{upload:?}");
        assert_eq!(
            printed,
            format!(
                "Upload {{ name: \"shot.png\", content_type: \"image/png\", bytes: {} }}",
                upload.bytes.len()
            )
        );
    }

    /// The skill's Python, which reads and writes links for `files.py`, `create_issue.py`,
    /// `update_issue.py` and `create_comment.py`, run over `fixture` as JSON on its stdin; its
    /// answers, as JSON.
    fn python_ports(fixture: &serde_json::Value, api_url: &str) -> serde_json::Value {
        const DRIVER: &str = r#"
import json, sys
sys.path.insert(0, sys.argv[1])
import linear_api as linear
f = json.load(sys.stdin)
json.dump({
    "links": [linear.linked_files(md, linear.is_linear_file) for md in f["markdown"]],
    "types": [linear.content_type_of(name) for name in f["types"]],
    "embeds": [
        linear.embed(text, [(n, linear.content_type_of(n), u) for n, u in files])
        for text, files in f["embeds"]
    ],
    "linear": [linear.is_linear_file(url) for url in f["urls"]],
    "markers": [linear.is_control_marker(text) for text in f["markers"]],
}, sys.stdout)
"#;
        use std::io::Write as _;
        let lib = concat!(env!("CARGO_MANIFEST_DIR"), "/skills/linear/lib");
        let mut child = std::process::Command::new("python3")
            .args(["-B", "-c", DRIVER, lib])
            .env("LINEAR_API_URL", api_url)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("run python3");
        child
            .stdin
            .take()
            .expect("piped stdin")
            .write_all(fixture.to_string().as_bytes())
            .expect("write the fixture");
        let out = child.wait_with_output().expect("python3 ends");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).expect("the ports answer JSON")
    }

    /// The skill's Python ports read and write links exactly as this module does, over one
    /// fixture of the inputs that tell two readings apart: wide and four-byte text at each
    /// bound, escapes, nesting, every destination and title form, the whitespace and control
    /// characters Python and Rust class differently, extensions in any case, names a link must
    /// escape, and afkd's markers escaped and indented.
    #[test]
    fn the_skill_ports_read_and_write_links_as_the_plugin_does() {
        use crate::auth::Auth;
        use crate::client::{LinearClient, GRAPHQL_PATH};
        use crate::settings::AuthConfig;
        use serde_json::json;

        let base = "http://127.0.0.1:8080/";
        let up = "https://uploads.linear.app/o/1";
        let client = LinearClient::new(
            base,
            Arc::new(Auth::new(&AuthConfig::ApiKey("lin_api_k".into()))),
        );
        let is_linear = |url: &str| client.is_linear_file(url);
        let text = |c: &str, n: usize| c.repeat(n);
        let markdown = vec![
            format!(
                "## 背景 🚨\n\nBefore: ![shot — 修复.png]({up}/a.png) and \
                 [run.log](<{up}/b log.txt>) — <{up}/bare>"
            ),
            format!("\\[not a link]({up}/c) but \\\\[is one]({up}/d), \\\\\\[not]({up}/x)"),
            format!("[![inner.png]({up}/e)]({up}/f) [a [nested] text]({up}/g)"),
            format!(
                "[a]({up}/h \"title\") [b]({up}/i 'it''s') [c]({up}/j (paren)) \
                 [d]({up}/k (a(b)) [e]({up}/l \"no close) [f]({up}/m x)"
            ),
            format!("[[[[ [a]({up}/n [b](<{up}/o<p>) [c](<{up}/q\n>)"),
            format!("[{}]({up}/r)", text("字", 341)),
            format!("[{}]({up}/s)", text("字", 342)),
            format!(
                "[{}]({up}/t) [{}]({up}/u)",
                text("a", 1024),
                text("a", 1025)
            ),
            format!("[{}]({up}/v)", text("😀", 256)),
            format!("[p]({up}/{}{})", text("(", 32), text(")", 32)),
            format!("[p]({up}/{}{})", text("(", 33), text(")", 33)),
            format!("[q]({up}/1c\u{1c}x) [r]({up}/85\u{85}t) [s]({up}/3000\u{3000}\"t\")"),
            "[x](https://example.com/a.png) [y](http://uploads.linear.app/o/1/a)".to_string(),
            format!("[z](https://uploads.linear.app.evil.example/o) [log]({base}uploads/o/1)"),
            format!("[a\n\nb]({up}/w) [a\nb]({up}/y)"),
            format!("[big]({up}/{})", text("a", 3000)),
            format!("[a\\]b\\\\c\\d\\字]({up}/z)"),
            String::new(),
            text("[](", 500),
        ];
        let types = [
            "shot.PNG",
            "a.JpEg",
            "x.svg",
            "notes.MD",
            ".png",
            "noext",
            "a.Patch",
            "b.diff",
            "c.json",
            "d.pdf",
            "e.log",
            "f.TXT",
            "g.webp",
            "h.gif",
            "archive.tar.gz",
            "x.\u{212a}",
            "ÉCRAN.PNG",
            "trailing.",
        ];
        let embeds: Vec<(&str, Vec<(&str, String)>)> = vec![
            ("After the fix ✅", vec![("shot.png", format!("{up}/a"))]),
            ("", vec![("run [final].log", format!("{up}/b"))]),
            (
                "## 計画\n\ntwo:",
                vec![
                    ("a\\b].png", format!("{up}/c")),
                    ("c.md", format!("{up}/d")),
                ],
            ),
            ("", vec![]),
        ];
        let urls = [
            format!("{up}/a"),
            "https://uploads.linear.app".into(),
            "https://uploads.linear.app.evil.example/o/1/a".into(),
            "http://uploads.linear.app/o/1/a".into(),
            format!("{base}uploads/x"),
            "http://127.0.0.1:8080".into(),
            "http://127.0.0.1:80800/x".into(),
            String::new(),
        ];
        let markers = [
            "[afkd-claim] owner=afkd-17",
            "  \n[afkd-ran] owner=afkd-17 upto=1",
            "\\[afkd-park\\] service=afkd::develop",
            "\u{1c}[afkd-attempt] 1/2",
            "\u{3000}[afkd-attempt] 1/2",
            "seen a stray [afkd-claim] on this issue",
            "\\\\[afkd-claim] owner=x",
            "",
        ];
        let fixture = json!({
            "markdown": markdown,
            "types": types,
            "embeds": embeds,
            "urls": urls,
            "markers": markers,
        });
        let rust = json!({
            "links": markdown.iter().map(|md| linked_files(md, &is_linear)).collect::<Vec<_>>(),
            "types": types.iter().map(|name| content_type_of(name)).collect::<Vec<_>>(),
            "embeds": embeds
                .iter()
                .map(|(text, files)| {
                    let uploads: Vec<Upload> = files.iter().map(|(name, _)| upload(name)).collect();
                    let urls: Vec<String> = files.iter().map(|(_, url)| url.clone()).collect();
                    embed(text, &uploads, &urls)
                })
                .collect::<Vec<_>>(),
            "linear": urls.iter().map(|url| is_linear(url)).collect::<Vec<_>>(),
            "markers": markers.iter().map(|text| is_control_marker(text)).collect::<Vec<_>>(),
        });
        let found: usize = rust["links"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l.as_array().unwrap().len())
            .sum();
        assert!(
            found >= 15,
            "the fixture finds links to compare: {}",
            rust["links"]
        );
        let python = python_ports(&fixture, &format!("{base}{GRAPHQL_PATH}"));
        for key in ["links", "types", "embeds", "linear", "markers"] {
            let (rust, python) = (&rust[key], &python[key]);
            for (n, (r, p)) in rust
                .as_array()
                .unwrap()
                .iter()
                .zip(python.as_array().unwrap())
                .enumerate()
            {
                assert_eq!(r, p, "{key}[{n}]: Rust, then Python");
            }
            assert_eq!(rust, python, "{key}");
        }
    }
}
