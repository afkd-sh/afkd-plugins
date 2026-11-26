//! Files on a card: the upload `trello.create_attachment` puts there, the multipart form
//! Trello takes it in, and the atomic write `trello.download_attachment` and the claim
//! record both write through.
//!
//! The plugin runs as an unconfined child on the operator's host, so a download writes the
//! file at the path the slot names itself, and the slot reads it from there. The path is
//! absolute, since the child's working directory is not the slot's `work_dir`.
//!
//! An upload reads its file when it is called, and the bytes ride the [`Upload`] from then
//! on.

use std::fmt;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;

/// The most one attachment may hold, either way: what Trello takes in one upload on a free
/// board, and the same guard the skill's `create_attachment.py` and `download_attachment.py`
/// keep.
pub(crate) const MAX_ATTACHMENT_BYTES: u64 = 10 * 1024 * 1024;

/// A file to put on a card: the name it is stored under, and its bytes as they were read.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Upload {
    /// The attachment's name on the card, verbatim.
    pub(crate) name: String,
    /// Shared, so passing an upload on clones a pointer and not the file.
    pub(crate) bytes: Arc<[u8]>,
}

/// The name and the length, never the bytes: an upload can be a whole patch, and a log line
/// may print one.
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

/// Write `bytes` to `dest` through a temp file beside it, renamed onto it, so `dest` never
/// holds part of a download and no temp file outlives a failed write. A missing parent
/// directory is an error, not created, and so is a path that names no file.
pub(crate) fn write_atomic(dest: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let Some(file) = dest.file_name() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "the path names no file",
        ));
    };
    let temp = dest.with_file_name(format!(
        ".{}.afkd-trello-{}",
        file.to_string_lossy(),
        std::process::id()
    ));
    std::fs::write(&temp, bytes)
        .and_then(|()| std::fs::rename(&temp, dest))
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&temp);
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::TempDir;

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
}
