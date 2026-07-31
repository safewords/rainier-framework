//! [`SqsQueue`] — the [`Queue`] port over an [`SqsClient`].

use std::time::Duration;

use rainier_drivers::{AwsConnector, SqsClient};
use rainier_support::{BoxFuture, Error, Result};
use serde_json::json;

use crate::job::QueuedJob;
use crate::queue::Queue;

/// Where `reserve` stashes the receipt handle so `acknowledge` can find it.
const RECEIPT_HANDLE: &str = "__sqs_receipt_handle";

/// Jobs on Amazon SQS.
///
/// An **adapter**: the SQS knowledge — its operations, its limits, that changing
/// a visibility timeout is how a message gets released — lives in
/// [`rainier-drivers`](rainier_drivers). What is decided here is queue policy:
/// how a job is serialised, where the receipt handle rides, and what an attempt
/// count means.
///
/// SQS's model is **already** reserve-then-acknowledge, which is why it fits the
/// port with no claim protocol of its own: a received message goes invisible for
/// its visibility timeout and stays in the queue unless deleted, so a worker that
/// dies mid-job lets the timeout lapse and the message comes back.
///
/// ```no_run
/// use rainier_drivers::AwsConnector;
/// use rainier_queue::SqsQueue;
///
/// # async fn run() -> rainier_support::Result<()> {
/// let queue = SqsQueue::new(
///     &AwsConnector::from_env().await,
///     "https://sqs.eu-west-2.amazonaws.com/123456789012/jobs",
/// );
/// # let _ = queue; Ok(()) }
/// ```
///
/// ## What it declines to pretend
///
/// **`size` is approximate**, because SQS has no exact count to give.
///
/// **There is no failed-job store.** [`fail`](Queue::fail) deletes and logs, since
/// SQS's own answer is a **redrive policy**: configure a dead-letter queue and SQS
/// moves exhausted messages there. A second mechanism here would disagree with
/// the first.
pub struct SqsQueue {
    client: SqsClient,
    visibility: Duration,
    wait_time: Duration,
}

impl SqsQueue {
    /// Jobs on the queue at `queue_url`.
    pub fn new(connector: &AwsConnector, queue_url: impl Into<String>) -> Self {
        Self::with_client(SqsClient::new(connector, queue_url))
    }

    /// Use a client you already have.
    pub fn with_client(client: SqsClient) -> Self {
        Self { client, visibility: Duration::from_secs(90), wait_time: Duration::ZERO }
    }

    /// How long a received message stays invisible.
    ///
    /// Must exceed how long a job takes, or a second worker picks it up while the
    /// first is still running it — the one reliable way to get a job executed
    /// twice.
    pub fn with_visibility_timeout(mut self, visibility: Duration) -> Self {
        self.visibility = visibility;
        self
    }

    /// Wait up to this long for a message rather than returning immediately.
    ///
    /// **Long polling, and worth turning on.** At zero, a worker on an empty
    /// queue makes a billed request every time round its loop; at twenty seconds
    /// it makes one. The driver caps it at SQS's maximum.
    pub fn with_wait_time(mut self, wait_time: Duration) -> Self {
        self.wait_time = wait_time;
        self
    }

    /// The queue's URL.
    pub fn queue_url(&self) -> &str {
        self.client.queue_url()
    }

    /// The client, for an operation this port does not expose.
    pub fn client(&self) -> &SqsClient {
        &self.client
    }
}

impl Queue for SqsQueue {
    fn name(&self) -> &str {
        "sqs"
    }

    fn push<'a>(&'a self, job: QueuedJob) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let delay = job
                .available_at
                .signed_duration_since(chrono::Utc::now())
                .to_std()
                .unwrap_or(Duration::ZERO);

            self.client.send(&serde_json::to_string(&job)?, delay).await?;

            // SQS assigns its own message id, deliberately not returned: the
            // job's id is inside the body and is what the application knows it
            // by, so returning SQS's would leave a caller unable to correlate.
            Ok(job.id)
        })
    }

    fn reserve<'a>(&'a self, _queue: &'a str) -> BoxFuture<'a, Result<Option<QueuedJob>>> {
        Box::pin(async move {
            // The queue name is unused: an SQS queue *is* a URL, so one
            // `SqsQueue` serves one queue. A worker wanting several names wants
            // several instances, which is also how SQS priorities are done.
            let Some(message) = self.client.receive(self.visibility, self.wait_time).await? else {
                return Ok(None);
            };

            let mut job: QueuedJob = match serde_json::from_str(&message.body) {
                Ok(job) => job,
                Err(e) => {
                    // A message we cannot parse is not a job. Leaving it would
                    // have every poll return it forever; deleting it loses
                    // whatever it was. Logging loudly and deleting is the lesser
                    // evil, and a redrive policy is the real answer.
                    tracing::error!(error = %e, "discarding an unparseable SQS message");
                    let _ = self.client.delete(&message.receipt_handle).await;
                    return Ok(None);
                }
            };

            // The handle is valid only for this receive, so it rides on the job.
            job.payload[RECEIPT_HANDLE] = json!(message.receipt_handle);

            // SQS's delivery count is authoritative: it counts deliveries to
            // workers that died without recording anything.
            job.attempts = message.receive_count;

            Ok(Some(job))
        })
    }

    fn acknowledge<'a>(&'a self, job: &'a QueuedJob) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { self.client.delete(receipt_handle(job)?).await })
    }

    fn release<'a>(&'a self, job: &'a QueuedJob, delay: Duration) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { self.client.change_visibility(receipt_handle(job)?, delay).await })
    }

    fn fail<'a>(&'a self, job: &'a QueuedJob, error: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            tracing::error!(
                job = %job.name,
                id = %job.id,
                attempts = job.attempts,
                %error,
                "a job failed after its last attempt; configure a dead-letter queue to keep it"
            );

            self.client.delete(receipt_handle(job)?).await
        })
    }

    fn size<'a>(&'a self, _queue: &'a str) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move { self.client.approximate_size().await })
    }

    fn clear<'a>(&'a self, _queue: &'a str) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            // Purging is allowed once a minute and takes up to a minute, so the
            // count before it is the best answer available — and it is
            // approximate.
            let before = self.client.approximate_size().await.unwrap_or(0);
            self.client.purge().await?;
            Ok(before)
        })
    }
}

impl std::fmt::Debug for SqsQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqsQueue")
            .field("queue_url", &self.queue_url())
            .field("visibility", &self.visibility)
            .field("wait_time", &self.wait_time)
            .finish()
    }
}

/// The receipt handle [`reserve`](Queue::reserve) stashed on the job.
fn receipt_handle(job: &QueuedJob) -> Result<&str> {
    job.payload.get(RECEIPT_HANDLE).and_then(|value| value.as_str()).ok_or_else(|| {
        // Reachable only if a job was constructed rather than reserved, which is
        // a programming error rather than an SQS one.
        Error::internal("this job carries no SQS receipt handle, so it was not reserved from SQS")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "https://sqs.eu-west-2.amazonaws.com/123456789012/jobs";

    async fn queue() -> SqsQueue {
        SqsQueue::new(&AwsConnector::with_credentials("id", "secret", "eu-west-2").await, URL)
    }

    fn job() -> QueuedJob {
        QueuedJob {
            id: "job-1".to_string(),
            name: "test.job".to_string(),
            queue: "default".to_string(),
            payload: json!({ "x": 1 }),
            attempts: 0,
            max_attempts: 3,
            available_at: chrono::Utc::now(),
            created_at: chrono::Utc::now(),
            unique_key: None,
        }
    }

    #[tokio::test]
    async fn the_driver_is_named() {
        let queue = queue().await;
        assert_eq!(queue.name(), "sqs");
        assert_eq!(queue.queue_url(), URL);
    }

    #[tokio::test]
    async fn the_timings_are_configurable() {
        let queue = queue()
            .await
            .with_visibility_timeout(Duration::from_secs(600))
            .with_wait_time(Duration::from_secs(20));

        assert_eq!(queue.visibility, Duration::from_secs(600));
        assert_eq!(queue.wait_time, Duration::from_secs(20));
    }

    #[test]
    fn a_job_without_a_receipt_handle_cannot_be_acknowledged() {
        // Reachable only by constructing a job rather than reserving one, which
        // is a programming error and says so.
        let err = receipt_handle(&job()).unwrap_err();
        assert!(err.message().contains("not reserved from SQS"), "{}", err.message());
    }

    #[test]
    fn a_reserved_job_carries_its_handle() {
        let mut reserved = job();
        reserved.payload[RECEIPT_HANDLE] = json!("AQEB-handle");

        assert_eq!(receipt_handle(&reserved).unwrap(), "AQEB-handle");
    }

    #[test]
    fn a_job_round_trips_through_its_serialised_body() {
        // The body is the whole job, so a worker in another process reconstructs
        // exactly what was pushed.
        let original = job();
        let decoded: QueuedJob =
            serde_json::from_str(&serde_json::to_string(&original).unwrap()).unwrap();

        assert_eq!(decoded.id, original.id);
        assert_eq!(decoded.name, original.name);
        assert_eq!(decoded.max_attempts, original.max_attempts);
    }

    // The operations need a live queue. Run with:
    //   cargo test -p rainier-queue --features sqs -- --ignored
    // against a queue named by RAINIER_TEST_QUEUE_URL.
    #[tokio::test]
    #[ignore = "needs a live SQS queue"]
    async fn a_job_round_trips_through_sqs() {
        let url = std::env::var("RAINIER_TEST_QUEUE_URL").expect("RAINIER_TEST_QUEUE_URL");
        let queue = SqsQueue::new(&AwsConnector::from_env().await, url)
            .with_wait_time(Duration::from_secs(5));

        assert_eq!(queue.push(job()).await.unwrap(), "job-1");

        let reserved = queue.reserve("default").await.unwrap().expect("a message");
        assert_eq!(reserved.name, "test.job");
        assert!(reserved.attempts >= 1, "SQS counts the delivery");

        queue.acknowledge(&reserved).await.unwrap();
    }
}
