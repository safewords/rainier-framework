//! [`DatabaseStore`]: Lookout's own tables, with their own migrations.
//!
//! Four tables, defined as ORM entities so the same code runs on every dialect
//! Rainier supports (SQLite and D1, MySQL/MariaDB, Postgres):
//!
//! | Table | Key | Holds |
//! |---|---|---|
//! | `lookout_batches` | `id` (the trace id) | one row per batch: the [`BatchSummary`] columns, tags and per-type counts as JSON |
//! | `lookout_entries` | `(batch_id, seq)` | one row per entry, its payload as JSON text |
//! | `lookout_batch_tags` | `(batch_id, tag)` | one row per batch tag, so the tag filter is an indexed lookup |
//! | `lookout_aggregates` | `(bucket_start, period_s, type, key_hash)` | rolled-up counts and durations |
//!
//! Register [`DatabaseStore::migrations`] with the application's migrator.
//!
//! ## Choices worth knowing
//!
//! - **Times are signed `BIGINT` microseconds** (`started_at_us`,
//!   `occurred_at_us`; `bucket_start` is Unix seconds). The ORM's timestamp
//!   column is a MySQL `TIMESTAMP`, which keeps whole seconds and ends in
//!   2038; an integer keeps the microseconds the cursor pages by, sorts the
//!   same everywhere and needs no dialect-specific parsing.
//! - **Durations, counts and sums are signed `BIGINT`.** MariaDB returns
//!   `SUM` over an unsigned column as `DECIMAL`, which SQLite tests never
//!   show. No sum is computed in SQL here anyway: aggregates are added by an
//!   atomic `INSERT … ON CONFLICT` increment, and rolled up across buckets in
//!   Rust.
//! - **Batch tags get their own table; entry tags stay inline.** The listing
//!   filters on a batch tag, which a JSON column can only answer with a
//!   `LIKE` scan that dialects escape differently. Nothing queries entry tags,
//!   so they ride along in the entry row.
//! - **Indexed strings are `VARCHAR(255)`** (MySQL cannot index `TEXT`), so
//!   ids, route names, user ids and tags longer than 255 characters are
//!   truncated on write. The family key itself is `TEXT`; `family_hash` and
//!   `key_hash` are a 16-hex-digit FNV-1a hash of it, for the index.
//! - **No transactions.** Rainier's connection port has none to offer, so a
//!   write that fails half-way can leave a batch without some of its entries.
//!   Re-storing the same id cleans that up: a store first deletes every row of
//!   the ids it is about to write, which is also how a replayed write
//!   replaces rather than duplicates.
//! - **The aggregate maximum is a guarded update.** The upsert increments
//!   `count` and `sum_us`; `max_us` is then raised with
//!   `UPDATE … SET max_us = ? WHERE <key> AND max_us < ?`. Both are atomic per
//!   row, so two writers racing on one bucket lose nothing.
//! - **Statements stay under a bind-parameter budget**
//!   ([`with_max_bind_params`](DatabaseStore::with_max_bind_params), 900 by
//!   default — under SQLite's historical 999). Cloudflare D1 allows 100.
//!
//! Every statement runs inside a span carrying `rainier.lookout.internal =
//! true`, so Lookout does not record its own queries.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use rainier_database::{statement, Criteria, Database, Migrator, Subquery, Upsert};
use rainier_orm::Entity;
use rainier_support::{BoxFuture, Error, Result};
use tracing::Instrument as _;

use super::conformance::{effective_limit, select_aggregates};
use super::{AggregateQuery, BatchQuery, Store};
use crate::aggregate;
use crate::entry::{Aggregate, Batch, BatchKind, BatchSummary, Entry, EntryType};

/// The longest an indexed string column holds — see the module docs.
const KEYED_LEN: usize = 255;

/// The most rows one prune step selects and deletes.
const PRUNE_CHUNK: usize = 500;

/// A batch, as a row.
#[derive(Entity, Clone, Debug, PartialEq)]
#[orm(table = "lookout_batches")]
#[orm(index = "route_name, started_at_us")]
#[orm(index = "user_id, started_at_us")]
struct BatchRow {
    #[orm(pk)]
    id: String,
    #[orm(index)]
    trace_id: String,
    kind: String,
    summary: String,
    route_name: Option<String>,
    route_pattern: Option<String>,
    method: Option<String>,
    status: Option<i64>,
    guard: Option<String>,
    user_id: Option<String>,
    duration_us: i64,
    has_exception: bool,
    has_slow: bool,
    host: Option<String>,
    #[orm(index)]
    started_at_us: i64,
    /// JSON array.
    tags: String,
    /// JSON object, entry type → count, so a listing never reads entries.
    counts: String,
}

/// An entry, as a row.
#[derive(Entity, Clone, Debug, PartialEq)]
#[orm(table = "lookout_entries")]
#[orm(index = "type, occurred_at_us")]
struct EntryRow {
    /// With `seq`, the key; its prefix serves "the entries of this batch".
    #[orm(pk)]
    batch_id: String,
    /// Position in the batch, which is the order entries come back in.
    #[orm(pk)]
    seq: i64,
    entry_id: String,
    #[orm(column = "type")]
    entry_type: String,
    span_id: Option<String>,
    parent_span_id: Option<String>,
    family_key: Option<String>,
    #[orm(index)]
    family_hash: Option<String>,
    summary: String,
    occurred_at_us: i64,
    duration_us: Option<i64>,
    /// JSON array.
    tags: String,
    /// JSON.
    content: String,
}

/// A batch tag, as a row.
#[derive(Entity, Clone, Debug, PartialEq)]
#[orm(table = "lookout_batch_tags")]
#[orm(index = "tag, started_at_us")]
struct TagRow {
    #[orm(pk)]
    batch_id: String,
    #[orm(pk)]
    tag: String,
    started_at_us: i64,
}

/// An aggregate bucket, as a row.
#[derive(Entity, Clone, Debug, PartialEq)]
#[orm(table = "lookout_aggregates")]
#[orm(index = "type, period_s, bucket_start")]
struct AggregateRow {
    /// Unix seconds.
    #[orm(pk)]
    bucket_start: i64,
    #[orm(pk)]
    period_s: i64,
    #[orm(pk, column = "type")]
    entry_type: String,
    #[orm(pk)]
    key_hash: String,
    key: String,
    count: i64,
    sum_us: i64,
    max_us: i64,
}

/// Lookout's own tables, on any Rainier database connection.
///
/// ```ignore
/// migrator = migrator.merge(DatabaseStore::migrations());
/// let store = DatabaseStore::new(db.clone());
/// ```
pub struct DatabaseStore {
    db: Database,
    max_bind_params: usize,
}

impl DatabaseStore {
    /// A store on `db`. Run [`migrations`](Self::migrations) first.
    pub fn new(db: Database) -> Self {
        Self { db, max_bind_params: 900 }
    }

    /// The most bind parameters one statement may carry — 100 on Cloudflare
    /// D1. Inserts and deletes are chunked to stay under it.
    pub fn with_max_bind_params(mut self, max: usize) -> Self {
        self.max_bind_params = max.max(16);
        self
    }

    /// The migrations this store needs, named `rainier_lookout_000N_…`.
    pub fn migrations() -> Migrator {
        Migrator::new()
            .create_table::<BatchRow>("rainier_lookout_0001_batches")
            .create_table::<EntryRow>("rainier_lookout_0002_entries")
            .create_table::<TagRow>("rainier_lookout_0003_batch_tags")
            .create_table::<AggregateRow>("rainier_lookout_0004_aggregates")
    }

    /// How many rows of `columns` columns fit one statement.
    fn rows_per_statement(&self, columns: usize) -> usize {
        (self.max_bind_params / columns.max(1)).max(1)
    }

    /// `SELECT * FROM <E> WHERE <criteria>`. Through the statement layer rather
    /// than a repository: three of the four tables have composite keys, which
    /// a repository (and `Model`) does not take.
    async fn select<E: Entity>(&self, criteria: Criteria) -> Result<Vec<E>> {
        self.db.fetch_all::<E>(statement::select_matching::<E>(self.db.dialect(), &criteria)).await
    }

    /// `DELETE FROM <E> WHERE <criteria>`. Returns rows affected.
    async fn delete<E: Entity>(&self, criteria: Criteria) -> Result<u64> {
        Ok(self
            .db
            .execute(statement::delete_matching::<E>(self.db.dialect(), &criteria))
            .await?
            .rows_affected)
    }

    async fn insert_all<E: Entity>(&self, rows: &[E], columns: usize) -> Result<()> {
        for chunk in rows.chunks(self.rows_per_statement(columns)) {
            self.db.execute(statement::insert_many::<E>(self.db.dialect(), chunk)?).await?;
        }
        Ok(())
    }

    /// Delete every row of these batch ids, in all three batch tables.
    async fn delete_batches(&self, ids: &[String]) -> Result<u64> {
        let mut deleted = 0;
        for chunk in ids.chunks(self.max_bind_params.min(PRUNE_CHUNK)) {
            let ids = chunk.to_vec();
            self.delete::<EntryRow>(Criteria::new().where_in("batch_id", ids.clone())).await?;
            self.delete::<TagRow>(Criteria::new().where_in("batch_id", ids.clone())).await?;
            deleted += self.delete::<BatchRow>(Criteria::new().where_in("id", ids)).await?;
        }
        Ok(deleted)
    }

    async fn write(&self, batches: Vec<Batch>) -> Result<()> {
        // The last write of an id within one call wins, as it would across calls.
        let mut unique: BTreeMap<String, Batch> = BTreeMap::new();
        for batch in batches {
            unique.insert(keyed(&batch.id), batch);
        }
        if unique.is_empty() {
            return Ok(());
        }
        let batches: Vec<Batch> = unique.into_values().collect();

        let ids: Vec<String> = batches.iter().map(|b| keyed(&b.id)).collect();
        self.delete_batches(&ids).await?;

        let mut batch_rows = Vec::with_capacity(batches.len());
        let mut entry_rows = Vec::new();
        let mut tag_rows = Vec::new();
        for batch in &batches {
            batch_rows.push(BatchRow::from_batch(batch)?);
            for (seq, entry) in batch.entries.iter().enumerate() {
                entry_rows.push(EntryRow::from_entry(&batch.id, seq, entry)?);
            }
            let mut seen = std::collections::BTreeSet::new();
            for tag in &batch.tags {
                if seen.insert(keyed(tag)) {
                    tag_rows.push(TagRow {
                        batch_id: keyed(&batch.id),
                        tag: keyed(tag),
                        started_at_us: micros(batch.started_at),
                    });
                }
            }
        }

        // Batches first: a write that fails later leaves a batch short of
        // entries, which is listed and pruned normally — never entries that
        // nothing points at.
        self.insert_all(&batch_rows, BatchRow::columns().len()).await?;
        self.insert_all(&tag_rows, TagRow::columns().len()).await?;
        self.insert_all(&entry_rows, EntryRow::columns().len()).await?;

        self.roll_up(&batches).await
    }

    async fn roll_up(&self, batches: &[Batch]) -> Result<()> {
        let rows: Vec<AggregateRow> =
            aggregate::rollup(batches).iter().map(AggregateRow::from_aggregate).collect();
        if rows.is_empty() {
            return Ok(());
        }
        let plan = Upsert::on(["bucket_start", "period_s", "type", "key_hash"])
            .increment(["count", "sum_us"]);
        for chunk in rows.chunks(self.rows_per_statement(AggregateRow::columns().len())) {
            self.db
                .execute(statement::upsert_many_with::<AggregateRow>(
                    self.db.dialect(),
                    chunk,
                    &plan,
                )?)
                .await?;
        }
        // Raise the maximum where this write's is larger. A fresh row already
        // holds it, and the guard makes the update a no-op then.
        for row in rows.iter().filter(|r| r.max_us > 0) {
            self.db
                .execute(statement::update_matching::<AggregateRow>(
                    self.db.dialect(),
                    &Criteria::new()
                        .where_eq("bucket_start", row.bucket_start)
                        .where_eq("period_s", row.period_s)
                        .where_eq("type", row.entry_type.clone())
                        .where_eq("key_hash", row.key_hash.clone())
                        .where_lt("max_us", row.max_us),
                    vec![("max_us".into(), row.max_us.into())],
                ))
                .await?;
        }
        Ok(())
    }

    async fn list(&self, query: BatchQuery) -> Result<Vec<BatchSummary>> {
        let limit = effective_limit(query.limit);
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut criteria = Criteria::new();
        if let Some(kind) = query.kind {
            criteria = criteria.where_eq("kind", kind_name(kind));
        }
        if let Some(trace) = &query.trace_id {
            criteria = criteria.where_eq("trace_id", keyed(trace));
        }
        if let Some(route) = &query.route_name {
            criteria = criteria.where_eq("route_name", keyed(route));
        }
        if let Some(user) = &query.user_id {
            criteria = criteria.where_eq("user_id", keyed(user));
        }
        if let Some(has) = query.has_exception {
            criteria = criteria.where_eq("has_exception", has);
        }
        if let Some(has) = query.has_slow {
            criteria = criteria.where_eq("has_slow", has);
        }
        if let Some(before) = query.before {
            criteria = criteria.where_lt("started_at_us", micros(before));
        }
        if let Some(entry_type) = query.entry_type {
            criteria = criteria.where_exists(
                Subquery::count(EntryRow::table())
                    .correlate("batch_id", "id")
                    .where_eq("type", entry_type.as_str()),
            );
        }
        if let Some(tag) = &query.tag {
            criteria = criteria.where_exists(
                Subquery::count(TagRow::table())
                    .correlate("batch_id", "id")
                    .where_eq("tag", keyed(tag)),
            );
        }
        let criteria =
            criteria.order_by_desc("started_at_us").order_by_desc("id").limit(limit as u64);
        self.select::<BatchRow>(criteria).await?.into_iter().map(BatchRow::into_summary).collect()
    }

    async fn find(&self, id: &str) -> Result<Option<Batch>> {
        let Some(row) = self
            .select::<BatchRow>(Criteria::new().where_eq("id", keyed(id)).limit(1))
            .await?
            .pop()
        else {
            return Ok(None);
        };
        let entries = self
            .select::<EntryRow>(Criteria::new().where_eq("batch_id", keyed(id)).order_by("seq"))
            .await?
            .into_iter()
            .map(EntryRow::into_entry)
            .collect::<Result<Vec<_>>>()?;
        let mut batch = row.into_batch()?;
        batch.entries = entries;
        Ok(Some(batch))
    }

    async fn of_type(
        &self,
        entry_type: EntryType,
        before: Option<DateTime<Utc>>,
        limit: usize,
    ) -> Result<Vec<Entry>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut criteria = Criteria::new().where_eq("type", entry_type.as_str());
        if let Some(before) = before {
            criteria = criteria.where_lt("occurred_at_us", micros(before));
        }
        let criteria = criteria
            .order_by_desc("occurred_at_us")
            .order_by_desc("batch_id")
            .order_by_desc("seq")
            .limit(limit as u64);
        self.select::<EntryRow>(criteria).await?.into_iter().map(EntryRow::into_entry).collect()
    }

    async fn rolled_up(&self, query: AggregateQuery) -> Result<Vec<Aggregate>> {
        let from = aggregate::bucket_start(query.since, query.period_s).timestamp();
        let rows = self
            .select::<AggregateRow>(
                Criteria::new()
                    .where_eq("type", query.entry_type.as_str())
                    .where_eq("period_s", i64::from(query.period_s))
                    .where_gte("bucket_start", from),
            )
            .await?
            .into_iter()
            .map(AggregateRow::into_aggregate)
            .collect::<Result<Vec<_>>>()?;
        Ok(select_aggregates(rows, &query))
    }

    async fn prune_rows(
        &self,
        batches_before: DateTime<Utc>,
        aggregates_before: DateTime<Utc>,
    ) -> Result<u64> {
        let chunk = self.max_bind_params.min(PRUNE_CHUNK);
        let cutoff = micros(batches_before);
        let mut pruned = 0;
        loop {
            let old = self
                .select::<BatchRow>(
                    Criteria::new()
                        .where_lt("started_at_us", cutoff)
                        .order_by("started_at_us")
                        .limit(chunk as u64),
                )
                .await?;
            let ids: Vec<String> = old.into_iter().map(|row| row.id).collect();
            pruned += self.delete_batches(&ids).await?;
            if ids.len() < chunk {
                break;
            }
        }

        // `bucket_start < aggregates_before`, in whole seconds.
        let cutoff = aggregates_before.timestamp()
            + i64::from(aggregates_before.timestamp_subsec_nanos() > 0);
        loop {
            let old = self
                .select::<AggregateRow>(
                    Criteria::new()
                        .where_lt("bucket_start", cutoff)
                        .order_by("bucket_start")
                        .limit(chunk as u64),
                )
                .await?;
            let Some(last) = old.last() else { break };
            if old.len() < chunk {
                self.delete::<AggregateRow>(Criteria::new().where_lt("bucket_start", cutoff))
                    .await?;
                break;
            }
            // Whole buckets at a time: at most one bucket beyond the chunk.
            self.delete::<AggregateRow>(
                Criteria::new().where_lte("bucket_start", last.bucket_start),
            )
            .await?;
        }
        Ok(pruned)
    }

    async fn clear_all(&self) -> Result<()> {
        for table in [EntryRow::table(), TagRow::table(), BatchRow::table(), AggregateRow::table()]
        {
            self.db.statement(&format!("DELETE FROM {table}")).await?;
        }
        Ok(())
    }
}

/// The span every store call runs in.
fn internal(operation: &'static str) -> tracing::Span {
    tracing::info_span!(
        "lookout.store",
        rainier.lookout.internal = true,
        lookout.store = "database",
        lookout.operation = operation
    )
}

impl Store for DatabaseStore {
    fn name(&self) -> &'static str {
        "database"
    }

    fn store(&self, batches: Vec<Batch>) -> BoxFuture<'_, Result<()>> {
        Box::pin(self.write(batches).instrument(internal("store")))
    }

    fn batches(&self, query: BatchQuery) -> BoxFuture<'_, Result<Vec<BatchSummary>>> {
        Box::pin(self.list(query).instrument(internal("batches")))
    }

    fn batch(&self, id: &str) -> BoxFuture<'_, Result<Option<Batch>>> {
        let id = id.to_string();
        Box::pin(async move { self.find(&id).await }.instrument(internal("batch")))
    }

    fn entries(
        &self,
        entry_type: EntryType,
        before: Option<DateTime<Utc>>,
        limit: usize,
    ) -> BoxFuture<'_, Result<Vec<Entry>>> {
        Box::pin(self.of_type(entry_type, before, limit).instrument(internal("entries")))
    }

    fn aggregates(&self, query: AggregateQuery) -> BoxFuture<'_, Result<Vec<Aggregate>>> {
        Box::pin(self.rolled_up(query).instrument(internal("aggregates")))
    }

    fn prune(
        &self,
        batches_before: DateTime<Utc>,
        aggregates_before: DateTime<Utc>,
    ) -> BoxFuture<'_, Result<u64>> {
        Box::pin(self.prune_rows(batches_before, aggregates_before).instrument(internal("prune")))
    }

    fn clear(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(self.clear_all().instrument(internal("clear")))
    }
}

impl std::fmt::Debug for DatabaseStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DatabaseStore")
            .field("dialect", &self.db.dialect())
            .field("max_bind_params", &self.max_bind_params)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Row conversions
// ---------------------------------------------------------------------------

impl BatchRow {
    fn from_batch(batch: &Batch) -> Result<Self> {
        Ok(Self {
            id: keyed(&batch.id),
            trace_id: keyed(&batch.trace_id),
            kind: kind_name(batch.kind).to_string(),
            summary: batch.summary.clone(),
            route_name: batch.route_name.as_deref().map(keyed),
            route_pattern: batch.route_pattern.clone(),
            method: batch.method.clone(),
            status: batch.status.map(i64::from),
            guard: batch.guard.clone(),
            user_id: batch.user_id.as_deref().map(keyed),
            duration_us: signed(batch.duration_us),
            has_exception: batch.has_exception,
            has_slow: batch.has_slow,
            host: batch.host.clone(),
            started_at_us: micros(batch.started_at),
            tags: serde_json::to_string(&batch.tags)?,
            counts: serde_json::to_string(&batch.counts())?,
        })
    }

    /// The batch, without its entries.
    fn into_batch(self) -> Result<Batch> {
        Ok(Batch {
            kind: parse_kind(&self.kind)?,
            started_at: from_micros(self.started_at_us)?,
            tags: serde_json::from_str(&self.tags)?,
            id: self.id,
            trace_id: self.trace_id,
            summary: self.summary,
            route_name: self.route_name,
            route_pattern: self.route_pattern,
            method: self.method,
            status: self.status.and_then(|s| u16::try_from(s).ok()),
            guard: self.guard,
            user_id: self.user_id,
            duration_us: unsigned(self.duration_us),
            has_exception: self.has_exception,
            has_slow: self.has_slow,
            host: self.host,
            entries: Vec::new(),
        })
    }

    fn into_summary(self) -> Result<BatchSummary> {
        let counts: BTreeMap<EntryType, usize> = serde_json::from_str(&self.counts)?;
        let mut summary = self.into_batch()?.summarize();
        summary.counts = counts;
        Ok(summary)
    }
}

impl EntryRow {
    fn from_entry(batch_id: &str, seq: usize, entry: &Entry) -> Result<Self> {
        Ok(Self {
            batch_id: keyed(batch_id),
            seq: seq as i64,
            entry_id: entry.id.clone(),
            entry_type: entry.entry_type.as_str().to_string(),
            span_id: entry.span_id.clone(),
            parent_span_id: entry.parent_span_id.clone(),
            family_hash: entry.family_hash.as_deref().map(hash),
            family_key: entry.family_hash.clone(),
            summary: entry.summary.clone(),
            occurred_at_us: micros(entry.occurred_at),
            duration_us: entry.duration_us.map(signed),
            tags: serde_json::to_string(&entry.tags)?,
            content: serde_json::to_string(&entry.content)?,
        })
    }

    fn into_entry(self) -> Result<Entry> {
        Ok(Entry {
            entry_type: parse_type(&self.entry_type)?,
            occurred_at: from_micros(self.occurred_at_us)?,
            tags: serde_json::from_str(&self.tags)?,
            content: serde_json::from_str(&self.content)?,
            id: self.entry_id,
            batch_id: self.batch_id,
            span_id: self.span_id,
            parent_span_id: self.parent_span_id,
            family_hash: self.family_key,
            summary: self.summary,
            duration_us: self.duration_us.map(unsigned),
        })
    }
}

impl AggregateRow {
    fn from_aggregate(a: &Aggregate) -> Self {
        Self {
            bucket_start: a.bucket_start.timestamp(),
            period_s: i64::from(a.period_s),
            entry_type: a.entry_type.as_str().to_string(),
            key_hash: hash(&a.key),
            key: a.key.clone(),
            count: signed(a.count),
            sum_us: signed(a.sum_us),
            max_us: signed(a.max_us),
        }
    }

    fn into_aggregate(self) -> Result<Aggregate> {
        Ok(Aggregate {
            bucket_start: from_micros(self.bucket_start.saturating_mul(1_000_000))?,
            period_s: u32::try_from(self.period_s).unwrap_or(u32::MAX),
            entry_type: parse_type(&self.entry_type)?,
            key: self.key,
            count: unsigned(self.count),
            sum_us: unsigned(self.sum_us),
            max_us: unsigned(self.max_us),
        })
    }
}

// ---------------------------------------------------------------------------
// Encoding helpers, shared with the cache store where they apply
// ---------------------------------------------------------------------------

/// A 16-hex-digit FNV-1a hash: short enough to index, stable across
/// platforms and releases (unlike `std`'s hasher).
pub(crate) fn hash(value: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in value.as_bytes() {
        h ^= u64::from(*byte);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// At most [`KEYED_LEN`] characters.
fn keyed(value: &str) -> String {
    match value.char_indices().nth(KEYED_LEN) {
        Some((at, _)) => value[..at].to_string(),
        None => value.to_string(),
    }
}

fn micros(at: DateTime<Utc>) -> i64 {
    at.timestamp_micros()
}

fn from_micros(us: i64) -> Result<DateTime<Utc>> {
    DateTime::from_timestamp_micros(us)
        .ok_or_else(|| Error::internal(format!("lookout: timestamp {us}µs is out of range")))
}

fn signed(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn unsigned(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

fn kind_name(kind: BatchKind) -> &'static str {
    match kind {
        BatchKind::Request => "request",
        BatchKind::Job => "job",
        BatchKind::Command => "command",
        BatchKind::ScheduledTask => "scheduled_task",
        BatchKind::Other => "other",
    }
}

fn parse_kind(name: &str) -> Result<BatchKind> {
    serde_json::from_value(serde_json::Value::String(name.to_string()))
        .map_err(|_| Error::internal(format!("lookout: unknown batch kind `{name}`")))
}

fn parse_type(name: &str) -> Result<EntryType> {
    EntryType::parse(name)
        .ok_or_else(|| Error::internal(format!("lookout: unknown entry type `{name}`")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_migrations_are_named_for_lookout() {
        assert_eq!(
            DatabaseStore::migrations().names(),
            vec![
                "rainier_lookout_0001_batches",
                "rainier_lookout_0002_entries",
                "rainier_lookout_0003_batch_tags",
                "rainier_lookout_0004_aggregates",
            ]
        );
    }

    #[test]
    fn the_schema_renders_on_every_dialect_with_signed_integers() {
        use rainier_orm::Dialect;
        for dialect in [Dialect::Sqlite, Dialect::MySql, Dialect::Postgres] {
            let ddl = [
                rainier_orm::schema::schema_ddl::<BatchRow>(dialect),
                rainier_orm::schema::schema_ddl::<EntryRow>(dialect),
                rainier_orm::schema::schema_ddl::<TagRow>(dialect),
                rainier_orm::schema::schema_ddl::<AggregateRow>(dialect),
            ]
            .concat()
            .join(";\n");
            assert!(!ddl.to_lowercase().contains("unsigned"), "{dialect:?}: {ddl}");
            assert!(ddl.contains("lookout_aggregates"), "{dialect:?}");
            // Four tables plus their secondary indexes.
            assert!(ddl.matches("CREATE INDEX").count() >= 6, "{dialect:?}: {ddl}");
        }
    }

    #[test]
    fn the_hash_is_short_and_stable() {
        assert_eq!(hash(""), "cbf29ce484222325");
        assert_eq!(hash("select ?").len(), 16);
        assert_ne!(hash("a"), hash("b"));
    }

    #[test]
    fn keyed_strings_are_cut_on_a_character_boundary() {
        let long = "é".repeat(300);
        assert_eq!(keyed(&long).chars().count(), KEYED_LEN);
        assert_eq!(keyed("short"), "short");
    }

    #[test]
    fn kinds_round_trip_through_their_names() {
        for kind in [
            BatchKind::Request,
            BatchKind::Job,
            BatchKind::Command,
            BatchKind::ScheduledTask,
            BatchKind::Other,
        ] {
            assert_eq!(parse_kind(kind_name(kind)).unwrap(), kind);
        }
    }

    #[cfg(all(feature = "sea-orm-executor", not(target_arch = "wasm32")))]
    mod sqlite {
        use super::super::*;
        use rainier_drivers::sql::SeaOrmExecutor;
        use rainier_orm::PoolConfig;

        async fn store() -> DatabaseStore {
            // A pool of one: an in-memory SQLite database lives as long as its
            // connection.
            let executor = SeaOrmExecutor::connect("sqlite::memory:", &PoolConfig::serverless())
                .await
                .expect("connect");
            let db = Database::new(executor);
            DatabaseStore::migrations().run(&db).await.expect("migrate");
            DatabaseStore::new(db)
        }

        #[tokio::test]
        async fn conforms_on_sqlite() {
            crate::store::conformance::run(&store().await).await;
        }

        #[tokio::test]
        async fn conforms_under_a_d1_sized_parameter_budget() {
            // 100 parameters per statement: every insert and delete is chunked.
            let store = store().await.with_max_bind_params(100);
            crate::store::conformance::run(&store).await;
        }

        #[tokio::test]
        async fn migrating_twice_is_a_no_op() {
            let executor = SeaOrmExecutor::connect("sqlite::memory:", &PoolConfig::serverless())
                .await
                .expect("connect");
            let db = Database::new(executor);
            assert_eq!(DatabaseStore::migrations().run(&db).await.unwrap().len(), 4);
            assert!(DatabaseStore::migrations().run(&db).await.unwrap().is_empty());
        }

        #[tokio::test]
        async fn long_keyed_values_are_truncated_rather_than_refused() {
            use crate::store::conformance::{at, batch};
            let store = store().await;
            let mut b = batch(&"x".repeat(40), BatchKind::Request, at(0, 0));
            b.route_name = Some("r".repeat(400));
            b.tags = vec!["t".repeat(400)];
            store.store(vec![b]).await.unwrap();
            let listed = store
                .batches(BatchQuery { route_name: Some("r".repeat(400)), ..Default::default() })
                .await
                .unwrap();
            assert_eq!(listed.len(), 1);
            assert_eq!(listed[0].route_name.as_ref().map(String::len), Some(KEYED_LEN));
        }
    }
}
