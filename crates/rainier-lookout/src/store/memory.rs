//! An in-process [`Store`]: a bounded ring of batches, for development and tests.
//!
//! The newest `capacity` batches are kept; storing one more evicts the oldest
//! stored. Aggregates are kept beside them and outlive the batches they came
//! from, as in every store — they are what makes "slowest routes this week"
//! answerable after the raw batches are gone — until [`prune`](Store::prune)
//! or [`clear`](Store::clear).
//!
//! Per process: on Kubernetes each pod has its own. The semantics are the
//! [reference ones](super::conformance), and the conformance suite runs
//! against it.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Mutex, MutexGuard};

use chrono::{DateTime, Utc};
use rainier_support::{BoxFuture, Result};

use super::conformance::{effective_limit, matches, newest_first};
use super::{BatchQuery, Store};
use crate::aggregate::{bucket_start, merge, rollup};
use crate::cases::{observe, occurrences, select_cases, Case, CaseQuery};
use crate::entry::{Aggregate, Batch, BatchSummary, Entry, EntryType};

/// (bucket start, period, type, key).
type AggregateKey = (DateTime<Utc>, u32, EntryType, String);

/// The in-memory store.
#[derive(Debug)]
pub struct MemoryStore {
    capacity: usize,
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    /// Oldest stored first.
    batches: VecDeque<Batch>,
    aggregates: BTreeMap<AggregateKey, Aggregate>,
    cases: BTreeMap<String, Case>,
}

impl MemoryStore {
    /// A store keeping at most `capacity` batches (at least one).
    pub fn new(capacity: usize) -> Self {
        Self { capacity: capacity.max(1), state: Mutex::new(State::default()) }
    }

    /// How many batches it keeps.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// How many batches it holds now.
    pub fn len(&self) -> usize {
        self.lock().batches.len()
    }

    /// Whether it holds none.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Every batch held, oldest stored first — for tests.
    pub fn all(&self) -> Vec<Batch> {
        self.lock().batches.iter().cloned().collect()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Store for MemoryStore {
    fn name(&self) -> &'static str {
        "memory"
    }

    fn store(&self, batches: Vec<Batch>) -> BoxFuture<'_, Result<()>> {
        self.store_counting(batches, Vec::new())
    }

    fn store_counting(
        &self,
        batches: Vec<Batch>,
        counted: Vec<Aggregate>,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let mut rolled = rollup(&batches);
            rolled.extend(counted);
            let seen = occurrences(&batches);
            let mut state = self.lock();
            for occurrence in &seen {
                let existing = state.cases.remove(&occurrence.id);
                state.cases.insert(occurrence.id.clone(), observe(existing, occurrence));
            }
            for row in rolled {
                let key = (row.bucket_start, row.period_s, row.entry_type, row.key.clone());
                match state.aggregates.get_mut(&key) {
                    Some(existing) => merge(existing, &row),
                    None => {
                        state.aggregates.insert(key, row);
                    }
                }
            }
            for batch in batches {
                if let Some(slot) = state.batches.iter_mut().find(|b| b.id == batch.id) {
                    *slot = batch;
                    continue;
                }
                state.batches.push_back(batch);
                while state.batches.len() > self.capacity {
                    state.batches.pop_front();
                }
            }
            Ok(())
        })
    }

    fn batches(&self, query: BatchQuery) -> BoxFuture<'_, Result<Vec<BatchSummary>>> {
        Box::pin(async move {
            let state = self.lock();
            let mut found: Vec<BatchSummary> = state
                .batches
                .iter()
                .filter(|b| query.before.is_none_or(|before| b.started_at < before))
                .map(Batch::summarize)
                .filter(|s| matches(&query, s))
                .collect();
            found.sort_by(newest_first);
            found.truncate(effective_limit(query.limit));
            Ok(found)
        })
    }

    fn batch(&self, id: &str) -> BoxFuture<'_, Result<Option<Batch>>> {
        let id = id.to_string();
        Box::pin(async move { Ok(self.lock().batches.iter().find(|b| b.id == id).cloned()) })
    }

    fn entries(
        &self,
        entry_type: EntryType,
        before: Option<DateTime<Utc>>,
        limit: usize,
    ) -> BoxFuture<'_, Result<Vec<Entry>>> {
        Box::pin(async move {
            let state = self.lock();
            let mut found: Vec<Entry> = state
                .batches
                .iter()
                .flat_map(|b| b.entries.iter())
                .filter(|e| e.entry_type == entry_type)
                .filter(|e| before.is_none_or(|before| e.occurred_at < before))
                .cloned()
                .collect();
            found.sort_by_key(|e| std::cmp::Reverse(e.occurred_at));
            found.truncate(limit);
            Ok(found)
        })
    }

    fn family_entries(
        &self,
        entry_type: EntryType,
        key: &str,
        before: Option<DateTime<Utc>>,
        limit: usize,
    ) -> BoxFuture<'_, Result<Vec<Entry>>> {
        let key = key.to_string();
        Box::pin(async move {
            let state = self.lock();
            let mut found: Vec<Entry> = state
                .batches
                .iter()
                .flat_map(|b| b.entries.iter())
                .filter(|e| e.entry_type == entry_type && e.family_key() == key)
                .filter(|e| before.is_none_or(|before| e.occurred_at < before))
                .cloned()
                .collect();
            found.sort_by_key(|e| std::cmp::Reverse(e.occurred_at));
            found.truncate(limit);
            Ok(found)
        })
    }

    fn aggregate_rows(
        &self,
        entry_type: EntryType,
        period_s: u32,
        since: DateTime<Utc>,
    ) -> BoxFuture<'_, Result<Vec<Aggregate>>> {
        Box::pin(async move {
            let from = bucket_start(since, period_s);
            Ok(self
                .lock()
                .aggregates
                .values()
                .filter(|a| a.entry_type == entry_type && a.period_s == period_s)
                .filter(|a| a.bucket_start >= from)
                .cloned()
                .collect())
        })
    }

    fn cases(&self, query: CaseQuery) -> BoxFuture<'_, Result<Vec<Case>>> {
        Box::pin(async move {
            let all: Vec<Case> = self.lock().cases.values().cloned().collect();
            Ok(select_cases(all, &query))
        })
    }

    fn case(&self, id: &str) -> BoxFuture<'_, Result<Option<Case>>> {
        let id = id.to_string();
        Box::pin(async move { Ok(self.lock().cases.get(&id).cloned()) })
    }

    fn save_case(&self, case: Case) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            self.lock().cases.insert(case.id.clone(), case);
            Ok(())
        })
    }

    fn prune(
        &self,
        batches_before: DateTime<Utc>,
        aggregates_before: DateTime<Utc>,
    ) -> BoxFuture<'_, Result<u64>> {
        Box::pin(async move {
            let mut state = self.lock();
            let before = state.batches.len();
            state.batches.retain(|b| b.started_at >= batches_before);
            let removed = before - state.batches.len();
            state.aggregates.retain(|_, a| a.bucket_start >= aggregates_before);
            state.cases.retain(|_, c| c.last_seen >= aggregates_before);
            Ok(removed as u64)
        })
    }

    fn clear(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let mut state = self.lock();
            state.batches.clear();
            state.aggregates.clear();
            state.cases.clear();
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::conformance;

    #[tokio::test]
    async fn conforms() {
        conformance::run(&MemoryStore::new(1_000)).await;
    }

    #[tokio::test]
    async fn the_ring_evicts_the_oldest_stored() {
        let store = MemoryStore::new(2);
        for (i, id) in ["a", "b", "c"].into_iter().enumerate() {
            let batch =
                conformance::batch(id, crate::BatchKind::Other, conformance::at(i as i64, 0));
            store.store(vec![batch]).await.expect("store");
        }
        let ids: Vec<String> = store.all().into_iter().map(|b| b.id).collect();
        assert_eq!(ids, vec!["b", "c"]);
        assert_eq!(store.len(), 2);
    }
}
