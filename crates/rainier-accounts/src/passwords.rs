//! Password reset tokens — [`PasswordBroker`].
//!
//! ```ignore
//! let broker = PasswordBroker::new(cache);
//! let token = broker.issue(&user.auth_identifier()).await?;       // → the link
//! broker.consume(&user.auth_identifier(), &submitted).await?;     // ← the form
//! ```
//!
//! # What it guarantees
//!
//! - **Unguessable.** 256 bits from the OS random source. Unlike a six-digit
//!   [challenge](rainier_auth::Challenges), a reset token is clicked rather than
//!   typed, so it can be long enough that counting attempts is unnecessary.
//! - **Stored hashed.** The cache holds SHA-256 of the token, never the token.
//!   Whoever can read the cache — an operator, a debugging dump, a backup — can
//!   not reset anybody's password with what they read. SHA-256 rather than a
//!   password hash because the input is already 256 random bits: there is no
//!   dictionary to slow down.
//! - **One live token per account.** Issuing replaces; the previous link stops
//!   working. Two live links doubles what a leaked inbox exposes.
//! - **Single use.** A consumed token is forgotten before the caller acts on
//!   it.
//! - **Expiring**, by the store's own TTL — so, like
//!   [`Challenges`](rainier_auth::Challenges), there is no table to sweep.
//! - **Throttled.** A second link inside the throttle window is not issued.
//!   The caller still answers exactly as if it had been, which is the point:
//!   the throttle must not become a way to learn whether an address has an
//!   account.

use std::sync::Arc;
use std::time::Duration;

use rainier_cache::{Cache, CacheExt};
use rainier_support::{Error, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// What the store holds for one account.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Pending {
    /// SHA-256 of the token, hex.
    hash: String,
    /// When it was issued, seconds since the epoch — for the throttle.
    issued_at: i64,
}

/// Issues and consumes password reset tokens.
pub struct PasswordBroker {
    cache: Arc<dyn Cache>,
    prefix: String,
    ttl: Duration,
    throttle: Duration,
}

impl PasswordBroker {
    /// Tokens in `cache`, lasting an hour, at most one a minute per account.
    pub fn new(cache: Arc<dyn Cache>) -> Self {
        Self {
            cache,
            prefix: "password-reset:".into(),
            ttl: Duration::from_secs(3600),
            throttle: Duration::from_secs(60),
        }
    }

    /// How long a token works.
    #[must_use = "this returns a configured broker rather than configuring in place"]
    pub fn lasting(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    /// How soon another token may be issued for the same account.
    #[must_use = "this returns a configured broker rather than configuring in place"]
    pub fn throttled(mut self, throttle: Duration) -> Self {
        self.throttle = throttle;
        self
    }

    /// Whether a token issued here can be consumed on another replica.
    ///
    /// Worth asserting at boot: over a per-process cache, a reset link works
    /// only when the click lands on the replica that sent it.
    pub fn is_shared(&self) -> bool {
        self.cache.is_shared()
    }

    /// Issue a token for `subject`, or `None` when one was issued inside the
    /// throttle window.
    pub async fn issue(&self, subject: &str) -> Result<Option<String>> {
        let key = self.key(subject);

        if let Some(existing) = self.cache.get_json::<Pending>(&key).await? {
            if now().saturating_sub(existing.issued_at) < self.throttle.as_secs() as i64 {
                return Ok(None);
            }
        }

        let token = random_token();
        let pending = Pending { hash: digest(&token), issued_at: now() };
        self.cache.put_json(&key, &pending, Some(self.ttl)).await?;
        Ok(Some(token))
    }

    /// Whether `token` is the live one for `subject`, **without** consuming it
    /// — for a form that wants to say "this link has expired" before asking for
    /// a new password, rather than after.
    pub async fn exists(&self, subject: &str, token: &str) -> Result<bool> {
        Ok(match self.cache.get_json::<Pending>(&self.key(subject)).await? {
            Some(pending) => matches(&pending, token),
            None => false,
        })
    }

    /// Check `token` and consume it.
    ///
    /// # Errors
    ///
    /// One answer for "never issued", "expired", "already used" and "wrong":
    /// distinguishing them tells somebody holding a guessed token which of
    /// their guesses are aimed at a live account.
    pub async fn consume(&self, subject: &str, token: &str) -> Result<()> {
        let key = self.key(subject);
        let invalid = || {
            Error::validation(serde_json::json!({
                "email": ["This password reset link is invalid or has expired."],
            }))
        };

        let Some(pending) = self.cache.get_json::<Pending>(&key).await? else {
            return Err(invalid());
        };
        if !matches(&pending, token) {
            return Err(invalid());
        }

        // Before the caller changes the password, so a second submission of
        // the same form a moment later finds nothing.
        self.cache.forget(&key).await?;
        Ok(())
    }

    /// Forget any outstanding token for `subject` — after the password changes
    /// some other way, so a reset link in an old email does not undo it.
    pub async fn revoke(&self, subject: &str) -> Result<()> {
        self.cache.forget(&self.key(subject)).await?;
        Ok(())
    }

    fn key(&self, subject: &str) -> String {
        format!("{}{subject}", self.prefix)
    }
}

impl std::fmt::Debug for PasswordBroker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PasswordBroker")
            .field("store", &self.cache.name())
            .field("ttl", &self.ttl)
            .field("throttle", &self.throttle)
            .finish()
    }
}

/// Constant time, so the comparison does not leak the hash a byte at a time.
fn matches(pending: &Pending, token: &str) -> bool {
    pending.hash.as_bytes().ct_eq(digest(token).as_bytes()).into()
}

/// SHA-256, hex.
pub(crate) fn digest(value: &str) -> String {
    Sha256::digest(value.as_bytes()).iter().map(|byte| format!("{byte:02x}")).collect()
}

/// 256 bits from the OS, hex.
fn random_token() -> String {
    use rand::Rng as _;

    let mut bytes = [0u8; 32];
    rand::rand_core::UnwrapErr(rand::rngs::SysRng).fill_bytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub(crate) fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_cache::MemoryCache;

    fn broker() -> (PasswordBroker, Arc<MemoryCache>) {
        let cache = Arc::new(MemoryCache::new());
        (PasswordBroker::new(Arc::clone(&cache) as _).throttled(Duration::ZERO), cache)
    }

    #[tokio::test]
    async fn a_token_is_consumed_once() {
        let (broker, _) = broker();
        let token = broker.issue("7").await.unwrap().unwrap();

        assert!(broker.exists("7", &token).await.unwrap());
        broker.consume("7", &token).await.unwrap();
        assert!(broker.consume("7", &token).await.is_err(), "a reused token is refused");
    }

    #[tokio::test]
    async fn a_wrong_token_is_refused_and_leaves_the_right_one_working() {
        let (broker, _) = broker();
        let token = broker.issue("7").await.unwrap().unwrap();

        assert_eq!(broker.consume("7", "nope").await.unwrap_err().status(), 422);
        broker.consume("7", &token).await.unwrap();
    }

    #[tokio::test]
    async fn a_token_is_for_one_account() {
        let (broker, _) = broker();
        let token = broker.issue("7").await.unwrap().unwrap();
        assert!(broker.consume("8", &token).await.is_err());
    }

    #[tokio::test]
    async fn issuing_again_retires_the_previous_link() {
        let (broker, _) = broker();
        let first = broker.issue("7").await.unwrap().unwrap();
        let second = broker.issue("7").await.unwrap().unwrap();

        assert_ne!(first, second);
        assert!(broker.consume("7", &first).await.is_err());
        broker.consume("7", &second).await.unwrap();
    }

    #[tokio::test]
    async fn the_store_holds_a_hash_not_the_token() {
        let (broker, cache) = broker();
        let token = broker.issue("7").await.unwrap().unwrap();

        let stored = cache.get_string("password-reset:7").await.unwrap().unwrap();
        assert!(!stored.contains(&token), "the plaintext token is not stored");
        assert!(stored.contains(&digest(&token)));
        assert_eq!(token.len(), 64, "256 bits, hex");
    }

    #[tokio::test]
    async fn an_expired_token_is_refused() {
        let cache = Arc::new(MemoryCache::new());
        let broker = PasswordBroker::new(cache as _)
            .lasting(Duration::from_millis(1))
            .throttled(Duration::ZERO);
        let token = broker.issue("7").await.unwrap().unwrap();

        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(broker.consume("7", &token).await.is_err());
    }

    #[tokio::test]
    async fn a_second_link_inside_the_throttle_is_not_issued() {
        let broker = PasswordBroker::new(Arc::new(MemoryCache::new()));
        assert!(broker.issue("7").await.unwrap().is_some());
        assert!(broker.issue("7").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn revoking_retires_the_link() {
        let (broker, _) = broker();
        let token = broker.issue("7").await.unwrap().unwrap();
        broker.revoke("7").await.unwrap();
        assert!(!broker.exists("7", &token).await.unwrap());
    }
}
