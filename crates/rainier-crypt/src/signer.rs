//! Message signing — the [`Signer`] port and its HMAC implementation.
//!
//! Signing is what you want when the value is not a secret but its *integrity*
//! is: an unsubscribe link, a password-reset token, a cookie the client is
//! allowed to read but not to edit. Encryption also authenticates, but it
//! hides the value, which makes the result opaque to logs, to support staff,
//! and to you.

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine as _;
use hmac::{Hmac, Mac};
use rainier_support::{Error, Result};
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::key::{Key, KeyRing};

type HmacSha256 = Hmac<Sha256>;

/// Attaches a tamper-evident tag to a value, and checks it.
pub trait Signer: Send + Sync + 'static {
    /// Return `value` with a signature attached.
    fn sign(&self, value: &str) -> Result<String>;

    /// Recover the value from a signed string, or fail if it was altered.
    fn verify(&self, signed: &str) -> Result<String>;
}

/// The conveniences every [`Signer`] gets, kept out of the object-safe trait.
pub trait SignerExt: Signer {
    /// Whether `signed` is intact, without recovering the value.
    fn is_valid(&self, signed: &str) -> bool {
        self.verify(signed).is_ok()
    }
}

impl<S: Signer + ?Sized> SignerExt for S {}

/// HMAC-SHA256 over a [`KeyRing`].
///
/// The signed form is `<value>.<key id>.<tag>`, so a reader can see the value
/// without holding the key — which is the point — while a writer cannot forge
/// one without it.
pub struct HmacSigner {
    keys: KeyRing,
}

impl HmacSigner {
    /// A signer over `keys`.
    pub fn new(keys: KeyRing) -> Self {
        Self { keys }
    }

    /// The key ring.
    pub fn keys(&self) -> &KeyRing {
        &self.keys
    }

    /// A **detached** tag for `value` — `<key id>.<tag>`, without the value.
    ///
    /// [`sign`](Signer::sign) returns `<value>.<kid>.<tag>`, which is right
    /// when the reader needs to see the value and wrong when the value lives
    /// somewhere else already. A signed URL is the second case: the thing
    /// being signed is the URL, the URL is already in the address bar, and it
    /// is full of dots — which `sign` refuses, because it splits on them.
    ///
    /// The key id travels with the tag so a rotated ring can still verify.
    pub fn detached_tag(&self, value: &str) -> Result<String> {
        let key = self.keys.current();
        Ok(format!("{}.{}", key.id(), B64.encode(Self::tag(key, value)?)))
    }

    /// Whether `tag` is a [`detached_tag`](Self::detached_tag) for `value`.
    ///
    /// Verifies against the key the tag names, so a tag written before a
    /// rotation still checks out as long as the old key is still on the ring.
    pub fn verify_detached(&self, value: &str, tag: &str) -> bool {
        let Some((key_id, tag)) = tag.split_once('.') else {
            return false;
        };
        let Some(key) = self.keys.find(key_id) else {
            return false;
        };
        let (Ok(expected), Ok(presented)) = (Self::tag(key, value), B64.decode(tag)) else {
            return false;
        };

        presented.ct_eq(&expected).into()
    }

    /// The tag for `value` under `key`.
    ///
    /// The key id is folded into the tag so a signature cannot be replayed
    /// under a different key's label.
    fn tag(key: &Key, value: &str) -> Result<Vec<u8>> {
        let mut mac = HmacSha256::new_from_slice(key.bytes())
            .map_err(|_| Error::internal("the key is the wrong length for HMAC-SHA256"))?;
        mac.update(key.id().as_bytes());
        mac.update(b".");
        mac.update(value.as_bytes());
        Ok(mac.finalize().into_bytes().to_vec())
    }
}

impl Signer for HmacSigner {
    fn sign(&self, value: &str) -> Result<String> {
        if value.contains('.') {
            // The format is dot-separated and the value comes first, so a dot
            // in it would make the split ambiguous. Callers with structured
            // values should encode them first.
            return Err(Error::internal(
                "a signed value must not contain `.` — encode it first (base64, or JSON \
                 through the encrypter)",
            ));
        }

        let key = self.keys.current();
        Ok(format!("{value}.{}.{}", key.id(), B64.encode(Self::tag(key, value)?)))
    }

    fn verify(&self, signed: &str) -> Result<String> {
        let invalid = || Error::bad_request("the signature is not valid");

        let mut parts = signed.split('.');
        let (Some(value), Some(key_id), Some(tag), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(invalid());
        };

        let key = self.keys.find(key_id).ok_or_else(invalid)?;
        let expected = Self::tag(key, value)?;
        let presented = B64.decode(tag).map_err(|_| invalid())?;

        // Constant time: a byte-by-byte comparison leaks how much of a forged
        // tag was right, which is enough to construct the rest of it.
        if presented.ct_eq(&expected).into() {
            Ok(value.to_string())
        } else {
            Err(invalid())
        }
    }
}

impl std::fmt::Debug for HmacSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HmacSigner").field("keys", &self.keys.ids()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signer() -> HmacSigner {
        HmacSigner::new(KeyRing::new(Key::generate()))
    }

    #[test]
    fn a_value_round_trips() {
        let signer = signer();
        let signed = signer.sign("user-42").unwrap();

        assert_eq!(signer.verify(&signed).unwrap(), "user-42");
    }

    #[test]
    fn the_value_stays_readable() {
        // The whole reason to sign rather than encrypt.
        assert!(signer().sign("unsubscribe-42").unwrap().starts_with("unsubscribe-42."));
    }

    #[test]
    fn altering_the_value_invalidates_it() {
        let signer = signer();
        let signed = signer.sign("user-42").unwrap();
        let forged = signed.replacen("user-42", "user-43", 1);

        assert!(signer.verify(&forged).is_err());
    }

    #[test]
    fn altering_the_tag_invalidates_it() {
        let signer = signer();
        let signed = signer.sign("user-42").unwrap();
        let mut parts: Vec<&str> = signed.split('.').collect();
        let other = B64.encode([0u8; 32]);
        parts[2] = &other;

        assert!(signer.verify(&parts.join(".")).is_err());
    }

    #[test]
    fn another_key_cannot_verify_it() {
        let signed = signer().sign("user-42").unwrap();
        assert!(signer().verify(&signed).is_err());
    }

    #[test]
    fn a_retired_key_still_verifies_what_it_signed() {
        let old = Key::generate();
        let signed = HmacSigner::new(KeyRing::new(old.clone())).sign("user-42").unwrap();

        let rotated = HmacSigner::new(KeyRing::new(Key::generate()).with_previous(old));
        assert_eq!(rotated.verify(&signed).unwrap(), "user-42");
    }

    #[test]
    fn a_signature_cannot_be_replayed_under_another_key_id() {
        let old = Key::generate();
        let signer = HmacSigner::new(KeyRing::new(Key::generate()).with_previous(old.clone()));

        let signed = signer.sign("user-42").unwrap();
        let mut parts: Vec<&str> = signed.split('.').collect();
        parts[1] = old.id();

        assert!(signer.verify(&parts.join(".")).is_err(), "the id is folded into the tag");
    }

    #[test]
    fn a_malformed_signature_is_a_400() {
        let signer = signer();

        for rubbish in ["", "nope", "a.b", "a.b.c.d"] {
            assert_eq!(signer.verify(rubbish).unwrap_err().status(), 400, "{rubbish}");
        }
    }

    #[test]
    fn a_value_containing_a_dot_is_refused_rather_than_mangled() {
        let err = signer().sign("a.b").unwrap_err();
        assert!(err.message().contains("encode it first"), "{}", err.message());
    }

    #[test]
    fn is_valid_answers_without_the_value() {
        let signer = signer();
        let signed = signer.sign("x").unwrap();

        assert!(signer.is_valid(&signed));
        assert!(!signer.is_valid("x.y.z"));
    }

    #[test]
    fn the_port_is_object_safe() {
        let signer: std::sync::Arc<dyn Signer> = std::sync::Arc::new(signer());
        assert_eq!(signer.verify(&signer.sign("via dyn").unwrap()).unwrap(), "via dyn");
    }
}
