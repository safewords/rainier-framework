//! [`LocalFilesystem`] — files on this machine's disk.

use std::path::{Path, PathBuf};

use bytes::Bytes;
use chrono::{DateTime, Utc};
use rainier_support::{BoxFuture, Error, Result};

use crate::filesystem::{normalise_path, normalise_prefix, Filesystem, Metadata};

/// Files under one root directory.
///
/// Every path is [normalised](normalise_path) and then joined to the root, and
/// the result is checked to still be **inside** it. Two independent guards,
/// because the consequence of getting it wrong is reading or writing anywhere
/// the process can reach.
pub struct LocalFilesystem {
    root: PathBuf,
    /// A URL prefix, for a root something else serves.
    url_prefix: Option<String>,
}

impl LocalFilesystem {
    /// Files under `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into(), url_prefix: None }
    }

    /// The URL prefix files under this root are reachable at.
    ///
    /// Only meaningful if something — nginx, a CDN, a route you wrote — actually
    /// serves the directory. Without it [`url`](Filesystem::url) is `None`,
    /// because a link that 404s is worse than no link.
    ///
    /// Setting it does **not** make this driver able to sign. Whatever serves
    /// the directory was configured somewhere else and has never heard of this
    /// process, so a signature minted here would be checked by nobody: the URL
    /// would be exactly as public and as permanent as [`url`](Filesystem::url),
    /// while carrying a query string that says otherwise. That is worse than
    /// refusing, because it is the failure that looks fixed.
    /// [`temporary_url`](Filesystem::temporary_url) therefore keeps the port's
    /// refusing default.
    pub fn with_url_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.url_prefix = Some(prefix.into().trim_end_matches('/').to_string());
        self
    }

    /// The root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve a storage path to a filesystem path inside the root.
    fn resolve(&self, path: &str) -> Result<PathBuf> {
        let normalised = normalise_path(path)?;
        let joined = self.root.join(&normalised);

        // The second guard. `normalise_path` already refuses `..`, but a symlink
        // inside the root can point outside it, and on Windows a name like
        // `C:foo` or a device name (`CON`, `NUL`) can escape a naive join.
        // Comparing the canonical parent is what actually holds.
        let parent = joined.parent().unwrap_or(&self.root);
        if let (Ok(canonical_root), Ok(canonical_parent)) =
            (std::fs::canonicalize(&self.root), std::fs::canonicalize(parent))
        {
            if !canonical_parent.starts_with(&canonical_root) {
                return Err(Error::bad_request(
                    "that storage path resolves outside the filesystem root",
                ));
            }
        }

        Ok(joined)
    }

    async fn read_metadata(&self, path: &str, resolved: &Path) -> Result<Option<Metadata>> {
        match tokio::fs::metadata(resolved).await {
            Ok(meta) if meta.is_file() => Ok(Some(Metadata {
                path: normalise_path(path)?,
                size: meta.len(),
                last_modified: meta.modified().ok().map(system_time_to_utc),
                // Neither driver records one.
                content_type: None,
            })),
            // A directory is not a file, and reporting it as one would make
            // `exists` true for something `get` cannot read.
            Ok(_) => Ok(None),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io_error("read metadata for", path, e)),
        }
    }
}

impl Filesystem for LocalFilesystem {
    fn name(&self) -> &str {
        "local"
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn get<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<Option<Bytes>>> {
        Box::pin(async move {
            let resolved = self.resolve(path)?;

            // Checked before reading rather than inferred from the error, because
            // the error a directory produces is **platform-specific**: Unix gives
            // `IsADirectory` and Windows gives `PermissionDenied`. Matching on
            // either would make this port behave differently by platform, and
            // `PermissionDenied` is also a genuine failure worth surfacing.
            if self.read_metadata(path, &resolved).await?.is_none() {
                return Ok(None);
            }

            match tokio::fs::read(&resolved).await {
                Ok(bytes) => Ok(Some(Bytes::from(bytes))),
                // Deleted between the check and the read.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(io_error("read", path, e)),
            }
        })
    }

    fn read_chunks<'a>(
        &'a self,
        path: &'a str,
        on_chunk: &'a mut (dyn FnMut(&[u8]) -> Result<()> + Send),
    ) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            use tokio::io::AsyncReadExt;

            let resolved = self.resolve(path)?;

            let mut file = match tokio::fs::File::open(&resolved).await {
                Ok(file) => file,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                // A directory, on Unix. Windows reports opening one as
                // `PermissionDenied`, which is indistinguishable from a real
                // permission failure, so that case falls through to the error —
                // the same asymmetry `get` documents above.
                Err(e) if e.kind() == std::io::ErrorKind::IsADirectory => return Ok(false),
                Err(e) => return Err(io_error("open", path, e)),
            };

            // 64 KiB: large enough that the syscall overhead disappears against
            // the work done per chunk, small enough to stay in cache.
            let mut buffer = vec![0u8; 64 * 1024];

            loop {
                match file.read(&mut buffer).await {
                    Ok(0) => return Ok(true),
                    Ok(n) => on_chunk(&buffer[..n])?,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(io_error("read", path, e)),
                }
            }
        })
    }

    fn put<'a>(&'a self, path: &'a str, contents: Bytes) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let resolved = self.resolve(path)?;

            if let Some(parent) = resolved.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| io_error("create the directory for", path, e))?;
            }

            // Written to a temporary file and renamed, so a reader never sees a
            // half-written file and a crash mid-write does not truncate what was
            // already there. The rename is atomic on every platform that matters.
            let temporary = resolved.with_extension(format!(
                "{}.rainier-tmp",
                resolved.extension().and_then(|e| e.to_str()).unwrap_or("")
            ));

            tokio::fs::write(&temporary, &contents)
                .await
                .map_err(|e| io_error("write", path, e))?;

            match tokio::fs::rename(&temporary, &resolved).await {
                Ok(()) => Ok(()),
                Err(e) => {
                    // Leaving the temporary behind would accumulate rubbish next
                    // to the real files.
                    let _ = tokio::fs::remove_file(&temporary).await;
                    Err(io_error("write", path, e))
                }
            }
        })
    }

    fn delete<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            let resolved = self.resolve(path)?;

            match tokio::fs::remove_file(&resolved).await {
                Ok(()) => Ok(true),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(e) => Err(io_error("delete", path, e)),
            }
        })
    }

    fn exists<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            let resolved = self.resolve(path)?;
            Ok(self.read_metadata(path, &resolved).await?.is_some())
        })
    }

    fn metadata<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<Option<Metadata>>> {
        Box::pin(async move {
            let resolved = self.resolve(path)?;
            self.read_metadata(path, &resolved).await
        })
    }

    fn list<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, Result<Vec<Metadata>>> {
        Box::pin(async move {
            let normalised = normalise_prefix(prefix)?;
            let directory =
                if normalised.is_empty() { self.root.clone() } else { self.root.join(&normalised) };

            let mut entries = match tokio::fs::read_dir(&directory).await {
                Ok(entries) => entries,
                // A prefix with nothing under it lists as empty, not as an
                // error: "what is in here" has a sensible answer for a
                // directory that does not exist yet.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
                Err(e) => return Err(io_error("list", prefix, e)),
            };

            let mut out = Vec::new();
            while let Some(entry) =
                entries.next_entry().await.map_err(|e| io_error("list", prefix, e))?
            {
                let meta = match entry.metadata().await {
                    Ok(meta) if meta.is_file() => meta,
                    _ => continue,
                };

                let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                    // A name that is not UTF-8 has no storage path, so it is not
                    // addressable through this port and listing it would be a
                    // lie.
                    continue;
                };

                // The half-written files `put` leaves during a rename are not
                // stored files.
                if name.ends_with(".rainier-tmp") {
                    continue;
                }

                out.push(Metadata {
                    path: if normalised.is_empty() { name } else { format!("{normalised}/{name}") },
                    size: meta.len(),
                    last_modified: meta.modified().ok().map(system_time_to_utc),
                    // Neither driver records one.
                    content_type: None,
                });
            }

            // Sorted, so a listing is reproducible; the OS gives no order.
            out.sort_by(|a, b| a.path.cmp(&b.path));
            Ok(out)
        })
    }

    fn directories<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, Result<Vec<String>>> {
        Box::pin(async move {
            let normalised = normalise_prefix(prefix)?;
            let directory =
                if normalised.is_empty() { self.root.clone() } else { self.root.join(&normalised) };

            let mut entries = match tokio::fs::read_dir(&directory).await {
                Ok(entries) => entries,
                // Nothing there lists as empty, matching `list`.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
                Err(e) => return Err(io_error("list the directories under", prefix, e)),
            };

            let mut out = Vec::new();
            while let Some(entry) = entries
                .next_entry()
                .await
                .map_err(|e| io_error("list the directories under", prefix, e))?
            {
                // The exact inverse of `list`'s `is_file`, so between the two
                // every entry is accounted for once and neither reports the
                // other's.
                match entry.metadata().await {
                    Ok(meta) if meta.is_dir() => {}
                    _ => continue,
                }

                let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                    // Not UTF-8, so it has no storage path and nothing could be
                    // done with it if it were reported.
                    continue;
                };

                out.push(if normalised.is_empty() { name } else { format!("{normalised}/{name}") });
            }

            // Sorted, so a listing is reproducible; the OS gives no order.
            out.sort();
            Ok(out)
        })
    }

    fn rename<'a>(&'a self, from: &'a str, to: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let source = self.resolve(from)?;
            let target = self.resolve(to)?;

            if let Some(parent) = target.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| io_error("create the directory for", to, e))?;
            }

            // A real rename rather than copy-then-delete: atomic, and it does
            // not read the file through this process.
            tokio::fs::rename(&source, &target).await.map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    Error::not_found(format!("`{from}` does not exist"))
                } else {
                    io_error("move", from, e)
                }
            })
        })
    }

    fn url(&self, path: &str) -> Option<String> {
        let prefix = self.url_prefix.as_ref()?;
        let normalised = normalise_path(path).ok()?;
        Some(format!("{prefix}/{normalised}"))
    }
}

impl std::fmt::Debug for LocalFilesystem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalFilesystem").field("root", &self.root).finish()
    }
}

fn system_time_to_utc(time: std::time::SystemTime) -> DateTime<Utc> {
    let seconds =
        time.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    DateTime::from_timestamp(seconds, 0).unwrap_or_default()
}

/// Turn an I/O error into a framework one.
///
/// The path is named but the underlying message is not passed through verbatim:
/// an OS error frequently contains the absolute path, which discloses the
/// deployment's directory layout.
fn io_error(action: &str, path: &str, error: std::io::Error) -> Error {
    Error::internal(format!("could not {action} `{path}`: {}", error.kind()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filesystem::FilesystemExt;

    /// A temporary root, removed when the test ends.
    struct Temp(PathBuf);

    impl Temp {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir()
                .join("rainier-fs-tests")
                .join(format!("{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            Self(root)
        }

        fn filesystem(&self) -> LocalFilesystem {
            LocalFilesystem::new(&self.0)
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn a_file_round_trips() {
        let temp = Temp::new("round-trip");
        let fs = temp.filesystem();

        fs.put_string("a/b/hello.txt", "hello").await.unwrap();

        assert_eq!(fs.get_string("a/b/hello.txt").await.unwrap().as_deref(), Some("hello"));
        assert!(fs.exists("a/b/hello.txt").await.unwrap());
        assert_eq!(fs.size("a/b/hello.txt").await.unwrap(), Some(5));
    }

    #[tokio::test]
    async fn directories_are_created_as_needed() {
        let temp = Temp::new("mkdir");
        let fs = temp.filesystem();

        fs.put_string("deeply/nested/path/file.txt", "x").await.unwrap();
        assert!(fs.exists("deeply/nested/path/file.txt").await.unwrap());
    }

    #[tokio::test]
    async fn a_missing_file_reads_as_none_rather_than_erring() {
        let temp = Temp::new("missing");
        let fs = temp.filesystem();

        assert_eq!(fs.get("absent.txt").await.unwrap(), None);
        assert!(!fs.exists("absent.txt").await.unwrap());
        assert_eq!(fs.metadata("absent.txt").await.unwrap(), None);
        assert_eq!(fs.size("absent.txt").await.unwrap(), None);
    }

    #[tokio::test]
    async fn get_or_fail_is_a_404() {
        let temp = Temp::new("or-fail");
        let err = temp.filesystem().get_or_fail("absent.txt").await.unwrap_err();

        assert_eq!(err.status(), 404);
    }

    #[tokio::test]
    async fn deleting_reports_whether_it_was_there() {
        let temp = Temp::new("delete");
        let fs = temp.filesystem();
        fs.put_string("gone.txt", "x").await.unwrap();

        assert!(fs.delete("gone.txt").await.unwrap());
        assert!(!fs.delete("gone.txt").await.unwrap());
    }

    #[tokio::test]
    async fn a_write_replaces_what_was_there() {
        let temp = Temp::new("replace");
        let fs = temp.filesystem();

        fs.put_string("f.txt", "first").await.unwrap();
        fs.put_string("f.txt", "second").await.unwrap();

        assert_eq!(fs.get_string("f.txt").await.unwrap().as_deref(), Some("second"));
    }

    #[tokio::test]
    async fn a_write_leaves_no_temporary_behind() {
        let temp = Temp::new("no-temp");
        let fs = temp.filesystem();
        fs.put_string("f.txt", "x").await.unwrap();

        let listed = fs.list("").await.unwrap();
        assert_eq!(listed.len(), 1, "{listed:?}");
        assert_eq!(listed[0].path, "f.txt");
    }

    #[tokio::test]
    async fn listing_is_shallow_and_sorted() {
        let temp = Temp::new("list");
        let fs = temp.filesystem();

        fs.put_string("z.txt", "x").await.unwrap();
        fs.put_string("a.txt", "x").await.unwrap();
        fs.put_string("sub/deep.txt", "x").await.unwrap();

        let root: Vec<String> = fs.list("").await.unwrap().into_iter().map(|m| m.path).collect();
        assert_eq!(root, vec!["a.txt", "z.txt"], "directories and their contents are not listed");

        let sub: Vec<String> = fs.list("sub").await.unwrap().into_iter().map(|m| m.path).collect();
        assert_eq!(sub, vec!["sub/deep.txt"], "and a listed path is one `get` accepts");
    }

    #[tokio::test]
    async fn listing_a_prefix_with_nothing_under_it_is_empty_not_an_error() {
        let temp = Temp::new("list-empty");
        assert!(temp.filesystem().list("never/created").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn directories_are_found_one_level_at_a_time() {
        let temp = Temp::new("directories");
        let fs = temp.filesystem();

        fs.put_string("a.txt", "x").await.unwrap();
        fs.put_string("uploads/one.png", "x").await.unwrap();
        fs.put_string("uploads/variants/small.png", "x").await.unwrap();
        fs.put_string("archive/old.zip", "x").await.unwrap();

        assert_eq!(fs.directories("").await.unwrap(), vec!["archive", "uploads"], "not recursive");
        assert_eq!(fs.directories("uploads").await.unwrap(), vec!["uploads/variants"]);

        // A returned directory is a prefix `list` accepts.
        let inside: Vec<String> =
            fs.list("uploads/variants").await.unwrap().into_iter().map(|meta| meta.path).collect();
        assert_eq!(inside, vec!["uploads/variants/small.png"]);
    }

    #[tokio::test]
    async fn a_file_is_not_a_directory() {
        // The exact inverse of `a_directory_is_not_a_file`, and between them
        // every entry is reported by one of the two and never by both.
        let temp = Temp::new("files-are-not-directories");
        let fs = temp.filesystem();
        fs.put_string("uploads/one.png", "x").await.unwrap();

        assert!(fs.directories("uploads").await.unwrap().is_empty());
        assert_eq!(fs.list("uploads").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_prefix_with_no_directories_under_it_is_empty_not_an_error() {
        let temp = Temp::new("directories-empty");
        assert!(temp.filesystem().directories("never/created").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn enumerating_directories_refuses_a_traversal() {
        let temp = Temp::new("directories-traversal");
        let fs = temp.filesystem();

        // A canary outside the root, which must not be enumerated.
        let outside = temp.0.parent().unwrap().join("outside-directory");
        let _ = std::fs::create_dir_all(&outside);

        for hostile in ["../", "a/../../", "..\\"] {
            assert!(fs.directories(hostile).await.is_err(), "{hostile}");
        }

        let _ = std::fs::remove_dir_all(&outside);
    }

    #[tokio::test]
    async fn a_directory_is_not_a_file() {
        let temp = Temp::new("directory");
        let fs = temp.filesystem();
        fs.put_string("dir/file.txt", "x").await.unwrap();

        // `exists("dir")` must be false, or `get("dir")` would be expected to
        // work and cannot.
        assert!(!fs.exists("dir").await.unwrap());
        assert_eq!(fs.get("dir").await.unwrap(), None);
    }

    #[tokio::test]
    async fn copying_leaves_the_original() {
        let temp = Temp::new("copy");
        let fs = temp.filesystem();
        fs.put_string("a.txt", "content").await.unwrap();

        fs.copy("a.txt", "b/c.txt").await.unwrap();

        assert_eq!(fs.get_string("a.txt").await.unwrap().as_deref(), Some("content"));
        assert_eq!(fs.get_string("b/c.txt").await.unwrap().as_deref(), Some("content"));
    }

    #[tokio::test]
    async fn copying_something_absent_is_a_404() {
        let temp = Temp::new("copy-missing");
        assert_eq!(temp.filesystem().copy("no.txt", "b.txt").await.unwrap_err().status(), 404);
    }

    #[tokio::test]
    async fn moving_removes_the_original() {
        let temp = Temp::new("move");
        let fs = temp.filesystem();
        fs.put_string("a.txt", "content").await.unwrap();

        fs.rename("a.txt", "sub/b.txt").await.unwrap();

        assert!(!fs.exists("a.txt").await.unwrap());
        assert_eq!(fs.get_string("sub/b.txt").await.unwrap().as_deref(), Some("content"));
    }

    #[tokio::test]
    async fn moving_something_absent_is_a_404() {
        let temp = Temp::new("move-missing");
        assert_eq!(temp.filesystem().rename("no.txt", "b.txt").await.unwrap_err().status(), 404);
    }

    #[tokio::test]
    async fn a_traversal_cannot_escape_the_root() {
        let temp = Temp::new("traversal");
        let fs = temp.filesystem();

        // A canary outside the root, which none of these must reach.
        let outside = temp.0.parent().unwrap().join("outside.txt");
        std::fs::write(&outside, "secret").unwrap();

        for hostile in ["../outside.txt", "a/../../outside.txt", "..\\outside.txt"] {
            assert!(fs.get(hostile).await.is_err(), "{hostile}");
            assert!(fs.put(hostile, Bytes::from_static(b"x")).await.is_err(), "{hostile}");
            assert!(fs.delete(hostile).await.is_err(), "{hostile}");
        }

        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "secret", "untouched");
        let _ = std::fs::remove_file(&outside);
    }

    #[tokio::test]
    async fn an_absolute_path_is_treated_as_relative_to_the_root() {
        let temp = Temp::new("absolute");
        let fs = temp.filesystem();

        // A leading slash is stripped rather than honoured, so this writes
        // inside the root — not to `/etc`.
        fs.put_string("/etc/passwd", "not really").await.unwrap();

        assert!(temp.0.join("etc/passwd").exists());
    }

    #[tokio::test]
    async fn appending_accumulates() {
        let temp = Temp::new("append");
        let fs = temp.filesystem();

        fs.append("log.txt", b"one\n").await.unwrap();
        fs.append("log.txt", b"two\n").await.unwrap();

        assert_eq!(fs.get_string("log.txt").await.unwrap().as_deref(), Some("one\ntwo\n"));
    }

    #[tokio::test]
    async fn metadata_records_a_modification_time() {
        let temp = Temp::new("mtime");
        let fs = temp.filesystem();
        fs.put_string("f.txt", "x").await.unwrap();

        let modified = fs.last_modified("f.txt").await.unwrap().expect("a time");
        assert!(modified.timestamp() > 1_600_000_000, "{modified}");
    }

    #[test]
    fn there_is_no_url_without_a_prefix() {
        let temp = Temp::new("url");
        assert_eq!(temp.filesystem().url("a.txt"), None);

        let served = temp.filesystem().with_url_prefix("https://cdn.example.com/files/");
        assert_eq!(served.url("a/b.txt").as_deref(), Some("https://cdn.example.com/files/a/b.txt"));
    }

    #[test]
    fn a_url_is_not_produced_for_a_path_that_would_be_refused() {
        let temp = Temp::new("url-traversal");
        let served = temp.filesystem().with_url_prefix("https://cdn.example.com");

        assert_eq!(served.url("../secrets"), None);
    }

    #[tokio::test]
    async fn the_driver_is_named() {
        let temp = Temp::new("name");
        assert_eq!(temp.filesystem().name(), "local");
    }

    #[tokio::test]
    async fn unicode_names_work() {
        let temp = Temp::new("unicode");
        let fs = temp.filesystem();

        fs.put_string("uploads/my file 🎉.txt", "x").await.unwrap();
        assert!(fs.exists("uploads/my file 🎉.txt").await.unwrap());
    }

    #[tokio::test]
    async fn a_chunked_read_sees_every_byte_in_order() {
        let temp = Temp::new("chunks");
        let fs = temp.filesystem();

        // Larger than the read buffer, so this genuinely crosses chunks rather
        // than passing by fitting in one.
        let written: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        fs.put("big.bin", Bytes::from(written.clone())).await.unwrap();

        let mut seen = Vec::new();
        let mut chunks = 0usize;
        let found = fs
            .read_chunks("big.bin", &mut |chunk| {
                chunks += 1;
                seen.extend_from_slice(chunk);
                Ok(())
            })
            .await
            .unwrap();

        assert!(found);
        assert_eq!(seen, written, "the bytes came back different");

        // The point of the method. One chunk would mean it quietly fell back to
        // reading the whole object, which is the memory profile it exists to
        // avoid — and every assertion above would still pass.
        assert!(chunks > 1, "read in a single chunk; it is not streaming");
    }

    #[tokio::test]
    async fn a_chunked_read_of_a_missing_file_is_not_an_error() {
        let temp = Temp::new("chunks-missing");

        let mut called = false;
        let found = temp
            .filesystem()
            .read_chunks("nope.bin", &mut |_| {
                called = true;
                Ok(())
            })
            .await
            .unwrap();

        assert!(!found);
        assert!(!called, "the callback ran for a file that does not exist");
    }

    #[tokio::test]
    async fn a_chunked_read_stops_when_the_callback_fails() {
        let temp = Temp::new("chunks-abort");
        let fs = temp.filesystem();

        fs.put("big.bin", Bytes::from(vec![7u8; 300_000])).await.unwrap();

        // A caller that has seen enough should stop paying for the rest.
        let mut chunks = 0usize;
        let result = fs
            .read_chunks("big.bin", &mut |_| {
                chunks += 1;
                Err(Error::internal("seen enough"))
            })
            .await;

        assert!(result.is_err());
        assert_eq!(chunks, 1, "the read carried on after the callback refused");
    }
}
