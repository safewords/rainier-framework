//! [`MemoryFilesystem`] — files in this process, for tests.

use std::collections::HashMap;
use std::sync::Mutex;

use bytes::Bytes;
use chrono::{DateTime, Utc};
use rainier_support::{BoxFuture, Result};

use crate::filesystem::{normalise_path, normalise_prefix, Filesystem, Metadata};

/// Files held in memory.
///
/// What a test wants: no directory to create, no cleanup to forget, and the
/// same [`Filesystem`] the application uses in production. It also **enforces
/// the same path rules**, so a test cannot pass with a path the local or S3
/// driver would refuse.
pub struct MemoryFilesystem {
    files: Mutex<HashMap<String, Entry>>,
}

#[derive(Debug, Clone)]
struct Entry {
    contents: Bytes,
    last_modified: DateTime<Utc>,
}

impl Default for MemoryFilesystem {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryFilesystem {
    /// An empty filesystem.
    pub fn new() -> Self {
        Self { files: Mutex::new(HashMap::new()) }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        self.files.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// How many files are held.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether nothing is held.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Every path, sorted. For asserting on what a test wrote.
    pub fn paths(&self) -> Vec<String> {
        let mut paths: Vec<String> = self.lock().keys().cloned().collect();
        paths.sort();
        paths
    }
}

impl Filesystem for MemoryFilesystem {
    fn name(&self) -> &str {
        "memory"
    }

    fn get<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<Option<Bytes>>> {
        Box::pin(async move {
            let path = normalise_path(path)?;
            Ok(self.lock().get(&path).map(|entry| entry.contents.clone()))
        })
    }

    fn put<'a>(&'a self, path: &'a str, contents: Bytes) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let path = normalise_path(path)?;
            self.lock().insert(path, Entry { contents, last_modified: Utc::now() });
            Ok(())
        })
    }

    fn delete<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            let path = normalise_path(path)?;
            Ok(self.lock().remove(&path).is_some())
        })
    }

    fn exists<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            let path = normalise_path(path)?;
            Ok(self.lock().contains_key(&path))
        })
    }

    fn metadata<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<Option<Metadata>>> {
        Box::pin(async move {
            let normalised = normalise_path(path)?;
            Ok(self.lock().get(&normalised).map(|entry| Metadata {
                path: normalised.clone(),
                size: entry.contents.len() as u64,
                last_modified: Some(entry.last_modified),
            }))
        })
    }

    fn list<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, Result<Vec<Metadata>>> {
        Box::pin(async move {
            let prefix = normalise_prefix(prefix)?;
            let files = self.lock();

            let mut out: Vec<Metadata> = files
                .iter()
                .filter(|(path, _)| {
                    // Shallow, like the local driver: directly under the prefix
                    // and no deeper, so the two behave the same in a test.
                    let Some(rest) = strip_prefix(path, &prefix) else { return false };
                    !rest.contains('/')
                })
                .map(|(path, entry)| Metadata {
                    path: path.clone(),
                    size: entry.contents.len() as u64,
                    last_modified: Some(entry.last_modified),
                })
                .collect();

            out.sort_by(|a, b| a.path.cmp(&b.path));
            Ok(out)
        })
    }
}

impl std::fmt::Debug for MemoryFilesystem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryFilesystem").field("files", &self.len()).finish()
    }
}

/// The part of `path` after `prefix`, or `None` if it is not under it.
fn strip_prefix<'a>(path: &'a str, prefix: &str) -> Option<&'a str> {
    if prefix.is_empty() {
        return Some(path);
    }
    path.strip_prefix(prefix)?.strip_prefix('/')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filesystem::FilesystemExt;

    #[tokio::test]
    async fn a_file_round_trips() {
        let fs = MemoryFilesystem::new();
        fs.put_string("a/b.txt", "hello").await.unwrap();

        assert_eq!(fs.get_string("a/b.txt").await.unwrap().as_deref(), Some("hello"));
        assert!(fs.exists("a/b.txt").await.unwrap());
        assert_eq!(fs.size("a/b.txt").await.unwrap(), Some(5));
        assert_eq!(fs.len(), 1);
    }

    #[tokio::test]
    async fn a_missing_file_reads_as_none() {
        let fs = MemoryFilesystem::new();

        assert_eq!(fs.get("absent.txt").await.unwrap(), None);
        assert!(!fs.exists("absent.txt").await.unwrap());
    }

    #[tokio::test]
    async fn deleting_reports_whether_it_was_there() {
        let fs = MemoryFilesystem::new();
        fs.put_string("f.txt", "x").await.unwrap();

        assert!(fs.delete("f.txt").await.unwrap());
        assert!(!fs.delete("f.txt").await.unwrap());
        assert!(fs.is_empty());
    }

    #[tokio::test]
    async fn paths_are_normalised_the_same_way_as_the_real_drivers() {
        // A test that passes here must pass against local and S3, so the same
        // rules apply — otherwise the double is more permissive than production.
        let fs = MemoryFilesystem::new();

        fs.put_string("/a//b.txt", "x").await.unwrap();
        assert_eq!(fs.paths(), vec!["a/b.txt"]);
        assert_eq!(fs.get_string("a/b.txt").await.unwrap().as_deref(), Some("x"));

        assert!(fs.put_string("../escape.txt", "x").await.is_err());
        assert!(fs.get("../escape.txt").await.is_err());
    }

    #[tokio::test]
    async fn listing_is_shallow_and_sorted() {
        let fs = MemoryFilesystem::new();
        fs.put_string("z.txt", "x").await.unwrap();
        fs.put_string("a.txt", "x").await.unwrap();
        fs.put_string("sub/deep.txt", "x").await.unwrap();
        fs.put_string("sub/deeper/deepest.txt", "x").await.unwrap();

        let root: Vec<String> = fs.list("").await.unwrap().into_iter().map(|m| m.path).collect();
        assert_eq!(root, vec!["a.txt", "z.txt"]);

        let sub: Vec<String> = fs.list("sub").await.unwrap().into_iter().map(|m| m.path).collect();
        assert_eq!(sub, vec!["sub/deep.txt"], "one level only");
    }

    #[tokio::test]
    async fn a_prefix_is_matched_on_a_separator_not_a_substring() {
        // `subdirectory/x` must not be listed under the prefix `sub`.
        let fs = MemoryFilesystem::new();
        fs.put_string("sub/a.txt", "x").await.unwrap();
        fs.put_string("subdirectory/b.txt", "x").await.unwrap();

        let listed: Vec<String> =
            fs.list("sub").await.unwrap().into_iter().map(|m| m.path).collect();
        assert_eq!(listed, vec!["sub/a.txt"]);
    }

    #[tokio::test]
    async fn copying_and_moving_behave() {
        let fs = MemoryFilesystem::new();
        fs.put_string("a.txt", "content").await.unwrap();

        fs.copy("a.txt", "b.txt").await.unwrap();
        assert!(fs.exists("a.txt").await.unwrap() && fs.exists("b.txt").await.unwrap());

        fs.rename("a.txt", "c.txt").await.unwrap();
        assert!(!fs.exists("a.txt").await.unwrap());
        assert_eq!(fs.get_string("c.txt").await.unwrap().as_deref(), Some("content"));
    }

    #[tokio::test]
    async fn there_is_no_url() {
        // Nothing serves this, so a URL would 404.
        assert_eq!(MemoryFilesystem::new().url("a.txt"), None);
    }

    #[tokio::test]
    async fn the_port_is_object_safe() {
        let fs: std::sync::Arc<dyn Filesystem> = std::sync::Arc::new(MemoryFilesystem::new());

        fs.put_string("k.txt", "v").await.unwrap();
        assert_eq!(fs.get_string("k.txt").await.unwrap().as_deref(), Some("v"));
    }
}
