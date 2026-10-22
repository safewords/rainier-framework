//! Accepting the tokens on the API side — [`OAuthGuard`].
//!
//! A `rainier-auth` [`Guard`], so it sits in an [`AuthManager`] beside the
//! session guard and routes say `auth:api` as they would for any other. A
//! request is authenticated when its bearer token:
//!
//! 1. verifies — signature and lifetime, see [`AccessTokenVerifier`];
//! 2. has a row in the store that is not revoked;
//! 3. belongs to a client that is not revoked (and, when the guard is tied to
//!    a user provider, a client of that provider);
//! 4. names a user the provider can still find.
//!
//! Step 2 is a database read per request. That is the price of a token that
//! can be revoked before it expires, and it is the behaviour every client of
//! the server this replaces already relies on.
//!
//! The token's scopes become the request's [`Abilities`], so
//! `RequireAbility` is the scope middleware — see [`crate::scope`] for how
//! namespaces carry over.
//!
//! [`AuthManager`]: rainier_auth::AuthManager

use std::sync::Arc;

use rainier_auth::{Abilities, Authenticatable, Guard, UserProvider};
use rainier_http::Request;
use rainier_support::{BoxFuture, Result};

use crate::jwt::{AccessTokenVerifier, VerifiedToken};
use crate::scope::to_abilities;
use crate::store::OAuthStore;

/// A token that passed every check, and the record behind it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthenticatedToken {
    /// What the JWT said.
    pub token: VerifiedToken,
    /// The user it was issued to, or `None` for a client-credentials token.
    pub user_id: Option<String>,
}

impl AuthenticatedToken {
    /// Whether the token carries `scope` (by namespace, or `*`).
    pub fn can(&self, scope: &str) -> bool {
        crate::scope::held_grants(&self.token.scopes, scope)
    }
}

/// Authenticates bearer access tokens.
pub struct OAuthGuard<U: Authenticatable> {
    name: String,
    verifier: AccessTokenVerifier,
    store: Arc<dyn OAuthStore>,
    users: Arc<dyn UserProvider<U>>,
    provider: Option<String>,
}

impl<U: Authenticatable> OAuthGuard<U> {
    /// A guard named `name`, verifying with `verifier`, checking revocation in
    /// `store`, and finding users through `users`.
    pub fn new(
        name: impl Into<String>,
        verifier: AccessTokenVerifier,
        store: Arc<dyn OAuthStore>,
        users: Arc<dyn UserProvider<U>>,
    ) -> Self {
        Self { name: name.into(), verifier, store, users, provider: None }
    }

    /// Only accept tokens from clients of this user provider (or of no
    /// particular one) — for an application with two user tables, so a
    /// customer's token cannot be presented as a staff member with the same id.
    pub fn for_provider(mut self, provider: impl Into<String>) -> Self {
        self.provider = Some(provider.into());
        self
    }

    /// Check the request's bearer token without resolving a user — what a
    /// client-credentials route wants, where there is no user to find.
    pub async fn token(&self, request: &Request) -> Result<Option<AuthenticatedToken>> {
        let Some(bearer) = request.bearer_token() else { return Ok(None) };
        let Ok(token) = self.verifier.verify(bearer) else { return Ok(None) };

        let Some(record) = self.store.find_access_token(&token.id).await? else {
            return Ok(None);
        };
        if record.revoked || record.client_id != token.client_id {
            return Ok(None);
        }

        let Some(client) = self.store.find_client(&token.client_id).await? else {
            return Ok(None);
        };
        if client.revoked {
            return Ok(None);
        }
        if let (Some(wanted), Some(theirs)) = (&self.provider, &client.provider) {
            if wanted != theirs {
                return Ok(None);
            }
        }

        Ok(Some(AuthenticatedToken { token, user_id: record.user_id }))
    }
}

impl<U: Authenticatable> Guard<U> for OAuthGuard<U> {
    fn name(&self) -> &str {
        &self.name
    }

    fn user<'a>(&'a self, request: &'a Request) -> BoxFuture<'a, Result<Option<U>>> {
        Box::pin(async move {
            let Some(authenticated) = self.token(request).await? else { return Ok(None) };
            let Some(user_id) = authenticated.user_id else { return Ok(None) };
            self.users.retrieve_by_id(&user_id).await
        })
    }

    fn abilities<'a>(&'a self, request: &'a Request) -> BoxFuture<'a, Result<Abilities>> {
        Box::pin(async move {
            Ok(match self.token(request).await? {
                Some(authenticated) => to_abilities(&authenticated.token.scopes),
                None => Abilities::none(),
            })
        })
    }
}

impl<U: Authenticatable> std::fmt::Debug for OAuthGuard<U> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthGuard")
            .field("name", &self.name)
            .field("provider", &self.provider)
            .finish_non_exhaustive()
    }
}
