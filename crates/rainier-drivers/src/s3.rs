//! S3 — the service interface.
//!
//! Everything that knows what S3 *is*: its operations, how absence is reported,
//! how a listing paginates, what a directory marker looks like. No filesystem
//! semantics — this module has never heard of a path, a traversal, or a URL
//! prefix.

use aws_sdk_s3::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_s3::Client;
use rainier_support::{Error, Result};

use crate::aws::{sdk_error, AwsConnector};

/// One object as a listing reported it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3Object {
    /// The full key.
    pub key: String,
    /// Size in bytes.
    pub size: u64,
    /// When it was last written, as seconds since the epoch.
    pub last_modified: Option<i64>,
}

/// What a `HEAD` reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3Head {
    /// Size in bytes.
    pub size: u64,
    /// When it was last written, as seconds since the epoch.
    pub last_modified: Option<i64>,
}

/// Talks to one S3 bucket.
///
/// Works against S3, Cloudflare R2, MinIO, B2 and Wasabi without knowing which:
/// they differ only in endpoint and addressing, and both are
/// [`AwsConnector`]'s business.
#[derive(Clone)]
pub struct S3Client {
    client: Client,
    bucket: String,
}

impl S3Client {
    /// A client for `bucket`.
    pub fn new(connector: &AwsConnector, bucket: impl Into<String>) -> Self {
        Self { client: connector.s3(), bucket: bucket.into() }
    }

    /// Use an SDK client you built yourself.
    pub fn with_client(client: Client, bucket: impl Into<String>) -> Self {
        Self { client, bucket: bucket.into() }
    }

    /// The bucket name.
    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    /// The SDK client, for an operation this does not expose — a presigned URL,
    /// a multipart upload, object tagging.
    pub fn inner(&self) -> &Client {
        &self.client
    }

    /// Read an object. `None` if there is no such key.
    pub async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let output = match self.client.get_object().bucket(&self.bucket).key(key).send().await {
            Ok(output) => output,
            Err(e) if is_not_found(&e) => return Ok(None),
            Err(e) => return Err(sdk_error(&format!("S3 get_object `{key}`"), e)),
        };

        let bytes = output
            .body
            .collect()
            .await
            .map_err(|e| Error::internal(format!("could not read `{key}`: {e}")))?;

        Ok(Some(bytes.to_vec()))
    }

    /// Write an object.
    pub async fn put(&self, key: &str, body: Vec<u8>, content_type: &str) -> Result<()> {
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(body.into())
            .content_type(content_type)
            .send()
            .await
            .map_err(|e| sdk_error(&format!("S3 put_object `{key}`"), e))?;

        Ok(())
    }

    /// Delete an object.
    ///
    /// Returns nothing, because **S3 cannot tell you whether it was there**: it
    /// answers `204` either way. A caller that needs to know has to
    /// [`head`](Self::head) first.
    pub async fn delete(&self, key: &str) -> Result<()> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| sdk_error(&format!("S3 delete_object `{key}`"), e))?;

        Ok(())
    }

    /// An object's size and modification time, without its body.
    pub async fn head(&self, key: &str) -> Result<Option<S3Head>> {
        let output = match self.client.head_object().bucket(&self.bucket).key(key).send().await {
            Ok(output) => output,
            Err(e) if is_not_found(&e) => return Ok(None),
            Err(e) => return Err(sdk_error(&format!("S3 head_object `{key}`"), e)),
        };

        Ok(Some(S3Head {
            size: output.content_length().unwrap_or(0).max(0) as u64,
            last_modified: output.last_modified().map(|time| time.secs()),
        }))
    }

    /// Every object directly under `prefix`.
    ///
    /// Shallow: `delimiter("/")` stops it returning everything beneath the
    /// prefix. Paginated to the end, because a bucket can hold more than one page
    /// and stopping at the first would silently truncate the answer.
    ///
    /// Directory markers — keys ending in `/`, which some tools create — are
    /// **excluded**: they are not objects a caller can read.
    pub async fn list(&self, prefix: &str) -> Result<Vec<S3Object>> {
        let mut pages = self
            .client
            .list_objects_v2()
            .bucket(&self.bucket)
            .prefix(prefix)
            .delimiter("/")
            .into_paginator()
            .send();

        let mut out = Vec::new();
        while let Some(page) = pages.next().await {
            let page = page.map_err(|e| sdk_error(&format!("S3 list_objects `{prefix}`"), e))?;

            for object in page.contents() {
                let Some(key) = object.key() else { continue };
                if key.ends_with('/') {
                    continue;
                }

                out.push(S3Object {
                    key: key.to_string(),
                    size: object.size().unwrap_or(0).max(0) as u64,
                    last_modified: object.last_modified().map(|time| time.secs()),
                });
            }
        }

        Ok(out)
    }

    /// Copy an object **server-side**.
    ///
    /// `false` if the source did not exist. Server-side matters: the alternative
    /// is reading the object and writing it back, which for a large one is a great
    /// deal of pointless traffic through the calling process.
    pub async fn copy(&self, from: &str, to: &str) -> Result<bool> {
        match self
            .client
            .copy_object()
            .bucket(&self.bucket)
            .key(to)
            .copy_source(format!("{}/{from}", self.bucket))
            .send()
            .await
        {
            Ok(_) => Ok(true),
            Err(e) if is_not_found(&e) => Ok(false),
            Err(e) => Err(sdk_error(&format!("S3 copy_object `{from}`"), e)),
        }
    }
}

impl std::fmt::Debug for S3Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Client").field("bucket", &self.bucket).finish()
    }
}

/// Whether an SDK error means "no such object".
///
/// S3 reports absence two ways, and both have to be recognised: `GetObject`
/// answers `NoSuchKey`, while `HeadObject` answers a bare `NotFound` because a
/// `HEAD` has no body to put a richer code in.
pub fn is_not_found<E, R>(error: &SdkError<E, R>) -> bool
where
    E: ProvideErrorMetadata,
{
    match error {
        SdkError::ServiceError(service) => {
            matches!(service.err().code(), Some("NoSuchKey" | "NotFound" | "NoSuchBucket"))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn client() -> S3Client {
        S3Client::new(
            &AwsConnector::with_credentials("id", "secret", "us-east-1").await,
            "my-bucket",
        )
    }

    #[tokio::test]
    async fn it_remembers_its_bucket() {
        assert_eq!(client().await.bucket(), "my-bucket");
    }

    #[tokio::test]
    async fn an_r2_connector_produces_a_client_the_same_way() {
        // The point of keeping endpoint handling in the connector: nothing here
        // knows which service it is talking to.
        let r2 = AwsConnector::with_credentials("id", "secret", "auto")
            .await
            .endpoint("https://account.r2.cloudflarestorage.com");

        assert_eq!(S3Client::new(&r2, "bucket").bucket(), "bucket");
        assert!(r2.is_path_style(), "an endpoint override implies path style");
    }

    #[test]
    fn a_listed_object_carries_what_a_caller_needs() {
        let object =
            S3Object { key: "a/b.txt".to_string(), size: 12, last_modified: Some(1_705_314_600) };

        assert_eq!(object.key, "a/b.txt");
        assert_eq!(object.size, 12);
    }
}
