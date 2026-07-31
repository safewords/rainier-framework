//! Signed URLs — [`UrlSigner`].
//!
//! ```ignore
//! let link = signer.sign("/unsubscribe?user=42")?;
//! // /unsubscribe?user=42&signature=9f3c…
//!
//! let link = signer.sign_until("/verify-email?user=42", expires_at)?;
//! // /verify-email?user=42&expires=1790000000&signature=9f3c…
//! ```
//!
//! A link that proves the application produced it, so following it needs no
//! session and no database row. That is the whole trick: an unsubscribe link
//! in an email, an email-verification link, a one-time download — none of them
//! need a token table, a lookup, or a sweep job, because the URL *is* the
//! token and it verifies itself.
//!
//! # What is signed
//!
//! The **path and the query**, with parameters sorted and `signature` removed.
//! Sorting is what makes the signature independent of the order a client
//! happens to send them in; removing `signature` is obvious in hindsight and
//! is the first thing everybody gets wrong.
//!
//! The **host is not signed**. The key is the boundary here, not the hostname:
//! two deployments holding the same `APP_KEY` are the same application by
//! definition, and signing the host would break every link the moment a
//! request arrives through a proxy that rewrote it. If you need a link to work
//! on exactly one hostname, put the hostname in the query — then it is signed
//! like everything else, and your handler can check it.
//!
//! # What a signature is not
//!
//! **It is not single-use.** Anyone holding the link can follow it as many
//! times as they like until it expires, and a link in an email lives in that
//! mailbox forever. For anything that must happen once — accepting an
//! invitation, changing an address — the signature proves *this application
//! issued it* and something stateful still has to prove *it has not been used*.
//!
//! **It is not secret.** The query is in plain sight, in the address bar, in
//! the referrer header, and in whatever logs the URL. Sign an id; do not sign
//! anything you would mind reading over somebody's shoulder.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use rainier_support::{Error, Result};

use crate::key::KeyRing;
use crate::signer::HmacSigner;

/// The query parameter holding the signature.
const SIGNATURE: &str = "signature";

/// The query parameter holding the expiry, in seconds since the epoch.
const EXPIRES: &str = "expires";

/// Signs and verifies URLs.
pub struct UrlSigner {
    signer: HmacSigner,
}

impl UrlSigner {
    /// A signer over `keys`.
    ///
    /// The same ring the rest of the application encrypts with, so rotating a
    /// key retires the links it signed — which is the correct behaviour and
    /// worth knowing before rotating one: every outstanding verification email
    /// stops working.
    pub fn new(keys: KeyRing) -> Self {
        Self { signer: HmacSigner::new(keys) }
    }

    /// Sign `url`, which may be a path or an absolute URL.
    ///
    /// Returns it with `signature` appended.
    pub fn sign(&self, url: &str) -> Result<String> {
        self.attach(url, None)
    }

    /// Sign `url` so that it stops verifying after `expires_at`.
    ///
    /// `expires_at` is seconds since the epoch. It goes into the query — and
    /// is therefore signed, so it cannot be moved without invalidating the
    /// link.
    pub fn sign_until(&self, url: &str, expires_at: i64) -> Result<String> {
        self.attach(url, Some(expires_at))
    }

    /// Whether `url` carries a signature this application produced, and has
    /// not expired.
    ///
    /// # Errors
    ///
    /// Distinguishes the two cases a caller wants to tell apart: a link that
    /// was never valid, and one that was and is not any more. Somebody with an
    /// expired verification email should be offered a new one; somebody with a
    /// forged link should not.
    pub fn verify(&self, url: &str) -> Result<()> {
        let (path, query) = split(url);

        let Some(provided) = value_of(query, SIGNATURE) else {
            return Err(Error::unauthorized("This link is not signed."));
        };

        // Constant time, and against the key the tag itself names — so a link
        // signed before a rotation still verifies while the old key is on the
        // ring.
        if !self.verify_tag(&canonical(path, query), &provided) {
            return Err(Error::unauthorized("This link's signature is not valid."));
        }

        // Checked **after** the signature, so a forged link with a past expiry
        // is reported as forged rather than as expired.
        if let Some(expires_at) = value_of(query, EXPIRES) {
            let expires_at: i64 = expires_at
                .parse()
                .map_err(|_| Error::unauthorized("This link's expiry is not valid."))?;

            if now() > expires_at {
                return Err(Error::unauthorized("This link has expired."));
            }
        }

        Ok(())
    }

    /// Whether `url` verifies, as a boolean.
    pub fn is_valid(&self, url: &str) -> bool {
        self.verify(url).is_ok()
    }

    /// The signature for a URL, without attaching it.
    ///
    /// For a caller building the link some other way — a redirect, a template
    /// — that still wants the same tag.
    pub fn signature_for(&self, url: &str) -> Result<String> {
        let (path, query) = split(url);
        self.tag(&canonical(path, query))
    }

    fn attach(&self, url: &str, expires_at: Option<i64>) -> Result<String> {
        let (path, query) = split(url);

        // Any signature already on it is replaced rather than added to, so
        // signing twice is idempotent instead of producing a link with two.
        let mut pairs: Vec<(String, String)> = parse(query)
            .into_iter()
            .filter(|(name, _)| name != SIGNATURE && name != EXPIRES)
            .collect();

        if let Some(expires_at) = expires_at {
            pairs.push((EXPIRES.to_string(), expires_at.to_string()));
        }

        let query = encode(&pairs);
        let signature = self.tag(&canonical(path, &query))?;

        let separator = if query.is_empty() { "" } else { "&" };
        Ok(format!("{path}?{query}{separator}{SIGNATURE}={signature}"))
    }

    /// The tag for an already-canonical string, in a form a query can hold.
    ///
    /// Base64url over `<kid>.<tag>`, because the key id may contain characters
    /// a query would have to escape and a signature that needs escaping is a
    /// signature somebody will mangle.
    fn tag(&self, canonical: &str) -> Result<String> {
        Ok(URL_SAFE_NO_PAD.encode(self.signer.detached_tag(canonical)?.as_bytes()))
    }

    /// The inverse of [`tag`](Self::tag).
    fn verify_tag(&self, canonical: &str, presented: &str) -> bool {
        let Ok(decoded) = URL_SAFE_NO_PAD.decode(presented) else {
            return false;
        };
        let Ok(decoded) = String::from_utf8(decoded) else {
            return false;
        };

        self.signer.verify_detached(canonical, &decoded)
    }
}

impl std::fmt::Debug for UrlSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("UrlSigner")
    }
}

/// Split a URL into everything before the query and the query itself.
fn split(url: &str) -> (&str, &str) {
    match url.split_once('?') {
        Some((path, query)) => (path, query),
        None => (url, ""),
    }
}

/// The path of a URL, with any scheme and authority removed.
///
/// `https://app.example.com/verify` becomes `/verify`, so signing an absolute
/// link and verifying the request's path agree about what was signed — which
/// is what makes an emailed link work when it arrives at a server that only
/// ever sees the path.
fn path_of(url: &str) -> &str {
    let Some(after_scheme) = url.split_once("://").map(|(_, rest)| rest) else {
        return url;
    };

    match after_scheme.find('/') {
        Some(start) => &after_scheme[start..],
        // `https://example.com` with no path at all.
        None => "/",
    }
}

/// The string that actually gets signed: the path, then the query sorted by
/// name with `signature` removed.
fn canonical(path: &str, query: &str) -> String {
    let path = path_of(path);

    let mut pairs: Vec<(String, String)> =
        parse(query).into_iter().filter(|(name, _)| name != SIGNATURE).collect();

    // By name, then by value, so repeated parameters have one canonical order
    // rather than whichever the client sent.
    pairs.sort();

    format!("{path}?{}", encode(&pairs))
}

/// `a=1&b=2` into pairs, percent-decoded.
fn parse(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((name, value)) => (decode(name), decode(value)),
            None => (decode(pair), String::new()),
        })
        .collect()
}

/// Pairs back into `a=1&b=2`, percent-encoded.
fn encode(pairs: &[(String, String)]) -> String {
    pairs
        .iter()
        .map(|(name, value)| format!("{}={}", percent_encode(name), percent_encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Percent-encode everything outside the unreserved set.
///
/// Deliberately strict: encoding more than strictly necessary is always safe,
/// and the alternative is arguing about which delimiters a particular parser
/// treats as significant.
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());

    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Reverse [`percent_encode`], leaving anything malformed as it was.
fn decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;

    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                match u8::from_str_radix(&value[i + 1..i + 3], 16) {
                    Ok(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    // Not a valid escape, so it is a literal `%`.
                    Err(_) => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }

    String::from_utf8_lossy(&out).into_owned()
}

/// The value of `name` in a query, if it is there.
fn value_of(query: &str, name: &str) -> Option<String> {
    parse(query).into_iter().find(|(found, _)| found == name).map(|(_, value)| value)
}

/// Seconds since the epoch.
fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::key::Key;

    fn signer() -> UrlSigner {
        UrlSigner::new(KeyRing::new(Key::generate()))
    }

    fn in_an_hour() -> i64 {
        now() + 3600
    }

    #[test]
    fn a_signed_url_verifies() {
        let signer = signer();
        let signed = signer.sign("/unsubscribe?user=42").unwrap();

        assert!(signed.contains("signature="));
        assert!(signer.is_valid(&signed));
    }

    #[test]
    fn changing_anything_at_all_invalidates_it() {
        let signer = signer();
        let signed = signer.sign("/unsubscribe?user=42").unwrap();

        // The one that matters: a different id on somebody else's link.
        assert!(!signer.is_valid(&signed.replace("user=42", "user=43")));
        assert!(!signer.is_valid(&signed.replace("/unsubscribe", "/delete-account")));
        assert!(!signer.is_valid(&format!("{signed}&admin=1")));
    }

    #[test]
    fn a_link_from_another_key_does_not_verify() {
        let signed = signer().sign("/unsubscribe?user=42").unwrap();

        assert!(!signer().is_valid(&signed), "a different application signed this");
    }

    #[test]
    fn an_unsigned_url_is_refused_rather_than_accepted() {
        let error = signer().verify("/unsubscribe?user=42").unwrap_err();

        assert!(error.message().contains("not signed"), "{}", error.message());
    }

    #[test]
    fn the_order_of_the_query_does_not_matter() {
        // A client, a proxy or a mail client may reorder them, and the link
        // has to keep working.
        let signer = signer();
        let signed = signer.sign("/report?to=ada&from=grace").unwrap();

        let signature = value_of(split(&signed).1, SIGNATURE).unwrap();
        let reordered = format!("/report?from=grace&to=ada&signature={signature}");

        assert!(signer.is_valid(&reordered));
    }

    #[test]
    fn a_url_with_no_query_signs_and_verifies() {
        let signer = signer();
        let signed = signer.sign("/newsletter/unsubscribe").unwrap();

        assert!(signer.is_valid(&signed));
    }

    #[test]
    fn signing_twice_replaces_the_signature_rather_than_adding_one() {
        let signer = signer();
        let once = signer.sign("/unsubscribe?user=42").unwrap();
        let twice = signer.sign(&once).unwrap();

        assert_eq!(once, twice);
        assert_eq!(twice.matches("signature=").count(), 1);
        assert!(signer.is_valid(&twice));
    }

    #[test]
    fn an_expiring_link_verifies_until_it_does_not() {
        let signer = signer();

        let live = signer.sign_until("/verify?user=42", in_an_hour()).unwrap();
        assert!(signer.is_valid(&live));

        let dead = signer.sign_until("/verify?user=42", now() - 1).unwrap();
        let error = signer.verify(&dead).unwrap_err();
        assert!(error.message().contains("expired"), "{}", error.message());
    }

    #[test]
    fn the_expiry_cannot_be_moved() {
        // It is in the query, so it is signed. Extending it breaks the tag.
        let signer = signer();
        let signed = signer.sign_until("/verify?user=42", now() - 1).unwrap();

        let extended = signed.replace(&format!("expires={}", now() - 1), "expires=99999999999");

        let error = signer.verify(&extended).unwrap_err();
        assert!(error.message().contains("not valid"), "{}", error.message());
    }

    #[test]
    fn a_forged_link_with_a_past_expiry_reads_as_forged_rather_than_expired() {
        // Reporting "expired" would confirm the rest of the URL was right,
        // which is a hint worth not giving.
        let signer = signer();
        let forged = format!("/verify?user=42&expires={}&signature=deadbeef", now() - 1);

        let error = signer.verify(&forged).unwrap_err();
        assert!(error.message().contains("not valid"), "{}", error.message());
    }

    #[test]
    fn a_signature_of_the_wrong_length_is_refused_without_panicking() {
        let signer = signer();

        for forged in ["", "x", "not-base64-at-all-!!!", &"a".repeat(500)] {
            assert!(!signer.is_valid(&format!("/verify?user=42&signature={forged}")));
        }
    }

    #[test]
    fn a_value_with_delimiters_in_it_survives_the_round_trip() {
        // An email address, which is the value these links carry most often,
        // and which contains characters a naive splitter mangles.
        let signer = signer();
        let signed = signer.sign("/verify?email=ada%2Blovelace%40example.com&note=a%26b").unwrap();

        assert!(signer.is_valid(&signed));
        assert!(!signer.is_valid(&signed.replace("ada%2B", "eve%2B")));
    }

    #[test]
    fn the_canonical_form_drops_the_signature_and_sorts() {
        assert_eq!(canonical("/x", "b=2&a=1&signature=zzz"), "/x?a=1&b=2");
        assert_eq!(canonical("/x", ""), "/x?");
    }

    #[test]
    fn percent_coding_round_trips() {
        for value in ["ada@example.com", "a b", "a&b=c", "100%", "é", ""] {
            assert_eq!(decode(&percent_encode(value)), value, "{value:?}");
        }
    }

    #[test]
    fn a_link_signed_before_a_rotation_still_verifies() {
        // Retiring a key invalidates its links, which is correct — but the
        // *previous* key staying on the ring is how an application rotates
        // without invalidating every verification email in flight.
        let old = Key::generate();
        let signed = UrlSigner::new(KeyRing::new(old.clone())).sign("/verify?user=42").unwrap();

        let rotated = UrlSigner::new(KeyRing::new(Key::generate()).with_previous(old));
        assert!(rotated.is_valid(&signed));

        // And once it is off the ring, it does not.
        let without = UrlSigner::new(KeyRing::new(Key::generate()));
        assert!(!without.is_valid(&signed));
    }

    #[test]
    fn an_absolute_link_and_its_path_share_a_signature() {
        // The property that makes an emailed link work: it is signed with the
        // host on it, and verified by a server that only sees the path.
        let signer = signer();

        let absolute = signer.sign("https://app.example.com/verify?user=42").unwrap();
        let path = absolute.trim_start_matches("https://app.example.com");

        assert!(signer.is_valid(path));
        assert!(signer.is_valid(&absolute));
    }

    #[test]
    fn the_host_is_deliberately_not_covered() {
        // Stated in the docs, so asserted here: the same link verifies on any
        // hostname the application answers to. The key is the boundary.
        let signer = signer();
        let signed = signer.sign("https://app.example.com/verify?user=42").unwrap();

        assert!(signer.is_valid(&signed.replace("app.example.com", "staging.example.com")));
    }

    #[test]
    fn a_url_with_a_host_and_no_path_still_signs() {
        let signer = signer();
        let signed = signer.sign("https://app.example.com").unwrap();

        assert!(signer.is_valid(&signed));
    }

    #[test]
    fn the_authority_is_stripped_from_the_canonical_form() {
        assert_eq!(path_of("https://app.example.com/verify"), "/verify");
        assert_eq!(path_of("http://localhost:8000/a/b"), "/a/b");
        assert_eq!(path_of("https://app.example.com"), "/");
        assert_eq!(path_of("/verify"), "/verify");
        assert_eq!(path_of(""), "");
    }
}
