//! The symmetric AEAD algorithms, and the key derivation that feeds them.

use aes_gcm::{Aes128Gcm, Aes256Gcm};
use aes_gcm_siv::Aes256GcmSiv;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, XChaCha20Poly1305};
use hkdf::Hkdf;
use rainier_support::{Error, Result};
use sha2::Sha256;

use crate::key::Key;

/// A symmetric authenticated cipher.
///
/// Every one here is an **AEAD**: it authenticates as well as encrypts, so
/// there is no way to decrypt without checking integrity first. A plain cipher
/// with no tag is not offered, because using one correctly requires bolting a
/// MAC on in exactly the right order and getting it wrong is silent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Cipher {
    /// XChaCha20-Poly1305. **The default, and the one to use.**
    ///
    /// Its 192-bit nonce is large enough that a random nonce per message has no
    /// meaningful collision risk, so there is no counter to keep and no state
    /// to synchronise between processes.
    #[default]
    XChaCha20Poly1305,

    /// ChaCha20-Poly1305, as used by TLS.
    ///
    /// A 96-bit nonce, which is small enough that random nonces have a real
    /// birthday bound — safe for a few billion messages under one key, and a
    /// reason to prefer the X variant when you are not constrained by a peer.
    ChaCha20Poly1305,

    /// AES-256-GCM. For interoperating with something that requires it, or
    /// where hardware AES makes it measurably faster.
    ///
    /// The same 96-bit nonce caveat as `ChaCha20Poly1305`, and a harsher
    /// failure: repeating a nonce under GCM leaks the authentication key, not
    /// just the plaintext relationship.
    Aes256Gcm,

    /// AES-128-GCM. A shorter key, derived from the same 256-bit key material.
    Aes128Gcm,

    /// AES-256-GCM-SIV — nonce-misuse resistant.
    ///
    /// Repeating a nonce here reveals only that two plaintexts were equal,
    /// rather than breaking the cipher. Worth choosing when nonces come from
    /// somewhere you do not fully control.
    Aes256GcmSiv,
}

impl Cipher {
    /// Every cipher, for a diagnostic listing and for tests.
    pub const ALL: [Cipher; 5] = [
        Cipher::XChaCha20Poly1305,
        Cipher::ChaCha20Poly1305,
        Cipher::Aes256Gcm,
        Cipher::Aes128Gcm,
        Cipher::Aes256GcmSiv,
    ];

    /// The short identifier written into a payload.
    pub fn id(self) -> &'static str {
        match self {
            Cipher::XChaCha20Poly1305 => "xc20p",
            Cipher::ChaCha20Poly1305 => "c20p",
            Cipher::Aes256Gcm => "a256gcm",
            Cipher::Aes128Gcm => "a128gcm",
            Cipher::Aes256GcmSiv => "a256siv",
        }
    }

    /// The cipher a payload names.
    ///
    /// `v1` is accepted as an alias for XChaCha20-Poly1305: it is what the
    /// first release wrote, before payloads named their algorithm, and those
    /// payloads still have to open.
    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            "xc20p" | "v1" => Some(Cipher::XChaCha20Poly1305),
            "c20p" => Some(Cipher::ChaCha20Poly1305),
            "a256gcm" => Some(Cipher::Aes256Gcm),
            "a128gcm" => Some(Cipher::Aes128Gcm),
            "a256siv" => Some(Cipher::Aes256GcmSiv),
            _ => None,
        }
    }

    /// The key length this cipher needs, in bytes.
    pub fn key_len(self) -> usize {
        match self {
            Cipher::Aes128Gcm => 16,
            _ => 32,
        }
    }

    /// The nonce length this cipher needs, in bytes.
    pub fn nonce_len(self) -> usize {
        match self {
            Cipher::XChaCha20Poly1305 => 24,
            _ => 12,
        }
    }

    /// Whether repeating a nonce is survivable.
    pub fn is_nonce_misuse_resistant(self) -> bool {
        matches!(self, Cipher::Aes256GcmSiv)
    }

    /// Derive this cipher's key from a ring key.
    ///
    /// HKDF with the cipher's id as `info`, which does two things: it produces
    /// the right length for AES-128, and it gives every cipher a **different**
    /// key from the same material. Without that separation, encrypting one
    /// value under two ciphers with one key would reuse key material across
    /// primitives — which is the kind of thing that is fine until a specific
    /// pair of algorithms interacts badly.
    fn derive(self, key: &Key) -> Result<Vec<u8>> {
        let hkdf = Hkdf::<Sha256>::new(Some(b"rainier-cipher"), key.bytes());
        let mut out = vec![0u8; self.key_len()];
        hkdf.expand(self.id().as_bytes(), &mut out)
            .map_err(|_| Error::internal("could not derive a cipher key"))?;
        Ok(out)
    }

    /// Encrypt `plain`, authenticating `aad` alongside it.
    pub fn encrypt(self, key: &Key, nonce: &[u8], aad: &[u8], plain: &[u8]) -> Result<Vec<u8>> {
        self.check_nonce(nonce)?;
        let derived = self.derive(key)?;
        let payload = Payload { msg: plain, aad };
        let failed = || Error::internal("encryption failed");

        match self {
            Cipher::XChaCha20Poly1305 => XChaCha20Poly1305::new_from_slice(&derived)
                .map_err(|_| failed())?
                .encrypt(nonce.into(), payload)
                .map_err(|_| failed()),
            Cipher::ChaCha20Poly1305 => ChaCha20Poly1305::new_from_slice(&derived)
                .map_err(|_| failed())?
                .encrypt(nonce.into(), payload)
                .map_err(|_| failed()),
            Cipher::Aes256Gcm => {
                use aes_gcm::aead::Aead as _;
                aes_gcm::KeyInit::new_from_slice(&derived)
                    .map(|cipher: Aes256Gcm| cipher)
                    .map_err(|_| failed())?
                    .encrypt(nonce.into(), payload)
                    .map_err(|_| failed())
            }
            Cipher::Aes128Gcm => {
                use aes_gcm::aead::Aead as _;
                aes_gcm::KeyInit::new_from_slice(&derived)
                    .map(|cipher: Aes128Gcm| cipher)
                    .map_err(|_| failed())?
                    .encrypt(nonce.into(), payload)
                    .map_err(|_| failed())
            }
            Cipher::Aes256GcmSiv => {
                use aes_gcm_siv::aead::Aead as _;
                aes_gcm_siv::KeyInit::new_from_slice(&derived)
                    .map(|cipher: Aes256GcmSiv| cipher)
                    .map_err(|_| failed())?
                    .encrypt(nonce.into(), payload)
                    .map_err(|_| failed())
            }
        }
    }

    /// Decrypt, verifying `aad`.
    ///
    /// Every failure is the same error: a caller can do nothing useful with the
    /// difference between "the tag was wrong" and "the length was wrong", and
    /// telling them apart is how padding-oracle attacks start.
    pub fn decrypt(self, key: &Key, nonce: &[u8], aad: &[u8], sealed: &[u8]) -> Result<Vec<u8>> {
        self.check_nonce(nonce)?;
        let derived = self.derive(key)?;
        let payload = Payload { msg: sealed, aad };
        let failed = || Error::bad_request("the payload could not be decrypted");

        match self {
            Cipher::XChaCha20Poly1305 => XChaCha20Poly1305::new_from_slice(&derived)
                .map_err(|_| failed())?
                .decrypt(nonce.into(), payload)
                .map_err(|_| failed()),
            Cipher::ChaCha20Poly1305 => ChaCha20Poly1305::new_from_slice(&derived)
                .map_err(|_| failed())?
                .decrypt(nonce.into(), payload)
                .map_err(|_| failed()),
            Cipher::Aes256Gcm => {
                use aes_gcm::aead::Aead as _;
                aes_gcm::KeyInit::new_from_slice(&derived)
                    .map(|cipher: Aes256Gcm| cipher)
                    .map_err(|_| failed())?
                    .decrypt(nonce.into(), payload)
                    .map_err(|_| failed())
            }
            Cipher::Aes128Gcm => {
                use aes_gcm::aead::Aead as _;
                aes_gcm::KeyInit::new_from_slice(&derived)
                    .map(|cipher: Aes128Gcm| cipher)
                    .map_err(|_| failed())?
                    .decrypt(nonce.into(), payload)
                    .map_err(|_| failed())
            }
            Cipher::Aes256GcmSiv => {
                use aes_gcm_siv::aead::Aead as _;
                aes_gcm_siv::KeyInit::new_from_slice(&derived)
                    .map(|cipher: Aes256GcmSiv| cipher)
                    .map_err(|_| failed())?
                    .decrypt(nonce.into(), payload)
                    .map_err(|_| failed())
            }
        }
    }

    /// A fresh nonce of the right length for this cipher.
    pub fn nonce(self) -> Vec<u8> {
        use rand::RngCore;

        let mut nonce = vec![0u8; self.nonce_len()];
        rand::rngs::OsRng.fill_bytes(&mut nonce);
        nonce
    }

    fn check_nonce(self, nonce: &[u8]) -> Result<()> {
        if nonce.len() == self.nonce_len() {
            Ok(())
        } else {
            Err(Error::bad_request("the payload could not be decrypted"))
        }
    }
}

impl std::fmt::Display for Cipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.id())
    }
}

impl std::str::FromStr for Cipher {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        Self::from_id(value).ok_or_else(|| {
            Error::internal(format!(
                "`{value}` is not a known cipher; expected one of {}",
                Cipher::ALL.iter().map(|c| c.id()).collect::<Vec<_>>().join(", ")
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> Key {
        Key::generate()
    }

    #[test]
    fn every_cipher_round_trips() {
        let key = key();

        for cipher in Cipher::ALL {
            let nonce = cipher.nonce();
            let sealed = cipher.encrypt(&key, &nonce, b"aad", b"the message").unwrap();
            let opened = cipher.decrypt(&key, &nonce, b"aad", &sealed).unwrap();

            assert_eq!(opened, b"the message", "{cipher}");
            assert_ne!(sealed, b"the message", "{cipher}");
        }
    }

    #[test]
    fn every_cipher_detects_tampering() {
        let key = key();

        for cipher in Cipher::ALL {
            let nonce = cipher.nonce();
            let mut sealed = cipher.encrypt(&key, &nonce, b"aad", b"transfer 10").unwrap();
            sealed[0] ^= 0x01;

            assert!(cipher.decrypt(&key, &nonce, b"aad", &sealed).is_err(), "{cipher}");
        }
    }

    #[test]
    fn every_cipher_authenticates_its_aad() {
        let key = key();

        for cipher in Cipher::ALL {
            let nonce = cipher.nonce();
            let sealed = cipher.encrypt(&key, &nonce, b"header-a", b"message").unwrap();

            assert!(
                cipher.decrypt(&key, &nonce, b"header-b", &sealed).is_err(),
                "{cipher} must not open with different associated data"
            );
        }
    }

    #[test]
    fn a_wrong_nonce_length_is_refused() {
        let key = key();

        for cipher in Cipher::ALL {
            assert!(cipher.encrypt(&key, &[0u8; 7], b"", b"x").is_err(), "{cipher}");
            assert!(cipher.decrypt(&key, &[0u8; 7], b"", b"x").is_err(), "{cipher}");
        }
    }

    #[test]
    fn ids_round_trip_and_are_distinct() {
        let mut seen = std::collections::HashSet::new();

        for cipher in Cipher::ALL {
            assert_eq!(Cipher::from_id(cipher.id()), Some(cipher));
            assert!(seen.insert(cipher.id()), "duplicate id: {cipher}");
        }
    }

    #[test]
    fn the_legacy_v1_id_still_names_xchacha() {
        // Payloads written by the first release say `v1`, and they must open.
        assert_eq!(Cipher::from_id("v1"), Some(Cipher::XChaCha20Poly1305));
    }

    #[test]
    fn an_unknown_id_is_none_and_parses_to_a_helpful_error() {
        assert_eq!(Cipher::from_id("rot13"), None);

        let err = "rot13".parse::<Cipher>().unwrap_err();
        assert!(err.message().contains("xc20p"), "{}", err.message());
    }

    #[test]
    fn the_default_is_xchacha() {
        assert_eq!(Cipher::default(), Cipher::XChaCha20Poly1305);
    }

    #[test]
    fn each_cipher_derives_a_different_key_from_the_same_material() {
        // Domain separation: the same ring key must not produce the same
        // cipher key for two different algorithms.
        let key = key();
        let mut derived = std::collections::HashSet::new();

        for cipher in Cipher::ALL {
            assert!(derived.insert(cipher.derive(&key).unwrap()), "{cipher} shares a key");
        }
    }

    #[test]
    fn derivation_is_deterministic() {
        let key = key();
        assert_eq!(
            Cipher::Aes256Gcm.derive(&key).unwrap(),
            Cipher::Aes256Gcm.derive(&key).unwrap()
        );
    }

    #[test]
    fn aes_128_gets_a_short_key_and_the_rest_a_long_one() {
        let key = key();

        assert_eq!(Cipher::Aes128Gcm.derive(&key).unwrap().len(), 16);
        for cipher in Cipher::ALL.iter().filter(|c| **c != Cipher::Aes128Gcm) {
            assert_eq!(cipher.derive(&key).unwrap().len(), 32, "{cipher}");
        }
    }

    #[test]
    fn one_cipher_cannot_open_anothers_payload() {
        let key = key();
        let nonce = Cipher::Aes256Gcm.nonce();
        let sealed = Cipher::Aes256Gcm.encrypt(&key, &nonce, b"", b"secret").unwrap();

        // Same nonce length, same key material, different algorithm and
        // therefore a different derived key.
        assert!(Cipher::ChaCha20Poly1305.decrypt(&key, &nonce, b"", &sealed).is_err());
    }

    #[test]
    fn nonces_are_the_right_length_and_not_repeated() {
        for cipher in Cipher::ALL {
            let a = cipher.nonce();
            let b = cipher.nonce();

            assert_eq!(a.len(), cipher.nonce_len(), "{cipher}");
            assert_ne!(a, b, "{cipher}");
        }
    }

    #[test]
    fn only_siv_claims_misuse_resistance() {
        assert!(Cipher::Aes256GcmSiv.is_nonce_misuse_resistant());
        for cipher in Cipher::ALL.iter().filter(|c| **c != Cipher::Aes256GcmSiv) {
            assert!(!cipher.is_nonce_misuse_resistant(), "{cipher}");
        }
    }

    #[test]
    fn a_repeated_nonce_under_siv_reveals_only_equality() {
        // Not a licence to reuse nonces — a demonstration of the difference.
        let key = key();
        let nonce = Cipher::Aes256GcmSiv.nonce();

        let one = Cipher::Aes256GcmSiv.encrypt(&key, &nonce, b"", b"same").unwrap();
        let two = Cipher::Aes256GcmSiv.encrypt(&key, &nonce, b"", b"same").unwrap();
        let other = Cipher::Aes256GcmSiv.encrypt(&key, &nonce, b"", b"different").unwrap();

        assert_eq!(one, two, "SIV is deterministic for a given key, nonce and message");
        assert_ne!(one, other);
        assert_eq!(Cipher::Aes256GcmSiv.decrypt(&key, &nonce, b"", &one).unwrap(), b"same");
    }

    #[test]
    fn empty_and_large_messages_round_trip() {
        let key = key();
        let big = vec![7u8; 100_000];

        for cipher in Cipher::ALL {
            let nonce = cipher.nonce();

            assert_eq!(
                cipher
                    .decrypt(&key, &nonce, b"", &cipher.encrypt(&key, &nonce, b"", b"").unwrap())
                    .unwrap(),
                Vec::<u8>::new(),
                "{cipher}"
            );
            assert_eq!(
                cipher
                    .decrypt(&key, &nonce, b"", &cipher.encrypt(&key, &nonce, b"", &big).unwrap())
                    .unwrap(),
                big,
                "{cipher}"
            );
        }
    }
}
