//! Access tokens — RS256 JWTs, [`AccessTokenSigner`] and
//! [`AccessTokenVerifier`].
//!
//! # Why not `rainier_crypt::Jwt`
//!
//! That one is right for tokens this framework issues to itself: it requires
//! a `kid`, and reads timestamps as integers. An authorization server that
//! takes over from another inherits tokens already in the field, and the
//! widely deployed PHP issuer writes **no `kid`** and timestamps **with
//! microseconds** (`"exp": 1729550000.123456`). Both are legal JWT; refusing
//! them would log out every client at the cutover. So this module signs the
//! narrow token shape an OAuth access token has, and verifies the broader one.
//!
//! The algorithm is fixed to RS256 by the key, never read from the token's
//! header — a header saying `none` or `HS256` is refused.
//!
//! # The claims
//!
//! | Claim | Value |
//! |---|---|
//! | `aud` | the client id |
//! | `jti` | the token's id — the row revocation is checked against |
//! | `iat`, `nbf`, `exp` | issue, not-before, expiry |
//! | `sub` | the user id, or the client id for a client-credentials token |
//! | `scopes` | the granted scopes, as an array |
//!
//! plus `iss` when an issuer is configured, and any static claims added with
//! [`with_claim`](AccessTokenSigner::with_claim).

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine as _;
use chrono::{DateTime, TimeZone, Utc};
use rainier_support::{Error, Result};
use rsa::pkcs1::{DecodeRsaPrivateKey, DecodeRsaPublicKey};
use rsa::pkcs1v15::{Signature, SigningKey, VerifyingKey};
use rsa::pkcs8::{DecodePrivateKey, DecodePublicKey};
use rsa::sha2::Sha256;
use rsa::signature::{SignatureEncoding, Signer, Verifier};
use rsa::{RsaPrivateKey, RsaPublicKey};
use serde_json::{json, Map, Value};

/// What goes into a new access token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessTokenClaims {
    /// The token id (`jti`).
    pub id: String,
    /// The client (`aud`).
    pub client_id: String,
    /// The user, or `None` for a client token.
    pub user_id: Option<String>,
    /// The scopes.
    pub scopes: Vec<String>,
    /// When it was issued.
    pub issued_at: DateTime<Utc>,
    /// When it expires.
    pub expires_at: DateTime<Utc>,
}

/// Signs access tokens with an RSA private key.
pub struct AccessTokenSigner {
    key: SigningKey<Sha256>,
    kid: Option<String>,
    issuer: Option<String>,
    extra: Map<String, Value>,
}

impl AccessTokenSigner {
    /// A signer from a PEM private key — PKCS#8 (`BEGIN PRIVATE KEY`) or
    /// PKCS#1 (`BEGIN RSA PRIVATE KEY`). Encrypted keys are not read; decrypt
    /// it into the secret store instead of shipping the passphrase beside it.
    pub fn from_pem(pem: &str) -> Result<Self> {
        let pem = normalise_pem(pem);
        let key = RsaPrivateKey::from_pkcs8_pem(&pem)
            .or_else(|_| RsaPrivateKey::from_pkcs1_pem(&pem))
            .map_err(|_| {
                Error::internal("the access token private key is not a readable RSA PEM")
            })?;
        Ok(Self::from_key(key))
    }

    /// A signer over a freshly generated key of `bits` bits — for tests,
    /// examples and local development. A deployment loads its key with
    /// [`from_pem`](Self::from_pem), or every restart invalidates every token.
    pub fn generate(bits: usize) -> Result<Self> {
        let key = RsaPrivateKey::new(&mut rsa::rand_core::OsRng, bits)
            .map_err(|e| Error::internal(format!("could not generate an RSA key: {e}")))?;
        Ok(Self::from_key(key))
    }

    /// A signer over an already-parsed key.
    pub fn from_key(key: RsaPrivateKey) -> Self {
        Self { key: SigningKey::new(key), kid: None, issuer: None, extra: Map::new() }
    }

    /// Put `kid` in every token's header — worth it once a JWKS is published.
    /// Off by default, because verifiers of tokens this server replaced may not
    /// expect one.
    pub fn with_kid(mut self, kid: impl Into<String>) -> Self {
        self.kid = Some(kid.into());
        self
    }

    /// Add `iss` to every token.
    pub fn issued_by(mut self, issuer: impl Into<String>) -> Self {
        self.issuer = Some(issuer.into());
        self
    }

    /// Add a static claim to every token. The registered claims above cannot
    /// be overridden this way.
    pub fn with_claim(mut self, name: impl Into<String>, value: Value) -> Self {
        self.extra.insert(name.into(), value);
        self
    }

    /// Sign a token.
    pub fn sign(&self, claims: &AccessTokenClaims) -> Result<String> {
        let mut header = Map::new();
        header.insert("typ".into(), "JWT".into());
        header.insert("alg".into(), "RS256".into());
        if let Some(kid) = &self.kid {
            header.insert("kid".into(), kid.clone().into());
        }

        let mut body = self.extra.clone();
        body.insert("aud".into(), claims.client_id.clone().into());
        body.insert("jti".into(), claims.id.clone().into());
        body.insert("iat".into(), claims.issued_at.timestamp().into());
        body.insert("nbf".into(), claims.issued_at.timestamp().into());
        body.insert("exp".into(), claims.expires_at.timestamp().into());
        body.insert(
            "sub".into(),
            claims.user_id.clone().unwrap_or_else(|| claims.client_id.clone()).into(),
        );
        body.insert("scopes".into(), json!(claims.scopes));
        if let Some(issuer) = &self.issuer {
            body.insert("iss".into(), issuer.clone().into());
        }

        let signing_input = format!(
            "{}.{}",
            B64.encode(serde_json::to_vec(&Value::Object(header))?),
            B64.encode(serde_json::to_vec(&Value::Object(body))?)
        );
        let signature = self.key.sign(signing_input.as_bytes()).to_bytes();
        Ok(format!("{signing_input}.{}", B64.encode(signature)))
    }

    /// The matching verifier.
    pub fn verifier(&self) -> AccessTokenVerifier {
        let public = RsaPublicKey::from(self.key.as_ref());
        let mut verifier = AccessTokenVerifier::from_key(public, self.kid.clone());
        verifier.issuer = self.issuer.clone();
        verifier
    }
}

impl std::fmt::Debug for AccessTokenSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccessTokenSigner")
            .field("kid", &self.kid)
            .field("issuer", &self.issuer)
            .finish_non_exhaustive()
    }
}

/// A token whose signature and lifetime check out. Whether it has been
/// **revoked** is the store's to say — see [`OAuthGuard`](crate::OAuthGuard).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedToken {
    /// `jti`.
    pub id: String,
    /// `aud` — the first one, if the token lists several.
    pub client_id: String,
    /// `sub`.
    pub subject: String,
    /// `scopes`.
    pub scopes: Vec<String>,
    /// `exp`.
    pub expires_at: DateTime<Utc>,
}

/// Verifies access tokens against one or more RSA public keys.
#[derive(Clone)]
pub struct AccessTokenVerifier {
    keys: Vec<(Option<String>, VerifyingKey<Sha256>)>,
    issuer: Option<String>,
    leeway: i64,
}

impl AccessTokenVerifier {
    /// A verifier from a PEM public key — SPKI (`BEGIN PUBLIC KEY`) or PKCS#1
    /// (`BEGIN RSA PUBLIC KEY`).
    pub fn from_pem(pem: &str) -> Result<Self> {
        Ok(Self::from_key(parse_public(pem)?, None))
    }

    /// A verifier over an already-parsed key, optionally under a `kid`.
    pub fn from_key(key: RsaPublicKey, kid: Option<String>) -> Self {
        Self { keys: vec![(kid, VerifyingKey::new(key))], issuer: None, leeway: 0 }
    }

    /// Also accept tokens signed by `pem` — the previous key, during a
    /// rotation. A token naming a `kid` is checked against that key alone;
    /// one naming none is tried against each.
    pub fn also_accepting(mut self, pem: &str, kid: Option<String>) -> Result<Self> {
        self.keys.push((kid, VerifyingKey::new(parse_public(pem)?)));
        Ok(self)
    }

    /// Require this `iss`.
    pub fn issued_by(mut self, issuer: impl Into<String>) -> Self {
        self.issuer = Some(issuer.into());
        self
    }

    /// Seconds of clock skew to forgive. None by default.
    pub fn leeway(mut self, seconds: i64) -> Self {
        self.leeway = seconds;
        self
    }

    /// Verify `token` as of `now`.
    pub fn verify_at(&self, token: &str, now: DateTime<Utc>) -> Result<VerifiedToken> {
        let refuse = |why: &str| {
            Error::unauthenticated(format!("Access token could not be verified: {why}"))
        };

        let mut parts = token.split('.');
        let (Some(header), Some(body), Some(signature), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(refuse("not a JWT"));
        };

        let header: Value = decode_json(header).ok_or_else(|| refuse("unreadable header"))?;
        if header.get("alg").and_then(Value::as_str) != Some("RS256") {
            return Err(refuse("not RS256"));
        }
        let signature = B64
            .decode(signature)
            .ok()
            .and_then(|bytes| Signature::try_from(bytes.as_slice()).ok())
            .ok_or_else(|| refuse("unreadable signature"))?;
        let signing_input = &token[..header_len(token)];

        let kid = header.get("kid").and_then(Value::as_str);
        let candidates: Vec<&VerifyingKey<Sha256>> = match kid {
            Some(kid) => self
                .keys
                .iter()
                .filter(|(k, _)| k.as_deref() == Some(kid) || k.is_none())
                .map(|(_, key)| key)
                .collect(),
            None => self.keys.iter().map(|(_, key)| key).collect(),
        };
        if !candidates.iter().any(|key| key.verify(signing_input.as_bytes(), &signature).is_ok()) {
            return Err(refuse("bad signature"));
        }

        let claims: Value = decode_json(body).ok_or_else(|| refuse("unreadable claims"))?;
        let now_s = now.timestamp() as f64 + f64::from(now.timestamp_subsec_micros()) / 1e6;
        let leeway = self.leeway as f64;

        let exp = number(&claims, "exp").ok_or_else(|| refuse("no expiry"))?;
        if now_s >= exp + leeway {
            return Err(refuse("expired"));
        }
        if number(&claims, "nbf").is_some_and(|nbf| nbf > now_s + leeway) {
            return Err(refuse("not yet valid"));
        }
        if number(&claims, "iat").is_some_and(|iat| iat > now_s + leeway) {
            return Err(refuse("issued in the future"));
        }
        if let Some(issuer) = &self.issuer {
            if claims.get("iss").and_then(Value::as_str) != Some(issuer.as_str()) {
                return Err(refuse("wrong issuer"));
            }
        }

        let client_id = match claims.get("aud") {
            Some(Value::String(aud)) => aud.clone(),
            Some(Value::Array(list)) => {
                list.first().and_then(Value::as_str).unwrap_or_default().to_string()
            }
            _ => String::new(),
        };
        let id = claims.get("jti").and_then(Value::as_str).unwrap_or_default().to_string();
        if id.is_empty() || client_id.is_empty() {
            return Err(refuse("missing jti or aud"));
        }
        let subject = match claims.get("sub") {
            Some(Value::String(sub)) => sub.clone(),
            Some(Value::Number(sub)) => sub.to_string(),
            _ => String::new(),
        };
        let scopes = claims
            .get("scopes")
            .and_then(Value::as_array)
            .map(|list| list.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default();

        Ok(VerifiedToken {
            id,
            client_id,
            subject,
            scopes,
            expires_at: Utc.timestamp_opt(exp as i64, 0).single().unwrap_or(now),
        })
    }

    /// Verify `token` now.
    pub fn verify(&self, token: &str) -> Result<VerifiedToken> {
        self.verify_at(token, Utc::now())
    }
}

impl std::fmt::Debug for AccessTokenVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccessTokenVerifier")
            .field("keys", &self.keys.len())
            .field("issuer", &self.issuer)
            .field("leeway", &self.leeway)
            .finish()
    }
}

fn parse_public(pem: &str) -> Result<RsaPublicKey> {
    let pem = normalise_pem(pem);
    RsaPublicKey::from_public_key_pem(&pem)
        .or_else(|_| RsaPublicKey::from_pkcs1_pem(&pem))
        .map_err(|_| Error::internal("the access token public key is not a readable RSA PEM"))
}

/// Keys kept in an environment variable often arrive with literal `\n`s.
fn normalise_pem(pem: &str) -> String {
    pem.replace("\\n", "\n").trim().to_string()
}

/// Everything before the last `.` — the signed part.
fn header_len(token: &str) -> usize {
    token.rfind('.').unwrap_or(0)
}

fn decode_json(segment: &str) -> Option<Value> {
    serde_json::from_slice(&B64.decode(segment).ok()?).ok()
}

/// A NumericDate, integer or fractional.
fn number(claims: &Value, name: &str) -> Option<f64> {
    match claims.get(name)? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::OnceLock;

    /// One 2048-bit key for the whole test run — generating one is the slow
    /// part of every test that signs.
    pub(crate) fn key() -> RsaPrivateKey {
        static KEY: OnceLock<RsaPrivateKey> = OnceLock::new();
        KEY.get_or_init(|| {
            RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048).expect("generate a test key")
        })
        .clone()
    }

    fn claims(now: DateTime<Utc>) -> AccessTokenClaims {
        AccessTokenClaims {
            id: "token-id".into(),
            client_id: "client-id".into(),
            user_id: Some("7".into()),
            scopes: vec!["account".into()],
            issued_at: now,
            expires_at: now + chrono::Duration::hours(1),
        }
    }

    #[test]
    fn a_signed_token_has_the_documented_shape_and_verifies() {
        let signer = AccessTokenSigner::from_key(key());
        let now = Utc::now();
        let token = signer.sign(&claims(now)).unwrap();

        let mut parts = token.split('.');
        let header = decode_json(parts.next().unwrap()).unwrap();
        assert_eq!(header, json!({ "typ": "JWT", "alg": "RS256" }));
        let body = decode_json(parts.next().unwrap()).unwrap();
        assert_eq!(body["aud"], "client-id");
        assert_eq!(body["jti"], "token-id");
        assert_eq!(body["sub"], "7");
        assert_eq!(body["scopes"], json!(["account"]));
        assert_eq!(body["exp"], (now + chrono::Duration::hours(1)).timestamp());

        let verified = signer.verifier().verify(&token).unwrap();
        assert_eq!(verified.id, "token-id");
        assert_eq!(verified.client_id, "client-id");
        assert_eq!(verified.subject, "7");
        assert_eq!(verified.scopes, vec!["account"]);
    }

    #[test]
    fn a_client_token_names_the_client_as_its_subject() {
        let signer = AccessTokenSigner::from_key(key());
        let mut c = claims(Utc::now());
        c.user_id = None;
        let verified = signer.verifier().verify(&signer.sign(&c).unwrap()).unwrap();
        assert_eq!(verified.subject, "client-id");
    }

    /// Sign arbitrary header and claims, the way another issuer would.
    fn foreign(header: Value, body: Value) -> String {
        let signing_input = format!(
            "{}.{}",
            B64.encode(serde_json::to_vec(&header).unwrap()),
            B64.encode(serde_json::to_vec(&body).unwrap())
        );
        let signature = SigningKey::<Sha256>::new(key()).sign(signing_input.as_bytes()).to_bytes();
        format!("{signing_input}.{}", B64.encode(signature))
    }

    #[test]
    fn fractional_timestamps_an_array_audience_and_no_kid_all_verify() {
        let now = Utc::now().timestamp() as f64;
        let token = foreign(
            json!({ "typ": "JWT", "alg": "RS256" }),
            json!({
                "aud": ["client-id"], "jti": "j", "sub": "7", "scopes": [],
                "iat": now - 1.123456, "nbf": now - 1.123456, "exp": now + 3600.654321,
            }),
        );
        let verified =
            AccessTokenVerifier::from_key(RsaPublicKey::from(&key()), None).verify(&token).unwrap();
        assert_eq!(verified.client_id, "client-id");
    }

    #[test]
    fn expired_future_and_forged_tokens_are_refused() {
        let verifier = AccessTokenVerifier::from_key(RsaPublicKey::from(&key()), None);
        let now = Utc::now().timestamp();
        let body = |exp: i64, nbf: i64| json!({ "aud": "c", "jti": "j", "sub": "1", "scopes": [], "nbf": nbf, "exp": exp });
        let rs = json!({ "alg": "RS256" });

        assert!(verifier.verify(&foreign(rs.clone(), body(now - 1, now - 10))).is_err(), "expired");
        assert!(verifier.verify(&foreign(rs.clone(), body(now + 99, now + 60))).is_err(), "nbf");

        let none = foreign(json!({ "alg": "none" }), body(now + 99, now));
        assert!(verifier.verify(&none).is_err(), "alg none");

        let good = foreign(rs, body(now + 99, now));
        let mut tampered: Vec<&str> = good.split('.').collect();
        let other_body = B64.encode(serde_json::to_vec(&body(now + 99_999, now)).unwrap());
        tampered[1] = &other_body;
        assert!(verifier.verify(&tampered.join(".")).is_err(), "tampered claims");
    }

    #[test]
    fn a_rotation_accepts_the_previous_key() {
        let new_key = RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048).unwrap();
        let old = AccessTokenSigner::from_key(key());
        let old_token = old.sign(&claims(Utc::now())).unwrap();

        let verifier = AccessTokenVerifier::from_key(RsaPublicKey::from(&new_key), None);
        assert!(verifier.verify(&old_token).is_err());

        use rsa::pkcs8::{EncodePublicKey, LineEnding};
        let old_pem = RsaPublicKey::from(&key()).to_public_key_pem(LineEnding::LF).unwrap();
        let verifier = verifier.also_accepting(&old_pem, None).unwrap();
        assert!(verifier.verify(&old_token).is_ok());
    }

    #[test]
    fn pem_keys_with_escaped_newlines_are_read() {
        use rsa::pkcs8::{EncodePrivateKey, LineEnding};
        let pem = key().to_pkcs8_pem(LineEnding::LF).unwrap().replace('\n', "\\n");
        assert!(AccessTokenSigner::from_pem(&pem).is_ok());
    }
}
