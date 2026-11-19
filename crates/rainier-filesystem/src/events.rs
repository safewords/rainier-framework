//! What this crate publishes on the event bus: [`StorageOperation`], one per
//! call on a disk a [`Storage`](crate::Storage) holds.

use std::sync::Arc;
use std::time::Duration;

use rainier_events::{Activity, Dispatcher, Fields};
use rainier_support::ErrorKind;

/// One call on a disk: kind `storage.get`, `storage.read`, `storage.put`,
/// `storage.delete`, `storage.exists`, `storage.metadata`, `storage.list`,
/// `storage.directories`, `storage.copy`, `storage.rename`, `storage.sign` or
/// `storage.sign_upload` — the `storage.*` span's name — with the fields that
/// span records.
///
/// Published by the wrapper [`Storage`](crate::Storage) applies to every disk
/// it holds, whether or not the `telemetry` feature is on. Like the span, it
/// never carries the path or the contents: a path is routinely personal data.
#[derive(Debug, Clone)]
pub struct StorageOperation {
    /// The span name: `storage.get`, `storage.put`, …
    pub kind: &'static str,
    /// `storage.operation`: `get`, `put`, …
    pub operation: &'static str,
    /// `otel.name`: `storage get`, `storage put`, …
    pub otel_name: &'static str,
    /// `storage.driver`: the disk's driver name — `local`, `memory`, `s3`, …
    pub driver: String,
    /// `storage.bytes`: how many bytes a `get` read or a `put` wrote.
    pub bytes: Option<usize>,
    /// How long the call took.
    pub duration: Duration,
    /// The error's kind, when the call failed.
    pub error: Option<ErrorKind>,
}

impl Activity for StorageOperation {
    fn kind(&self) -> &'static str {
        self.kind
    }

    fn fields(&self, fields: &mut Fields) {
        fields
            .set("otel.name", self.otel_name)
            .set("storage.operation", self.operation)
            .set("storage.driver", self.driver.clone())
            .maybe("storage.bytes", self.bytes);
        if let Some(kind) = self.error {
            fields.set("otel.status_code", "ERROR").set("error.type", error_type(kind));
        }
    }

    fn duration(&self) -> Option<Duration> {
        Some(self.duration)
    }

    fn failed(&self) -> bool {
        self.error.is_some()
    }
}

/// The bus, when something on it would hear a [`StorageOperation`].
#[inline]
pub(crate) fn listening() -> Option<Arc<Dispatcher>> {
    // Asked first by borrowing the bus — no lock, no reference count — so
    // the answer "nobody" costs an atomic load; only a yes clones it.
    if !rainier_events::activity::has_activity_subscribers::<StorageOperation>() {
        return None;
    }
    rainier_events::bus()
}

/// The `error.type` an [`ErrorKind`] records as — the same strings as
/// `rainier_support::telemetry::kind_name`, which exists only with the
/// `telemetry` feature.
fn error_type(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::BadRequest => "bad_request",
        ErrorKind::Unauthenticated => "unauthenticated",
        ErrorKind::Unauthorized => "unauthorized",
        ErrorKind::NotFound => "not_found",
        ErrorKind::MethodNotAllowed => "method_not_allowed",
        ErrorKind::Conflict => "conflict",
        ErrorKind::PayloadTooLarge => "payload_too_large",
        ErrorKind::Validation => "validation",
        ErrorKind::TooManyRequests => "too_many_requests",
        ErrorKind::Internal => "internal",
        ErrorKind::ServiceUnavailable => "service_unavailable",
        ErrorKind::Status(_) => "status",
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use bytes::Bytes;
    use rainier_events::Value;

    use super::*;
    use crate::{MemoryFilesystem, Storage};

    /// Kind, fields and whether it failed, per event heard.
    type Heard = Arc<Mutex<Vec<(&'static str, Fields, bool)>>>;

    fn heard(events: &Dispatcher) -> Heard {
        let heard = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&heard);
        events.on::<StorageOperation>().run_sync(move |op, _| {
            sink.lock().unwrap().push((op.kind(), op.collect_fields(), op.failed()));
            Ok(())
        });
        heard
    }

    #[tokio::test]
    async fn every_disk_call_is_published_with_its_size_and_never_its_path() {
        let events = Arc::new(Dispatcher::new());
        let _bus = rainier_events::bus::set_default(Arc::clone(&events));
        let heard = heard(&events);
        let storage = Storage::memory().with_disk("archive", Arc::new(MemoryFilesystem::new()));
        let disk = storage.default_disk();

        disk.put("exports/ada@example.com.zip", Bytes::from("12345")).await.unwrap();
        disk.get("exports/ada@example.com.zip").await.unwrap();
        disk.exists("exports/ada@example.com.zip").await.unwrap();
        disk.metadata("exports/ada@example.com.zip").await.unwrap();
        disk.list("exports").await.unwrap();
        disk.directories("").await.unwrap();
        disk.copy("exports/ada@example.com.zip", "b").await.unwrap();
        disk.rename("b", "c").await.unwrap();
        let mut sink = |_: &[u8]| Ok(());
        disk.read_chunks("c", &mut sink).await.unwrap();
        disk.delete("c").await.unwrap();
        storage.disk("archive").unwrap().get("nothing").await.unwrap();
        events.settled().await;

        let heard = heard.lock().unwrap();
        let kinds: Vec<_> = heard.iter().map(|(kind, _, _)| *kind).collect();
        assert_eq!(
            kinds,
            [
                "storage.put",
                "storage.get",
                "storage.exists",
                "storage.metadata",
                "storage.list",
                "storage.directories",
                "storage.copy",
                "storage.rename",
                "storage.read",
                "storage.delete",
                "storage.get",
            ]
        );
        let put = &heard[0].1;
        assert_eq!(put.text("otel.name"), Some("storage put"));
        assert_eq!(put.text("storage.operation"), Some("put"));
        assert_eq!(put.text("storage.driver"), Some("memory"));
        assert_eq!(put.get("storage.bytes"), Some(&Value::U64(5)));
        assert_eq!(heard[1].1.get("storage.bytes"), Some(&Value::U64(5)));
        assert_eq!(heard[10].1.get("storage.bytes"), None, "nothing was read");
        assert!(!format!("{heard:?}").contains("ada@example.com"), "never the path");
        assert!(heard.iter().all(|(_, _, failed)| !failed));
    }

    #[tokio::test]
    async fn a_failed_call_is_published_failed() {
        let events = Arc::new(Dispatcher::new());
        let _bus = rainier_events::bus::set_default(Arc::clone(&events));
        let heard = heard(&events);
        let storage = Storage::memory();

        // The memory disk cannot sign: a failure.
        storage.default_disk().temporary_url("x", Duration::from_secs(60)).await.unwrap_err();
        events.settled().await;

        let heard = heard.lock().unwrap();
        assert_eq!(heard[0].0, "storage.sign");
        assert!(heard[0].2);
        assert_eq!(heard[0].1.text("otel.status_code"), Some("ERROR"));
        assert!(heard[0].1.text("error.type").is_some());
    }

    #[tokio::test]
    async fn with_nobody_subscribed_nothing_is_built() {
        let events = Arc::new(Dispatcher::new());
        let _bus = rainier_events::bus::set_default(Arc::clone(&events));
        events.on::<String>().run_sync(|_, _| Ok(()));
        assert!(listening().is_none());

        let storage = Storage::memory();
        storage.default_disk().put("a", Bytes::from("1")).await.unwrap();
        assert!(storage.default_disk().get("a").await.unwrap().is_some());
    }

    #[test]
    fn a_downcast_still_reaches_the_driver() {
        let storage = Storage::new(Arc::new(MemoryFilesystem::new()));
        assert!(storage.default_disk().as_any().downcast_ref::<MemoryFilesystem>().is_some());
    }
}
