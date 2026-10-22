//! OpenID Connect, by discovery — [`Oidc`].
//!
//! # What is verified
//!
//! The person comes from the **ID token**, not from an API call, and the ID
//! token is checked before a single claim is read:
//!
//! - the **signature**, against the issuer's published JWKS, by the `kid` the
//!   token names — refetched once when the token names a key the cached set
//!   does not hold, which is what an issuer rotating its keys looks like;
//! - the **algorithm** from the key, never from the token's header (see
//!   [`Jwt::verify`]);
//! - `exp`, with a minute of leeway;
//! - `aud` contains this client's id, and `azp`, when present, is this client;
//! - `iss` is the issuer the discovery document named — with `{tenantid}`
//!   filled from the token's `tid` for Microsoft's multi-tenant endpoints;
//! - `nonce` is the value this session generated, compared in constant time.
//!
//! # Discovery is fetched once
//!
//! The document at `/.well-known/openid-configuration` is fetched on first use
//! and held for the life of the provider. Its `issuer` must equal the issuer
//! this was configured with (OpenID Connect Discovery §4.3) — a document
//! claiming to be some other issuer is refused rather than trusted.

use async_trait::async_trait;
use rainier_crypt::{Jwt, JwtKeyRing};
use rainier_support::{Error, Result};
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::{OnceCell, RwLock};

use crate::api::Api;
use crate::provider::{Endpoints, ProfileSource};
use crate::secret::same;
use crate::user::{flag, text, AccessToken, Profile};

/// What a discovery document says.
#[derive(Debug, Clone)]
struct Discovery {
    issuer: String,
    authorize: String,
    token: String,
    userinfo: Option<String>,
    jwks_uri: String,
}

/// Who an OpenID Connect sign-in belongs to.
pub struct Oidc {
    issuer: String,
    client_id: String,
    also_accept: Vec<String>,
    multi_tenant: bool,
    trust_email_verified: bool,
    discovery: OnceCell<Discovery>,
    keys: RwLock<Option<Arc<Jwt>>>,
}

impl Oidc {
    /// The issuer at `issuer`, for the client `client_id`.
    pub fn new(issuer: impl Into<String>, client_id: impl Into<String>) -> Self {
        Self {
            issuer: issuer.into().trim_end_matches('/').to_string(),
            client_id: client_id.into(),
            also_accept: Vec::new(),
            multi_tenant: false,
            trust_email_verified: true,
            discovery: OnceCell::new(),
            keys: RwLock::new(None),
        }
    }

    /// Accept ID tokens whose `iss` is `issuer` as well as the discovered one.
    #[must_use]
    pub fn also_accept_issuer(mut self, issuer: impl Into<String>) -> Self {
        self.also_accept.push(issuer.into());
        self
    }

    /// A Microsoft multi-tenant endpoint: its discovery document names a
    /// templated issuer (`…/{tenantid}/v2.0`) rather than itself, and each
    /// token's `iss` is that template filled with the token's own `tid`.
    #[must_use]
    pub fn multi_tenant(mut self, multi: bool) -> Self {
        self.multi_tenant = multi;
        self
    }

    /// Never report an email as verified, whatever the issuer claims — for an
    /// issuer whose `email` is not something it verifies.
    #[must_use]
    pub fn ignore_email_verified(mut self) -> Self {
        self.trust_email_verified = false;
        self
    }

    async fn discovery(&self, api: &Api) -> Result<&Discovery> {
        self.discovery
            .get_or_try_init(|| async {
                let url = format!("{}/.well-known/openid-configuration", self.issuer);
                let document = api.get_json(&self.issuer, &url, None).await?;

                let field = |name: &str| {
                    text(&document, name).ok_or_else(|| {
                        Error::internal(format!(
                            "{url} names no `{name}`, so it is not a discovery document"
                        ))
                    })
                };

                let issuer = field("issuer")?.trim_end_matches('/').to_string();
                if !self.multi_tenant && issuer != self.issuer {
                    return Err(Error::internal(format!(
                        "{url} claims to be the issuer `{issuer}`, not `{}`",
                        self.issuer
                    )));
                }

                Ok(Discovery {
                    issuer,
                    authorize: field("authorization_endpoint")?,
                    token: field("token_endpoint")?,
                    userinfo: text(&document, "userinfo_endpoint"),
                    jwks_uri: field("jwks_uri")?,
                })
            })
            .await
    }

    /// The verifier over the issuer's keys — refetched when `fresh`.
    async fn verifier(&self, api: &Api, discovery: &Discovery, fresh: bool) -> Result<Arc<Jwt>> {
        if !fresh {
            if let Some(jwt) = self.keys.read().await.as_ref() {
                return Ok(Arc::clone(jwt));
            }
        }

        let mut document = api.get_json(&self.issuer, &discovery.jwks_uri, None).await?;
        infer_algorithms(&mut document);

        let jwt =
            Arc::new(Jwt::new(JwtKeyRing::from_jwks(&document)?).for_audience(&self.client_id));
        *self.keys.write().await = Some(Arc::clone(&jwt));
        Ok(jwt)
    }

    /// The ID token's claims, once everything in the module docs holds.
    async fn verify(&self, api: &Api, id_token: &str, nonce: Option<&str>) -> Result<Value> {
        let discovery = self.discovery(api).await?;

        let kid = Jwt::kid_of(id_token)
            .ok_or_else(|| Error::unauthenticated("the ID token names no signing key"))?;
        let mut jwt = self.verifier(api, discovery, false).await?;
        if jwt.ring().find(&kid).is_none() {
            // The issuer rotated. One refetch, not a loop: a key that is not
            // in the fresh set either is a key the issuer does not hold.
            jwt = self.verifier(api, discovery, true).await?;
        }

        let claims: Value = jwt.verify(id_token)?;

        let expected = match text(&claims, "tid") {
            Some(tid) => discovery.issuer.replace("{tenantid}", &tid),
            None => discovery.issuer.clone(),
        };
        let iss = text(&claims, "iss").unwrap_or_default();
        if iss.contains('{') || (iss != expected && !self.also_accept.contains(&iss)) {
            return Err(Error::unauthenticated(format!(
                "the ID token was issued by `{iss}`, not `{expected}`"
            )));
        }

        if let Some(azp) = text(&claims, "azp") {
            if azp != self.client_id {
                return Err(Error::unauthenticated("the ID token was issued to another client"));
            }
        }

        if let Some(expected) = nonce {
            let carried = text(&claims, "nonce").unwrap_or_default();
            if !same(&carried, expected) {
                return Err(Error::unauthenticated(
                    "the ID token's nonce is not this sign-in's — it was minted for another",
                ));
            }
        }

        if text(&claims, "sub").is_none() {
            return Err(Error::unauthenticated("the ID token names no subject"));
        }

        Ok(claims)
    }

    fn profile_from(&self, claims: Value) -> Profile {
        Profile {
            id: text(&claims, "sub").unwrap_or_default(),
            email: text(&claims, "email"),
            email_verified: if self.trust_email_verified {
                flag(&claims, "email_verified")
            } else {
                None
            },
            name: text(&claims, "name"),
            nickname: text(&claims, "preferred_username").or_else(|| text(&claims, "nickname")),
            avatar: text(&claims, "picture"),
            raw: claims,
        }
    }
}

impl std::fmt::Debug for Oidc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Oidc")
            .field("issuer", &self.issuer)
            .field("client_id", &self.client_id)
            .field("multi_tenant", &self.multi_tenant)
            .finish()
    }
}

#[async_trait]
impl ProfileSource for Oidc {
    async fn endpoints(&self, api: &Api) -> Result<Endpoints> {
        let discovery = self.discovery(api).await?;
        Ok(Endpoints { authorize: discovery.authorize.clone(), token: discovery.token.clone() })
    }

    fn uses_nonce(&self) -> bool {
        true
    }

    async fn profile(
        &self,
        api: &Api,
        token: &AccessToken,
        nonce: Option<&str>,
    ) -> Result<Profile> {
        let discovery = self.discovery(api).await?;

        let Some(id_token) = token.id_token.as_deref() else {
            // A round trip that asked for `openid` and got no ID token is not
            // an OpenID Connect sign-in, and is refused rather than quietly
            // downgraded to a userinfo call nobody can tie to this session.
            if nonce.is_some() {
                return Err(Error::unauthenticated(format!(
                    "{} issued no ID token — is `openid` among the scopes?",
                    self.issuer
                )));
            }
            // A token handed in from outside: ask the issuer who it is for.
            let url = discovery.userinfo.as_deref().ok_or_else(|| {
                Error::internal(format!("{} publishes no userinfo endpoint", self.issuer))
            })?;
            let claims = api.get_json(&self.issuer, url, Some(&token.access_token)).await?;
            return Ok(self.profile_from(claims));
        };

        let mut claims = self.verify(api, id_token, nonce).await?;

        // Some issuers keep the ID token lean and put the email only behind
        // userinfo. Merge it — but only for the same subject.
        if text(&claims, "email").is_none() {
            if let Some(url) = discovery.userinfo.as_deref() {
                let info = api.get_json(&self.issuer, url, Some(&token.access_token)).await?;
                if text(&info, "sub") == text(&claims, "sub") {
                    if let (Some(into), Some(from)) = (claims.as_object_mut(), info.as_object()) {
                        for (key, value) in from {
                            into.entry(key.clone()).or_insert_with(|| value.clone());
                        }
                    }
                }
            }
        }

        Ok(self.profile_from(claims))
    }
}

/// Give each JWKS entry without an `alg` the one its key type implies.
///
/// `JwtKey::from_jwk` takes the algorithm from the key — rightly, never from
/// the token — and so needs `alg`, which RFC 7517 makes optional and Microsoft
/// omits. An RSA signing key means RS256 and a P-256 key ES256 in every issuer
/// this talks to; anything else is left alone and skipped by the ring.
fn infer_algorithms(document: &mut Value) {
    let Some(keys) = document.get_mut("keys").and_then(Value::as_array_mut) else { return };
    for key in keys {
        let Some(object) = key.as_object_mut() else { continue };
        if object.contains_key("alg") {
            continue;
        }
        let alg = match (
            object.get("kty").and_then(Value::as_str),
            object.get("crv").and_then(Value::as_str),
        ) {
            (Some("RSA"), _) => "RS256",
            (Some("EC"), Some("P-256")) => "ES256",
            _ => continue,
        };
        object.insert("alg".into(), Value::String(alg.into()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_missing_alg_is_inferred_from_the_key_type() {
        let mut document = json!({ "keys": [
            { "kty": "RSA", "kid": "a" },
            { "kty": "EC", "crv": "P-256", "kid": "b" },
            { "kty": "EC", "crv": "P-384", "kid": "c" },
            { "kty": "RSA", "kid": "d", "alg": "RS512" },
        ]});
        infer_algorithms(&mut document);

        assert_eq!(document["keys"][0]["alg"], "RS256");
        assert_eq!(document["keys"][1]["alg"], "ES256");
        assert!(document["keys"][2].get("alg").is_none());
        assert_eq!(document["keys"][3]["alg"], "RS512");
    }
}
