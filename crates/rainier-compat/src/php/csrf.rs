//! Anti-forgery tokens a PHP application issued — [`EncryptedXsrf`].
//!
//! ```ignore
//! use rainier_session::VerifyCsrfToken;
//!
//! let csrf = VerifyCsrfToken::new()
//!     .codec(Arc::new(EncryptedXsrf::new(cookies.clone())));
//! ```
//!
//! Rainier's own [`VerifyCsrfToken`](rainier_session::VerifyCsrfToken) does
//! the checking. What differs beside a PHP application is two details, and
//! both are covered without a second middleware:
//!
//! - **The token.** It is the session's `_token` under the same name in both,
//!   so a session migrated by [`ForeignSessionStore`](super::session::ForeignSessionStore)
//!   carries it across and a form the PHP side rendered still posts.
//! - **The `XSRF-TOKEN` cookie.** The PHP side seals it like every other
//!   cookie, with the name-binding prefix inside. A front end copies the
//!   sealed value into `X-XSRF-TOKEN` verbatim, so the header has to be
//!   opened the same way. That is this codec.
//!
//! Opening drops the prefix **without** verifying it, as the PHP side does
//! for this header: the token inside is compared against the session next,
//! and that comparison is the check.

use rainier_session::XsrfCodec;
use rainier_support::Result;

use super::cookie::{percent_decoded, EncryptedCookies};

/// The `XSRF-TOKEN` cookie, sealed the way the PHP application seals it.
#[derive(Debug, Clone)]
pub struct EncryptedXsrf {
    cookies: EncryptedCookies,
}

impl EncryptedXsrf {
    /// Seal and open with `cookies`.
    pub fn new(cookies: EncryptedCookies) -> Self {
        Self { cookies }
    }
}

impl XsrfCodec for EncryptedXsrf {
    fn seal(&self, token: &str) -> Result<String> {
        self.cookies.encrypt(rainier_session::csrf::XSRF_COOKIE, token)
    }

    fn open(&self, header: &str) -> Option<String> {
        self.cookies.decrypt_unverified(&percent_decoded(header)).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_crypt::{Key, KeyRing};

    fn codec() -> EncryptedXsrf {
        EncryptedXsrf::new(EncryptedCookies::new(KeyRing::new(Key::from_bytes([3; 32]))))
    }

    #[test]
    fn a_sealed_token_opens_to_itself() {
        let codec = codec();
        let sealed = codec.seal("tok3n").unwrap();
        assert_ne!(sealed, "tok3n");
        assert_eq!(codec.open(&sealed).as_deref(), Some("tok3n"));
    }

    #[test]
    fn a_header_a_script_percent_encoded_still_opens() {
        let codec = codec();
        let sealed = codec.seal("tok3n").unwrap().replace('=', "%3D");
        assert_eq!(codec.open(&sealed).as_deref(), Some("tok3n"));
    }

    #[test]
    fn a_header_sealed_under_another_key_opens_to_nothing() {
        let other =
            EncryptedXsrf::new(EncryptedCookies::new(KeyRing::new(Key::from_bytes([4; 32]))));
        assert_eq!(codec().open(&other.seal("tok3n").unwrap()), None);
        assert_eq!(codec().open("plain-token"), None);
    }
}
