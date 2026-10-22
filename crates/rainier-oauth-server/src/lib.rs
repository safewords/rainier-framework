//! # rainier-oauth-server
//!
//! An OAuth 2.0 authorization server: the token endpoint, the two halves of
//! the authorization endpoint, personal access tokens, and the guard that
//! accepts what it issues.
//!
//! | | |
//! |---|---|
//! | [`AuthorizationServer`] | the grants — authorization code with PKCE, refresh with rotation, client credentials, password — and personal access tokens |
//! | [`OAuthGuard`] | a `rainier-auth` guard for bearer access tokens; scopes become abilities |
//! | [`OAuthStore`] / [`MemoryStore`] | where clients, tokens and codes live |
//! | [`TokenSealer`] / [`EncrypterSealer`] | how refresh tokens and codes are sealed |
//! | [`AccessTokenSigner`] / [`AccessTokenVerifier`] | RS256 access tokens |
//! | [`Scopes`] | which scopes exist, and namespace inheritance |
//! | [`AuthorizationSession`] | the pending authorization, across the consent screen |
//!
//! ```ignore
//! let server = Arc::new(
//!     AuthorizationServer::new(store.clone(), sealer, AccessTokenSigner::from_pem(&private_pem)?, hasher)
//!         .scopes(Scopes::new().with("account", "Manage your account"))
//!         .password_grant(Arc::new(UserProviderOwners::new(users.clone()))),
//! );
//!
//! router.post("/oauth/token", {
//!     let server = server.clone();
//!     move |request: Req| { let server = server.clone(); async move { server.respond_to_token_request(&request).await } }
//! });
//!
//! let guard = OAuthGuard::new("api", server.verifier(), store, users);
//! ```
//!
//! ## What is the application's
//!
//! The consent screen, the login that precedes it, and client registration.
//! The server validates an authorization request and finishes the one the
//! application decides on; see [`server`].
//!
//! ## What is not here
//!
//! The implicit grant (removed by RFC 9700, and never safe), the device
//! authorization grant, token introspection (RFC 7662), dynamic client
//! registration, and OpenID Connect — an `id_token` is a different promise to
//! a different party, and `identity` is where this organisation makes it.
//!
//! Taking over from an existing PHP server, with its tables and tokens left
//! where they are, is `rainier-compat`'s `php-oauth` feature.

#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

pub mod codec;
pub mod error;
pub mod guard;
pub mod jwt;
pub mod model;
pub mod owners;
pub mod pkce;
pub mod scope;
pub mod server;
pub mod session;
pub mod store;

#[cfg(test)]
mod tests;

pub use codec::{AuthCodePayload, EncrypterSealer, RefreshTokenPayload, TokenSealer};
pub use error::OAuthError;
pub use guard::{AuthenticatedToken, OAuthGuard};
pub use jwt::{AccessTokenClaims, AccessTokenSigner, AccessTokenVerifier, VerifiedToken};
pub use model::{grant, AccessTokenRecord, AuthCodeRecord, Client, RefreshTokenRecord};
pub use owners::{ResourceOwners, UserProviderOwners};
pub use scope::Scopes;
pub use server::{AuthorizationRequest, AuthorizationServer, Lifetimes, PersonalAccessToken};
pub use session::AuthorizationSession;
pub use store::{MemoryStore, OAuthStore, PurgeOptions, Purged};
