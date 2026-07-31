//! [`DynamoDbCache`] — the [`Cache`] port over a [`DynamoDbClient`].

use std::time::Duration;

use chrono::Utc;
use rainier_drivers::{AwsConnector, DynamoDbClient};
use rainier_support::{BoxFuture, Error, Result};

use crate::cache::Cache;

/// A cache in a DynamoDB table.
///
/// An **adapter**: the DynamoDB knowledge — the item shape, the attribute names,
/// that its TTL sweep lags by up to 48 hours — lives in
/// [`rainier-drivers`](rainier_drivers). What is decided here is cache policy:
/// that a lagging expiry reads as a **miss**, and that flushing is refused.
///
/// The reason to reach for it: **no server to run**. A managed table that scales
/// itself and expires its own rows suits a Lambda or Fargate task with nowhere
/// convenient to put a Redis. The reason not to: it is a database being used as a
/// cache, billed per request, with none of Redis's data structures.
///
/// See [`DynamoDbClient`] for the table it expects.
pub struct DynamoDbCache {
    client: DynamoDbClient,
}

impl DynamoDbCache {
    /// A cache in `table`.
    pub fn new(connector: &AwsConnector, table: impl Into<String>) -> Self {
        Self { client: DynamoDbClient::new(connector, table) }
    }

    /// Use a client you already have.
    pub fn with_client(client: DynamoDbClient) -> Self {
        Self { client }
    }

    /// The table name.
    pub fn table(&self) -> &str {
        self.client.table()
    }

    /// The client, for an operation this port does not expose.
    pub fn client(&self) -> &DynamoDbClient {
        &self.client
    }

    /// A TTL as the absolute expiry DynamoDB stores.
    fn expires_at(ttl: Option<Duration>) -> Option<i64> {
        Some(now() + ttl?.as_secs().max(1) as i64)
    }
}

impl Cache for DynamoDbCache {
    fn name(&self) -> &str {
        "dynamodb"
    }

    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Option<Vec<u8>>>> {
        Box::pin(async move {
            let Some(item) = self.client.get(key).await? else { return Ok(None) };

            // The policy decision this adapter exists to make: DynamoDB's own
            // sweep is up to 48 hours late, so an expired row is very often still
            // there. Returning its value would hand back something the writer
            // asked to have expired — a one-minute cache serving two-day-old
            // values.
            if item.is_expired(now()) {
                return Ok(None);
            }

            Ok(item.value)
        })
    }

    fn put<'a>(
        &'a self,
        key: &'a str,
        value: &'a [u8],
        ttl: Option<Duration>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { self.client.put(key, value, Self::expires_at(ttl)).await })
    }

    fn forget<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move { self.client.delete(key).await })
    }

    fn flush(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            // Refused rather than attempted. Emptying a DynamoDB table means
            // scanning it and deleting every item — unbounded in time and cost,
            // and not atomic. What actually does it is DeleteTable followed by
            // CreateTable, which is an infrastructure change rather than
            // something a cache method should perform.
            Err(Error::internal(format!(
                "a DynamoDB cache cannot flush: emptying `{}` means scanning and deleting \
                 every item, which is unbounded in time and cost. Delete and recreate the \
                 table, or forget the keys you know about.",
                self.table()
            )))
        })
    }

    fn increment<'a>(&'a self, key: &'a str, by: i64) -> BoxFuture<'a, Result<i64>> {
        Box::pin(async move { self.client.add_to_counter(key, by).await })
    }

    fn add<'a>(
        &'a self,
        key: &'a str,
        value: &'a [u8],
        ttl: Option<Duration>,
    ) -> BoxFuture<'a, Result<bool>> {
        // A conditional `PutItem`: DynamoDB evaluates the condition as part of
        // the write, so two callers racing produce exactly one winner.
        //
        // `now` is passed in rather than read inside the driver because the
        // condition also treats an expired row as absent — DynamoDB's own TTL
        // sweep is up to 48 hours late, and without that a released lock whose
        // row has not been swept yet could never be taken again.
        Box::pin(async move {
            let now = Utc::now().timestamp();
            self.client.put_if_absent(key, value, Self::expires_at(ttl), now).await
        })
    }

    fn forget_if<'a>(&'a self, key: &'a str, expected: &'a [u8]) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move { self.client.delete_if(key, expected).await })
    }
}

impl std::fmt::Debug for DynamoDbCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DynamoDbCache").field("table", &self.table()).finish()
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn cache() -> DynamoDbCache {
        DynamoDbCache::new(
            &AwsConnector::with_credentials("id", "secret", "us-east-1").await,
            "rainier_cache",
        )
    }

    #[tokio::test]
    async fn the_driver_is_named() {
        let cache = cache().await;
        assert_eq!(cache.name(), "dynamodb");
        assert_eq!(cache.table(), "rainier_cache");
    }

    #[test]
    fn a_ttl_becomes_an_absolute_expiry_in_the_future() {
        let expires = DynamoDbCache::expires_at(Some(Duration::from_secs(60))).unwrap();

        assert!(expires > now());
        assert!(expires <= now() + 61);
    }

    #[test]
    fn a_sub_second_ttl_still_expires_rather_than_never() {
        assert!(DynamoDbCache::expires_at(Some(Duration::from_millis(1))).unwrap() > now());
    }

    #[test]
    fn no_ttl_means_no_expiry_attribute() {
        assert_eq!(DynamoDbCache::expires_at(None), None);
    }

    #[tokio::test]
    async fn flushing_is_refused_with_a_reason() {
        let err = cache().await.flush().await.unwrap_err();

        assert!(err.message().contains("cannot flush"), "{}", err.message());
        assert!(err.message().contains("rainier_cache"), "{}", err.message());
    }

    // The operations need a live table. Run with:
    //   cargo test -p rainier-cache --features dynamodb -- --ignored
    // against a table named by RAINIER_TEST_TABLE.
    #[tokio::test]
    #[ignore = "needs a live DynamoDB table"]
    async fn a_value_round_trips() {
        use crate::cache::CacheExt;

        let table = std::env::var("RAINIER_TEST_TABLE").expect("RAINIER_TEST_TABLE");
        let cache = DynamoDbCache::new(&AwsConnector::from_env().await, table);

        cache.put_string("k", "v", Some(Duration::from_secs(60))).await.unwrap();
        assert_eq!(cache.get_string("k").await.unwrap().as_deref(), Some("v"));

        assert!(cache.forget("k").await.unwrap());
        assert!(!cache.forget("k").await.unwrap());
    }

    #[tokio::test]
    #[ignore = "needs a live DynamoDB table"]
    async fn an_expired_row_reads_as_absent_before_dynamodb_sweeps_it() {
        use crate::cache::CacheExt;

        // The behaviour the read-time filter exists for.
        let table = std::env::var("RAINIER_TEST_TABLE").expect("RAINIER_TEST_TABLE");
        let cache = DynamoDbCache::new(&AwsConnector::from_env().await, table);

        cache.put_string("brief", "v", Some(Duration::from_secs(1))).await.unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;

        assert_eq!(cache.get("brief").await.unwrap(), None);
        cache.forget("brief").await.unwrap();
    }
}
