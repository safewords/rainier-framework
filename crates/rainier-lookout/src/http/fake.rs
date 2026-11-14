//! A tiny in-memory [`Store`] for the dashboard's and the commands' tests —
//! deliberately not the real `MemoryStore`, so these tests exercise the HTTP
//! layer and nothing else.

use std::sync::Mutex;

use chrono::{DateTime, TimeZone, Utc};
use rainier_support::{BoxFuture, Result};

use crate::aggregate::{merge, rollup};
use crate::entry::{Aggregate, Batch, BatchKind, BatchSummary, Entry, EntryType};
use crate::store::{AggregateOrder, AggregateQuery, BatchQuery, Store};

#[derive(Default)]
pub(crate) struct FakeStore {
    pub(crate) batches: Mutex<Vec<Batch>>,
    pub(crate) aggregates: Mutex<Vec<Aggregate>>,
    pub(crate) last_query: Mutex<Option<BatchQuery>>,
    pub(crate) pruned_with: Mutex<Option<(DateTime<Utc>, DateTime<Utc>)>>,
}

impl FakeStore {
    pub(crate) fn with(batches: Vec<Batch>) -> Self {
        let store = Self::default();
        *store.aggregates.lock().unwrap() = rollup(&batches);
        *store.batches.lock().unwrap() = batches;
        store
    }
}

impl Store for FakeStore {
    fn name(&self) -> &'static str {
        "fake"
    }

    fn store(&self, batches: Vec<Batch>) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            self.aggregates.lock().unwrap().extend(rollup(&batches));
            self.batches.lock().unwrap().extend(batches);
            Ok(())
        })
    }

    fn batches(&self, query: BatchQuery) -> BoxFuture<'_, Result<Vec<BatchSummary>>> {
        Box::pin(async move {
            *self.last_query.lock().unwrap() = Some(query.clone());
            let mut out: Vec<BatchSummary> = self
                .batches
                .lock()
                .unwrap()
                .iter()
                .filter(|b| query.kind.is_none_or(|k| b.kind == k))
                .filter(|b| {
                    query.entry_type.is_none_or(|t| b.entries.iter().any(|e| e.entry_type == t))
                })
                .filter(|b| {
                    query.route_name.as_ref().is_none_or(|r| b.route_name.as_ref() == Some(r))
                })
                .filter(|b| query.user_id.as_ref().is_none_or(|u| b.user_id.as_ref() == Some(u)))
                .filter(|b| query.tag.as_ref().is_none_or(|t| b.tags.contains(t)))
                .filter(|b| query.trace_id.as_ref().is_none_or(|t| &b.trace_id == t))
                .filter(|b| query.has_exception.is_none_or(|x| b.has_exception == x))
                .filter(|b| query.has_slow.is_none_or(|x| b.has_slow == x))
                .filter(|b| query.before.is_none_or(|at| b.started_at < at))
                .map(Batch::summarize)
                .collect();
            out.sort_by_key(|b| std::cmp::Reverse(b.started_at));
            out.truncate(query.limit.unwrap_or(50).min(200));
            Ok(out)
        })
    }

    fn batch(&self, id: &str) -> BoxFuture<'_, Result<Option<Batch>>> {
        let id = id.to_string();
        Box::pin(
            async move { Ok(self.batches.lock().unwrap().iter().find(|b| b.id == id).cloned()) },
        )
    }

    fn entries(
        &self,
        entry_type: EntryType,
        before: Option<DateTime<Utc>>,
        limit: usize,
    ) -> BoxFuture<'_, Result<Vec<Entry>>> {
        Box::pin(async move {
            let mut out: Vec<Entry> = self
                .batches
                .lock()
                .unwrap()
                .iter()
                .flat_map(|b| b.entries.iter())
                .filter(|e| e.entry_type == entry_type)
                .filter(|e| before.is_none_or(|at| e.occurred_at < at))
                .cloned()
                .collect();
            out.sort_by_key(|e| std::cmp::Reverse(e.occurred_at));
            out.truncate(limit);
            Ok(out)
        })
    }

    fn aggregates(&self, query: AggregateQuery) -> BoxFuture<'_, Result<Vec<Aggregate>>> {
        Box::pin(async move {
            let mut per_key: Vec<Aggregate> = Vec::new();
            for row in self.aggregates.lock().unwrap().iter() {
                if row.entry_type != query.entry_type
                    || row.period_s != query.period_s
                    || row.bucket_start
                        < crate::aggregate::bucket_start(query.since, query.period_s)
                {
                    continue;
                }
                match per_key.iter_mut().find(|a| a.key == row.key) {
                    Some(existing) => merge(existing, row),
                    None => per_key.push(row.clone()),
                }
            }
            let avg = |a: &Aggregate| a.sum_us / a.count.max(1);
            per_key.sort_by(|a, b| match query.order {
                AggregateOrder::Count => b.count.cmp(&a.count),
                AggregateOrder::Average => avg(b).cmp(&avg(a)),
                AggregateOrder::Max => b.max_us.cmp(&a.max_us),
            });
            per_key.truncate(query.limit);
            Ok(per_key)
        })
    }

    fn prune(
        &self,
        batches_before: DateTime<Utc>,
        aggregates_before: DateTime<Utc>,
    ) -> BoxFuture<'_, Result<u64>> {
        Box::pin(async move {
            *self.pruned_with.lock().unwrap() = Some((batches_before, aggregates_before));
            let mut batches = self.batches.lock().unwrap();
            let before = batches.len();
            batches.retain(|b| b.started_at >= batches_before);
            self.aggregates.lock().unwrap().retain(|a| a.bucket_start >= aggregates_before);
            Ok((before - batches.len()) as u64)
        })
    }

    fn clear(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            self.batches.lock().unwrap().clear();
            self.aggregates.lock().unwrap().clear();
            Ok(())
        })
    }
}

/// An entry at `offset_ms` into a batch starting at `start`.
pub(crate) fn entry(
    batch_id: &str,
    id: &str,
    entry_type: EntryType,
    summary: &str,
    start: DateTime<Utc>,
    offset_ms: i64,
    duration_us: Option<u64>,
) -> Entry {
    Entry {
        id: id.to_string(),
        batch_id: batch_id.to_string(),
        entry_type,
        span_id: Some(format!("span-{id}")),
        parent_span_id: None,
        family_hash: Some(summary.to_string()),
        summary: summary.to_string(),
        occurred_at: start + chrono::Duration::milliseconds(offset_ms),
        duration_us,
        tags: Vec::new(),
        content: serde_json::json!({}),
    }
}

/// A request batch started `ago_s` seconds before now.
pub(crate) fn request(
    id: &str,
    route: &str,
    status: u16,
    ago_s: i64,
    entries: Vec<Entry>,
) -> Batch {
    let started_at = Utc::now() - chrono::Duration::seconds(ago_s);
    let entries = entries
        .into_iter()
        .map(|mut e| {
            let offset = e.occurred_at - Utc.timestamp_opt(0, 0).unwrap();
            e.occurred_at = started_at + offset;
            e.batch_id = id.to_string();
            e
        })
        .collect::<Vec<_>>();
    let has_exception = entries.iter().any(|e| e.entry_type == EntryType::Exception);
    Batch {
        id: id.to_string(),
        trace_id: format!("trace-{id}"),
        kind: BatchKind::Request,
        summary: format!("GET /{route} {status}"),
        route_name: Some(route.to_string()),
        route_pattern: Some(format!("/{route}")),
        method: Some("GET".to_string()),
        status: Some(status),
        guard: None,
        user_id: None,
        duration_us: 20_000,
        has_exception,
        has_slow: false,
        host: None,
        started_at,
        tags: Vec::new(),
        entries,
    }
}

/// The epoch, for building entries whose offsets [`request`] rebases.
pub(crate) fn epoch() -> DateTime<Utc> {
    Utc.timestamp_opt(0, 0).unwrap()
}
