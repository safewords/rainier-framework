//! A passkey's public key, in the COSE form an authenticator hands over —
//! [`CoseKey`].
//!
//! Three algorithms, which between them cover every passkey provider and
//! security key in circulation:
//!
//! | COSE `alg` | Name | Key |
//! |---|---|---|
//! | `-7` | ES256 | ECDSA on P-256, SHA-256 — almost everything |
//! | `-8` | EdDSA | Ed25519 — some security keys |
//! | `-257` | RS256 | RSASSA-PKCS1-v1_5, SHA-256 — Windows Hello |
//!
//! A key is checked when it is parsed — the point is on the curve, the RSA
//! modulus is at least 2048 bits — so a malformed key is refused at
//! registration rather than discovered at the first sign-in.

use rainier_support::{Error, Result};

use crate::cbor::{self, Value};

/// A signature algorithm, by its COSE identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CoseAlgorithm {
    /// ECDSA P-256 with SHA-256.
    Es256,
    /// Ed25519.
    EdDsa,
    /// RSASSA-PKCS1-v1_5 with SHA-256.
    Rs256,
}

impl CoseAlgorithm {
    /// Every algorithm this crate verifies, in the order registration
    /// offers them.
    pub const ALL: [CoseAlgorithm; 3] = [Self::Es256, Self::EdDsa, Self::Rs256];

    /// The COSE identifier.
    pub fn id(self) -> i64 {
        match self {
            Self::Es256 => -7,
            Self::EdDsa => -8,
            Self::Rs256 => -257,
        }
    }

    /// The algorithm with this COSE identifier, if it is one of the three.
    pub fn from_id(id: i128) -> Option<Self> {
        match id {
            -7 => Some(Self::Es256),
            -8 => Some(Self::EdDsa),
            -257 => Some(Self::Rs256),
            _ => None,
        }
    }
}

/// A parsed, validated public key.
#[derive(Clone, PartialEq, Eq)]
pub enum CoseKey {
    /// A P-256 point.
    Es256 {
        /// The x coordinate.
        x: [u8; 32],
        /// The y coordinate.
        y: [u8; 32],
    },
    /// An Ed25519 public key.
    Ed25519 {
        /// The encoded point.
        x: [u8; 32],
    },
    /// An RSA public key.
    Rs256 {
        /// The modulus, big-endian.
        n: Vec<u8>,
        /// The public exponent, big-endian.
        e: Vec<u8>,
    },
}

impl CoseKey {
    /// Parse and validate COSE key bytes.
    ///
    /// # Errors
    ///
    /// Not CBOR, not a key, an algorithm other than the three, or a key that
    /// fails validation.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        Self::from_value(&cbor::decode(bytes)?)
    }

    /// Parse and validate an already-decoded COSE key.
    ///
    /// # Errors
    ///
    /// As [`from_bytes`](Self::from_bytes).
    pub fn from_value(value: &Value) -> Result<Self> {
        let int = |key| value.get_int(key).and_then(Value::as_int);
        let bytes = |key| value.get_int(key).and_then(Value::as_bytes);

        let kty = int(1).ok_or_else(|| invalid("no key type"))?;
        let alg = int(3)
            .and_then(CoseAlgorithm::from_id)
            .ok_or_else(|| invalid("an algorithm other than ES256, EdDSA or RS256"))?;

        let key = match (kty, alg) {
            (2, CoseAlgorithm::Es256) => {
                if int(-1) != Some(1) {
                    return Err(invalid("an EC2 key on a curve other than P-256"));
                }
                let x = fixed(bytes(-2), "x")?;
                let y = fixed(bytes(-3), "y")?;
                Self::Es256 { x, y }
            }
            (1, CoseAlgorithm::EdDsa) => {
                if int(-1) != Some(6) {
                    return Err(invalid("an OKP key on a curve other than Ed25519"));
                }
                Self::Ed25519 { x: fixed(bytes(-2), "x")? }
            }
            (3, CoseAlgorithm::Rs256) => {
                let n = bytes(-1).ok_or_else(|| invalid("an RSA key with no modulus"))?.to_vec();
                let e = bytes(-2).ok_or_else(|| invalid("an RSA key with no exponent"))?.to_vec();
                Self::Rs256 { n, e }
            }
            _ => return Err(invalid("a key type that does not match its algorithm")),
        };

        key.validate()?;
        Ok(key)
    }

    /// The algorithm this key signs with.
    pub fn algorithm(&self) -> CoseAlgorithm {
        match self {
            Self::Es256 { .. } => CoseAlgorithm::Es256,
            Self::Ed25519 { .. } => CoseAlgorithm::EdDsa,
            Self::Rs256 { .. } => CoseAlgorithm::Rs256,
        }
    }

    /// The canonical COSE encoding.
    pub fn to_bytes(&self) -> Vec<u8> {
        let entries = match self {
            Self::Es256 { x, y } => vec![
                (Value::Unsigned(1), Value::Unsigned(2)),
                (Value::Unsigned(3), Value::Negative(-7)),
                (Value::Negative(-1), Value::Unsigned(1)),
                (Value::Negative(-2), Value::Bytes(x.to_vec())),
                (Value::Negative(-3), Value::Bytes(y.to_vec())),
            ],
            Self::Ed25519 { x } => vec![
                (Value::Unsigned(1), Value::Unsigned(1)),
                (Value::Unsigned(3), Value::Negative(-8)),
                (Value::Negative(-1), Value::Unsigned(6)),
                (Value::Negative(-2), Value::Bytes(x.to_vec())),
            ],
            Self::Rs256 { n, e } => vec![
                (Value::Unsigned(1), Value::Unsigned(3)),
                (Value::Unsigned(3), Value::Negative(-257)),
                (Value::Negative(-1), Value::Bytes(n.clone())),
                (Value::Negative(-2), Value::Bytes(e.clone())),
            ],
        };
        cbor::encode(&Value::Map(entries))
    }

    /// Verify `signature` over `message`.
    ///
    /// ES256 signatures are DER, as WebAuthn specifies; EdDSA and RS256 are
    /// raw. Ed25519 is verified strictly, refusing the malleable encodings
    /// lax verification accepts.
    ///
    /// # Errors
    ///
    /// A signature that does not verify — one error for every reason.
    pub fn verify(&self, message: &[u8], signature: &[u8]) -> Result<()> {
        let verified = match self {
            Self::Es256 { x, y } => {
                use p256::ecdsa::signature::Verifier as _;
                use p256::ecdsa::{Signature, VerifyingKey};

                let key = VerifyingKey::from_sec1_bytes(&sec1(x, y)).ok();
                let signature = Signature::from_der(signature).ok();
                match (key, signature) {
                    (Some(key), Some(signature)) => key.verify(message, &signature).is_ok(),
                    _ => false,
                }
            }
            Self::Ed25519 { x } => {
                let key = ed25519_dalek::VerifyingKey::from_bytes(x).ok();
                let signature = ed25519_dalek::Signature::from_slice(signature).ok();
                match (key, signature) {
                    (Some(key), Some(signature)) => key.verify_strict(message, &signature).is_ok(),
                    _ => false,
                }
            }
            Self::Rs256 { n, e } => match rsa_key(n, e) {
                Ok(key) => {
                    use sha2::Digest as _;

                    let hashed = sha2::Sha256::digest(message);
                    key.verify(rsa::Pkcs1v15Sign::new::<rsa::sha2::Sha256>(), &hashed, signature)
                        .is_ok()
                }
                Err(_) => false,
            },
        };

        if verified {
            Ok(())
        } else {
            Err(Error::unauthorized("The passkey signature did not verify."))
        }
    }

    fn validate(&self) -> Result<()> {
        match self {
            Self::Es256 { x, y } => p256::ecdsa::VerifyingKey::from_sec1_bytes(&sec1(x, y))
                .map(|_| ())
                .map_err(|_| invalid("a P-256 point that is not on the curve")),
            Self::Ed25519 { x } => ed25519_dalek::VerifyingKey::from_bytes(x)
                .map(|_| ())
                .map_err(|_| invalid("an Ed25519 key that is not a valid point")),
            Self::Rs256 { n, e } => rsa_key(n, e).map(|_| ()),
        }
    }
}

impl std::fmt::Debug for CoseKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // A public key is not a secret, but a 256-byte modulus is not a
        // useful log line either.
        write!(f, "CoseKey({:?})", self.algorithm())
    }
}

fn sec1(x: &[u8; 32], y: &[u8; 32]) -> Vec<u8> {
    let mut point = Vec::with_capacity(65);
    point.push(0x04);
    point.extend_from_slice(x);
    point.extend_from_slice(y);
    point
}

fn rsa_key(n: &[u8], e: &[u8]) -> Result<rsa::RsaPublicKey> {
    use rsa::traits::PublicKeyParts as _;

    let key =
        rsa::RsaPublicKey::new(rsa::BigUint::from_bytes_be(n), rsa::BigUint::from_bytes_be(e))
            .map_err(|_| invalid("an RSA key that is not valid"))?;
    if key.size() * 8 < 2048 {
        return Err(invalid("an RSA modulus shorter than 2048 bits"));
    }
    Ok(key)
}

fn fixed(bytes: Option<&[u8]>, name: &str) -> Result<[u8; 32]> {
    bytes
        .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
        .ok_or_else(|| invalid(&format!("a coordinate `{name}` that is not 32 bytes")))
}

fn invalid(why: &str) -> Error {
    Error::bad_request(format!("invalid passkey public key: {why}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::webauthn::testing::SoftwareKey;

    #[test]
    fn every_algorithm_round_trips_and_verifies_its_own_signatures() {
        for key in [SoftwareKey::es256(), SoftwareKey::ed25519(), SoftwareKey::rs256()] {
            let public = CoseKey::from_bytes(&key.cose()).unwrap();
            assert_eq!(public.algorithm(), key.algorithm());
            assert_eq!(CoseKey::from_bytes(&public.to_bytes()).unwrap(), public);

            let signature = key.sign(b"message");
            public.verify(b"message", &signature).unwrap();
            assert!(public.verify(b"messagf", &signature).is_err(), "{:?}", key.algorithm());

            let mut bent = signature.clone();
            let last = bent.len() - 1;
            bent[last] ^= 1;
            assert!(public.verify(b"message", &bent).is_err(), "{:?}", key.algorithm());
        }
    }

    #[test]
    fn a_key_from_one_authenticator_does_not_verify_another() {
        let a = SoftwareKey::es256();
        let b = SoftwareKey::es256();
        let public = CoseKey::from_bytes(&a.cose()).unwrap();
        assert!(public.verify(b"m", &b.sign(b"m")).is_err());
    }

    #[test]
    fn malformed_keys_are_refused_at_parse() {
        let point_off_curve = Value::Map(vec![
            (Value::Unsigned(1), Value::Unsigned(2)),
            (Value::Unsigned(3), Value::Negative(-7)),
            (Value::Negative(-1), Value::Unsigned(1)),
            (Value::Negative(-2), Value::Bytes(vec![1; 32])),
            (Value::Negative(-3), Value::Bytes(vec![2; 32])),
        ]);
        assert!(CoseKey::from_value(&point_off_curve).is_err());

        let wrong_curve = Value::Map(vec![
            (Value::Unsigned(1), Value::Unsigned(2)),
            (Value::Unsigned(3), Value::Negative(-7)),
            (Value::Negative(-1), Value::Unsigned(2)),
            (Value::Negative(-2), Value::Bytes(vec![1; 32])),
            (Value::Negative(-3), Value::Bytes(vec![2; 32])),
        ]);
        assert!(CoseKey::from_value(&wrong_curve).is_err());

        let unsupported = Value::Map(vec![
            (Value::Unsigned(1), Value::Unsigned(2)),
            (Value::Unsigned(3), Value::Negative(-35)),
        ]);
        assert!(CoseKey::from_value(&unsupported).is_err(), "ES384 is not offered");

        let short_rsa = Value::Map(vec![
            (Value::Unsigned(1), Value::Unsigned(3)),
            (Value::Unsigned(3), Value::Negative(-257)),
            (Value::Negative(-1), Value::Bytes(vec![0xc5; 128])),
            (Value::Negative(-2), Value::Bytes(vec![1, 0, 1])),
        ]);
        assert!(CoseKey::from_value(&short_rsa).is_err(), "1024-bit RSA");

        let mismatched = Value::Map(vec![
            (Value::Unsigned(1), Value::Unsigned(1)),
            (Value::Unsigned(3), Value::Negative(-7)),
        ]);
        assert!(CoseKey::from_value(&mismatched).is_err());
    }
}
