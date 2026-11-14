//! # rainier-queue
//!
//! Deferred work: the [`Job`] contract, the [`Queue`] port and its drivers, and
//! the [`Worker`] that runs them.
//!
//! ```
//! use rainier_queue::{Job, JobContext, JobRegistry, MemoryQueue, Queue, QueueManager};
//! use serde::{Deserialize, Serialize};
//! use std::sync::Arc;
//!
//! #[derive(Serialize, Deserialize)]
//! struct SendWelcomeEmail { user_id: u64 }
//!
//! #[async_trait::async_trait]
//! impl Job for SendWelcomeEmail {
//!     const NAME: &'static str = "mail.welcome";
//!     const QUEUE: &'static str = "mail";
//!
//!     async fn handle(&self, _context: &JobContext) -> rainier_support::Result<()> {
//!         Ok(())
//!     }
//! }
//!
//! # #[tokio::main] async fn main() -> rainier_support::Result<()> {
//! let registry = Arc::new(JobRegistry::new().with::<SendWelcomeEmail>());
//! let queue = Arc::new(MemoryQueue::new());
//! let dispatcher = QueueManager::new(queue.clone(), registry);
//!
//! dispatcher.dispatch(SendWelcomeEmail { user_id: 1 }).await?;
//! assert_eq!(queue.size("mail").await?, 1);
//! # Ok(()) }
//! ```
//!
//! ## A job crosses a process boundary
//!
//! Everything about the design follows from that. A job is written by a web
//! request and read, later, by a worker that may be a different process on a
//! different machine — so a job is a **serialisable payload plus a stable
//! name**, not a closure, and the worker needs a [`JobRegistry`] to turn the
//! name back into code. Dependencies cannot be captured either, so a running
//! job resolves them from the container through its [`JobContext`].
//!
//! ## Drivers
//!
//! | Driver | Survives a restart | For |
//! |---|---|---|
//! | [`SyncQueue`] | — runs inline | development, and tests that want the side effect |
//! | [`MemoryQueue`] | no | tests, single-process development |
//! | [`DatabaseQueue`] | yes | production, on the database you already have |
//! | `SqsQueue` | yes | production, managed — needs the `sqs` feature |
//!
//! ## Connections
//!
//! An application with more than one backend declares them as
//! [`Connections`] — a `default` and a named map, each entry naming its own
//! driver — and dispatches to one by name:
//!
//! ```ignore
//! queue.pending(RebuildIndex)?.on_connection("bulk").send().await?;
//! ```
//!
//! A name that is not declared is an **error**, never the default. That is the
//! one silent failure this whole area has: a job pushed to the wrong backend is
//! accepted, gets an id, and then waits in a store no worker drains — no error,
//! no retry, no failed-job row.
//!
//! ## When a job fails
//!
//! The [`Worker`] retries a failed job until its attempts run out, then fails
//! it for good. Each failed attempt fires [`JobReleased`] or [`JobFailed`]
//! (with [`Worker::with_events`]), which carry the failure both as text and as
//! a structured [`rainier_events::Exception`] — a panic included, flagged as
//! one.
//!
//! Every failed attempt is also **reported**, as a
//! [`JobAttemptFailed`](rainier_events::report::JobAttemptFailed) naming the
//! job, queue, connection and attempt — through [`Worker::with_reporter`], or
//! the process-wide reporter without one. So is every failure the queue would
//! otherwise only log: a broker refusing an acknowledgement, a release or a
//! fail, a reserve it retries, a message it had to throw away, a dead-letter
//! write, a Kubernetes call it worked around
//! ([`QueueOperationFailed`](rainier_events::report::QueueOperationFailed),
//! [`QueueMessageDiscarded`](rainier_events::report::QueueMessageDiscarded)).
//! See [`worker`] for the details.
//!
//! ## Testing
//!
//! [`QueueManager::fake`] records dispatches instead of performing them:
//!
//! ```
//! # use rainier_queue::{Job, JobContext, QueueManager};
//! # use serde::{Deserialize, Serialize};
//! # #[derive(Serialize, Deserialize)] struct SendInvoice;
//! # #[async_trait::async_trait] impl Job for SendInvoice {
//! #     const NAME: &'static str = "billing.invoice";
//! #     async fn handle(&self, _: &JobContext) -> rainier_support::Result<()> { Ok(()) }
//! # }
//! # #[tokio::main] async fn main() -> rainier_support::Result<()> {
//! let queue = QueueManager::fake();
//! queue.dispatch(SendInvoice).await?;
//! queue.assert_pushed_times::<SendInvoice>(1);
//! # Ok(()) }
//! ```

#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

pub mod connections;
pub mod database;
pub mod driver;
pub mod job;
#[cfg(feature = "kafka")]
pub mod kafka;
pub mod kubernetes;
pub mod manager;
pub mod queue;
#[cfg(feature = "redis")]
pub mod redis;
mod reporting;
#[cfg(feature = "sqs")]
pub mod sqs;
#[cfg(feature = "telemetry")]
pub mod telemetry;
pub mod worker;

pub use connections::{
    ConnectionConfig, Connections, DatabaseConnection, KafkaConnection, QueueDeclaration,
    QueueResources, RedisConnection, SqsConnection, SqsCredentials,
};
pub use database::{DatabaseQueue, FailedJobRow, JobRow};
pub use driver::QueueDriver;
pub use job::{Job, JobContext, JobRegistry, QueuedJob};
#[cfg(feature = "kafka")]
pub use kafka::{require_shared as require_shared_locks, KafkaQueue};
pub use kubernetes::{
    run_single_job, KubernetesJobSpec, ATTEMPT_ENV, JOB_NAME_ENV, PAYLOAD_ENV, QUEUED_ID_ENV,
    SINGLE_JOB_SUBCOMMAND,
};
#[cfg(feature = "kubernetes")]
pub use kubernetes::{KubernetesDispatcher, PodTiming, RbacReport};
pub use manager::{PendingDispatch, QueueManager, SyncQueue};
pub use queue::{FailedJob, MemoryQueue, Queue};
#[cfg(feature = "redis")]
pub use redis::{Keys as RedisQueueKeys, RedisQueue};
#[cfg(feature = "sqs")]
pub use sqs::SqsQueue;
#[cfg(feature = "telemetry")]
pub use telemetry::{JobOutcome, JobTiming, QueueObserver};
pub use worker::{
    JobFailed, JobProcessed, JobProcessing, JobReleased, Outcome, Worker, WorkerOptions,
    WorkerStats,
};

// Re-exported so job implementations get the attribute macro without adding
// the dependency themselves.
pub use async_trait::async_trait;
