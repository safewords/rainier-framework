//! Rolling entries up into [`Aggregate`] buckets.
//!
//! One implementation, used by every store, so "slowest routes" means the
//! same thing whichever store answers it.

use std::collections::BTreeMap;

use chrono::{DateTime, TimeZone, Utc};

use crate::entry::{Aggregate, Batch, Entry, EntryType};

/// (bucket start, period, type, key).
type BucketKey = (DateTime<Utc>, u32, EntryType, String);
/// (count, sum, max, histogram).
type Totals = (u64, u64, u64, Histogram);

/// How many durations fell in each slot: slot → count, only the slots that
/// have any.
///
/// Slot 0 holds zero; slot `s ≥ 1` holds durations in
/// `[2^((s-1)/4), 2^(s/4))` microseconds, four slots per doubling. A
/// percentile read off it is within about 9% of the true one, two
/// histograms merge by adding, and a bucket's histogram is a handful of
/// slots, since one key's durations rarely spread over more.
pub type Histogram = BTreeMap<u16, u64>;

/// Four slots per doubling of the duration.
const SLOTS_PER_DOUBLING: f64 = 4.0;

/// The [`Histogram`] slot holding `us` microseconds.
pub fn slot(us: u64) -> u16 {
    if us == 0 {
        return 0;
    }
    // `log2` is exact on powers of two, so each slot starts exactly on its
    // lower bound.
    1 + ((us as f64).log2() * SLOTS_PER_DOUBLING).floor() as u16
}

/// The lower bound of a slot, in microseconds.
fn lower_bound(slot: u16) -> f64 {
    if slot == 0 {
        return 0.0;
    }
    (f64::from(slot - 1) / SLOTS_PER_DOUBLING).exp2()
}

/// The `p`th percentile (0–100) of the durations in `histogram`, in
/// microseconds, never above `max_us` (the slowest one, which is known
/// exactly). `None` when the histogram is empty.
///
/// The nearest rank — the smallest duration with at least p% at or below
/// it — placed inside its slot by how far into the slot's count the rank
/// falls, geometrically: a slot's only duration reads as its upper bound
/// (then clamped to the maximum, so a lone run reads exactly), and many
/// spread across it.
pub fn percentile(histogram: &Histogram, p: f64, max_us: u64) -> Option<u64> {
    let total: u64 = histogram.values().sum();
    if total == 0 {
        return None;
    }
    let rank = ((p.clamp(0.0, 100.0) / 100.0) * total as f64).ceil().max(1.0) as u64;
    let mut seen = 0;
    for (&slot, &count) in histogram {
        if seen + count >= rank {
            if slot == 0 {
                return Some(0);
            }
            let into = (rank - seen) as f64 / count as f64;
            let lower = lower_bound(slot);
            let value = lower * (into / SLOTS_PER_DOUBLING).exp2();
            return Some((value.round() as u64).min(max_us));
        }
        seen += count;
    }
    Some(max_us)
}

/// Add `from` into `into`.
pub fn add_histogram(into: &mut Histogram, from: &Histogram) {
    for (&slot, &count) in from {
        *into.entry(slot).or_insert(0) += count;
    }
}

/// The bucket lengths every entry is rolled into, in seconds.
pub const PERIODS: &[u32] = &[60, 600, 3600];

/// The entry types worth rolling up — timed, or (exceptions, model changes)
/// counted.
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
    EntryType::Model,
];

/// The types aggregated by count alone: an exception and a model change have
/// no duration, and are counted rather than skipped.
fn counted_untimed(entry_type: EntryType) -> bool {
    matches!(entry_type, EntryType::Exception | EntryType::Model)
}

/// The start of the `period_s`-second bucket holding `at`.
pub fn bucket_start(at: DateTime<Utc>, period_s: u32) -> DateTime<Utc> {
    let period = i64::from(period_s.max(1));
    let secs = at.timestamp().div_euclid(period) * period;
    Utc.timestamp_opt(secs, 0).single().unwrap_or(at)
}

/// Roll `batches` into aggregates, one per (bucket, period, type, key).
///
/// The key is the entry's family key ([`Entry::family_hash`](crate::entry::Entry::family_hash)),
/// falling back to its summary. Exceptions and model changes are counted
/// without a duration.
pub fn rollup(batches: &[Batch]) -> Vec<Aggregate> {
    rollup_entries(batches.iter().flat_map(|b| &b.entries))
}

/// Roll entries into aggregates, as [`rollup`] does for whole batches — for
/// the entries of a batch that was counted but not kept.
pub fn rollup_entries<'a>(entries: impl IntoIterator<Item = &'a Entry>) -> Vec<Aggregate> {
    let mut acc: BTreeMap<BucketKey, Totals> = BTreeMap::new();
    {
        for entry in entries {
            if !AGGREGATED.contains(&entry.entry_type) {
                continue;
            }
            if entry.duration_us.is_none() && !counted_untimed(entry.entry_type) {
                continue;
            }
            let duration = entry.duration_us.unwrap_or(0);
            let key = entry.family_key().to_string();
            for &period in PERIODS {
                let slot = acc
                    .entry((
                        bucket_start(entry.occurred_at, period),
                        period,
                        entry.entry_type,
                        key.clone(),
                    ))
                    .or_insert_with(|| (0, 0, 0, Histogram::new()));
                slot.0 += 1;
                slot.1 = slot.1.saturating_add(duration);
                slot.2 = slot.2.max(duration);
                // An exception without a duration is counted, not timed.
                if let Some(us) = entry.duration_us {
                    *slot.3.entry(self::slot(us)).or_insert(0) += 1;
                }
            }
        }
    }
    acc.into_iter()
        .map(|((bucket_start, period_s, entry_type, key), (count, sum_us, max_us, histogram))| {
            Aggregate { bucket_start, period_s, entry_type, key, count, sum_us, max_us, histogram }
        })
        .collect()
}

/// `rows` with each (bucket, period, type, key) appearing once, merged.
///
/// What a store writes when aggregates arrive from two sources at once — the
/// batches it keeps and the batches only counted — so no statement names a
/// row twice.
pub fn combine(rows: Vec<Aggregate>) -> Vec<Aggregate> {
    let mut out: BTreeMap<BucketKey, Aggregate> = BTreeMap::new();
    for row in rows {
        let key = (row.bucket_start, row.period_s, row.entry_type, row.key.clone());
        match out.get_mut(&key) {
            Some(existing) => merge(existing, &row),
            None => {
                out.insert(key, row);
            }
        }
    }
    out.into_values().collect()
}

/// Merge `incoming` into `existing`, as a store does when a bucket already
/// has a row: counts, sums and histograms add, maxima take the larger.
pub fn merge(existing: &mut Aggregate, incoming: &Aggregate) {
    existing.count += incoming.count;
    existing.sum_us = existing.sum_us.saturating_add(incoming.sum_us);
    existing.max_us = existing.max_us.max(incoming.max_us);
    add_histogram(&mut existing.histogram, &incoming.histogram);
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
        assert!(select.iter().all(|a| a.histogram.values().sum::<u64>() == 2));
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
        assert_eq!(a.histogram, Histogram::from([(slot(10), 2)]));
    }

    #[test]
    fn an_exception_without_a_duration_is_counted_but_not_timed() {
        let rows = rollup(&[batch(vec![entry(EntryType::Exception, "boom", 0, None)])]);
        assert!(rows.iter().all(|a| a.count == 1 && a.histogram.is_empty()));
    }

    #[test]
    fn slots_are_quarter_doublings_starting_on_their_bound() {
        assert_eq!(slot(0), 0);
        assert_eq!(slot(1), 1);
        assert_eq!(slot(2), 5);
        assert_eq!(slot(1024), 41);
        assert_eq!(slot(1023), 40);
        assert!(slot(u64::MAX) > slot(1 << 62));
    }

    #[test]
    fn percentiles_land_within_a_slot_of_the_truth() {
        // 1 ms … 100 ms, one each.
        let mut h = Histogram::new();
        for ms in 1..=100u64 {
            *h.entry(slot(ms * 1000)).or_insert(0) += 1;
        }
        for (p, truth) in [(50.0, 50_000.0), (90.0, 90_000.0), (95.0, 95_000.0), (99.0, 99_000.0)] {
            let got = percentile(&h, p, 100_000).unwrap() as f64;
            assert!((got / truth - 1.0).abs() < 0.1, "p{p}: {got} vs {truth}");
        }
        assert_eq!(percentile(&h, 100.0, 100_000), Some(100_000), "never above the maximum");
        assert_eq!(percentile(&Histogram::new(), 50.0, 0), None);
        // A lone duration reads as itself.
        let one = Histogram::from([(slot(38_793), 1)]);
        assert_eq!(percentile(&one, 50.0, 38_793), Some(38_793));
    }
}
