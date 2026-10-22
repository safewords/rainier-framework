//! Where the server keeps its records — the [`OAuthStore`] port, and
//! [`MemoryStore`].
//!
//! A port because the schema is not always this crate's to choose: an
//! application taking over from another OAuth server keeps that server's
//! tables, and its tokens stay valid only if the rows they point at are read
//! where they already are. `rainier-compat` ships such a store; an
//! application starting fresh writes one over its own tables, or uses the
//! memory one in tests.
//!
//! # Revoking is claiming
//!
//! [`revoke_auth_code`](OAuthStore::revoke_auth_code) and
//! [`revoke_refresh_token`](OAuthStore::revoke_refresh_token) answer whether
//! **this call** revoked the row. The server spends a code or a refresh token
//! by revoking it *first* and issuing only when that answer is `true`, so two
//! requests racing with the same code get one token between them, not two. An
//! implementation over SQL does this with `UPDATE … SET revoked = 1 WHERE id =
//! ? AND revoked = 0` and the affected-row count.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rainier_support::Result;

use crate::model::{AccessTokenRecord, AuthCodeRecord, Client, RefreshTokenRecord};

/// How many rows a purge removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Purged {
    /// Access tokens.
    pub access_tokens: u64,
    /// Refresh tokens.
    pub refresh_tokens: u64,
    /// Authorization codes.
    pub auth_codes: u64,
}

/// What a purge removes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PurgeOptions {
    /// Remove revoked rows.
    pub revoked: bool,
    /// Remove rows that expired before this instant.
    pub expired_before: Option<DateTime<Utc>>,
}

/// The persistence an [`AuthorizationServer`](crate::AuthorizationServer)
/// and an [`OAuthGuard`](crate::OAuthGuard) need.
#[async_trait]
pub trait OAuthStore: Send + Sync + 'static {
    /// The client with this id, revoked or not.
    async fn find_client(&self, id: &str) -> Result<Option<Client>>;

    /// The client personal access tokens are issued through, for users of
    /// `provider` (or the default provider, for `None`).
    async fn personal_access_client(&self, provider: Option<&str>) -> Result<Option<Client>>;

    /// Store a newly issued access token.
    async fn create_access_token(&self, token: &AccessTokenRecord) -> Result<()>;

    /// An access token by id.
    async fn find_access_token(&self, id: &str) -> Result<Option<AccessTokenRecord>>;

    /// Revoke an access token. Whether it was live before this call.
    async fn revoke_access_token(&self, id: &str) -> Result<bool>;

    /// Store a newly issued refresh token.
    async fn create_refresh_token(&self, token: &RefreshTokenRecord) -> Result<()>;

    /// A refresh token by id.
    async fn find_refresh_token(&self, id: &str) -> Result<Option<RefreshTokenRecord>>;

    /// Revoke a refresh token. Whether **this call** revoked it — see the
    /// module documentation.
    async fn revoke_refresh_token(&self, id: &str) -> Result<bool>;

    /// Revoke every refresh token issued alongside an access token.
    async fn revoke_refresh_tokens_for(&self, access_token_id: &str) -> Result<u64>;

    /// Store a newly issued authorization code.
    async fn create_auth_code(&self, code: &AuthCodeRecord) -> Result<()>;

    /// Revoke an authorization code. Whether **this call** revoked it.
    async fn revoke_auth_code(&self, id: &str) -> Result<bool>;

    /// Whether an authorization code is revoked — or unknown, which is the
    /// same answer.
    async fn is_auth_code_revoked(&self, id: &str) -> Result<bool>;

    /// The live tokens `user_id` holds for `client_id`, newest first — for
    /// skipping the consent screen when the user already approved these
    /// scopes, and for a "connected apps" page.
    async fn tokens_for(
        &self,
        user_id: &str,
        client_id: Option<&str>,
    ) -> Result<Vec<AccessTokenRecord>>;

    /// Remove dead rows.
    async fn purge(&self, options: PurgeOptions) -> Result<Purged>;
}

/// An [`OAuthStore`] in a mutex — for tests, and for nothing that restarts.
#[derive(Default)]
pub struct MemoryStore {
    clients: Mutex<HashMap<String, Client>>,
    access: Mutex<HashMap<String, AccessTokenRecord>>,
    refresh: Mutex<HashMap<String, RefreshTokenRecord>>,
    codes: Mutex<HashMap<String, AuthCodeRecord>>,
    personal: Mutex<Option<String>>,
}

impl MemoryStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a client.
    pub fn with_client(self, client: Client) -> Self {
        self.insert_client(client);
        self
    }

    /// Register a client after construction.
    pub fn insert_client(&self, client: Client) {
        self.clients.lock().expect("store lock").insert(client.id.clone(), client);
    }

    /// Name the client personal access tokens go through.
    pub fn with_personal_access_client(self, id: impl Into<String>) -> Self {
        *self.personal.lock().expect("store lock") = Some(id.into());
        self
    }

    /// Every access token, for assertions.
    pub fn access_tokens(&self) -> Vec<AccessTokenRecord> {
        self.access.lock().expect("store lock").values().cloned().collect()
    }

    /// Every refresh token, for assertions.
    pub fn refresh_tokens(&self) -> Vec<RefreshTokenRecord> {
        self.refresh.lock().expect("store lock").values().cloned().collect()
    }

    /// Every authorization code, for assertions.
    pub fn auth_codes(&self) -> Vec<AuthCodeRecord> {
        self.codes.lock().expect("store lock").values().cloned().collect()
    }
}

#[async_trait]
impl OAuthStore for MemoryStore {
    async fn find_client(&self, id: &str) -> Result<Option<Client>> {
        Ok(self.clients.lock().expect("store lock").get(id).cloned())
    }

    async fn personal_access_client(&self, _provider: Option<&str>) -> Result<Option<Client>> {
        let id = self.personal.lock().expect("store lock").clone();
        Ok(id.and_then(|id| self.clients.lock().expect("store lock").get(&id).cloned()))
    }

    async fn create_access_token(&self, token: &AccessTokenRecord) -> Result<()> {
        self.access.lock().expect("store lock").insert(token.id.clone(), token.clone());
        Ok(())
    }

    async fn find_access_token(&self, id: &str) -> Result<Option<AccessTokenRecord>> {
        Ok(self.access.lock().expect("store lock").get(id).cloned())
    }

    async fn revoke_access_token(&self, id: &str) -> Result<bool> {
        Ok(claim(self.access.lock().expect("store lock").get_mut(id).map(|t| &mut t.revoked)))
    }

    async fn create_refresh_token(&self, token: &RefreshTokenRecord) -> Result<()> {
        self.refresh.lock().expect("store lock").insert(token.id.clone(), token.clone());
        Ok(())
    }

    async fn find_refresh_token(&self, id: &str) -> Result<Option<RefreshTokenRecord>> {
        Ok(self.refresh.lock().expect("store lock").get(id).cloned())
    }

    async fn revoke_refresh_token(&self, id: &str) -> Result<bool> {
        Ok(claim(self.refresh.lock().expect("store lock").get_mut(id).map(|t| &mut t.revoked)))
    }

    async fn revoke_refresh_tokens_for(&self, access_token_id: &str) -> Result<u64> {
        let mut count = 0;
        for token in self.refresh.lock().expect("store lock").values_mut() {
            if token.access_token_id == access_token_id && !token.revoked {
                token.revoked = true;
                count += 1;
            }
        }
        Ok(count)
    }

    async fn create_auth_code(&self, code: &AuthCodeRecord) -> Result<()> {
        self.codes.lock().expect("store lock").insert(code.id.clone(), code.clone());
        Ok(())
    }

    async fn revoke_auth_code(&self, id: &str) -> Result<bool> {
        Ok(claim(self.codes.lock().expect("store lock").get_mut(id).map(|c| &mut c.revoked)))
    }

    async fn is_auth_code_revoked(&self, id: &str) -> Result<bool> {
        Ok(self.codes.lock().expect("store lock").get(id).is_none_or(|c| c.revoked))
    }

    async fn tokens_for(
        &self,
        user_id: &str,
        client_id: Option<&str>,
    ) -> Result<Vec<AccessTokenRecord>> {
        let now = Utc::now();
        let mut tokens: Vec<AccessTokenRecord> = self
            .access
            .lock()
            .expect("store lock")
            .values()
            .filter(|t| t.user_id.as_deref() == Some(user_id))
            .filter(|t| client_id.is_none_or(|c| t.client_id == c))
            .filter(|t| !t.revoked && t.expires_at > now)
            .cloned()
            .collect();
        tokens.sort_by_key(|token| std::cmp::Reverse(token.created_at));
        Ok(tokens)
    }

    async fn purge(&self, options: PurgeOptions) -> Result<Purged> {
        let dead = |revoked: bool, expires_at: DateTime<Utc>| {
            (options.revoked && revoked) || options.expired_before.is_some_and(|t| expires_at < t)
        };
        let mut purged = Purged::default();
        self.access.lock().expect("store lock").retain(|_, t| {
            let gone = dead(t.revoked, t.expires_at);
            purged.access_tokens += u64::from(gone);
            !gone
        });
        self.refresh.lock().expect("store lock").retain(|_, t| {
            let gone = dead(t.revoked, t.expires_at);
            purged.refresh_tokens += u64::from(gone);
            !gone
        });
        self.codes.lock().expect("store lock").retain(|_, c| {
            let gone = dead(c.revoked, c.expires_at);
            purged.auth_codes += u64::from(gone);
            !gone
        });
        Ok(purged)
    }
}

/// Set a revoked flag, answering whether this call was the one that set it.
fn claim(flag: Option<&mut bool>) -> bool {
    match flag {
        Some(revoked) if !*revoked => {
            *revoked = true;
            true
        }
        _ => false,
    }
}

impl std::fmt::Debug for MemoryStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryStore").finish_non_exhaustive()
    }
}
