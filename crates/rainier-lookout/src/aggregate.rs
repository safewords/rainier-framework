//! Rolling entries up into [`Aggregate`] buckets.
//!
//! One implementation, used by every store, so "slowest routes" means the
//! same thing whichever store answers it.

use std::collections::BTreeMap;

use chrono::{DateTime, TimeZone, Utc};

use crate::entry::{Aggregate, Batch, EntryType};

/// (bucket start, period, type, key).
type BucketKey = (DateTime<Utc>, u32, EntryType, String);
/// (count, sum, max).
type Totals = (u64, u64, u64);

/// The bucket lengths every entry is rolled into, in seconds.
pub const PERIODS: &[u32] = &[60, 600, 3600];

/// The entry types worth rolling up, and only when they have a duration.
pub const AGGREGATED: &[EntryType] = &[
    EntryType::Request,
    EntryType::Query,
    EntryType::HttpClient,
    EntryType::Job,
    EntryType::Cache,
    EntryType::Command,
    EntryType::ScheduledTask,
    EntryType::Mail,
    EntryType::Notification,
    EntryType::Exception,
];

/// The start of the `period_s`-second bucket holding `at`.
pub fn bucket_start(at: DateTime<Utc>, period_s: u32) -> DateTime<Utc> {
    let period = i64::from(period_s.max(1));
    let secs = at.timestamp().div_euclid(period) * period;
    Utc.timestamp_opt(secs, 0).single().unwrap_or(at)
}

/// Roll `batches` into aggregates, one per (bucket, period, type, key).
///
/// The key is the entry's family key ([`Entry::family_hash`](crate::entry::Entry::family_hash)),
/// falling back to its summary. Exceptions are counted even without a
/// duration.
pub fn rollup(batches: &[Batch]) -> Vec<Aggregate> {
    let mut acc: BTreeMap<BucketKey, Totals> = BTreeMap::new();
    for batch in batches {
        for entry in &batch.entries {
            if !AGGREGATED.contains(&entry.entry_type) {
                continue;
            }
            if entry.duration_us.is_none() && entry.entry_type != EntryType::Exception {
                continue;
            }
            let duration = entry.duration_us.unwrap_or(0);
            let key = entry.family_hash.clone().unwrap_or_else(|| entry.summary.clone());
            for &period in PERIODS {
                let slot = acc
                    .entry((
                        bucket_start(entry.occurred_at, period),
                        period,
                        entry.entry_type,
                        key.clone(),
                    ))
                    .or_insert((0, 0, 0));
                slot.0 += 1;
                slot.1 = slot.1.saturating_add(duration);
                slot.2 = slot.2.max(duration);
            }
        }
    }
    acc.into_iter()
        .map(|((bucket_start, period_s, entry_type, key), (count, sum_us, max_us))| Aggregate {
            bucket_start,
            period_s,
            entry_type,
            key,
            count,
            sum_us,
            max_us,
        })
        .collect()
}

/// Merge `incoming` into `existing`, as a store does when a bucket already
/// has a row: counts and sums add, maxima take the larger.
pub fn merge(existing: &mut Aggregate, incoming: &Aggregate) {
    existing.count += incoming.count;
    existing.sum_us = existing.sum_us.saturating_add(incoming.sum_us);
    existing.max_us = existing.max_us.max(incoming.max_us);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::{BatchKind, Entry};

    fn entry(t: EntryType, key: &str, at: i64, us: Option<u64>) -> Entry {
        Entry {
            id: format!("{key}-{at}"),
            batch_id: "b".into(),
            entry_type: t,
            span_id: None,
            parent_span_id: None,
            family_hash: Some(key.into()),
            summary: key.into(),
            occurred_at: Utc.timestamp_opt(at, 0).unwrap(),
            duration_us: us,
            tags: vec![],
            content: serde_json::Value::Null,
        }
    }

    fn batch(entries: Vec<Entry>) -> Batch {
        Batch {
            id: "b".into(),
            trace_id: "t".into(),
            kind: BatchKind::Request,
            summary: String::new(),
            route_name: None,
            route_pattern: None,
            method: None,
            status: None,
            guard: None,
            user_id: None,
            duration_us: 0,
            has_exception: false,
            has_slow: false,
            host: None,
            started_at: Utc.timestamp_opt(0, 0).unwrap(),
            tags: vec![],
            entries,
        }
    }

    #[test]
    fn buckets_align_to_their_period() {
        let at = Utc.timestamp_opt(3_725, 0).unwrap();
        assert_eq!(bucket_start(at, 60).timestamp(), 3_720);
        assert_eq!(bucket_start(at, 3600).timestamp(), 3_600);
    }

    #[test]
    fn rolls_up_count_sum_and_max_per_key_and_period() {
        let b = batch(vec![
            entry(EntryType::Query, "select ?", 10, Some(100)),
            entry(EntryType::Query, "select ?", 20, Some(300)),
            entry(EntryType::Query, "update ?", 30, Some(50)),
            entry(EntryType::Exception, "boom", 40, None),
            entry(EntryType::Log, "ignored", 40, Some(1)),
            entry(EntryType::Cache, "no duration", 40, None),
        ]);
        let rows = rollup(&[b]);
        let select: Vec<_> = rows.iter().filter(|a| a.key == "select ?").collect();
        assert_eq!(select.len(), PERIODS.len());
        assert!(select.iter().all(|a| a.count == 2 && a.sum_us == 400 && a.max_us == 300));
        assert!(rows
            .iter()
            .any(|a| a.key == "boom" && a.entry_type == EntryType::Exception && a.count == 1));
        assert!(!rows.iter().any(|a| a.key == "ignored" || a.key == "no duration"));
    }

    #[test]
    fn merging_adds_and_keeps_the_worst() {
        let rows = rollup(&[batch(vec![entry(EntryType::Job, "j", 0, Some(10))])]);
        let mut a = rows[0].clone();
        let mut b = rows[0].clone();
        b.max_us = 99;
        merge(&mut a, &b);
        assert_eq!((a.count, a.sum_us, a.max_us), (2, 20, 99));
    }
}
