//! # rainier-sso
//!
//! "Sign in with …" — OAuth 2.0 and OpenID Connect providers: `redirect`, then
//! `user`, with the safe defaults on.
//!
//! | | |
//! |---|---|
//! | [`Provider`] | one provider's round trip: [`redirect`](Provider::redirect), then [`user`](Provider::user) |
//! | [`OAuth2Provider`] | the authorization-code flow every built-in is a configuration of |
//! | [`providers`] | GitHub, GitLab, Discord, X, Google, Microsoft, and any OIDC issuer by discovery |
//! | [`SocialUser`] | who signed in, the same shape from every provider |
//! | [`Sso`] / [`SsoConfig`] | providers by name, declared in configuration |
//! | [`SocialAccounts`] / [`login_with`] | find-or-create, link, and log in on a guard |
//!
//! ```ignore
//! let sso = Sso::from_config(&config.get("services.sso")?, http)?;
//!
//! router.get("/auth/{provider}/redirect", move |request: Req| {
//!     let sso = sso.clone();
//!     async move { sso.driver(request.route_param("provider").unwrap_or(""))?.redirect(&request).await }
//! });
//!
//! router.get("/auth/{provider}/callback", move |request: Req| { /* … */
//!     let social = sso.driver(provider)?.user(&request).await?;
//!     let (user, _) = login_with(&*guard, &*accounts, &request, &social).await?;
//!     Ok(Redirect::to("/dashboard"))
//! });
//! ```
//!
//! ## Safer than the usual defaults
//!
//! - **PKCE on every provider**, `S256` only — the code is useless without a
//!   verifier that never leaves the server.
//! - **`state` compared in constant time and consumed** on the first callback,
//!   success or failure. There is no `stateless()`; see [`provider`].
//! - **ID tokens verified** against the issuer's JWKS — signature, `iss`,
//!   `aud`, `azp`, `exp`, and the `nonce` this session generated. See
//!   [`providers::Oidc`].
//! - **No account linking by an unverified email.** See [`accounts`] for the
//!   takeover it prevents.
//! - **No secret in `Debug`.** Client secrets and tokens print as
//!   `[redacted]`.
//!
//! ## Every call goes through the framework's HTTP port
//!
//! A provider is built on an [`Api`] over a
//! [`Transport`](rainier_http_client::Transport), so a test hands it a
//! [`FakeTransport`](rainier_http_client::FakeTransport), queues the
//! provider's answers, and asserts on the exact token request — no network,
//! no mock server.
//!
//! ## What this is not
//!
//! Not SAML. SAML is XML signatures and their canonicalisation, a large and
//! sharp surface that deserves its own crate rather than a corner of this
//! one; most identity providers that speak SAML also speak OIDC, and this
//! crate covers them that way.

#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

pub mod accounts;
pub mod api;
pub mod manager;
pub mod provider;
pub mod providers;
pub mod secret;
pub mod user;

pub use accounts::{connect, login_with, LoginOutcome, SocialAccounts};
pub use api::Api;
pub use manager::{ProviderConfig, Sso, SsoConfig};
pub use provider::{
    ClientAuth, ClientCredentials, Endpoints, OAuth2Provider, ProfileSource, Provider,
};
pub use user::{AccessToken, Profile, SocialUser};

#[cfg(test)]
mod tests;
