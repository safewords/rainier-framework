//! The authorization server — [`AuthorizationServer`].
//!
//! One value, built once at boot, answering three things:
//!
//! - **the token endpoint**, [`respond_to_token_request`](AuthorizationServer::respond_to_token_request):
//!   `authorization_code` (with PKCE), `refresh_token` (with rotation),
//!   `password`, `client_credentials`;
//! - **the authorization endpoint's two halves**,
//!   [`validate_authorization_request`](AuthorizationServer::validate_authorization_request)
//!   before the application's consent screen and
//!   [`complete_authorization_request`](AuthorizationServer::complete_authorization_request)
//!   after it;
//! - **personal access tokens**,
//!   [`issue_personal_access_token`](AuthorizationServer::issue_personal_access_token).
//!
//! The consent screen is the application's: what it looks like and whether a
//! returning user skips it are product decisions, so the server validates the
//! request, hands it over, and finishes what the application decides.
//! [`AuthorizationSession`](crate::AuthorizationSession) carries the request
//! across that round trip.
//!
//! # Spending is claiming
//!
//! A code and a refresh token are each good for one exchange. The server
//! revokes the row **before** issuing anything, and issues only if that
//! revocation was the one that took — so two requests racing with one code
//! get one token between them. (The common PHP implementation revokes after
//! issuing, which lets a raced code mint two; this is the one place the
//! behaviour deliberately differs.)

use std::sync::Arc;
use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine as _;
use chrono::Utc;
use rainier_crypt::Hasher;
use rainier_http::{Request, Response};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::codec::{open_json, seal_json, AuthCodePayload, RefreshTokenPayload, TokenSealer};
use crate::error::{append_query, found, token_response, OAuthError};
use crate::jwt::{AccessTokenClaims, AccessTokenSigner, AccessTokenVerifier};
use crate::model::{
    grant, new_identifier, AccessTokenRecord, AuthCodeRecord, Client, RefreshTokenRecord,
};
use crate::owners::ResourceOwners;
use crate::scope::Scopes;
use crate::store::{OAuthStore, PurgeOptions, Purged};

/// How long each kind of credential lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lifetimes {
    /// Access tokens from every grant but personal access. An hour.
    pub access_token: Duration,
    /// Refresh tokens. Thirty days.
    pub refresh_token: Duration,
    /// Authorization codes. Ten minutes — long enough for a redirect.
    pub auth_code: Duration,
    /// Personal access tokens. A year.
    pub personal_access_token: Duration,
}

impl Default for Lifetimes {
    fn default() -> Self {
        Self {
            access_token: Duration::from_secs(60 * 60),
            refresh_token: Duration::from_secs(30 * 24 * 60 * 60),
            auth_code: Duration::from_secs(10 * 60),
            personal_access_token: Duration::from_secs(365 * 24 * 60 * 60),
        }
    }
}

/// An authorization request the server validated, waiting on the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorizationRequest {
    /// The client asking.
    pub client_id: String,
    /// Its display name, for the consent screen.
    pub client_name: String,
    /// The redirect URI the request named, if it named one.
    pub redirect_uri: Option<String>,
    /// Where the answer goes — the named URI, or the client's only one.
    pub final_redirect_uri: String,
    /// The client's `state`, echoed back.
    pub state: Option<String>,
    /// The scopes requested.
    pub scopes: Vec<String>,
    /// The PKCE challenge.
    pub code_challenge: Option<String>,
    /// `S256` or `plain`.
    pub code_challenge_method: Option<String>,
}

/// A personal access token, freshly issued.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersonalAccessToken {
    /// The bearer token. Shown to the user once; it is not stored.
    pub access_token: String,
    /// The stored record.
    pub token: AccessTokenRecord,
    /// Seconds until it expires.
    pub expires_in: i64,
}

/// What a token request produced.
struct Issued {
    access: AccessTokenRecord,
    jwt: String,
    refresh: Option<String>,
}

/// The authorization server. See the [module documentation](self).
pub struct AuthorizationServer {
    store: Arc<dyn OAuthStore>,
    sealer: Arc<dyn TokenSealer>,
    signer: AccessTokenSigner,
    hasher: Arc<dyn Hasher>,
    scopes: Scopes,
    owners: Option<Arc<dyn ResourceOwners>>,
    lifetimes: Lifetimes,
    rotate_refresh_tokens: bool,
    require_pkce_for_public_clients: bool,
    allow_plain_pkce: bool,
}

impl AuthorizationServer {
    /// A server keeping records in `store`, sealing refresh tokens and codes
    /// with `sealer`, signing access tokens with `signer`, and checking client
    /// secrets with `hasher`.
    ///
    /// Only `authorization_code`, `refresh_token` and `client_credentials` are
    /// answered until [`password_grant`](Self::password_grant) is given
    /// somewhere to check passwords.
    pub fn new(
        store: Arc<dyn OAuthStore>,
        sealer: Arc<dyn TokenSealer>,
        signer: AccessTokenSigner,
        hasher: Arc<dyn Hasher>,
    ) -> Self {
        Self {
            store,
            sealer,
            signer,
            hasher,
            scopes: Scopes::new(),
            owners: None,
            lifetimes: Lifetimes::default(),
            rotate_refresh_tokens: true,
            require_pkce_for_public_clients: true,
            allow_plain_pkce: true,
        }
    }

    /// The scopes this server knows.
    pub fn scopes(mut self, scopes: Scopes) -> Self {
        self.scopes = scopes;
        self
    }

    /// Answer the password grant, checking credentials with `owners`.
    ///
    /// RFC 9700 (OAuth security BCP) says not to: it teaches users to type
    /// their password into somebody else's app. It is here because first-party
    /// mobile apps were built on it and have to keep working while they move.
    pub fn password_grant(mut self, owners: Arc<dyn ResourceOwners>) -> Self {
        self.owners = Some(owners);
        self
    }

    /// Credential lifetimes.
    pub fn lifetimes(mut self, lifetimes: Lifetimes) -> Self {
        self.lifetimes = lifetimes;
        self
    }

    /// Whether spending a refresh token revokes it. On by default — rotation,
    /// so a stolen refresh token stops working the next time its owner uses
    /// theirs.
    pub fn rotate_refresh_tokens(mut self, rotate: bool) -> Self {
        self.rotate_refresh_tokens = rotate;
        self
    }

    /// Whether a public client must use PKCE. On by default, as RFC 9700
    /// requires; turning it off is for clients shipped before it was.
    pub fn require_pkce_for_public_clients(mut self, require: bool) -> Self {
        self.require_pkce_for_public_clients = require;
        self
    }

    /// Whether the `plain` PKCE method is accepted. On by default for
    /// compatibility; `S256` is the one worth using.
    pub fn allow_plain_pkce(mut self, allow: bool) -> Self {
        self.allow_plain_pkce = allow;
        self
    }

    /// The verifier for tokens this server signs.
    pub fn verifier(&self) -> AccessTokenVerifier {
        self.signer.verifier()
    }

    /// The store.
    pub fn store(&self) -> &Arc<dyn OAuthStore> {
        &self.store
    }

    /// The known scopes.
    pub fn known_scopes(&self) -> &Scopes {
        &self.scopes
    }

    // --- the token endpoint ---------------------------------------------------

    /// `POST /oauth/token`.
    pub async fn respond_to_token_request(&self, request: &Request) -> Response {
        match self.token_request(request).await {
            Ok(issued) => {
                let mut body = Map::new();
                body.insert("token_type".into(), "Bearer".into());
                body.insert(
                    "expires_in".into(),
                    (issued.access.expires_at - Utc::now()).num_seconds().into(),
                );
                body.insert("access_token".into(), issued.jwt.into());
                if let Some(refresh) = issued.refresh {
                    body.insert("refresh_token".into(), refresh.into());
                }
                token_response(&Value::Object(body))
            }
            Err(mut error) => {
                if error.error == "invalid_client" {
                    error.authorization_header = request.header("authorization").map(|h| {
                        if h.len() >= 6 && h[..6].eq_ignore_ascii_case("bearer") {
                            "Bearer"
                        } else {
                            "Basic"
                        }
                    });
                }
                error.into_response()
            }
        }
    }

    async fn token_request(&self, request: &Request) -> Result<Issued, OAuthError> {
        match param(request, "grant_type").as_deref() {
            Some(grant::AUTHORIZATION_CODE) => self.authorization_code(request).await,
            Some(grant::REFRESH_TOKEN) => self.refresh_token(request).await,
            Some(grant::PASSWORD) if self.owners.is_some() => self.password(request).await,
            Some(grant::CLIENT_CREDENTIALS) => self.client_credentials(request).await,
            _ => Err(OAuthError::unsupported_grant_type()),
        }
    }

    async fn authorization_code(&self, request: &Request) -> Result<Issued, OAuthError> {
        let client = self.authenticate_client(request, grant::AUTHORIZATION_CODE).await?;
        let sealed = param(request, "code").ok_or_else(|| OAuthError::invalid_request("code"))?;

        let payload: AuthCodePayload = open_json(&*self.sealer, &sealed).ok_or_else(|| {
            OAuthError::invalid_grant("Cannot validate the provided authorization code")
        })?;
        if payload.expire_time < Utc::now().timestamp() {
            return Err(OAuthError::invalid_grant("Authorization code has expired"));
        }
        if payload.client_id != client.id {
            return Err(OAuthError::invalid_request("code")
                .with_hint("Authorization code was not issued to this client"));
        }
        let redirect_uri = param(request, "redirect_uri");
        match (&payload.redirect_uri, &redirect_uri) {
            (Some(_), None) => return Err(OAuthError::invalid_request("redirect_uri")),
            (expected, Some(given)) if expected.as_deref() != Some(given.as_str()) => {
                return Err(
                    OAuthError::invalid_request("redirect_uri").with_hint("Invalid redirect URI")
                );
            }
            _ => {}
        }

        let verifier = param(request, "code_verifier");
        match (&payload.code_challenge, &verifier) {
            (None, Some(_)) => {
                return Err(OAuthError::invalid_request("code_challenge")
                    .with_hint("code_verifier received when no code_challenge is present"));
            }
            (Some(challenge), _) => {
                let verifier =
                    verifier.ok_or_else(|| OAuthError::invalid_request("code_verifier"))?;
                if !is_pkce_string(&verifier) {
                    return Err(OAuthError::invalid_request("code_verifier")
                        .with_hint("Code Verifier must follow the specifications of RFC-7636."));
                }
                let method = payload.code_challenge_method.as_deref().unwrap_or("plain");
                let computed = match method {
                    "S256" => B64.encode(Sha256::digest(verifier.as_bytes())),
                    "plain" if self.allow_plain_pkce => verifier.clone(),
                    other => {
                        return Err(OAuthError::server_error(format!(
                            "unsupported code challenge method `{other}`"
                        )))
                    }
                };
                if !bool::from(computed.as_bytes().ct_eq(challenge.as_bytes())) {
                    return Err(OAuthError::invalid_grant("Failed to verify `code_verifier`."));
                }
            }
            (None, None) => {}
        }

        let scopes = self.scopes.validate(Some(&payload.scopes.join(" ")))?;
        let scopes = self.scopes.finalize(scopes, grant::AUTHORIZATION_CODE, &client);

        // Spend the code before issuing anything — see the module docs.
        if !self.store.revoke_auth_code(&payload.auth_code_id).await? {
            return Err(OAuthError::invalid_grant("Authorization code has been revoked"));
        }

        let user = payload.user_id.clone();
        self.issue(&client, user, scopes, self.lifetimes.access_token, true, None).await
    }

    async fn refresh_token(&self, request: &Request) -> Result<Issued, OAuthError> {
        let client = self.authenticate_client(request, grant::REFRESH_TOKEN).await?;
        let sealed = param(request, "refresh_token")
            .ok_or_else(|| OAuthError::invalid_request("refresh_token"))?;

        let old: RefreshTokenPayload = open_json(&*self.sealer, &sealed)
            .ok_or_else(|| OAuthError::invalid_refresh_token("Cannot decrypt the refresh token"))?;
        if old.client_id != client.id {
            return Err(OAuthError::invalid_refresh_token("Token is not linked to client"));
        }
        if old.expire_time < Utc::now().timestamp() {
            return Err(OAuthError::invalid_refresh_token("Token has expired"));
        }

        let requested = param(request, "scope").unwrap_or_else(|| old.scopes.join(" "));
        let scopes = self.scopes.validate(Some(&requested))?;
        if let Some(extra) = scopes.iter().find(|scope| !old.scopes.contains(scope)) {
            return Err(OAuthError::invalid_scope(extra));
        }
        let scopes = self.scopes.finalize(scopes, grant::REFRESH_TOKEN, &client);

        // Spend it first. Without rotation it is not spent, but it still has to
        // be live.
        if self.rotate_refresh_tokens {
            if !self.store.revoke_refresh_token(&old.refresh_token_id).await? {
                return Err(OAuthError::invalid_refresh_token("Token has been revoked"));
            }
        } else if self
            .store
            .find_refresh_token(&old.refresh_token_id)
            .await?
            .is_none_or(|row| row.revoked)
        {
            return Err(OAuthError::invalid_refresh_token("Token has been revoked"));
        }
        self.store.revoke_access_token(&old.access_token_id).await?;

        self.issue(&client, old.user_id, scopes, self.lifetimes.access_token, true, None).await
    }

    async fn password(&self, request: &Request) -> Result<Issued, OAuthError> {
        let client = self.authenticate_client(request, grant::PASSWORD).await?;
        let scopes = self.scopes.validate(param(request, "scope").as_deref())?;
        let username =
            param(request, "username").ok_or_else(|| OAuthError::invalid_request("username"))?;
        let password =
            param(request, "password").ok_or_else(|| OAuthError::invalid_request("password"))?;

        let owners = self.owners.as_ref().ok_or_else(OAuthError::unsupported_grant_type)?;
        let user = owners
            .verify(&username, &password, &client)
            .await?
            .ok_or_else(OAuthError::invalid_credentials)?;

        let scopes = self.scopes.finalize(scopes, grant::PASSWORD, &client);
        self.issue(&client, Some(user), scopes, self.lifetimes.access_token, true, None).await
    }

    async fn client_credentials(&self, request: &Request) -> Result<Issued, OAuthError> {
        let client = self.authenticate_client(request, grant::CLIENT_CREDENTIALS).await?;
        if !client.is_confidential() {
            return Err(OAuthError::invalid_client());
        }
        let scopes = self.scopes.validate(param(request, "scope").as_deref())?;
        let scopes = self.scopes.finalize(scopes, grant::CLIENT_CREDENTIALS, &client);
        self.issue(&client, None, scopes, self.lifetimes.access_token, false, None).await
    }

    /// Identify the client from Basic auth or the body, check it may use
    /// `grant`, and check its secret when it has one.
    async fn authenticate_client(
        &self,
        request: &Request,
        grant: &str,
    ) -> Result<Client, OAuthError> {
        let (basic_id, basic_secret) = basic_credentials(request);
        let client_id = param(request, "client_id")
            .or(basic_id)
            .ok_or_else(|| OAuthError::invalid_request("client_id"))?;
        let secret = param(request, "client_secret").or(basic_secret).unwrap_or_default();

        let client =
            self.active_client(&client_id).await?.ok_or_else(OAuthError::invalid_client)?;
        if !client.supports_grant(grant) {
            return Err(OAuthError::unauthorized_client());
        }
        if client.is_confidential() {
            if secret.is_empty() {
                return Err(OAuthError::invalid_request("client_secret"));
            }
            let hash = client.secret_hash.as_deref().unwrap_or_default();
            if !self.hasher.verify(&secret, hash) {
                return Err(OAuthError::invalid_client());
            }
        }
        Ok(client)
    }

    async fn active_client(&self, id: &str) -> Result<Option<Client>, OAuthError> {
        Ok(self.store.find_client(id).await?.filter(|client| !client.revoked))
    }

    /// Mint an access token, and a refresh token when asked for and the client
    /// may hold one.
    async fn issue(
        &self,
        client: &Client,
        user_id: Option<String>,
        scopes: Vec<String>,
        lifetime: Duration,
        with_refresh: bool,
        name: Option<String>,
    ) -> Result<Issued, OAuthError> {
        let now = Utc::now();
        let access = AccessTokenRecord {
            id: new_identifier(),
            user_id: user_id.clone(),
            client_id: client.id.clone(),
            name,
            scopes: scopes.clone(),
            revoked: false,
            created_at: now,
            expires_at: now + chrono_duration(lifetime),
        };
        self.store.create_access_token(&access).await?;

        let jwt = self.signer.sign(&AccessTokenClaims {
            id: access.id.clone(),
            client_id: client.id.clone(),
            user_id: user_id.clone(),
            scopes: scopes.clone(),
            issued_at: now,
            expires_at: access.expires_at,
        })?;

        let refresh = if with_refresh && client.supports_grant(grant::REFRESH_TOKEN) {
            let row = RefreshTokenRecord {
                id: new_identifier(),
                access_token_id: access.id.clone(),
                revoked: false,
                expires_at: now + chrono_duration(self.lifetimes.refresh_token),
            };
            self.store.create_refresh_token(&row).await?;
            Some(seal_json(
                &*self.sealer,
                &RefreshTokenPayload {
                    client_id: client.id.clone(),
                    refresh_token_id: row.id.clone(),
                    access_token_id: access.id.clone(),
                    scopes,
                    user_id,
                    expire_time: row.expires_at.timestamp(),
                },
            )?)
        } else {
            None
        };

        Ok(Issued { access, jwt, refresh })
    }

    // --- the authorization endpoint -------------------------------------------

    /// Check `GET /oauth/authorize` before showing the consent screen.
    ///
    /// An error that can safely go back to the client — a bad scope, once
    /// the redirect URI is known to be the client's — carries that redirect;
    /// one that cannot (an unknown client, an unregistered redirect URI) does
    /// not, and must be shown to the user instead, because redirecting to an
    /// unverified URI is an open redirect.
    pub async fn validate_authorization_request(
        &self,
        request: &Request,
    ) -> Result<AuthorizationRequest, OAuthError> {
        if query(request, "response_type").as_deref() != Some("code") {
            return Err(OAuthError::unsupported_grant_type());
        }
        let client_id =
            query(request, "client_id").ok_or_else(|| OAuthError::invalid_request("client_id"))?;
        let client =
            self.active_client(&client_id).await?.ok_or_else(OAuthError::invalid_client)?;
        if !client.supports_grant(grant::AUTHORIZATION_CODE) {
            return Err(OAuthError::unauthorized_client());
        }

        let redirect_uri = query(request, "redirect_uri");
        let final_redirect_uri = match &redirect_uri {
            Some(uri) if redirect_uri_allowed(uri, &client.redirect_uris) => uri.clone(),
            Some(_) => return Err(OAuthError::invalid_client()),
            None if client.redirect_uris.len() == 1 => client.redirect_uris[0].clone(),
            None => return Err(OAuthError::invalid_client()),
        };

        let state = query(request, "state");
        let back = |error: OAuthError| {
            let pairs: Vec<(String, String)> =
                state.iter().map(|state| ("state".to_string(), state.clone())).collect();
            error.redirecting_to(append_query(&final_redirect_uri, &pairs))
        };

        let scopes = self.scopes.validate(query(request, "scope").as_deref()).map_err(back)?;

        let (code_challenge, code_challenge_method) = match query(request, "code_challenge") {
            Some(challenge) => {
                let method =
                    query(request, "code_challenge_method").unwrap_or_else(|| "plain".into());
                let supported = method == "S256" || (method == "plain" && self.allow_plain_pkce);
                if !supported {
                    let methods = if self.allow_plain_pkce { "`S256`, `plain`" } else { "`S256`" };
                    return Err(back(
                        OAuthError::invalid_request("code_challenge_method")
                            .with_hint(format!("Code challenge method must be one of {methods}")),
                    ));
                }
                if !is_pkce_string(&challenge) {
                    return Err(back(
                        OAuthError::invalid_request("code_challenge").with_hint(
                            "Code challenge must follow the specifications of RFC-7636.",
                        ),
                    ));
                }
                (Some(challenge), Some(method))
            }
            None if self.require_pkce_for_public_clients && !client.is_confidential() => {
                return Err(back(
                    OAuthError::invalid_request("code_challenge")
                        .with_hint("Code challenge must be provided for public clients"),
                ));
            }
            None => (None, None),
        };

        Ok(AuthorizationRequest {
            client_id: client.id,
            client_name: client.name,
            redirect_uri,
            final_redirect_uri,
            state,
            scopes,
            code_challenge,
            code_challenge_method,
        })
    }

    /// Whether `user_id` already holds a live token for this client covering
    /// every requested scope — the usual reason to skip the consent screen.
    pub async fn previously_approved(
        &self,
        authorization: &AuthorizationRequest,
        user_id: &str,
    ) -> rainier_support::Result<bool> {
        let tokens = self.store.tokens_for(user_id, Some(&authorization.client_id)).await?;
        Ok(tokens.iter().any(|token| {
            authorization.scopes.iter().all(|scope| crate::scope::held_grants(&token.scopes, scope))
        }))
    }

    /// Finish an authorization the user approved or denied: a `302` back to
    /// the client with a code, or with `access_denied`.
    pub async fn complete_authorization_request(
        &self,
        authorization: &AuthorizationRequest,
        user_id: &str,
        approved: bool,
    ) -> Response {
        let state: Vec<(String, String)> =
            authorization.state.iter().map(|s| ("state".to_string(), s.clone())).collect();

        if !approved {
            return OAuthError::access_denied("The user denied the request")
                .redirecting_to(append_query(&authorization.final_redirect_uri, &state))
                .into_response();
        }

        match self.issue_code(authorization, user_id).await {
            Ok(code) => {
                let mut pairs = vec![("code".to_string(), code)];
                pairs.extend(state);
                found(&append_query(&authorization.final_redirect_uri, &pairs))
            }
            Err(error) => error.into_response(),
        }
    }

    async fn issue_code(
        &self,
        authorization: &AuthorizationRequest,
        user_id: &str,
    ) -> Result<String, OAuthError> {
        let client = self
            .active_client(&authorization.client_id)
            .await?
            .ok_or_else(OAuthError::invalid_client)?;
        let scopes =
            self.scopes.finalize(authorization.scopes.clone(), grant::AUTHORIZATION_CODE, &client);
        let expires_at = Utc::now() + chrono_duration(self.lifetimes.auth_code);
        let row = AuthCodeRecord {
            id: new_identifier(),
            user_id: user_id.to_string(),
            client_id: client.id.clone(),
            scopes: scopes.clone(),
            revoked: false,
            expires_at,
        };
        self.store.create_auth_code(&row).await?;

        Ok(seal_json(
            &*self.sealer,
            &AuthCodePayload {
                client_id: client.id,
                redirect_uri: authorization.redirect_uri.clone(),
                auth_code_id: row.id,
                scopes,
                user_id: Some(user_id.to_string()),
                expire_time: expires_at.timestamp(),
                code_challenge: authorization.code_challenge.clone(),
                code_challenge_method: authorization.code_challenge_method.clone(),
            },
        )?)
    }

    // --- personal access tokens -----------------------------------------------

    /// Issue `user_id` a named, long-lived token through the personal access
    /// client for `provider`.
    pub async fn issue_personal_access_token(
        &self,
        user_id: &str,
        name: &str,
        scopes: &[&str],
        provider: Option<&str>,
    ) -> Result<PersonalAccessToken, OAuthError> {
        let client = self
            .store
            .personal_access_client(provider)
            .await?
            .filter(|client| !client.revoked)
            .ok_or_else(|| {
                OAuthError::server_error(format!(
                    "no personal access client for the `{}` provider",
                    provider.unwrap_or("default")
                ))
            })?;
        let scopes = self.scopes.validate(Some(&scopes.join(" ")))?;
        let scopes = self.scopes.finalize(scopes, grant::PERSONAL_ACCESS, &client);
        let issued = self
            .issue(
                &client,
                Some(user_id.to_string()),
                scopes,
                self.lifetimes.personal_access_token,
                false,
                Some(name.to_string()),
            )
            .await?;
        Ok(PersonalAccessToken {
            expires_in: (issued.access.expires_at - Utc::now()).num_seconds(),
            access_token: issued.jwt,
            token: issued.access,
        })
    }

    // --- revocation and housekeeping ------------------------------------------

    /// Revoke an access token and every refresh token issued with it.
    pub async fn revoke(&self, access_token_id: &str) -> rainier_support::Result<()> {
        self.store.revoke_access_token(access_token_id).await?;
        self.store.revoke_refresh_tokens_for(access_token_id).await?;
        Ok(())
    }

    /// `POST /oauth/revoke` — RFC 7009. The client authenticates as at the
    /// token endpoint and names a token it holds; whatever happens, the answer
    /// is `200`, so the endpoint cannot be used to probe which tokens exist.
    pub async fn respond_to_revocation_request(&self, request: &Request) -> Response {
        let (basic_id, basic_secret) = basic_credentials(request);
        let Some(client_id) = param(request, "client_id").or(basic_id) else {
            return OAuthError::invalid_request("client_id").into_response();
        };
        let secret = param(request, "client_secret").or(basic_secret).unwrap_or_default();
        let client = match self.active_client(&client_id).await {
            Ok(Some(client)) => client,
            Ok(None) => return OAuthError::invalid_client().into_response(),
            Err(error) => return error.into_response(),
        };
        if client.is_confidential()
            && !self.hasher.verify(&secret, client.secret_hash.as_deref().unwrap_or_default())
        {
            return OAuthError::invalid_client().into_response();
        }
        let Some(token) = param(request, "token") else {
            return OAuthError::invalid_request("token").into_response();
        };

        if let Ok(verified) = self.verifier().verify(&token) {
            if verified.client_id == client.id {
                let _ = self.revoke(&verified.id).await;
            }
        } else if let Some(refresh) = open_json::<RefreshTokenPayload>(&*self.sealer, &token) {
            if refresh.client_id == client.id {
                let _ = self.store.revoke_refresh_token(&refresh.refresh_token_id).await;
                let _ = self.revoke(&refresh.access_token_id).await;
            }
        }
        token_response(&json!({}))
    }

    /// Remove revoked rows, and rows that expired more than `retention` ago.
    /// Schedule it daily.
    pub async fn purge(&self, retention: Duration) -> rainier_support::Result<Purged> {
        self.store
            .purge(PurgeOptions {
                revoked: true,
                expired_before: Some(Utc::now() - chrono_duration(retention)),
            })
            .await
    }
}

impl std::fmt::Debug for AuthorizationServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthorizationServer")
            .field("lifetimes", &self.lifetimes)
            .field("password_grant", &self.owners.is_some())
            .field("rotate_refresh_tokens", &self.rotate_refresh_tokens)
            .finish_non_exhaustive()
    }
}

/// A body parameter, trimmed, absent when empty. The token endpoint reads the
/// body only — a secret in a query string ends up in access logs.
fn param(request: &Request, name: &str) -> Option<String> {
    scalar(request.body_input().get(name))
}

/// A query-string parameter, trimmed, absent when empty.
fn query(request: &Request, name: &str) -> Option<String> {
    scalar(request.query().get(name))
}

fn scalar(value: Option<&Value>) -> Option<String> {
    let text = match value? {
        Value::String(s) => s.trim().to_string(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        _ => return None,
    };
    (!text.is_empty()).then_some(text)
}

/// `Authorization: Basic …`, split. Malformed is the same as absent.
fn basic_credentials(request: &Request) -> (Option<String>, Option<String>) {
    let Some(header) = request.header("authorization") else { return (None, None) };
    if header.len() < 6 || !header[..6].eq_ignore_ascii_case("basic ") {
        return (None, None);
    }
    let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(header[6..].trim()) else {
        return (None, None);
    };
    let Ok(decoded) = String::from_utf8(decoded) else { return (None, None) };
    match decoded.split_once(':') {
        Some(("", _)) => (None, None),
        Some((user, password)) => (Some(user.to_string()), Some(password.to_string())),
        None => (None, None),
    }
}

/// RFC 7636's character set and length, for a verifier and a challenge.
fn is_pkce_string(value: &str) -> bool {
    (43..=128).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
}

/// Exact match, except that an `http` loopback URI matches on any port
/// (RFC 8252 §7.3 — a native app binds whatever port is free).
pub(crate) fn redirect_uri_allowed(uri: &str, allowed: &[String]) -> bool {
    if let Some(stripped) = strip_loopback_port(uri) {
        return allowed
            .iter()
            .any(|a| strip_loopback_port(a).as_deref() == Some(stripped.as_str()));
    }
    allowed.iter().any(|a| a == uri)
}

fn strip_loopback_port(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("http://")?;
    let host_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, path) = rest.split_at(host_end);
    let host = if authority.starts_with('[') {
        &authority[..authority.find(']')? + 1]
    } else {
        authority.split(':').next()?
    };
    matches!(host, "127.0.0.1" | "[::1]").then(|| format!("http://{host}{path}"))
}

fn chrono_duration(duration: Duration) -> chrono::Duration {
    chrono::Duration::from_std(duration).unwrap_or(chrono::Duration::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_redirects_match_on_any_port_and_nothing_else_does() {
        let allowed = vec!["http://127.0.0.1/callback".to_string(), "https://app.test/cb".into()];
        assert!(redirect_uri_allowed("http://127.0.0.1:53111/callback", &allowed));
        assert!(redirect_uri_allowed("https://app.test/cb", &allowed));
        assert!(!redirect_uri_allowed("https://app.test/cb/", &allowed));
        assert!(!redirect_uri_allowed("https://app.test:444/cb", &allowed));
        assert!(!redirect_uri_allowed("http://127.0.0.1:1/other", &allowed));
        assert!(!redirect_uri_allowed("http://localhost:1/callback", &allowed));
        let v6 = vec!["http://[::1]/cb".to_string()];
        assert!(redirect_uri_allowed("http://[::1]:8080/cb", &v6));
    }

    #[test]
    fn pkce_strings_follow_rfc_7636() {
        assert!(is_pkce_string(&"a".repeat(43)));
        assert!(!is_pkce_string(&"a".repeat(42)));
        assert!(!is_pkce_string(&format!("{}+", "a".repeat(43))));
    }
}
