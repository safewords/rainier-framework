//! Where Lookout keeps what it records.
//!
//! [`Store`] is the seam: the recorder writes [`Batch`]es through it from a
//! background task, the dashboard reads through it, and `lookout:prune` trims
//! it. Implementations: [`memory`] (one process, for development and tests),
//! `database` (its own tables and migrations) and `cache` (a ring buffer in the
//! application's cache — Redis in production).

use chrono::{DateTime, Utc};
use rainier_support::{BoxFuture, Result};
use serde::{Deserialize, Serialize};

use crate::entry::{Aggregate, Batch, BatchKind, BatchSummary, Entry, EntryType};

pub mod cache;
pub mod conformance;
pub mod database;
pub mod memory;

/// What to list. Every field narrows; `None` / empty means "any".
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BatchQuery {
    /// Only batches of this kind.
    pub kind: Option<BatchKind>,
    /// Only batches holding at least one entry of this type.
    pub entry_type: Option<EntryType>,
    /// Only batches in this trace — a request and the jobs it queued.
    pub trace_id: Option<String>,
    /// Only this route name.
    pub route_name: Option<String>,
    /// Only this user.
    pub user_id: Option<String>,
    /// Only batches with this tag.
    pub tag: Option<String>,
    /// Only batches with an exception.
    pub has_exception: Option<bool>,
    /// Only batches with something slow.
    pub has_slow: Option<bool>,
    /// Only batches that started before this — the cursor for paging.
    pub before: Option<DateTime<Utc>>,
    /// At most this many; the store caps it at 200 and defaults to 50.
    pub limit: Option<usize>,
}

/// What to roll up.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AggregateQuery {
    /// What was measured.
    pub entry_type: EntryType,
    /// The bucket length, in seconds.
    pub period_s: u32,
    /// From when.
    pub since: DateTime<Utc>,
    /// How to order the keys.
    pub order: AggregateOrder,
    /// At most this many keys.
    pub limit: usize,
}

/// How [`AggregateQuery`] orders its keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AggregateOrder {
    /// Most frequent first.
    Count,
    /// Slowest on average first.
    Average,
    /// Slowest worst case first.
    Max,
}

/// Where Lookout keeps what it records.
///
/// Object-safe (`Arc<dyn Store>`), and called from a background writer, never
/// from the request path.
pub trait Store: Send + Sync + 'static {
    /// The store's name, for `lookout:status`.
    fn name(&self) -> &'static str;

    /// Persist batches. Also rolls their entries into aggregates.
    fn store(&self, batches: Vec<Batch>) -> BoxFuture<'_, Result<()>>;

    /// Newest first.
    fn batches(&self, query: BatchQuery) -> BoxFuture<'_, Result<Vec<BatchSummary>>>;

    /// One batch, with its entries.
    fn batch(&self, id: &str) -> BoxFuture<'_, Result<Option<Batch>>>;

    /// Entries of one type across batches, newest first — the "all queries"
    /// and "all exceptions" views.
    fn entries(
        &self,
        entry_type: EntryType,
        before: Option<DateTime<Utc>>,
        limit: usize,
    ) -> BoxFuture<'_, Result<Vec<Entry>>>;

    /// Rolled-up measurements.
    fn aggregates(&self, query: AggregateQuery) -> BoxFuture<'_, Result<Vec<Aggregate>>>;

    /// Delete raw batches that started before `batches_before`, and aggregates
    /// whose bucket started before `aggregates_before`. Returns how many
    /// batches went. Deletes in bounded chunks.
    fn prune(
        &self,
        batches_before: DateTime<Utc>,
        aggregates_before: DateTime<Utc>,
    ) -> BoxFuture<'_, Result<u64>>;

    /// Delete everything.
    fn clear(&self) -> BoxFuture<'_, Result<()>>;
}
