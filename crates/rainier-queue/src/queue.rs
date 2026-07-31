//! The [`Queue`] port and its in-process drivers.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use chrono::Utc;
use rainier_support::{BoxFuture, Result};

use crate::job::QueuedJob;

/// A queue backend.
///
/// The contract is deliberately "reserve, then acknowledge" rather than "pop":
/// a worker that crashes mid-job must not lose it. A reserved job stays in the
/// store, invisible to other workers, until it is acknowledged, released or
/// its reservation times out.
pub trait Queue: Send + Sync + 'static {
    /// A label for diagnostics — `"database"`, `"memory"`, `"sync"`.
    fn name(&self) -> &str;

    /// Enqueue a job.
    fn push<'a>(&'a self, job: QueuedJob) -> BoxFuture<'a, Result<String>>;

    /// Reserve the next available job on `queue`, or `None` if there is none.
    fn reserve<'a>(&'a self, queue: &'a str) -> BoxFuture<'a, Result<Option<QueuedJob>>>;

    /// Remove a finished job.
    fn acknowledge<'a>(&'a self, job: &'a QueuedJob) -> BoxFuture<'a, Result<()>>;

    /// Return a job to the queue for another attempt, available after `delay`.
    fn release<'a>(&'a self, job: &'a QueuedJob, delay: Duration) -> BoxFuture<'a, Result<()>>;

    /// Move a job to the failed store after its last attempt.
    fn fail<'a>(&'a self, job: &'a QueuedJob, error: &'a str) -> BoxFuture<'a, Result<()>>;

    /// How many jobs are waiting on `queue`.
    fn size<'a>(&'a self, queue: &'a str) -> BoxFuture<'a, Result<u64>>;

    /// Discard everything on `queue`. Returns how many were removed.
    fn clear<'a>(&'a self, queue: &'a str) -> BoxFuture<'a, Result<u64>>;
}

/// A job that exhausted its attempts.
#[derive(Debug, Clone, PartialEq)]
pub struct FailedJob {
    /// The job as it was when it failed.
    pub job: QueuedJob,
    /// The last error's message.
    pub error: String,
    /// When it failed.
    pub failed_at: chrono::DateTime<Utc>,
}

/// An in-process queue.
///
/// Jobs live in this process's memory, so they are lost when it stops and are
/// invisible to any other process. Right for tests and for a single-process
/// development server; use the database driver for anything that must survive
/// a restart.
#[derive(Default)]
pub struct MemoryQueue {
    pending: Mutex<HashMap<String, Vec<QueuedJob>>>,
    reserved: Mutex<Vec<QueuedJob>>,
    failed: Mutex<Vec<FailedJob>>,
}

impl MemoryQueue {
    /// An empty queue.
    pub fn new() -> Self {
        Self::default()
    }

    /// Every job that has exhausted its attempts.
    pub fn failed_jobs(&self) -> Vec<FailedJob> {
        self.failed.lock().expect("queue lock poisoned").clone()
    }

    /// How many jobs are currently reserved by a worker.
    pub fn reserved_count(&self) -> usize {
        self.reserved.lock().expect("queue lock poisoned").len()
    }

    /// Every pending job on `queue`, available or not.
    pub fn pending(&self, queue: &str) -> Vec<QueuedJob> {
        self.pending.lock().expect("queue lock poisoned").get(queue).cloned().unwrap_or_default()
    }
}

impl Queue for MemoryQueue {
    fn name(&self) -> &str {
        "memory"
    }

    fn push<'a>(&'a self, job: QueuedJob) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let id = job.id.clone();
            self.pending
                .lock()
                .expect("queue lock poisoned")
                .entry(job.queue.clone())
                .or_default()
                .push(job);
            Ok(id)
        })
    }

    fn reserve<'a>(&'a self, queue: &'a str) -> BoxFuture<'a, Result<Option<QueuedJob>>> {
        Box::pin(async move {
            let mut pending = self.pending.lock().expect("queue lock poisoned");
            let Some(jobs) = pending.get_mut(queue) else {
                return Ok(None);
            };

            // The first *available* job, not simply the first — a delayed job
            // at the head must not block the ones behind it.
            let Some(index) = jobs.iter().position(|job| job.is_available()) else {
                return Ok(None);
            };

            let mut job = jobs.remove(index);
            job.attempts += 1;
            self.reserved.lock().expect("queue lock poisoned").push(job.clone());
            Ok(Some(job))
        })
    }

    fn acknowledge<'a>(&'a self, job: &'a QueuedJob) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.reserved
                .lock()
                .expect("queue lock poisoned")
                .retain(|reserved| reserved.id != job.id);
            Ok(())
        })
    }

    fn release<'a>(&'a self, job: &'a QueuedJob, delay: Duration) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.reserved
                .lock()
                .expect("queue lock poisoned")
                .retain(|reserved| reserved.id != job.id);

            let released = job.clone().delayed_by(delay);
            self.pending
                .lock()
                .expect("queue lock poisoned")
                .entry(released.queue.clone())
                .or_default()
                .push(released);
            Ok(())
        })
    }

    fn fail<'a>(&'a self, job: &'a QueuedJob, error: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.reserved
                .lock()
                .expect("queue lock poisoned")
                .retain(|reserved| reserved.id != job.id);

            self.failed.lock().expect("queue lock poisoned").push(FailedJob {
                job: job.clone(),
                error: error.to_string(),
                failed_at: Utc::now(),
            });
            Ok(())
        })
    }

    fn size<'a>(&'a self, queue: &'a str) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            Ok(self
                .pending
                .lock()
                .expect("queue lock poisoned")
                .get(queue)
                .map(|jobs| jobs.len() as u64)
                .unwrap_or(0))
        })
    }

    fn clear<'a>(&'a self, queue: &'a str) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            let removed = self
                .pending
                .lock()
                .expect("queue lock poisoned")
                .remove(queue)
                .map(|jobs| jobs.len() as u64)
                .unwrap_or(0);
            Ok(removed)
        })
    }
}

impl std::fmt::Debug for MemoryQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryQueue")
            .field("reserved", &self.reserved_count())
            .field("failed", &self.failed.lock().map(|f| f.len()).unwrap_or(0))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{Job, JobContext};
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize)]
    struct Ping {
        n: u32,
    }

    #[async_trait::async_trait]
    impl Job for Ping {
        const NAME: &'static str = "test.ping";
        async fn handle(&self, _: &JobContext) -> Result<()> {
            Ok(())
        }
    }

    fn ping(n: u32) -> QueuedJob {
        QueuedJob::from_job(&Ping { n }).unwrap()
    }

    #[tokio::test]
    async fn a_pushed_job_can_be_reserved() {
        let queue = MemoryQueue::new();
        queue.push(ping(1)).await.unwrap();

        assert_eq!(queue.size("default").await.unwrap(), 1);
        let reserved = queue.reserve("default").await.unwrap().expect("a job should be waiting");
        assert_eq!(reserved.payload["n"], 1);
    }

    #[tokio::test]
    async fn reserving_increments_the_attempt_count() {
        let queue = MemoryQueue::new();
        queue.push(ping(1)).await.unwrap();

        let reserved = queue.reserve("default").await.unwrap().unwrap();
        assert_eq!(reserved.attempts, 1);
    }

    #[tokio::test]
    async fn a_reserved_job_is_invisible_to_other_workers() {
        let queue = MemoryQueue::new();
        queue.push(ping(1)).await.unwrap();

        queue.reserve("default").await.unwrap().unwrap();
        assert!(
            queue.reserve("default").await.unwrap().is_none(),
            "two workers must not get the same job"
        );
        assert_eq!(queue.reserved_count(), 1);
    }

    #[tokio::test]
    async fn acknowledging_removes_it_for_good() {
        let queue = MemoryQueue::new();
        queue.push(ping(1)).await.unwrap();

        let job = queue.reserve("default").await.unwrap().unwrap();
        queue.acknowledge(&job).await.unwrap();

        assert_eq!(queue.reserved_count(), 0);
        assert_eq!(queue.size("default").await.unwrap(), 0);
    }

    #[tokio::test]
    async fn releasing_puts_it_back_with_its_attempt_count_kept() {
        let queue = MemoryQueue::new();
        queue.push(ping(1)).await.unwrap();

        let job = queue.reserve("default").await.unwrap().unwrap();
        queue.release(&job, Duration::ZERO).await.unwrap();

        assert_eq!(queue.reserved_count(), 0);
        let again = queue.reserve("default").await.unwrap().unwrap();
        assert_eq!(again.attempts, 2, "the retry counts as a further attempt");
    }

    #[tokio::test]
    async fn releasing_with_a_delay_holds_it_back() {
        let queue = MemoryQueue::new();
        queue.push(ping(1)).await.unwrap();

        let job = queue.reserve("default").await.unwrap().unwrap();
        queue.release(&job, Duration::from_secs(60)).await.unwrap();

        assert_eq!(queue.size("default").await.unwrap(), 1, "it is back in the queue");
        assert!(queue.reserve("default").await.unwrap().is_none(), "but not yet available");
    }

    #[tokio::test]
    async fn a_delayed_job_does_not_block_the_ones_behind_it() {
        let queue = MemoryQueue::new();
        queue.push(ping(1).delayed_by(Duration::from_secs(60))).await.unwrap();
        queue.push(ping(2)).await.unwrap();

        let reserved = queue.reserve("default").await.unwrap().expect("the ready job");
        assert_eq!(reserved.payload["n"], 2);
    }

    #[tokio::test]
    async fn failing_moves_it_to_the_failed_store() {
        let queue = MemoryQueue::new();
        queue.push(ping(1)).await.unwrap();

        let job = queue.reserve("default").await.unwrap().unwrap();
        queue.fail(&job, "connection refused").await.unwrap();

        assert_eq!(queue.reserved_count(), 0);
        let failed = queue.failed_jobs();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].error, "connection refused");
        assert_eq!(failed[0].job.id, job.id);
    }

    #[tokio::test]
    async fn queues_are_independent() {
        let queue = MemoryQueue::new();
        queue.push(ping(1).on_queue("emails")).await.unwrap();
        queue.push(ping(2)).await.unwrap();

        assert_eq!(queue.size("emails").await.unwrap(), 1);
        assert_eq!(queue.size("default").await.unwrap(), 1);
        assert_eq!(queue.size("nothing-here").await.unwrap(), 0);

        let reserved = queue.reserve("emails").await.unwrap().unwrap();
        assert_eq!(reserved.payload["n"], 1);
    }

    #[tokio::test]
    async fn jobs_come_back_in_the_order_they_went_in() {
        let queue = MemoryQueue::new();
        for n in 1..=3 {
            queue.push(ping(n)).await.unwrap();
        }

        for expected in 1..=3 {
            let job = queue.reserve("default").await.unwrap().unwrap();
            assert_eq!(job.payload["n"], expected);
        }
    }

    #[tokio::test]
    async fn clearing_discards_everything_pending() {
        let queue = MemoryQueue::new();
        queue.push(ping(1)).await.unwrap();
        queue.push(ping(2)).await.unwrap();

        assert_eq!(queue.clear("default").await.unwrap(), 2);
        assert_eq!(queue.size("default").await.unwrap(), 0);
    }

    #[tokio::test]
    async fn an_empty_queue_reserves_nothing() {
        let queue = MemoryQueue::new();
        assert!(queue.reserve("default").await.unwrap().is_none());
        assert_eq!(queue.size("default").await.unwrap(), 0);
    }
}
