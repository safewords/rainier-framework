//! Cookies — [`Cookie`], [`SameSite`] and the `Cookie:` / `Set-Cookie:`
//! encodings.

use std::collections::HashMap;
use std::fmt::Write as _;

use chrono::{DateTime, Utc};
use percent_encoding::{percent_decode_str, utf8_percent_encode, AsciiSet, CONTROLS};

/// Characters escaped in cookie values.
///
/// The RFC 6265 grammar forbids control characters, whitespace, quotes,
/// commas, semicolons and backslashes in a cookie value. Percent-encoding just
/// those keeps ordinary values (including JSON-ish ones) readable in a browser
/// inspector, rather than turning every value into an unreadable blob.
const COOKIE_VALUE: &AsciiSet =
    &CONTROLS.add(b' ').add(b'"').add(b',').add(b';').add(b'\\').add(b'%');

/// A cookie's cross-site policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SameSite {
    /// Sent on same-site requests and top-level cross-site navigations. The
    /// default, and the right choice for a session cookie.
    #[default]
    Lax,
    /// Never sent cross-site.
    Strict,
    /// Always sent. Browsers reject this unless `Secure` is also set, so
    /// [`Cookie::same_site`] turns `Secure` on with it.
    None,
}

impl SameSite {
    fn as_str(self) -> &'static str {
        match self {
            SameSite::Lax => "Lax",
            SameSite::Strict => "Strict",
            SameSite::None => "None",
        }
    }
}

/// A cookie to send, or one that arrived.
#[derive(Debug, Clone, PartialEq)]
pub struct Cookie {
    name: String,
    value: String,
    path: Option<String>,
    domain: Option<String>,
    max_age: Option<i64>,
    expires: Option<DateTime<Utc>>,
    secure: bool,
    http_only: bool,
    same_site: Option<SameSite>,
}

impl Cookie {
    /// A cookie with sensible defaults: path `/`, `HttpOnly`, `SameSite=Lax`.
    ///
    /// Those defaults are the safe ones rather than the empty ones. A cookie
    /// this framework sets is a session or CSRF cookie until proven otherwise,
    /// and both want to be invisible to scripts and not sent cross-site. Opt
    /// out explicitly with [`http_only(false)`](Self::http_only).
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
            path: Some("/".to_string()),
            domain: None,
            max_age: None,
            expires: None,
            secure: false,
            http_only: true,
            same_site: Some(SameSite::Lax),
        }
    }

    /// A cookie that instructs the browser to delete the named one.
    pub fn removal(name: impl Into<String>) -> Self {
        Self::new(name, "").max_age(0).expires(DateTime::UNIX_EPOCH)
    }

    /// The cookie's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The cookie's value.
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Restrict the cookie to a path prefix.
    pub fn path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    /// Scope the cookie to a domain (and its subdomains).
    pub fn domain(mut self, domain: impl Into<String>) -> Self {
        self.domain = Some(domain.into());
        self
    }

    /// Expire the cookie after `seconds`. `0` deletes it immediately.
    pub fn max_age(mut self, seconds: i64) -> Self {
        self.max_age = Some(seconds);
        self
    }

    /// Expire the cookie at an absolute time.
    pub fn expires(mut self, at: DateTime<Utc>) -> Self {
        self.expires = Some(at);
        self
    }

    /// Only send the cookie over HTTPS.
    pub fn secure(mut self, secure: bool) -> Self {
        self.secure = secure;
        self
    }

    /// Hide the cookie from JavaScript. On by default.
    pub fn http_only(mut self, http_only: bool) -> Self {
        self.http_only = http_only;
        self
    }

    /// Set the cross-site policy.
    ///
    /// [`SameSite::None`] also forces `Secure`, because every current browser
    /// rejects `SameSite=None` without it — silently, which would be a
    /// miserable thing to debug.
    pub fn same_site(mut self, same_site: SameSite) -> Self {
        if same_site == SameSite::None {
            self.secure = true;
        }
        self.same_site = Some(same_site);
        self
    }

    /// Render the `Set-Cookie` header value.
    pub fn to_set_cookie(&self) -> String {
        let mut out = String::with_capacity(self.name.len() + self.value.len() + 48);
        let _ = write!(
            out,
            "{}={}",
            utf8_percent_encode(&self.name, COOKIE_VALUE),
            utf8_percent_encode(&self.value, COOKIE_VALUE)
        );

        if let Some(path) = &self.path {
            let _ = write!(out, "; Path={path}");
        }
        if let Some(domain) = &self.domain {
            let _ = write!(out, "; Domain={domain}");
        }
        if let Some(max_age) = self.max_age {
            let _ = write!(out, "; Max-Age={max_age}");
        }
        if let Some(expires) = self.expires {
            // RFC 7231 IMF-fixdate, e.g. "Thu, 01 Jan 1970 00:00:00 GMT".
            let _ = write!(out, "; Expires={}", expires.format("%a, %d %b %Y %H:%M:%S GMT"));
        }
        if self.secure {
            out.push_str("; Secure");
        }
        if self.http_only {
            out.push_str("; HttpOnly");
        }
        if let Some(same_site) = self.same_site {
            let _ = write!(out, "; SameSite={}", same_site.as_str());
        }
        out
    }
}

/// Parse a request's `Cookie:` header into name/value pairs.
///
/// Attributes never appear on the request side — a browser sends only
/// `name=value` pairs — so this is deliberately simpler than parsing
/// `Set-Cookie`.
pub fn parse_cookie_header(header: &str) -> HashMap<String, String> {
    let mut cookies = HashMap::new();
    for pair in header.split(';') {
        let pair = pair.trim();
        if pair.is_empty() {
            continue;
        }
        let (name, value) = match pair.split_once('=') {
            Some((name, value)) => (name.trim(), value.trim()),
            // A bare token with no `=` is a valueless cookie; keep it rather
            // than drop data the application might be looking for.
            None => (pair, ""),
        };
        cookies.insert(decode(name), decode(value));
    }
    cookies
}

fn decode(raw: &str) -> String {
    // Quoted-string form: `name="value"`.
    let trimmed = raw.strip_prefix('"').and_then(|r| r.strip_suffix('"')).unwrap_or(raw);
    percent_decode_str(trimmed).decode_utf8_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_the_safe_ones() {
        let rendered = Cookie::new("session", "abc").to_set_cookie();
        assert!(rendered.starts_with("session=abc"));
        assert!(rendered.contains("; Path=/"));
        assert!(rendered.contains("; HttpOnly"));
        assert!(rendered.contains("; SameSite=Lax"));
        assert!(!rendered.contains("Secure"));
    }

    #[test]
    fn attributes_render() {
        let rendered = Cookie::new("a", "b")
            .path("/admin")
            .domain("example.com")
            .max_age(3600)
            .secure(true)
            .http_only(false)
            .same_site(SameSite::Strict)
            .to_set_cookie();

        assert!(rendered.contains("; Path=/admin"));
        assert!(rendered.contains("; Domain=example.com"));
        assert!(rendered.contains("; Max-Age=3600"));
        assert!(rendered.contains("; Secure"));
        assert!(!rendered.contains("HttpOnly"));
        assert!(rendered.contains("; SameSite=Strict"));
    }

    #[test]
    fn same_site_none_forces_secure() {
        let rendered = Cookie::new("a", "b").same_site(SameSite::None).to_set_cookie();
        assert!(rendered.contains("; SameSite=None"));
        assert!(rendered.contains("; Secure"), "browsers drop SameSite=None without Secure");
    }

    #[test]
    fn removal_expires_in_the_past() {
        let rendered = Cookie::removal("session").to_set_cookie();
        assert!(rendered.starts_with("session="));
        assert!(rendered.contains("; Max-Age=0"));
        assert!(rendered.contains("Expires=Thu, 01 Jan 1970 00:00:00 GMT"));
    }

    #[test]
    fn values_needing_escapes_are_encoded() {
        let rendered = Cookie::new("data", "a;b c").to_set_cookie();
        assert!(rendered.starts_with("data=a%3Bb%20c"), "{rendered}");
    }

    #[test]
    fn parses_a_request_cookie_header() {
        let cookies = parse_cookie_header("session=abc; theme=dark; ");
        assert_eq!(cookies.get("session").map(String::as_str), Some("abc"));
        assert_eq!(cookies.get("theme").map(String::as_str), Some("dark"));
        assert_eq!(cookies.len(), 2);
    }

    #[test]
    fn parsing_reverses_the_encoding() {
        let rendered = Cookie::new("data", "a;b c").to_set_cookie();
        let pair = rendered.split(';').next().unwrap();
        assert_eq!(parse_cookie_header(pair).get("data").map(String::as_str), Some("a;b c"));
    }

    #[test]
    fn handles_quoted_and_valueless_cookies() {
        let cookies = parse_cookie_header("quoted=\"hello\"; bare");
        assert_eq!(cookies.get("quoted").map(String::as_str), Some("hello"));
        assert_eq!(cookies.get("bare").map(String::as_str), Some(""));
    }

    #[test]
    fn an_empty_header_yields_nothing() {
        assert!(parse_cookie_header("").is_empty());
        assert!(parse_cookie_header("  ;  ").is_empty());
    }
}
