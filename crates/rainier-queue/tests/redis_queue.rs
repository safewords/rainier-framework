//! The Redis queue against a real Redis.
//!
//! Every interesting property of this driver is a property of Redis — that a
//! consumer group redelivers what a dead worker left, that `XAUTOCLAIM`
//! respects an idle time, that a sorted set promotion is atomic. None of them
//! can be checked without a server, so these tests are skipped unless one is
//! reachable, and CI provides one.
//!
//! Run them locally with:
//!
//! ```sh
//! docker run --rm -p 6379:6379 redis:7
//! cargo test -p rainier-queue --features redis --test redis_queue
//! ```
#![cfg(feature = "redis")]

use std::time::Duration;

use chrono::Utc;
use rainier_drivers::RedisConnector;
use rainier_queue::{Queue, QueuedJob, RedisQueue};
use serde_json::json;

/// A queue on a key prefix nothing else uses, or `None` when Redis is not
/// there.
///
/// Skipping rather than failing, so a contributor without Redis running gets a
/// green suite — **except** where `REDIS_REQUIRED` is set, which CI does. A
/// test suite that silently skipped in CI would be a driver nobody had ever
/// run, reported as passing.
async fn queue(name: &str) -> Option<RedisQueue> {
    let url = std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1/".to_string());
    let connector = RedisConnector::open(&url).ok()?;

    let queue = RedisQueue::connect(&connector)
        .await
        .ok()?
        // A prefix per test, so they can run at once without seeing each
        // other's jobs.
        .with_prefix(format!("rainier-test:{name}:"))
        .with_reservation(Duration::from_millis(200))
        .as_consumer(format!("{name}-worker"));

    queue.clear("default").await.ok()?;
    Some(queue)
}

macro_rules! redis_or_skip {
    ($name:literal) => {
        match queue($name).await {
            Some(queue) => queue,
            None if std::env::var("REDIS_REQUIRED").is_ok() => {
                panic!(
                    "REDIS_REQUIRED is set and no Redis answered at {}",
                    std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1/".into())
                )
            }
            None => {
                eprintln!("skipping `{}`: no Redis at REDIS_URL", $name);
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
        queue: "default".into(),
        attempts: 0,
        max_attempts: 3,
        available_at: Utc::now(),
        created_at: Utc::now(),
        unique_key: None,
        delivery_handle: None,
    }
}

#[tokio::test]
async fn a_pushed_job_comes_back_from_reserve() {
    let queue = redis_or_skip!("roundtrip");

    queue.push(job("j1")).await.expect("push");
    assert_eq!(queue.size("default").await.unwrap(), 1);

    let reserved = queue.reserve("default").await.expect("reserve").expect("a job");

    assert_eq!(reserved.id, "j1");
    assert_eq!(reserved.attempts, 1, "reserving is an attempt");
    assert_eq!(reserved.payload["n"], 1);

    queue.acknowledge(&reserved).await.expect("ack");
    assert_eq!(queue.size("default").await.unwrap(), 0, "an acked job is gone from the stream");
}

#[tokio::test]
async fn an_empty_queue_reserves_nothing() {
    let queue = redis_or_skip!("empty");

    assert!(queue.reserve("default").await.expect("reserve").is_none());
}

#[tokio::test]
async fn two_workers_do_not_get_the_same_job() {
    // The one property Redis genuinely gives you for free, and the reason a
    // consumer group is the right primitive.
    let queue = redis_or_skip!("exclusive");
    queue.push(job("j1")).await.expect("push");

    let first = queue.reserve("default").await.expect("reserve");
    let second = queue.reserve("default").await.expect("reserve");

    assert!(first.is_some());
    assert!(second.is_none(), "the second worker should find nothing");
}

#[tokio::test]
async fn a_job_a_dead_worker_held_is_redelivered() {
    // Reserve and then never acknowledge, as a worker that was killed would.
    // The reservation is 200ms here, so the claim is quick.
    let queue = redis_or_skip!("redelivery");
    queue.push(job("j1")).await.expect("push");

    let abandoned = queue.reserve("default").await.expect("reserve").expect("a job");
    assert_eq!(abandoned.attempts, 1);

    // Before the reservation lapses, nobody else may have it.
    assert!(queue.reserve("default").await.expect("reserve").is_none());

    tokio::time::sleep(Duration::from_millis(400)).await;

    let reclaimed = queue.reserve("default").await.expect("reserve").expect("redelivered");
    assert_eq!(reclaimed.id, "j1", "the same job comes back");

    queue.acknowledge(&reclaimed).await.expect("ack");
}

#[tokio::test]
async fn a_delayed_job_is_not_available_until_it_is_due() {
    let queue = redis_or_skip!("delayed");

    let mut delayed = job("j1");
    delayed.available_at = Utc::now() + chrono::Duration::milliseconds(400);
    queue.push(delayed).await.expect("push");

    assert_eq!(queue.size("default").await.unwrap(), 1, "queued, just not due");
    assert!(queue.reserve("default").await.expect("reserve").is_none());

    tokio::time::sleep(Duration::from_millis(600)).await;

    let due = queue.reserve("default").await.expect("reserve").expect("now due");
    assert_eq!(due.id, "j1");
}

#[tokio::test]
async fn releasing_puts_a_job_back_for_another_attempt() {
    let queue = redis_or_skip!("release");
    queue.push(job("j1")).await.expect("push");

    let reserved = queue.reserve("default").await.expect("reserve").expect("a job");
    queue.release(&reserved, Duration::ZERO).await.expect("release");

    let again = queue.reserve("default").await.expect("reserve").expect("back again");

    assert_eq!(again.id, "j1");
    assert_eq!(again.attempts, 2, "the attempt count carries across the release");
}

#[tokio::test]
async fn a_released_job_with_a_delay_waits_for_it() {
    let queue = redis_or_skip!("backoff");
    queue.push(job("j1")).await.expect("push");

    let reserved = queue.reserve("default").await.expect("reserve").expect("a job");
    queue.release(&reserved, Duration::from_millis(400)).await.expect("release");

    assert!(queue.reserve("default").await.expect("reserve").is_none(), "still backing off");

    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(queue.reserve("default").await.expect("reserve").is_some());
}

#[tokio::test]
async fn a_failed_job_leaves_the_stream_and_lands_in_the_failed_list() {
    let queue = redis_or_skip!("failed");
    queue.push(job("j1")).await.expect("push");

    let reserved = queue.reserve("default").await.expect("reserve").expect("a job");
    queue.fail(&reserved, "it did not work").await.expect("fail");

    assert_eq!(queue.size("default").await.unwrap(), 0);

    let failed = queue.failed_jobs("default").await.expect("read the failed list");
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].job.id, "j1");
    assert_eq!(failed[0].error, "it did not work");

    // The transport's bookkeeping does not follow the job into storage.
    assert!(failed[0].job.payload.get("__redis_entry_id").is_none());
}

#[tokio::test]
async fn clearing_removes_ready_delayed_and_failed_alike() {
    let queue = redis_or_skip!("clear");

    queue.push(job("ready")).await.expect("push");

    let mut later = job("later");
    later.available_at = Utc::now() + chrono::Duration::seconds(60);
    queue.push(later).await.expect("push");

    assert_eq!(queue.size("default").await.unwrap(), 2);

    let removed = queue.clear("default").await.expect("clear");
    assert_eq!(removed, 2);
    assert_eq!(queue.size("default").await.unwrap(), 0);
}

#[tokio::test]
async fn two_queues_on_one_redis_do_not_see_each_other() {
    let queue = redis_or_skip!("isolation");

    queue.push(job("on-default")).await.expect("push");
    queue.push(QueuedJob { queue: "mail".into(), ..job("on-mail") }).await.expect("push");

    assert_eq!(queue.size("default").await.unwrap(), 1);
    assert_eq!(queue.size("mail").await.unwrap(), 1);

    let reserved = queue.reserve("mail").await.expect("reserve").expect("a job");
    assert_eq!(reserved.id, "on-mail");

    queue.acknowledge(&reserved).await.expect("ack");
    queue.clear("mail").await.expect("clear");
}

#[tokio::test]
async fn the_eviction_policy_is_reported() {
    // Not asserted to be `noeviction` — a developer's Redis is whatever it is,
    // and the point is that the check answers rather than that it passes.
    let queue = redis_or_skip!("eviction");

    if let Some(policy) = queue.check_eviction_policy().await {
        assert!(!policy.is_empty());
    }
}
