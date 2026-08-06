//! JWTs and a JWKS document (feature `jwt`).
//!
//! ```ignore
//! let jwt = Jwt::new(ring).issued_by("https://id.example.com").for_audience("api");
//!
//! let token = jwt.sign(&claims)?;
//! let claims: Claims = jwt.verify(&token)?;
//!
//! // /.well-known/jwks.json
//! Response::json(&jwt.jwks())
//! ```
//!
//! # Why this is here and not left to an application
//!
//! `rainier-crypt` had exactly one asymmetric signer — Ed25519 — and OIDC
//! relying parties expect **RS256**. So the one asymmetric algorithm the
//! framework shipped could not do the one asymmetric job an identity provider
//! has, and every such application wrote key loading, rotation and a JWKS
//! serialiser itself.
//!
//! It is also not only for issuers. Anything that *verifies* a third-party
//! token — a Google ID token, an Apple one, a Kubernetes service-account token
//! — needs the same key ring keyed by `kid`, and the same rule about which
//! keys are still acceptable.
//!
//! # Rotation is an overlap, not a switch
//!
//! Signing keys rotate; tokens already issued keep arriving. So a ring
//! **signs with the newest key and verifies against every key it still
//! holds**, and a key is retired in two steps: stop signing with it, then —
//! once every token it signed has expired — remove it.
//!
//! Publishing the JWKS is what makes that work for somebody else's verifier:
//! it lists every key that can still verify, so a relying party that refreshes
//! its copy keeps accepting tokens across the change. Removing a key from the
//! JWKS the moment it stops signing is the classic mistake, and it invalidates
//! every unexpired token that key issued.
//!
//! # This is not an OAuth server
//!
//! Sign, verify, rotate, publish. Grants, consent, PKCE and the endpoints
//! around them are an application's business — they are where the product is,
//! and a framework that shipped them would be shipping opinions about a
//! product.

use std::sync::Arc;

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use base64::Engine as _;
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use rainier_support::{Error, Result};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Value};

/// The signature algorithms this supports.
///
/// Both are asymmetric and both are what relying parties actually ask for.
/// HMAC (`HS256`) is deliberately absent: a symmetric JWT cannot be verified
/// by anyone who cannot also *mint* one, which makes a published JWKS
/// meaningless and is the wrong shape for every use here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JwtAlgorithm {
    /// RSASSA-PKCS1-v1_5 with SHA-256. What OIDC relying parties expect.
    Rs256,
    /// ECDSA over P-256 with SHA-256. Smaller keys and signatures; not every
    /// relying party accepts it.
    Es256,
}

impl JwtAlgorithm {
    fn as_jsonwebtoken(self) -> Algorithm {
        match self {
            Self::Rs256 => Algorithm::RS256,
            Self::Es256 => Algorithm::ES256,
        }
    }

    /// The `alg` value a JWK carries.
    pub fn name(self) -> &'static str {
        match self {
            Self::Rs256 => "RS256",
            Self::Es256 => "ES256",
        }
    }

    /// Read an `alg` from a JWKS entry.
    ///
    /// An unknown one is refused rather than defaulted. A default here would
    /// decide, on a verifier's behalf, which algorithm somebody else's key
    /// uses — and being wrong about that is either a rejection storm or a
    /// forgery, depending on which way it is wrong.
    pub fn parse(name: &str) -> Result<Self> {
        match name {
            "RS256" => Ok(Self::Rs256),
            "ES256" => Ok(Self::Es256),
            other => Err(Error::internal(format!(
                "`{other}` is not an algorithm this service verifies; only RS256 and ES256"
            ))),
        }
    }
}

/// One signing key, with the public half a JWKS needs.
pub struct JwtKey {
    kid: String,
    algorithm: JwtAlgorithm,
    /// Absent for a key rebuilt from somebody else's JWKS.
    ///
    /// A relying party has the public half and nothing else, which is the
    /// whole point of publishing a JWKS. Modelling that as `None` rather than
    /// as a second type keeps one `JwtKeyRing` for both sides — the verifier
    /// in a service that also signs its own tokens is the same ring.
    encoding: Option<EncodingKey>,
    decoding: DecodingKey,
    /// The public JWK, minus the fields the ring fills in.
    jwk: Value,
}

impl JwtKey {
    /// An RS256 key from a PKCS#8 PEM private key.
    ///
    /// The `kid` is yours to choose and travels in every token's header, so it
    /// has to be stable across restarts — a random one per boot means every
    /// token issued before the last deploy stops verifying.
    pub fn rs256_from_pem(kid: impl Into<String>, pem: &str) -> Result<Self> {
        use rsa::pkcs1::DecodeRsaPrivateKey;
        use rsa::pkcs8::DecodePrivateKey;
        use rsa::traits::PublicKeyParts;
        use rsa::RsaPrivateKey;

        // Both spellings are in the wild: `BEGIN PRIVATE KEY` (PKCS#8) and
        // `BEGIN RSA PRIVATE KEY` (PKCS#1). Refusing one of them would be a
        // support question rather than a security property.
        let private = RsaPrivateKey::from_pkcs8_pem(pem)
            .or_else(|_| RsaPrivateKey::from_pkcs1_pem(pem))
            .map_err(|e| Error::internal(format!("could not read the RSA private key: {e}")))?;

        let public = private.to_public_key();
        let jwk = json!({
            "kty": "RSA",
            "n": B64URL.encode(public.n().to_bytes_be()),
            "e": B64URL.encode(public.e().to_bytes_be()),
        });

        Ok(Self {
            kid: kid.into(),
            algorithm: JwtAlgorithm::Rs256,
            encoding: Some(
                EncodingKey::from_rsa_pem(pem.as_bytes())
                    .map_err(|e| Error::internal(format!("could not load the RSA key: {e}")))?,
            ),
            decoding: DecodingKey::from_rsa_pem(public_pem(&public)?.as_bytes())
                .map_err(|e| Error::internal(format!("could not load the RSA key: {e}")))?,
            jwk,
        })
    }

    /// An ES256 key from a PKCS#8 PEM private key.
    pub fn es256_from_pem(kid: impl Into<String>, pem: &str) -> Result<Self> {
        use p256::elliptic_curve::sec1::ToEncodedPoint;
        use p256::pkcs8::DecodePrivateKey;
        use p256::SecretKey;

        let secret = SecretKey::from_pkcs8_pem(pem)
            .map_err(|e| Error::internal(format!("could not read the P-256 private key: {e}")))?;

        let point = secret.public_key().to_encoded_point(false);
        let jwk = json!({
            "kty": "EC",
            "crv": "P-256",
            "x": B64URL.encode(point.x().ok_or_else(|| Error::internal("no x coordinate"))?),
            "y": B64URL.encode(point.y().ok_or_else(|| Error::internal("no y coordinate"))?),
        });

        let public_pem = {
            use p256::pkcs8::EncodePublicKey;
            secret
                .public_key()
                .to_public_key_pem(Default::default())
                .map_err(|e| Error::internal(format!("could not encode the public key: {e}")))?
        };

        Ok(Self {
            kid: kid.into(),
            algorithm: JwtAlgorithm::Es256,
            encoding: Some(
                EncodingKey::from_ec_pem(pem.as_bytes())
                    .map_err(|e| Error::internal(format!("could not load the P-256 key: {e}")))?,
            ),
            decoding: DecodingKey::from_ec_pem(public_pem.as_bytes())
                .map_err(|e| Error::internal(format!("could not load the P-256 key: {e}")))?,
            jwk,
        })
    }

    /// Generate an RS256 key. **For tests and for a first boot.**
    ///
    /// A key generated at boot is a key that changes on restart, so every
    /// token issued before it stops verifying — fine for a test, and a source
    /// of intermittent logouts anywhere else.
    pub fn generate_rs256(kid: impl Into<String>, bits: usize) -> Result<Self> {
        use rsa::pkcs8::{EncodePrivateKey, LineEnding};
        use rsa::RsaPrivateKey;

        let private = RsaPrivateKey::new(&mut rand::thread_rng(), bits)
            .map_err(|e| Error::internal(format!("could not generate an RSA key: {e}")))?;
        let pem = private
            .to_pkcs8_pem(LineEnding::LF)
            .map_err(|e| Error::internal(format!("could not encode the key: {e}")))?;

        Self::rs256_from_pem(kid, &pem)
    }

    /// Generate an ES256 key. **For tests and for a first boot** — see
    /// [`generate_rs256`](Self::generate_rs256).
    pub fn generate_es256(kid: impl Into<String>) -> Result<Self> {
        use p256::pkcs8::{EncodePrivateKey, LineEnding};

        let secret = p256::SecretKey::random(&mut rand::thread_rng());
        let pem = secret
            .to_pkcs8_pem(LineEnding::LF)
            .map_err(|e| Error::internal(format!("could not encode the key: {e}")))?;

        Self::es256_from_pem(kid, &pem)
    }

    /// The key id.
    pub fn kid(&self) -> &str {
        &self.kid
    }

    /// The algorithm.
    pub fn algorithm(&self) -> JwtAlgorithm {
        self.algorithm
    }

    /// A verify-only key, rebuilt from a JWKS entry.
    ///
    /// The inverse of [`to_jwk`](Self::to_jwk), and what a relying party needs:
    /// it has the published document and no private half. Without this, every
    /// service verifying somebody else's tokens hand-decodes `n` and `e` into
    /// a public key, which is a piece of bignum-and-base64 handling that has
    /// no business being written more than once.
    ///
    /// **The algorithm comes from the JWK's `alg`, never from a token's
    /// header.** That is the same rule [`Jwt::verify`] follows, for the same
    /// reason: a header saying `none`, or saying `HS256` over a public key
    /// everybody can read, is the classic JWT forgery.
    ///
    /// Requires `kid`. A JWKS entry without one cannot be selected by a
    /// token's header, so accepting it would put a key in the ring that
    /// nothing can ever match.
    pub fn from_jwk(jwk: &Value) -> Result<Self> {
        let field = |name: &str| -> Result<String> {
            jwk.get(name)
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| Error::internal(format!("this JWKS entry has no `{name}`")))
        };

        let decode = |name: &str| -> Result<Vec<u8>> {
            B64URL
                .decode(field(name)?)
                .map_err(|e| Error::internal(format!("`{name}` is not base64url: {e}")))
        };

        let kid = field("kid")?;
        let algorithm = JwtAlgorithm::parse(&field("alg")?)?;

        let decoding = match field("kty")?.as_str() {
            "RSA" => {
                use rsa::{BigUint, RsaPublicKey};

                // Big-endian, unsigned, as RFC 7518 §6.3.1 specifies. Reading
                // these little-endian produces a key that is structurally
                // valid and verifies nothing, which is the failure this is
                // most likely to be broken into.
                let public = RsaPublicKey::new(
                    BigUint::from_bytes_be(&decode("n")?),
                    BigUint::from_bytes_be(&decode("e")?),
                )
                .map_err(|e| Error::internal(format!("this JWKS entry is not an RSA key: {e}")))?;

                DecodingKey::from_rsa_pem(public_pem(&public)?.as_bytes())
                    .map_err(|e| Error::internal(format!("could not load the RSA key: {e}")))?
            }
            "EC" => {
                use p256::elliptic_curve::sec1::FromEncodedPoint;
                use p256::pkcs8::EncodePublicKey;
                use p256::{AffinePoint, EncodedPoint};

                let curve = field("crv")?;
                if curve != "P-256" {
                    return Err(Error::internal(format!(
                        "`{curve}` is not a curve this service verifies; only P-256"
                    )));
                }

                let point = EncodedPoint::from_affine_coordinates(
                    decode("x")?.as_slice().into(),
                    decode("y")?.as_slice().into(),
                    false,
                );

                let affine = Option::<AffinePoint>::from(AffinePoint::from_encoded_point(&point))
                    .ok_or_else(|| {
                    Error::internal("this JWKS entry is not a point on P-256")
                })?;

                let public = p256::PublicKey::from_affine(affine).map_err(|e| {
                    Error::internal(format!("this JWKS entry is not a P-256 key: {e}"))
                })?;

                let pem = public
                    .to_public_key_pem(Default::default())
                    .map_err(|e| Error::internal(format!("could not encode the key: {e}")))?;

                DecodingKey::from_ec_pem(pem.as_bytes())
                    .map_err(|e| Error::internal(format!("could not load the P-256 key: {e}")))?
            }
            other => {
                return Err(Error::internal(format!(
                    "`{other}` is not a key type this service verifies; only RSA and EC"
                )))
            }
        };

        Ok(Self { kid, algorithm, encoding: None, decoding, jwk: jwk.clone() })
    }

    /// Whether this key can sign, or only verify.
    pub fn can_sign(&self) -> bool {
        self.encoding.is_some()
    }

    /// The public JWK, as a JWKS entry.
    pub fn to_jwk(&self) -> Value {
        let mut jwk = self.jwk.clone();
        jwk["kid"] = json!(self.kid);
        jwk["alg"] = json!(self.algorithm.name());
        jwk["use"] = json!("sig");
        jwk
    }
}

impl std::fmt::Debug for JwtKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JwtKey")
            .field("kid", &self.kid)
            .field("algorithm", &self.algorithm)
            .finish()
    }
}

/// The keys this service signs and verifies with.
///
/// Signs with the **first**; verifies against **any**. See the module docs for
/// why retiring one is two steps.
#[derive(Debug, Default)]
pub struct JwtKeyRing {
    keys: Vec<Arc<JwtKey>>,
}

impl JwtKeyRing {
    /// A ring signing with `key`.
    pub fn new(key: JwtKey) -> Self {
        Self { keys: vec![Arc::new(key)] }
    }

    /// Also verify tokens signed by `key`, without signing new ones with it.
    ///
    /// The middle step of a rotation. Keep it here until every token it signed
    /// has expired.
    #[must_use = "this returns a ring with the key added"]
    pub fn with_previous(mut self, key: JwtKey) -> Self {
        self.keys.push(Arc::new(key));
        self
    }

    /// The key new tokens are signed with.
    pub fn current(&self) -> Option<&Arc<JwtKey>> {
        self.keys.first()
    }

    /// The key with this id, if the ring holds it.
    pub fn find(&self, kid: &str) -> Option<&Arc<JwtKey>> {
        self.keys.iter().find(|key| key.kid == kid)
    }

    /// Every key id, current first.
    pub fn ids(&self) -> Vec<&str> {
        self.keys.iter().map(|key| key.kid.as_str()).collect()
    }

    /// A verify-only ring, rebuilt from a published JWKS document.
    ///
    /// What a relying party holds: every key the issuer says can still verify,
    /// and no private half. Order is the document's, which for a well-behaved
    /// issuer puts the current signing key first — but nothing here depends on
    /// that, since [`Jwt::verify`] selects by `kid`.
    ///
    /// Entries this build cannot verify with — an unknown `kty`, an `alg` that
    /// is not RS256 or ES256, a missing `kid` — are **skipped rather than
    /// fatal**. An issuer that adds a key type before its relying parties
    /// understand it should not take them all down; the tokens naming that key
    /// are rejected individually, which is the correct blast radius.
    ///
    /// A document that yields no usable key at all *is* an error, because
    /// there is nothing to distinguish it from a successful fetch of an empty
    /// or wrong document — and a silently empty ring rejects every token with
    /// "names a key this service does not hold".
    pub fn from_jwks(document: &Value) -> Result<Self> {
        let entries = document
            .get("keys")
            .and_then(Value::as_array)
            .ok_or_else(|| Error::internal("this JWKS document has no `keys` array"))?;

        let keys: Vec<Arc<JwtKey>> =
            entries.iter().filter_map(|jwk| JwtKey::from_jwk(jwk).ok()).map(Arc::new).collect();

        if keys.is_empty() {
            return Err(Error::internal(
                "this JWKS document holds no key this service can verify with",
            ));
        }

        Ok(Self { keys })
    }

    /// The JWKS document.
    ///
    /// Every key that can still verify, which is the whole ring — not just the
    /// signing one. Publishing only the current key invalidates every
    /// unexpired token the previous one issued, which is the classic way to
    /// break a rotation.
    pub fn jwks(&self) -> Value {
        json!({ "keys": self.keys.iter().map(|key| key.to_jwk()).collect::<Vec<_>>() })
    }
}

/// Signs and verifies tokens.
pub struct Jwt {
    ring: JwtKeyRing,
    issuer: Option<String>,
    audience: Vec<String>,
    leeway: u64,
    require_nbf: bool,
}

impl Jwt {
    /// Sign and verify with `ring`.
    pub fn new(ring: JwtKeyRing) -> Self {
        Self { ring, issuer: None, audience: Vec::new(), leeway: 60, require_nbf: false }
    }

    /// Set the `iss` this service claims, and require it when verifying.
    #[must_use = "this returns a configured signer rather than configuring in place"]
    pub fn issued_by(mut self, issuer: impl Into<String>) -> Self {
        self.issuer = Some(issuer.into());
        self
    }

    /// Require this `aud` when verifying.
    ///
    /// Worth setting. A token minted for one audience being accepted by
    /// another is how a token issued for an analytics API turns into one that
    /// works against the admin API.
    #[must_use = "this returns a configured signer rather than configuring in place"]
    pub fn for_audience(mut self, audience: impl Into<String>) -> Self {
        self.audience.push(audience.into());
        self
    }

    /// How much clock skew to tolerate on `exp` and `nbf`, in seconds.
    ///
    /// Sixty by default. Zero means a verifier whose clock is a second behind
    /// the issuer's rejects freshly minted tokens, which is a bug that only
    /// appears on somebody else's machine.
    #[must_use = "this returns a configured signer rather than configuring in place"]
    pub fn leeway(mut self, seconds: u64) -> Self {
        self.leeway = seconds;
        self
    }

    /// Require `nbf` to be present, and validate it.
    ///
    /// Off by default: plenty of third-party issuers mint no `nbf`, and
    /// requiring it would refuse every token they sign. An issuer verifying
    /// its **own** tokens — which always carry it — turns this on, so a token
    /// doctored to start in the future is refused rather than read.
    #[must_use = "this returns a configured signer rather than configuring in place"]
    pub fn require_not_before(mut self) -> Self {
        self.require_nbf = true;
        self
    }

    /// The key ring.
    pub fn ring(&self) -> &JwtKeyRing {
        &self.ring
    }

    /// The JWKS document — see [`JwtKeyRing::jwks`].
    pub fn jwks(&self) -> Value {
        self.ring.jwks()
    }

    /// Sign `claims` with the current key.
    ///
    /// The header carries the `kid`, which is what lets a verifier pick the
    /// right key out of a JWKS instead of trying all of them.
    pub fn sign<C: Serialize>(&self, claims: &C) -> Result<String> {
        let key = self.ring.current().ok_or_else(|| {
            Error::internal("the JWT key ring is empty, so nothing can be signed")
        })?;

        // A ring built from somebody else's JWKS can verify and nothing else.
        // Saying so beats a signature failure deeper in, which reads like a
        // key problem rather than like asking a verifier to sign.
        let encoding = key.encoding.as_ref().ok_or_else(|| {
            Error::internal(
                "this key was rebuilt from a JWKS and holds no private half, so it can verify but not sign",
            )
        })?;

        let mut header = Header::new(key.algorithm.as_jsonwebtoken());
        header.kid = Some(key.kid.clone());

        jsonwebtoken::encode(&header, claims, encoding)
            .map_err(|e| Error::internal(format!("could not sign the token: {e}")))
    }

    /// Verify `token` and return its claims.
    ///
    /// Checks the signature, `exp`, `nbf`, and — when configured — `iss` and
    /// `aud`.
    ///
    /// # Errors
    ///
    /// A `401` for anything wrong with the token, with the reason in the
    /// message: an unknown `kid`, a bad signature, an expired token, the wrong
    /// issuer.
    pub fn verify<C: DeserializeOwned>(&self, token: &str) -> Result<C> {
        let header = jsonwebtoken::decode_header(token)
            .map_err(|_| Error::unauthenticated("This token is not readable."))?;

        let kid = header.kid.ok_or_else(|| Error::unauthenticated("This token names no key."))?;

        // By id, rather than trying every key. A token that names a key this
        // service does not hold is not a token worth spending CPU on, and
        // trying them all would make an unknown-key token cost the same as a
        // real one.
        let key = self.ring.find(&kid).ok_or_else(|| {
            Error::unauthenticated("This token names a key this service does not hold.")
        })?;

        // The algorithm from the **key**, never from the token's header. A
        // verifier that trusts the header's `alg` is the classic JWT
        // vulnerability: a forged header saying `none`, or saying `HS256` over
        // a public key everybody has.
        let mut validation = Validation::new(key.algorithm.as_jsonwebtoken());
        validation.leeway = self.leeway;

        if self.require_nbf {
            validation.validate_nbf = true;
            validation.required_spec_claims.insert("nbf".to_string());
        }

        if let Some(issuer) = &self.issuer {
            validation.set_issuer(&[issuer]);
        }
        if !self.audience.is_empty() {
            validation.set_audience(&self.audience);
        } else {
            // Otherwise `jsonwebtoken` refuses a token carrying an `aud` this
            // service did not ask about, which is not what "no audience
            // configured" should mean.
            validation.validate_aud = false;
        }

        jsonwebtoken::decode::<C>(token, &key.decoding, &validation)
            .map(|data| data.claims)
            .map_err(|e| Error::unauthenticated(format!("This token is not valid: {e}")))
    }

    /// The `kid` a token names, without verifying it.
    ///
    /// For deciding whether a JWKS needs refreshing before rejecting the
    /// token — which is the difference between a rotation that costs nothing
    /// and one that rejects traffic for a cache lifetime.
    pub fn kid_of(token: &str) -> Option<String> {
        jsonwebtoken::decode_header(token).ok()?.kid
    }
}

impl std::fmt::Debug for Jwt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Jwt")
            .field("keys", &self.ring.ids())
            .field("issuer", &self.issuer)
            .field("audience", &self.audience)
            .finish()
    }
}

/// An RSA public key as PEM, which is what `DecodingKey` wants.
fn public_pem(public: &rsa::RsaPublicKey) -> Result<String> {
    use rsa::pkcs8::{EncodePublicKey, LineEnding};

    public
        .to_public_key_pem(LineEnding::LF)
        .map_err(|e| Error::internal(format!("could not encode the public key: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    struct Claims {
        sub: String,
        exp: i64,
        #[serde(skip_serializing_if = "Option::is_none")]
        nbf: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        iss: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        aud: Option<String>,
    }

    fn in_an_hour() -> i64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
            + 3600
    }

    fn claims() -> Claims {
        Claims { sub: "user-42".into(), exp: in_an_hour(), nbf: None, iss: None, aud: None }
    }

    /// 2048 bits, generated once per test binary: RSA key generation is slow
    /// enough that doing it per test doubles the suite's runtime.
    fn rsa_key() -> &'static JwtKey {
        use std::sync::OnceLock;
        static KEY: OnceLock<JwtKey> = OnceLock::new();
        KEY.get_or_init(|| JwtKey::generate_rs256("test-rsa", 2048).expect("generate"))
    }

    fn rsa_jwt() -> Jwt {
        Jwt::new(JwtKeyRing::new(JwtKey::rs256_from_pem("test-rsa", &rsa_pem()).expect("from pem")))
    }

    /// The same key material as `rsa_key`, re-encoded — so two `Jwt`s in one
    /// test share a key without generating two.
    fn rsa_pem() -> String {
        use rsa::pkcs8::{EncodePrivateKey, LineEnding};
        use std::sync::OnceLock;
        static PEM: OnceLock<String> = OnceLock::new();

        PEM.get_or_init(|| {
            let private = rsa::RsaPrivateKey::new(&mut rand::thread_rng(), 2048).expect("generate");
            private.to_pkcs8_pem(LineEnding::LF).expect("encode").to_string()
        })
        .clone()
    }

    #[test]
    fn an_rs256_token_round_trips() {
        let jwt = rsa_jwt();
        let token = jwt.sign(&claims()).unwrap();

        assert_eq!(jwt.verify::<Claims>(&token).unwrap().sub, "user-42");
    }

    #[test]
    fn an_es256_token_round_trips() {
        let jwt = Jwt::new(JwtKeyRing::new(JwtKey::generate_es256("test-ec").unwrap()));
        let token = jwt.sign(&claims()).unwrap();

        assert_eq!(jwt.verify::<Claims>(&token).unwrap().sub, "user-42");
    }

    #[test]
    fn the_header_names_the_key() {
        // Which is what lets a verifier pick one out of a JWKS rather than
        // trying all of them.
        let token = rsa_jwt().sign(&claims()).unwrap();

        assert_eq!(Jwt::kid_of(&token).as_deref(), Some("test-rsa"));
    }

    #[test]
    fn a_tampered_token_does_not_verify() {
        let jwt = rsa_jwt();
        let token = jwt.sign(&claims()).unwrap();

        // Flip a character in the payload segment.
        let mut parts: Vec<&str> = token.split('.').collect();
        let payload = parts[1].to_string();
        let tampered_payload = format!("{}A", &payload[..payload.len() - 1]);
        parts[1] = &tampered_payload;

        assert!(jwt.verify::<Claims>(&parts.join(".")).is_err());
    }

    #[test]
    fn an_expired_token_does_not_verify() {
        let jwt = rsa_jwt();
        let expired = Claims { exp: 1, ..claims() };

        let token = jwt.sign(&expired).unwrap();

        assert!(jwt.verify::<Claims>(&token).is_err());
    }

    #[test]
    fn a_token_from_another_key_does_not_verify() {
        let token = rsa_jwt().sign(&claims()).unwrap();

        let stranger = Jwt::new(JwtKeyRing::new(
            JwtKey::rs256_from_pem("test-rsa", &other_pem()).expect("from pem"),
        ));

        assert!(stranger.verify::<Claims>(&token).is_err());
    }

    fn other_pem() -> String {
        use rsa::pkcs8::{EncodePrivateKey, LineEnding};
        use std::sync::OnceLock;
        static PEM: OnceLock<String> = OnceLock::new();

        PEM.get_or_init(|| {
            let private = rsa::RsaPrivateKey::new(&mut rand::thread_rng(), 2048).expect("generate");
            private.to_pkcs8_pem(LineEnding::LF).expect("encode").to_string()
        })
        .clone()
    }

    #[test]
    fn an_unknown_kid_is_refused_without_trying_every_key() {
        let jwt = rsa_jwt();
        let token = jwt.sign(&claims()).unwrap();

        // Re-sign under a ring that does not hold this kid.
        let other = Jwt::new(JwtKeyRing::new(JwtKey::generate_es256("somebody-else").unwrap()));

        let error = other.verify::<Claims>(&token).unwrap_err();
        assert!(error.message().contains("does not hold"), "{}", error.message());
    }

    #[test]
    fn a_rotation_keeps_verifying_the_previous_keys_tokens() {
        // The whole point of an overlap. A token signed before the rotation
        // has to keep working until it expires.
        let old = JwtKey::rs256_from_pem("old", &rsa_pem()).unwrap();
        let signed_before = Jwt::new(JwtKeyRing::new(old)).sign(&claims()).unwrap();

        let rotated = Jwt::new(
            JwtKeyRing::new(JwtKey::generate_es256("new").unwrap())
                .with_previous(JwtKey::rs256_from_pem("old", &rsa_pem()).unwrap()),
        );

        assert!(rotated.verify::<Claims>(&signed_before).is_ok());

        // And new tokens use the new key.
        let signed_after = rotated.sign(&claims()).unwrap();
        assert_eq!(Jwt::kid_of(&signed_after).as_deref(), Some("new"));
    }

    #[test]
    fn the_jwks_lists_every_key_that_can_still_verify() {
        // Publishing only the signing key invalidates every unexpired token
        // the previous one issued — the classic way to break a rotation.
        let ring = JwtKeyRing::new(JwtKey::generate_es256("new").unwrap())
            .with_previous(JwtKey::rs256_from_pem("old", &rsa_pem()).unwrap());

        let jwks = ring.jwks();
        let keys = jwks["keys"].as_array().unwrap();

        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0]["kid"], "new");
        assert_eq!(keys[1]["kid"], "old");
    }

    #[test]
    fn a_jwks_entry_is_the_shape_a_relying_party_reads() {
        let jwks = JwtKeyRing::new(JwtKey::rs256_from_pem("k1", &rsa_pem()).unwrap()).jwks();
        let key = &jwks["keys"][0];

        assert_eq!(key["kty"], "RSA");
        assert_eq!(key["alg"], "RS256");
        assert_eq!(key["use"], "sig");
        assert_eq!(key["kid"], "k1");
        // The modulus and exponent, base64url with no padding.
        assert!(key["n"].as_str().unwrap().len() > 300);
        assert_eq!(key["e"].as_str().unwrap(), "AQAB", "65537, as every RSA key uses");
        assert!(!key["n"].as_str().unwrap().contains('='), "base64url is unpadded");
    }

    #[test]
    fn an_ec_jwks_entry_carries_its_curve_and_point() {
        let jwks = JwtKeyRing::new(JwtKey::generate_es256("k1").unwrap()).jwks();
        let key = &jwks["keys"][0];

        assert_eq!(key["kty"], "EC");
        assert_eq!(key["crv"], "P-256");
        assert_eq!(key["alg"], "ES256");
        // P-256 coordinates are 32 bytes, so 43 base64url characters.
        assert_eq!(key["x"].as_str().unwrap().len(), 43);
        assert_eq!(key["y"].as_str().unwrap().len(), 43);
    }

    #[test]
    fn the_issuer_is_checked_when_it_is_configured() {
        let signer = rsa_jwt().issued_by("https://id.example.com");
        let token = signer
            .sign(&Claims { iss: Some("https://id.example.com".into()), ..claims() })
            .unwrap();

        assert!(signer.verify::<Claims>(&token).is_ok());

        // A token from somewhere else, signed with the same key, is refused.
        let impostor = signer
            .sign(&Claims { iss: Some("https://evil.example.com".into()), ..claims() })
            .unwrap();
        assert!(signer.verify::<Claims>(&impostor).is_err());
    }

    #[test]
    fn the_audience_is_checked_when_it_is_configured() {
        // A token minted for one audience being accepted by another is how a
        // token for an analytics API turns into one for the admin API.
        let signer = rsa_jwt().for_audience("api");

        let right = signer.sign(&Claims { aud: Some("api".into()), ..claims() }).unwrap();
        assert!(signer.verify::<Claims>(&right).is_ok());

        let wrong = signer.sign(&Claims { aud: Some("analytics".into()), ..claims() }).unwrap();
        assert!(signer.verify::<Claims>(&wrong).is_err());
    }

    #[test]
    fn a_token_with_an_audience_verifies_when_none_is_configured() {
        // "No audience configured" must mean "do not check", not "refuse
        // anything that has one".
        let jwt = rsa_jwt();
        let token = jwt.sign(&Claims { aud: Some("api".into()), ..claims() }).unwrap();

        assert!(jwt.verify::<Claims>(&token).is_ok());
    }

    #[test]
    fn nbf_is_enforced_only_when_asked_for() {
        // Third-party issuers often mint no `nbf`, so the default accepts its
        // absence; an issuer verifying its own tokens opts in and gets both
        // halves — the claim must exist, and it must have arrived.
        let lax = rsa_jwt();
        let strict = rsa_jwt().require_not_before();

        let without = lax.sign(&claims()).unwrap();
        assert!(lax.verify::<Claims>(&without).is_ok());
        assert!(strict.verify::<Claims>(&without).is_err(), "nbf is required");

        let future = lax.sign(&Claims { nbf: Some(in_an_hour()), ..claims() }).unwrap();
        assert!(strict.verify::<Claims>(&future).is_err(), "not valid yet");

        let valid = lax.sign(&Claims { nbf: Some(0), ..claims() }).unwrap();
        assert!(strict.verify::<Claims>(&valid).is_ok());
    }

    #[test]
    fn nonsense_is_refused_without_panicking() {
        let jwt = rsa_jwt();

        for token in ["", "not.a.token", "a.b", &"x".repeat(5000)] {
            assert!(jwt.verify::<Claims>(token).is_err(), "{token:?}");
        }
    }

    #[test]
    fn an_empty_ring_cannot_sign() {
        let jwt = Jwt::new(JwtKeyRing::default());

        let error = jwt.sign(&claims()).unwrap_err();
        assert!(error.message().contains("empty"), "{}", error.message());
    }

    #[test]
    fn a_generated_key_can_be_read_back_from_its_own_pem() {
        // The path an application takes on a first boot: generate, save, load.
        let key = rsa_key();

        assert_eq!(key.kid(), "test-rsa");
        assert_eq!(key.algorithm(), JwtAlgorithm::Rs256);
        assert_eq!(key.to_jwk()["kty"], "RSA");
    }
}
