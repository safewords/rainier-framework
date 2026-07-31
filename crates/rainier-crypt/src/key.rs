//! Keys and the [`KeyRing`] that holds them.
//!
//! A single key is a deployment that can never rotate. Everything here is
//! built around the assumption that keys *will* change: the ring has one
//! current key for encrypting and any number of retired keys for decrypting,
//! and every payload records which key produced it.

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use rainier_support::{Error, Result};
use sha2::{Digest, Sha256};

/// The length of every key, in bytes. 256 bits, for both XChaCha20-Poly1305
/// and HMAC-SHA256.
pub const KEY_LEN: usize = 32;

/// One symmetric key, plus the short identifier that travels with anything it
/// produced.
#[derive(Clone)]
pub struct Key {
    id: String,
    bytes: [u8; KEY_LEN],
}

impl Key {
    /// A key from exactly [`KEY_LEN`] bytes.
    pub fn from_bytes(bytes: [u8; KEY_LEN]) -> Self {
        Self { id: fingerprint(&bytes), bytes }
    }

    /// A key from a base64 string, with or without a `base64:` prefix.
    ///
    /// Accepting the prefix means a key generated for a PHP application
    /// can be pasted straight in, which matters when the two are being run
    /// side by side during a migration.
    pub fn from_base64(encoded: &str) -> Result<Self> {
        let trimmed = encoded.trim();
        let payload = trimmed.strip_prefix("base64:").unwrap_or(trimmed);

        let decoded = BASE64
            .decode(payload)
            .map_err(|_| Error::internal("the application key is not valid base64"))?;

        let bytes: [u8; KEY_LEN] = decoded.as_slice().try_into().map_err(|_| {
            Error::internal(format!(
                "the application key must be {KEY_LEN} bytes, but decoded to {}",
                decoded.len()
            ))
        })?;

        Ok(Self::from_bytes(bytes))
    }

    /// A fresh key from the OS CSPRNG. What `key:generate` calls.
    pub fn generate() -> Self {
        use rand::RngCore;

        let mut bytes = [0u8; KEY_LEN];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        Self::from_bytes(bytes)
    }

    /// The key's short identifier, as it appears in a payload.
    ///
    /// Derived from the key rather than configured alongside it, so it cannot
    /// be set inconsistently across deployments and cannot be forgotten when a
    /// key is added.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The raw key material.
    pub fn bytes(&self) -> &[u8; KEY_LEN] {
        &self.bytes
    }

    /// The key as `base64:…`, ready to paste into a `.env`.
    pub fn to_base64(&self) -> String {
        format!("base64:{}", BASE64.encode(self.bytes))
    }
}

/// Deliberately opaque: a key that lands in a log line or a panic message is a
/// key that has to be rotated.
impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Key").field("id", &self.id).field("bytes", &"<redacted>").finish()
    }
}

/// A hash of the key, truncated. Eight hex characters is enough to tell a
/// handful of keys apart and far too little to attack the key with.
fn fingerprint(bytes: &[u8; KEY_LEN]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"rainier-key-id\0");
    hasher.update(bytes);
    let digest = hasher.finalize();
    digest.iter().take(4).map(|byte| format!("{byte:02x}")).collect()
}

/// The current key, plus any retired ones still needed to read old payloads.
///
/// Rotation is the whole point. Adding a key to the front of the ring means
/// everything written from then on uses it, while everything already written
/// still decrypts — so a rotation is a deploy, not a migration.
#[derive(Clone, Debug)]
pub struct KeyRing {
    keys: Vec<Key>,
}

impl KeyRing {
    /// A ring with one key.
    pub fn new(current: Key) -> Self {
        Self { keys: vec![current] }
    }

    /// Add a retired key, usable for decryption only.
    ///
    /// Order among retired keys does not matter; lookup is by id.
    pub fn with_previous(mut self, key: Key) -> Self {
        self.keys.push(key);
        self
    }

    /// Build from an application key and any number of retired ones.
    pub fn from_base64(current: &str, previous: &[String]) -> Result<Self> {
        let mut ring = Self::new(Key::from_base64(current)?);
        for encoded in previous {
            if encoded.trim().is_empty() {
                continue;
            }
            ring = ring.with_previous(Key::from_base64(encoded)?);
        }
        Ok(ring)
    }

    /// The key new payloads are written with.
    pub fn current(&self) -> &Key {
        // The constructor requires one, and nothing can empty the ring.
        &self.keys[0]
    }

    /// The key with this id, if the ring still holds it.
    pub fn find(&self, id: &str) -> Option<&Key> {
        self.keys.iter().find(|key| key.id() == id)
    }

    /// How many keys the ring holds, current included.
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Always `false` — a ring is constructed with a current key.
    pub fn is_empty(&self) -> bool {
        false
    }

    /// Every key, current first.
    ///
    /// For a payload format that carries no key id and can only be decrypted
    /// by trying — the PHP envelope, for one. Rainier's own format names its
    /// key, so it uses [`find`](Self::find) instead.
    pub fn all(&self) -> impl Iterator<Item = &Key> {
        self.keys.iter()
    }

    /// Every key id, current first. For a diagnostic command.
    pub fn ids(&self) -> Vec<&str> {
        self.keys.iter().map(Key::id).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_generated_key_round_trips_through_base64() {
        let key = Key::generate();
        let restored = Key::from_base64(&key.to_base64()).unwrap();

        assert_eq!(restored.bytes(), key.bytes());
        assert_eq!(restored.id(), key.id());
    }

    #[test]
    fn the_base64_prefix_is_accepted_and_optional() {
        let key = Key::generate();
        let raw = BASE64.encode(key.bytes());

        assert_eq!(Key::from_base64(&raw).unwrap().bytes(), key.bytes());
        assert_eq!(Key::from_base64(&format!("base64:{raw}")).unwrap().bytes(), key.bytes());
    }

    #[test]
    fn a_key_of_the_wrong_length_is_rejected_with_its_length() {
        let short = BASE64.encode([0u8; 16]);
        let err = Key::from_base64(&short).unwrap_err();

        assert!(err.message().contains("32"), "{}", err.message());
        assert!(err.message().contains("16"), "{}", err.message());
    }

    #[test]
    fn rubbish_is_rejected_as_base64() {
        assert!(Key::from_base64("not base64 at all!!").is_err());
    }

    #[test]
    fn the_id_is_derived_from_the_key_and_is_stable() {
        let key = Key::from_bytes([7u8; KEY_LEN]);
        let same = Key::from_bytes([7u8; KEY_LEN]);
        let other = Key::from_bytes([8u8; KEY_LEN]);

        assert_eq!(key.id(), same.id());
        assert_ne!(key.id(), other.id());
        assert_eq!(key.id().len(), 8);
    }

    #[test]
    fn debug_does_not_disclose_the_key() {
        let key = Key::generate();
        let rendered = format!("{key:?}");

        assert!(rendered.contains("redacted"));
        assert!(!rendered.contains(&BASE64.encode(key.bytes())), "{rendered}");
        // The id is a hash of the key, so disclosing it is fine and useful.
        assert!(rendered.contains(key.id()), "{rendered}");
    }

    #[test]
    fn a_ring_finds_current_and_retired_keys_by_id() {
        let current = Key::generate();
        let old = Key::generate();
        let ring = KeyRing::new(current.clone()).with_previous(old.clone());

        assert_eq!(ring.current().id(), current.id());
        assert_eq!(ring.find(old.id()).unwrap().bytes(), old.bytes());
        assert!(ring.find("00000000").is_none());
        assert_eq!(ring.len(), 2);
    }

    #[test]
    fn empty_previous_entries_are_skipped() {
        let key = Key::generate().to_base64();
        let ring = KeyRing::from_base64(&key, &[String::new(), "   ".to_string()]).unwrap();

        assert_eq!(ring.len(), 1, "blank entries in a comma-separated list are not keys");
    }
}
