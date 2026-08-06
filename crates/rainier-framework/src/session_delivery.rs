//! Per-session realtime delivery, wired to a request and a cache.
//!
//! [`rainier_broadcast::sessions`] holds the shape — how a session's channel
//! is named, what makes a key usable, and whether a caller may subscribe to
//! one. All of that is pure, so a worker with no router can publish to a
//! session channel.
//!
//! This is the half that needs the rest of the framework: reading a key out of
//! a request's session, and a [`SessionRoster`] backed by the cache.
//!
//! # Using it
//!
//! ```ignore
//! use rainier_framework::session_delivery::{self, CachedRoster};
//! use rainier_broadcast::sessions::SessionChannels;
//!
//! // Once, wherever the application names things.
//! fn channels() -> SessionChannels {
//!     SessionChannels::new("feed-session.")
//! }
//!
//! // In the route that answers a request:
//! let key = session_delivery::key_of(&request, "feed_delivery_key");
//!
//! // Publishing a reply: `channels().channel(&key)`.
//! // Publishing to every device: `CachedRoster::default().of(subject)`.
//! ```
//!
//! The subscribe side is [`rainier_broadcast::sessions::authorize`], which
//! takes the key the caller's own session holds and the channel they asked
//! for. Mount it on a route with **no** auth guard: it authorises on holding
//! the key, which is a stronger check than being signed in and works for a
//! visitor who is not.

use std::time::Duration;

use async_trait::async_trait;
use rainier_broadcast::sessions::{
    SessionChannels, SessionRoster, DEFAULT_ROSTER_LIMIT, DEFAULT_ROSTER_TTL,
};
use rainier_http::Request;
use rainier_session::SessionRequestExt;

use rainier_container::Facade;

use crate::facades::Cache;

/// The delivery key held in this request's session, minting one if it has none.
///
/// Written into the session on first use rather than derived from the session
/// id, so the id itself never appears in a channel name, a subscribe frame or
/// an auth request.
///
/// `None` when the request has no session at all, which is every route that
/// does not start one — and means there is nowhere to deliver, so a caller
/// should answer rather than queue work for a channel nobody can be on.
pub fn key_of(request: &Request, session_key: &str) -> Option<String> {
    let session = request.session()?;

    if let Some(existing) = session.string(session_key) {
        // Validated on the way out, not only on the way in. It round-trips
        // through a session store, and a channel name is not where to find out
        // it came back wrong.
        if SessionChannels::is_valid_key(&existing) {
            return Some(existing);
        }
    }

    let key = SessionChannels::mint();
    if let Err(e) = session.put(session_key, &key) {
        tracing::warn!(error = %e.message(), "could not store a session delivery key");
        return None;
    }

    Some(key)
}

/// A [`SessionRoster`] kept in the cache.
///
/// One entry per subject holding its keys, most recently seen first. A cache
/// rather than a table because none of it is authoritative — see the trait for
/// what a missing or stale key costs.
#[derive(Debug, Clone)]
pub struct CachedRoster {
    prefix: String,
    ttl: Duration,
    limit: usize,
}

impl Default for CachedRoster {
    fn default() -> Self {
        Self {
            prefix: "sessions:".to_string(),
            ttl: DEFAULT_ROSTER_TTL,
            limit: DEFAULT_ROSTER_LIMIT,
        }
    }
}

impl CachedRoster {
    /// A roster under a different cache prefix.
    ///
    /// For an application tracking more than one kind of session, or sharing a
    /// cache with something that already uses `sessions:`.
    pub fn with_prefix(prefix: impl Into<String>) -> Self {
        Self { prefix: prefix.into(), ..Self::default() }
    }

    /// How long an entry stands without being seen again.
    #[must_use = "this returns a configured roster rather than configuring in place"]
    pub fn expiring_after(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    /// How many sessions to track per subject.
    #[must_use = "this returns a configured roster rather than configuring in place"]
    pub fn keeping(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    fn cache_key(&self, subject: u64) -> String {
        format!("{}{subject}", self.prefix)
    }

    async fn store(&self, subject: u64, keys: &[String]) {
        let Ok(encoded) = serde_json::to_vec(keys) else { return };

        let manager = Cache::instance();
        if let Err(e) =
            manager.store().put(&self.cache_key(subject), &encoded, Some(self.ttl)).await
        {
            tracing::debug!(error = %e.message(), "could not record a delivery session");
        }
    }
}

#[async_trait]
impl SessionRoster for CachedRoster {
    async fn remember(&self, subject: u64, key: &str) {
        let mut keys = self.of(subject).await;

        if keys.first().is_some_and(|first| first == key) {
            // Already the most recent, which is the common case by far — one
            // device asking repeatedly — and not worth a write.
            return;
        }

        keys.retain(|held| held != key);
        keys.insert(0, key.to_string());
        keys.truncate(self.limit);

        self.store(subject, &keys).await;
    }

    async fn of(&self, subject: u64) -> Vec<String> {
        let manager = Cache::instance();

        match manager.store().get(&self.cache_key(subject)).await {
            Ok(Some(bytes)) => serde_json::from_slice(&bytes).unwrap_or_default(),
            Ok(None) => Vec::new(),
            Err(e) => {
                // Nothing known is the safe answer: a fan-out publishes
                // nowhere rather than failing whatever prompted it.
                tracing::debug!(error = %e.message(), "could not read delivery sessions");
                Vec::new()
            }
        }
    }

    async fn forget(&self, subject: u64, key: &str) {
        let mut keys = self.of(subject).await;

        let before = keys.len();
        keys.retain(|held| held != key);
        if keys.len() == before {
            return;
        }

        self.store(subject, &keys).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_roster_defaults_to_bounds_that_suit_a_person() {
        let roster = CachedRoster::default();

        assert_eq!(roster.limit, DEFAULT_ROSTER_LIMIT);
        assert_eq!(roster.ttl, DEFAULT_ROSTER_TTL);
    }

    #[test]
    fn a_prefix_keeps_two_rosters_apart() {
        // Two kinds of session in one cache must not read each other's keys —
        // the symptom would be publishing to channels of the wrong kind.
        let one = CachedRoster::default();
        let two = CachedRoster::with_prefix("admin-sessions:");

        assert_ne!(one.cache_key(7), two.cache_key(7));
    }

    #[test]
    fn a_subject_gets_its_own_entry() {
        let roster = CachedRoster::default();

        assert_ne!(roster.cache_key(7), roster.cache_key(8));
    }
}
