//! Authenticated encryption — the [`Encrypter`] port and its AEAD
//! implementation.

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine as _;
use rainier_support::{Error, Result};
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::cipher::Cipher;
use crate::key::KeyRing;

/// Reversible, tamper-evident encryption.
///
/// A port rather than a concrete type for the same reason [`Hasher`] is one in
/// `rainier-auth`: the right primitive changes over time, and an application
/// migrating between two should be able to run both.
///
/// [`Hasher`]: https://docs.rs/rainier-auth
pub trait Encrypter: Send + Sync + 'static {
    /// Encrypt `plain`, returning a self-describing, URL-safe payload.
    fn encrypt_bytes(&self, plain: &[u8]) -> Result<String>;

    /// Decrypt a payload produced by [`encrypt_bytes`](Self::encrypt_bytes).
    ///
    /// Fails if the payload was truncated, altered, or produced by a key this
    /// encrypter does not hold. There is deliberately no way to decrypt
    /// without authenticating.
    fn decrypt_bytes(&self, payload: &str) -> Result<Vec<u8>>;
}

/// The typed conveniences every [`Encrypter`] gets.
///
/// Separate from the trait so that stays object-safe — the generic methods
/// would otherwise keep `Arc<dyn Encrypter>` from existing.
pub trait EncrypterExt: Encrypter {
    /// Encrypt a string.
    fn encrypt(&self, plain: &str) -> Result<String> {
        self.encrypt_bytes(plain.as_bytes())
    }

    /// Decrypt to a string.
    fn decrypt(&self, payload: &str) -> Result<String> {
        let bytes = self.decrypt_bytes(payload)?;
        String::from_utf8(bytes)
            .map_err(|_| Error::internal("the decrypted value is not valid UTF-8"))
    }

    /// Encrypt any serialisable value as JSON.
    fn encrypt_json<T: Serialize>(&self, value: &T) -> Result<String> {
        let json = serde_json::to_vec(value)?;
        self.encrypt_bytes(&json)
    }

    /// Decrypt JSON back into a value.
    fn decrypt_json<T: DeserializeOwned>(&self, payload: &str) -> Result<T> {
        let bytes = self.decrypt_bytes(payload)?;
        serde_json::from_slice(&bytes).map_err(|e| {
            // The payload authenticated, so this is our own format changing
            // under us rather than anything an attacker did.
            Error::internal(format!("the decrypted value is not the expected shape: {e}"))
        })
    }
}

impl<E: Encrypter + ?Sized> EncrypterExt for E {}

/// An AEAD [`Cipher`] over a [`KeyRing`].
///
/// Writes with one cipher and reads **any** of them, because the payload names
/// its own algorithm. So changing cipher is the same kind of change as
/// [rotating a key](KeyRing): deploy the new setting, and everything already
/// written still opens.
///
/// ```
/// # use rainier_crypt::{AeadEncrypter, Cipher, EncrypterExt, Key, KeyRing};
/// # fn main() -> rainier_support::Result<()> {
/// let keys = KeyRing::new(Key::generate());
/// let written = AeadEncrypter::new(keys.clone()).encrypt("before")?;
///
/// // Switched to AES; the old payload still reads.
/// let now = AeadEncrypter::new(keys).with_cipher(Cipher::Aes256Gcm);
/// assert_eq!(now.decrypt(&written)?, "before");
/// assert!(now.encrypt("after")?.starts_with("a256gcm."));
/// # Ok(()) }
/// ```
pub struct AeadEncrypter {
    keys: KeyRing,
    cipher: Cipher,
}

impl AeadEncrypter {
    /// An encrypter over `keys`, using the default cipher.
    pub fn new(keys: KeyRing) -> Self {
        Self { keys, cipher: Cipher::default() }
    }

    /// Write with a specific cipher. Reading is unaffected.
    pub fn with_cipher(mut self, cipher: Cipher) -> Self {
        self.cipher = cipher;
        self
    }

    /// The key ring, for a diagnostic command.
    pub fn keys(&self) -> &KeyRing {
        &self.keys
    }

    /// The cipher new payloads are written with.
    pub fn cipher(&self) -> Cipher {
        self.cipher
    }
}

impl Encrypter for AeadEncrypter {
    fn encrypt_bytes(&self, plain: &[u8]) -> Result<String> {
        let key = self.keys.current();
        let nonce = self.cipher.nonce();

        // The header is authenticated but not encrypted, so a payload cannot
        // be relabelled with a different cipher or key id and still open.
        let header = format!("{}.{}", self.cipher.id(), key.id());
        let sealed = self.cipher.encrypt(key, &nonce, header.as_bytes(), plain)?;

        Ok(format!("{header}.{}.{}", B64.encode(&nonce), B64.encode(&sealed)))
    }

    fn decrypt_bytes(&self, payload: &str) -> Result<Vec<u8>> {
        // One message for every malformed-or-forged case. Distinguishing "the
        // padding was wrong" from "the tag was wrong" is how padding-oracle
        // attacks start, and the caller can do nothing useful with the
        // difference anyway.
        let invalid = || Error::bad_request("the payload could not be decrypted");

        let mut parts = payload.split('.');
        let (Some(algorithm), Some(key_id), Some(nonce), Some(sealed), None) =
            (parts.next(), parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(invalid());
        };

        let cipher = Cipher::from_id(algorithm).ok_or_else(invalid)?;

        let key = self.keys.find(key_id).ok_or_else(|| {
            // Worth its own message: this one is an operator error — a key was
            // dropped from the ring while payloads written with it still
            // exist — not an attack, and the fix is to put the key back.
            Error::internal(format!(
                "no key with id `{key_id}` is on the ring; it is still needed to read \
                 payloads written with it"
            ))
        })?;

        let nonce = B64.decode(nonce).map_err(|_| invalid())?;
        let sealed = B64.decode(sealed).map_err(|_| invalid())?;

        // The header as written, not as reconstructed from the parsed cipher —
        // so a `v1` payload authenticates against `v1` rather than `xc20p`.
        let header = format!("{algorithm}.{key_id}");
        cipher.decrypt(key, &nonce, header.as_bytes(), &sealed)
    }
}

impl std::fmt::Debug for AeadEncrypter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AeadEncrypter")
            .field("cipher", &self.cipher.id())
            .field("keys", &self.keys.ids())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::key::Key;

    fn encrypter() -> AeadEncrypter {
        AeadEncrypter::new(KeyRing::new(Key::generate()))
    }

    #[test]
    fn a_value_round_trips() {
        let crypt = encrypter();
        let payload = crypt.encrypt("hello, world").unwrap();

        assert_eq!(crypt.decrypt(&payload).unwrap(), "hello, world");
    }

    #[test]
    fn the_ciphertext_does_not_contain_the_plaintext() {
        let payload = encrypter().encrypt("correct-horse-battery-staple").unwrap();
        assert!(!payload.contains("correct-horse"), "{payload}");
    }

    #[test]
    fn the_same_value_encrypts_differently_every_time() {
        let crypt = encrypter();

        // A random nonce per message, so equal plaintexts are not detectable
        // as equal — which is what makes an encrypted column safe to index on
        // nothing and unsafe to compare for equality.
        assert_ne!(crypt.encrypt("same").unwrap(), crypt.encrypt("same").unwrap());
    }

    #[test]
    fn a_payload_is_url_safe() {
        let payload = encrypter().encrypt("a value with / and + in mind").unwrap();

        assert!(
            payload.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')),
            "{payload}"
        );
    }

    #[test]
    fn tampering_with_the_ciphertext_is_detected() {
        let crypt = encrypter();
        let payload = crypt.encrypt("transfer 10").unwrap();

        let mut parts: Vec<&str> = payload.split('.').collect();
        let flipped = {
            let mut bytes = B64.decode(parts[3]).unwrap();
            bytes[0] ^= 0x01;
            B64.encode(bytes)
        };
        parts[3] = &flipped;

        assert!(crypt.decrypt(&parts.join(".")).is_err());
    }

    #[test]
    fn tampering_with_the_nonce_is_detected() {
        let crypt = encrypter();
        let payload = crypt.encrypt("transfer 10").unwrap();

        let mut parts: Vec<&str> = payload.split('.').collect();
        let other = B64.encode(vec![0u8; Cipher::default().nonce_len()]);
        parts[2] = &other;

        assert!(crypt.decrypt(&parts.join(".")).is_err());
    }

    #[test]
    fn relabelling_the_cipher_is_detected() {
        // The algorithm is part of the authenticated header, so claiming a
        // payload was written with a different one does not open it.
        let crypt = encrypter();
        let payload = crypt.encrypt("secret").unwrap();

        let mut parts: Vec<&str> = payload.split('.').collect();
        parts[0] = "a256gcm";

        assert!(crypt.decrypt(&parts.join(".")).is_err());
    }

    #[test]
    fn every_cipher_round_trips_through_the_encrypter() {
        let keys = KeyRing::new(Key::generate());

        for cipher in Cipher::ALL {
            let crypt = AeadEncrypter::new(keys.clone()).with_cipher(cipher);
            let payload = crypt.encrypt("the message").unwrap();

            assert!(payload.starts_with(&format!("{}.", cipher.id())), "{payload}");
            assert_eq!(crypt.decrypt(&payload).unwrap(), "the message", "{cipher}");
        }
    }

    #[test]
    fn a_payload_written_by_any_cipher_reads_under_any_default() {
        // The point of naming the algorithm in the payload: switching the
        // configured cipher is a deploy, not a migration.
        let keys = KeyRing::new(Key::generate());
        let written: Vec<String> = Cipher::ALL
            .iter()
            .map(|c| AeadEncrypter::new(keys.clone()).with_cipher(*c).encrypt("old").unwrap())
            .collect();

        for reader in Cipher::ALL {
            let crypt = AeadEncrypter::new(keys.clone()).with_cipher(reader);
            for payload in &written {
                assert_eq!(crypt.decrypt(payload).unwrap(), "old", "{reader} reading {payload}");
            }
        }
    }

    #[test]
    fn an_unknown_cipher_id_is_refused() {
        let crypt = encrypter();
        assert_eq!(crypt.decrypt("rot13.abc.def.ghi").unwrap_err().status(), 400);
    }

    #[test]
    fn a_legacy_v1_payload_still_opens() {
        // What the first release wrote: `v1` in place of an algorithm name,
        // XChaCha20-Poly1305, and the header authenticated as written.
        let key = Key::generate();
        let cipher = Cipher::XChaCha20Poly1305;
        let nonce = cipher.nonce();
        let header = format!("v1.{}", key.id());
        let sealed = cipher.encrypt(&key, &nonce, header.as_bytes(), b"from before").unwrap();
        let payload = format!("{header}.{}.{}", B64.encode(&nonce), B64.encode(&sealed));

        let crypt = AeadEncrypter::new(KeyRing::new(key));
        assert_eq!(crypt.decrypt(&payload).unwrap(), "from before");
    }

    #[test]
    fn relabelling_the_header_is_detected() {
        // The header is the AEAD's associated data, so claiming a payload was
        // written by a different key does not let it open with that key.
        let old = Key::generate();
        let current = Key::generate();
        let crypt = AeadEncrypter::new(KeyRing::new(current).with_previous(old.clone()));

        let payload = crypt.encrypt("secret").unwrap();
        let mut parts: Vec<&str> = payload.split('.').collect();
        parts[1] = old.id();

        assert!(crypt.decrypt(&parts.join(".")).is_err());
    }

    #[test]
    fn a_malformed_payload_is_a_400_not_a_500() {
        let crypt = encrypter();

        for rubbish in ["", "nonsense", "v1.abc", "v1.abc.def.ghi.jkl", "v2.abc.def.ghi"] {
            let err = crypt.decrypt(rubbish).unwrap_err();
            assert_eq!(err.status(), 400, "{rubbish}");
        }
    }

    #[test]
    fn every_failure_reports_the_same_thing() {
        // No padding oracle: the caller cannot tell *why* it failed.
        let crypt = encrypter();
        let truncated = crypt.decrypt("v1.abc.def.ghi").unwrap_err();
        let garbage = crypt.decrypt("v1.abc.!!!.!!!").unwrap_err();

        assert_eq!(truncated.message(), garbage.message());
    }

    #[test]
    fn another_applications_key_cannot_read_the_payload() {
        let payload = encrypter().encrypt("secret").unwrap();
        assert!(encrypter().decrypt(&payload).is_err());
    }

    #[test]
    fn a_retired_key_still_reads_what_it_wrote() {
        let old = Key::generate();
        let written = AeadEncrypter::new(KeyRing::new(old.clone())).encrypt("before").unwrap();

        // Rotation: a new current key, the old one kept for reading.
        let rotated = AeadEncrypter::new(KeyRing::new(Key::generate()).with_previous(old));

        assert_eq!(rotated.decrypt(&written).unwrap(), "before");
        assert!(
            rotated.encrypt("after").unwrap().contains(rotated.keys().current().id()),
            "new payloads use the new key"
        );
    }

    #[test]
    fn dropping_a_key_that_is_still_needed_says_so() {
        let written = AeadEncrypter::new(KeyRing::new(Key::generate())).encrypt("x").unwrap();
        let err = encrypter().decrypt(&written).unwrap_err();

        assert_eq!(err.status(), 500, "an operator error, not a client one");
        assert!(err.message().contains("still needed"), "{}", err.message());
    }

    #[test]
    fn json_round_trips() {
        #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
        struct Token {
            user: u64,
            scopes: Vec<String>,
        }

        let crypt = encrypter();
        let token = Token { user: 7, scopes: vec!["read".into()] };
        let payload = crypt.encrypt_json(&token).unwrap();

        assert_eq!(crypt.decrypt_json::<Token>(&payload).unwrap(), token);
    }

    #[test]
    fn an_empty_value_round_trips() {
        let crypt = encrypter();
        assert_eq!(crypt.decrypt(&crypt.encrypt("").unwrap()).unwrap(), "");
    }

    #[test]
    fn binary_round_trips() {
        let crypt = encrypter();
        let bytes: Vec<u8> = (0..=255u8).collect();

        assert_eq!(crypt.decrypt_bytes(&crypt.encrypt_bytes(&bytes).unwrap()).unwrap(), bytes);
    }

    #[test]
    fn the_port_is_object_safe() {
        let crypt: std::sync::Arc<dyn Encrypter> = std::sync::Arc::new(encrypter());
        // The ext trait must reach through the trait object, or the whole
        // split was pointless.
        assert_eq!(crypt.decrypt(&crypt.encrypt("via dyn").unwrap()).unwrap(), "via dyn");
    }
}
