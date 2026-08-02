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
//!
//! ## Declaring disks rather than wiring them
//!
//! [`Storage`] holds a default disk and a map of named ones. Populating that map
//! by hand works until two disks live on **different backends**, at which point
//! the loop that builds them all from one connector hands the second disk the
//! right bucket name pointed at the wrong service — and that reads an empty
//! prefix rather than raising, so it looks like an empty bucket.
//!
//! [`Disks`] is the declarative form, where every disk carries its own driver
//! and its own settings and is built from those alone:
//!
//! ```
//! use rainier_filesystem::{DiskConfig, Disks};
//!
//! # #[tokio::main] async fn main() -> rainier_support::Result<()> {
//! let storage = Disks::new("uploads")
//!     .with("uploads", DiskConfig::local("storage/app"))
//!     .with("scratch", DiskConfig::memory())
//!     .build()
//!     .await?;
//!
//! assert_eq!(storage.driver(), "local");
//! # Ok(()) }
//! ```
//!
//! It deserialises from a configuration tree, so the same set can come from a
//! `filesystems` section instead. See [the module](disks) for the wire shape and
//! for what a declaration refuses.

#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

pub mod disks;
pub mod driver;
pub mod filesystem;
pub mod local;
pub mod memory;

#[cfg(feature = "s3")]
pub mod s3;

pub use disks::{DiskConfig, Disks, LocalDisk, S3Credentials, S3Disk};
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
    /// Disks reachable by name, beyond the default one above.
    ///
    /// An application that keeps different kinds of file in different places —
    /// uploads separate from generated derivatives, public separate from paid —
    /// needs to say which it means at the call site. Without that, every
    /// operation goes to whichever disk happened to be configured, and a delete
    /// aimed at the wrong bucket is indistinguishable from one that worked.
    named: std::collections::HashMap<String, Arc<dyn Filesystem>>,
}

impl Storage {
    /// Wrap a filesystem as the default disk.
    pub fn new(disk: Arc<dyn Filesystem>) -> Self {
        Self { disk, named: std::collections::HashMap::new() }
    }

    /// Register a disk under a name.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use rainier_filesystem::{MemoryFilesystem, Storage};
    /// let storage = Storage::memory()
    ///     .with_disk("uploads", Arc::new(MemoryFilesystem::new()))
    ///     .with_disk("archive", Arc::new(MemoryFilesystem::new()));
    /// ```
    ///
    /// The imperative form. It takes an already-built disk, so a caller
    /// registering several is the one deciding what each is built from — and a
    /// loop that builds them all from one connector is how a disk ends up with
    /// the right bucket name pointed at the wrong service. [`Disks`] is the
    /// declarative form that cannot express that.
    pub fn with_disk(mut self, name: impl Into<String>, disk: Arc<dyn Filesystem>) -> Self {
        self.named.insert(name.into(), disk);
        self
    }

    /// The disk registered under `name` — `Storage::disk("content")`.
    ///
    /// Named for the framework this borrows from, where the equivalent reads
    /// `Storage::disk('content')->get($path)`. An earlier spelling of this was
    /// `on`, purely because `disk` was taken by the default-disk accessor
    /// (now [`default_disk`](Self::default_disk)); that traded a familiar name
    /// for an unfamiliar one to avoid a rename, which is the wrong way round.
    ///
    /// `None` rather than a fallback to the default: an operation aimed at a
    /// disk that was never configured must not quietly land somewhere else. A
    /// delete is the case that decides this — silently deleting from the wrong
    /// bucket is unrecoverable, and reads the same as success.
    ///
    /// The `Option` is this language's spelling of the exception the original
    /// throws for an unconfigured disk, not a softening of it: both refuse to
    /// guess, and this one is refused at compile time rather than at runtime.
    pub fn disk(&self, name: &str) -> Option<Storage> {
        self.named.get(name).map(|disk| Storage::new(Arc::clone(disk)))
    }

    /// Whether a disk is registered under `name`.
    pub fn has_disk(&self, name: &str) -> bool {
        self.named.contains_key(name)
    }

    /// Every registered disk name, for diagnostics.
    pub fn disk_names(&self) -> impl Iterator<Item = &str> {
        self.named.keys().map(String::as_str)
    }

    /// Files under a local directory.
    pub fn local(root: impl Into<std::path::PathBuf>) -> Self {
        Self::new(Arc::new(LocalFilesystem::new(root)))
    }

    /// Files in memory — for tests.
    pub fn memory() -> Self {
        Self::new(Arc::new(MemoryFilesystem::new()))
    }

    /// The filesystem underneath — the *default* disk, not a named one.
    ///
    /// Spelled out rather than left as a bare `disk()`, so that the name a
    /// caller reaches for by habit ([`disk`](Self::disk), taking a name) is the
    /// one that asks which disk it means.
    pub fn default_disk(&self) -> &Arc<dyn Filesystem> {
        &self.disk
    }

    /// The driver's name — `"local"`, `"s3"`, `"memory"`.
    pub fn driver(&self) -> &str {
        self.disk.name()
    }

    /// This disk as a concrete driver, or `None` if it is a different one.
    ///
    /// The port covers what every backend can do; a presigned URL, a multipart
    /// upload and object tagging are not on it, and reaching them means getting
    /// back to [`S3Filesystem`] itself. That used to mean keeping the value you
    /// constructed — which stops being possible the moment disks are
    /// [declared in configuration](Disks) and nobody constructs one.
    ///
    /// ```
    /// # use rainier_filesystem::{MemoryFilesystem, LocalFilesystem, Storage};
    /// let storage = Storage::memory();
    ///
    /// assert!(storage.as_driver::<MemoryFilesystem>().is_some());
    /// assert!(storage.as_driver::<LocalFilesystem>().is_none());
    /// ```
    ///
    /// `None` rather than a panic, for the same reason [`disk`](Self::disk)
    /// answers `None`: which driver a disk is configured with is a deployment's
    /// decision, and code that asks has to be able to cope with the answer.
    pub fn as_driver<T: Filesystem>(&self) -> Option<&T> {
        self.disk.as_any().downcast_ref::<T>()
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

    #[test]
    fn a_named_disk_is_reachable_and_separate_from_the_default() {
        let storage = Storage::memory().with_disk("content", Arc::new(MemoryFilesystem::new()));

        assert!(storage.has_disk("content"));
        assert!(storage.disk("content").is_some());
    }

    #[test]
    fn an_unregistered_disk_is_none_rather_than_the_default() {
        // The property that matters, and the reason this returns `Option`
        // instead of falling back: an operation aimed at a disk nobody
        // configured must not quietly land somewhere else. A delete decides it
        // — deleting from the wrong bucket is unrecoverable and reads exactly
        // like success.
        let storage = Storage::memory().with_disk("content", Arc::new(MemoryFilesystem::new()));

        assert!(storage.disk("content-paid").is_none());
        assert!(!storage.has_disk("content-paid"));
    }

    #[tokio::test]
    async fn writing_to_one_disk_does_not_appear_on_another() {
        let storage = Storage::memory()
            .with_disk("content", Arc::new(MemoryFilesystem::new()))
            .with_disk("content-paid", Arc::new(MemoryFilesystem::new()));

        storage.disk("content").unwrap().put("a.txt", Bytes::from_static(b"public")).await.unwrap();

        assert!(storage.disk("content").unwrap().exists("a.txt").await.unwrap());
        assert!(
            !storage.disk("content-paid").unwrap().exists("a.txt").await.unwrap(),
            "the disks must not share storage"
        );
    }
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
