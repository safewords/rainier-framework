//! S3 — the service interface.
//!
//! Everything that knows what S3 *is*: its operations, how absence is reported,
//! how a listing paginates, what a directory marker looks like. No filesystem
//! semantics — this module has never heard of a path, a traversal, or a URL
//! prefix.

use std::time::Duration;

use aws_sdk_s3::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_s3::presigning::PresigningConfig;
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

    /// The SDK client, for an operation this does not expose — a multipart
    /// upload, object tagging, a presigned `PUT`.
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

    /// The **common prefixes** directly under `prefix` — S3's answer to "what
    /// directories are in here".
    ///
    /// A bucket has no directories; it has keys with slashes in them. Asking for
    /// them is `delimiter("/")` again, but reading a different half of the
    /// response: [`list`](Self::list) takes `contents` and this takes
    /// `common_prefixes`, which is the set of distinct next segments S3 rolled
    /// the deeper keys up into. Deriving the same set by listing every key
    /// underneath and cutting at the first slash would work and would download
    /// the whole subtree to do it.
    ///
    /// The trailing delimiter is stripped, so what comes back is a prefix that
    /// can be handed straight back to [`list`](Self::list) rather than one that
    /// has to be trimmed at every call site.
    ///
    /// Paginated to the end, for the same reason `list` is: a prefix with more
    /// children than fit in a page would otherwise silently answer with the
    /// first page and look complete.
    pub async fn list_prefixes(&self, prefix: &str) -> Result<Vec<String>> {
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

            for common in page.common_prefixes() {
                let Some(found) = common.prefix() else { continue };
                out.push(found.strip_suffix('/').unwrap_or(found).to_string());
            }
        }

        Ok(out)
    }

    /// A **presigned** `GET` URL for one object, good for `expires_in`.
    ///
    /// The SDK's presigner, not a signature assembled here. That matters beyond
    /// not rewriting SigV4: the signing credential comes from the same provider
    /// every other call uses, so a temporary one — an instance role, a task
    /// role, IRSA — is picked up *and* carries its `X-Amz-Security-Token`. A
    /// hand-rolled signer that omits that token produces a URL which validates
    /// as a signature and is then rejected as an unknown key, which reads as
    /// "presigning is broken" rather than "the token is missing".
    ///
    /// Nothing is sent: the URL is computed locally from the credential, so this
    /// costs no request and works while the object does not yet exist.
    ///
    /// SigV4 caps a query-string signature at **seven days**, and the SDK
    /// refuses to build a longer one. Refused rather than silently clamped: a
    /// link that expires six days before the caller asked is worse than being
    /// told the ask was impossible.
    pub async fn presigned_get_url(&self, key: &str, expires_in: Duration) -> Result<String> {
        let config = PresigningConfig::expires_in(expires_in).map_err(|_| {
            Error::bad_request(format!(
                "a presigned URL cannot last {} seconds; SigV4 caps a signed URL at 7 days",
                expires_in.as_secs()
            ))
        })?;

        let request = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .presigned(config)
            .await
            .map_err(|e| sdk_error(&format!("S3 presign get_object `{key}`"), e))?;

        Ok(request.uri().to_string())
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

    #[tokio::test]
    async fn a_presigned_url_is_built_locally_and_carries_its_own_expiry() {
        // No network: the signature is computed from the credential, which is
        // why this can be asserted on at all.
        let url =
            client().await.presigned_get_url("a/b.txt", Duration::from_secs(900)).await.unwrap();

        assert!(url.starts_with("https://my-bucket.s3.us-east-1.amazonaws.com/a/b.txt"), "{url}");
        assert!(url.contains("X-Amz-Expires=900"), "{url}");
        assert!(url.contains("X-Amz-Signature="), "{url}");
        assert!(url.contains("X-Amz-Algorithm=AWS4-HMAC-SHA256"), "{url}");
    }

    #[tokio::test]
    async fn a_presigned_url_beyond_seven_days_is_refused_rather_than_clamped() {
        // A link that expires six days before the caller asked is worse than
        // being told the ask was impossible.
        let error = client()
            .await
            .presigned_get_url("a/b.txt", Duration::from_secs(8 * 24 * 60 * 60))
            .await
            .unwrap_err();

        assert_eq!(error.status(), 400);
        assert!(error.message().contains("7 days"), "{}", error.message());
    }

    #[test]
    fn a_listed_object_carries_what_a_caller_needs() {
        let object =
            S3Object { key: "a/b.txt".to_string(), size: 12, last_modified: Some(1_705_314_600) };

        assert_eq!(object.key, "a/b.txt");
        assert_eq!(object.size, 12);
    }
}
