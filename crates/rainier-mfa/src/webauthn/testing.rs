//! A software authenticator — [`SoftwareAuthenticator`] — for testing passkey
//! flows without a browser or a security key.
//!
//! It does what a real authenticator does with the options a
//! [`RelyingParty`](super::RelyingParty) issues: builds authenticator data,
//! CBOR-encodes an attestation object, signs, and answers with the JSON
//! `PublicKeyCredential.toJSON()` would produce. The answer is a
//! `serde_json::Value`, so a test can bend any field and assert the relying
//! party notices.
//!
//! ```
//! use rainier_mfa::webauthn::{testing::SoftwareAuthenticator, *};
//!
//! let rp = RelyingParty::new("example.com", "Example");
//! let mut authenticator = SoftwareAuthenticator::es256("example.com", "https://example.com");
//!
//! let user = PasskeyUser::new(b"user-1".to_vec(), "ada@example.com", "Ada");
//! let (options, state) = rp.start_registration(&user, &[]);
//! let created = authenticator.register(&options);
//! let credential = rp
//!     .finish_registration(&state, &RegistrationResponse::from_json(&created).unwrap())
//!     .unwrap();
//!
//! let (options, state) = rp.start_authentication(&[]);
//! let signed = authenticator.authenticate(&options);
//! let response = AuthenticationResponse::from_json(&signed).unwrap();
//! assert!(rp.finish_authentication(&state, &response, &credential).is_ok());
//! ```
//!
//! **Never use it outside tests.** Its keys live in memory and it verifies
//! nobody.

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use base64::Engine as _;
use serde_json::{json, Value as Json};
use sha2::{Digest, Sha256};

use crate::cbor::{self, Value};
use crate::cose::{CoseAlgorithm, CoseKey};

/// A private key of one of the three passkey algorithms.
pub struct SoftwareKey(Inner);

enum Inner {
    Es256(p256::ecdsa::SigningKey),
    Ed25519(ed25519_dalek::SigningKey),
    Rs256(Box<rsa::RsaPrivateKey>),
}

impl SoftwareKey {
    /// A fresh P-256 key.
    pub fn es256() -> Self {
        loop {
            // A random 32 bytes is a valid scalar unless it is zero or above
            // the group order — vanishingly rare, but retried rather than
            // assumed.
            if let Ok(key) = p256::ecdsa::SigningKey::from_slice(&crate::random_bytes(32)) {
                return Self(Inner::Es256(key));
            }
        }
    }

    /// A fresh Ed25519 key.
    pub fn ed25519() -> Self {
        let seed: [u8; 32] = crate::random_bytes(32).try_into().expect("thirty-two bytes");
        Self(Inner::Ed25519(ed25519_dalek::SigningKey::from_bytes(&seed)))
    }

    /// A fresh 2048-bit RSA key. Slow in a debug build — a second or two.
    pub fn rs256() -> Self {
        let key = rsa::RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048)
            .expect("an RSA key generates");
        Self(Inner::Rs256(Box::new(key)))
    }

    /// The algorithm.
    pub fn algorithm(&self) -> CoseAlgorithm {
        match &self.0 {
            Inner::Es256(_) => CoseAlgorithm::Es256,
            Inner::Ed25519(_) => CoseAlgorithm::EdDsa,
            Inner::Rs256(_) => CoseAlgorithm::Rs256,
        }
    }

    /// The public key, COSE-encoded.
    pub fn cose(&self) -> Vec<u8> {
        let key = match &self.0 {
            Inner::Es256(key) => {
                let point = key.verifying_key().to_sec1_point(false);
                let bytes = point.as_bytes();
                CoseKey::Es256 {
                    x: bytes[1..33].try_into().expect("x"),
                    y: bytes[33..65].try_into().expect("y"),
                }
            }
            Inner::Ed25519(key) => CoseKey::Ed25519 { x: key.verifying_key().to_bytes() },
            Inner::Rs256(key) => {
                use rsa::traits::PublicKeyParts as _;
                CoseKey::Rs256 { n: key.n().to_bytes_be(), e: key.e().to_bytes_be() }
            }
        };
        key.to_bytes()
    }

    /// Sign `message` the way WebAuthn wants: DER for ES256, raw otherwise.
    pub fn sign(&self, message: &[u8]) -> Vec<u8> {
        match &self.0 {
            Inner::Es256(key) => {
                use p256::ecdsa::signature::Signer as _;
                let signature: p256::ecdsa::Signature = key.sign(message);
                signature.to_der().as_bytes().to_vec()
            }
            Inner::Ed25519(key) => {
                use ed25519_dalek::Signer as _;
                key.sign(message).to_bytes().to_vec()
            }
            Inner::Rs256(key) => {
                let hashed = Sha256::digest(message);
                key.sign(rsa::Pkcs1v15Sign::new::<rsa::sha2::Sha256>(), &hashed).expect("RSA signs")
            }
        }
    }
}

/// The attestation statement a [`SoftwareAuthenticator`] produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attestation {
    /// `none`, with an empty statement — what passkey providers send.
    None,
    /// `packed` self-attestation, signed by the credential key.
    PackedSelf,
    /// A format this crate does not verify, `fido-u2f`, with a junk statement.
    Unverifiable,
}

/// An authenticator holding one credential.
pub struct SoftwareAuthenticator {
    key: SoftwareKey,
    rp_id: String,
    origin: String,
    credential_id: Vec<u8>,
    user_handle: Vec<u8>,
    /// The next signature counter; `None` reports zero forever, like a synced
    /// passkey.
    counter: Option<u32>,
    user_verified: bool,
    backup: (bool, bool),
    attestation: Attestation,
}

impl SoftwareAuthenticator {
    /// An authenticator with a fresh key, for the RP `rp_id`, whose browser
    /// reports `origin`.
    pub fn new(key: SoftwareKey, rp_id: impl Into<String>, origin: impl Into<String>) -> Self {
        Self {
            key,
            rp_id: rp_id.into(),
            origin: origin.into(),
            credential_id: crate::random_bytes(16),
            user_handle: Vec::new(),
            counter: None,
            user_verified: true,
            backup: (true, true),
            attestation: Attestation::None,
        }
    }

    /// The common case: a P-256 synced passkey.
    pub fn es256(rp_id: impl Into<String>, origin: impl Into<String>) -> Self {
        Self::new(SoftwareKey::es256(), rp_id, origin)
    }

    /// Count signatures from `start`, like a hardware key.
    #[must_use]
    pub fn counting_from(mut self, start: u32) -> Self {
        self.counter = Some(start);
        self
    }

    /// Whether the user-verified flag is set. Set by default.
    #[must_use]
    pub fn verifying_user(mut self, verified: bool) -> Self {
        self.user_verified = verified;
        self
    }

    /// The backup-eligible and backed-up flags. Both set by default.
    #[must_use]
    pub fn backup_flags(mut self, eligible: bool, backed_up: bool) -> Self {
        self.backup = (eligible, backed_up);
        self
    }

    /// The attestation statement to produce.
    #[must_use]
    pub fn attesting(mut self, attestation: Attestation) -> Self {
        self.attestation = attestation;
        self
    }

    /// Report a different origin from now on — a phishing page, say.
    pub fn set_origin(&mut self, origin: impl Into<String>) {
        self.origin = origin.into();
    }

    /// Wind the signature counter to `value`.
    pub fn set_counter(&mut self, value: u32) {
        self.counter = Some(value);
    }

    /// The credential id.
    pub fn credential_id(&self) -> &[u8] {
        &self.credential_id
    }

    /// Answer `navigator.credentials.create()` for these options.
    pub fn register(&mut self, options: &Json) -> Json {
        self.user_handle = bytes(&options["user"]["id"]);
        let client_data = self.client_data("webauthn.create", &options["challenge"]);

        let mut attested = Vec::new();
        attested.extend_from_slice(&[0u8; 16]);
        attested.extend_from_slice(&(self.credential_id.len() as u16).to_be_bytes());
        attested.extend_from_slice(&self.credential_id);
        attested.extend_from_slice(&self.key.cose());
        let auth_data = self.auth_data(0x40, &attested);

        let client_data_hash = Sha256::digest(&client_data);
        let (format, statement) = match self.attestation {
            Attestation::None => ("none", Value::Map(Vec::new())),
            Attestation::PackedSelf => {
                let mut signed = auth_data.clone();
                signed.extend_from_slice(&client_data_hash);
                (
                    "packed",
                    Value::Map(vec![
                        (Value::Text("alg".into()), int(self.key.algorithm().id())),
                        (Value::Text("sig".into()), Value::Bytes(self.key.sign(&signed))),
                    ]),
                )
            }
            Attestation::Unverifiable => (
                "fido-u2f",
                Value::Map(vec![(Value::Text("sig".into()), Value::Bytes(vec![0; 8]))]),
            ),
        };

        let object = cbor::encode(&Value::Map(vec![
            (Value::Text("fmt".into()), Value::Text(format.into())),
            (Value::Text("attStmt".into()), statement),
            (Value::Text("authData".into()), Value::Bytes(auth_data)),
        ]));

        let id = B64URL.encode(&self.credential_id);
        json!({
            "id": id,
            "rawId": id,
            "type": "public-key",
            "response": {
                "clientDataJSON": B64URL.encode(&client_data),
                "attestationObject": B64URL.encode(object),
                "transports": ["internal", "hybrid"],
            },
            "authenticatorAttachment": "platform",
            "clientExtensionResults": {},
        })
    }

    /// Answer `navigator.credentials.get()` for these options.
    pub fn authenticate(&mut self, options: &Json) -> Json {
        let client_data = self.client_data("webauthn.get", &options["challenge"]);
        let auth_data = self.auth_data(0, &[]);

        let mut signed = auth_data.clone();
        signed.extend_from_slice(&Sha256::digest(&client_data));
        let signature = self.key.sign(&signed);

        let id = B64URL.encode(&self.credential_id);
        json!({
            "id": id,
            "rawId": id,
            "type": "public-key",
            "response": {
                "clientDataJSON": B64URL.encode(&client_data),
                "authenticatorData": B64URL.encode(&auth_data),
                "signature": B64URL.encode(signature),
                "userHandle": B64URL.encode(&self.user_handle),
            },
            "authenticatorAttachment": "platform",
            "clientExtensionResults": {},
        })
    }

    fn client_data(&self, kind: &str, challenge: &Json) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "type": kind,
            "challenge": challenge.as_str().unwrap_or_default(),
            "origin": self.origin,
            "crossOrigin": false,
        }))
        .expect("JSON serialises")
    }

    fn auth_data(&mut self, extra_flags: u8, tail: &[u8]) -> Vec<u8> {
        let mut flags = 0x01 | extra_flags;
        if self.user_verified {
            flags |= 0x04;
        }
        if self.backup.0 {
            flags |= 0x08;
        }
        if self.backup.1 {
            flags |= 0x10;
        }

        let count = match &mut self.counter {
            Some(counter) => {
                *counter += 1;
                *counter
            }
            None => 0,
        };

        let mut data = Sha256::digest(self.rp_id.as_bytes()).to_vec();
        data.push(flags);
        data.extend_from_slice(&count.to_be_bytes());
        data.extend_from_slice(tail);
        data
    }
}

fn bytes(value: &Json) -> Vec<u8> {
    B64URL.decode(value.as_str().unwrap_or_default()).unwrap_or_default()
}

fn int(value: i64) -> Value {
    if value < 0 {
        Value::Negative(i128::from(value))
    } else {
        Value::Unsigned(value as u64)
    }
}
