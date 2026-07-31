//! [`S3Filesystem`] — the [`Filesystem`] port over an [`S3Client`].

use bytes::Bytes;
use chrono::DateTime;
use rainier_drivers::{AwsConnector, S3Client};
use rainier_support::{BoxFuture, Error, Result};

use crate::filesystem::{normalise_path, normalise_prefix, Filesystem, Metadata};

/// Files in an S3-compatible bucket.
///
/// An **adapter**: the S3 knowledge — its operations, that absence is reported
/// two different ways, how a listing paginates — lives in
/// [`rainier-drivers`](rainier_drivers). What is decided here is filesystem
/// policy: path normalisation, what a content type may be, and that a missing
/// object is `Ok(None)`.
///
/// **S3-compatible is not a special case.** R2, MinIO, B2 and Wasabi differ only
/// in their endpoint, which is [`AwsConnector::endpoint`]'s business.
///
/// ```no_run
/// use rainier_drivers::AwsConnector;
/// use rainier_filesystem::S3Filesystem;
///
/// # async fn run() -> rainier_support::Result<()> {
/// // S3, with credentials from the default provider chain.
/// let s3 = S3Filesystem::new(&AwsConnector::from_env().await, "my-bucket");
///
/// // Cloudflare R2: the same driver, an explicit key pair and an endpoint.
/// let r2 = S3Filesystem::new(
///     &AwsConnector::with_credentials("id", "secret", "auto")
///         .await
///         .endpoint("https://account.r2.cloudflarestorage.com"),
///     "my-bucket",
/// );
/// # let _ = (s3, r2); Ok(()) }
/// ```
pub struct S3Filesystem {
    client: S3Client,
    url_prefix: Option<String>,
}

impl S3Filesystem {
    /// Files in `bucket`.
    pub fn new(connector: &AwsConnector, bucket: impl Into<String>) -> Self {
        Self::with_client(S3Client::new(connector, bucket))
    }

    /// Use a client you already have.
    pub fn with_client(client: S3Client) -> Self {
        Self { client, url_prefix: None }
    }

    /// The public URL prefix objects are reachable at.
    ///
    /// A CloudFront distribution, an R2 custom domain, or a bucket with public
    /// read. Without it [`url`](Filesystem::url) is `None`: a private bucket's
    /// object URL answers `403`, and a link that fails is worse than no link.
    pub fn with_url_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.url_prefix = Some(prefix.into().trim_end_matches('/').to_string());
        self
    }

    /// The bucket name.
    pub fn bucket(&self) -> &str {
        self.client.bucket()
    }

    /// The client, for an operation this port does not expose — a presigned URL,
    /// a multipart upload, object tagging.
    pub fn client(&self) -> &S3Client {
        &self.client
    }
}

impl Filesystem for S3Filesystem {
    fn name(&self) -> &str {
        "s3"
    }

    fn get<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<Option<Bytes>>> {
        Box::pin(async move {
            let key = normalise_path(path)?;
            Ok(self.client.get(&key).await?.map(Bytes::from))
        })
    }

    fn put<'a>(&'a self, path: &'a str, contents: Bytes) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let key = normalise_path(path)?;
            self.client.put(&key, contents.to_vec(), guess_content_type(&key)).await
        })
    }

    fn delete<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            let key = normalise_path(path)?;

            // S3 cannot report whether the object was there, so "was it" needs
            // its own look. One extra request buys a `delete` that means the same
            // thing as the local driver's.
            let existed = self.client.head(&key).await?.is_some();
            self.client.delete(&key).await?;

            Ok(existed)
        })
    }

    fn exists<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move { Ok(self.metadata(path).await?.is_some()) })
    }

    fn metadata<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<Option<Metadata>>> {
        Box::pin(async move {
            let key = normalise_path(path)?;

            Ok(self.client.head(&key).await?.map(|head| Metadata {
                path: key,
                size: head.size,
                last_modified: head
                    .last_modified
                    .and_then(|secs| DateTime::from_timestamp(secs, 0)),
            }))
        })
    }

    fn list<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, Result<Vec<Metadata>>> {
        Box::pin(async move {
            let prefix = normalise_prefix(prefix)?;
            let search = if prefix.is_empty() { String::new() } else { format!("{prefix}/") };

            let mut out: Vec<Metadata> = self
                .client
                .list(&search)
                .await?
                .into_iter()
                .map(|object| Metadata {
                    path: object.key,
                    size: object.size,
                    last_modified: object
                        .last_modified
                        .and_then(|secs| DateTime::from_timestamp(secs, 0)),
                })
                .collect();

            // Sorted, so a listing is reproducible — matching the local driver.
            out.sort_by(|a, b| a.path.cmp(&b.path));
            Ok(out)
        })
    }

    fn copy<'a>(&'a self, from: &'a str, to: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let source = normalise_path(from)?;
            let target = normalise_path(to)?;

            if self.client.copy(&source, &target).await? {
                Ok(())
            } else {
                Err(Error::not_found(format!("`{from}` does not exist")))
            }
        })
    }

    fn url(&self, path: &str) -> Option<String> {
        let prefix = self.url_prefix.as_ref()?;
        let key = normalise_path(path).ok()?;
        Some(format!("{prefix}/{}", encode_path(&key)))
    }
}

impl std::fmt::Debug for S3Filesystem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Filesystem").field("bucket", &self.bucket()).finish()
    }
}

/// Percent-encode a key for a public URL, keeping the separators.
fn encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// A content type from the extension.
///
/// Guessed rather than sniffed, and deliberately conservative: the type is what a
/// browser will trust, so anything unrecognised becomes
/// `application/octet-stream` — which downloads rather than renders, and cannot
/// be turned into stored XSS by a file named `.html`.
fn guess_content_type(key: &str) -> &'static str {
    match key.rsplit('.').next().map(str::to_ascii_lowercase).as_deref() {
        Some("txt") => "text/plain; charset=utf-8",
        Some("csv") => "text/csv; charset=utf-8",
        Some("json") => "application/json",
        Some("pdf") => "application/pdf",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("zip") => "application/zip",
        Some("mp4") => "video/mp4",
        // `svg` is deliberately absent: an SVG is a document that can carry
        // script, so serving one as `image/svg+xml` from a bucket users upload
        // to is stored XSS.
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn s3() -> S3Filesystem {
        S3Filesystem::new(
            &AwsConnector::with_credentials("id", "secret", "us-east-1").await,
            "my-bucket",
        )
    }

    #[tokio::test]
    async fn the_driver_is_named() {
        let s3 = s3().await;
        assert_eq!(s3.name(), "s3");
        assert_eq!(s3.bucket(), "my-bucket");
    }

    #[tokio::test]
    async fn a_traversal_is_refused_before_any_request() {
        let s3 = s3().await;

        assert!(s3.get("../other-bucket/secret").await.is_err());
        assert!(s3.put("../escape", Bytes::from_static(b"x")).await.is_err());
        assert!(s3.delete("a/../../b").await.is_err());
        assert!(s3.list("../..").await.is_err());
    }

    #[tokio::test]
    async fn there_is_no_url_without_a_prefix() {
        // A private bucket's object URL is a 403, so no link is better.
        assert_eq!(s3().await.url("a.txt"), None);

        let served = s3().await.with_url_prefix("https://cdn.example.com/");
        assert_eq!(served.url("a/b c.txt").as_deref(), Some("https://cdn.example.com/a/b%20c.txt"));
    }

    #[tokio::test]
    async fn a_url_is_not_produced_for_a_path_that_would_be_refused() {
        let served = s3().await.with_url_prefix("https://cdn.example.com");
        assert_eq!(served.url("../secrets"), None);
    }

    #[test]
    fn an_unknown_extension_downloads_rather_than_renders() {
        // A file named `.html` served as text/html is stored XSS.
        assert_eq!(guess_content_type("evil.html"), "application/octet-stream");
        assert_eq!(guess_content_type("script.js"), "application/octet-stream");
        assert_eq!(guess_content_type("noextension"), "application/octet-stream");

        // An SVG can carry script, so it stays out of the list too.
        assert_eq!(guess_content_type("logo.svg"), "application/octet-stream");

        assert_eq!(guess_content_type("photo.PNG"), "image/png", "case-insensitive");
        assert_eq!(guess_content_type("a/b/report.pdf"), "application/pdf");
    }

    #[test]
    fn a_key_is_encoded_for_a_url_but_keeps_its_separators() {
        assert_eq!(encode_path("a/b c.txt"), "a/b%20c.txt");
        assert_eq!(encode_path("emoji/🎉.png"), "emoji/%F0%9F%8E%89.png");
        assert_eq!(encode_path("plain.txt"), "plain.txt");
    }

    // The operations need a bucket. Run with:
    //   cargo test -p rainier-filesystem --features s3 -- --ignored
    // against a bucket named by RAINIER_TEST_BUCKET.
    #[tokio::test]
    #[ignore = "needs a live bucket"]
    async fn a_file_round_trips() {
        use crate::filesystem::FilesystemExt;

        let bucket = std::env::var("RAINIER_TEST_BUCKET").expect("RAINIER_TEST_BUCKET");
        let fs = S3Filesystem::new(&AwsConnector::from_env().await, bucket);

        fs.put_string("rainier-test/a.txt", "hello").await.unwrap();

        assert_eq!(fs.get_string("rainier-test/a.txt").await.unwrap().as_deref(), Some("hello"));
        assert_eq!(fs.size("rainier-test/a.txt").await.unwrap(), Some(5));

        fs.copy("rainier-test/a.txt", "rainier-test/b.txt").await.unwrap();

        let listed: Vec<String> =
            fs.list("rainier-test").await.unwrap().into_iter().map(|m| m.path).collect();
        assert_eq!(listed, vec!["rainier-test/a.txt", "rainier-test/b.txt"]);

        assert!(fs.delete("rainier-test/a.txt").await.unwrap());
        assert!(!fs.delete("rainier-test/a.txt").await.unwrap());
        assert!(fs.delete("rainier-test/b.txt").await.unwrap());
    }

    #[tokio::test]
    #[ignore = "needs a live bucket"]
    async fn a_missing_object_reads_as_none() {
        let bucket = std::env::var("RAINIER_TEST_BUCKET").expect("RAINIER_TEST_BUCKET");
        let fs = S3Filesystem::new(&AwsConnector::from_env().await, bucket);

        assert_eq!(fs.get("rainier-test/definitely-absent").await.unwrap(), None);
        assert!(!fs.exists("rainier-test/definitely-absent").await.unwrap());
        assert_eq!(fs.metadata("rainier-test/definitely-absent").await.unwrap(), None);
    }
}
