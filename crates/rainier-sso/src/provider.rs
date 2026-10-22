//! The authorization-code flow — [`Provider`], and [`OAuth2Provider`], the
//! one implementation every built-in is a configuration of.
//!
//! # The round trip
//!
//! 1. [`redirect`](Provider::redirect) generates a `state`, a PKCE verifier
//!    and (for OpenID Connect) a `nonce`, keeps them in **this browser's
//!    session**, and sends the browser to the provider with the `state`, the
//!    verifier's `S256` challenge and the `nonce`.
//! 2. The provider sends the browser back with a `code` and the `state`.
//! 3. [`user`](Provider::user) takes the pending values **out** of the session
//!    — whatever happens next, they are gone — compares the `state` in
//!    constant time, redeems the `code` with the verifier, and asks the
//!    provider who the token belongs to.
//!
//! # What each value defends against
//!
//! | | |
//! |---|---|
//! | `state` | **Login CSRF.** Without it an attacker starts a sign-in with their own account, stops at the callback, and gets a victim's browser to finish it — the victim is now signed in as the attacker, and whatever they save is the attacker's. |
//! | PKCE | **Code interception.** A code that leaks — a referrer, a log, a malicious app registered for the same redirect — cannot be redeemed without the verifier, which never left this server. Always on, `S256` only. |
//! | `nonce` | **ID-token replay.** The ID token must carry the value this session generated, so one minted for some other sign-in is refused. |
//! | single use | **Replay of the callback URL.** The pending values are pulled, not read; a second visit to the same callback URL finds nothing. |
//!
//! # There is no `stateless()`
//!
//! Some OAuth libraries offer one, for APIs with no session. It switches off the `state`
//! check, which is the login-CSRF defence, and it is reached for whenever the
//! session middleware is missing from a route — which is a wiring mistake to
//! fix, not a check to remove. A mobile or SPA client that genuinely has no
//! session should run the flow itself (with its own PKCE) and hand this
//! server the access token: [`user_from_token`](Provider::user_from_token).

use std::sync::Arc;

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use chrono::Utc;
use rainier_http::{IntoResponse, Redirect, Request, Response};
use rainier_session::SessionRequestExt;
use rainier_support::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::api::Api;
use crate::secret::{code_challenge, random_token, same};
use crate::user::{text, AccessToken, Profile, SocialUser};

/// A sign-in provider.
///
/// Object-safe, so an [`Sso`](crate::Sso) manager holds them as
/// `Arc<dyn Provider>` and a route picks one by name.
#[async_trait]
pub trait Provider: Send + Sync + 'static {
    /// The name it is registered under — `"github"`.
    fn name(&self) -> &str;

    /// Start a sign-in: remember a fresh `state`, verifier and nonce in this
    /// request's session, and answer with a `302` to the provider.
    ///
    /// # Errors
    ///
    /// When the route has no session — the `state` would have nowhere to live,
    /// and the callback could not check it.
    async fn redirect(&self, request: &Request) -> Result<Response>;

    /// Finish a sign-in from the callback request.
    ///
    /// # Errors
    ///
    /// `401` when the provider reported an error, when no sign-in was in
    /// progress for this session, when the `state` does not match, or when the
    /// provider refused the code; `400` when the callback carried no code.
    async fn user(&self, request: &Request) -> Result<SocialUser>;

    /// Who an access token obtained elsewhere belongs to — a mobile app that
    /// ran the flow itself and hands the server the token.
    ///
    /// No ID token is involved, so for an OpenID Connect provider this asks
    /// the `userinfo` endpoint, which is the provider vouching for the token
    /// directly.
    async fn user_from_token(&self, token: AccessToken) -> Result<SocialUser>;

    /// Trade a refresh token for a new access token.
    async fn refresh(&self, refresh_token: &str) -> Result<AccessToken>;
}

/// Where a provider's flow happens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    /// The page the browser is sent to.
    pub authorize: String,
    /// Where a code is redeemed.
    pub token: String,
}

/// How a provider turns a token into a person — the part that differs between
/// providers.
///
/// The built-ins are [`GitHub`](crate::providers::GitHub),
/// [`GitLab`](crate::providers::GitLab), [`Discord`](crate::providers::Discord)
/// and [`Oidc`](crate::providers::Oidc) (which Google and Microsoft are
/// configurations of). Implement it for a provider none of those covers.
#[async_trait]
pub trait ProfileSource: Send + Sync + 'static {
    /// The authorize and token endpoints. Async because an OpenID Connect
    /// issuer publishes them in its discovery document.
    async fn endpoints(&self, api: &Api) -> Result<Endpoints>;

    /// Whether the authorize request carries a `nonce` the ID token must echo.
    fn uses_nonce(&self) -> bool {
        false
    }

    /// The person behind `token`.
    ///
    /// `nonce` is the value this session generated, when a sign-in round trip
    /// produced `token`; `None` from
    /// [`user_from_token`](Provider::user_from_token).
    async fn profile(&self, api: &Api, token: &AccessToken, nonce: Option<&str>)
        -> Result<Profile>;
}

/// The app's registration with a provider.
///
/// `Debug` prints no secret.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientCredentials {
    /// The client id the provider issued.
    pub client_id: String,
    /// The client secret the provider issued.
    pub client_secret: String,
    /// The callback URL, exactly as registered with the provider.
    pub redirect_uri: String,
}

impl ClientCredentials {
    /// The three values every registration has.
    pub fn new(
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
        redirect_uri: impl Into<String>,
    ) -> Self {
        Self {
            client_id: client_id.into(),
            client_secret: client_secret.into(),
            redirect_uri: redirect_uri.into(),
        }
    }
}

impl std::fmt::Debug for ClientCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientCredentials")
            .field("client_id", &self.client_id)
            .field("client_secret", &"[redacted]")
            .field("redirect_uri", &self.redirect_uri)
            .finish()
    }
}

/// How the client authenticates to the token endpoint (RFC 6749 §2.3.1).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ClientAuth {
    /// `client_id` and `client_secret` in the form body. What most providers
    /// accept, and the default.
    #[default]
    Post,
    /// HTTP Basic, the form-urlencoded id and secret as user and password —
    /// what the RFC says every server must support, and some require.
    Basic,
}

/// What [`redirect`](Provider::redirect) leaves in the session for the
/// callback.
#[derive(Debug, Serialize, Deserialize)]
struct Pending {
    state: String,
    verifier: String,
    nonce: Option<String>,
}

/// The authorization-code flow, configured for one provider.
///
/// Every built-in is one of these with a [`ProfileSource`]; build one directly
/// for a provider that is not built in.
pub struct OAuth2Provider {
    name: String,
    credentials: ClientCredentials,
    scopes: Vec<String>,
    scope_separator: String,
    parameters: Vec<(String, String)>,
    client_auth: ClientAuth,
    source: Arc<dyn ProfileSource>,
    api: Api,
}

impl OAuth2Provider {
    /// A provider named `name`, finding people through `source`.
    pub fn new(
        name: impl Into<String>,
        credentials: ClientCredentials,
        source: impl ProfileSource,
        api: Api,
    ) -> Self {
        Self {
            name: name.into(),
            credentials,
            scopes: Vec::new(),
            scope_separator: " ".into(),
            parameters: Vec::new(),
            client_auth: ClientAuth::Post,
            source: Arc::new(source),
            api,
        }
    }

    /// Register it under another name — a second GitHub app, or an `oidc`
    /// provider known as `work`. The name also namespaces its session key, so
    /// two providers mid-flow in one browser do not clobber each other.
    #[must_use]
    pub fn renamed(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// Replace the requested scopes.
    #[must_use]
    pub fn scopes<S: Into<String>>(mut self, scopes: impl IntoIterator<Item = S>) -> Self {
        self.scopes = scopes.into_iter().map(Into::into).collect();
        self
    }

    /// Add to the requested scopes.
    #[must_use]
    pub fn with_scopes<S: Into<String>>(mut self, scopes: impl IntoIterator<Item = S>) -> Self {
        for scope in scopes {
            let scope = scope.into();
            if !self.scopes.contains(&scope) {
                self.scopes.push(scope);
            }
        }
        self
    }

    /// How scopes are joined in the authorize URL. A space, as RFC 6749 says,
    /// unless the provider says otherwise.
    #[must_use]
    pub fn scope_separator(mut self, separator: impl Into<String>) -> Self {
        self.scope_separator = separator.into();
        self
    }

    /// An extra parameter on the authorize URL — `prompt=select_account`,
    /// `hd=example.com`, `access_type=offline`.
    ///
    /// The flow's own parameters (`state`, `code_challenge`, `nonce`, …)
    /// cannot be overridden this way; they are written after these.
    #[must_use]
    pub fn with_parameter(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.parameters.push((key.into(), value.into()));
        self
    }

    /// How the client authenticates to the token endpoint.
    #[must_use]
    pub fn client_auth(mut self, auth: ClientAuth) -> Self {
        self.client_auth = auth;
        self
    }

    /// The registration this provider was built with.
    pub fn credentials(&self) -> &ClientCredentials {
        &self.credentials
    }

    /// The scopes requested.
    pub fn requested_scopes(&self) -> &[String] {
        &self.scopes
    }

    fn session_key(&self) -> String {
        format!("_sso.{}", self.name)
    }

    /// Generate and remember the round trip's secrets, and build the URL the
    /// browser is sent to. [`redirect`](Provider::redirect) is this and a
    /// `302`; call it directly to hand the URL to a front end instead.
    pub async fn authorization_url(&self, request: &Request) -> Result<String> {
        let session = request.session().ok_or_else(|| {
            Error::internal(format!(
                "signing in with {} needs the session middleware on this route: the state \
                 that defends the callback has nowhere else to live",
                self.name
            ))
        })?;

        let endpoints = self.source.endpoints(&self.api).await?;
        let pending = Pending {
            state: random_token(),
            verifier: random_token(),
            nonce: self.source.uses_nonce().then(random_token),
        };

        let mut query = form_urlencoded::Serializer::new(String::new());
        for (key, value) in &self.parameters {
            query.append_pair(key, value);
        }
        query
            .append_pair("client_id", &self.credentials.client_id)
            .append_pair("redirect_uri", &self.credentials.redirect_uri)
            .append_pair("response_type", "code")
            .append_pair("state", &pending.state)
            .append_pair("code_challenge", &code_challenge(&pending.verifier))
            .append_pair("code_challenge_method", "S256");
        if !self.scopes.is_empty() {
            query.append_pair("scope", &self.scopes.join(&self.scope_separator));
        }
        if let Some(nonce) = &pending.nonce {
            query.append_pair("nonce", nonce);
        }

        session.put(self.session_key(), &pending)?;

        let joiner = if endpoints.authorize.contains('?') { '&' } else { '?' };
        Ok(format!("{}{joiner}{}", endpoints.authorize, query.finish()))
    }

    /// POST a grant to the token endpoint and read the answer.
    async fn grant(&self, form: &mut Vec<(&str, String)>) -> Result<AccessToken> {
        let endpoints = self.source.endpoints(&self.api).await?;

        let mut headers = Vec::new();
        match self.client_auth {
            ClientAuth::Post => {
                form.push(("client_id", self.credentials.client_id.clone()));
                form.push(("client_secret", self.credentials.client_secret.clone()));
            }
            ClientAuth::Basic => {
                let encode =
                    |s: &str| form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>();
                let pair = format!(
                    "{}:{}",
                    encode(&self.credentials.client_id),
                    encode(&self.credentials.client_secret)
                );
                headers.push(("authorization", format!("Basic {}", B64.encode(pair))));
            }
        }

        let pairs: Vec<(&str, &str)> = form.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let response = self.api.post_form(&endpoints.token, &pairs, &headers).await?;
        let body = crate::api::json_of(&self.name, &endpoints.token, response)?;
        self.read_token(&body)
    }

    /// A token response, or the error it carries — GitHub answers a bad code
    /// with a `200` and an `error` field, so the status alone is not enough.
    fn read_token(&self, body: &Value) -> Result<AccessToken> {
        if let Some(error) = text(body, "error") {
            let description = text(body, "error_description").unwrap_or_default();
            return Err(Error::unauthenticated(format!(
                "{} refused the token request: {error} {description}",
                self.name
            )));
        }

        let access_token = text(body, "access_token").ok_or_else(|| {
            Error::unauthenticated(format!("{} issued no access token", self.name))
        })?;

        let expires_in = match body.get("expires_in") {
            Some(Value::Number(n)) => n.as_i64(),
            Some(Value::String(s)) => s.parse().ok(),
            _ => None,
        };

        // Granted scopes, which can be fewer than requested. Comma-separated
        // from GitHub, space-separated from everybody else; absent means
        // "what you asked for", per RFC 6749 §5.1.
        let scopes = match text(body, "scope") {
            Some(granted) => granted
                .split([' ', ','])
                .filter(|scope| !scope.is_empty())
                .map(str::to_string)
                .collect(),
            None => self.scopes.clone(),
        };

        Ok(AccessToken {
            access_token,
            refresh_token: text(body, "refresh_token"),
            expires_at: expires_in.map(|secs| Utc::now() + chrono::Duration::seconds(secs)),
            scopes,
            id_token: text(body, "id_token"),
        })
    }
}

impl std::fmt::Debug for OAuth2Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuth2Provider")
            .field("name", &self.name)
            .field("credentials", &self.credentials)
            .field("scopes", &self.scopes)
            .field("client_auth", &self.client_auth)
            .finish()
    }
}

/// A string query parameter of the callback.
fn param(request: &Request, key: &str) -> Option<String> {
    request.query().get(key).and_then(Value::as_str).map(str::to_string)
}

#[async_trait]
impl Provider for OAuth2Provider {
    fn name(&self) -> &str {
        &self.name
    }

    async fn redirect(&self, request: &Request) -> Result<Response> {
        Ok(Redirect::to(self.authorization_url(request).await?).into_response())
    }

    async fn user(&self, request: &Request) -> Result<SocialUser> {
        let session = request.session().ok_or_else(|| {
            Error::internal(format!(
                "the {} callback needs the session middleware on its route",
                self.name
            ))
        })?;

        // Pulled before anything is checked, so every outcome — success, a
        // wrong state, a provider error — consumes it. A callback URL works
        // once.
        let pending: Option<Pending> = session.pull(&self.session_key());

        if let Some(error) = param(request, "error") {
            let description = param(request, "error_description").unwrap_or_default();
            return Err(Error::unauthenticated(format!(
                "{} did not sign you in: {error} {description}",
                self.name
            )));
        }

        let pending = pending.ok_or_else(|| {
            Error::unauthenticated(format!(
                "no {} sign-in is in progress for this session — it expired, was already \
                 completed, or was started in another browser",
                self.name
            ))
        })?;

        let returned = param(request, "state").unwrap_or_default();
        if !same(&returned, &pending.state) {
            return Err(Error::unauthenticated(format!(
                "the {} callback's state does not match this session's",
                self.name
            )));
        }

        let code = param(request, "code").ok_or_else(|| {
            Error::bad_request(format!("the {} callback carried no code", self.name))
        })?;

        let token = self
            .grant(&mut vec![
                ("grant_type", "authorization_code".into()),
                ("code", code),
                ("redirect_uri", self.credentials.redirect_uri.clone()),
                ("code_verifier", pending.verifier),
            ])
            .await?;

        let profile = self.source.profile(&self.api, &token, pending.nonce.as_deref()).await?;
        Ok(profile.into_user(&self.name, token))
    }

    async fn user_from_token(&self, token: AccessToken) -> Result<SocialUser> {
        // Any ID token on a token handed in from outside is not one this
        // server asked for — no nonce of ours is in it — so it is set aside
        // rather than half-trusted.
        let token = AccessToken { id_token: None, ..token };
        let profile = self.source.profile(&self.api, &token, None).await?;
        Ok(profile.into_user(&self.name, token))
    }

    async fn refresh(&self, refresh_token: &str) -> Result<AccessToken> {
        let mut token = self
            .grant(&mut vec![
                ("grant_type", "refresh_token".into()),
                ("refresh_token", refresh_token.to_string()),
            ])
            .await?;

        // Most providers do not rotate refresh tokens; the old one still
        // works, and dropping it would strand the account at the next expiry.
        if token.refresh_token.is_none() {
            token.refresh_token = Some(refresh_token.to_string());
        }
        Ok(token)
    }
}
