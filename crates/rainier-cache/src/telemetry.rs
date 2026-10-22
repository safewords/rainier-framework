//! A `cache.*` span per operation — behind the `telemetry` feature.
//!
//! [`TracedCache`] wraps a store and opens a span around each call.
//! [`CacheManager`](crate::CacheManager) applies it to every store it holds,
//! so anything that reaches the cache through the application's manager — a
//! controller, a rate limiter, a session store, the locks behind the scheduler
//! — is traced without asking.
//!
//! # Never the key
//!
//! A cache key is very often an identifier somebody would rather keep: a
//! session id, an email address, `password-reset:{token}`. It is also the
//! highest-cardinality string in the process. So a span records the store
//! (`cache.store`, its driver), the operation, and for a read whether it hit
//! (`cache.hit`) — never the key, and never the value.

use std::sync::Arc;
use std::time::Duration;

use rainier_support::{BoxFuture, Result};

use crate::cache::Cache;

/// A store, with a span around each call.
pub struct TracedCache {
    inner: Arc<dyn Cache>,
}

impl TracedCache {
    /// Trace `inner` — unless it already is, so wrapping twice is harmless.
    pub fn wrap(inner: Arc<dyn Cache>) -> Arc<dyn Cache> {
        if inner.is_traced() {
            return inner;
        }
        Arc::new(Self { inner })
    }
}

/// One operation's span. A macro because a span's name must be a constant.
macro_rules! traced {
    ($self:ident, $name:literal, $operation:literal, $call:expr) => {{
        traced!($self, $name, $operation, $call, |_| None::<bool>)
    }};
    ($self:ident, $name:literal, $operation:literal, $call:expr, $hit:expr) => {{
        let span = rainier_support::span!(
            $name,
            otel.name = concat!("cache ", $operation),
            cache.operation = $operation,
            cache.store = $self.inner.name(),
            cache.hit = tracing::field::Empty,
        );
        let traced = span.clone();
        let call = $call;
        Box::pin(tracing::Instrument::instrument(
            async move {
                let result = call.await;
                match &result {
                    Ok(value) =>
                    {
                        #[allow(clippy::redundant_closure_call)]
                        if let Some(hit) = ($hit)(value) {
                            traced.record("cache.hit", hit);
                        }
                    }
                    Err(error) => rainier_support::telemetry::record_error(&traced, error),
                }
                result
            },
            span,
        ))
    }};
}

impl Cache for TracedCache {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Option<Vec<u8>>>> {
        traced!(self, "cache.get", "get", self.inner.get(key), |value: &Option<Vec<u8>>| {
            Some(value.is_some())
        })
    }

    fn health<'a>(&'a self) -> BoxFuture<'a, Result<()>> {
        // Not traced: a readiness probe every few seconds is not an operation
        // anybody wants a span for.
        self.inner.health()
    }

    fn put<'a>(
        &'a self,
        key: &'a str,
        value: &'a [u8],
        ttl: Option<Duration>,
    ) -> BoxFuture<'a, Result<()>> {
        traced!(self, "cache.put", "put", self.inner.put(key, value, ttl))
    }

    fn forget<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<bool>> {
        traced!(self, "cache.forget", "forget", self.inner.forget(key))
    }

    fn flush(&self) -> BoxFuture<'_, Result<()>> {
        traced!(self, "cache.flush", "flush", self.inner.flush())
    }

    fn is_shared(&self) -> bool {
        self.inner.is_shared()
    }

    fn supports_atomic_add(&self) -> bool {
        self.inner.supports_atomic_add()
    }

    fn has<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<bool>> {
        traced!(self, "cache.has", "has", self.inner.has(key), |hit: &bool| Some(*hit))
    }

    fn increment<'a>(&'a self, key: &'a str, by: i64) -> BoxFuture<'a, Result<i64>> {
        traced!(self, "cache.increment", "increment", self.inner.increment(key, by))
    }

    fn add<'a>(
        &'a self,
        key: &'a str,
        value: &'a [u8],
        ttl: Option<Duration>,
    ) -> BoxFuture<'a, Result<bool>> {
        // `cache.hit` here is "it was already there": the add — the lock
        // acquisition — lost.
        traced!(self, "cache.add", "add", self.inner.add(key, value, ttl), |stored: &bool| {
            Some(!*stored)
        })
    }

    fn forget_if<'a>(&'a self, key: &'a str, expected: &'a [u8]) -> BoxFuture<'a, Result<bool>> {
        traced!(self, "cache.forget_if", "forget_if", self.inner.forget_if(key, expected))
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

#[cfg(test)]
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
