//! A `multipart/form-data` reader (RFC 7578): what the fake Bot API reads an upload with,
//! and what the plugin's own unit test reads its forms back with. std only, so either side
//! can `#[path]`-include it.

#![allow(dead_code)]

/// One part of a form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    pub name: String,
    pub filename: Option<String>,
    pub content_type: Option<String>,
    pub bytes: Vec<u8>,
}

/// The parts of `body`, sent under the `Content-Type` header `content_type`, or what is
/// wrong with it.
pub fn parse(content_type: &str, body: &[u8]) -> Result<Vec<Part>, String> {
    let boundary = content_type
        .split(';')
        .map(str::trim)
        .find_map(|p| p.strip_prefix("boundary="))
        .map(|b| b.trim_matches('"'))
        .ok_or_else(|| format!("no boundary in `{content_type}`"))?;
    let delimiter = format!("--{boundary}").into_bytes();
    let mut rest = body
        .strip_prefix(delimiter.as_slice())
        .ok_or("the body does not open with the boundary")?;
    let mut parts = Vec::new();
    loop {
        if rest.starts_with(b"--") {
            return Ok(parts);
        }
        rest = rest
            .strip_prefix(b"\r\n")
            .ok_or("a delimiter not ended by CR LF")?;
        let head_end = find(rest, b"\r\n\r\n").ok_or("a part with no blank line")?;
        let head = std::str::from_utf8(&rest[..head_end]).map_err(|e| e.to_string())?;
        let after = &rest[head_end + 4..];
        let mut next = b"\r\n".to_vec();
        next.extend_from_slice(&delimiter);
        let end = find(after, &next).ok_or("a part not closed by the boundary")?;
        let (mut name, mut filename, mut content_type) = (None, None, None);
        for line in head.split("\r\n") {
            let Some((header, value)) = line.split_once(':') else {
                return Err(format!("a header line without a colon: {line:?}"));
            };
            if header.eq_ignore_ascii_case("content-disposition") {
                for param in value.split(';').map(str::trim) {
                    if let Some(v) = param.strip_prefix("name=") {
                        name = Some(v.trim_matches('"').to_string());
                    } else if let Some(v) = param.strip_prefix("filename=") {
                        filename = Some(v.trim_matches('"').to_string());
                    }
                }
            } else if header.eq_ignore_ascii_case("content-type") {
                content_type = Some(value.trim().to_string());
            }
        }
        parts.push(Part {
            name: name.ok_or("a part with no name")?,
            filename,
            content_type,
            bytes: after[..end].to_vec(),
        });
        rest = &after[end + next.len()..];
    }
}

/// Where `needle` first starts in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}
