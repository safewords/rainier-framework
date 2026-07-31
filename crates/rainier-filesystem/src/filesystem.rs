//! The [`Filesystem`] port and the conveniences every driver gets.

use bytes::Bytes;
use chrono::{DateTime, Utc};
use rainier_support::{BoxFuture, Error, Result};

/// What a driver knows about one stored file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Metadata {
    /// The path, as the driver would accept it back.
    pub path: String,
    /// Size in bytes.
    pub size: u64,
    /// When it was last written, if the driver records that.
    pub last_modified: Option<DateTime<Utc>>,
}

/// Where a driver keeps bytes.
///
/// One shape for every driver, for one reason: an
/// application that writes uploads should not know whether they land on a disk,
/// in S3, or in a test's memory.
///
/// Paths are **always `/`-separated and always relative**, whatever the platform
/// underneath. A driver that stores on disk translates; a driver that stores in a
/// bucket does not have to.
pub trait Filesystem: Send + Sync + 'static {
    /// A label for diagnostics — `"local"`, `"s3"`, `"memory"`.
    fn name(&self) -> &str;

    /// Read a file. `None` if it is not there.
    ///
    /// A missing file is **not an error**: "read it if it exists" is the common
    /// case, and making the caller distinguish absence from failure by parsing a
    /// message is how absence ends up being treated as an outage.
    fn get<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<Option<Bytes>>>;

    /// Write a file, creating any directories it needs and replacing what was
    /// there.
    fn put<'a>(&'a self, path: &'a str, contents: Bytes) -> BoxFuture<'a, Result<()>>;

    /// Delete a file. `true` if it was there.
    fn delete<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<bool>>;

    /// Whether a file exists.
    fn exists<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<bool>>;

    /// A file's metadata, or `None`.
    fn metadata<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<Option<Metadata>>>;

    /// Every file directly under `prefix`, not recursing.
    ///
    /// `""` lists the root.
    fn list<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, Result<Vec<Metadata>>>;

    /// Copy a file.
    ///
    /// Provided as read-then-write, which every driver can do. Override it where
    /// the backend can copy server-side — S3 can, and doing so avoids pulling
    /// the object through this process.
    fn copy<'a>(&'a self, from: &'a str, to: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let contents = self
                .get(from)
                .await?
                .ok_or_else(|| Error::not_found(format!("`{from}` does not exist")))?;
            self.put(to, contents).await
        })
    }

    /// Move a file.
    fn rename<'a>(&'a self, from: &'a str, to: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.copy(from, to).await?;
            self.delete(from).await?;
            Ok(())
        })
    }

    /// A URL a browser can fetch, if this driver has one.
    ///
    /// `None` for a driver with no public face — a local disk behind an
    /// application is not reachable by URL, and pretending otherwise produces a
    /// link that 404s.
    fn url(&self, path: &str) -> Option<String> {
        let _ = path;
        None
    }
}

/// The typed conveniences every [`Filesystem`] gets.
///
/// Kept out of the object-safe trait so `Arc<dyn Filesystem>` still exists.
#[async_trait::async_trait]
pub trait FilesystemExt: Filesystem {
    /// Read a file as a string.
    async fn get_string(&self, path: &str) -> Result<Option<String>> {
        match self.get(path).await? {
            Some(bytes) => String::from_utf8(bytes.to_vec())
                .map(Some)
                .map_err(|_| Error::internal(format!("`{path}` is not valid UTF-8"))),
            None => Ok(None),
        }
    }

    /// Write a string.
    async fn put_string(&self, path: &str, contents: &str) -> Result<()> {
        self.put(path, Bytes::from(contents.to_string().into_bytes())).await
    }

    /// Read a file, failing with a `404` if it is absent.
    ///
    /// What a controller serving a file wants, so the missing case is one `?`.
    async fn get_or_fail(&self, path: &str) -> Result<Bytes> {
        self.get(path).await?.ok_or_else(|| Error::not_found(format!("`{path}` does not exist")))
    }

    /// A file's size.
    async fn size(&self, path: &str) -> Result<Option<u64>> {
        Ok(self.metadata(path).await?.map(|meta| meta.size))
    }

    /// When a file was last written.
    async fn last_modified(&self, path: &str) -> Result<Option<DateTime<Utc>>> {
        Ok(self.metadata(path).await?.and_then(|meta| meta.last_modified))
    }

    /// Append to a file, creating it if absent.
    ///
    /// **Read-modify-write, and therefore not atomic.** Two concurrent appends
    /// can lose one. Fine for a log nobody depends on; wrong for anything else,
    /// and there is no portable way to make it right across a disk and a bucket.
    async fn append(&self, path: &str, contents: &[u8]) -> Result<()> {
        let mut existing = self.get(path).await?.map(|b| b.to_vec()).unwrap_or_default();
        existing.extend_from_slice(contents);
        self.put(path, Bytes::from(existing)).await
    }
}

impl<F: Filesystem + ?Sized> FilesystemExt for F {}

/// Reject a path that could escape its root, and normalise separators.
///
/// Applies to **every** driver, not only the local one: a `..` in an S3 key is
/// legal but nearly always a bug, and a leading `/` produces a key with an empty
/// first segment that is then impossible to address consistently.
pub fn normalise_path(path: &str) -> Result<String> {
    let path = path.replace('\\', "/");

    let mut segments = Vec::new();
    for segment in path.split('/') {
        match segment {
            // A doubled separator, a leading one, or `./` — all noise.
            "" | "." => continue,
            ".." => {
                return Err(Error::bad_request(
                    "a storage path must not contain `..`; it is either an attempt to escape \
                     the root or a path that was built wrong",
                ))
            }
            segment => segments.push(segment),
        }
    }

    if segments.is_empty() {
        return Err(Error::bad_request("a storage path must not be empty"));
    }

    // A NUL truncates the path in every C API underneath, so a name containing
    // one can address a different file than it appears to.
    let joined = segments.join("/");
    if joined.contains('\0') {
        return Err(Error::bad_request("a storage path must not contain a NUL byte"));
    }

    Ok(joined)
}

/// Normalise a listing prefix, which unlike a path may be empty.
pub fn normalise_prefix(prefix: &str) -> Result<String> {
    let trimmed = prefix.trim_matches('/');
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    normalise_path(trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_path_passes_through() {
        assert_eq!(normalise_path("a/b/c.txt").unwrap(), "a/b/c.txt");
    }

    #[test]
    fn separators_are_normalised() {
        assert_eq!(normalise_path("a\\b\\c.txt").unwrap(), "a/b/c.txt");
        assert_eq!(normalise_path("a//b///c.txt").unwrap(), "a/b/c.txt");
        assert_eq!(normalise_path("/a/b/").unwrap(), "a/b");
        assert_eq!(normalise_path("./a/./b").unwrap(), "a/b");
    }

    #[test]
    fn a_traversal_is_refused_rather_than_resolved() {
        // Resolving it would silently address a different file, which is worse
        // than refusing: the caller thinks it wrote where it asked.
        for hostile in ["../secrets", "a/../../etc/passwd", "..", "a/..", "..\\windows"] {
            let err = normalise_path(hostile).unwrap_err();
            assert_eq!(err.status(), 400, "{hostile}");
            assert!(err.message().contains(".."), "{hostile}");
        }
    }

    #[test]
    fn an_empty_path_is_refused() {
        for empty in ["", "/", "//", ".", "./"] {
            assert!(normalise_path(empty).is_err(), "{empty:?}");
        }
    }

    #[test]
    fn a_nul_byte_is_refused() {
        // It truncates the path in every C API underneath, so `a\0b.txt` can
        // address `a` while looking like something else.
        assert!(normalise_path("a\0b.txt").is_err());
    }

    #[test]
    fn a_prefix_may_be_empty_but_still_may_not_escape() {
        assert_eq!(normalise_prefix("").unwrap(), "");
        assert_eq!(normalise_prefix("/").unwrap(), "");
        assert_eq!(normalise_prefix("uploads/").unwrap(), "uploads");
        assert!(normalise_prefix("../etc").is_err());
    }

    #[test]
    fn unicode_and_spaces_survive() {
        assert_eq!(normalise_path("uploads/my file 🎉.png").unwrap(), "uploads/my file 🎉.png");
    }
}
