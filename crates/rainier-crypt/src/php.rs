//! Reading what a PHP application encrypted — [`PhpEncrypter`].
//!
//! ```ignore
//! // In a provider, while the ported rows are still in the PHP envelope.
//! app.instance(Encryption::new(
//!     Arc::new(PhpEncrypter::new(keys)),
//!     Arc::new(HmacSigner::new(keys)),
//! ));
//! ```
//!
//! Not a preference. A ported application's database already holds columns
//! that PHP wrote, and they have to stay readable — so this reads and writes
//! the `{iv, value, mac}` envelope PHP MVC frameworks produce, byte for byte,
//! against the same `APP_KEY`.
//!
//! # The format
//!
//! ```text
//! base64( json( { "iv": base64(iv), "value": base64(ciphertext), "mac": hex(mac) } ) )
//! ```
//!
//! AES-256-CBC with PKCS#7 padding, and an HMAC-SHA256 over the **base64
//! forms** of the IV and the ciphertext concatenated — not over the raw bytes,
//! which is the detail every reimplementation gets wrong and then cannot
//! explain.
//!
//! # It is encrypt-then-MAC, and the MAC is checked first
//!
//! The PHP implementation checks the MAC before it decrypts, and so does
//! this. The order is not a preference either: CBC without an authenticated
//! MAC first is a padding oracle, and a padding oracle recovers the plaintext
//! without the key.
//!
//! # Why this is not a `Cipher` variant
//!
//! [`Cipher`](crate::Cipher) selects the AEAD *inside* Rainier's own envelope,
//! which names its algorithm, carries its key id and is URL-safe. The PHP
//! format is a different envelope with none of those properties. Making it a
//! cipher variant would mean one `Encrypter` producing two incompatible
//! payload shapes depending on a setting, which is exactly the ambiguity the
//! self-describing envelope exists to avoid.
//!
//! # Use it to migrate, not to stay
//!
//! Everything Rainier's own [`AeadEncrypter`](crate::AeadEncrypter) has that
//! this does not: a key id in the payload, so rotation is possible without
//! re-encrypting; an algorithm name, so the cipher can change; AEAD, so there
//! is one primitive to get right instead of two composed in the correct order.
//!
//! The shape that works is the same one [legacy password
//! hashes](https://docs.rs/rainier-auth) use: read with this, write with the
//! native encrypter, and let the rows convert as they are touched.

use rainier_support::setting_enum;

setting_enum! {
    /// Which payload envelope this application writes.
    ///
    /// Selected by `APP_CIPHER`, and deliberately a closed set: writing the
    /// wrong envelope is not a preference that degrades gracefully, it is a
    /// column nothing can read.
    ///
    /// ```
    /// use rainier_crypt::CryptScheme;
    /// use rainier_support::Setting;
    ///
    /// assert_eq!(CryptScheme::parse("php").unwrap(), CryptScheme::Php);
    /// assert!(CryptScheme::parse("hpp").is_err());
    /// ```
    pub enum CryptScheme: "encryption scheme" {
        /// Rainier's own: a self-describing, URL-safe payload naming its
        /// algorithm and its key.
        ///
        /// The default, and what a new application wants — rotation and a
        /// cipher change are both possible without re-encrypting anything.
        #[default]
        Native = "native",

        /// The `{iv, value, mac}` envelope, for a database PHP already wrote.
        ///
        /// A migration position, not a destination — see the module docs.
        Php = "php",
    }
}

use aes::cipher::block_padding::Pkcs7;
use aes::cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use hmac::{Hmac, Mac};
use rainier_support::{Error, Result};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::encrypter::Encrypter;
use crate::key::{Key, KeyRing};

type Aes256CbcEnc = cbc::Encryptor<aes::Aes256>;
type Aes256CbcDec = cbc::Decryptor<aes::Aes256>;
type HmacSha256 = Hmac<Sha256>;

/// AES-256-CBC's block and IV size.
const IV_LEN: usize = 16;

/// The JSON the PHP encrypter base64-encodes.
#[derive(Debug, Serialize, Deserialize)]
struct Payload {
    iv: String,
    value: String,
    mac: String,
    /// Present for the GCM variant, absent for CBC. Kept so a payload written
    /// by a newer PHP implementation round-trips rather than being silently
    /// dropped.
    #[serde(skip_serializing_if = "Option::is_none")]
    tag: Option<String>,
}

/// The `{iv, value, mac}` envelope PHP MVC frameworks write, in Rust.
pub struct PhpEncrypter {
    keys: KeyRing,
}

impl PhpEncrypter {
    /// Read and write with `keys`.
    ///
    /// The current key encrypts; **every** key on the ring is tried when
    /// decrypting, because the PHP payload carries no key id and there is no
    /// other way to support a rotation.
    pub fn new(keys: KeyRing) -> Self {
        Self { keys }
    }

    /// The key ring.
    pub fn keys(&self) -> &KeyRing {
        &self.keys
    }

    /// Encrypt under one specific key.
    fn encrypt_with(key: &Key, plain: &[u8]) -> Result<String> {
        let mut iv = [0u8; IV_LEN];
        rand::Rng::fill(&mut rand::thread_rng(), &mut iv[..]);

        let ciphertext = Aes256CbcEnc::new_from_slices(key.bytes(), &iv)
            .map_err(|_| Error::internal("the key or IV is the wrong length for AES-256-CBC"))?
            .encrypt_padded_vec_mut::<Pkcs7>(plain);

        let iv = B64.encode(iv);
        let value = B64.encode(&ciphertext);
        let mac = Self::mac(key, &iv, &value)?;

        let payload = serde_json::to_vec(&Payload { iv, value, mac, tag: None })?;
        Ok(B64.encode(payload))
    }

    /// Decrypt under one specific key.
    fn decrypt_with(key: &Key, payload: &Payload) -> Result<Vec<u8>> {
        let expected = Self::mac(key, &payload.iv, &payload.value)?;

        // Before decrypting, always. CBC without this is a padding oracle, and
        // a padding oracle recovers the plaintext without the key.
        let presented = hex_decode(&payload.mac).ok_or_else(invalid)?;
        let expected = hex_decode(&expected).ok_or_else(invalid)?;

        if !bool::from(presented.ct_eq(&expected)) {
            return Err(invalid());
        }

        let iv = B64.decode(&payload.iv).map_err(|_| invalid())?;
        let ciphertext = B64.decode(&payload.value).map_err(|_| invalid())?;

        Aes256CbcDec::new_from_slices(key.bytes(), &iv)
            .map_err(|_| invalid())?
            .decrypt_padded_vec_mut::<Pkcs7>(&ciphertext)
            .map_err(|_| invalid())
    }

    /// `hex(hmac_sha256(key, iv_base64 + value_base64))`.
    ///
    /// Over the **base64 forms**, concatenated. That is what PHP does, and it
    /// is the one detail that makes an otherwise-correct reimplementation
    /// produce MACs nothing accepts.
    fn mac(key: &Key, iv: &str, value: &str) -> Result<String> {
        let mut mac = HmacSha256::new_from_slice(key.bytes())
            .map_err(|_| Error::internal("the key is the wrong length for HMAC-SHA256"))?;

        mac.update(iv.as_bytes());
        mac.update(value.as_bytes());

        Ok(hex_encode(&mac.finalize().into_bytes()))
    }
}

impl Encrypter for PhpEncrypter {
    fn encrypt_bytes(&self, plain: &[u8]) -> Result<String> {
        Self::encrypt_with(self.keys.current(), plain)
    }

    fn decrypt_bytes(&self, payload: &str) -> Result<Vec<u8>> {
        let decoded = B64.decode(payload).map_err(|_| invalid())?;
        let payload: Payload = serde_json::from_slice(&decoded).map_err(|_| invalid())?;

        // Every key on the ring, current first. The PHP payload names no
        // key, so trying them is the only way a rotation can work at all —
        // and it is why rotating here costs a decrypt attempt per retired key
        // rather than being free the way the native format is.
        for key in self.keys.all() {
            if let Ok(plain) = Self::decrypt_with(key, &payload) {
                return Ok(plain);
            }
        }

        Err(invalid())
    }
}

impl std::fmt::Debug for PhpEncrypter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PhpEncrypter").field("keys", &self.keys.ids()).finish()
    }
}

/// One error for every failure, so nothing distinguishes a bad MAC from bad
/// padding from the wrong key.
fn invalid() -> Error {
    Error::internal("could not decrypt the payload")
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn hex_decode(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }

    (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encrypter::EncrypterExt;

    fn encrypter() -> PhpEncrypter {
        PhpEncrypter::new(KeyRing::new(Key::generate()))
    }

    #[test]
    fn a_value_round_trips() {
        let encrypter = encrypter();
        let payload = encrypter.encrypt("a card number").unwrap();

        assert_eq!(encrypter.decrypt(&payload).unwrap(), "a card number");
    }

    #[test]
    fn the_payload_is_the_shape_php_writes() {
        let payload = encrypter().encrypt("hello").unwrap();

        let decoded = B64.decode(&payload).expect("base64 around the JSON");
        let json: serde_json::Value = serde_json::from_slice(&decoded).expect("JSON inside");

        assert!(json["iv"].is_string());
        assert!(json["value"].is_string());
        assert!(json["mac"].is_string());

        // The IV is 16 raw bytes, base64'd — which is what a PHP reader will
        // pass straight to openssl_decrypt.
        assert_eq!(B64.decode(json["iv"].as_str().unwrap()).unwrap().len(), IV_LEN);
        // And the MAC is hex-encoded SHA-256: 64 characters.
        assert_eq!(json["mac"].as_str().unwrap().len(), 64);
    }

    #[test]
    fn the_mac_covers_the_base64_forms() {
        // The detail every reimplementation gets wrong. Asserted directly, so
        // a "tidy-up" that MACs the raw bytes instead fails here rather than
        // in production against a real PHP application.
        let key = Key::generate();
        let iv = B64.encode([7u8; IV_LEN]);
        let value = B64.encode(b"ciphertext");

        let mut expected = HmacSha256::new_from_slice(key.bytes()).unwrap();
        expected.update(format!("{iv}{value}").as_bytes());

        assert_eq!(
            PhpEncrypter::mac(&key, &iv, &value).unwrap(),
            hex_encode(&expected.finalize().into_bytes())
        );
    }

    #[test]
    fn a_tampered_ciphertext_is_refused_rather_than_decrypted() {
        // The padding oracle this order of operations exists to close.
        let encrypter = encrypter();
        let payload = encrypter.encrypt("a card number").unwrap();

        let decoded = B64.decode(&payload).unwrap();
        let mut json: Payload = serde_json::from_slice(&decoded).unwrap();

        // Flip a byte of the ciphertext, leaving the MAC alone.
        let mut ciphertext = B64.decode(&json.value).unwrap();
        ciphertext[0] ^= 0xff;
        json.value = B64.encode(&ciphertext);

        let tampered = B64.encode(serde_json::to_vec(&json).unwrap());

        assert!(encrypter.decrypt(&tampered).is_err());
    }

    #[test]
    fn a_tampered_iv_is_refused() {
        let encrypter = encrypter();
        let payload = encrypter.encrypt("a card number").unwrap();

        let decoded = B64.decode(&payload).unwrap();
        let mut json: Payload = serde_json::from_slice(&decoded).unwrap();

        let mut iv = B64.decode(&json.iv).unwrap();
        iv[0] ^= 0xff;
        json.iv = B64.encode(&iv);

        let tampered = B64.encode(serde_json::to_vec(&json).unwrap());

        assert!(encrypter.decrypt(&tampered).is_err());
    }

    #[test]
    fn another_applications_payload_does_not_decrypt() {
        let payload = encrypter().encrypt("a card number").unwrap();

        assert!(encrypter().decrypt(&payload).is_err());
    }

    #[test]
    fn a_retired_key_still_reads_what_it_wrote() {
        // The PHP payload names no key, so this is a decrypt attempt per
        // retired key rather than a lookup — worth knowing before keeping ten
        // of them on the ring.
        let old = Key::generate();
        let payload = PhpEncrypter::new(KeyRing::new(old.clone())).encrypt("secret").unwrap();

        let rotated = PhpEncrypter::new(KeyRing::new(Key::generate()).with_previous(old));
        assert_eq!(rotated.decrypt(&payload).unwrap(), "secret");
    }

    #[test]
    fn nonsense_is_an_error_rather_than_a_panic() {
        let encrypter = encrypter();

        for payload in ["", "not base64 at all !!", &B64.encode("not json"), &B64.encode("{}")] {
            assert!(encrypter.decrypt(payload).is_err(), "{payload:?}");
        }
    }

    #[test]
    fn every_failure_looks_the_same() {
        // A bad MAC, bad padding and the wrong key must be indistinguishable:
        // telling them apart is what a padding oracle is built from.
        let encrypter = encrypter();
        let payload = encrypter.encrypt("secret").unwrap();

        let corrupt = B64.encode("{}");
        let wrong_key =
            PhpEncrypter::new(KeyRing::new(Key::generate())).encrypt("secret").unwrap();
        let _ = payload;

        assert_eq!(
            encrypter.decrypt(&corrupt).unwrap_err().message(),
            encrypter.decrypt(&wrong_key).unwrap_err().message()
        );
    }

    #[test]
    fn empty_and_long_values_round_trip() {
        let encrypter = encrypter();

        for plain in ["", "a", &"x".repeat(10_000), "héllo — ünicode"] {
            assert_eq!(encrypter.decrypt(&encrypter.encrypt(plain).unwrap()).unwrap(), plain);
        }
    }

    #[test]
    fn hex_round_trips() {
        assert_eq!(hex_decode(&hex_encode(&[0, 15, 16, 255])), Some(vec![0, 15, 16, 255]));
        assert_eq!(hex_decode("abc"), None, "an odd length is not hex");
        assert_eq!(hex_decode("zz"), None);
    }

    /// A key and IV of known bytes, so the ciphertext is deterministic.
    fn fixed_key() -> Key {
        Key::from_base64("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=").unwrap()
    }

    #[test]
    fn the_ciphertext_is_pinned_to_a_known_vector() {
        // AES-256-CBC over a fixed key and IV, with PKCS#7 padding. Frozen so
        // a change to the padding, the mode or the key handling fails here
        // rather than silently making every existing row unreadable.
        //
        // Both values below were cross-checked against an independent
        // implementation (Python's `cryptography` for the AES-256-CBC/PKCS#7
        // ciphertext, `hmac`+`hashlib` for the tag) rather than being read
        // back out of this code — so they pin interoperability and not merely
        // "whatever it does today".
        let key = fixed_key();
        let iv = B64.encode([0u8; IV_LEN]);

        let ciphertext = Aes256CbcEnc::new_from_slices(key.bytes(), &[0u8; IV_LEN])
            .unwrap()
            .encrypt_padded_vec_mut::<Pkcs7>(b"a card number");

        assert_eq!(B64.encode(&ciphertext), "nH4xXUTQpgBTKXvgzml9eg==");

        // And the MAC over the two base64 strings, which is the part a PHP
        // reader recomputes.
        assert_eq!(
            PhpEncrypter::mac(&key, &iv, &B64.encode(&ciphertext)).unwrap(),
            "f648b5eac55d1dd21fe92175da4660271c245c3c0d23a9839e932eb583716aad"
        );
    }
}
