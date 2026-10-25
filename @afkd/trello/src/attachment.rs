//! Files on a card: the `attach` action's upload, and the `fetch_attachment` action that
//! writes one back to a path.
//!
//! An action answers afkd with nothing but `ok` or the sentence that refuses it, so no
//! action can hand a slot a file's bytes. It does not need to: afkd runs this plugin as an
//! unconfined child on the operator's host, so `fetch_attachment` writes the file at the
//! path the slot names itself, and the slot reads it from there. Both paths are absolute,
//! since the child's working directory is not the slot's `work_dir`.
//!
//! `attach` reads its file when it is called, and the bytes ride the [`Upload`] from then
//! on: a write the board could not take is retried from the outbox with what the slot saw,
//! even once the file has moved or gone. `fetch_attachment` is a read the slot needs now,
//! so it is never queued: what Trello holds now is what it finds.

use std::fmt;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

use crate::board::{BoardClient, Card};
use crate::card::ACTION_TAG;
use crate::common::Diag;

/// The most one attachment may hold, either way: what Trello takes in one upload on a free
/// board, and the same guard the skill's `post_attachment.py` keeps.
pub(crate) const MAX_ATTACHMENT_BYTES: u64 = 10 * 1024 * 1024;

/// A file to put on a card: the name it is stored under, and its bytes as they were read.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Upload {
    /// The attachment's name on the card, verbatim: what `fetch_attachment` matches.
    pub(crate) name: String,
    /// Shared, so a retry from the outbox clones a pointer and not the file.
    pub(crate) bytes: Arc<[u8]>,
}

/// The name and the length, never the bytes: an upload can be a whole patch, and it sits
/// in queue entries a log line may print.
impl fmt::Debug for Upload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Upload")
            .field("name", &self.name)
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

/// Read the file at `path` into an upload named `name`, or the sentence that refuses it: a
/// relative path, an empty name, a path that is not a regular file, one that cannot be
/// read, or a file over [`MAX_ATTACHMENT_BYTES`].
pub(crate) fn read_upload(path: &str, name: &str) -> Result<Upload, String> {
    if !Path::new(path).is_absolute() {
        return Err(format!("attach: path must be absolute, not {path:?}"));
    }
    if name.is_empty() {
        return Err("attach: name must not be empty".to_string());
    }
    let cannot = |e: std::io::Error| format!("attach: cannot read {path}: {e}");
    let file = File::open(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => format!("attach: no file at {path}"),
        _ => cannot(e),
    })?;
    if !file.metadata().map_err(cannot)?.is_file() {
        return Err(format!("attach: {path} is not a regular file"));
    }
    // Read one byte past the cap rather than trust the size the metadata gave: a file
    // still being written can grow between the two.
    let mut bytes = Vec::new();
    file.take(MAX_ATTACHMENT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(cannot)?;
    if bytes.len() as u64 > MAX_ATTACHMENT_BYTES {
        return Err(format!(
            "attach: {path} is over the {} MiB Trello takes in one attachment",
            MAX_ATTACHMENT_BYTES / (1024 * 1024)
        ));
    }
    Ok(Upload {
        name: name.to_string(),
        bytes: bytes.into(),
    })
}

/// The `multipart/form-data` body Trello's upload takes, and its content type: a `name`
/// part carrying the attachment's name verbatim, which Trello stores as its name, and a
/// `file` part carrying the bytes verbatim under an ASCII-safe copy of the name, so no
/// quote, line break or wide character can break its header.
///
/// The boundary is the first `afkd-trello-<n>` that occurs in neither the name nor the
/// bytes.
pub(crate) fn multipart(name: &str, bytes: &[u8]) -> (String, Vec<u8>) {
    let boundary = (0u64..)
        .map(|n| format!("afkd-trello-{n}"))
        .find(|b| {
            let delimiter = format!("--{b}");
            !contains(name.as_bytes(), delimiter.as_bytes())
                && !contains(bytes, delimiter.as_bytes())
        })
        .unwrap_or_default();
    let mut body = Vec::with_capacity(bytes.len() + name.len() + 512);
    body.extend_from_slice(
        format!("--{boundary}\r\nContent-Disposition: form-data; name=\"name\"\r\n\r\n{name}\r\n")
            .as_bytes(),
    );
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; \
             filename=\"{}\"\r\nContent-Type: application/octet-stream\r\n\r\n",
            safe_filename(name)
        )
        .as_bytes(),
    );
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

/// `name` with every character outside `[A-Za-z0-9._+-]` made `_`, or `attachment` when
/// nothing is left.
fn safe_filename(name: &str) -> String {
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._+-".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    if safe.is_empty() {
        "attachment".to_string()
    } else {
        safe
    }
}

/// Whether `needle` occurs in `haystack`.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// One attachment as the card's listing describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Attachment {
    pub(crate) id: String,
    pub(crate) name: String,
    /// When it was put on the card; `None` when the listing gave no date it could read.
    pub(crate) date: Option<SystemTime>,
    /// Where its bytes are fetched from.
    pub(crate) url: String,
    /// An uploaded file, rather than a link someone attached.
    pub(crate) is_upload: bool,
    /// Its size, when the listing gives one.
    pub(crate) bytes: Option<u64>,
}

/// The newest uploaded file named exactly `name`: the greatest `(date, id)`. The date is
/// to the millisecond, since two uploads a slot makes back to back often share a second,
/// and Trello's ids do not order within one — a Trello id is only its second, then bytes
/// that differ by server. The id settles a tie in the millisecond, for a pick that is the
/// same every time. A link is no file, so a link of that name is passed over.
pub(crate) fn newest_named<'a>(
    attachments: &'a [Attachment],
    name: &str,
) -> Option<&'a Attachment> {
    attachments
        .iter()
        .filter(|a| a.is_upload && a.name == name)
        .max_by(|a, b| (a.date, &a.id).cmp(&(b.date, &b.id)))
}

/// Write the newest upload named `name` on `card` to the absolute path `to`, or say on the
/// diagnostic channel that there is none and write nothing. The bytes go to a temp file
/// beside `to` and are renamed onto it, so `to` never holds part of a download; a missing
/// parent directory is an error, not created.
pub(crate) fn fetch(
    board: &dyn BoardClient,
    card: &Card,
    name: &str,
    to: &str,
    diag: &dyn Diag,
) -> Result<(), String> {
    let dest = Path::new(to);
    if !dest.is_absolute() {
        return Err(format!("fetch_attachment: to must be absolute, not {to:?}"));
    }
    let Some(file) = dest.file_name() else {
        return Err(format!("fetch_attachment: to names no file: {to}"));
    };
    let listed = board
        .card_attachments(&card.id)
        .map_err(|e| e.to_string())?;
    let title = &card.title;
    let Some(found) = newest_named(&listed, name) else {
        diag.narrate(&format!(
            "{ACTION_TAG} no attachment named {name:?} on card \"{title}\"; nothing written"
        ));
        return Ok(());
    };
    if let Some(size) = found.bytes.filter(|&n| n > MAX_ATTACHMENT_BYTES) {
        return Err(format!(
            "fetch_attachment: attachment {name:?} is {size} bytes, over the {} MiB the plugin \
             fetches",
            MAX_ATTACHMENT_BYTES / (1024 * 1024)
        ));
    }
    let bytes = board
        .download_attachment(&found.url)
        .map_err(|e| e.to_string())?;
    let temp = dest.with_file_name(format!(
        ".{}.afkd-trello-{}",
        file.to_string_lossy(),
        std::process::id()
    ));
    std::fs::write(&temp, &bytes)
        .and_then(|()| std::fs::rename(&temp, dest))
        .map_err(|e| {
            let _ = std::fs::remove_file(&temp);
            format!("fetch_attachment: cannot write {to}: {e}")
        })?;
    diag.narrate(&format!(
        "{ACTION_TAG} fetching attachment {name:?} from card \"{title}\" into {to} ({} bytes)",
        bytes.len()
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::MockBoard;
    use crate::common::{CaptureDiag, TempDir};

    /// A body's parts as a server reads them: split on the boundary the content type
    /// names, each part's header block and its content, and the close delimiter last.
    fn parts(content_type: &str, body: &[u8]) -> Vec<(String, Vec<u8>)> {
        let boundary = content_type
            .strip_prefix("multipart/form-data; boundary=")
            .expect("a multipart content type");
        let delimiter = format!("--{boundary}");
        let mut segments = Vec::new();
        let mut rest = body;
        while let Some(at) = find(rest, delimiter.as_bytes()) {
            segments.push(&rest[..at]);
            rest = &rest[at + delimiter.len()..];
        }
        assert_eq!(
            segments.first(),
            Some(&&b""[..]),
            "the body opens on a delimiter"
        );
        assert_eq!(rest, b"--\r\n", "the close delimiter ends the body");
        segments[1..]
            .iter()
            .map(|segment| {
                let part = segment
                    .strip_prefix(b"\r\n")
                    .and_then(|p| p.strip_suffix(b"\r\n"))
                    .expect("a part between CRLFs");
                let split = find(part, b"\r\n\r\n").expect("a header block");
                (
                    String::from_utf8(part[..split].to_vec()).expect("ASCII headers"),
                    part[split + 4..].to_vec(),
                )
            })
            .collect()
    }

    fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }

    /// A body holding the first boundary the builder would pick, and a name that is wide,
    /// quoted and two lines, round-trip: the boundary moves on past what the body holds,
    /// the name part carries the name byte for byte, the file part the bytes, and the
    /// file name is ASCII the header can carry.
    #[test]
    fn multipart_round_trips_a_body_holding_the_first_boundary() {
        let name = "work – 計画 \"v2\"\r\n.patch";
        let mut bytes = b"--afkd-trello-0\r\n\0\xff\xfe binary".to_vec();
        bytes.extend_from_slice(b"\r\n--afkd-trello-1--\r\n");
        let (content_type, body) = multipart(name, &bytes);
        assert_eq!(content_type, "multipart/form-data; boundary=afkd-trello-2");
        let parts = parts(&content_type, &body);
        assert_eq!(parts.len(), 2, "{parts:?}");
        assert_eq!(
            parts[0],
            (
                "Content-Disposition: form-data; name=\"name\"".to_string(),
                name.as_bytes().to_vec()
            )
        );
        assert_eq!(
            parts[1].0,
            "Content-Disposition: form-data; name=\"file\"; \
             filename=\"work_______v2___.patch\"\r\nContent-Type: application/octet-stream"
        );
        assert_eq!(parts[1].1, bytes);
        assert_eq!(safe_filename("計画"), "__");
        assert_eq!(safe_filename(""), "attachment");
    }

    /// Each refusal names what is wrong, and only a regular file under the cap is read —
    /// its bytes exact, NULs and invalid UTF-8 included.
    #[test]
    fn read_upload_refuses_what_it_cannot_attach() {
        let dir = TempDir::new("read-upload");
        let at = |name: &str| dir.path().join(name).to_string_lossy().into_owned();
        let bytes = b"diff --git a/x b/x\n\0\xe5\xe4\xf6\r\n".to_vec();
        std::fs::write(at("work.patch"), &bytes).unwrap();
        let big = File::create(at("big.bin")).unwrap();
        big.set_len(MAX_ATTACHMENT_BYTES + 1).unwrap();
        File::create(at("edge.bin"))
            .unwrap()
            .set_len(MAX_ATTACHMENT_BYTES)
            .unwrap();

        let upload = read_upload(&at("work.patch"), "work.patch").unwrap();
        assert_eq!(&*upload.bytes, bytes.as_slice());
        assert_eq!(upload.name, "work.patch");
        assert_eq!(
            read_upload(&at("edge.bin"), "edge").unwrap().bytes.len() as u64,
            MAX_ATTACHMENT_BYTES
        );
        let dir_path = dir.path().to_string_lossy().into_owned();
        for (path, name, error) in [
            (
                "work.patch".to_string(),
                "w",
                "attach: path must be absolute, not \"work.patch\"".to_string(),
            ),
            (
                at("work.patch"),
                "",
                "attach: name must not be empty".into(),
            ),
            (
                at("gone.patch"),
                "w",
                format!("attach: no file at {}", at("gone.patch")),
            ),
            (
                dir_path.clone(),
                "w",
                format!("attach: {dir_path} is not a regular file"),
            ),
            (
                at("big.bin"),
                "w",
                format!(
                    "attach: {} is over the 10 MiB Trello takes in one attachment",
                    at("big.bin")
                ),
            ),
        ] {
            assert_eq!(read_upload(&path, name), Err(error), "{path}");
        }
    }

    /// An upload's debug form names it and counts its bytes, and never prints them.
    #[test]
    fn an_uploads_debug_form_hides_its_bytes() {
        let upload = Upload {
            name: "plan.md".into(),
            bytes: b"SECRET-PLAN-TEXT".to_vec().into(),
        };
        let shown = format!("{upload:?}");
        assert_eq!(shown, "Upload { name: \"plan.md\", bytes: 16 }");
    }

    fn listed(id: &str, name: &str, millis: Option<u64>, is_upload: bool) -> Attachment {
        Attachment {
            id: id.into(),
            name: name.into(),
            date: millis.map(|m| std::time::UNIX_EPOCH + std::time::Duration::from_millis(m)),
            url: format!("https://trello.com/{id}"),
            is_upload,
            bytes: Some(1),
        }
    }

    /// The newest upload of the exact name wins; a newer link or another name never does.
    /// Within one second the later millisecond wins whatever the ids say — the order a
    /// live board gave two back-to-back uploads (their real ids below) — and only a tie in
    /// the millisecond goes to the later id. An undated one is the oldest.
    #[test]
    fn newest_named_takes_the_latest_upload_of_that_name() {
        let all = [
            listed("a1", "plan.md", None, true),
            listed("a2", "plan.md", Some(100_000), true),
            listed("6add9f6a3da4586220bcb47a", "plan.md", Some(200_954), true),
            listed("6add9f6ac8f214f14ecbcf6e", "plan.md", Some(200_087), true),
            listed("a5", "plan.md", Some(300_000), false),
            listed("a6", "plan.md.old", Some(400_000), true),
            listed("a7", "Plan.md", Some(400_000), true),
        ];
        assert_eq!(
            newest_named(&all, "plan.md").map(|a| a.id.as_str()),
            Some("6add9f6a3da4586220bcb47a")
        );
        let tied = [
            listed("b2", "plan.md", Some(200_087), true),
            listed("b1", "plan.md", Some(200_087), true),
        ];
        assert_eq!(
            newest_named(&tied, "plan.md").map(|a| a.id.as_str()),
            Some("b2")
        );
        assert_eq!(
            newest_named(&all[..1], "plan.md").map(|a| a.id.as_str()),
            Some("a1")
        );
        assert_eq!(newest_named(&all, "work.patch"), None);
        assert_eq!(
            newest_named(&all[4..5], "plan.md"),
            None,
            "a link is no file"
        );
    }

    /// The title the narration tests render: the delimiter a line wraps it in, wide CJK,
    /// an emoji and an em dash.
    const TITLE: &str = "Fix \"the\" café — 修复 🚨";

    /// A board holding `card1`, titled [`TITLE`], and the card as the board reads it.
    fn board() -> (MockBoard, Card) {
        let board = MockBoard::new();
        board.add_list("L");
        board.add_card("L", "card1", TITLE, "");
        let card = board.read_card("card1").unwrap().expect("seeded");
        (board, card)
    }

    /// The newest upload is written to `to` through a temp file that does not outlive
    /// it, and said; with none of the name, nothing is written — a file already at `to`
    /// left as it was — and that is said instead.
    #[test]
    fn fetch_writes_the_newest_or_nothing() {
        let (board, card) = board();
        board.seed_attachment("card1", "plan.md", b"old plan\n", 100);
        board.seed_attachment("card1", "plan.md", "## 計画\n\n- one\n".as_bytes(), 200);
        let dir = TempDir::new("fetch");
        let to = dir.path().join("earlier-plan.md");
        let to = to.to_str().unwrap();
        let diag = CaptureDiag::default();
        fetch(&board, &card, "plan.md", to, &diag).unwrap();
        assert_eq!(std::fs::read(to).unwrap(), "## 計画\n\n- one\n".as_bytes());
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            1,
            "no temp file is left"
        );

        std::fs::write(to, "kept").unwrap();
        fetch(&board, &card, "work.patch", to, &diag).unwrap();
        assert_eq!(std::fs::read_to_string(to).unwrap(), "kept");
        let title = TITLE;
        assert_eq!(
            diag.narrated(),
            [
                format!(
                    "[trello] fetching attachment \"plan.md\" from card \"{title}\" into {to} \
                     (17 bytes)"
                ),
                format!(
                    "[trello] no attachment named \"work.patch\" on card \"{title}\"; nothing \
                     written"
                ),
            ]
        );
        assert!(diag.errs().is_empty(), "{:?}", diag.errs());
    }

    /// A relative `to` is refused before the board is asked; a missing parent directory,
    /// an oversized listing and a board that cannot be reached are each the call's error.
    #[test]
    fn fetch_refuses_what_it_cannot_write() {
        let (board, card) = board();
        board.seed_attachment("card1", "plan.md", b"plan", 100);
        let diag = CaptureDiag::default();
        assert_eq!(
            fetch(&board, &card, "plan.md", "earlier-plan.md", &diag),
            Err("fetch_attachment: to must be absolute, not \"earlier-plan.md\"".into())
        );
        assert_eq!(board.calls(), ["read card"], "only the fixture's read");

        let dir = TempDir::new("fetch-refuses");
        let nowhere = dir.path().join("missing/plan.md");
        let nowhere = nowhere.to_str().unwrap();
        let error = fetch(&board, &card, "plan.md", nowhere, &diag).unwrap_err();
        assert!(
            error.starts_with(&format!("fetch_attachment: cannot write {nowhere}: ")),
            "{error}"
        );

        let downloads = |board: &MockBoard| {
            board
                .calls()
                .iter()
                .filter(|&&c| c == "download attachment")
                .count()
        };
        let before = downloads(&board);
        board.seed_attachment_sized("card1", "huge.bin", MAX_ATTACHMENT_BYTES + 1, 300);
        let to = dir.path().join("huge.bin");
        assert_eq!(
            fetch(&board, &card, "huge.bin", to.to_str().unwrap(), &diag),
            Err(
                "fetch_attachment: attachment \"huge.bin\" is 10485761 bytes, over the 10 MiB \
                 the plugin fetches"
                    .into()
            )
        );
        assert_eq!(
            downloads(&board),
            before,
            "an oversized listing is never fetched"
        );

        board.go_down();
        assert_eq!(
            fetch(&board, &card, "plan.md", to.to_str().unwrap(), &diag),
            Err("trello list attachments: no response (mock failure)".into())
        );
        assert!(!to.exists());
    }
}
