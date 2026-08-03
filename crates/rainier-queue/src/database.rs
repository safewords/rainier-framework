//! The [`DatabaseQueue`] driver — jobs that survive a restart.
//!
//! Rows in two tables, defined as ordinary Rainier ORM entities, so the queue
//! runs on whatever backend the application already uses (SQLite, D1, MySQL,
//! Postgres) with no extra infrastructure.
//!
//! ## Reserving without losing or duplicating jobs
//!
//! Two workers polling the same table will see the same row. A plain
//! `SELECT … LIMIT 1` followed by `UPDATE` would hand it to both.
//!
//! The fix here is an **optimistic claim**: select a candidate, then update it
//! with `WHERE id = ? AND reserved_at IS NULL` and check the affected-row
//! count. Exactly one worker's update touches a row; the losers see zero and
//! try the next candidate. That needs no `SELECT … FOR UPDATE`, no advisory
//! lock, and no dialect-specific syntax — which matters because D1 has none of
//! them.
//!
//! A worker that dies mid-job leaves a row reserved forever, so a reservation
//! also carries a deadline: [`reclaim_expired`](DatabaseQueue::reclaim_expired)
//! releases anything held past it.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use rainier_database::{Criteria, Database, EntityRepository, Model, Repository};
use rainier_orm::Entity;
use rainier_support::{BoxFuture, Error, Result};

use crate::job::QueuedJob;
use crate::queue::Queue;

/// A queued job, as a row.
#[derive(Entity, Clone, Debug, PartialEq)]
#[orm(table = "rainier_jobs")]
#[orm(index = "queue, reserved_at")]
pub struct JobRow {
    /// The queued job's id.
    #[orm(pk)]
    pub id: String,
    /// Which queue it is on.
    #[orm(index)]
    pub queue: String,
    /// The job's registered name.
    pub name: String,
    /// Its serialised body, as JSON text.
    pub payload: String,
    /// How many times it has been attempted.
    pub attempts: i64,
    /// How many attempts it gets.
    pub max_attempts: i64,
    /// The earliest it may run.
    pub available_at: DateTime<Utc>,
    /// When a worker claimed it, or `NULL` if it is free.
    pub reserved_at: Option<DateTime<Utc>>,
    /// When the current reservation lapses.
    pub reserved_until: Option<DateTime<Utc>>,
    /// When it was enqueued.
    pub created_at: DateTime<Utc>,
    /// The uniqueness lock this job holds, if it declared a `unique_id`.
    ///
    /// A column rather than something folded into the payload, because the
    /// worker needs it to release the lock and reading it should not mean
    /// deserialising the job.
    pub unique_key: Option<String>,
}

impl Model for JobRow {}

/// A job that exhausted its attempts, as a row.
#[derive(Entity, Clone, Debug, PartialEq)]
#[orm(table = "rainier_failed_jobs")]
pub struct FailedJobRow {
    /// The failed job's id — the same id it had while queued.
    #[orm(pk)]
    pub id: String,
    /// Which queue it was on.
    #[orm(index)]
    pub queue: String,
    /// The job's registered name.
    pub name: String,
    /// Its serialised body.
    pub payload: String,
    /// The last error's message.
    pub error: String,
    /// When it gave up.
    pub failed_at: DateTime<Utc>,
}

impl Model for FailedJobRow {}

impl JobRow {
    fn from_queued(job: &QueuedJob) -> Result<Self> {
        Ok(Self {
            id: job.id.clone(),
            queue: job.queue.clone(),
            name: job.name.clone(),
            payload: serde_json::to_string(&job.payload)?,
            attempts: job.attempts as i64,
            max_attempts: job.max_attempts as i64,
            available_at: job.available_at,
            reserved_at: None,
            reserved_until: None,
            created_at: job.created_at,
            unique_key: job.unique_key.clone(),
        })
    }

    fn into_queued(self) -> Result<QueuedJob> {
        Ok(QueuedJob {
            id: self.id,
            name: self.name,
            payload: serde_json::from_str(&self.payload)?,
            queue: self.queue,
            attempts: self.attempts.max(0) as u32,
            max_attempts: self.max_attempts.max(0) as u32,
            available_at: self.available_at,
            created_at: self.created_at,
            unique_key: self.unique_key,
        })
    }
}

/// A queue backed by database rows.
pub struct DatabaseQueue {
    db: Database,
    jobs: Arc<dyn Repository<JobRow>>,
    failed: Arc<dyn Repository<FailedJobRow>>,
    /// How long a reservation lasts before another worker may reclaim it.
    reservation: Duration,
    /// How many candidates to try before giving up a reserve attempt.
    ///
    /// Bounded so a burst of contention cannot spin: with N workers racing,
    /// each loses at most this many claims before returning idle and being
    /// retried on the next poll.
    max_claim_attempts: usize,
}

impl DatabaseQueue {
    /// How long a claim lasts when a connection does not say.
    ///
    /// Public, and read by [`ConnectionConfig`](crate::ConnectionConfig) rather
    /// than copied into it: the check that a reservation outlives the worker's
    /// timeout is worthless if it compares against a stale duplicate of this
    /// number.
    pub const DEFAULT_RESERVATION: Duration = Duration::from_secs(90);

    /// A database queue over `db`.
    pub fn new(db: Database) -> Self {
        Self {
            jobs: Arc::new(EntityRepository::<JobRow>::new(db.clone())),
            failed: Arc::new(EntityRepository::<FailedJobRow>::new(db.clone())),
            db,
            reservation: Self::DEFAULT_RESERVATION,
            max_claim_attempts: 5,
        }
    }

    /// How long a worker's claim on a job lasts.
    ///
    /// Must exceed the worker's job timeout, or a job could be reclaimed and
    /// run twice while the first attempt is still going.
    pub fn with_reservation(mut self, reservation: Duration) -> Self {
        self.reservation = reservation;
        self
    }

    /// The migrations this driver needs.
    pub fn migrations() -> rainier_database::Migrator {
        rainier_database::Migrator::new()
            .create_table::<JobRow>("rainier_queue_0001_jobs")
            .create_table::<FailedJobRow>("rainier_queue_0002_failed_jobs")
    }

    /// Release every job whose reservation has lapsed, so a job held by a
    /// worker that died becomes available again. Returns how many.
    pub async fn reclaim_expired(&self) -> Result<u64> {
        let stale = self
            .jobs
            .matching(
                Criteria::new()
                    .where_not_null("reserved_at")
                    .where_lt("reserved_until", Utc::now())
                    .limit(100),
            )
            .await?;

        let mut reclaimed = 0;
        for mut row in stale {
            row.reserved_at = None;
            row.reserved_until = None;
            reclaimed += self.jobs.update(&row).await?;
        }
        Ok(reclaimed)
    }

    /// Every failed job, newest first.
    pub async fn failed_jobs(&self, limit: u64) -> Result<Vec<FailedJobRow>> {
        self.jobs_repository_failed(limit).await
    }

    async fn jobs_repository_failed(&self, limit: u64) -> Result<Vec<FailedJobRow>> {
        self.failed.matching(Criteria::new().order_by_desc("failed_at").limit(limit)).await
    }

    /// Put a failed job back on its queue for another run.
    pub async fn retry_failed(&self, id: &str) -> Result<bool> {
        let Some(row) = self.failed.find(id.into()).await? else {
            return Ok(false);
        };

        self.jobs
            .create(JobRow {
                id: row.id.clone(),
                queue: row.queue.clone(),
                name: row.name.clone(),
                payload: row.payload.clone(),
                attempts: 0,
                max_attempts: 1,
                available_at: Utc::now(),
                reserved_at: None,
                reserved_until: None,
                created_at: Utc::now(),
                // Not re-claimed. The original claim was released when the job
                // failed, and taking a new one here would mean a retry could be
                // refused by a *fresh* dispatch that happened in between —
                // which is the opposite of what retrying a failure should do.
                unique_key: None,
            })
            .await?;

        self.failed.delete(id.into()).await?;
        Ok(true)
    }

    /// Try to claim `row` for this worker. `Ok(true)` if we got it.
    async fn claim(&self, row: &JobRow) -> Result<bool> {
        let now = Utc::now();
        let mut claimed = row.clone();
        claimed.attempts += 1;
        claimed.reserved_at = Some(now);
        claimed.reserved_until = Some(
            now + chrono::Duration::from_std(self.reservation)
                .unwrap_or_else(|_| chrono::Duration::seconds(90)),
        );

        // The race is resolved here: the update only matches while the row is
        // still unreserved, so exactly one worker's update affects a row.
        let affected = self
            .db
            .execute(rainier_database::statement::update_matching::<JobRow>(
                self.db.dialect(),
                &Criteria::new().where_eq("id", row.id.clone()).where_null("reserved_at"),
                vec![
                    ("attempts".into(), claimed.attempts.into()),
                    ("reserved_at".into(), now.into()),
                    ("reserved_until".into(), claimed.reserved_until.expect("just set").into()),
                ],
            ))
            .await?
            .rows_affected;

        Ok(affected == 1)
    }
}

impl Queue for DatabaseQueue {
    fn name(&self) -> &str {
        "database"
    }

    fn push<'a>(&'a self, job: QueuedJob) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let row = JobRow::from_queued(&job)?;
            let id = row.id.clone();
            self.jobs.create(row).await?;
            Ok(id)
        })
    }

    fn reserve<'a>(&'a self, queue: &'a str) -> BoxFuture<'a, Result<Option<QueuedJob>>> {
        Box::pin(async move {
            let candidates = self
                .jobs
                .matching(
                    Criteria::new()
                        .where_eq("queue", queue)
                        .where_null("reserved_at")
                        .where_lte("available_at", Utc::now())
                        // Oldest first, so the queue is roughly FIFO.
                        .order_by("available_at")
                        .limit(self.max_claim_attempts as u64),
                )
                .await?;

            for row in candidates {
                if self.claim(&row).await? {
                    let mut queued = row.into_queued()?;
                    // `claim` incremented the stored count; reflect it here so
                    // the worker's attempt number matches the row's.
                    queued.attempts += 1;
                    return Ok(Some(queued));
                }
            }
            Ok(None)
        })
    }

    fn acknowledge<'a>(&'a self, job: &'a QueuedJob) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.jobs.delete(job.id.clone().into()).await?;
            Ok(())
        })
    }

    fn release<'a>(&'a self, job: &'a QueuedJob, delay: Duration) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let available_at = Utc::now()
                + chrono::Duration::from_std(delay).unwrap_or_else(|_| chrono::Duration::zero());

            self.db
                .execute(rainier_database::statement::update_matching::<JobRow>(
                    self.db.dialect(),
                    &Criteria::new().where_eq("id", job.id.clone()),
                    vec![
                        (
                            "reserved_at".into(),
                            rainier_orm::sea_query::Value::ChronoDateTimeUtc(None),
                        ),
                        (
                            "reserved_until".into(),
                            rainier_orm::sea_query::Value::ChronoDateTimeUtc(None),
                        ),
                        ("available_at".into(), available_at.into()),
                    ],
                ))
                .await?;
            Ok(())
        })
    }

    fn fail<'a>(&'a self, job: &'a QueuedJob, error: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.failed
                .create(FailedJobRow {
                    id: job.id.clone(),
                    queue: job.queue.clone(),
                    name: job.name.clone(),
                    payload: serde_json::to_string(&job.payload)?,
                    error: error.to_string(),
                    failed_at: Utc::now(),
                })
                .await
                .map_err(|e| {
                    Error::internal(format!("could not record the failed job `{}`: {e}", job.id))
                })?;

            self.jobs.delete(job.id.clone().into()).await?;
            Ok(())
        })
    }

    fn size<'a>(&'a self, queue: &'a str) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            self.jobs
                .count_matching(Criteria::new().where_eq("queue", queue).where_null("reserved_at"))
                .await
        })
    }

    fn clear<'a>(&'a self, queue: &'a str) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            self.jobs.delete_matching(Criteria::new().where_eq("queue", queue)).await
        })
    }
}

impl std::fmt::Debug for DatabaseQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DatabaseQueue").field("reservation", &self.reservation).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{Job, JobContext};
    use rainier_database::testing::{fake_database, MemoryConnection};
    use rainier_database::OwnedRow;
    use rainier_orm::Dialect;
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize)]
    struct Ping;

    #[async_trait::async_trait]
    impl Job for Ping {
        const NAME: &'static str = "test.ping";
        async fn handle(&self, _: &JobContext) -> Result<()> {
            Ok(())
        }
    }

    fn job_row(id: &str) -> OwnedRow {
        OwnedRow::new()
            .with("id", id)
            .with("queue", "default")
            .with("name", "test.ping")
            .with("payload", "null")
            .with("attempts", 0_i64)
            .with("max_attempts", 3_i64)
            .with("available_at", "2020-01-01T00:00:00Z")
            .with("reserved_at", None::<String>)
            .with("reserved_until", None::<String>)
            .with("created_at", "2020-01-01T00:00:00Z")
    }

    #[test]
    fn the_row_entities_describe_their_tables() {
        assert_eq!(JobRow::table(), "rainier_jobs");
        assert_eq!(FailedJobRow::table(), "rainier_failed_jobs");
        assert_eq!(JobRow::primary_key(), "id");
    }

    #[test]
    fn the_driver_ships_its_own_migrations() {
        let migrator = DatabaseQueue::migrations();
        assert_eq!(
            migrator.names(),
            vec!["rainier_queue_0001_jobs", "rainier_queue_0002_failed_jobs"]
        );
    }

    #[test]
    fn a_queued_job_round_trips_through_a_row() {
        let queued = QueuedJob::from_job(&Ping).unwrap().on_queue("mail");
        let row = JobRow::from_queued(&queued).unwrap();

        assert_eq!(row.queue, "mail");
        assert_eq!(row.name, "test.ping");
        assert!(row.reserved_at.is_none());

        let back = row.into_queued().unwrap();
        assert_eq!(back.id, queued.id);
        assert_eq!(back.name, queued.name);
        assert_eq!(back.max_attempts, queued.max_attempts);
    }

    #[tokio::test]
    async fn pushing_writes_a_row() {
        let (db, connection) = fake_database(MemoryConnection::new(Dialect::Sqlite));
        let queue = DatabaseQueue::new(db);

        queue.push(QueuedJob::from_job(&Ping).unwrap()).await.unwrap();

        let sql = connection.last_statement().unwrap();
        assert!(sql.starts_with("INSERT INTO"), "{sql}");
        assert!(sql.contains("rainier_jobs"), "{sql}");
    }

    #[tokio::test]
    async fn reserving_selects_only_free_available_jobs() {
        let (db, connection) = fake_database(MemoryConnection::new(Dialect::Sqlite));
        let queue = DatabaseQueue::new(db);

        queue.reserve("default").await.unwrap();

        let sql = connection.statements()[0].clone();
        assert!(sql.contains("reserved_at"), "{sql}");
        assert!(sql.contains("IS NULL"), "{sql}");
        assert!(sql.contains("available_at"), "{sql}");
        assert!(sql.contains("ORDER BY"), "{sql}");
    }

    #[tokio::test]
    async fn a_claim_is_conditional_on_the_job_still_being_free() {
        // The race guard: the UPDATE must carry `reserved_at IS NULL`, or two
        // workers could both believe they claimed the same job.
        let (db, connection) = fake_database(
            MemoryConnection::new(Dialect::Sqlite).returning([job_row("job-1")]).with_outcome(1, 0),
        );
        let queue = DatabaseQueue::new(db);

        let reserved = queue.reserve("default").await.unwrap();
        assert!(reserved.is_some());

        let update = connection
            .statements()
            .into_iter()
            .find(|s| s.starts_with("UPDATE"))
            .expect("the claim should be an UPDATE");
        assert!(update.contains("IS NULL"), "{update}");
        assert!(update.contains("reserved_at"), "{update}");
    }

    #[tokio::test]
    async fn losing_the_claim_race_yields_no_job() {
        // `rows_affected == 0` means another worker got there first.
        let (db, _) = fake_database(
            MemoryConnection::new(Dialect::Sqlite).returning([job_row("job-1")]).with_outcome(0, 0),
        );
        let queue = DatabaseQueue::new(db);

        assert!(queue.reserve("default").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_reserved_job_reports_the_attempt_the_row_now_holds() {
        let (db, _) = fake_database(
            MemoryConnection::new(Dialect::Sqlite).returning([job_row("job-1")]).with_outcome(1, 0),
        );
        let queue = DatabaseQueue::new(db);

        let reserved = queue.reserve("default").await.unwrap().unwrap();
        assert_eq!(reserved.attempts, 1, "the claim incremented it");
    }

    #[tokio::test]
    async fn acknowledging_deletes_the_row() {
        let (db, connection) = fake_database(MemoryConnection::new(Dialect::Sqlite));
        let queue = DatabaseQueue::new(db);

        let job = QueuedJob::from_job(&Ping).unwrap();
        queue.acknowledge(&job).await.unwrap();

        assert!(connection.last_statement().unwrap().starts_with("DELETE FROM"));
    }

    #[tokio::test]
    async fn releasing_clears_the_reservation_and_pushes_availability_out() {
        let (db, connection) = fake_database(MemoryConnection::new(Dialect::Sqlite));
        let queue = DatabaseQueue::new(db);

        let job = QueuedJob::from_job(&Ping).unwrap();
        queue.release(&job, Duration::from_secs(30)).await.unwrap();

        let sql = connection.last_statement().unwrap();
        assert!(sql.starts_with("UPDATE"), "{sql}");
        assert!(sql.contains("reserved_at"), "{sql}");
        assert!(sql.contains("available_at"), "{sql}");
    }

    #[tokio::test]
    async fn failing_records_the_row_before_removing_the_job() {
        let (db, connection) = fake_database(MemoryConnection::new(Dialect::Sqlite));
        let queue = DatabaseQueue::new(db);

        let job = QueuedJob::from_job(&Ping).unwrap();
        queue.fail(&job, "exploded").await.unwrap();

        let statements = connection.statements();
        let insert = statements.iter().position(|s| s.contains("rainier_failed_jobs"));
        let delete = statements.iter().position(|s| s.starts_with("DELETE FROM"));

        assert!(insert.is_some(), "{statements:?}");
        assert!(delete.is_some(), "{statements:?}");
        assert!(insert < delete, "record the failure before dropping the job");
    }

    #[tokio::test]
    async fn size_counts_only_unreserved_jobs() {
        let (db, connection) = fake_database(
            MemoryConnection::new(Dialect::Sqlite).returning([OwnedRow::new().with("cnt", 7_i64)]),
        );
        let queue = DatabaseQueue::new(db);

        assert_eq!(queue.size("default").await.unwrap(), 7);
        let sql = connection.last_statement().unwrap();
        assert!(sql.contains("COUNT"), "{sql}");
        assert!(sql.contains("IS NULL"), "{sql}");
    }

    #[tokio::test]
    async fn reclaiming_releases_jobs_held_past_their_deadline() {
        let (db, connection) = fake_database(MemoryConnection::new(Dialect::Sqlite));
        let queue = DatabaseQueue::new(db);

        queue.reclaim_expired().await.unwrap();

        let sql = connection.last_statement().unwrap();
        assert!(sql.contains("reserved_until"), "{sql}");
        assert!(sql.contains("IS NOT NULL"), "{sql}");
    }
}
