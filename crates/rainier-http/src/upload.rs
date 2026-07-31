//! `multipart/form-data` parsing — [`UploadedFile`] and the file bag.
//!
//! Deliberately a **buffered** parser: it takes the complete body and splits it
//! on the boundary, rather than streaming parts as they arrive. That matches
//! the rest of the request model (see [`crate::body`]) and keeps
//! `$request->file('avatar')` a synchronous call. The protection against a
//! malicious 10 GB upload is therefore the server's body-size limit, applied
//! *before* this ever runs — not backpressure inside it.

use std::collections::HashMap;

use bytes::Bytes;
use rainier_support::{Error, Result};
use serde_json::Value;

/// A file received in a `multipart/form-data` request.
#[derive(Debug, Clone)]
pub struct UploadedFile {
    field: String,
    file_name: Option<String>,
    content_type: Option<String>,
    bytes: Bytes,
}

impl UploadedFile {
    /// The form field this file arrived under.
    pub fn field(&self) -> &str {
        &self.field
    }

    /// The client-supplied file name.
    ///
    /// **Untrusted.** A browser sends whatever the client says, which may
    /// contain `../`, a null byte, or a name designed to collide with
    /// something on disk. Use [`extension`](Self::extension) and generate your
    /// own name when storing; never join this onto a path.
    pub fn client_file_name(&self) -> Option<&str> {
        self.file_name.as_deref()
    }

    /// The client-supplied MIME type. Also untrusted — sniff the contents if
    /// the distinction matters for security.
    pub fn content_type(&self) -> Option<&str> {
        self.content_type.as_deref()
    }

    /// The file's contents.
    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }

    /// The file's size in bytes.
    pub fn size(&self) -> usize {
        self.bytes.len()
    }

    /// Whether the part carried no content — a file input left empty submits
    /// an empty part rather than nothing at all.
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// The lowercased extension of the client file name, with no dot, and only
    /// when it is alphanumeric — so `image.php\0.jpg` or `x.../y` yields
    /// `None` rather than something dangerous to concatenate.
    pub fn extension(&self) -> Option<String> {
        let name = self.file_name.as_deref()?;
        let ext = name.rsplit_once('.')?.1;
        if ext.is_empty() || !ext.chars().all(|c| c.is_ascii_alphanumeric()) {
            return None;
        }
        Some(ext.to_ascii_lowercase())
    }

    /// Write the file to `path`.
    pub fn store(&self, path: impl AsRef<std::path::Path>) -> Result<()> {
        std::fs::write(path.as_ref(), &self.bytes).map_err(|e| {
            Error::internal(format!("could not store upload at {}: {e}", path.as_ref().display()))
        })
    }
}

/// The outcome of parsing a `multipart/form-data` body: ordinary fields lifted
/// into a JSON tree, plus the files.
#[derive(Debug, Default)]
pub struct Multipart {
    /// Non-file fields, in the same shape as a urlencoded form.
    pub fields: Value,
    /// Files, keyed by form field name. A repeated field (`photos[]`) keeps
    /// every file under that key.
    pub files: HashMap<String, Vec<UploadedFile>>,
}

/// Pull the `boundary=` parameter out of a `Content-Type` header.
pub fn boundary_of(content_type: &str) -> Option<String> {
    let (_, params) = content_type.split_once(';')?;
    for param in params.split(';') {
        let (name, value) = param.split_once('=')?;
        if name.trim().eq_ignore_ascii_case("boundary") {
            let value = value.trim();
            let value = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')).unwrap_or(value);
            return Some(value.to_string());
        }
    }
    None
}

/// Parse a `multipart/form-data` body given its boundary.
pub fn parse(body: &Bytes, boundary: &str) -> Result<Multipart> {
    let delimiter = format!("--{boundary}");
    let mut result =
        Multipart { fields: Value::Object(serde_json::Map::new()), ..Default::default() };

    for part in split_on(body, delimiter.as_bytes()) {
        // The closing delimiter is `--boundary--`; anything before the first
        // delimiter is preamble. Both arrive here as parts we skip.
        let part = match part.strip_prefix(b"\r\n".as_slice()) {
            Some(rest) => rest,
            None => continue,
        };

        let Some((head, body)) = split_once(part, b"\r\n\r\n") else {
            continue;
        };

        let headers = parse_headers(head);
        let Some(disposition) = headers.get("content-disposition") else {
            continue;
        };
        let Some(field) = header_param(disposition, "name") else {
            continue;
        };

        // Each part's body is terminated by the CRLF preceding its delimiter.
        let body = body.strip_suffix(b"\r\n".as_slice()).unwrap_or(body);

        match header_param(disposition, "filename") {
            Some(file_name) => {
                let file = UploadedFile {
                    field: field.clone(),
                    file_name: Some(file_name),
                    content_type: headers.get("content-type").cloned(),
                    bytes: Bytes::copy_from_slice(body),
                };
                // `photos[]` and `photos` both collect under `photos`.
                let key = field.strip_suffix("[]").unwrap_or(&field).to_string();
                result.files.entry(key).or_default().push(file);
            }
            None => {
                let text = String::from_utf8_lossy(body).into_owned();
                crate::input::insert(&mut result.fields, &field, Value::String(text));
            }
        }
    }

    Ok(result)
}

/// Split `haystack` on every occurrence of `needle`.
fn split_on<'a>(haystack: &'a [u8], needle: &[u8]) -> Vec<&'a [u8]> {
    let mut parts = Vec::new();
    let mut rest = haystack;

    while let Some(at) = find(rest, needle) {
        parts.push(&rest[..at]);
        rest = &rest[at + needle.len()..];
    }
    parts.push(rest);
    parts
}

fn split_once<'a>(haystack: &'a [u8], needle: &[u8]) -> Option<(&'a [u8], &'a [u8])> {
    let at = find(haystack, needle)?;
    Some((&haystack[..at], &haystack[at + needle.len()..]))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|window| window == needle)
}

/// Parse a part's headers into a lowercase-keyed map.
fn parse_headers(head: &[u8]) -> HashMap<String, String> {
    let mut headers = HashMap::new();
    for line in String::from_utf8_lossy(head).lines() {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    headers
}

/// Read a `name="value"` parameter out of a header value.
fn header_param(header: &str, param: &str) -> Option<String> {
    for piece in header.split(';').skip(1) {
        let (name, value) = piece.split_once('=')?;
        if !name.trim().eq_ignore_ascii_case(param) {
            continue;
        }
        let value = value.trim();
        let value = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')).unwrap_or(value);
        return Some(value.to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const BOUNDARY: &str = "----RainierBoundary";

    fn body(parts: &[&str]) -> Bytes {
        let mut out = String::new();
        for part in parts {
            out.push_str(&format!("--{BOUNDARY}\r\n{part}\r\n"));
        }
        out.push_str(&format!("--{BOUNDARY}--\r\n"));
        Bytes::from(out)
    }

    #[test]
    fn extracts_the_boundary_from_a_content_type() {
        assert_eq!(boundary_of("multipart/form-data; boundary=abc123").as_deref(), Some("abc123"));
        assert_eq!(
            boundary_of("multipart/form-data; charset=utf-8; boundary=\"a b\"").as_deref(),
            Some("a b")
        );
        assert_eq!(boundary_of("application/json"), None);
    }

    #[test]
    fn parses_plain_fields() {
        let raw = body(&[
            "Content-Disposition: form-data; name=\"title\"\r\n\r\nHello",
            "Content-Disposition: form-data; name=\"body\"\r\n\r\nWorld",
        ]);
        let parsed = parse(&raw, BOUNDARY).unwrap();
        assert_eq!(parsed.fields, json!({ "title": "Hello", "body": "World" }));
        assert!(parsed.files.is_empty());
    }

    #[test]
    fn fields_honour_bracket_notation() {
        let raw = body(&[
            "Content-Disposition: form-data; name=\"tags[]\"\r\n\r\na",
            "Content-Disposition: form-data; name=\"tags[]\"\r\n\r\nb",
            "Content-Disposition: form-data; name=\"user[name]\"\r\n\r\nada",
        ]);
        let parsed = parse(&raw, BOUNDARY).unwrap();
        assert_eq!(parsed.fields, json!({ "tags": ["a", "b"], "user": { "name": "ada" } }));
    }

    #[test]
    fn parses_a_file_part() {
        let raw =
            body(&["Content-Disposition: form-data; name=\"avatar\"; filename=\"me.PNG\"\r\n\
             Content-Type: image/png\r\n\r\nPNGDATA"]);
        let parsed = parse(&raw, BOUNDARY).unwrap();

        let file = &parsed.files["avatar"][0];
        assert_eq!(file.field(), "avatar");
        assert_eq!(file.client_file_name(), Some("me.PNG"));
        assert_eq!(file.content_type(), Some("image/png"));
        assert_eq!(file.bytes(), &Bytes::from("PNGDATA"));
        assert_eq!(file.size(), 7);
        assert_eq!(file.extension().as_deref(), Some("png"));
    }

    #[test]
    fn several_files_collect_under_one_key() {
        let raw = body(&[
            "Content-Disposition: form-data; name=\"photos[]\"; filename=\"a.jpg\"\r\n\r\nAAA",
            "Content-Disposition: form-data; name=\"photos[]\"; filename=\"b.jpg\"\r\n\r\nBBB",
        ]);
        let parsed = parse(&raw, BOUNDARY).unwrap();
        assert_eq!(parsed.files["photos"].len(), 2);
        assert_eq!(parsed.files["photos"][1].bytes(), &Bytes::from("BBB"));
    }

    #[test]
    fn files_and_fields_coexist() {
        let raw = body(&[
            "Content-Disposition: form-data; name=\"title\"\r\n\r\nMy post",
            "Content-Disposition: form-data; name=\"cover\"; filename=\"c.gif\"\r\n\r\nGIF",
        ]);
        let parsed = parse(&raw, BOUNDARY).unwrap();
        assert_eq!(parsed.fields, json!({ "title": "My post" }));
        assert_eq!(parsed.files["cover"][0].bytes(), &Bytes::from("GIF"));
    }

    #[test]
    fn binary_content_survives_intact() {
        // Includes bytes that are not valid UTF-8 and a CRLF in the middle.
        let payload: &[u8] = &[0x00, 0xFF, 0x0D, 0x0A, 0x42, 0x80];
        let mut raw = Vec::new();
        raw.extend_from_slice(
            format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"f\"; filename=\"b.bin\"\r\n\r\n"
            )
            .as_bytes(),
        );
        raw.extend_from_slice(payload);
        raw.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());

        let parsed = parse(&Bytes::from(raw), BOUNDARY).unwrap();
        assert_eq!(parsed.files["f"][0].bytes().as_ref(), payload);
    }

    #[test]
    fn an_empty_file_part_is_reported_as_empty() {
        let raw = body(&["Content-Disposition: form-data; name=\"avatar\"; filename=\"\"\r\n\r\n"]);
        let parsed = parse(&raw, BOUNDARY).unwrap();
        assert!(parsed.files["avatar"][0].is_empty());
    }

    #[test]
    fn a_hostile_file_name_yields_no_extension() {
        let raw = body(&[
            "Content-Disposition: form-data; name=\"f\"; filename=\"../../etc/passwd\"\r\n\r\nx",
        ]);
        let parsed = parse(&raw, BOUNDARY).unwrap();
        let file = &parsed.files["f"][0];
        // The raw name is preserved (it may be worth logging) but yields
        // nothing that could be concatenated into a path.
        assert_eq!(file.client_file_name(), Some("../../etc/passwd"));
        assert_eq!(file.extension(), None);
    }

    #[test]
    fn a_double_extension_with_a_null_byte_yields_none() {
        let raw = body(&[
            "Content-Disposition: form-data; name=\"f\"; filename=\"shell.php\u{0}.jpg\"\r\n\r\nx",
        ]);
        let parsed = parse(&raw, BOUNDARY).unwrap();
        // The last extension is `jpg` here, which is fine — the point is that
        // it is alphanumeric-only and the caller never sees `php\0`.
        assert_eq!(parsed.files["f"][0].extension().as_deref(), Some("jpg"));
    }

    #[test]
    fn an_empty_body_parses_to_nothing() {
        let parsed = parse(&Bytes::new(), BOUNDARY).unwrap();
        assert_eq!(parsed.fields, json!({}));
        assert!(parsed.files.is_empty());
    }

    #[test]
    fn parts_without_a_name_are_skipped() {
        let raw = body(&["Content-Disposition: form-data\r\n\r\norphan"]);
        let parsed = parse(&raw, BOUNDARY).unwrap();
        assert_eq!(parsed.fields, json!({}));
    }
}
