//! AWS transport — the shared configuration S3, SQS and DynamoDB are built
//! from.
//!
//! This wraps the **official AWS SDK** rather than signing requests itself. An
//! earlier version of this module hand-rolled SigV4, which worked and was
//! shorter, and was wrong in a way that only shows up in production: it could
//! only read static credentials from environment variables.
//!
//! Real AWS workloads almost never use those. They use an EC2 instance role, an
//! ECS task role, EKS IRSA, SSO, or a profile in `~/.aws/config` — and every one
//! of those needs a credential *provider* that discovers, caches and **refreshes**
//! a temporary credential before it expires. That is the part of the SDK worth
//! having, and reimplementing it would be reimplementing the interesting half.
//!
//! What this module keeps is the reason it exists: credentials are resolved
//! **once** and the same configuration builds every service client, so an
//! application does not configure AWS three times.

use std::sync::Arc;

use rainier_support::{Error, ErrorKind};

/// Where to reach AWS, and how to authenticate.
///
/// Built from [`AwsConnector::from_env`] in almost every case: the SDK's default
/// chain already looks in the right places, in the right order, and refreshes
/// what needs refreshing.
#[derive(Clone)]
pub struct AwsConnector {
    config: Arc<aws_config::SdkConfig>,
    /// Set when the endpoint was overridden, for diagnostics and for the
    /// path-style decision S3 needs.
    endpoint: Option<String>,
    force_path_style: bool,
}

impl AwsConnector {
    /// Resolve credentials and region the way every AWS tool does.
    ///
    /// The default provider chain, in order: environment variables, a web
    /// identity token (EKS IRSA), the SSO cache, `~/.aws/credentials` and
    /// `~/.aws/config` profiles, the ECS container credential endpoint, and
    /// finally EC2 instance metadata.
    ///
    /// Temporary credentials from any of those are **refreshed automatically**,
    /// which is the thing a hand-rolled signer gets wrong: a task role's
    /// credential expires within hours, and a process that cached it at boot
    /// starts failing with `403` in the middle of the night.
    pub async fn from_env() -> Self {
        let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
        Self { config: Arc::new(config), endpoint: None, force_path_style: false }
    }

    /// The same, pinned to a region.
    pub async fn in_region(region: impl Into<String>) -> Self {
        let region = aws_config::Region::new(region.into());
        let config =
            aws_config::defaults(aws_config::BehaviorVersion::latest()).region(region).load().await;

        Self { config: Arc::new(config), endpoint: None, force_path_style: false }
    }

    /// Explicit credentials, for a service that is not AWS.
    ///
    /// Cloudflare R2, MinIO and Backblaze B2 issue their own key pairs and have
    /// no credential chain to discover, so this is the honest way to reach them.
    /// Prefer [`from_env`](Self::from_env) for AWS itself.
    pub async fn with_credentials(
        access_key_id: impl Into<String>,
        secret_access_key: impl Into<String>,
        region: impl Into<String>,
    ) -> Self {
        let credentials = aws_credential_types::Credentials::new(
            access_key_id.into(),
            secret_access_key.into(),
            None,
            None,
            "rainier-explicit",
        );

        let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new(region.into()))
            .credentials_provider(credentials)
            .load()
            .await;

        Self { config: Arc::new(config), endpoint: None, force_path_style: false }
    }

    /// Build from an SDK configuration you assembled yourself.
    ///
    /// The escape hatch: anything the SDK can be configured to do that this
    /// wrapper does not expose is reachable this way, without forking it.
    pub fn from_sdk_config(config: aws_config::SdkConfig) -> Self {
        Self { config: Arc::new(config), endpoint: None, force_path_style: false }
    }

    /// Talk to something other than AWS.
    ///
    /// R2: `https://<account>.r2.cloudflarestorage.com` with region `auto`.
    /// MinIO: wherever it runs, with [`path_style`](Self::path_style).
    pub fn endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = Some(endpoint.into().trim_end_matches('/').to_string());
        self
    }

    /// Put the bucket in the path rather than the host.
    ///
    /// Needed by MinIO and by anything reached over a bare IP, where a bucket
    /// as a subdomain has nowhere to resolve to.
    pub fn path_style(mut self, path_style: bool) -> Self {
        self.force_path_style = path_style;
        self
    }

    /// The SDK configuration.
    pub fn sdk_config(&self) -> &aws_config::SdkConfig {
        &self.config
    }

    /// The region, if one was resolved.
    pub fn region(&self) -> Option<&str> {
        self.config.region().map(|region| region.as_ref())
    }

    /// The endpoint override, if any.
    pub fn endpoint_url(&self) -> Option<&str> {
        self.endpoint.as_deref()
    }

    /// Whether S3 addressing is path-style.
    pub fn is_path_style(&self) -> bool {
        self.force_path_style || self.endpoint.is_some()
    }

    /// An S3 client.
    ///
    /// Path-style addressing is turned on automatically when an endpoint is
    /// overridden: a fixed endpoint host has nowhere to put a bucket subdomain,
    /// and forgetting the flag is the usual reason R2 and MinIO "do not work".
    #[cfg(feature = "aws-s3")]
    pub fn s3(&self) -> aws_sdk_s3::Client {
        let mut builder = aws_sdk_s3::config::Builder::from(self.config.as_ref());

        if let Some(endpoint) = &self.endpoint {
            builder = builder.endpoint_url(endpoint);
        }
        if self.is_path_style() {
            builder = builder.force_path_style(true);
        }

        aws_sdk_s3::Client::from_conf(builder.build())
    }

    /// An SQS client.
    #[cfg(feature = "aws-sqs")]
    pub fn sqs(&self) -> aws_sdk_sqs::Client {
        let mut builder = aws_sdk_sqs::config::Builder::from(self.config.as_ref());
        if let Some(endpoint) = &self.endpoint {
            builder = builder.endpoint_url(endpoint);
        }
        aws_sdk_sqs::Client::from_conf(builder.build())
    }

    /// A DynamoDB client.
    #[cfg(feature = "aws-dynamodb")]
    pub fn dynamodb(&self) -> aws_sdk_dynamodb::Client {
        let mut builder = aws_sdk_dynamodb::config::Builder::from(self.config.as_ref());
        if let Some(endpoint) = &self.endpoint {
            builder = builder.endpoint_url(endpoint);
        }
        aws_sdk_dynamodb::Client::from_conf(builder.build())
    }
}

impl std::fmt::Debug for AwsConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // No credentials: the SDK holds them behind its own provider, and this
        // type never sees them, which is one fewer place they can be logged.
        f.debug_struct("AwsConnector")
            .field("region", &self.region())
            .field("endpoint", &self.endpoint)
            .field("path_style", &self.is_path_style())
            .finish()
    }
}

/// Turn an SDK error into a framework one.
///
/// Two decisions worth stating, because both affect how an outage looks on a
/// dashboard:
///
/// - **Throttling and 5xx are [`ServiceUnavailable`]**, not `Internal`. They are
///   retryable and somebody's to page about; they are not bugs in the request
///   that happened to hit them.
/// - **The message is the SDK's own code**, not its `Display`, which for some
///   errors includes the request URL and therefore the bucket or table name in a
///   context where that is noise.
///
/// [`ServiceUnavailable`]: rainier_support::ErrorKind::ServiceUnavailable
pub fn sdk_error<E, R>(action: &str, error: aws_sdk_s3::error::SdkError<E, R>) -> Error
where
    E: std::error::Error + aws_sdk_s3::error::ProvideErrorMetadata,
{
    use aws_sdk_s3::error::SdkError;

    let (kind, detail) = match &error {
        // Never reached the service: no credentials, no network, no DNS.
        SdkError::ConstructionFailure(_) => {
            (ErrorKind::Internal, "the request could not be built".to_string())
        }
        SdkError::TimeoutError(_) => (ErrorKind::ServiceUnavailable, "timed out".to_string()),
        SdkError::DispatchFailure(_) => {
            (ErrorKind::ServiceUnavailable, "could not be sent".to_string())
        }
        SdkError::ResponseError(_) => {
            (ErrorKind::ServiceUnavailable, "returned an unreadable response".to_string())
        }
        SdkError::ServiceError(service) => {
            let code = service.err().code().unwrap_or("an unrecognised error").to_string();
            let kind = match code.as_str() {
                "AccessDenied"
                | "AccessDeniedException"
                | "InvalidAccessKeyId"
                | "SignatureDoesNotMatch" => ErrorKind::Unauthorized,
                "NoSuchKey"
                | "NotFound"
                | "ResourceNotFoundException"
                | "AWS.SimpleQueueService.NonExistentQueue" => ErrorKind::NotFound,
                "SlowDown"
                | "ThrottlingException"
                | "RequestThrottled"
                | "ProvisionedThroughputExceededException"
                | "RequestLimitExceeded" => ErrorKind::ServiceUnavailable,
                _ => ErrorKind::Internal,
            };
            (kind, code)
        }
        _ => (ErrorKind::Internal, "failed".to_string()),
    };

    Error::new(kind, format!("AWS {action}: {detail}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn explicit_credentials_produce_a_usable_configuration() {
        // No network: `load` resolves providers but does not call them.
        let connector = AwsConnector::with_credentials("id", "secret", "eu-west-2").await;

        assert_eq!(connector.region(), Some("eu-west-2"));
        assert!(!connector.is_path_style());
        assert_eq!(connector.endpoint_url(), None);
    }

    #[tokio::test]
    async fn an_endpoint_override_turns_on_path_style() {
        // The usual reason R2 and MinIO "do not work" is forgetting this, so it
        // is not something the caller has to remember.
        let connector = AwsConnector::with_credentials("id", "secret", "auto")
            .await
            .endpoint("https://account.r2.cloudflarestorage.com/");

        assert_eq!(
            connector.endpoint_url(),
            Some("https://account.r2.cloudflarestorage.com"),
            "a trailing slash is trimmed"
        );
        assert!(connector.is_path_style());
    }

    #[tokio::test]
    async fn path_style_can_be_forced_without_an_endpoint() {
        let connector =
            AwsConnector::with_credentials("id", "secret", "us-east-1").await.path_style(true);

        assert!(connector.is_path_style());
    }

    #[tokio::test]
    async fn a_region_can_be_pinned() {
        assert_eq!(
            AwsConnector::in_region("ap-southeast-2").await.region(),
            Some("ap-southeast-2")
        );
    }

    #[tokio::test]
    async fn debug_holds_no_credentials_to_disclose() {
        // The SDK owns them; this type never sees them.
        let connector =
            AwsConnector::with_credentials("AKIA-visible", "super-secret", "us-east-1").await;
        let rendered = format!("{connector:?}");

        assert!(!rendered.contains("super-secret"), "{rendered}");
        assert!(!rendered.contains("AKIA-visible"), "{rendered}");
        assert!(rendered.contains("us-east-1"));
    }

    #[cfg(feature = "aws-s3")]
    #[tokio::test]
    async fn every_service_client_builds_from_one_configuration() {
        // The reason this module exists: credentials resolved once, shared.
        let connector = AwsConnector::with_credentials("id", "secret", "us-east-1").await;

        let _ = connector.s3();
        #[cfg(feature = "aws-sqs")]
        let _ = connector.sqs();
        #[cfg(feature = "aws-dynamodb")]
        let _ = connector.dynamodb();
    }
}
