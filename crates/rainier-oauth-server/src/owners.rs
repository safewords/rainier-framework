//! Who may use the password grant — [`ResourceOwners`].
//!
//! The password grant hands a username and password to the server, which has
//! to check them against the application's users. A port, so the server is
//! not generic over the user model; [`UserProviderOwners`] adapts the
//! `rainier-auth` user provider an application already has.

use std::marker::PhantomData;
use std::sync::Arc;

use async_trait::async_trait;
use rainier_auth::{Authenticatable, Credentials, UserProvider};
use rainier_support::Result;

use crate::model::Client;

/// Checks resource-owner credentials.
#[async_trait]
pub trait ResourceOwners: Send + Sync + 'static {
    /// The id of the user these credentials belong to, or `None` when they
    /// are wrong. `client` is the client asking, for an application that
    /// limits the grant per client.
    async fn verify(
        &self,
        username: &str,
        password: &str,
        client: &Client,
    ) -> Result<Option<String>>;
}

/// [`ResourceOwners`] over a `rainier-auth` [`UserProvider`], looking users up
/// by `column` (the provider's username column — `email` by default).
pub struct UserProviderOwners<U: Authenticatable> {
    provider: Arc<dyn UserProvider<U>>,
    column: String,
    _user: PhantomData<fn() -> U>,
}

impl<U: Authenticatable> UserProviderOwners<U> {
    /// Check credentials through `provider`, by `U`'s username column.
    pub fn new(provider: Arc<dyn UserProvider<U>>) -> Self {
        Self { provider, column: U::auth_username_column().to_string(), _user: PhantomData }
    }

    /// Look users up by a different column.
    pub fn by(mut self, column: impl Into<String>) -> Self {
        self.column = column.into();
        self
    }
}

#[async_trait]
impl<U: Authenticatable> ResourceOwners for UserProviderOwners<U> {
    async fn verify(
        &self,
        username: &str,
        password: &str,
        _client: &Client,
    ) -> Result<Option<String>> {
        let credentials =
            Credentials::new().with(&self.column, username).with("password", password);
        let Some(user) = self.provider.retrieve_by_credentials(&credentials).await? else {
            return Ok(None);
        };
        if !self.provider.validate_credentials(&user, &credentials).await? {
            return Ok(None);
        }
        Ok(Some(user.auth_identifier()))
    }
}

impl<U: Authenticatable> std::fmt::Debug for UserProviderOwners<U> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UserProviderOwners").field("column", &self.column).finish()
    }
}
