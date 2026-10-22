//! Cookies a PHP application encrypted — [`EncryptedCookies`].
//!
//! ```
//! use rainier_compat::php::cookie::EncryptedCookies;
//! use rainier_crypt::{Key, KeyRing};
//!
//! let cookies = EncryptedCookies::new(KeyRing::new(Key::generate()));
//!
//! let sealed = cookies.encrypt("app_session", "aBcD1234").unwrap();
//! assert_eq!(cookies.decrypt("app_session", &sealed).unwrap(), "aBcD1234");
//!
//! // The same ciphertext presented under another cookie's name is refused.
//! assert!(cookies.decrypt("remember_me", &sealed).is_err());
//! ```
//!
//! # The format
//!
//! A cookie value is the PHP encrypter's envelope (see
//! [`rainier_crypt::PhpEncrypter`]) around
//!
//! ```text
//! hex(hmac_sha1(cookie_name + "v2", key)) + "|" + value
//! ```
//!
//! — the plaintext carries a **prefix binding it to its cookie's name**. It
//! exists because without it, every encrypted cookie is interchangeable: a
//! client could take the ciphertext of a cookie whose value it controls and
//! present it as the session cookie. The prefix is 40 hex digits and a bar,
//! 41 bytes, and the key is the raw application key — each key on the ring
//! in turn, so a rotation keeps reading cookies the previous key wrote.
//!
//! Reading **verifies** the prefix rather than cutting 41 bytes off and
//! hoping, so a cookie moved between names is refused here even where the
//! PHP side would have only stripped it.
//!
//! # Not serialized
//!
//! The encrypted value is the cookie's string itself. A PHP application can be
//! configured to `serialize()` cookie values before encrypting them, which
//! modern ones do not; [`decrypt_serialized`](EncryptedCookies::decrypt_serialized)
//! reads that older shape.

use std::sync::Arc;

use hmac::{Hmac, KeyInit, Mac};
use rainier_crypt::{Encrypter, EncrypterExt, Key, KeyRing, PhpEncrypter};
use rainier_support::{Error, Result};
use sha1::Sha1;
use subtle::ConstantTimeEq;

use super::serialize::{unserialize, Value};

type HmacSha1 = Hmac<Sha1>;

/// Length of the name-binding prefix: 40 hex digits and a `|`.
pub const PREFIX_LEN: usize = 41;

/// Encrypts and decrypts cookies in the PHP application's format.
#[derive(Clone)]
pub struct EncryptedCookies {
    encrypter: Arc<PhpEncrypter>,
}

impl EncryptedCookies {
    /// Cookies sealed with `keys` — the application key current first, then
    /// any previous ones still being read.
    pub fn new(keys: KeyRing) -> Self {
        Self { encrypter: Arc::new(PhpEncrypter::new(keys)) }
    }

    /// Share an encrypter that is already configured — the AES-256-GCM
    /// variant, say.
    pub fn with_encrypter(encrypter: Arc<PhpEncrypter>) -> Self {
        Self { encrypter }
    }

    /// The encrypter underneath.
    pub fn encrypter(&self) -> &Arc<PhpEncrypter> {
        &self.encrypter
    }

    /// The prefix binding a value to `name` under `key`.
    pub fn prefix(name: &str, key: &Key) -> String {
        let mut mac =
            HmacSha1::new_from_slice(key.bytes()).expect("HMAC takes a key of any length");
        mac.update(name.as_bytes());
        mac.update(b"v2");
        let digest = mac.finalize().into_bytes();
        let mut prefix: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        prefix.push('|');
        prefix
    }

    /// Seal `value` as the cookie `name`, under the current key.
    pub fn encrypt(&self, name: &str, value: &str) -> Result<String> {
        let prefix = Self::prefix(name, self.encrypter.keys().current());
        self.encrypter.encrypt(&format!("{prefix}{value}"))
    }

    /// Open the cookie `name`, verifying it was sealed **as** `name`.
    ///
    /// One error for every failure — wrong key, tampering, a moved cookie — so
    /// a client probing values learns nothing from which it was.
    pub fn decrypt(&self, name: &str, sealed: &str) -> Result<String> {
        let plain = self.open(sealed)?;
        self.strip_verified(name, &plain).ok_or_else(invalid)
    }

    /// Open a cookie whose value was `serialize()`d before it was encrypted.
    pub fn decrypt_serialized(&self, name: &str, sealed: &str) -> Result<Value> {
        let plain = self.open(sealed)?;
        let value = unserialize(plain.as_bytes()).map_err(|_| invalid())?;
        let text = value.as_str().ok_or_else(invalid)?;
        let stripped = self.strip_verified(name, text).ok_or_else(invalid)?;
        Ok(Value::string(stripped))
    }

    /// Open a value and drop the first 41 bytes **without** checking them.
    ///
    /// Only for the anti-forgery header, which is the one place the PHP side
    /// itself does not verify the prefix: the header holds the token cookie
    /// copied by the page's JavaScript, and the token inside is compared
    /// against the session afterwards, which is the real check.
    pub fn decrypt_unverified(&self, sealed: &str) -> Result<String> {
        let plain = self.open(sealed)?;
        plain.get(PREFIX_LEN..).map(str::to_string).ok_or_else(invalid)
    }

    fn open(&self, sealed: &str) -> Result<String> {
        let sealed = percent_decoded(sealed);
        let bytes = self.encrypter.decrypt_bytes(&sealed).map_err(|_| invalid())?;
        String::from_utf8(bytes).map_err(|_| invalid())
    }

    fn strip_verified(&self, name: &str, plain: &str) -> Option<String> {
        let presented = plain.get(..PREFIX_LEN)?;
        let matched =
            self.encrypter.keys().all().any(|key| {
                bool::from(Self::prefix(name, key).as_bytes().ct_eq(presented.as_bytes()))
            });
        matched.then(|| plain[PREFIX_LEN..].to_string())
    }
}

impl std::fmt::Debug for EncryptedCookies {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncryptedCookies").finish_non_exhaustive()
    }
}

fn invalid() -> Error {
    Error::bad_request("the cookie is not one this application sealed")
}

/// A value as a browser or script may have percent-encoded it. The envelope
/// is base64 with `=` padding, which is exactly what gets encoded on the way
/// out; decoding is a no-op on anything without a `%`.
pub(crate) fn percent_decoded(value: &str) -> String {
    if !value.contains('%') {
        return value.to_string();
    }
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let digits = &bytes[i + 1..i + 3];
            // Both digits checked: `from_str_radix` would accept a sign.
            let hex = std::str::from_utf8(digits)
                .ok()
                .filter(|_| digits.iter().all(u8::is_ascii_hexdigit));
            if let Some(byte) = hex.and_then(|hex| u8::from_str_radix(hex, 16).ok()) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_crypt::PhpCipher;

    fn key(byte: u8) -> Key {
        Key::from_bytes([byte; 32])
    }

    fn cookies() -> EncryptedCookies {
        EncryptedCookies::new(KeyRing::new(key(7)))
    }

    #[test]
    fn the_prefix_is_hmac_sha1_of_the_name_and_v2_under_the_raw_key() {
        // Independent computation: HMAC-SHA1 by hand over the documented input.
        let prefix = EncryptedCookies::prefix("app_session", &key(7));
        assert_eq!(prefix.len(), PREFIX_LEN);
        assert!(prefix.ends_with('|'));

        let mut mac = HmacSha1::new_from_slice(&[7u8; 32]).unwrap();
        mac.update(b"app_sessionv2");
        let expected: String =
            mac.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(&prefix[..40], expected);
    }

    #[test]
    fn a_known_hmac_sha1_vector_matches() {
        // RFC 2202 test case 2: key "Jefe", data "what do ya want for nothing?".
        let mut mac = HmacSha1::new_from_slice(b"Jefe").unwrap();
        mac.update(b"what do ya want for nothing?");
        let hex: String = mac.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, "effcdf6ae5eb2fa2d27416d5f184df9c259a7c79");
    }

    #[test]
    fn a_sealed_cookie_opens_under_its_own_name() {
        let cookies = cookies();
        let sealed = cookies.encrypt("app_session", "Q2h2Rk9pTjN3YnZ1").unwrap();
        assert_eq!(cookies.decrypt("app_session", &sealed).unwrap(), "Q2h2Rk9pTjN3YnZ1");
    }

    #[test]
    fn a_cookie_moved_to_another_name_is_refused() {
        let cookies = cookies();
        let sealed = cookies.encrypt("theme", "dark").unwrap();
        assert!(cookies.decrypt("app_session", &sealed).is_err());
    }

    #[test]
    fn a_tampered_prefix_is_refused_even_with_a_valid_envelope() {
        let cookies = cookies();
        let forged = cookies.encrypter().encrypt(&format!("{}|abc", "0".repeat(40))).unwrap();
        assert!(cookies.decrypt("app_session", &forged).is_err());
        // ...though the unverified path, which the header uses, strips it.
        assert_eq!(cookies.decrypt_unverified(&forged).unwrap(), "abc");
    }

    #[test]
    fn the_wrong_key_is_refused_and_a_previous_key_still_reads() {
        let old = EncryptedCookies::new(KeyRing::new(key(1)));
        let sealed = old.encrypt("app_session", "value").unwrap();

        assert!(cookies().decrypt("app_session", &sealed).is_err());

        let rotated = EncryptedCookies::new(KeyRing::new(key(2)).with_previous(key(1)));
        assert_eq!(rotated.decrypt("app_session", &sealed).unwrap(), "value");
    }

    #[test]
    fn a_gcm_configured_application_reads_too() {
        let gcm = EncryptedCookies::with_encrypter(Arc::new(
            PhpEncrypter::new(KeyRing::new(key(7))).writing(PhpCipher::Aes256Gcm),
        ));
        let sealed = gcm.encrypt("app_session", "value").unwrap();
        assert_eq!(cookies().decrypt("app_session", &sealed).unwrap(), "value");
    }

    #[test]
    fn a_percent_encoded_value_is_decoded_first() {
        let cookies = cookies();
        let sealed = cookies.encrypt("app_session", "value").unwrap();
        let encoded = sealed.replace('=', "%3D").replace('+', "%2B").replace('/', "%2F");
        assert_eq!(cookies.decrypt("app_session", &encoded).unwrap(), "value");
    }

    #[test]
    fn a_serialized_cookie_value_reads() {
        let cookies = cookies();
        let prefixed = format!("{}{}", EncryptedCookies::prefix("old", &key(7)), "v");
        let serialized = super::super::serialize::serialize(&Value::string(&prefixed));
        let sealed = cookies.encrypter().encrypt_bytes(&serialized).unwrap();
        assert_eq!(cookies.decrypt_serialized("old", &sealed).unwrap(), Value::string("v"));
    }

    #[test]
    fn garbage_is_one_error() {
        let cookies = cookies();
        for junk in ["", "%", "not-base64", "eyJpdiI6IiJ9", "%ZZ"] {
            assert!(cookies.decrypt("app_session", junk).is_err(), "{junk}");
        }
    }

    #[test]
    fn percent_decoding_leaves_odd_input_alone() {
        assert_eq!(percent_decoded("a%3Db"), "a=b");
        assert_eq!(percent_decoded("100%"), "100%");
        assert_eq!(percent_decoded("%G1x"), "%G1x");
        assert_eq!(percent_decoded("%4"), "%4");
    }
}
