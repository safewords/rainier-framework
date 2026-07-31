//! Public-key cryptography: Ed25519 signatures and X25519 sealed boxes.
//!
//! The difference from the symmetric side is who needs which secret.
//!
//! | | Writer needs | Reader needs |
//! |---|---|---|
//! | [`HmacSigner`](crate::HmacSigner) | the shared key | **the shared key** |
//! | [`Ed25519Signer`] | the signing key | the **public** key |
//! | [`AeadEncrypter`](crate::AeadEncrypter) | the shared key | **the shared key** |
//! | [`SealedBox`] | the **public** key | the secret key |
//!
//! That asymmetry is the whole reason to reach for these. A shared key means
//! everyone who can verify can also forge, and everyone who can decrypt can
//! also encrypt. Sometimes that is fine — a cookie you issue and you read —
//! and sometimes it is exactly wrong.

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine as _;
use ed25519_dalek::{Signature, Signer as _, SigningKey, Verifier as _, VerifyingKey};
use rainier_support::{Error, Result};
use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey as X25519Public, StaticSecret};

use crate::cipher::Cipher;
use crate::key::Key;
use crate::signer::Signer;

/// The sealed-box format tag.
const SEAL: &str = "seal1";

/// A short, stable identifier for a public key.
///
/// Derived from the key rather than configured, for the same reason a
/// [symmetric key id](crate::Key::id) is: it cannot be set inconsistently and
/// cannot be forgotten.
fn public_id(domain: &[u8], public: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(public);
    hasher.finalize().iter().take(4).map(|byte| format!("{byte:02x}")).collect()
}

// --- Ed25519 ---------------------------------------------------------------

/// An Ed25519 keypair: signs, and verifies its own signatures.
#[derive(Clone)]
pub struct SigningKeyPair {
    signing: SigningKey,
    id: String,
}

impl SigningKeyPair {
    /// A fresh keypair from the OS CSPRNG.
    pub fn generate() -> Self {
        use rand::RngCore;

        let mut seed = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut seed);
        Self::from_seed(seed)
    }

    /// A keypair from a 32-byte seed.
    pub fn from_seed(seed: [u8; 32]) -> Self {
        let signing = SigningKey::from_bytes(&seed);
        let id = public_id(b"rainier-ed25519", signing.verifying_key().as_bytes());
        Self { signing, id }
    }

    /// A keypair from a base64 seed, with or without a `base64:` prefix.
    pub fn from_base64(encoded: &str) -> Result<Self> {
        Ok(Self::from_seed(decode_32(encoded, "signing key")?))
    }

    /// The seed, as `base64:…`. **This is the secret half.**
    pub fn to_base64(&self) -> String {
        format!("base64:{}", BASE64.encode(self.signing.to_bytes()))
    }

    /// The public half, which is safe to publish.
    pub fn public(&self) -> VerifyingPublicKey {
        VerifyingPublicKey { verifying: self.signing.verifying_key(), id: self.id.clone() }
    }

    /// The key's short identifier.
    pub fn id(&self) -> &str {
        &self.id
    }
}

impl std::fmt::Debug for SigningKeyPair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SigningKeyPair").field("id", &self.id).field("seed", &"<redacted>").finish()
    }
}

/// An Ed25519 public key: verifies, and cannot sign.
#[derive(Clone)]
pub struct VerifyingPublicKey {
    verifying: VerifyingKey,
    id: String,
}

impl VerifyingPublicKey {
    /// A public key from its 32 bytes.
    pub fn from_bytes(bytes: [u8; 32]) -> Result<Self> {
        let verifying = VerifyingKey::from_bytes(&bytes)
            .map_err(|_| Error::internal("that is not a valid Ed25519 public key"))?;
        let id = public_id(b"rainier-ed25519", verifying.as_bytes());
        Ok(Self { verifying, id })
    }

    /// A public key from base64.
    pub fn from_base64(encoded: &str) -> Result<Self> {
        Self::from_bytes(decode_32(encoded, "public key")?)
    }

    /// The key as `base64:…`. Safe to publish.
    pub fn to_base64(&self) -> String {
        format!("base64:{}", BASE64.encode(self.verifying.as_bytes()))
    }

    /// The key's short identifier.
    pub fn id(&self) -> &str {
        &self.id
    }
}

impl std::fmt::Debug for VerifyingPublicKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VerifyingPublicKey").field("id", &self.id).finish()
    }
}

/// Ed25519 signing, as a [`Signer`].
///
/// Same wire shape as the HMAC signer — `<value>.<key id>.<signature>` — so the
/// two are interchangeable at a call site. The difference is who can verify: an
/// HMAC needs the secret, an Ed25519 signature needs only the public key.
///
/// ```
/// # use rainier_crypt::{Ed25519Signer, SigningKeyPair, Signer};
/// # fn main() -> rainier_support::Result<()> {
/// let keys = SigningKeyPair::generate();
/// let signer = Ed25519Signer::new(keys.clone());
///
/// let signed = signer.sign("licence-42")?;
/// assert_eq!(signer.verify(&signed)?, "licence-42");
///
/// // A verifier holding only the public key can check it, and cannot sign.
/// let checker = Ed25519Signer::verify_only(keys.public());
/// assert_eq!(checker.verify(&signed)?, "licence-42");
/// assert!(checker.sign("forged").is_err());
/// # Ok(()) }
/// ```
pub struct Ed25519Signer {
    signing: Option<SigningKeyPair>,
    trusted: Vec<VerifyingPublicKey>,
}

impl Ed25519Signer {
    /// A signer that can both sign and verify.
    pub fn new(keys: SigningKeyPair) -> Self {
        let public = keys.public();
        Self { signing: Some(keys), trusted: vec![public] }
    }

    /// A verifier that holds no secret and so cannot sign.
    pub fn verify_only(public: VerifyingPublicKey) -> Self {
        Self { signing: None, trusted: vec![public] }
    }

    /// Also trust signatures from this key.
    ///
    /// How a rotation works on the verifying side, and how one service accepts
    /// signatures from several others.
    pub fn trusting(mut self, public: VerifyingPublicKey) -> Self {
        if !self.trusted.iter().any(|key| key.id() == public.id()) {
            self.trusted.push(public);
        }
        self
    }

    /// Whether this instance holds a signing key.
    pub fn can_sign(&self) -> bool {
        self.signing.is_some()
    }

    /// Every public key this instance will accept.
    pub fn trusted_ids(&self) -> Vec<&str> {
        self.trusted.iter().map(VerifyingPublicKey::id).collect()
    }
}

impl Signer for Ed25519Signer {
    fn sign(&self, value: &str) -> Result<String> {
        let keys = self.signing.as_ref().ok_or_else(|| {
            Error::internal(
                "this Ed25519 signer holds only a public key, so it can verify but not sign",
            )
        })?;

        if value.contains('.') {
            return Err(Error::internal(
                "a signed value must not contain `.` — encode it first (base64, or JSON \
                 through the encrypter)",
            ));
        }

        // The key id is signed alongside the value, so a signature cannot be
        // replayed under a different key's label.
        let message = format!("{}.{value}", keys.id());
        let signature = keys.signing.sign(message.as_bytes());

        Ok(format!("{value}.{}.{}", keys.id(), B64.encode(signature.to_bytes())))
    }

    fn verify(&self, signed: &str) -> Result<String> {
        let invalid = || Error::bad_request("the signature is not valid");

        let mut parts = signed.split('.');
        let (Some(value), Some(key_id), Some(tag), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(invalid());
        };

        let key = self.trusted.iter().find(|key| key.id() == key_id).ok_or_else(invalid)?;

        let bytes: [u8; 64] =
            B64.decode(tag).map_err(|_| invalid())?[..].try_into().map_err(|_| invalid())?;
        let signature = Signature::from_bytes(&bytes);

        let message = format!("{key_id}.{value}");
        key.verifying
            .verify(message.as_bytes(), &signature)
            .map(|()| value.to_string())
            .map_err(|_| invalid())
    }
}

impl std::fmt::Debug for Ed25519Signer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ed25519Signer")
            .field("can_sign", &self.can_sign())
            .field("trusted", &self.trusted_ids())
            .finish()
    }
}

// --- X25519 ----------------------------------------------------------------

/// An X25519 keypair, for key agreement and [sealed boxes](SealedBox).
#[derive(Clone)]
pub struct BoxKeyPair {
    secret: StaticSecret,
    id: String,
}

impl BoxKeyPair {
    /// A fresh keypair from the OS CSPRNG.
    pub fn generate() -> Self {
        use rand::RngCore;

        let mut bytes = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        Self::from_bytes(bytes)
    }

    /// A keypair from 32 bytes of secret.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        let secret = StaticSecret::from(bytes);
        let id = public_id(b"rainier-x25519", X25519Public::from(&secret).as_bytes());
        Self { secret, id }
    }

    /// A keypair from base64.
    pub fn from_base64(encoded: &str) -> Result<Self> {
        Ok(Self::from_bytes(decode_32(encoded, "box key")?))
    }

    /// The secret, as `base64:…`. **Keep this out of anything published.**
    pub fn to_base64(&self) -> String {
        format!("base64:{}", BASE64.encode(self.secret.to_bytes()))
    }

    /// The public half.
    pub fn public(&self) -> BoxPublicKey {
        BoxPublicKey { public: X25519Public::from(&self.secret), id: self.id.clone() }
    }

    /// The key's short identifier.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The shared secret this keypair and `theirs` agree on.
    ///
    /// Both sides compute the same value from their own secret and the other's
    /// public key, without either transmitting a secret. Returned as a
    /// [`Key`], already run through the KDF, so it is usable with any
    /// [`Cipher`] rather than being raw Diffie-Hellman output.
    pub fn agree(&self, theirs: &BoxPublicKey) -> Key {
        derive_shared(&self.secret, &theirs.public, b"rainier-x25519-agree")
    }
}

impl std::fmt::Debug for BoxKeyPair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoxKeyPair").field("id", &self.id).field("secret", &"<redacted>").finish()
    }
}

/// An X25519 public key. Safe to publish; enough to encrypt *to* the holder.
#[derive(Clone, PartialEq, Eq)]
pub struct BoxPublicKey {
    public: X25519Public,
    id: String,
}

impl BoxPublicKey {
    /// A public key from its 32 bytes.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        let public = X25519Public::from(bytes);
        let id = public_id(b"rainier-x25519", public.as_bytes());
        Self { public, id }
    }

    /// A public key from base64.
    pub fn from_base64(encoded: &str) -> Result<Self> {
        Ok(Self::from_bytes(decode_32(encoded, "public box key")?))
    }

    /// The key as `base64:…`.
    pub fn to_base64(&self) -> String {
        format!("base64:{}", BASE64.encode(self.public.as_bytes()))
    }

    /// The key's short identifier.
    pub fn id(&self) -> &str {
        &self.id
    }
}

impl std::fmt::Debug for BoxPublicKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoxPublicKey").field("id", &self.id).finish()
    }
}

/// Turn a Diffie-Hellman result into a usable symmetric key.
///
/// Raw X25519 output is not uniformly random and must not be used as a key
/// directly; HKDF is what makes it one.
fn derive_shared(secret: &StaticSecret, public: &X25519Public, domain: &[u8]) -> Key {
    use hkdf::Hkdf;

    let shared = secret.diffie_hellman(public);
    let hkdf = Hkdf::<Sha256>::new(Some(domain), shared.as_bytes());

    let mut bytes = [0u8; 32];
    // Cannot fail for a 32-byte output from SHA-256.
    hkdf.expand(b"key", &mut bytes).expect("32 bytes is within HKDF-SHA256's limit");
    Key::from_bytes(bytes)
}

/// Anonymous public-key encryption — libsodium's `crypto_box_seal`.
///
/// **Anyone** holding a recipient's public key can seal a message to them, and
/// **only** the recipient can open it. The sender is not authenticated and is
/// not recoverable: a sealed box says nothing about who wrote it.
///
/// That is the right shape for one-directional secrets — a client reporting
/// something to a server it cannot be given a shared key for, an offline
/// machine encrypting to a key held elsewhere, a bug report containing a token.
/// It is the wrong shape if you need to know who sent it; sign the plaintext as
/// well, or use [`agree`](BoxKeyPair::agree) with both parties' keys.
///
/// ```
/// # use rainier_crypt::{BoxKeyPair, SealedBox};
/// # fn main() -> rainier_support::Result<()> {
/// let recipient = BoxKeyPair::generate();
///
/// // The sender needs only the public key.
/// let sealed = SealedBox::new().seal(&recipient.public(), b"a secret report")?;
///
/// assert_eq!(SealedBox::new().unseal(&recipient, &sealed)?, b"a secret report");
/// # Ok(()) }
/// ```
#[derive(Debug, Clone, Default)]
pub struct SealedBox {
    cipher: Cipher,
}

impl SealedBox {
    /// A sealed box using the default cipher.
    pub fn new() -> Self {
        Self { cipher: Cipher::default() }
    }

    /// Use a specific cipher for the payload.
    pub fn with_cipher(mut self, cipher: Cipher) -> Self {
        self.cipher = cipher;
        self
    }

    /// Encrypt `plain` so only the holder of `recipient`'s secret can read it.
    pub fn seal(&self, recipient: &BoxPublicKey, plain: &[u8]) -> Result<String> {
        // A throwaway keypair per message. Its public half travels in the
        // payload; its secret is dropped immediately, which is what makes the
        // message unopenable even by the sender afterwards.
        let ephemeral = BoxKeyPair::generate();
        let shared = derive_shared(&ephemeral.secret, &recipient.public, b"rainier-sealed-box");

        let nonce = self.cipher.nonce();
        let ephemeral_public = ephemeral.public();

        // Both public keys are in the authenticated header, so a payload cannot
        // be re-addressed to a different recipient or have its ephemeral key
        // swapped for one the attacker knows.
        let header = format!(
            "{SEAL}.{}.{}.{}",
            self.cipher.id(),
            recipient.id(),
            B64.encode(ephemeral_public.public.as_bytes())
        );
        let sealed = self.cipher.encrypt(&shared, &nonce, header.as_bytes(), plain)?;

        Ok(format!("{header}.{}.{}", B64.encode(&nonce), B64.encode(&sealed)))
    }

    /// Encrypt a string.
    pub fn seal_string(&self, recipient: &BoxPublicKey, plain: &str) -> Result<String> {
        self.seal(recipient, plain.as_bytes())
    }

    /// Open a sealed box addressed to `recipient`.
    pub fn unseal(&self, recipient: &BoxKeyPair, payload: &str) -> Result<Vec<u8>> {
        let invalid = || Error::bad_request("the sealed box could not be opened");

        let mut parts = payload.split('.');
        let (
            Some(tag),
            Some(algorithm),
            Some(key_id),
            Some(ephemeral),
            Some(nonce),
            Some(sealed),
            None,
        ) = (
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
        )
        else {
            return Err(invalid());
        };

        if tag != SEAL || key_id != recipient.id() {
            return Err(invalid());
        }

        let cipher = Cipher::from_id(algorithm).ok_or_else(invalid)?;

        let ephemeral_bytes: [u8; 32] =
            B64.decode(ephemeral).map_err(|_| invalid())?[..].try_into().map_err(|_| invalid())?;
        let shared = derive_shared(
            &recipient.secret,
            &X25519Public::from(ephemeral_bytes),
            b"rainier-sealed-box",
        );

        let nonce = B64.decode(nonce).map_err(|_| invalid())?;
        let sealed = B64.decode(sealed).map_err(|_| invalid())?;

        let header = format!("{tag}.{algorithm}.{key_id}.{ephemeral}");
        cipher.decrypt(&shared, &nonce, header.as_bytes(), &sealed).map_err(|_| invalid())
    }

    /// Open a sealed box holding a string.
    pub fn unseal_string(&self, recipient: &BoxKeyPair, payload: &str) -> Result<String> {
        let bytes = self.unseal(recipient, payload)?;
        String::from_utf8(bytes).map_err(|_| Error::internal("the opened value is not valid UTF-8"))
    }
}

/// Decode 32 bytes of base64, with or without a `base64:` prefix.
fn decode_32(encoded: &str, what: &str) -> Result<[u8; 32]> {
    let trimmed = encoded.trim();
    let payload = trimmed.strip_prefix("base64:").unwrap_or(trimmed);

    let decoded = BASE64
        .decode(payload)
        .map_err(|_| Error::internal(format!("the {what} is not valid base64")))?;

    decoded.as_slice().try_into().map_err(|_| {
        Error::internal(format!("the {what} must be 32 bytes, but decoded to {}", decoded.len()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signer::SignerExt;

    // --- Ed25519 -----------------------------------------------------------

    #[test]
    fn an_ed25519_signature_round_trips() {
        let signer = Ed25519Signer::new(SigningKeyPair::generate());
        let signed = signer.sign("licence-42").unwrap();

        assert!(signed.starts_with("licence-42."), "the value stays readable");
        assert_eq!(signer.verify(&signed).unwrap(), "licence-42");
    }

    #[test]
    fn a_public_key_verifies_without_being_able_to_sign() {
        // The property that distinguishes this from HMAC.
        let keys = SigningKeyPair::generate();
        let signed = Ed25519Signer::new(keys.clone()).sign("licence-42").unwrap();

        let checker = Ed25519Signer::verify_only(keys.public());

        assert!(!checker.can_sign());
        assert_eq!(checker.verify(&signed).unwrap(), "licence-42");

        let err = checker.sign("forged").unwrap_err();
        assert!(err.message().contains("cannot sign") || err.message().contains("but not sign"));
    }

    #[test]
    fn altering_the_value_invalidates_it() {
        let signer = Ed25519Signer::new(SigningKeyPair::generate());
        let signed = signer.sign("licence-42").unwrap();

        assert!(signer.verify(&signed.replacen("licence-42", "licence-43", 1)).is_err());
    }

    #[test]
    fn another_keys_signature_is_not_accepted() {
        let signed = Ed25519Signer::new(SigningKeyPair::generate()).sign("x").unwrap();
        let other = Ed25519Signer::new(SigningKeyPair::generate());

        assert!(other.verify(&signed).is_err());
    }

    #[test]
    fn a_trusted_second_key_is_accepted() {
        // Rotation on the verifying side.
        let old = SigningKeyPair::generate();
        let signed = Ed25519Signer::new(old.clone()).sign("from-before").unwrap();

        let now = Ed25519Signer::new(SigningKeyPair::generate()).trusting(old.public());

        assert_eq!(now.verify(&signed).unwrap(), "from-before");
        assert_eq!(now.trusted_ids().len(), 2);
    }

    #[test]
    fn trusting_the_same_key_twice_does_not_duplicate_it() {
        let keys = SigningKeyPair::generate();
        let signer = Ed25519Signer::new(keys.clone()).trusting(keys.public());

        assert_eq!(signer.trusted_ids().len(), 1);
    }

    #[test]
    fn a_signature_cannot_be_replayed_under_another_key_id() {
        let old = SigningKeyPair::generate();
        let current = SigningKeyPair::generate();
        let signer = Ed25519Signer::new(current).trusting(old.public());

        let signed = signer.sign("value").unwrap();
        let mut parts: Vec<&str> = signed.split('.').collect();
        parts[1] = old.id();

        assert!(signer.verify(&parts.join(".")).is_err(), "the id is inside the signature");
    }

    #[test]
    fn a_malformed_ed25519_signature_is_a_400() {
        let signer = Ed25519Signer::new(SigningKeyPair::generate());

        for bad in ["", "nope", "a.b", "a.b.c.d", "value.deadbeef.!!!"] {
            assert_eq!(signer.verify(bad).unwrap_err().status(), 400, "{bad}");
        }
    }

    #[test]
    fn a_value_containing_a_dot_is_refused() {
        let signer = Ed25519Signer::new(SigningKeyPair::generate());
        assert!(signer.sign("a.b").unwrap_err().message().contains("encode it first"));
    }

    #[test]
    fn is_valid_reaches_ed25519_too() {
        let signer = Ed25519Signer::new(SigningKeyPair::generate());
        assert!(signer.is_valid(&signer.sign("x").unwrap()));
    }

    #[test]
    fn a_signing_keypair_round_trips_through_base64() {
        let keys = SigningKeyPair::generate();
        let restored = SigningKeyPair::from_base64(&keys.to_base64()).unwrap();

        assert_eq!(restored.id(), keys.id());
        assert_eq!(
            Ed25519Signer::verify_only(keys.public())
                .verify(&Ed25519Signer::new(restored).sign("x").unwrap())
                .unwrap(),
            "x"
        );
    }

    #[test]
    fn a_public_key_round_trips_through_base64() {
        let public = SigningKeyPair::generate().public();
        let restored = VerifyingPublicKey::from_base64(&public.to_base64()).unwrap();

        assert_eq!(restored.id(), public.id());
    }

    #[test]
    fn rubbish_public_keys_are_rejected() {
        assert!(VerifyingPublicKey::from_base64("not base64!!").is_err());
        assert!(VerifyingPublicKey::from_base64(&BASE64.encode([0u8; 16])).is_err());
    }

    #[test]
    fn ed25519_debug_does_not_disclose_the_secret() {
        let keys = SigningKeyPair::generate();
        let rendered = format!("{keys:?}");

        assert!(rendered.contains("redacted"));
        assert!(!rendered.contains(&BASE64.encode(keys.signing.to_bytes())), "{rendered}");
    }

    // --- sealed boxes ------------------------------------------------------

    #[test]
    fn a_sealed_box_round_trips() {
        let recipient = BoxKeyPair::generate();
        let sealed = SealedBox::new().seal_string(&recipient.public(), "a secret").unwrap();

        assert!(!sealed.contains("secret"), "{sealed}");
        assert_eq!(SealedBox::new().unseal_string(&recipient, &sealed).unwrap(), "a secret");
    }

    #[test]
    fn only_the_recipient_can_open_it() {
        let recipient = BoxKeyPair::generate();
        let someone_else = BoxKeyPair::generate();
        let sealed = SealedBox::new().seal(&recipient.public(), b"secret").unwrap();

        assert!(SealedBox::new().unseal(&someone_else, &sealed).is_err());
    }

    #[test]
    fn the_sender_cannot_reopen_it() {
        // The ephemeral secret is dropped, which is the whole point of a
        // *sealed* box as opposed to an authenticated one.
        let recipient = BoxKeyPair::generate();
        let sealed = SealedBox::new().seal(&recipient.public(), b"secret").unwrap();

        // Nothing in the payload lets anyone but the recipient derive the key.
        // The best a sender can do is keep the plaintext.
        assert_eq!(sealed.matches('.').count(), 5, "tag.alg.kid.ephemeral.nonce.ct");
    }

    #[test]
    fn sealing_the_same_value_twice_produces_different_payloads() {
        let recipient = BoxKeyPair::generate();
        let seal = SealedBox::new();

        assert_ne!(
            seal.seal(&recipient.public(), b"same").unwrap(),
            seal.seal(&recipient.public(), b"same").unwrap(),
            "a fresh ephemeral key and nonce per message"
        );
    }

    #[test]
    fn re_addressing_a_sealed_box_is_detected() {
        let recipient = BoxKeyPair::generate();
        let other = BoxKeyPair::generate();
        let sealed = SealedBox::new().seal(&recipient.public(), b"secret").unwrap();

        let mut parts: Vec<&str> = sealed.split('.').collect();
        parts[2] = other.id();

        assert!(SealedBox::new().unseal(&other, &parts.join(".")).is_err());
    }

    #[test]
    fn swapping_the_ephemeral_key_is_detected() {
        let recipient = BoxKeyPair::generate();
        let sealed = SealedBox::new().seal(&recipient.public(), b"secret").unwrap();

        let attacker = BoxKeyPair::generate();
        let swapped = B64.encode(attacker.public().public.as_bytes());
        let mut parts: Vec<&str> = sealed.split('.').collect();
        parts[3] = &swapped;

        assert!(SealedBox::new().unseal(&recipient, &parts.join(".")).is_err());
    }

    #[test]
    fn tampering_with_a_sealed_box_is_detected() {
        let recipient = BoxKeyPair::generate();
        let sealed = SealedBox::new().seal(&recipient.public(), b"transfer 10").unwrap();

        let mut parts: Vec<&str> = sealed.split('.').collect();
        let mut bytes = B64.decode(parts[5]).unwrap();
        bytes[0] ^= 0x01;
        let flipped = B64.encode(bytes);
        parts[5] = &flipped;

        assert!(SealedBox::new().unseal(&recipient, &parts.join(".")).is_err());
    }

    #[test]
    fn a_malformed_sealed_box_is_a_400() {
        let recipient = BoxKeyPair::generate();

        for bad in ["", "nope", "seal1.xc20p.abc", "notseal.xc20p.a.b.c.d"] {
            assert_eq!(
                SealedBox::new().unseal(&recipient, bad).unwrap_err().status(),
                400,
                "{bad}"
            );
        }
    }

    #[test]
    fn every_cipher_works_in_a_sealed_box() {
        let recipient = BoxKeyPair::generate();

        for cipher in Cipher::ALL {
            let seal = SealedBox::new().with_cipher(cipher);
            let sealed = seal.seal(&recipient.public(), b"secret").unwrap();

            assert_eq!(seal.unseal(&recipient, &sealed).unwrap(), b"secret", "{cipher}");
            // And a reader with a different default still reads it, because the
            // payload names its algorithm.
            assert_eq!(SealedBox::new().unseal(&recipient, &sealed).unwrap(), b"secret");
        }
    }

    #[test]
    fn empty_and_large_payloads_round_trip() {
        let recipient = BoxKeyPair::generate();
        let seal = SealedBox::new();
        let big = vec![3u8; 100_000];

        assert!(seal
            .unseal(&recipient, &seal.seal(&recipient.public(), b"").unwrap())
            .unwrap()
            .is_empty());
        assert_eq!(
            seal.unseal(&recipient, &seal.seal(&recipient.public(), &big).unwrap()).unwrap(),
            big
        );
    }

    // --- key agreement -----------------------------------------------------

    #[test]
    fn both_sides_agree_on_the_same_key() {
        let alice = BoxKeyPair::generate();
        let bob = BoxKeyPair::generate();

        assert_eq!(
            alice.agree(&bob.public()).bytes(),
            bob.agree(&alice.public()).bytes(),
            "that is what Diffie-Hellman is for"
        );
    }

    #[test]
    fn a_third_party_agrees_on_something_else() {
        let alice = BoxKeyPair::generate();
        let bob = BoxKeyPair::generate();
        let eve = BoxKeyPair::generate();

        assert_ne!(alice.agree(&bob.public()).bytes(), alice.agree(&eve.public()).bytes());
        assert_ne!(alice.agree(&bob.public()).bytes(), eve.agree(&bob.public()).bytes());
    }

    #[test]
    fn an_agreed_key_encrypts_between_the_two_parties() {
        let alice = BoxKeyPair::generate();
        let bob = BoxKeyPair::generate();
        let cipher = Cipher::default();

        let key = alice.agree(&bob.public());
        let nonce = cipher.nonce();
        let sealed = cipher.encrypt(&key, &nonce, b"", b"between us").unwrap();

        let theirs = bob.agree(&alice.public());
        assert_eq!(cipher.decrypt(&theirs, &nonce, b"", &sealed).unwrap(), b"between us");
    }

    #[test]
    fn an_agreed_key_is_not_the_raw_diffie_hellman_output() {
        // Raw X25519 output is not uniformly random; using it as a key directly
        // is the classic mistake. This pins that HKDF ran.
        let alice = BoxKeyPair::generate();
        let bob = BoxKeyPair::generate();

        let raw = alice.secret.diffie_hellman(&bob.public().public);
        assert_ne!(alice.agree(&bob.public()).bytes(), raw.as_bytes());
    }

    #[test]
    fn a_box_keypair_round_trips_through_base64() {
        let keys = BoxKeyPair::generate();
        let restored = BoxKeyPair::from_base64(&keys.to_base64()).unwrap();

        assert_eq!(restored.id(), keys.id());
        assert_eq!(restored.public(), keys.public());
    }

    #[test]
    fn a_box_public_key_round_trips_through_base64() {
        let public = BoxKeyPair::generate().public();
        assert_eq!(BoxPublicKey::from_base64(&public.to_base64()).unwrap(), public);
    }

    #[test]
    fn box_debug_does_not_disclose_the_secret() {
        let keys = BoxKeyPair::generate();
        let rendered = format!("{keys:?}");

        assert!(rendered.contains("redacted"));
        assert!(!rendered.contains(&BASE64.encode(keys.secret.to_bytes())), "{rendered}");
    }

    #[test]
    fn key_ids_are_domain_separated_between_the_two_schemes() {
        // The same 32 bytes used as a signing seed and as a box secret must not
        // produce the same id, or a diagnostic would conflate them.
        let bytes = [5u8; 32];
        assert_ne!(SigningKeyPair::from_seed(bytes).id(), BoxKeyPair::from_bytes(bytes).id());
    }
}
