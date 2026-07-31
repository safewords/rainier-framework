//! DynamoDB — the service interface.
//!
//! Everything that knows what DynamoDB *is*: its item shape, its attribute
//! types, its TTL behaviour, its expression syntax. No cache semantics — this
//! module has never heard of a hit, a miss, or a driver name.

use aws_sdk_dynamodb::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_dynamodb::primitives::Blob;
use aws_sdk_dynamodb::types::{AttributeValue, ReturnValue};
use aws_sdk_dynamodb::Client;
use rainier_support::{Error, Result};

use crate::aws::{sdk_error, AwsConnector};

/// The attribute a key/value item is stored under.
pub const KEY_ATTRIBUTE: &str = "key";
/// The attribute a value is stored under.
pub const VALUE_ATTRIBUTE: &str = "value";
/// The attribute DynamoDB's TTL is configured on.
pub const TTL_ATTRIBUTE: &str = "expires_at";
/// The attribute a counter is stored under.
pub const COUNTER_ATTRIBUTE: &str = "counter";

/// A key/value item as DynamoDB returned it.
#[derive(Debug, Clone)]
pub struct DynamoItem {
    /// The stored bytes, if the item had a value.
    pub value: Option<Vec<u8>>,
    /// The expiry, if one was set.
    pub expires_at: Option<i64>,
}

impl DynamoItem {
    /// Whether this item has passed its expiry.
    ///
    /// Has to be asked, because **DynamoDB's TTL sweep lags by up to 48 hours**:
    /// an expired row is very often still there, and returning its value would
    /// hand back something the writer asked to have expired. Without this check
    /// a one-minute cache would serve stale values for two days.
    pub fn is_expired(&self, now: i64) -> bool {
        matches!(self.expires_at, Some(expires_at) if expires_at <= now)
    }
}

/// Talks to one DynamoDB table as a key/value store.
///
/// Service-shaped, not cache-shaped. The table it expects has one string
/// partition key named `key`, and TTL enabled on `expires_at`:
///
/// ```text
/// aws dynamodb create-table \
///   --table-name rainier_cache \
///   --attribute-definitions AttributeName=key,AttributeType=S \
///   --key-schema AttributeName=key,KeyType=HASH \
///   --billing-mode PAY_PER_REQUEST
///
/// aws dynamodb update-time-to-live \
///   --table-name rainier_cache \
///   --time-to-live-specification Enabled=true,AttributeName=expires_at
/// ```
#[derive(Clone)]
pub struct DynamoDbClient {
    client: Client,
    table: String,
}

impl DynamoDbClient {
    /// A client for `table`.
    pub fn new(connector: &AwsConnector, table: impl Into<String>) -> Self {
        Self { client: connector.dynamodb(), table: table.into() }
    }

    /// Use an SDK client you built yourself.
    pub fn with_client(client: Client, table: impl Into<String>) -> Self {
        Self { client, table: table.into() }
    }

    /// The table name.
    pub fn table(&self) -> &str {
        &self.table
    }

    /// The SDK client, for an operation this does not expose.
    pub fn inner(&self) -> &Client {
        &self.client
    }

    /// Read an item.
    ///
    /// **Strongly consistent.** An eventually consistent read can serve a value
    /// that was just deleted, which surfaces as a flickering bug nobody can
    /// reproduce — worth more than the latency it saves.
    pub async fn get(&self, key: &str) -> Result<Option<DynamoItem>> {
        let output = self
            .client
            .get_item()
            .table_name(&self.table)
            .key(KEY_ATTRIBUTE, AttributeValue::S(key.to_string()))
            .consistent_read(true)
            .send()
            .await
            .map_err(|e| sdk_error("DynamoDB get_item", e))?;

        let Some(item) = output.item() else { return Ok(None) };

        Ok(Some(DynamoItem {
            value: match item.get(VALUE_ATTRIBUTE) {
                Some(AttributeValue::B(blob)) => Some(blob.as_ref().to_vec()),
                _ => None,
            },
            expires_at: match item.get(TTL_ATTRIBUTE) {
                Some(AttributeValue::N(seconds)) => seconds.parse().ok(),
                _ => None,
            },
        }))
    }

    /// Write an item, with an optional absolute expiry.
    pub async fn put(&self, key: &str, value: &[u8], expires_at: Option<i64>) -> Result<()> {
        let mut request = self
            .client
            .put_item()
            .table_name(&self.table)
            .item(KEY_ATTRIBUTE, AttributeValue::S(key.to_string()))
            .item(VALUE_ATTRIBUTE, AttributeValue::B(Blob::new(value.to_vec())));

        if let Some(expires_at) = expires_at {
            request = request.item(TTL_ATTRIBUTE, AttributeValue::N(expires_at.to_string()));
        }

        request.send().await.map_err(|e| sdk_error("DynamoDB put_item", e))?;
        Ok(())
    }

    /// Write an item **only if** the key is absent — or present but expired.
    ///
    /// `true` if it was written. The condition is evaluated by DynamoDB as part
    /// of the write, so two callers racing produce exactly one winner.
    ///
    /// The `OR expires_at < now` half matters more than it looks. DynamoDB's
    /// TTL sweep runs up to **48 hours** late, so a lock whose expiry passed
    /// yesterday is very often still a row — and a condition of
    /// `attribute_not_exists` alone would refuse forever, turning a released
    /// lock into a permanently held one.
    pub async fn put_if_absent(
        &self,
        key: &str,
        value: &[u8],
        expires_at: Option<i64>,
        now: i64,
    ) -> Result<bool> {
        let mut request = self
            .client
            .put_item()
            .table_name(&self.table)
            .item(KEY_ATTRIBUTE, AttributeValue::S(key.to_string()))
            .item(VALUE_ATTRIBUTE, AttributeValue::B(Blob::new(value.to_vec())))
            .condition_expression("attribute_not_exists(#k) OR #ttl < :now")
            .expression_attribute_names("#k", KEY_ATTRIBUTE)
            .expression_attribute_names("#ttl", TTL_ATTRIBUTE)
            .expression_attribute_values(":now", AttributeValue::N(now.to_string()));

        if let Some(expires_at) = expires_at {
            request = request.item(TTL_ATTRIBUTE, AttributeValue::N(expires_at.to_string()));
        }

        match request.send().await {
            Ok(_) => Ok(true),
            // The condition failing is the answer, not a failure: somebody else
            // holds it.
            Err(e) if is_conditional_check_failure(&e) => Ok(false),
            Err(e) => Err(sdk_error("DynamoDB put_item", e)),
        }
    }

    /// Delete an item **only if** it currently holds `expected`.
    ///
    /// The release half of a lock. See
    /// [`Cache::forget_if`](https://docs.rs/rainier-cache) for why an
    /// unconditional delete is wrong here.
    pub async fn delete_if(&self, key: &str, expected: &[u8]) -> Result<bool> {
        let result = self
            .client
            .delete_item()
            .table_name(&self.table)
            .key(KEY_ATTRIBUTE, AttributeValue::S(key.to_string()))
            .condition_expression("#v = :expected")
            .expression_attribute_names("#v", VALUE_ATTRIBUTE)
            .expression_attribute_values(
                ":expected",
                AttributeValue::B(Blob::new(expected.to_vec())),
            )
            .send()
            .await;

        match result {
            Ok(_) => Ok(true),
            Err(e) if is_conditional_check_failure(&e) => Ok(false),
            Err(e) => Err(sdk_error("DynamoDB delete_item", e)),
        }
    }

    /// Delete an item. `true` if it was there.
    ///
    /// Asks for the old value, which answers "was it there" in one round trip
    /// rather than two.
    pub async fn delete(&self, key: &str) -> Result<bool> {
        let output = self
            .client
            .delete_item()
            .table_name(&self.table)
            .key(KEY_ATTRIBUTE, AttributeValue::S(key.to_string()))
            .return_values(ReturnValue::AllOld)
            .send()
            .await
            .map_err(|e| sdk_error("DynamoDB delete_item", e))?;

        Ok(output.attributes().is_some())
    }

    /// Add to a counter, creating the item if absent. Returns the new value.
    ///
    /// `ADD` is atomic server-side, so there is no read-modify-write to lose an
    /// increment under concurrency.
    pub async fn add_to_counter(&self, key: &str, delta: i64) -> Result<i64> {
        let output = self
            .client
            .update_item()
            .table_name(&self.table)
            .key(KEY_ATTRIBUTE, AttributeValue::S(key.to_string()))
            .update_expression("ADD #counter :delta")
            // Named through ExpressionAttributeNames even though `counter` is not
            // reserved today: it costs nothing and survives DynamoDB adding a
            // reserved word later.
            .expression_attribute_names("#counter", COUNTER_ATTRIBUTE)
            .expression_attribute_values(":delta", AttributeValue::N(delta.to_string()))
            .return_values(ReturnValue::UpdatedNew)
            .send()
            .await
            .map_err(|e| sdk_error("DynamoDB update_item", e))?;

        match output.attributes().and_then(|attributes| attributes.get(COUNTER_ATTRIBUTE)) {
            Some(AttributeValue::N(value)) => value
                .parse()
                .map_err(|_| Error::internal("DynamoDB returned a non-numeric counter")),
            _ => Err(Error::internal("DynamoDB did not return the updated counter")),
        }
    }
}

impl std::fmt::Debug for DynamoDbClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DynamoDbClient").field("table", &self.table).finish()
    }
}

/// Whether a write was refused because its condition did not hold.
///
/// Not an error at the call site: it is the *answer*. `put_if_absent` returning
/// `Ok(false)` means somebody else holds the lock, which is a normal outcome of
/// two processes racing and the entire point of asking.
pub fn is_conditional_check_failure<E, R>(error: &SdkError<E, R>) -> bool
where
    E: ProvideErrorMetadata,
{
    match error {
        SdkError::ServiceError(service) => {
            matches!(service.err().code(), Some("ConditionalCheckFailedException"))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn client() -> DynamoDbClient {
        DynamoDbClient::new(
            &AwsConnector::with_credentials("id", "secret", "us-east-1").await,
            "rainier_cache",
        )
    }

    #[tokio::test]
    async fn it_remembers_its_table() {
        assert_eq!(client().await.table(), "rainier_cache");
    }

    #[test]
    fn an_item_with_no_expiry_never_expires() {
        let item = DynamoItem { value: Some(vec![1]), expires_at: None };
        assert!(!item.is_expired(i64::MAX));
    }

    #[test]
    fn an_item_past_its_expiry_is_expired() {
        // The check that matters: DynamoDB's own sweep is up to 48 hours late, so
        // the row is very often still present when this returns true.
        let item = DynamoItem { value: Some(vec![1]), expires_at: Some(1_000) };

        assert!(item.is_expired(1_000), "expiry is inclusive");
        assert!(item.is_expired(1_001));
        assert!(!item.is_expired(999));
    }

    #[test]
    fn the_attribute_names_are_the_documented_ones() {
        // Changing one of these silently orphans every existing item, and the
        // TTL one has to match what the table was configured with.
        assert_eq!(KEY_ATTRIBUTE, "key");
        assert_eq!(VALUE_ATTRIBUTE, "value");
        assert_eq!(TTL_ATTRIBUTE, "expires_at");
        assert_eq!(COUNTER_ATTRIBUTE, "counter");
    }
}
