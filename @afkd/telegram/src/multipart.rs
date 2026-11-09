//! A `multipart/form-data` body (RFC 7578), built by hand: the one shape the Bot API takes
//! a file upload in. ureq sends bytes, not forms, and a form is three parts here — the
//! chat, a caption, the file — so a dependency would be more than the code.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// A form's file part: the field it is sent under, the file name Telegram shows, and its
/// bytes.
pub(crate) struct FilePart<'a> {
    pub(crate) field: &'a str,
    pub(crate) name: &'a str,
    pub(crate) bytes: &'a [u8],
}

/// The `Content-Type` header and the body of a form of `fields`, each a text part, then
/// `file`. The boundary is drawn until it appears in no part, so no byte of the file can end
/// a part early.
pub(crate) fn form(fields: &[(&str, &str)], file: &FilePart) -> (String, Vec<u8>) {
    let boundary = boundary(|b| {
        let delimiter = format!("--{b}");
        !contains(file.bytes, delimiter.as_bytes())
            && fields
                .iter()
                .all(|(_, v)| !contains(v.as_bytes(), delimiter.as_bytes()))
    });
    let mut body = Vec::with_capacity(file.bytes.len() + 512);
    for (name, value) in fields {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{}\"\r\n\r\n",
                quoted(name)
            )
            .as_bytes(),
        );
        body.extend_from_slice(value.as_bytes());
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{}\"; filename=\"{}\"\r\n\
             Content-Type: application/octet-stream\r\n\r\n",
            quoted(file.field),
            quoted(file.name)
        )
        .as_bytes(),
    );
    body.extend_from_slice(file.bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

/// The first boundary `fits` takes, drawn from the clock and a counter. A boundary is 46
/// characters, so the first draw fits all but a file built to defeat it.
fn boundary(fits: impl Fn(&str) -> bool) -> String {
    static DRAWS: AtomicU64 = AtomicU64::new(0);
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    loop {
        let n = DRAWS.fetch_add(1, Ordering::Relaxed);
        let b = format!(
            "afkd-telegram-{:016x}{:016x}",
            seed ^ n.wrapping_mul(0x9E37_79B9_7F4A_7C15),
            n
        );
        if fits(&b) {
            return b;
        }
    }
}

/// Whether `needle` appears in `haystack`.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// `s` fit for a quoted header parameter: a quote, CR and LF percent-encoded, as browsers
/// send a file name holding one (RFC 7578 §4.2).
fn quoted(s: &str) -> String {
    s.replace('"', "%22")
        .replace('\r', "%0D")
        .replace('\n', "%0A")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::form::parse;

    /// Every byte value, a CR LF, and a line that looks like a multipart delimiter.
    fn awkward_bytes() -> Vec<u8> {
        let mut bytes: Vec<u8> = (0..=255u8).chain(0..=255u8).collect();
        bytes.extend_from_slice(b"\r\n--afkd-telegram-0000000000000000\r\n--\r\n");
        bytes
    }

    #[test]
    fn a_form_parses_back_part_by_part_with_the_exact_bytes() {
        let bytes = awkward_bytes();
        let (content_type, body) = form(
            &[
                ("chat_id", "-1001234567890"),
                ("caption", "デプロイ log — \"full\" ✓"),
            ],
            &FilePart {
                field: "document",
                name: "deploy \"prod\".log",
                bytes: &bytes,
            },
        );
        let parts = parse(&content_type, &body).unwrap();
        assert_eq!(parts.len(), 3);
        assert_eq!(
            (
                parts[0].name.as_str(),
                parts[0].bytes.as_slice(),
                parts[0].filename.as_deref()
            ),
            ("chat_id", b"-1001234567890".as_slice(), None)
        );
        assert_eq!(parts[1].name, "caption");
        assert_eq!(parts[1].bytes, "デプロイ log — \"full\" ✓".as_bytes());
        assert_eq!(parts[2].name, "document");
        assert_eq!(parts[2].filename.as_deref(), Some("deploy %22prod%22.log"));
        assert_eq!(parts[2].bytes, bytes, "the file's bytes, exactly");
    }

    /// The boundary never appears inside the parts, whatever they hold.
    #[test]
    fn the_boundary_is_absent_from_every_part() {
        let bytes = awkward_bytes();
        let (content_type, body) = form(
            &[("chat_id", "42")],
            &FilePart {
                field: "photo",
                name: "a.png",
                bytes: &bytes,
            },
        );
        let boundary = content_type.split("boundary=").nth(1).unwrap();
        assert!(!contains(&bytes, boundary.as_bytes()));
        // The delimiter shows up exactly once per part, and once to close.
        let delimiter = format!("--{boundary}");
        let count = body
            .windows(delimiter.len())
            .filter(|w| *w == delimiter.as_bytes())
            .count();
        assert_eq!(count, 3);
    }

    /// A draw that would collide is passed over.
    #[test]
    fn a_colliding_boundary_is_drawn_again() {
        let first = boundary(|_| true);
        let next = boundary(|b| b != first);
        assert_ne!(first, next);
    }
}
