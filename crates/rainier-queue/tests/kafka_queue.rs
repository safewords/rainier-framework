//! The Kafka queue against a real broker.
//!
//! The unit tests cover the naming and the arithmetic; what needs a broker is
//! everything that makes this driver different from the others — that an
//! acknowledgement is a cursor moving, that a retry is a new record rather than
//! the old one becoming visible again, and that a partition somebody else has
//! leased is not read.
//!
//! ```sh
//! docker run --rm -p 9092:9092 apache/kafka:3.9.0
//! KAFKA_BROKERS=localhost:9092 cargo test -p rainier-queue --features kafka --test kafka_queue
//! ```
#![cfg(feature = "kafka")]

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use rainier_cache::{LockManager, MemoryCache};
use rainier_drivers::kafka::{KafkaClient, KafkaConnector};
use rainier_queue::{KafkaQueue, Queue, QueuedJob};
use serde_json::json;

fn brokers() -> String {
    std::env::var("KAFKA_BROKERS").unwrap_or_else(|_| "localhost:9092".to_string())
}

/// A queue on a topic nothing else uses, or `None` when no broker answers.
///
/// One process, so the memory cache really is shared between the workers in
/// these tests — which is what `declared_shared` is for.
async fn queue(name: &str) -> Option<KafkaQueue> {
    let client = Arc::new(
        KafkaClient::connect(
            &KafkaConnector::parse(&brokers())
                .with_client_id("rainier-tests")
                // Short, so a machine with no broker skips in seconds.
                .with_timeout(Duration::from_secs(3)),
        )
        .await
        .ok()?,
    );

    let topic = format!("rainier-queue-{name}");
    client.create_topic(&topic, 1, 1).await.ok()?;
    client.create_topic(&format!("{topic}.failed"), 1, 1).await.ok()?;

    let locks = LockManager::new(Arc::new(MemoryCache::new())).declared_shared();

    let queue = KafkaQueue::new(client, locks)
        .ok()?
        .with_topic_prefix(format!("rainier-queue-{name}"))
        // A group per test, so one test's cursor is not another's.
        .in_group(format!("test-{name}"))
        .with_max_wait(Duration::from_millis(500));

    // Start at the end of whatever a previous run left behind.
    queue.clear("").await.ok()?;

    Some(queue)
}

macro_rules! kafka_or_skip {
    ($name:literal) => {
        match queue($name).await {
            Some(queue) => queue,
            None if std::env::var("KAFKA_REQUIRED").is_ok_and(|required| !required.is_empty()) => {
                panic!("KAFKA_REQUIRED is set and no broker answered at {}", brokers())
            }
            None => {
                eprintln!("skipping `{}`: no Kafka at KAFKA_BROKERS", $name);
                return;
            }
        }
    };
}

fn job(id: &str) -> QueuedJob {
    QueuedJob {
        id: id.into(),
        name: "test.job".into(),
        payload: json!({ "n": 1 }),
        // The queue name is empty because the topic prefix already names the
        // topic — `topic_for("")` is the prefix itself.
        queue: String::new(),
        attempts: 0,
        max_attempts: 3,
        available_at: Utc::now(),
        created_at: Utc::now(),
        unique_key: None,
    }
}

#[tokio::test]
async fn a_pushed_job_comes_back_from_reserve() {
    let queue = kafka_or_skip!("roundtrip");

    queue.push(job("j1")).await.expect("push");
    assert_eq!(queue.size("").await.expect("size"), 1, "one job of lag");

    let reserved = queue.reserve("").await.expect("reserve").expect("a job");

    assert_eq!(reserved.id, "j1");
    assert_eq!(reserved.attempts, 1, "reserving is an attempt");
    assert_eq!(reserved.payload["n"], 1);
}

#[tokio::test]
async fn acknowledging_moves_the_cursor_past_it() {
    let queue = kafka_or_skip!("acknowledge");

    queue.push(job("j1")).await.expect("push");
    let reserved = queue.reserve("").await.expect("reserve").expect("a job");

    queue.acknowledge(&reserved).await.expect("acknowledge");

    assert_eq!(queue.size("").await.expect("size"), 0, "nothing is waiting");
    assert!(
        queue.reserve("").await.expect("reserve").is_none(),
        "an acknowledged job must not come back"
    );
}

#[tokio::test]
async fn an_unacknowledged_job_is_handed_out_again_and_counted() {
    // Kafka redelivers from a cursor that never moved and says nothing about
    // having done so, which is why the driver counts deliveries itself. Without
    // the count, a job that kills its worker is retried forever.
    let queue = kafka_or_skip!("redelivery");

    queue.push(job("j1")).await.expect("push");

    let first = queue.reserve("").await.expect("reserve").expect("a job");
    assert_eq!(first.attempts, 1);

    let again = queue.reserve("").await.expect("reserve").expect("the same job");
    assert_eq!(again.id, "j1", "the cursor did not move, so this is the same record");
    assert_eq!(again.attempts, 2, "and it is a second attempt, not a first");
}

#[tokio::test]
async fn a_released_job_comes_back_keeping_its_attempts() {
    // The retry is a *new record at the end of the topic*, so its own delivery
    // count starts over. If the attempt number started over with it, the job
    // would never reach its last attempt.
    let queue = kafka_or_skip!("release");

    queue.push(job("j1")).await.expect("push");

    let reserved = queue.reserve("").await.expect("reserve").expect("a job");
    assert_eq!(reserved.attempts, 1);

    queue.release(&reserved, Duration::ZERO).await.expect("release");

    let retried = queue.reserve("").await.expect("reserve").expect("the retry");

    assert_eq!(retried.id, "j1");
    assert_eq!(retried.attempts, 2, "the retry carries its history");
}

#[tokio::test]
async fn a_released_job_with_a_delay_is_not_handed_out_yet() {
    let queue = kafka_or_skip!("delayed");

    queue.push(job("j1")).await.expect("push");
    let reserved = queue.reserve("").await.expect("reserve").expect("a job");

    queue.release(&reserved, Duration::from_secs(60)).await.expect("release");

    assert!(
        queue.reserve("").await.expect("reserve").is_none(),
        "a job that is not due blocks its partition rather than being handed out"
    );
    assert_eq!(queue.size("").await.expect("size"), 1, "it is still there, waiting");
}

#[tokio::test]
async fn a_failed_job_goes_to_the_failed_topic_and_the_cursor_moves_on() {
    let queue = kafka_or_skip!("failed");

    queue.push(job("j1")).await.expect("push");
    let reserved = queue.reserve("").await.expect("reserve").expect("a job");

    queue.fail(&reserved, "the invoice service said no").await.expect("fail");

    assert!(
        queue.reserve("").await.expect("reserve").is_none(),
        "a failed job must not block the partition"
    );

    // And it is on the dead-letter topic rather than gone.
    let client = queue.client();
    let failed = queue.failed_topic_for("");
    let end = client
        .offset(&failed, 0, rainier_drivers::kafka::KafkaOffset::Latest)
        .await
        .expect("latest");

    assert!(end > 0, "the failed job should have been recorded on `{failed}`");
}

#[tokio::test]
async fn jobs_come_back_in_the_order_they_went_in() {
    let queue = kafka_or_skip!("order");

    for id in ["j1", "j2", "j3"] {
        let mut queued = job(id);
        // One key, so one partition, so an order exists to assert on at all.
        queued.unique_key = Some("account-1".into());
        queue.push(queued).await.expect("push");
    }

    for expected in ["j1", "j2", "j3"] {
        let reserved = queue.reserve("").await.expect("reserve").expect("a job");
        assert_eq!(reserved.id, expected);
        queue.acknowledge(&reserved).await.expect("acknowledge");
    }
}

#[tokio::test]
async fn clearing_skips_what_is_waiting_and_says_how_many() {
    let queue = kafka_or_skip!("clear");

    queue.push(job("j1")).await.expect("push");
    queue.push(job("j2")).await.expect("push");

    let skipped = queue.clear("").await.expect("clear");

    assert_eq!(skipped, 2, "it reports what it skipped past");
    assert_eq!(queue.size("").await.expect("size"), 0);
    assert!(queue.reserve("").await.expect("reserve").is_none());
}

#[tokio::test]
async fn a_partition_another_worker_has_leased_is_left_alone() {
    // The property that makes two workers safe. Both share a lock store, as
    // two processes would, and the second must find nothing to do rather than
    // running the first one's job a second time.
    let Some(first) = queue("leases").await else {
        if std::env::var("KAFKA_REQUIRED").is_ok_and(|required| !required.is_empty()) {
            panic!("KAFKA_REQUIRED is set and no broker answered at {}", brokers());
        }
        eprintln!("skipping `leases`: no Kafka at KAFKA_BROKERS");
        return;
    };

    let client = Arc::clone(first.client());

    // Two lock managers over one store — which is what two processes are.
    let store: Arc<dyn rainier_cache::Cache> = Arc::new(MemoryCache::new());
    let locks = || LockManager::new(Arc::clone(&store)).declared_shared();

    let one = KafkaQueue::new(Arc::clone(&client), locks())
        .expect("a shared store")
        .with_topic_prefix("rainier-queue-leases")
        .in_group("test-leases-pair");
    let two = KafkaQueue::new(client, locks())
        .expect("a shared store")
        .with_topic_prefix("rainier-queue-leases")
        .in_group("test-leases-pair");

    one.clear("").await.expect("clear");
    one.push(job("j1")).await.expect("push");

    let mine = one.reserve("").await.expect("reserve").expect("the first worker takes it");
    assert_eq!(mine.id, "j1");

    assert!(
        two.reserve("").await.expect("reserve").is_none(),
        "the second worker must not read a partition the first has leased"
    );

    one.release_partitions().await.expect("hand it back");

    assert!(
        two.reserve("").await.expect("reserve").is_some(),
        "and may take it once the first lets go"
    );
}
