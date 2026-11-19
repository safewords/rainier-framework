//! The wrapper [`CacheManager`](crate::CacheManager) applies to every store
//! it holds: a [`CacheOperation`](crate::CacheOperation) event per call,
//! always, and a `cache.*` span per call with the `telemetry` feature.
//!
//! Anything that reaches the cache through the application's manager — a
//! controller, a rate limiter, a session store, the locks behind the scheduler
//! — is observed without asking.
//!
//! # Never the key, in a span
//!
//! A cache key is very often an identifier somebody would rather keep: a
//! session id, an email address, `password-reset:{token}`. It is also the
//! highest-cardinality string in the process. So a span records the store
//! (`cache.store`, its driver), the operation, and for a read whether it hit
//! (`cache.hit`) — never the key, and never the value. The in-process event
//! carries the key; see [`CacheOperation`](crate::CacheOperation).
//!
//! # With nobody listening
//!
//! Without the `telemetry` feature and with nothing subscribed, a call asks
//! the bus once and returns the store's own future: no clock read, no
//! allocation, no extra future.

use std::sync::Arc;
use std::time::Duration;

use rainier_support::{BoxFuture, Result};

use crate::cache::Cache;

/// A store, observed: an event per call, and a span with `telemetry`.
pub struct TracedCache {
    inner: Arc<dyn Cache>,
}

impl TracedCache {
    /// Observe `inner` — unless it already is, so wrapping twice is harmless.
    pub fn wrap(inner: Arc<dyn Cache>) -> Arc<dyn Cache> {
        if inner.is_traced() {
            return inner;
        }
        Arc::new(Self { inner })
    }
}

/// One operation's span and event. A macro because a span's name must be a
/// constant.
macro_rules! traced {
    ($self:ident, $name:literal, $operation:literal, $key:expr, $call:expr) => {{
        traced!($self, $name, $operation, $key, $call, |_| None::<bool>)
    }};
    ($self:ident, $name:literal, $operation:literal, $key:expr, $call:expr, $hit:expr) => {{
        let events = crate::events::listening();
        // Nothing to trace and nobody to tell: the store's own future, as is.
        #[cfg(not(feature = "telemetry"))]
        if events.is_none() {
            return $call;
        }
        #[cfg(feature = "telemetry")]
        let span = rainier_support::span!(
            $name,
            otel.name = concat!("cache ", $operation),
            cache.operation = $operation,
            cache.store = $self.inner.name(),
            cache.hit = tracing::field::Empty,
        );
        #[cfg(feature = "telemetry")]
        let traced = span.clone();
        let started = events.as_ref().map(|_| std::time::Instant::now());
        let key: Option<&str> = $key;
        let call = $call;
        let observed = async move {
            let result = call.await;
            #[allow(clippy::redundant_closure_call)]
            let hit = match &result {
                Ok(value) => ($hit)(value),
                Err(_) => None,
            };
            #[cfg(feature = "telemetry")]
            match &result {
                Ok(_) => {
                    if let Some(hit) = hit {
                        traced.record("cache.hit", hit);
                    }
                }
                Err(error) => rainier_support::telemetry::record_error(&traced, error),
            }
            if let (Some(events), Some(started)) = (events, started) {
                let duration = started.elapsed();
                events.publish_activity_with(|| crate::events::CacheOperation {
                    kind: $name,
                    operation: $operation,
                    otel_name: concat!("cache ", $operation),
                    store: $self.inner.name().to_string(),
                    key: key.map(str::to_string),
                    hit,
                    duration,
                    error: result.as_ref().err().map(|error| error.kind()),
                });
            }
            result
        };
        #[cfg(feature = "telemetry")]
        let observed = tracing::Instrument::instrument(observed, span);
        Box::pin(observed)
    }};
}

impl Cache for TracedCache {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Option<Vec<u8>>>> {
        traced!(self, "cache.get", "get", Some(key), self.inner.get(key), |value: &Option<
            Vec<u8>,
        >| {
            Some(value.is_some())
        })
    }

    fn health<'a>(&'a self) -> BoxFuture<'a, Result<()>> {
        // Not observed: a readiness probe every few seconds is not an
        // operation anybody wants a span or an event for.
        self.inner.health()
    }

    fn put<'a>(
        &'a self,
        key: &'a str,
        value: &'a [u8],
        ttl: Option<Duration>,
    ) -> BoxFuture<'a, Result<()>> {
        traced!(self, "cache.put", "put", Some(key), self.inner.put(key, value, ttl))
    }

    fn forget<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<bool>> {
        traced!(self, "cache.forget", "forget", Some(key), self.inner.forget(key))
    }

    fn flush(&self) -> BoxFuture<'_, Result<()>> {
        traced!(self, "cache.flush", "flush", None, self.inner.flush())
    }

    fn is_shared(&self) -> bool {
        self.inner.is_shared()
    }

    fn supports_atomic_add(&self) -> bool {
        self.inner.supports_atomic_add()
    }

    fn has<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<bool>> {
        traced!(self, "cache.has", "has", Some(key), self.inner.has(key), |hit: &bool| Some(*hit))
    }

    fn increment<'a>(&'a self, key: &'a str, by: i64) -> BoxFuture<'a, Result<i64>> {
        traced!(self, "cache.increment", "increment", Some(key), self.inner.increment(key, by))
    }

    fn touch<'a>(&'a self, key: &'a str, ttl: Option<Duration>) -> BoxFuture<'a, Result<bool>> {
        traced!(
            self,
            "cache.touch",
            "touch",
            Some(key),
            self.inner.touch(key, ttl),
            |found: &bool| { Some(*found) }
        )
    }

    fn touches_in_place(&self) -> bool {
        self.inner.touches_in_place()
    }

    fn add<'a>(
        &'a self,
        key: &'a str,
        value: &'a [u8],
        ttl: Option<Duration>,
    ) -> BoxFuture<'a, Result<bool>> {
        // `cache.hit` here is "it was already there": the add — the lock
        // acquisition — lost.
        traced!(
            self,
            "cache.add",
            "add",
            Some(key),
            self.inner.add(key, value, ttl),
            |stored: &bool| { Some(!*stored) }
        )
    }

    fn forget_if<'a>(&'a self, key: &'a str, expected: &'a [u8]) -> BoxFuture<'a, Result<bool>> {
        traced!(
            self,
            "cache.forget_if",
            "forget_if",
            Some(key),
            self.inner.forget_if(key, expected)
        )
    }

    fn is_traced(&self) -> bool {
        true
    }
}

impl std::fmt::Debug for TracedCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TracedCache").field("store", &self.inner.name()).finish()
    }
}

#[cfg(all(test, feature = "telemetry"))]
mod tests {
    use super::*;
    use crate::{CacheManager, MemoryCache};
    use rainier_support::telemetry::testing::Recorder;

    #[tokio::test]
    async fn every_store_the_manager_holds_is_traced_and_no_key_is_recorded() {
        let recorder = Recorder::new();
        let _guard = recorder.install();
        let manager = CacheManager::new(Arc::new(MemoryCache::new()))
            .with_store("local", Arc::new(MemoryCache::new()));

        manager.put("session:ada@example.com", b"secret-value", None).await.unwrap();
        manager.get("session:ada@example.com").await.unwrap();
        manager.get("missing").await.unwrap();
        manager.store_named("local").unwrap().forget("x").await.unwrap();

        let gets = recorder.named("cache.get");
        assert_eq!(gets[0].field("cache.hit"), Some("true"));
        assert_eq!(gets[1].field("cache.hit"), Some("false"));
        assert_eq!(gets[0].field("cache.store"), Some("memory"));
        assert_eq!(recorder.span("cache.put").field("otel.name"), Some("cache put"));
        recorder.span("cache.forget");

        let everything = recorder.everything();
        assert!(!everything.contains("ada@example.com"), "never the key");
        assert!(!everything.contains("secret-value"), "never the value");
    }

    #[test]
    fn wrapping_twice_is_wrapping_once() {
        let once = TracedCache::wrap(Arc::new(MemoryCache::new()));
        let twice = TracedCache::wrap(Arc::clone(&once));
        assert!(Arc::ptr_eq(&once, &twice));
    }
}
