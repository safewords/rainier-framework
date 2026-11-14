//! What Lookout records: [`Entry`] values, grouped into a [`Batch`] per unit
//! of work (a request, a job, a command, a scheduled task).
//!
//! These types are the contract between the recorder ([`crate::layer`]), the
//! stores ([`crate::store`]) and the dashboard ([`crate::http`]). They are all
//! `Serialize + Deserialize`: a store may keep them as JSON.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// What kind of thing an entry records.
///
/// Each corresponds to a watcher that can be toggled in
/// [`LookoutConfig`](crate::config::LookoutConfig).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum EntryType {
    /// An HTTP request handled by the kernel (`http.server.request`).
    Request,
    /// A database query (`db.query`).
    Query,
    /// A cache operation (`cache.*`).
    Cache,
    /// An outgoing HTTP call (`http.client.request`).
    HttpClient,
    /// A queued job's run (`queue.process`) or dispatch (`queue.publish`).
    Job,
    /// Mail being sent (`mail.send`).
    Mail,
    /// A notification being sent (`notification.send`).
    Notification,
    /// An event dispatched on the bus (`event.dispatch`).
    Event,
    /// A scheduled task (`schedule.task`).
    ScheduledTask,
    /// A console command (`console.command`).
    Command,
    /// A failure reported on the bus (`ExceptionReported`).
    Exception,
    /// A log line at or above the configured level.
    Log,
    /// A model created, updated or deleted.
    Model,
    /// A template render (`view.render`).
    View,
    /// A broadcast or socket operation.
    Broadcast,
    /// A session load or save (`session.*`).
    Session,
    /// An authentication attempt (`auth.attempt`).
    AuthAttempt,
    /// Derived: a run of near-identical queries — an N+1.
    NPlusOne,
    /// Derived: something that crossed its slow threshold.
    Slow,
    /// Anything else a custom watcher records.
    Custom,
}

impl EntryType {
    /// Every entry type, in declaration order.
    pub const ALL: &'static [EntryType] = &[
        EntryType::Request,
        EntryType::Query,
        EntryType::Cache,
        EntryType::HttpClient,
        EntryType::Job,
        EntryType::Mail,
        EntryType::Notification,
        EntryType::Event,
        EntryType::ScheduledTask,
        EntryType::Command,
        EntryType::Exception,
        EntryType::Log,
        EntryType::Model,
        EntryType::View,
        EntryType::Broadcast,
        EntryType::Session,
        EntryType::AuthAttempt,
        EntryType::NPlusOne,
        EntryType::Slow,
        EntryType::Custom,
    ];

    /// The snake_case name used in storage, config and the API.
    pub fn as_str(self) -> &'static str {
        match self {
            EntryType::Request => "request",
            EntryType::Query => "query",
            EntryType::Cache => "cache",
            EntryType::HttpClient => "http_client",
            EntryType::Job => "job",
            EntryType::Mail => "mail",
            EntryType::Notification => "notification",
            EntryType::Event => "event",
            EntryType::ScheduledTask => "scheduled_task",
            EntryType::Command => "command",
            EntryType::Exception => "exception",
            EntryType::Log => "log",
            EntryType::Model => "model",
            EntryType::View => "view",
            EntryType::Broadcast => "broadcast",
            EntryType::Session => "session",
            EntryType::AuthAttempt => "auth_attempt",
            EntryType::NPlusOne => "n_plus_one",
            EntryType::Slow => "slow",
            EntryType::Custom => "custom",
        }
    }

    /// Parse the snake_case name.
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|t| t.as_str() == name)
    }
}

impl std::fmt::Display for EntryType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One recorded thing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// Unique within the batch; stores may assign their own row id besides.
    pub id: String,
    /// The batch this belongs to — the trace id.
    pub batch_id: String,
    /// What kind of thing this is.
    #[serde(rename = "type")]
    pub entry_type: EntryType,
    /// The span this came from, when it came from one.
    pub span_id: Option<String>,
    /// The parent span, for drawing the waterfall.
    pub parent_span_id: Option<String>,
    /// The family key: what makes entries "the same thing" — the sanitised
    /// SQL, the route name or pattern, the job name, the exception
    /// fingerprint. Human-readable and possibly long; a store hashes it to
    /// index it. Aggregates are keyed by it.
    pub family_hash: Option<String>,
    /// A one-line summary for lists: `SELECT * FROM posts WHERE id = ?`,
    /// `GET /posts/{post} 200`, `SendInvoice`.
    pub summary: String,
    /// When it started.
    pub occurred_at: DateTime<Utc>,
    /// How long it took, when it has a duration.
    pub duration_us: Option<u64>,
    /// Tags for filtering: `slow`, `failed`, `user:42`, `route:posts.show`.
    pub tags: Vec<String>,
    /// The type-specific payload, already redacted.
    pub content: serde_json::Value,
}

/// What started a batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum BatchKind {
    /// An HTTP request.
    Request,
    /// A queued job.
    Job,
    /// A console command.
    Command,
    /// A scheduled task.
    ScheduledTask,
    /// Work outside any of those — a background task, boot.
    Other,
}

/// Everything recorded for one unit of work, kept or discarded as a whole.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Batch {
    /// Unique per batch: the trace id and the root span's id. A job shares
    /// its trace with the request that queued it, so the trace id alone
    /// would make the job's batch overwrite the request's.
    pub id: String,
    /// The trace id. The same for a request and every job it queued, via the
    /// propagated `traceparent` — filter on it to see the whole chain.
    pub trace_id: String,
    /// What started it.
    pub kind: BatchKind,
    /// A one-line summary: `GET /posts/{post} 200`, `job SendInvoice`.
    pub summary: String,
    /// The matched route's name, for a request.
    pub route_name: Option<String>,
    /// The matched route's pattern, for a request.
    pub route_pattern: Option<String>,
    /// The request method.
    pub method: Option<String>,
    /// The response status.
    pub status: Option<u16>,
    /// The guard that authenticated the request, if any.
    pub guard: Option<String>,
    /// The authenticated user's id, if any.
    pub user_id: Option<String>,
    /// How long the whole unit took.
    pub duration_us: u64,
    /// Whether any entry is an exception.
    pub has_exception: bool,
    /// Whether any entry crossed a slow threshold.
    pub has_slow: bool,
    /// The host (`HOSTNAME`) that recorded it.
    pub host: Option<String>,
    /// When it started.
    pub started_at: DateTime<Utc>,
    /// Batch-level tags (the union of the entries' and the batch's own).
    pub tags: Vec<String>,
    /// The entries, in the order they started.
    pub entries: Vec<Entry>,
}

impl Batch {
    /// How many entries of each type the batch holds — for list views.
    pub fn counts(&self) -> BTreeMap<EntryType, usize> {
        let mut counts = BTreeMap::new();
        for entry in &self.entries {
            *counts.entry(entry.entry_type).or_insert(0) += 1;
        }
        counts
    }

    /// The batch without its entries, for list views.
    pub fn summarize(&self) -> BatchSummary {
        BatchSummary {
            id: self.id.clone(),
            trace_id: self.trace_id.clone(),
            kind: self.kind,
            summary: self.summary.clone(),
            route_name: self.route_name.clone(),
            method: self.method.clone(),
            status: self.status,
            guard: self.guard.clone(),
            user_id: self.user_id.clone(),
            duration_us: self.duration_us,
            has_exception: self.has_exception,
            has_slow: self.has_slow,
            host: self.host.clone(),
            started_at: self.started_at,
            tags: self.tags.clone(),
            counts: self.counts(),
        }
    }
}

/// A batch without its entries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BatchSummary {
    /// See [`Batch::id`].
    pub id: String,
    /// See [`Batch::trace_id`].
    pub trace_id: String,
    /// See [`Batch::kind`].
    pub kind: BatchKind,
    /// See [`Batch::summary`].
    pub summary: String,
    /// See [`Batch::route_name`].
    pub route_name: Option<String>,
    /// See [`Batch::method`].
    pub method: Option<String>,
    /// See [`Batch::status`].
    pub status: Option<u16>,
    /// See [`Batch::guard`].
    pub guard: Option<String>,
    /// See [`Batch::user_id`].
    pub user_id: Option<String>,
    /// See [`Batch::duration_us`].
    pub duration_us: u64,
    /// See [`Batch::has_exception`].
    pub has_exception: bool,
    /// See [`Batch::has_slow`].
    pub has_slow: bool,
    /// See [`Batch::host`].
    pub host: Option<String>,
    /// See [`Batch::started_at`].
    pub started_at: DateTime<Utc>,
    /// See [`Batch::tags`].
    pub tags: Vec<String>,
    /// How many entries of each type.
    pub counts: BTreeMap<EntryType, usize>,
}

/// A rolled-up measurement: how many times `key` of `entry_type` happened in
/// a bucket, and how long it took in total and at worst.
///
/// What makes "slowest routes over the last week" cheap after the raw entries
/// are pruned.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Aggregate {
    /// The bucket's start.
    pub bucket_start: DateTime<Utc>,
    /// The bucket's length, in seconds (60, 600, 3600).
    pub period_s: u32,
    /// What was measured.
    pub entry_type: EntryType,
    /// The thing measured: a route name, a sanitised SQL, a job name.
    pub key: String,
    /// How many times.
    pub count: u64,
    /// Total duration, in microseconds.
    pub sum_us: u64,
    /// Worst duration, in microseconds.
    pub max_us: u64,
}
