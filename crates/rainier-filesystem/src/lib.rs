//! # rainier-filesystem
//!
//! File storage behind one port, with one point:
//! an application that writes an upload should not know whether it lands on a
//! disk, in a bucket, or in a test's memory.
//!
//! ```
//! use rainier_filesystem::{Filesystem, FilesystemExt, MemoryFilesystem};
//!
//! # #[tokio::main] async fn main() -> rainier_support::Result<()> {
//! let disk = MemoryFilesystem::new();
//!
//! disk.put_string("uploads/report.csv", "a,b,c").await?;
//! assert_eq!(disk.get_string("uploads/report.csv").await?.as_deref(), Some("a,b,c"));
//! assert_eq!(disk.size("uploads/report.csv").await?, Some(5));
//! # Ok(()) }
//! ```
//!
//! ## Drivers
//!
//! | Driver | Feature | Notes |
//! |---|---|---|
//! | [`LocalFilesystem`] | — | one directory on this machine |
//! | [`MemoryFilesystem`] | — | tests |
//! | `S3Filesystem` | `s3` | S3, **Cloudflare R2**, MinIO, B2, Wasabi |
//!
//! S3-compatible is not a special case. R2 and MinIO speak SigV4 against a
//! different endpoint, so the difference is entirely in
//! `AwsConfig`, from `rainier-drivers`:
//!
//! ```ignore
//! // S3
//! S3Filesystem::new(AwsClient::new(AwsConfig::from_env()?), "bucket")
//!
//! // R2 — the same driver
//! S3Filesystem::new(
//!     AwsClient::new(
//!         AwsConfig::new(id, secret, "auto")
//!             .endpoint("https://account.r2.cloudflarestorage.com"),
//!     ),
//!     "bucket",
//! )
//! ```
//!
//! ## A missing file is not an error
//!
//! [`get`](Filesystem::get) returns `Ok(None)` for an absent file and `Err` only
//! when the storage itself failed. "Read it if it is there" is the common case,
//! and making a caller distinguish absence from failure by parsing a message is
//! how absence ends up treated as an outage.
//!
//! [`FilesystemExt::get_or_fail`] is the other half, for a controller that wants
//! the `404` in one `?`.
//!
//! ## Paths are checked by every driver
//!
//! Always `/`-separated, always relative, and `..` is **refused** rather than
//! resolved:
//!
//! ```
//! # use rainier_filesystem::{Filesystem, MemoryFilesystem};
//! # #[tokio::main] async fn main() {
//! let disk = MemoryFilesystem::new();
//! assert!(disk.get("../../etc/passwd").await.is_err());
//! # }
//! ```
//!
//! Refusing beats resolving: a resolved traversal silently addresses a different
//! file, so the caller believes it wrote where it asked. The rule is enforced in
//! the **memory** driver too, so a test cannot pass with a path production would
//! reject.
//!
//! The local driver adds a second guard — the resolved parent must canonicalise
//! to somewhere inside the root — because a symlink inside the root can point
//! out of it and `..` alone does not catch that.

#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

pub mod driver;
pub mod filesystem;
pub mod local;
pub mod memory;

#[cfg(feature = "s3")]
pub mod s3;

pub use driver::FilesystemDriver;
pub use filesystem::{normalise_path, normalise_prefix, Filesystem, FilesystemExt, Metadata};
pub use local::LocalFilesystem;
pub use memory::MemoryFilesystem;

#[cfg(feature = "s3")]
pub use s3::S3Filesystem;

use std::sync::Arc;

/// The application's storage, as one container-storable value.
///
/// A newtype over the port rather than binding `Arc<dyn Filesystem>` directly,
/// so swapping a driver does not change the type every call site names — the
/// same shape as `rainier-framework`'s `Views`.
#[derive(Clone)]
pub struct Storage {
    disk: Arc<dyn Filesystem>,
}

impl Storage {
    /// Wrap a filesystem.
    pub fn new(disk: Arc<dyn Filesystem>) -> Self {
        Self { disk }
    }

    /// Files under a local directory.
    pub fn local(root: impl Into<std::path::PathBuf>) -> Self {
        Self::new(Arc::new(LocalFilesystem::new(root)))
    }

    /// Files in memory — for tests.
    pub fn memory() -> Self {
        Self::new(Arc::new(MemoryFilesystem::new()))
    }

    /// The filesystem underneath.
    pub fn disk(&self) -> &Arc<dyn Filesystem> {
        &self.disk
    }

    /// The driver's name — `"local"`, `"s3"`, `"memory"`.
    pub fn driver(&self) -> &str {
        self.disk.name()
    }
}

impl std::ops::Deref for Storage {
    type Target = Arc<dyn Filesystem>;

    /// So `Storage::instance().get(..)` works without naming `disk()`.
    fn deref(&self) -> &Self::Target {
        &self.disk
    }
}

impl std::fmt::Debug for Storage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Storage").field("driver", &self.driver()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    #[tokio::test]
    async fn the_facade_value_delegates_to_its_disk() {
        let storage = Storage::memory();

        storage.put_string("a.txt", "hello").await.unwrap();
        assert_eq!(storage.get_string("a.txt").await.unwrap().as_deref(), Some("hello"));
        assert_eq!(storage.driver(), "memory");
    }

    /// Every driver has to behave the same from the outside — which is the point
    /// of the port, and is where a driver-specific quirk would otherwise hide.
    #[tokio::test]
    async fn every_local_driver_behaves_identically() {
        let root =
            std::env::temp_dir().join("rainier-fs-parity").join(format!("{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let disks: Vec<(&str, Arc<dyn Filesystem>)> = vec![
            ("memory", Arc::new(MemoryFilesystem::new())),
            ("local", Arc::new(LocalFilesystem::new(&root))),
        ];

        for (name, disk) in disks {
            // Absence.
            assert_eq!(disk.get("absent.txt").await.unwrap(), None, "{name}");
            assert!(!disk.exists("absent.txt").await.unwrap(), "{name}");
            assert!(!disk.delete("absent.txt").await.unwrap(), "{name}");

            // Round trip.
            disk.put("a/b.txt", Bytes::from_static(b"hello")).await.unwrap();
            assert_eq!(disk.get("a/b.txt").await.unwrap().unwrap(), "hello", "{name}");
            assert_eq!(disk.size("a/b.txt").await.unwrap(), Some(5), "{name}");

            // Normalisation.
            assert_eq!(disk.get("/a//b.txt").await.unwrap().unwrap(), "hello", "{name}");

            // Traversal.
            assert!(disk.get("../escape").await.is_err(), "{name}");

            // Shallow listing.
            disk.put("a/deeper/c.txt", Bytes::from_static(b"x")).await.unwrap();
            let listed: Vec<String> =
                disk.list("a").await.unwrap().into_iter().map(|m| m.path).collect();
            assert_eq!(listed, vec!["a/b.txt".to_string()], "{name}");

            // Copy and move.
            disk.copy("a/b.txt", "copied.txt").await.unwrap();
            assert!(disk.exists("a/b.txt").await.unwrap(), "{name}");
            disk.rename("copied.txt", "moved.txt").await.unwrap();
            assert!(!disk.exists("copied.txt").await.unwrap(), "{name}");
            assert_eq!(disk.get("moved.txt").await.unwrap().unwrap(), "hello", "{name}");

            // A 404 from the failing variants.
            assert_eq!(disk.get_or_fail("absent.txt").await.unwrap_err().status(), 404, "{name}");
            assert_eq!(disk.copy("absent.txt", "x.txt").await.unwrap_err().status(), 404, "{name}");
        }

        let _ = std::fs::remove_dir_all(&root);
    }
}
