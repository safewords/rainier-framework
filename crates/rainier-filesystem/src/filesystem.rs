//! The [`Filesystem`] port and the conveniences every driver gets.

use std::time::Duration;

use bytes::Bytes;
use chrono::{DateTime, Utc};
use rainier_support::{BoxFuture, Error, ErrorKind, Result};

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

    /// This driver as its concrete type, for the operations the port does not
    /// expose.
    ///
    /// The port is deliberately the small set every backend can do, so a
    /// multipart upload, object tagging or a presigned `PUT` is reachable only
    /// through [`S3Filesystem::client`](crate::S3Filesystem::client) — which
    /// used to mean *keep the concrete value you constructed*. Once disks are
    /// [declared in configuration](crate::Disks) nobody constructs them, so
    /// without this there is no route back and the only workaround is to build
    /// a second client beside the one the disk already holds.
    ///
    /// Required rather than defaulted: a default would have to answer with
    /// something that downcasts to nothing, and a driver that silently refuses
    /// to be recognised is worse than one that does not compile. It is one
    /// line — `self` — in every implementation.
    ///
    /// ```
    /// # use rainier_filesystem::{Filesystem, MemoryFilesystem, Storage};
    /// let storage = Storage::memory();
    /// assert!(storage.as_driver::<MemoryFilesystem>().is_some());
    /// ```
    fn as_any(&self) -> &dyn std::any::Any;

    /// Read a file. `None` if it is not there.
    ///
    /// A missing file is **not an error**: "read it if it exists" is the common
    /// case, and making the caller distinguish absence from failure by parsing a
    /// message is how absence ends up being treated as an outage.
    ///
    /// This reads the whole file into memory. For anything user-supplied, whose
    /// size you did not choose, prefer [`read_chunks`](Filesystem::read_chunks).
    fn get<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<Option<Bytes>>>;

    /// Read a file in chunks, calling `on_chunk` with each.
    ///
    /// `Ok(false)` if the file is not there, matching [`get`](Filesystem::get)'s
    /// treatment of absence as an ordinary answer.
    ///
    /// # Why not just `get`
    ///
    /// `get` is fine for a config file and wrong for an upload. Hashing a video
    /// through `get` means holding the whole video in memory, so the peak
    /// footprint of a request is whatever the client chose to send — and the
    /// process that dies from it is the one serving every other request too.
    /// This keeps one chunk live at a time regardless of object size.
    ///
    /// # A callback, not a `Stream`
    ///
    /// A `Stream` would be the more composable shape and would put a generic in
    /// the signature of a trait that is used as `dyn Filesystem` everywhere.
    /// Every caller so far folds the bytes into an accumulator — a hash, a
    /// length, a parser — which a callback expresses without making the port
    /// harder to hold.
    ///
    /// The callback is synchronous on purpose: it runs between reads, and an
    /// `await` in there would stall the read for something unrelated to it.
    ///
    /// # Errors
    ///
    /// Returning `Err` from `on_chunk` aborts the read and surfaces that error
    /// unchanged, so a caller that finds what it needs partway can stop paying
    /// for the rest.
    ///
    /// # Default
    ///
    /// Defaults to one `get` and a single chunk. That is correct but has the
    /// memory profile this exists to avoid, so a driver that can genuinely
    /// stream — anything over HTTP, anything on a real disk — should override
    /// it. Left as a default rather than required so that adding this did not
    /// break every driver outside this repo.
    fn read_chunks<'a>(
        &'a self,
        path: &'a str,
        on_chunk: &'a mut (dyn FnMut(&[u8]) -> Result<()> + Send),
    ) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            match self.get(path).await? {
                Some(bytes) => {
                    on_chunk(&bytes)?;
                    Ok(true)
                }
                None => Ok(false),
            }
        })
    }

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

    /// Every **directory** directly under `prefix`, not recursing.
    ///
    /// [`list`](Self::list) answers with files and says nothing about what is
    /// below them, so "how many variants are stored beside this one" — a count
    /// of sibling directories — has no answer through this port without it. The
    /// alternative is listing every key in the subtree and cutting each at the
    /// first separator, which downloads a whole subtree to learn its shape.
    ///
    /// What comes back is a **prefix `list` accepts**, not a bare segment:
    /// `directories("a")` answers `["a/sub"]`, so descending is passing the
    /// answer back in rather than rebuilding a path at every call site.
    ///
    /// `""` enumerates the root. A prefix with no directories under it — or no
    /// prefix at all — is an empty `Vec`, not an error: "what is in here" has a
    /// sensible answer for somewhere nothing has been written yet, matching
    /// [`list`](Self::list).
    ///
    /// Required rather than defaulted, for the reason
    /// [`as_any`](Self::as_any) is: the only defensible default is an empty
    /// `Vec`, and an empty `Vec` is already the answer for "there are none".
    /// A driver that had not implemented this would be indistinguishable from
    /// one whose prefix is genuinely flat, and the caller counting siblings
    /// would count zero and believe it.
    fn directories<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, Result<Vec<String>>>;

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
    ///
    /// **Public and permanent.** For anything not everyone may read, see
    /// [`temporary_url`](Self::temporary_url), and see there for why one is
    /// never a substitute for the other.
    fn url(&self, path: &str) -> Option<String> {
        let _ = path;
        None
    }

    /// A URL that grants access to one object and stops working after
    /// `expires_in`.
    ///
    /// What paid or otherwise restricted content needs: the object is not
    /// publicly readable, so the link has to carry its own proof of
    /// authorisation, and that proof has to run out.
    ///
    /// # A public URL is never a substitute for a signed one
    ///
    /// This is the whole reason the method exists, and the one rule that must
    /// not bend. [`url`](Self::url) answers with a link that anyone who sees it
    /// can keep and pass on, **for ever** — there is nothing in it to expire. If
    /// a driver that cannot sign quietly answered with that instead, every
    /// paywalled object would ship with a permanent redistributable link, and
    /// nothing about the call site would look wrong: it asked for a temporary
    /// URL and got a URL. So a driver that cannot sign **fails**.
    ///
    /// That is also why this answers `Result<String>` and not
    /// `Result<Option<String>>`. `Option` is the right shape for
    /// [`url`](Self::url), where "there is no public face" is an ordinary state
    /// a caller handles by serving the bytes itself. Here it would be an
    /// invitation: `unwrap_or_else(|| public_url)` is a natural line to write,
    /// reads as a graceful fallback, and is the paywall bypass above. `Result`
    /// makes the same mistake require deliberately discarding an error, and
    /// makes the correct handling a `?`.
    ///
    /// # The default refuses
    ///
    /// A driver signs or it does not, and the one that has not implemented this
    /// says so — naming itself, so the answer to "why is this a 501" is the disk
    /// that is configured rather than a hunt through the drivers. It renders as
    /// **501**, because from outside it is exactly that: the deployment put
    /// restricted content somewhere with no way to sign for it, and no request
    /// the client could have made differently would work.
    ///
    /// ```
    /// # use std::time::Duration;
    /// # use rainier_filesystem::{Filesystem, MemoryFilesystem};
    /// # #[tokio::main] async fn main() {
    /// let disk = MemoryFilesystem::new();
    /// let error = disk.temporary_url("paid/film.mp4", Duration::from_secs(300)).await.unwrap_err();
    ///
    /// assert_eq!(error.status(), 501);
    /// # }
    /// ```
    fn temporary_url<'a>(
        &'a self,
        path: &'a str,
        expires_in: Duration,
    ) -> BoxFuture<'a, Result<String>> {
        let _ = expires_in;
        Box::pin(async move {
            // The path is deliberately absent from the message: this is a
            // deployment fault, not a fact about the object, and the path is
            // frequently the one thing worth not logging.
            let _ = path;
            Err(Error::new(
                ErrorKind::Status(501),
                format!(
                    "the `{}` disk cannot sign a temporary URL; storage that must expire needs a \
                     driver that signs",
                    self.name()
                ),
            ))
        })
    }

    /// A URL a client may **upload** to, for `expires_in`.
    ///
    /// # Weaker than a signed POST form, and usually unavoidable
    ///
    /// An S3 POST policy signs *conditions* — a size range, a content type —
    /// so the bucket itself refuses an upload that breaks them. A presigned
    /// PUT signs a URL and nothing more: whoever holds it may write any bytes,
    /// of any size, to that one key until it expires.
    ///
    /// Reach for a POST policy where the backend implements it. Cloudflare R2
    /// does not — it answers `501 NotImplemented` — so on R2 this is the only
    /// way to let a browser upload directly, and the missing conditions have
    /// to be compensated for by the caller: a short expiry, a key the client
    /// did not choose, and a size checked after the bytes land.
    fn temporary_upload_url<'a>(
        &'a self,
        path: &'a str,
        expires_in: Duration,
    ) -> BoxFuture<'a, Result<String>> {
        let _ = expires_in;
        Box::pin(async move {
            let _ = path;
            Err(Error::new(
                ErrorKind::Status(501),
                format!(
                    "the `{}` disk cannot sign an upload URL; letting a client upload directly                      needs a driver that signs",
                    self.name()
                ),
            ))
        })
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

    /// A driver that **has** a public URL and has **not** implemented signing.
    ///
    /// Deliberately that combination: it is the only shape in which a fallback
    /// from `temporary_url` to `url` would compile, run, and look like it
    /// worked. A driver with no public URL could not fall back even if someone
    /// wrote the code.
    struct PubliclyServed;

    impl Filesystem for PubliclyServed {
        fn name(&self) -> &str {
            "publicly-served"
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }

        fn get<'a>(&'a self, _path: &'a str) -> BoxFuture<'a, Result<Option<Bytes>>> {
            Box::pin(async { Ok(None) })
        }

        fn put<'a>(&'a self, _path: &'a str, _contents: Bytes) -> BoxFuture<'a, Result<()>> {
            Box::pin(async { Ok(()) })
        }

        fn delete<'a>(&'a self, _path: &'a str) -> BoxFuture<'a, Result<bool>> {
            Box::pin(async { Ok(false) })
        }

        fn exists<'a>(&'a self, _path: &'a str) -> BoxFuture<'a, Result<bool>> {
            Box::pin(async { Ok(false) })
        }

        fn metadata<'a>(&'a self, _path: &'a str) -> BoxFuture<'a, Result<Option<Metadata>>> {
            Box::pin(async { Ok(None) })
        }

        fn list<'a>(&'a self, _prefix: &'a str) -> BoxFuture<'a, Result<Vec<Metadata>>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn directories<'a>(&'a self, _prefix: &'a str) -> BoxFuture<'a, Result<Vec<String>>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn url(&self, path: &str) -> Option<String> {
            Some(format!("https://cdn.example.com/{path}"))
        }
    }

    /// **The test that must not be made to pass by relaxing it.**
    ///
    /// If someone gives [`Filesystem::temporary_url`] a default that falls back
    /// to [`Filesystem::url`], every other test in this crate still passes and
    /// every paywalled object silently starts shipping a permanent,
    /// redistributable link. This is the only thing standing in the way, so it
    /// asserts on the failure rather than on the error.
    #[tokio::test]
    async fn a_driver_that_cannot_sign_refuses_rather_than_falling_back_to_a_public_url() {
        let disk = PubliclyServed;
        let public = disk.url("paid/film.mp4").expect("this driver does have a public URL");

        match disk.temporary_url("paid/film.mp4", Duration::from_secs(300)).await {
            Ok(answered) if answered == public => panic!(
                "`temporary_url` answered with the *public* URL `{answered}`. That link never \
                 expires and anyone who receives it can redistribute it for ever, which is the \
                 paywall this method exists to keep shut. A driver that cannot sign must return \
                 an error."
            ),
            Ok(answered) => panic!(
                "`temporary_url` answered `{answered}` from a driver that has not implemented \
                 signing. Whatever it is, it was not signed."
            ),
            Err(error) => {
                assert_eq!(error.status(), 501, "a disk that cannot sign is not a client error");

                // Naming the driver is what makes the 501 actionable: the answer
                // to "why" is which disk is configured, not which route was hit.
                assert!(error.message().contains("publicly-served"), "{}", error.message());

                // A refusal is a fact about the deployment, not about the
                // object, and the path is often the thing worth not logging.
                assert!(!error.message().contains("paid/film.mp4"), "{}", error.message());
            }
        }
    }

    #[tokio::test]
    async fn the_refusal_does_not_depend_on_the_expiry_asked_for() {
        // Otherwise a caller could discover that some duration "works" and get a
        // public URL out of it.
        let disk = PubliclyServed;

        for expiry in [Duration::ZERO, Duration::from_secs(1), Duration::from_secs(86_400 * 365)] {
            assert!(disk.temporary_url("a.txt", expiry).await.is_err(), "{expiry:?}");
        }
    }
}
