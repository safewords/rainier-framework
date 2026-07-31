//! # rainier-crypt
//!
//! Reversible encryption and message signing — one `Crypt` value covering
//! both halves, rather than signing being spread across signed URLs and
//! cookie integrity.
//!
//! ```
//! use rainier_crypt::{Encryption, Key, KeyRing};
//!
//! # fn main() -> rainier_support::Result<()> {
//! let crypt = Encryption::from_keys(KeyRing::new(Key::generate()));
//!
//! let sealed = crypt.encrypt("a card number")?;
//! assert_eq!(crypt.decrypt(&sealed)?, "a card number");
//!
//! // Signed, not sealed: the value stays readable, but not writable.
//! let signed = crypt.sign("unsubscribe-42")?;
//! assert!(signed.starts_with("unsubscribe-42."));
//! assert_eq!(crypt.verify(&signed)?, "unsubscribe-42");
//! # Ok(()) }
//! ```
//!
//! ## Encrypt or sign?
//!
//! | | Hidden | Tamper-evident | Use for |
//! |---|---|---|---|
//! | [`encrypt`](Encryption::encrypt) | yes | yes | anything the client must not read |
//! | [`sign`](Encryption::sign) | no | yes | anything the client may read but not change |
//!
//! Reach for signing when the value is not a secret. An unsubscribe link, a
//! reset token, a "remember this choice" cookie — encrypting those makes them
//! opaque in your own logs for no gain.
//!
//! ## Encryption is not password hashing
//!
//! Passwords are hashed, never encrypted — that is the [`hash`] module: the
//! [`Hasher`] port, Argon2id and bcrypt drivers, and the [`HashManager`] that
//! selects which algorithm writes. If you can get the value back out, it is
//! the wrong tool for a password — see the
//! [hashing docs](https://github.com/safewords/rainier-framework/blob/main/docs/hashing.md).
//!
//! ## Rotation is designed in
//!
//! Every payload records which key produced it, and a [`KeyRing`] holds one
//! current key plus any number of retired ones. Rotating is a deploy:
//!
//! ```
//! # use rainier_crypt::{Encryption, Key, KeyRing};
//! # fn main() -> rainier_support::Result<()> {
//! # let retired = Key::generate();
//! # let sealed = Encryption::from_keys(KeyRing::new(retired.clone())).encrypt("old")?;
//! let crypt = Encryption::from_keys(
//!     KeyRing::new(Key::generate()).with_previous(retired),
//! );
//!
//! // Written with the old key, still readable; anything new uses the new one.
//! assert_eq!(crypt.decrypt(&sealed)?, "old");
//! # Ok(()) }
//! ```
//!
//! Retrofitting that is painful, which is why it is here from the first
//! release rather than deferred until a key is known to have leaked.

#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

pub mod asymmetric;
pub mod cipher;
pub mod encrypter;
pub mod hash;
#[cfg(feature = "jwt")]
pub mod jwt;
pub mod key;
pub mod php;
pub mod signer;
pub mod url;

pub use hash::{Argon2Hasher, HashDriver, HashManager, Hasher, LegacyVerifier};
#[cfg(feature = "bcrypt")]
pub use hash::{BcryptHasher, BcryptVerifier};

pub use asymmetric::{
    BoxKeyPair, BoxPublicKey, Ed25519Signer, SealedBox, SigningKeyPair, VerifyingPublicKey,
};
pub use cipher::Cipher;
pub use encrypter::{AeadEncrypter, Encrypter, EncrypterExt};
#[cfg(feature = "jwt")]
pub use jwt::{Jwt, JwtAlgorithm, JwtKey, JwtKeyRing};
pub use key::{Key, KeyRing, KEY_LEN};
pub use php::{CryptScheme, PhpCipher, PhpEncrypter};
pub use signer::{HmacSigner, Signer, SignerExt};
pub use url::UrlSigner;

use std::sync::Arc;

use rainier_support::Result;
use serde::de::DeserializeOwned;
use serde::Serialize;

/// The application's encrypter and signer, as one container-storable value.
///
/// A newtype over the two ports rather than binding them separately, so
/// swapping either does not change a call site — the same reason
/// `rainier-framework`'s `Views` wraps a view engine.
#[derive(Clone)]
pub struct Encryption {
    encrypter: Arc<dyn Encrypter>,
    signer: Arc<dyn Signer>,
    /// The ring both were built from, when it is known.
    ///
    /// `None` for [`new`](Self::new), which takes two already-built halves and
    /// cannot see inside them. Everything that needs the ring itself — signing
    /// URLs, for one — has to cope with not getting it.
    keys: Option<KeyRing>,
}

impl Encryption {
    /// Wrap an encrypter and a signer.
    pub fn new(encrypter: Arc<dyn Encrypter>, signer: Arc<dyn Signer>) -> Self {
        Self { encrypter, signer, keys: None }
    }

    /// The default pairing — XChaCha20-Poly1305 and HMAC-SHA256 over one ring.
    pub fn from_keys(keys: KeyRing) -> Self {
        Self::from_keys_with(keys, Cipher::default())
    }

    /// The same, with an explicit [`Cipher`].
    ///
    /// Reading is unaffected: a payload names its own algorithm, so switching
    /// this does not strand anything already written.
    pub fn from_keys_with(keys: KeyRing, cipher: Cipher) -> Self {
        Self {
            encrypter: Arc::new(AeadEncrypter::new(keys.clone()).with_cipher(cipher)),
            signer: Arc::new(HmacSigner::new(keys.clone())),
            keys: Some(keys),
        }
    }

    /// The key ring, when this was built from one.
    ///
    /// `None` when the encrypter and signer were supplied separately — see
    /// [`new`](Self::new). A caller that needs a ring of its own should ask
    /// for one rather than assume.
    pub fn keys(&self) -> Option<&KeyRing> {
        self.keys.as_ref()
    }

    /// The encrypter.
    pub fn encrypter(&self) -> &Arc<dyn Encrypter> {
        &self.encrypter
    }

    /// The signer.
    pub fn signer(&self) -> &Arc<dyn Signer> {
        &self.signer
    }

    /// Encrypt a string.
    pub fn encrypt(&self, plain: &str) -> Result<String> {
        self.encrypter.encrypt(plain)
    }

    /// Decrypt a string.
    pub fn decrypt(&self, payload: &str) -> Result<String> {
        self.encrypter.decrypt(payload)
    }

    /// Encrypt raw bytes.
    pub fn encrypt_bytes(&self, plain: &[u8]) -> Result<String> {
        self.encrypter.encrypt_bytes(plain)
    }

    /// Decrypt to raw bytes.
    pub fn decrypt_bytes(&self, payload: &str) -> Result<Vec<u8>> {
        self.encrypter.decrypt_bytes(payload)
    }

    /// Encrypt any serialisable value as JSON.
    pub fn encrypt_json<T: Serialize>(&self, value: &T) -> Result<String> {
        self.encrypter.encrypt_json(value)
    }

    /// Decrypt JSON back into a value.
    pub fn decrypt_json<T: DeserializeOwned>(&self, payload: &str) -> Result<T> {
        self.encrypter.decrypt_json(payload)
    }

    /// Sign a value, leaving it readable.
    pub fn sign(&self, value: &str) -> Result<String> {
        self.signer.sign(value)
    }

    /// Recover a signed value, or fail if it was altered.
    pub fn verify(&self, signed: &str) -> Result<String> {
        self.signer.verify(signed)
    }

    /// Whether a signed value is intact.
    pub fn is_valid(&self, signed: &str) -> bool {
        self.signer.is_valid(signed)
    }
}

impl std::fmt::Debug for Encryption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Encryption(..)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_facade_value_does_both_halves() {
        let crypt = Encryption::from_keys(KeyRing::new(Key::generate()));

        assert_eq!(crypt.decrypt(&crypt.encrypt("sealed").unwrap()).unwrap(), "sealed");
        assert_eq!(crypt.verify(&crypt.sign("signed").unwrap()).unwrap(), "signed");
    }

    #[test]
    fn json_round_trips_through_the_facade_value() {
        let crypt = Encryption::from_keys(KeyRing::new(Key::generate()));
        let payload = crypt.encrypt_json(&vec![1u8, 2, 3]).unwrap();

        assert_eq!(crypt.decrypt_json::<Vec<u8>>(&payload).unwrap(), vec![1, 2, 3]);
    }

    #[test]
    fn debug_does_not_disclose_anything() {
        let crypt = Encryption::from_keys(KeyRing::new(Key::from_bytes([3u8; KEY_LEN])));
        assert_eq!(format!("{crypt:?}"), "Encryption(..)");
    }
}
