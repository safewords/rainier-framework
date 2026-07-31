//! SQS — the service interface.
//!
//! Everything that knows what SQS *is*: its operations, its attribute names, its
//! limits, and how its replies decode. No queue semantics — this module has
//! never heard of a job, an attempt, or a reservation. That belongs to whatever
//! adapts it to a queue port.

use std::time::Duration;

use aws_sdk_sqs::types::{MessageSystemAttributeName, QueueAttributeName};
use aws_sdk_sqs::Client;
use rainier_support::{Error, Result};

use crate::aws::{sdk_error, AwsConnector};

/// SQS's largest permitted delay, in seconds.
pub const MAX_DELAY_SECONDS: u64 = 900;

/// SQS's largest permitted visibility timeout, in seconds.
pub const MAX_VISIBILITY_SECONDS: i32 = 43_200;

/// SQS's largest permitted long-poll wait, in seconds.
pub const MAX_WAIT_SECONDS: i32 = 20;

/// One message as SQS returned it.
#[derive(Debug, Clone)]
pub struct SqsMessage {
    /// The body, verbatim.
    pub body: String,
    /// The handle that deletes or releases this message. Valid only for this
    /// receive.
    pub receipt_handle: String,
    /// How many times SQS has delivered this message, including now.
    ///
    /// The only delivery count available, and authoritative: it counts
    /// deliveries to workers that died without recording anything.
    pub receive_count: u32,
}

/// Talks to one SQS queue.
///
/// Service-shaped, not queue-shaped: `send`, `receive`, `delete`. Adapting those
/// to a queue port — attempts, reservations, failure handling — is the adapter's
/// job, and keeping the two apart is what lets this be tested and reasoned about
/// as "does it speak SQS correctly".
#[derive(Clone)]
pub struct SqsClient {
    client: Client,
    queue_url: String,
}

impl SqsClient {
    /// A client for the queue at `queue_url`.
    pub fn new(connector: &AwsConnector, queue_url: impl Into<String>) -> Self {
        Self { client: connector.sqs(), queue_url: queue_url.into() }
    }

    /// Use an SDK client you built yourself.
    pub fn with_client(client: Client, queue_url: impl Into<String>) -> Self {
        Self { client, queue_url: queue_url.into() }
    }

    /// The queue's URL.
    pub fn queue_url(&self) -> &str {
        &self.queue_url
    }

    /// The SDK client, for an operation this does not expose.
    pub fn inner(&self) -> &Client {
        &self.client
    }

    /// Send a message, optionally delayed.
    pub async fn send(&self, body: &str, delay: Duration) -> Result<Option<String>> {
        let output = self
            .client
            .send_message()
            .queue_url(&self.queue_url)
            .message_body(body)
            .delay_seconds(Self::delay_seconds(delay)?)
            .send()
            .await
            .map_err(|e| sdk_error("SQS send_message", e))?;

        Ok(output.message_id().map(str::to_string))
    }

    /// Receive at most one message, making it invisible for `visibility`.
    ///
    /// `wait` turns on long polling. At zero, a worker polling an empty queue
    /// makes a billed request every time round its loop; at twenty seconds it
    /// makes one.
    pub async fn receive(
        &self,
        visibility: Duration,
        wait: Duration,
    ) -> Result<Option<SqsMessage>> {
        let output = self
            .client
            .receive_message()
            .queue_url(&self.queue_url)
            .max_number_of_messages(1)
            .visibility_timeout((visibility.as_secs() as i32).min(MAX_VISIBILITY_SECONDS))
            .wait_time_seconds((wait.as_secs() as i32).min(MAX_WAIT_SECONDS))
            .message_system_attribute_names(MessageSystemAttributeName::ApproximateReceiveCount)
            .send()
            .await
            .map_err(|e| sdk_error("SQS receive_message", e))?;

        let Some(message) = output.messages().first() else {
            return Ok(None);
        };

        Ok(Some(SqsMessage {
            body: message.body().unwrap_or_default().to_string(),
            receipt_handle: message.receipt_handle().unwrap_or_default().to_string(),
            receive_count: message
                .attributes()
                .and_then(|attributes| {
                    attributes.get(&MessageSystemAttributeName::ApproximateReceiveCount)
                })
                .and_then(|count| count.parse().ok())
                .unwrap_or(1),
        }))
    }

    /// Delete a message, so it is not redelivered.
    pub async fn delete(&self, receipt_handle: &str) -> Result<()> {
        self.client
            .delete_message()
            .queue_url(&self.queue_url)
            .receipt_handle(receipt_handle)
            .send()
            .await
            .map_err(|e| sdk_error("SQS delete_message", e))?;

        Ok(())
    }

    /// Make a message visible again after `delay`.
    ///
    /// SQS has no explicit "release": changing the visibility timeout **is** how
    /// it is done, and a timeout of zero returns the message immediately.
    pub async fn change_visibility(&self, receipt_handle: &str, delay: Duration) -> Result<()> {
        self.client
            .change_message_visibility()
            .queue_url(&self.queue_url)
            .receipt_handle(receipt_handle)
            .visibility_timeout((delay.as_secs() as i32).min(MAX_VISIBILITY_SECONDS))
            .send()
            .await
            .map_err(|e| sdk_error("SQS change_message_visibility", e))?;

        Ok(())
    }

    /// Roughly how many messages are waiting.
    ///
    /// **Approximate, unavoidably** — it is a distributed queue and there is no
    /// exact count to be had. Do not build anything that must be correct on it.
    pub async fn approximate_size(&self) -> Result<u64> {
        let output = self
            .client
            .get_queue_attributes()
            .queue_url(&self.queue_url)
            .attribute_names(QueueAttributeName::ApproximateNumberOfMessages)
            .send()
            .await
            .map_err(|e| sdk_error("SQS get_queue_attributes", e))?;

        Ok(output
            .attributes()
            .and_then(|attributes| attributes.get(&QueueAttributeName::ApproximateNumberOfMessages))
            .and_then(|count| count.parse().ok())
            .unwrap_or(0))
    }

    /// Discard everything on the queue.
    ///
    /// SQS allows this once every 60 seconds and takes up to 60 seconds to finish
    /// it. A development convenience, not something to call in a loop.
    pub async fn purge(&self) -> Result<()> {
        self.client
            .purge_queue()
            .queue_url(&self.queue_url)
            .send()
            .await
            .map_err(|e| sdk_error("SQS purge_queue", e))?;

        Ok(())
    }

    /// SQS's delay field: whole seconds, capped at fifteen minutes.
    ///
    /// A longer delay is **refused, not truncated** — a message scheduled for
    /// tomorrow arriving in fifteen minutes is worse than a clear failure.
    pub fn delay_seconds(delay: Duration) -> Result<i32> {
        let seconds = delay.as_secs();
        if seconds > MAX_DELAY_SECONDS {
            return Err(Error::internal(format!(
                "SQS cannot delay a message more than {MAX_DELAY_SECONDS} seconds; this one \
                 asked for {seconds}. Schedule it instead of delaying it."
            )));
        }
        Ok(seconds as i32)
    }
}

impl std::fmt::Debug for SqsClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqsClient").field("queue_url", &self.queue_url).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "https://sqs.eu-west-2.amazonaws.com/123456789012/jobs";

    async fn client() -> SqsClient {
        SqsClient::new(&AwsConnector::with_credentials("id", "secret", "eu-west-2").await, URL)
    }

    #[tokio::test]
    async fn it_remembers_its_queue() {
        assert_eq!(client().await.queue_url(), URL);
    }

    #[test]
    fn a_delay_within_the_limit_is_accepted() {
        assert_eq!(SqsClient::delay_seconds(Duration::ZERO).unwrap(), 0);
        assert_eq!(SqsClient::delay_seconds(Duration::from_secs(300)).unwrap(), 300);
        assert_eq!(SqsClient::delay_seconds(Duration::from_secs(900)).unwrap(), 900);
    }

    #[test]
    fn a_delay_over_the_limit_is_refused_rather_than_truncated() {
        let err = SqsClient::delay_seconds(Duration::from_secs(86_400)).unwrap_err();

        assert!(err.message().contains("900"), "{}", err.message());
        assert!(err.message().contains("86400"), "{}", err.message());
    }

    #[test]
    fn the_service_limits_are_the_documented_ones() {
        assert_eq!(MAX_DELAY_SECONDS, 900, "fifteen minutes");
        assert_eq!(MAX_VISIBILITY_SECONDS, 43_200, "twelve hours");
        assert_eq!(MAX_WAIT_SECONDS, 20);
    }
}
