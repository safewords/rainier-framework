//! What this crate publishes on the event bus: [`CacheOperation`], one per
//! call on a store a [`CacheManager`](crate::CacheManager) holds.

use std::sync::Arc;
use std::time::Duration;

use rainier_events::{Activity, Dispatcher, Fields};
use rainier_support::ErrorKind;

/// One call on a cache store: kind `cache.get`, `cache.put`, `cache.forget`,
/// `cache.flush`, `cache.has`, `cache.increment`, `cache.touch`, `cache.add`
/// or `cache.forget_if` — the `cache.*` span's name — with the fields that
/// span records, and the key.
///
/// Published by the wrapper [`CacheManager`](crate::CacheManager) applies to
/// every store it holds, whether or not the `telemetry` feature is on.
///
/// # The key
///
/// The span never records the key; this event carries it as `cache.key`,
/// because a subscriber in the process — Lookout, a debugging tool — is where
/// one is worth seeing. It is often an identifier somebody would rather keep
/// (`session:{id}`, `password-reset:{token}`), so a subscriber that ships
/// activities out of the process should drop it; Lookout shows it only when
/// its `capture.cache_keys` is on. Values are never carried.
#[derive(Debug, Clone)]
pub struct CacheOperation {
    /// The span name: `cache.get`, `cache.put`, …
    pub kind: &'static str,
    /// `cache.operation`: `get`, `put`, …
    pub operation: &'static str,
    /// `otel.name`: `cache get`, `cache put`, …
    pub otel_name: &'static str,
    /// `cache.store`: the store's driver name — `memory`, `redis`, …
    pub store: String,
    /// `cache.key`, for an operation on one key; `None` for `flush`.
    pub key: Option<String>,
    /// `cache.hit`: for `get` and `has` whether the key was there, for
    /// `touch` whether it was found, for `add` whether it was already there
    /// (the add lost). `None` for the rest, and on a failure.
    pub hit: Option<bool>,
    /// How long the call took.
    pub duration: Duration,
    /// The error's kind, when the call failed.
    pub error: Option<ErrorKind>,
}

impl Activity for CacheOperation {
    fn kind(&self) -> &'static str {
        self.kind
    }

    fn fields(&self, fields: &mut Fields) {
        fields
            .set("otel.name", self.otel_name)
            .set("cache.operation", self.operation)
            .set("cache.store", self.store.clone())
            .maybe("cache.key", self.key.clone())
            .maybe("cache.hit", self.hit);
        if let Some(kind) = self.error {
            fields.set("otel.status_code", "ERROR").set("error.type", kind.name());
        }
    }

    fn duration(&self) -> Option<Duration> {
        Some(self.duration)
    }

    fn failed(&self) -> bool {
        self.error.is_some()
    }
}

/// The bus, when something on it would hear a [`CacheOperation`].
#[inline]
pub(crate) fn listening() -> Option<Arc<Dispatcher>> {
    // Asked first by borrowing the bus — no lock, no reference count — so
    // the answer "nobody" costs an atomic load; only a yes clones it.
    if !rainier_events::activity::has_activity_subscribers::<CacheOperation>() {
        return None;
    }
    rainier_events::bus()
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use rainier_events::Value;

    use super::*;
    use crate::{Cache, CacheManager, MemoryCache};

    /// Kind, fields and whether it failed, per event heard.
    type Heard = Arc<Mutex<Vec<(&'static str, Fields, bool)>>>;

    fn heard(events: &Dispatcher) -> Heard {
        let heard = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&heard);
        events.on::<CacheOperation>().run_sync(move |op, _| {
            sink.lock().unwrap().push((op.kind(), op.collect_fields(), op.failed()));
            Ok(())
        });
        heard
    }

    #[tokio::test]
    async fn every_call_through_the_manager_is_published_with_the_spans_fields() {
        let events = Arc::new(Dispatcher::new());
        let _bus = rainier_events::bus::set_default(Arc::clone(&events));
        let heard = heard(&events);
        let manager = CacheManager::new(Arc::new(MemoryCache::new()))
            .with_store("local", Arc::new(MemoryCache::new()));

        manager.put("posts:1", b"secret-value", None).await.unwrap();
        manager.get("posts:1").await.unwrap();
        manager.get("missing").await.unwrap();
        manager.has("posts:1").await.unwrap();
        manager.increment("hits", 1).await.unwrap();
        manager.touch("posts:1", None).await.unwrap();
        manager.add("lock", b"me", None).await.unwrap();
        manager.forget_if("lock", b"me").await.unwrap();
        manager.store_named("local").unwrap().forget("x").await.unwrap();
        manager.flush().await.unwrap();
        manager.health().await.unwrap();
        events.settled().await;

        let heard = heard.lock().unwrap();
        let kinds: Vec<_> = heard.iter().map(|(kind, _, _)| *kind).collect();
        assert_eq!(
            kinds,
            [
                "cache.put",
                "cache.get",
                "cache.get",
                "cache.has",
                "cache.increment",
                "cache.touch",
                "cache.add",
                "cache.forget_if",
                "cache.forget",
                "cache.flush",
            ],
            "every operation but the health probe"
        );
        let hit = &heard[1].1;
        assert_eq!(hit.text("otel.name"), Some("cache get"));
        assert_eq!(hit.text("cache.operation"), Some("get"));
        assert_eq!(hit.text("cache.store"), Some("memory"));
        assert_eq!(hit.text("cache.key"), Some("posts:1"));
        assert_eq!(hit.get("cache.hit"), Some(&Value::Bool(true)));
        assert_eq!(heard[2].1.get("cache.hit"), Some(&Value::Bool(false)));
        assert_eq!(heard[6].1.get("cache.hit"), Some(&Value::Bool(false)), "the add won");
        assert_eq!(heard[0].1.get("cache.hit"), None, "a put has no hit");
        assert_eq!(heard[9].1.get("cache.key"), None, "a flush has no key");
        assert!(!format!("{heard:?}").contains("secret-value"), "never the value");
        assert!(heard.iter().all(|(_, _, failed)| !failed));
    }

    #[tokio::test]
    async fn a_failed_call_is_published_failed() {
        struct Broken;
        impl Cache for Broken {
            fn name(&self) -> &str {
                "broken"
            }
            fn get<'a>(
                &'a self,
                _: &'a str,
            ) -> rainier_support::BoxFuture<'a, rainier_support::Result<Option<Vec<u8>>>>
            {
                Box::pin(async { Err(rainier_support::Error::service_unavailable("down")) })
            }
            fn put<'a>(
                &'a self,
                _: &'a str,
                _: &'a [u8],
                _: Option<Duration>,
            ) -> rainier_support::BoxFuture<'a, rainier_support::Result<()>> {
                Box::pin(async { Ok(()) })
            }
            fn forget<'a>(
                &'a self,
                _: &'a str,
            ) -> rainier_support::BoxFuture<'a, rainier_support::Result<bool>> {
                Box::pin(async { Ok(false) })
            }
            fn flush(&self) -> rainier_support::BoxFuture<'_, rainier_support::Result<()>> {
                Box::pin(async { Ok(()) })
            }
            fn increment<'a>(
                &'a self,
                _: &'a str,
                _: i64,
            ) -> rainier_support::BoxFuture<'a, rainier_support::Result<i64>> {
                Box::pin(async { Ok(0) })
            }
            fn add<'a>(
                &'a self,
                _: &'a str,
                _: &'a [u8],
                _: Option<Duration>,
            ) -> rainier_support::BoxFuture<'a, rainier_support::Result<bool>> {
                Box::pin(async { Ok(false) })
            }
            fn forget_if<'a>(
                &'a self,
                _: &'a str,
                _: &'a [u8],
            ) -> rainier_support::BoxFuture<'a, rainier_support::Result<bool>> {
                Box::pin(async { Ok(false) })
            }
        }

        let events = Arc::new(Dispatcher::new());
        let _bus = rainier_events::bus::set_default(Arc::clone(&events));
        let heard = heard(&events);
        let manager = CacheManager::new(Arc::new(Broken));

        manager.get("k").await.unwrap_err();
        events.settled().await;

        let heard = heard.lock().unwrap();
        assert!(heard[0].2);
        assert_eq!(heard[0].1.text("otel.status_code"), Some("ERROR"));
        assert_eq!(heard[0].1.text("error.type"), Some("service_unavailable"));
        assert_eq!(heard[0].1.get("cache.hit"), None);
    }

    #[tokio::test]
    async fn with_nobody_subscribed_nothing_is_built() {
        let events = Arc::new(Dispatcher::new());
        let _bus = rainier_events::bus::set_default(Arc::clone(&events));
        events.on::<String>().run_sync(|_, _| Ok(()));
        assert!(listening().is_none());

        let manager = CacheManager::memory();
        manager.put("k", b"v", None).await.unwrap();
        assert_eq!(manager.get("k").await.unwrap().as_deref(), Some(&b"v"[..]));
    }

    #[test]
    fn wrapping_twice_is_wrapping_once() {
        let once = crate::telemetry::TracedCache::wrap(Arc::new(MemoryCache::new()));
        let twice = crate::telemetry::TracedCache::wrap(Arc::clone(&once));
        assert!(Arc::ptr_eq(&once, &twice));
    }
}
