//! A `storage.*` span per disk operation — behind the `telemetry` feature.
//!
//! [`TracedFilesystem`] wraps a disk; [`Storage`](crate::Storage) applies it
//! to every disk it holds, so uploads, downloads and listings are traced
//! without the code doing them knowing.
//!
//! # Never the path
//!
//! An object path is routinely personal data — `avatars/{user_id}.jpg`,
//! `exports/ada@example.com.zip`, `invoices/2026/acme-corp.pdf` — and it is
//! unbounded besides. A span records the disk's driver, the operation and,
//! for a read or a write, how many bytes moved. Never the path, and never the
//! contents. `as_any` is passed through, so a caller downcasting to a concrete
//! driver still reaches it.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use rainier_support::{BoxFuture, Result};

use crate::filesystem::{Filesystem, Metadata};

/// A disk, with a span around each call.
pub struct TracedFilesystem {
    inner: Arc<dyn Filesystem>,
}

impl TracedFilesystem {
    /// Trace `inner` — unless it already is.
    pub fn wrap(inner: Arc<dyn Filesystem>) -> Arc<dyn Filesystem> {
        if inner.is_traced() {
            return inner;
        }
        Arc::new(Self { inner })
    }
}

/// One operation's span. A macro because a span's name must be a constant.
macro_rules! traced {
    ($self:ident, $name:literal, $operation:literal, $call:expr) => {{
        traced!($self, $name, $operation, $call, |_| None::<usize>)
    }};
    ($self:ident, $name:literal, $operation:literal, $call:expr, $bytes:expr) => {{
        let span = rainier_support::span!(
            $name,
            otel.name = concat!("storage ", $operation),
            storage.operation = $operation,
            storage.driver = $self.inner.name(),
            storage.bytes = tracing::field::Empty,
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
                        if let Some(bytes) = ($bytes)(value) {
                            traced.record("storage.bytes", bytes);
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

impl Filesystem for TracedFilesystem {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self.inner.as_any()
    }

    fn get<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<Option<Bytes>>> {
        traced!(self, "storage.get", "get", self.inner.get(path), |value: &Option<Bytes>| {
            value.as_ref().map(Bytes::len)
        })
    }

    fn read_chunks<'a>(
        &'a self,
        path: &'a str,
        on_chunk: &'a mut (dyn FnMut(&[u8]) -> Result<()> + Send),
    ) -> BoxFuture<'a, Result<bool>> {
        traced!(self, "storage.read", "read", self.inner.read_chunks(path, on_chunk))
    }

    fn put<'a>(&'a self, path: &'a str, contents: Bytes) -> BoxFuture<'a, Result<()>> {
        let size = contents.len();
        traced!(self, "storage.put", "put", self.inner.put(path, contents), move |_: &()| Some(
            size
        ))
    }

    fn delete<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<bool>> {
        traced!(self, "storage.delete", "delete", self.inner.delete(path))
    }

    fn exists<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<bool>> {
        traced!(self, "storage.exists", "exists", self.inner.exists(path))
    }

    fn metadata<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<Option<Metadata>>> {
        traced!(self, "storage.metadata", "metadata", self.inner.metadata(path))
    }

    fn list<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, Result<Vec<Metadata>>> {
        traced!(self, "storage.list", "list", self.inner.list(prefix))
    }

    fn directories<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, Result<Vec<String>>> {
        traced!(self, "storage.directories", "directories", self.inner.directories(prefix))
    }

    fn copy<'a>(&'a self, from: &'a str, to: &'a str) -> BoxFuture<'a, Result<()>> {
        traced!(self, "storage.copy", "copy", self.inner.copy(from, to))
    }

    fn rename<'a>(&'a self, from: &'a str, to: &'a str) -> BoxFuture<'a, Result<()>> {
        traced!(self, "storage.rename", "rename", self.inner.rename(from, to))
    }

    fn url(&self, path: &str) -> Option<String> {
        self.inner.url(path)
    }

    fn temporary_url<'a>(
        &'a self,
        path: &'a str,
        expires_in: Duration,
    ) -> BoxFuture<'a, Result<String>> {
        traced!(self, "storage.sign", "sign", self.inner.temporary_url(path, expires_in))
    }

    fn temporary_upload_url<'a>(
        &'a self,
        path: &'a str,
        expires_in: Duration,
    ) -> BoxFuture<'a, Result<String>> {
        traced!(
            self,
            "storage.sign_upload",
            "sign_upload",
            self.inner.temporary_upload_url(path, expires_in)
        )
    }

    fn is_traced(&self) -> bool {
        true
    }
}

impl std::fmt::Debug for TracedFilesystem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TracedFilesystem").field("driver", &self.inner.name()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MemoryFilesystem, Storage};
    use rainier_support::telemetry::testing::Recorder;

    #[tokio::test]
    async fn a_disk_operation_is_a_span_with_its_size_and_never_its_path() {
        let recorder = Recorder::new();
        let _guard = recorder.install();
        let storage = Storage::memory();

        storage
            .default_disk()
            .put("exports/ada@example.com.zip", Bytes::from("12345"))
            .await
            .unwrap();
        storage.default_disk().get("exports/ada@example.com.zip").await.unwrap();

        assert_eq!(recorder.span("storage.put").field("storage.bytes"), Some("5"));
        assert_eq!(recorder.span("storage.get").field("storage.bytes"), Some("5"));
        assert_eq!(recorder.span("storage.get").field("storage.driver"), Some("memory"));
        assert!(!recorder.everything().contains("ada@example.com"));
    }

    #[test]
    fn a_downcast_still_reaches_the_driver() {
        let storage = Storage::new(Arc::new(MemoryFilesystem::new()));
        assert!(storage.default_disk().as_any().downcast_ref::<MemoryFilesystem>().is_some());
    }
}
