//! Keeping credentials off the error page.
//!
//! This module exists because Whoops taught the lesson the expensive way. A
//! debug error page renders the request that caused it, and a request routinely
//! carries an `Authorization` header, a session cookie, a password field and a
//! `DATABASE_URL` in the environment panel. Whoops has been the proximate cause
//! of real credential disclosure more than once — not because the page was
//! reachable in production (though sometimes it was), but because the page was
//! **screenshotted into a ticket**, pasted into a chat, or committed to a repo
//! as a bug report.
//!
//! So the rule here is the opposite of Whoops's default:
//!
//! > **Deny by name, and truncate everything else.**
//!
//! A value whose key looks sensitive is replaced outright. Every other value is
//! shown, because a page that hides everything is a page nobody reads — but
//! long values are truncated, because a 2 KB JWT in the body panel is both
//! useless to read and exactly the thing that should not be screenshotted.
//!
//! This is defence in depth, not the defence. The defence is that the page only
//! renders when `debug` is true. This is what makes the *screenshot* safe.

/// Key fragments that mark a value as secret, matched case-insensitively
/// anywhere in the key.
///
/// Deliberately broad. A false positive costs a developer one `[redacted]` on a
/// field they wanted to see; a false negative costs a credential. `key` catches
/// `api_key`, `secret_key` and `stripe_key`; `token` catches `access_token`,
/// `csrf_token` and `_token`.
const SECRET_KEYS: &[&str] = &[
    "authorization",
    "auth",
    "password",
    "passwd",
    "secret",
    "token",
    "key",
    "credential",
    "cookie",
    "session",
    "signature",
    "sig",
    "salt",
    "hash",
    "private",
    "cvv",
    "card",
    "pan",
    "ssn",
    "otp",
    "mfa",
    "pin",
];

/// Values longer than this are truncated in the request panels.
const MAX_VALUE: usize = 300;

/// What replaces a secret. Says which rule fired, so a developer who needs the
/// value knows it was a deliberate decision rather than a missing field.
const REDACTED: &str = "[redacted by rainier-debug]";

/// Is this key's value a secret?
pub fn is_secret_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    SECRET_KEYS.iter().any(|needle| lower.contains(needle))
}

/// The value to display for `key`.
///
/// Redacts by key, then truncates by length. Both, in that order: a long secret
/// must not be shown truncated, because the first 300 characters of a JWT are
/// still the header and payload.
pub fn value(key: &str, raw: &str) -> String {
    if is_secret_key(key) {
        return REDACTED.to_string();
    }
    truncate(raw)
}

/// Shorten a long value, saying how much was dropped.
pub fn truncate(raw: &str) -> String {
    if raw.len() <= MAX_VALUE {
        return raw.to_string();
    }
    // Slice on a character boundary — a body is arbitrary bytes and cutting a
    // UTF-8 sequence in half would panic inside the error page, which is a
    // spectacular way to lose the error you were trying to read.
    let mut end = MAX_VALUE;
    while end > 0 && !raw.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… [{} more bytes]", &raw[..end], raw.len() - end)
}

/// Walk a JSON value, redacting by key at every depth.
///
/// Recursive because a password is as likely to be at `user.credentials.password`
/// as at the top level, and a request body is arbitrary JSON.
pub fn json(value: &serde_json::Value) -> serde_json::Value {
    use serde_json::Value;

    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, inner)| {
                    if is_secret_key(key) {
                        (key.clone(), Value::String(REDACTED.to_string()))
                    } else {
                        (key.clone(), json(inner))
                    }
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(json).collect()),
        Value::String(text) => Value::String(truncate(text)),
        other => other.clone(),
    }
}

/// The environment, filtered.
///
/// An allowlist rather than a denylist, and the one place this module inverts
/// its own rule. A process environment holds `DATABASE_URL`, `AWS_SECRET_*`,
/// every `*_TOKEN` a deployment injected, and an unknown number of things named
/// after products nobody here has heard of — so guessing which are safe is not
/// a game worth playing. Only these are ever shown.
pub fn environment() -> Vec<(String, String)> {
    const SHOWN: &[&str] = &[
        "APP_ENV",
        "APP_DEBUG",
        "APP_URL",
        "APP_NAME",
        "RUST_LOG",
        "RUST_BACKTRACE",
        "HOSTNAME",
        "KUBERNETES_SERVICE_HOST",
    ];

    SHOWN
        .iter()
        .filter_map(|name| std::env::var(name).ok().map(|v| ((*name).to_string(), truncate(&v))))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json as j;

    #[test]
    fn redacts_the_obvious_ones() {
        for key in ["password", "Authorization", "API_KEY", "csrf_token", "session_id"] {
            assert_eq!(value(key, "hunter2"), REDACTED, "{key} should be redacted");
        }
    }

    #[test]
    fn shows_ordinary_fields() {
        assert_eq!(value("email", "a@b.com"), "a@b.com");
        assert_eq!(value("page", "2"), "2");
    }

    #[test]
    fn a_long_secret_is_never_shown_truncated() {
        let jwt = "e".repeat(2000);
        // Redaction wins over truncation — the first 300 bytes of a JWT are
        // still the header and the payload.
        assert_eq!(value("access_token", &jwt), REDACTED);
    }

    #[test]
    fn truncates_long_ordinary_values() {
        let long = "x".repeat(1000);
        let out = value("bio", &long);
        assert!(out.starts_with(&"x".repeat(300)));
        assert!(out.contains("700 more bytes"));
    }

    #[test]
    fn truncation_does_not_split_a_utf8_sequence() {
        // 'é' is two bytes; a naive slice at 300 would land mid-character.
        let text = "é".repeat(400);
        let out = truncate(&text);
        assert!(out.contains("more bytes"));
        // The point of the test is that it did not panic, and produced valid
        // UTF-8 — which it must have, to be a String at all.
    }

    #[test]
    fn redacts_nested_json_at_every_depth() {
        let body = j!({
            "email": "a@b.com",
            // `profile` is not a secret key, so the walk has to descend into
            // it and find the one below.
            "profile": { "display_name": "ely", "password": "hunter2" },
            "items": [ { "card": "4111111111111111" } ]
        });
        let out = json(&body);

        assert_eq!(out["email"], "a@b.com");
        assert_eq!(out["profile"]["display_name"], "ely", "an ordinary nested field survives");
        assert_eq!(out["profile"]["password"], REDACTED, "a secret two levels down");
        assert_eq!(out["items"][0]["card"], REDACTED, "inside an array");
    }

    #[test]
    fn a_secret_container_is_redacted_whole() {
        // `credentials` matches the denylist itself, so the entire subtree
        // under it goes — not just the leaves that happen to look secret.
        // That is the safer reading of an object called `credentials`, and
        // it is deliberate: whatever is in there, we do not want it.
        let body = j!({ "user": { "credentials": { "password": "x", "note": "harmless" } } });
        let out = json(&body);

        assert_eq!(out["user"]["credentials"], REDACTED);
        assert!(
            !serde_json::to_string(&out).unwrap().contains("harmless"),
            "nothing under a secret key survives, not even the innocuous parts"
        );
    }

    #[test]
    fn the_environment_is_an_allowlist() {
        std::env::set_var("SOME_SECRET_THING", "nope");
        std::env::set_var("APP_ENV", "local");
        let env = environment();
        assert!(env.iter().any(|(k, _)| k == "APP_ENV"));
        assert!(!env.iter().any(|(k, _)| k == "SOME_SECRET_THING"));
    }
}
