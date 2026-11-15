//! Keeping secrets out of what Lookout records.
//!
//! **Deny by name, star out what looks secret, and truncate everything else.**
//! A header or field is replaced by [`REDACTED`] outright when its name is in
//! [`Redaction::fields`] or [`Redaction::headers`], contains one of
//! [`Redaction::field_patterns`] (`password` covers `new_password`), or was
//! marked sensitive by the application — `#[orm(sensitive)]` on a model field
//! or `FormRequest::sensitive` ([`rainier_support::sensitive`]). Every other
//! string that [looks like a secret](mask_secret) — a key, a token, a hash —
//! is starred out the same way every time; the rest is kept but cut to
//! [`Redaction::max_value_len`] bytes. In that order: a long secret must not
//! survive as its first two kilobytes.
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

/// Whether `name` is a field whose value must not be kept: listed, matching
/// a pattern, or marked sensitive by the application.
pub fn is_redacted_field(name: &str, redaction: &Redaction) -> bool {
    if redaction.fields.iter().any(|f| f.eq_ignore_ascii_case(name)) {
        return true;
    }
    let lower = name.to_ascii_lowercase();
    redaction.field_patterns.iter().any(|p| lower.contains(&p.to_ascii_lowercase()))
        || rainier_support::sensitive::is_marked(&lower)
}

/// Whether `name` is a header whose value must not be kept.
pub fn is_redacted_header(name: &str, redaction: &Redaction) -> bool {
    redaction.headers.iter().any(|h| h.eq_ignore_ascii_case(name))
        || is_redacted_field(name, redaction)
}

/// The shortest string [`mask_secret`] will consider.
const SECRET_MIN_LEN: usize = 20;

/// How much of a secret is kept visible, to tell two apart at a glance.
const SECRET_PREFIX: usize = 4;

/// `value` starred out when it looks like a secret, or `None`.
///
/// "Looks like a secret": at least twenty characters, one word, drawn from
/// the alphabets keys and tokens are written in (letters, digits, `+/=_-.`),
/// with letters **and** digits in it, and random enough — over 3.5 bits of
/// Shannon entropy a character, which an API key, a session id, a JWT or a
/// hash clears and a `snake_case_identifier` does not. A UUID, a number, a
/// URL, a path, an email address or a sentence is left alone.
///
/// Deterministic: the same value always gives the same mask — its first four
/// characters, then a star for each of the rest (up to 32) and the length —
/// so two requests carrying the same token can be told to carry the same
/// one, and a different one is visibly different.
///
/// ```
/// use rainier_lookout::redact::mask_secret;
///
/// let key = "tok_9Fh2Lw8QzX4mN7pR3vT6yB1cK";
/// assert_eq!(mask_secret(key).unwrap(), format!("tok_{} ({} chars)", "*".repeat(25), key.len()));
/// assert_eq!(mask_secret(key), mask_secret(key), "deterministic");
/// assert!(mask_secret("550e8400-e29b-41d4-a716-446655440000").is_none(), "a UUID");
/// assert!(mask_secret("profile_picture_upload_status").is_none(), "an identifier");
/// assert!(mask_secret("https://lewd.net/posts/123456789012345").is_none(), "a URL");
/// ```
pub fn mask_secret(value: &str) -> Option<String> {
    let len = value.chars().count();
    if len < SECRET_MIN_LEN || !looks_like_a_secret(value) {
        return None;
    }
    let prefix: String = value.chars().take(SECRET_PREFIX).collect();
    let stars = "*".repeat((len - SECRET_PREFIX).min(32));
    Some(format!("{prefix}{stars} ({len} chars)"))
}

fn looks_like_a_secret(value: &str) -> bool {
    let token_char =
        |c: char| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=' | '_' | '-' | '.');
    if !value.chars().all(token_char) {
        return false; // spaces, '@', ':' — a sentence, an email, a URL
    }
    if value.starts_with('/') || value.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return false; // a path, a number
    }
    if is_uuid(value) {
        return false;
    }
    let has_letter = value.chars().any(|c| c.is_ascii_alphabetic());
    let has_digit = value.chars().any(|c| c.is_ascii_digit());
    has_letter && has_digit && entropy_per_char(value) > 3.5
}

fn is_uuid(value: &str) -> bool {
    let parts: Vec<&str> = value.split('-').collect();
    parts.len() == 5
        && [8, 4, 4, 4, 12]
            .iter()
            .zip(&parts)
            .all(|(n, p)| p.len() == *n && p.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Shannon entropy, in bits per character.
fn entropy_per_char(value: &str) -> f64 {
    let mut counts = std::collections::HashMap::new();
    for c in value.chars() {
        *counts.entry(c).or_insert(0usize) += 1;
    }
    let n = value.chars().count() as f64;
    counts
        .values()
        .map(|&k| {
            let p = k as f64 / n;
            -p * p.log2()
        })
        .sum()
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
        Value::String(text) => {
            if let Some(masked) = redaction.mask_secrets.then(|| mask_secret(text)).flatten() {
                *text = masked;
            } else if text.len() > redaction.max_value_len {
                *text = truncate(text, redaction.max_value_len);
            }
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
        } else if let Some(masked) = redaction.mask_secrets.then(|| mask_secret(value)).flatten() {
            masked
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
    fn a_field_containing_a_pattern_is_redacted() {
        let value = redacted(
            json!({"new_password": "x", "current_password": "y", "client_secret": "z", "display_name": "ada"}),
            &Redaction::default(),
        );
        assert_eq!(value["new_password"], REDACTED);
        assert_eq!(value["current_password"], REDACTED);
        assert_eq!(value["client_secret"], REDACTED);
        assert_eq!(value["display_name"], "ada");
    }

    #[test]
    fn a_field_marked_sensitive_by_the_application_is_redacted() {
        rainier_support::sensitive::mark(["redact_test_mothers_maiden_name"]);
        let value = redacted(
            json!({"redact_test_mothers_maiden_name": "smith", "nested": [{"REDACT_TEST_MOTHERS_MAIDEN_NAME": "jones"}]}),
            &Redaction::default(),
        );
        assert_eq!(value["redact_test_mothers_maiden_name"], REDACTED);
        assert_eq!(value["nested"][0]["REDACT_TEST_MOTHERS_MAIDEN_NAME"], REDACTED);
    }

    #[test]
    fn strings_that_look_like_secrets_are_starred_out_anywhere() {
        let jwt = "eyJhbGciOiJSUzI1NiJ9.eyJzdWIiOiI0MiJ9.Xk3vQ9mZp2Lr8TbW1nYc5Hs";
        let hash = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";
        let value = redacted(
            json!({"note": jwt, "list": [hash], "words": "a perfectly ordinary sentence of text",
                   "id": "550e8400-e29b-41d4-a716-446655440000", "n": "12345678901234567890123"}),
            &Redaction::default(),
        );
        assert!(value["note"].as_str().unwrap().starts_with("eyJh***"), "{value}");
        assert!(value["list"][0].as_str().unwrap().starts_with("9f86***"), "{value}");
        assert!(value["note"].as_str().unwrap().ends_with(&format!("({} chars)", jwt.len())));
        assert_eq!(value["words"], "a perfectly ordinary sentence of text");
        assert_eq!(value["id"], "550e8400-e29b-41d4-a716-446655440000");
        assert_eq!(value["n"], "12345678901234567890123");
        // The same value masks the same way; masking can be turned off.
        assert_eq!(mask_secret(jwt), mask_secret(jwt));
        let off = Redaction { mask_secrets: false, ..Redaction::default() };
        assert_eq!(redacted(json!({"note": jwt}), &off)["note"], jwt);
    }

    #[test]
    fn a_header_that_looks_secret_is_starred_and_one_named_like_a_field_is_redacted() {
        let value = redact_headers(
            [("X-Signature", "a"), ("X-Request-Id", "Zq8mN2pL7vR4tY9wK3xB6cD1")],
            &Redaction::default(),
        );
        assert_eq!(value["x-signature"], REDACTED, "the `signature` pattern");
        assert!(value["x-request-id"].as_str().unwrap().starts_with("Zq8m***"), "{value}");
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
