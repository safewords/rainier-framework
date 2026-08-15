//! Verifying tokens against a **remote** JWKS.
//!
//! [`Jwt`](rainier_crypt::jwt::Jwt) verifies a token against a key ring you
//! already hold, and [`JwtKeyRing::from_jwks`](rainier_crypt::jwt::JwtKeyRing::from_jwks)
//! builds a ring from a JWKS document. What was missing — and what every
//! relying party ended up hand-rolling (see the copy in `maps-api`'s
//! `attest.rs`) — is the layer between: **fetch** the issuer's JWKS over HTTP,
//! **cache** it, **re-fetch** when a token names a key the cache does not hold,
//! and verify against the right audience.
//!
//! [`Jwks`] is that layer, once. Point it at an issuer and its JWKS URL, hold
//! one in the container, and verify:
//!
//! ```no_run
//! # use rainier_framework::jwks::Jwks;
//! # use serde::Deserialize;
//! # #[derive(Deserialize)] struct Claims { sub: String }
//! # async fn f(token: &str) -> rainier_support::Result<()> {
//! let jwks = Jwks::new("https://accounts.example.com", "https://accounts.example.com/.well-known/jwks.json");
//!
//! // One issuer, many audiences — pass the audience per call.
//! let claims: Claims = jwks.verify(token, "my-service").await?;
//! # let _ = claims.sub; Ok(()) }
//! ```
//!
//! ## Why it is safe to cache
//!
//! A signing key **added** upstream is picked up on its first token: a verify
//! that fails only because the token names an unknown `kid` triggers exactly
//! one re-fetch before it is rejected, so a rotation costs one HTTP call rather
//! than a cache lifetime of refused traffic. A key **removed** upstream stops
//! verifying once the cache expires ([`ttl`](Jwks::with_ttl), an hour by
//! default). And if the JWKS endpoint is briefly unreachable, the last-known
//! document keeps verifying rather than closing the door — identity blinking
//! must not take every relying party down with it.
//!
//! ## The audience is not optional
//!
//! `verify` takes the audience because binding it is what stops a token minted
//! for one service being replayed against another. A `Jwks` verifies for one
//! **issuer**; the caller names the **audience** each token must carry, which
//! is usually this service's own name.

use std::time::{Duration, Instant};

use rainier_crypt::jwt::{Jwt, JwtKeyRing};
use rainier_http_client::Http;
use rainier_support::{Error, Result};
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::sync::RwLock;

/// The default a fetched JWKS is trusted before it is fetched again.
const DEFAULT_TTL: Duration = Duration::from_secs(3600);

/// A verifier for tokens signed by a remote issuer whose keys are published at
/// a JWKS URL. Fetches and caches the JWKS; verifies against a per-call
/// audience. Hold one per issuer, typically in the container.
pub struct Jwks {
    issuer: String,
    url: String,
    ttl: Duration,
    timeout: Duration,
    cache: RwLock<Option<(Value, Instant)>>,
}

impl Jwks {
    /// A verifier for `issuer`, fetching its keys from `jwks_url`.
    pub fn new(issuer: impl Into<String>, jwks_url: impl Into<String>) -> Self {
        Self {
            issuer: issuer.into().trim().trim_end_matches('/').to_string(),
            url: jwks_url.into().trim().to_string(),
            ttl: DEFAULT_TTL,
            timeout: Duration::from_secs(10),
            cache: RwLock::new(None),
        }
    }

    /// How long a fetched document is trusted before it is fetched again. This
    /// bounds how long a *removed* key keeps verifying; an *added* key is
    /// picked up immediately on its first token regardless.
    pub fn with_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    /// How long to wait for the JWKS endpoint before falling back to the cache.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The issuer this verifier trusts.
    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    /// Verify a token and deserialise its claims.
    ///
    /// Checks the signature against the issuer's published keys, that `iss`
    /// matches, that `aud` includes `audience`, and the time bounds. Re-fetches
    /// the JWKS once if the token names a key the cache does not hold.
    ///
    /// `Err` is a `401`-class error for a token that does not verify, and a
    /// `500`-class one only if the JWKS could not be reached and nothing is
    /// cached — the two a caller usually wants to answer differently.
    pub async fn verify<C: DeserializeOwned>(&self, token: &str, audience: &str) -> Result<C> {
        let verifier = self.verifier(audience, false).await?;

        match verifier.verify::<C>(token) {
            Ok(claims) => Ok(claims),
            Err(e) => {
                // The key is unknown, not wrong: re-fetch once and retry.
                let unknown_key =
                    Jwt::kid_of(token).is_some_and(|kid| verifier.ring().find(&kid).is_none());
                if !unknown_key {
                    return Err(e);
                }
                self.verifier(audience, true).await?.verify::<C>(token)
            }
        }
    }

    /// A `Jwt` bound to this issuer and the given audience, over the cached (or
    /// freshly fetched) ring.
    async fn verifier(&self, audience: &str, force: bool) -> Result<Jwt> {
        let document = self.document(force).await?;
        let ring = JwtKeyRing::from_jwks(&document)?;
        Ok(Jwt::new(ring).issued_by(self.issuer.clone()).for_audience(audience))
    }

    /// The cached JWKS document, refetched when stale or `force`d. Serves the
    /// last-known document when a refetch fails.
    async fn document(&self, force: bool) -> Result<Value> {
        if !force {
            if let Some((document, fetched)) = self.cache.read().await.as_ref() {
                if fetched.elapsed() < self.ttl {
                    return Ok(document.clone());
                }
            }
        }

        match self.fetch().await {
            Ok(document) => {
                *self.cache.write().await = Some((document.clone(), Instant::now()));
                Ok(document)
            }
            Err(e) => {
                // Keep serving whatever is cached, however old.
                if let Some((document, _)) = self.cache.read().await.as_ref() {
                    return Ok(document.clone());
                }
                Err(e)
            }
        }
    }

    async fn fetch(&self) -> Result<Value> {
        if self.url.is_empty() {
            return Err(Error::internal("no JWKS URL is configured"));
        }
        let response = Http::get(&self.url).timeout(self.timeout).send().await?;
        if !response.is_success() {
            return Err(Error::internal(format!(
                "the JWKS endpoint answered {}",
                response.status()
            )));
        }
        serde_json::from_slice(response.bytes())
            .map_err(|e| Error::internal(format!("the JWKS document is not JSON: {e}")))
    }
}

impl std::fmt::Debug for Jwks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Jwks").field("issuer", &self.issuer).field("url", &self.url).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_crypt::jwt::JwtKey;
    use serde::{Deserialize, Serialize};

    const ISSUER: &str = "https://accounts.example.com";

    #[derive(Serialize)]
    struct Minted {
        iss: String,
        sub: String,
        aud: String,
        exp: i64,
        nbf: i64,
    }

    #[derive(Deserialize)]
    struct Claims {
        sub: String,
    }

    fn minted(aud: &str) -> Minted {
        Minted { iss: ISSUER.into(), sub: "7".into(), aud: aud.into(), exp: 9_999_999_999, nbf: 0 }
    }

    /// The verification itself, without the fetch — a `Jwks` seeded with a
    /// document, so the audience binding and issuer check can be asserted
    /// directly.
    async fn seeded(identity: &Jwt) -> Jwks {
        let jwks = Jwks::new(ISSUER, "http://unused.test/jwks");
        *jwks.cache.write().await = Some((identity.jwks(), Instant::now()));
        jwks
    }

    #[tokio::test]
    async fn a_token_for_the_named_audience_verifies() {
        let identity = Jwt::new(JwtKeyRing::new(JwtKey::generate_rs256("k1", 2048).unwrap()));
        let jwks = seeded(&identity).await;

        let token = identity.sign(&minted("my-service")).unwrap();
        let claims: Claims = jwks.verify(&token, "my-service").await.unwrap();
        assert_eq!(claims.sub, "7");
    }

    #[tokio::test]
    async fn a_token_for_another_audience_is_refused() {
        let identity = Jwt::new(JwtKeyRing::new(JwtKey::generate_rs256("k1", 2048).unwrap()));
        let jwks = seeded(&identity).await;

        let token = identity.sign(&minted("someone-else")).unwrap();
        assert!(jwks.verify::<Claims>(&token, "my-service").await.is_err());
    }

    #[tokio::test]
    async fn a_token_from_another_issuer_is_refused() {
        let identity = Jwt::new(JwtKeyRing::new(JwtKey::generate_rs256("k1", 2048).unwrap()));
        let jwks = seeded(&identity).await;

        let impostor = Jwt::new(JwtKeyRing::new(JwtKey::generate_rs256("k1", 2048).unwrap()));
        let token = impostor.sign(&minted("my-service")).unwrap();
        assert!(jwks.verify::<Claims>(&token, "my-service").await.is_err());
    }
}
