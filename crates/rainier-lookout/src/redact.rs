//! Keeping secrets out of what Lookout records.
//!
//! **Deny by name, and truncate everything else.** A header or field whose
//! name is in [`Redaction`] — compared case-insensitively — is replaced by
//! [`REDACTED`] outright; every other string is kept but cut to
//! [`Redaction::max_value_len`] bytes. Both, in that order: a long secret must
//! not survive as its first two kilobytes.
//!
//! Defence in depth. The framework's spans already leave out values (bound
//! SQL parameters, cache keys, addresses); this is what makes the opt-in
//! captures — headers, a request body on failure, a failure's details — safe
//! to keep for a day in a store other people can read.

use serde_json::{Map, Value};

use crate::config::Redaction;

/// What replaces a redacted value.
pub const REDACTED: &str = "[redacted]";

/// The marker appended to a truncated value.
const ELLIPSIS: &str = "…";

/// Whether `name` is a field whose value must not be kept.
pub fn is_redacted_field(name: &str, redaction: &Redaction) -> bool {
    redaction.fields.iter().any(|f| f.eq_ignore_ascii_case(name))
}

/// Whether `name` is a header whose value must not be kept.
pub fn is_redacted_header(name: &str, redaction: &Redaction) -> bool {
    redaction.headers.iter().any(|h| h.eq_ignore_ascii_case(name))
}

/// `value`, cut to at most `max` bytes on a character boundary, with `…`
/// appended when anything was cut.
pub fn truncate(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_string();
    }
    let mut end = max;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{ELLIPSIS}", &value[..end])
}

/// Redact `value` in place: every object key named in
/// [`Redaction::fields`], at any depth, has its value replaced; every string
/// is truncated.
pub fn redact_json(value: &mut Value, redaction: &Redaction) {
    match value {
        Value::Object(map) => {
            for (key, inner) in map.iter_mut() {
                if is_redacted_field(key, redaction) {
                    *inner = Value::String(REDACTED.to_string());
                } else {
                    redact_json(inner, redaction);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                redact_json(item, redaction);
            }
        }
        Value::String(text) if text.len() > redaction.max_value_len => {
            *text = truncate(text, redaction.max_value_len);
        }
        _ => {}
    }
}

/// [`redact_json`] on an owned value.
pub fn redacted(mut value: Value, redaction: &Redaction) -> Value {
    redact_json(&mut value, redaction);
    value
}

/// Headers as a JSON object, names lower-cased, redacted by name and
/// truncated. A repeated header becomes an array.
pub fn redact_headers<'a>(
    headers: impl IntoIterator<Item = (&'a str, &'a str)>,
    redaction: &Redaction,
) -> Value {
    let mut map = Map::new();
    for (name, value) in headers {
        let name = name.to_ascii_lowercase();
        let value = if is_redacted_header(&name, redaction) {
            REDACTED.to_string()
        } else {
            truncate(value, redaction.max_value_len)
        };
        match map.get_mut(&name) {
            Some(Value::Array(values)) => values.push(Value::String(value)),
            Some(existing) => {
                let first = existing.take();
                *existing = Value::Array(vec![first, Value::String(value)]);
            }
            None => {
                map.insert(name, Value::String(value));
            }
        }
    }
    Value::Object(map)
}

/// A request or job body as something safe to keep: parsed and redacted when
/// it is JSON or a form, truncated text otherwise.
pub fn redact_body(body: &[u8], redaction: &Redaction) -> Value {
    if body.is_empty() {
        return Value::Null;
    }
    if let Ok(json) = serde_json::from_slice::<Value>(body) {
        return redacted(json, redaction);
    }
    let text = String::from_utf8_lossy(body);
    if text.contains('=') && !text.contains(char::is_whitespace) {
        let mut map = Map::new();
        for pair in text.split('&') {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            map.insert(key.to_string(), Value::String(value.to_string()));
        }
        return redacted(Value::Object(map), redaction);
    }
    Value::String(truncate(&text, redaction.max_value_len))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn redaction() -> Redaction {
        Redaction { max_value_len: 8, ..Redaction::default() }
    }

    #[test]
    fn fields_are_denied_by_name_case_insensitively_at_any_depth() {
        let value = redacted(
            json!({
                "Password": "hunter2",
                "user": {"name": "ada", "TOKEN": "abc", "list": [{"secret": 1}]},
                "ok": 3,
            }),
            &redaction(),
        );
        assert_eq!(value["Password"], REDACTED);
        assert_eq!(value["user"]["TOKEN"], REDACTED);
        assert_eq!(value["user"]["list"][0]["secret"], REDACTED);
        assert_eq!(value["user"]["name"], "ada");
        assert_eq!(value["ok"], 3);
    }

    #[test]
    fn long_strings_are_truncated_on_a_char_boundary() {
        assert_eq!(truncate("abcdefghij", 4), "abcd…");
        assert_eq!(truncate("short", 8), "short");
        // 'é' is two bytes; cutting at 3 would split the second one.
        assert_eq!(truncate("éééé", 3), "é…");
        let value = redacted(json!({"note": "0123456789"}), &redaction());
        assert_eq!(value["note"], "01234567…");
    }

    #[test]
    fn headers_are_denied_by_name_and_repeated_ones_kept() {
        let value = redact_headers(
            [("Authorization", "Bearer x"), ("Accept", "a"), ("accept", "b"), ("Cookie", "s=1")],
            &redaction(),
        );
        assert_eq!(value["authorization"], REDACTED);
        assert_eq!(value["cookie"], REDACTED);
        assert_eq!(value["accept"], json!(["a", "b"]));
    }

    #[test]
    fn bodies_are_parsed_then_redacted() {
        let r = Redaction::default();
        assert_eq!(redact_body(br#"{"password":"p","a":1}"#, &r)["password"], REDACTED);
        let form = redact_body(b"email=a%40b&password=p", &r);
        assert_eq!(form["password"], REDACTED);
        assert_eq!(form["email"], "a%40b");
        assert_eq!(redact_body(b"", &r), Value::Null);
        assert_eq!(redact_body(b"plain text", &r), "plain text");
    }
}
