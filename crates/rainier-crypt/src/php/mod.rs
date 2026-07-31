//! Reading what a PHP application encrypted — [`PhpEncrypter`].
//!
//! ```ignore
//! // In a provider, while the ported rows are still in the PHP envelope.
//! app.instance(Encryption::new(
//!     Arc::new(PhpEncrypter::new(keys)),
//!     Arc::new(HmacSigner::new(keys)),
//! ));
//!
//! // A database whose PHP co-reader is configured for AES-256-GCM:
//! PhpEncrypter::new(keys).writing(PhpCipher::Aes256Gcm)
//! ```
//!
//! Not a preference. A ported application's database already holds columns
//! that PHP wrote, and they have to stay readable — so this reads and writes
//! the envelope PHP MVC frameworks produce, byte for byte, against the same
//! `APP_KEY`.
//!
//! # This is a compatibility layer, and it is built like one
//!
//! "PHP" in the name refers to a **wire format**, not to a cipher — the
//! cryptography underneath is ordinary AES. The two concerns live apart, on
//! purpose:
//!
//! | Layer | Module | Knows about |
//! |---|---|---|
//! | the envelope | [`envelope`] | JSON, base64, hex, which bytes a MAC covers — **no cryptography** |
//! | the primitives | [`primitive`] | raw-key AES-256-CBC / AES-256-GCM / HMAC-SHA256 — **no encoding** |
//! | the composition | [`PhpEncrypter`] | the key ring, the write selection, and the one-error policy |
//!
//! The envelope module is where the format's one famous trap lives (the CBC
//! MAC covers the base64 *strings*, not the bytes); the primitive module is
//! where the raw-key rule lives (PHP feeds `APP_KEY` straight to
//! `openssl_encrypt`, unlike the native [`Cipher`](crate::Cipher), which
//! HKDF-derives per-algorithm subkeys). This type just introduces them.
//!
//! # Two variants of one envelope
//!
//! | Writes | On the wire |
//! |---|---|
//! | [`PhpCipher::Aes256Cbc`] (default) | `{iv, value, mac}` — encrypt-then-MAC, MAC checked first |
//! | [`PhpCipher::Aes256Gcm`] | `{iv, value, mac: "", tag}` — what a GCM-configured PHP app writes |
//!
//! Reading takes **both**, whatever is selected for writing — the payload
//! says which it is — so a table that changed cipher generations ago opens
//! end to end.
//!
//! # Why this is not a `Cipher` variant
//!
//! [`Cipher`](crate::Cipher) selects the AEAD *inside* Rainier's own
//! envelope, which names its algorithm, carries its key id and is URL-safe.
//! The PHP format is a different envelope with none of those properties.
//! Making it a cipher variant would mean one `Encrypter` producing two
//! incompatible payload shapes depending on a setting, which is exactly the
//! ambiguity the self-describing envelope exists to avoid.
//!
//! # Use it to migrate, not to stay
//!
//! Everything Rainier's own [`AeadEncrypter`](crate::AeadEncrypter) has that
//! this does not: a key id in the payload, so rotation is possible without
//! re-encrypting; an algorithm name, so the cipher can change; one primitive
//! to get right instead of two composed in the correct order.

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

        /// The `{iv, value, mac, tag}` envelope, for a database PHP already
        /// wrote.
        ///
        /// A migration position, not a destination — see the module docs.
        Php = "php",
    }
}

setting_enum! {
    /// Which cipher a [`PhpEncrypter`] writes with.
    ///
    /// A property of the *deployment being matched*: pick whatever the PHP
    /// application sharing the database is configured for, because its reader
    /// decrypts with its configured cipher rather than sniffing the payload.
    /// Reading is unaffected — both variants always open.
    pub enum PhpCipher: "php cipher" {
        /// `{iv, value, mac}` — AES-256-CBC with an HMAC checked first.
        /// PHP's long-standing default, and this type's.
        #[default]
        Aes256Cbc = "aes-256-cbc",

        /// `{iv, value, mac: "", tag}` — what PHP writes when configured for
        /// AES-256-GCM.
        Aes256Gcm = "aes-256-gcm",
    }
}

pub mod envelope;
pub mod primitive;

use envelope::{CbcDraft, Opened};
use rainier_support::{Error, Result};
use subtle::ConstantTimeEq;

use crate::encrypter::Encrypter;
use crate::key::{Key, KeyRing};

/// The envelope PHP MVC frameworks write, composed from [`envelope`] and
/// [`primitive`].
pub struct PhpEncrypter {
    keys: KeyRing,
    writes: PhpCipher,
}

impl PhpEncrypter {
    /// Read and write with `keys`, writing the CBC variant.
    ///
    /// The current key encrypts; **every** key on the ring is tried when
    /// decrypting, because the PHP payload carries no key id and there is no
    /// other way to support a rotation.
    pub fn new(keys: KeyRing) -> Self {
        Self { keys, writes: PhpCipher::Aes256Cbc }
    }

    /// Write with `cipher` instead. Reading is unaffected — both variants
    /// always open.
    #[must_use = "this returns a configured encrypter rather than configuring in place"]
    pub fn writing(mut self, cipher: PhpCipher) -> Self {
        self.writes = cipher;
        self
    }

    /// The key ring.
    pub fn keys(&self) -> &KeyRing {
        &self.keys
    }

    /// Which variant this writes.
    pub fn writes(&self) -> PhpCipher {
        self.writes
    }

    /// Encrypt under one specific key.
    fn encrypt_with(&self, key: &Key, plain: &[u8]) -> Result<String> {
        match self.writes {
            PhpCipher::Aes256Cbc => {
                let mut iv = [0u8; primitive::CBC_IV_LEN];
                rand::Rng::fill(&mut rand::thread_rng(), &mut iv[..]);

                let ciphertext = primitive::cbc_encrypt(key.bytes(), &iv, plain)?;

                // The draft hands out the exact bytes to MAC; which key MACs
                // them is this type's business, and hexing the result is the
                // envelope's.
                let draft = CbcDraft::new(&iv, &ciphertext);
                let mac = primitive::mac(key.bytes(), &draft.mac_covers())?;
                draft.seal(&mac)
            }
            PhpCipher::Aes256Gcm => {
                let mut nonce = [0u8; primitive::GCM_IV_LEN];
                rand::Rng::fill(&mut rand::thread_rng(), &mut nonce[..]);

                let (ciphertext, tag) = primitive::gcm_encrypt(key.bytes(), &nonce, plain)?;
                envelope::encode_gcm(&nonce, &ciphertext, &tag)
            }
        }
    }

    /// Decrypt under one specific key.
    fn decrypt_with(key: &Key, opened: &Opened) -> Result<Vec<u8>> {
        match opened {
            Opened::Cbc { iv, ciphertext, presented_mac, mac_covers } => {
                // Before decrypting, always. CBC without this is a padding
                // oracle, and a padding oracle recovers the plaintext without
                // the key.
                let expected = primitive::mac(key.bytes(), mac_covers)?;
                if !bool::from(presented_mac.ct_eq(&expected)) {
                    return Err(invalid());
                }

                primitive::cbc_decrypt(key.bytes(), iv, ciphertext).map_err(|_| invalid())
            }
            Opened::Gcm { iv, ciphertext, tag } => {
                primitive::gcm_decrypt(key.bytes(), iv, ciphertext, tag).map_err(|_| invalid())
            }
        }
    }
}

impl Encrypter for PhpEncrypter {
    fn encrypt_bytes(&self, plain: &[u8]) -> Result<String> {
        self.encrypt_with(self.keys.current(), plain)
    }

    fn decrypt_bytes(&self, payload: &str) -> Result<Vec<u8>> {
        let opened = envelope::decode(payload).map_err(|_| invalid())?;

        // Every key on the ring, current first. The PHP payload names no
        // key, so trying them is the only way a rotation can work at all —
        // and it is why rotating here costs a decrypt attempt per retired key
        // rather than being free the way the native format is.
        for key in self.keys.all() {
            if let Ok(plain) = Self::decrypt_with(key, &opened) {
                return Ok(plain);
            }
        }

        Err(invalid())
    }
}

impl std::fmt::Debug for PhpEncrypter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PhpEncrypter")
            .field("keys", &self.keys.ids())
            .field("writes", &self.writes)
            .finish()
    }
}

/// One error for every failure, so nothing distinguishes a bad MAC from bad
/// padding from a failed tag from the wrong key.
fn invalid() -> Error {
    Error::internal("could not decrypt the payload")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encrypter::EncrypterExt;
    use base64::engine::general_purpose::STANDARD as B64;
    use base64::Engine as _;

    fn encrypter() -> PhpEncrypter {
        PhpEncrypter::new(KeyRing::new(Key::generate()))
    }

    fn gcm_encrypter() -> PhpEncrypter {
        PhpEncrypter::new(KeyRing::new(Key::generate())).writing(PhpCipher::Aes256Gcm)
    }

    /// A key of known bytes, for the pinned vectors.
    fn fixed_key() -> Key {
        Key::from_base64("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=").unwrap()
    }

    #[test]
    fn a_value_round_trips() {
        let encrypter = encrypter();
        let payload = encrypter.encrypt("a card number").unwrap();

        assert_eq!(encrypter.decrypt(&payload).unwrap(), "a card number");
    }

    #[test]
    fn a_gcm_value_round_trips() {
        let encrypter = gcm_encrypter();
        let payload = encrypter.encrypt("a card number").unwrap();

        assert_eq!(encrypter.decrypt(&payload).unwrap(), "a card number");
    }

    #[test]
    fn either_writer_reads_the_other_variant() {
        // The payload says which variant it is; the write selection plays no
        // part in reading. This is what lets a table that changed cipher
        // generations ago open end to end.
        let keys = KeyRing::new(Key::generate());
        let cbc = PhpEncrypter::new(keys.clone());
        let gcm = PhpEncrypter::new(keys).writing(PhpCipher::Aes256Gcm);

        assert_eq!(gcm.decrypt(&cbc.encrypt("old row").unwrap()).unwrap(), "old row");
        assert_eq!(cbc.decrypt(&gcm.encrypt("new row").unwrap()).unwrap(), "new row");
    }

    #[test]
    fn the_cbc_payload_is_the_shape_php_writes() {
        let payload = encrypter().encrypt("hello").unwrap();

        let decoded = B64.decode(&payload).expect("base64 around the JSON");
        let json: serde_json::Value = serde_json::from_slice(&decoded).expect("JSON inside");

        assert!(json["iv"].is_string());
        assert!(json["value"].is_string());
        assert!(json["mac"].is_string());
        assert!(json.get("tag").is_none(), "{json}");

        // The IV is 16 raw bytes, base64'd — which is what a PHP reader will
        // pass straight to openssl_decrypt.
        assert_eq!(B64.decode(json["iv"].as_str().unwrap()).unwrap().len(), 16);
        // And the MAC is hex-encoded SHA-256: 64 characters.
        assert_eq!(json["mac"].as_str().unwrap().len(), 64);
    }

    #[test]
    fn the_gcm_payload_is_the_shape_php_writes() {
        // Including the empty `mac`: PHP's payload check insists the key
        // exists even where the tag does the authenticating, and omitting it
        // is the difference between a payload it opens and one it refuses.
        let payload = gcm_encrypter().encrypt("hello").unwrap();

        let decoded = B64.decode(&payload).unwrap();
        let json: serde_json::Value = serde_json::from_slice(&decoded).unwrap();

        assert_eq!(B64.decode(json["iv"].as_str().unwrap()).unwrap().len(), 12);
        assert_eq!(json["mac"], "");
        assert_eq!(B64.decode(json["tag"].as_str().unwrap()).unwrap().len(), 16);
    }

    #[test]
    fn a_payload_from_an_earlier_reimplementation_still_opens() {
        // Written by an identity provider's hand-rolled GCM crypter before
        // this type could read the variant — key 0x42×32, and no `mac` key at
        // all, which that implementation omitted where PHP writes `""`. A
        // real cross-implementation vector: those rows exist, and they have
        // to keep opening here.
        let key = Key::from_base64(&B64.encode([0x42u8; 32])).unwrap();
        let encrypter = PhpEncrypter::new(KeyRing::new(key));

        let vector = "eyJpdiI6InBWWTBIZ1VQa3RldzhRRTQiLCJ2YWx1ZSI6ImUxUEtqaHJ2Ni9DbGhaRWhlQT09IiwidGFnIjoiK1c0bGd6c0pLVTNmMzlMWXVQbDIxdz09In0=";

        assert_eq!(encrypter.decrypt(vector).unwrap(), "a card number");
    }

    #[test]
    fn the_mac_covers_the_base64_forms() {
        // The detail every reimplementation gets wrong. The rule itself lives
        // in the envelope module now; this pins the composition against an
        // independent HMAC.
        use hmac::Mac as _;

        let key = Key::generate();
        let payload = PhpEncrypter::new(KeyRing::new(key.clone())).encrypt("covered").unwrap();

        let json: serde_json::Value =
            serde_json::from_slice(&B64.decode(&payload).unwrap()).unwrap();

        let mut expected = hmac::Hmac::<sha2::Sha256>::new_from_slice(key.bytes()).unwrap();
        expected.update(json["iv"].as_str().unwrap().as_bytes());
        expected.update(json["value"].as_str().unwrap().as_bytes());
        let expected: String =
            expected.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect();

        assert_eq!(json["mac"].as_str().unwrap(), expected);
    }

    #[test]
    fn a_tampered_ciphertext_is_refused_rather_than_decrypted() {
        // The padding oracle this order of operations exists to close.
        let encrypter = encrypter();
        let payload = encrypter.encrypt("a card number").unwrap();

        let decoded = B64.decode(&payload).unwrap();
        let mut json: serde_json::Value = serde_json::from_slice(&decoded).unwrap();

        let mut ciphertext = B64.decode(json["value"].as_str().unwrap()).unwrap();
        ciphertext[0] ^= 0xff;
        json["value"] = serde_json::json!(B64.encode(&ciphertext));

        let tampered = B64.encode(serde_json::to_vec(&json).unwrap());

        assert!(encrypter.decrypt(&tampered).is_err());
    }

    #[test]
    fn a_tampered_gcm_tag_is_refused() {
        let encrypter = gcm_encrypter();
        let payload = encrypter.encrypt("a card number").unwrap();

        let decoded = B64.decode(&payload).unwrap();
        let mut json: serde_json::Value = serde_json::from_slice(&decoded).unwrap();

        let mut tag = B64.decode(json["tag"].as_str().unwrap()).unwrap();
        tag[0] ^= 0xff;
        json["tag"] = serde_json::json!(B64.encode(&tag));

        let tampered = B64.encode(serde_json::to_vec(&json).unwrap());

        assert!(encrypter.decrypt(&tampered).is_err());
    }

    #[test]
    fn another_applications_payload_does_not_decrypt() {
        let payload = encrypter().encrypt("a card number").unwrap();

        assert!(encrypter().decrypt(&payload).is_err());
    }

    #[test]
    fn a_retired_key_still_reads_what_it_wrote_in_either_variant() {
        // The PHP payload names no key, so this is a decrypt attempt per
        // retired key rather than a lookup — worth knowing before keeping ten
        // of them on the ring.
        let old = Key::generate();
        let cbc = PhpEncrypter::new(KeyRing::new(old.clone())).encrypt("secret").unwrap();
        let gcm = PhpEncrypter::new(KeyRing::new(old.clone()))
            .writing(PhpCipher::Aes256Gcm)
            .encrypt("secret")
            .unwrap();

        let rotated = PhpEncrypter::new(KeyRing::new(Key::generate()).with_previous(old));
        assert_eq!(rotated.decrypt(&cbc).unwrap(), "secret");
        assert_eq!(rotated.decrypt(&gcm).unwrap(), "secret");
    }

    #[test]
    fn every_failure_looks_the_same() {
        // A bad MAC, bad padding, a failed tag and the wrong key must be
        // indistinguishable: telling them apart is what a padding oracle is
        // built from.
        let encrypter = encrypter();

        let corrupt = B64.encode("{}");
        let wrong_key =
            PhpEncrypter::new(KeyRing::new(Key::generate())).encrypt("secret").unwrap();
        let wrong_key_gcm = gcm_encrypter().encrypt("secret").unwrap();

        let messages: std::collections::HashSet<String> = [corrupt, wrong_key, wrong_key_gcm]
            .iter()
            .map(|payload| encrypter.decrypt(payload).unwrap_err().message().to_string())
            .collect();

        assert_eq!(messages.len(), 1, "{messages:?}");
    }

    #[test]
    fn empty_and_long_values_round_trip_in_both_variants() {
        for encrypter in [encrypter(), gcm_encrypter()] {
            for plain in ["", "a", &"x".repeat(10_000), "héllo — ünicode"] {
                assert_eq!(
                    encrypter.decrypt(&encrypter.encrypt(plain).unwrap()).unwrap(),
                    plain,
                    "{:?}",
                    encrypter.writes()
                );
            }
        }
    }

    #[test]
    fn the_cbc_ciphertext_is_pinned_to_a_known_vector() {
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

        let ciphertext =
            primitive::cbc_encrypt(key.bytes(), &[0u8; 16], b"a card number").unwrap();
        assert_eq!(B64.encode(&ciphertext), "nH4xXUTQpgBTKXvgzml9eg==");

        // And the MAC over the two base64 strings, which is the part a PHP
        // reader recomputes.
        let draft = CbcDraft::new(&[0u8; 16], &ciphertext);
        let mac = primitive::mac(key.bytes(), &draft.mac_covers()).unwrap();
        let mac: String = mac.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(mac, "f648b5eac55d1dd21fe92175da4660271c245c3c0d23a9839e932eb583716aad");
    }

    #[test]
    fn the_write_selection_parses_the_way_a_dotenv_spells_it() {
        use rainier_support::Setting;

        assert_eq!(PhpCipher::parse("aes-256-cbc").unwrap(), PhpCipher::Aes256Cbc);
        assert_eq!(PhpCipher::parse("aes-256-gcm").unwrap(), PhpCipher::Aes256Gcm);
        assert!(PhpCipher::parse("aes-128-cbc").is_err());
    }
}
