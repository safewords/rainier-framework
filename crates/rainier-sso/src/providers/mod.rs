//! The built-in providers.
//!
//! | constructor | source | identity from |
//! |---|---|---|
//! | [`github`] | [`GitHub`] | `GET /user`, `GET /user/emails` |
//! | [`gitlab`] | [`GitLab`] | `GET /api/v4/user` |
//! | [`discord`] | [`Discord`] | `GET /users/@me` |
//! | [`x`] | [`X`] | `GET /2/users/me` |
//! | [`google`] | [`Oidc`] | the verified ID token |
//! | [`microsoft`] | [`Oidc`] | the verified ID token |
//! | [`oidc`] | [`Oidc`] | the verified ID token — Okta, Auth0, Keycloak, Authentik, safewords identity, anything with a discovery document |
//!
//! # Which emails count as verified
//!
//! [`SocialUser::email_verified`](crate::SocialUser::email_verified) is only
//! `Some(true)` when the provider asserts it:
//!
//! - **GitHub** — the address `/user/emails` marks `primary` and `verified`.
//!   The public email on `/user` is whatever the person typed into their
//!   profile, and is never treated as verified.
//! - **GitLab** — the account's email once the account is confirmed.
//! - **Discord** — its `verified` flag.
//! - **X** — `confirmed_email`, present only with the `users.email` scope,
//!   and only ever a confirmed address.
//! - **Google** and generic OIDC — the `email_verified` claim.
//! - **Microsoft** — never. Entra ID's `email` claim is an attribute a tenant
//!   administrator can set to anything, and it emits no `email_verified`;
//!   linking accounts by it is the "nOAuth" takeover. A Microsoft sign-in
//!   links by its `sub` (and `tid`), or not at all.

use rainier_support::Result;

use crate::api::Api;
use crate::provider::{ClientAuth, ClientCredentials, OAuth2Provider};

mod discord;
mod github;
mod gitlab;
mod oidc;
mod x;

pub use discord::Discord;
pub use github::GitHub;
pub use gitlab::GitLab;
pub use oidc::Oidc;
pub use x::X;

/// Sign in with GitHub — `read:user` and `user:email`.
pub fn github(credentials: ClientCredentials, api: Api) -> OAuth2Provider {
    OAuth2Provider::new("github", credentials, GitHub::new(), api)
        .scopes(["read:user", "user:email"])
}

/// Sign in with GitLab.com — `read_user`.
pub fn gitlab(credentials: ClientCredentials, api: Api) -> OAuth2Provider {
    OAuth2Provider::new("gitlab", credentials, GitLab::new(), api).scopes(["read_user"])
}

/// Sign in with Discord — `identify` and `email`.
pub fn discord(credentials: ClientCredentials, api: Api) -> OAuth2Provider {
    OAuth2Provider::new("discord", credentials, Discord::new(), api).scopes(["identify", "email"])
}

/// Sign in with X — `tweet.read`, `users.read` and `offline.access`.
///
/// X will not answer `users/me` to a token without `tweet.read`, and issues
/// no refresh token without `offline.access`. Add `users.email` to the scopes
/// once the app has email access, to receive a confirmed address. X requires
/// HTTP Basic client authentication for a confidential client.
pub fn x(credentials: ClientCredentials, api: Api) -> OAuth2Provider {
    OAuth2Provider::new("x", credentials, X::new(), api)
        .scopes(["tweet.read", "users.read", "offline.access"])
        .client_auth(ClientAuth::Basic)
}

/// Sign in with Google — OpenID Connect against `https://accounts.google.com`.
pub fn google(credentials: ClientCredentials, api: Api) -> OAuth2Provider {
    let source = Oidc::new("https://accounts.google.com", &credentials.client_id)
        // Google's ID tokens carry either form of its issuer.
        .also_accept_issuer("accounts.google.com");
    OAuth2Provider::new("google", credentials, source, api).scopes(["openid", "email", "profile"])
}

/// Sign in with Microsoft Entra ID.
///
/// `tenant` is a tenant id or domain for one organisation's accounts, or
/// `common`, `organizations` or `consumers`. With the multi-tenant values,
/// **any** tenant's users can sign in — the ID token's `tid` (in
/// [`raw`](crate::SocialUser::raw)) says which, and checking it is the
/// application's job.
pub fn microsoft(tenant: &str, credentials: ClientCredentials, api: Api) -> OAuth2Provider {
    let source = Oidc::new(
        format!("https://login.microsoftonline.com/{tenant}/v2.0"),
        &credentials.client_id,
    )
    .multi_tenant(matches!(tenant, "common" | "organizations" | "consumers"))
    .ignore_email_verified();
    OAuth2Provider::new("microsoft", credentials, source, api)
        .scopes(["openid", "email", "profile"])
}

/// Sign in with any OpenID Connect issuer, by discovery.
pub fn oidc(
    name: impl Into<String>,
    issuer: impl Into<String>,
    credentials: ClientCredentials,
    api: Api,
) -> OAuth2Provider {
    let source = Oidc::new(issuer, &credentials.client_id);
    OAuth2Provider::new(name, credentials, source, api).scopes(["openid", "email", "profile"])
}

/// Read an endpoint that answers 2xx with JSON, or say why not.
pub(crate) async fn fetch(
    api: &Api,
    service: &str,
    url: &str,
    token: &str,
) -> Result<serde_json::Value> {
    api.get_json(service, url, Some(token)).await
}
