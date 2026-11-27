//! Queues: how deep each one is, how many of its jobs are running against how
//! many slots, how fast it drains, and how long its jobs wait and run.
//!
//! Everything here is a [`QueueFacet`](EntryType::QueueFacet) aggregate row,
//! so every store keeps it without a table of its own and prunes it with the
//! rest. Two things write them:
//!
//! - **Every job run and dispatch**, counted or kept, rolled up beside its own
//!   row by [`rollup_entries`](crate::aggregate::rollup_entries): the job's
//!   `queue.process` activity carries its queue, its outcome and its wait, and
//!   `queue.publish` its queue.
//! - **The workers' samples.** A worker with the application's bus publishes
//!   a `queue.sample` activity per queue on its sampling interval (fifteen
//!   seconds by default): the queue's depth, how many of its jobs that worker
//!   is running, and how many it may. The subscriber rolls each sample
//!   straight into the counts the writer flushes — a sample belongs to no
//!   unit of work, and a `queue:work` command's batch, which lasts as long as
//!   the worker, is no place to hold one.
//!
//! # Keys
//!
//! | Key | One per | Its "duration" |
//! |---|---|---|
//! | `run {outcome} {queue}` | attempt (`processed`, `released`, `failed`, `timed_out`, `error`) | the attempt's run, µs |
//! | `wait {queue}` | attempt whose wait is known | the wait, µs |
//! | `pushed {queue}` | dispatch that succeeded | the push, µs |
//! | `depth {queue}` | sample | the depth, **in jobs** |
//! | `busy {queue}` | sample | running jobs × the interval, µs |
//! | `capacity {queue}` | sample | slots × the interval, µs |
//! | `workers {queue}` | sample | the interval, µs |
//!
//! The last three are **time**, so they add across replicas: a minute's
//! `busy` summed over every worker is the slot-time the queue's jobs held
//! that minute, and divided by the minute it is the average number running —
//! the queue's concurrency — however many replicas there were and however
//! they came and went. `capacity` is the same for slots, and `workers` for
//! the replicas themselves. `depth` is the one gauge: every replica sees the
//! same backlog, so it is averaged, never added.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use crate::aggregate::{self, add_histogram, percentile, Histogram, PERIODS};
use crate::entry::{Aggregate, Entry, EntryType};
use crate::facets::round;

/// The outcomes a run can have, as the queue labels them.
pub const OUTCOMES: &[&str] = &["processed", "released", "failed", "timed_out", "error"];

/// The longest queue name kept; a longer one is cut.
const MAX_QUEUE: usize = 200;

/// A failure rate at or above this is `failing`.
const FAILING_RATE: f64 = 0.05;
/// A failure rate at or above this is `degraded`.
const DEGRADED_RATE: f64 = 0.01;
/// A p95 wait at or above this, in microseconds, is `degraded`: a minute.
const DEGRADED_WAIT_US: u64 = 60_000_000;
/// Slots this busy, with jobs waiting, is `degraded`.
const DEGRADED_UTILISATION: f64 = 0.9;

fn queue_name(raw: Option<&str>) -> String {
    let name = raw.map(str::trim).filter(|q| !q.is_empty()).unwrap_or("default");
    name.chars().take(MAX_QUEUE).collect()
}

/// What a job entry adds to its queue's facet: `(key, value)` pairs, the
/// value in the key's unit (see the module docs). Empty for anything else.
pub fn facets_of(entry: &Entry) -> Vec<(String, u64)> {
    if entry.entry_type != EntryType::Job {
        return Vec::new();
    }
    let text = |key: &str| entry.content.get(key).and_then(Value::as_str);
    let number = |key: &str| {
        entry.content.get(key).and_then(|v| v.as_u64().or_else(|| v.as_str()?.trim().parse().ok()))
    };
    let queue = queue_name(text("messaging.destination.name"));
    let duration = entry.duration_us.unwrap_or(0);

    if entry.family_key().starts_with("dispatch:") {
        // A push that failed queued nothing.
        if text("error.type").is_some() {
            return Vec::new();
        }
        return vec![(format!("pushed {queue}"), duration)];
    }

    let outcome = text("rainier.job.outcome")
        .and_then(|o| OUTCOMES.iter().copied().find(|known| *known == o))
        .unwrap_or(if entry.tags.iter().any(|t| t == "failed") { "failed" } else { "processed" });
    let mut out = vec![(format!("run {outcome} {queue}"), duration)];
    if let Some(wait_ms) = number("messaging.queue.wait_ms") {
        out.push((format!("wait {queue}"), wait_ms.saturating_mul(1000)));
    }
    out
}

/// One worker's sample of one queue — a `queue.sample` activity's fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sample {
    /// The queue.
    pub queue: String,
    /// How many jobs were waiting, when the driver could say.
    pub depth: Option<u64>,
    /// How many of its jobs the worker was running.
    pub busy: u64,
    /// How many it may run at once.
    pub capacity: u64,
    /// How long the sample stands for, in microseconds.
    pub interval_us: u64,
}

/// The aggregate rows `sample`, taken at `at`, adds: one per key and period.
pub fn sample_rows(sample: &Sample, at: DateTime<Utc>) -> Vec<Aggregate> {
    if sample.interval_us == 0 {
        return Vec::new();
    }
    let queue = queue_name(Some(&sample.queue));
    let mut values = vec![
        (format!("busy {queue}"), sample.busy.saturating_mul(sample.interval_us)),
        (format!("capacity {queue}"), sample.capacity.saturating_mul(sample.interval_us)),
        (format!("workers {queue}"), sample.interval_us),
    ];
    if let Some(depth) = sample.depth {
        values.push((format!("depth {queue}"), depth));
    }
    let mut rows = Vec::new();
    for (key, value) in values {
        for &period_s in PERIODS {
            rows.push(Aggregate {
                bucket_start: aggregate::bucket_start(at, period_s),
                period_s,
                entry_type: EntryType::QueueFacet,
                key: key.clone(),
                count: 1,
                sum_us: value,
                max_us: value,
                histogram: Histogram::from([(aggregate::slot(value), 1)]),
            });
        }
    }
    rows
}

/// A key's parts: what it measures, the outcome for a run, and the queue.
fn parse(key: &str) -> Option<(&str, Option<&str>, &str)> {
    let (kind, rest) = key.split_once(' ')?;
    match kind {
        "run" => {
            let (outcome, queue) = rest.split_once(' ')?;
            Some((kind, Some(outcome), queue))
        }
        "wait" | "pushed" | "depth" | "busy" | "capacity" | "workers" => Some((kind, None, rest)),
        _ => None,
    }
}

/// Counts, sum, max and spread of one measure.
#[derive(Debug, Default, Clone)]
struct Measure {
    count: u64,
    sum: u64,
    max: u64,
    histogram: Histogram,
}

impl Measure {
    fn add(&mut self, row: &Aggregate) {
        self.count += row.count;
        self.sum = self.sum.saturating_add(row.sum_us);
        self.max = self.max.max(row.max_us);
        add_histogram(&mut self.histogram, &row.histogram);
    }

    fn percentile(&self, p: f64) -> Option<u64> {
        percentile(&self.histogram, p, self.max)
    }

    fn mean(&self) -> Option<f64> {
        (self.count > 0).then(|| self.sum as f64 / self.count as f64)
    }

    fn timing(&self) -> Value {
        if self.count == 0 {
            return Value::Null;
        }
        json!({
            "count": self.count,
            "average_us": self.mean().map(|m| m.round() as u64),
            "p50_us": self.percentile(50.0),
            "p95_us": self.percentile(95.0),
            "p99_us": self.percentile(99.0),
            "max_us": self.max,
        })
    }
}

/// One bucket of one queue.
#[derive(Debug, Default, Clone)]
struct Point {
    runs: u64,
    failed: u64,
    pushed: u64,
    depth: Measure,
    busy_us: u64,
}

/// Everything read about one queue in the window.
#[derive(Debug, Default, Clone)]
struct Queue {
    outcomes: BTreeMap<String, u64>,
    run: Measure,
    wait: Measure,
    pushed: u64,
    depth: Measure,
    /// The newest bucket with a depth sample, and its depth samples.
    latest_depth: Option<(DateTime<Utc>, Measure)>,
    busy: Measure,
    capacity_us: u64,
    workers: Measure,
    points: BTreeMap<DateTime<Utc>, Point>,
}

impl Queue {
    fn add(&mut self, kind: &str, outcome: Option<&str>, row: &Aggregate) {
        let point = self.points.entry(row.bucket_start).or_default();
        match kind {
            "run" => {
                let outcome = outcome.unwrap_or("processed");
                *self.outcomes.entry(outcome.to_string()).or_default() += row.count;
                self.run.add(row);
                point.runs += row.count;
                if outcome != "processed" && outcome != "released" {
                    point.failed += row.count;
                }
            }
            "wait" => self.wait.add(row),
            "pushed" => {
                self.pushed += row.count;
                point.pushed += row.count;
            }
            "depth" => {
                self.depth.add(row);
                point.depth.add(row);
                match &mut self.latest_depth {
                    Some((at, m)) if *at == row.bucket_start => m.add(row),
                    Some((at, _)) if *at > row.bucket_start => {}
                    latest => {
                        let mut m = Measure::default();
                        m.add(row);
                        *latest = Some((row.bucket_start, m));
                    }
                }
            }
            "busy" => {
                self.busy.add(row);
                point.busy_us = point.busy_us.saturating_add(row.sum_us);
            }
            "capacity" => self.capacity_us = self.capacity_us.saturating_add(row.sum_us),
            "workers" => self.workers.add(row),
            _ => {}
        }
    }

    fn outcome(&self, name: &str) -> u64 {
        self.outcomes.get(name).copied().unwrap_or(0)
    }

    /// Attempts that ended for good without succeeding, or could not be settled.
    fn failures(&self) -> u64 {
        self.outcome("failed") + self.outcome("timed_out") + self.outcome("error")
    }

    fn current_depth(&self) -> Option<u64> {
        self.latest_depth.as_ref().and_then(|(_, m)| m.mean()).map(|d| d.round() as u64)
    }
}

/// What to read, and how to answer.
#[derive(Debug, Clone, Default)]
pub struct QueueQuery {
    /// Only this queue.
    pub queue: Option<String>,
    /// `depth` (the default), `runs`, `wait`, `failures` or `queue`.
    pub order: Option<String>,
    /// The most rows.
    pub limit: usize,
    /// Each queue's points, bucket by bucket.
    pub series: bool,
}

/// Every queue seen in `rows` over the window, one row each, and their totals.
/// `minutes` is the window's length.
pub fn summarize(
    rows: &[Aggregate],
    period_s: u32,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    minutes: f64,
    query: &QueueQuery,
) -> (Vec<Value>, Value) {
    let first = aggregate::bucket_start(since, period_s);
    let last = aggregate::bucket_start(until, period_s);
    let mut queues: BTreeMap<String, Queue> = BTreeMap::new();
    for row in rows {
        if row.entry_type != EntryType::QueueFacet
            || row.period_s != period_s
            || row.bucket_start < first
            || row.bucket_start > last
        {
            continue;
        }
        let Some((kind, outcome, queue)) = parse(&row.key) else { continue };
        if query.queue.as_deref().is_some_and(|only| only != queue) {
            continue;
        }
        queues.entry(queue.to_string()).or_default().add(kind, outcome, row);
    }

    let window_us = (minutes * 60_000_000.0).max(1.0);
    let minutes = minutes.max(1.0 / 60.0);
    let mut totals = Totals::default();
    let mut out: Vec<(Value, Queue)> = queues
        .into_iter()
        .map(|(name, q)| {
            let row =
                queue_json(&name, &q, window_us, minutes, period_s, first, last, query.series);
            totals.add(&q, &row);
            (row, q)
        })
        .collect();

    let order = query.order.as_deref().unwrap_or("depth");
    out.sort_by(|(a, qa), (b, qb)| {
        let by = |row: &Value, q: &Queue| -> (f64, f64) {
            let runs = q.run.count as f64;
            match order {
                "runs" => (runs, 0.0),
                "wait" => (q.wait.percentile(95.0).unwrap_or(0) as f64, runs),
                "failures" => (q.failures() as f64, runs),
                _ => (row.pointer("/depth/current").and_then(Value::as_f64).unwrap_or(-1.0), runs),
            }
        };
        if order == "queue" {
            return a["queue"].as_str().cmp(&b["queue"].as_str());
        }
        by(b, qb)
            .partial_cmp(&by(a, qa))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a["queue"].as_str().cmp(&b["queue"].as_str()))
    });
    let rows: Vec<Value> = out.into_iter().take(query.limit.max(1)).map(|(row, _)| row).collect();
    (rows, totals.json(window_us, minutes))
}

/// A queue's status: `failing` when its jobs fail (5% or more of attempts) or
/// it is stalled (jobs waiting, workers sampling it, nothing run); `degraded`
/// when 1% or more fail, the p95 wait is a minute or more, or its slots are
/// 90% busy with jobs waiting; `idle` with nothing run and nothing waiting;
/// else `ok`.
fn status(q: &Queue, utilisation: Option<f64>) -> &'static str {
    let runs = q.run.count;
    let depth = q.current_depth().unwrap_or(0);
    let rate = if runs == 0 { 0.0 } else { q.failures() as f64 / runs as f64 };
    if runs > 0 && rate >= FAILING_RATE {
        return "failing";
    }
    if runs == 0 && depth > 0 && q.workers.count > 0 {
        return "failing";
    }
    if runs == 0 && depth == 0 {
        return "idle";
    }
    let waits = q.wait.percentile(95.0).unwrap_or(0) >= DEGRADED_WAIT_US;
    let saturated = depth > 0 && utilisation.is_some_and(|u| u >= DEGRADED_UTILISATION);
    if rate >= DEGRADED_RATE || waits || saturated {
        return "degraded";
    }
    "ok"
}

#[allow(clippy::too_many_arguments)]
fn queue_json(
    name: &str,
    q: &Queue,
    window_us: f64,
    minutes: f64,
    period_s: u32,
    first: DateTime<Utc>,
    last: DateTime<Utc>,
    with_series: bool,
) -> Value {
    let sampled = q.workers.count > 0;
    let utilisation =
        (q.capacity_us > 0).then(|| round(q.busy.sum as f64 / q.capacity_us as f64, 4));
    // The longest interval sampled, so a peak sample reads back as jobs.
    let interval = q.workers.max.max(1) as f64;
    let concurrency = sampled.then(|| {
        json!({
            "average": round(q.busy.sum as f64 / window_us, 2),
            "capacity": round(q.capacity_us as f64 / window_us, 2),
            "utilisation": utilisation,
            "workers": round(q.workers.sum as f64 / window_us, 2),
            "peak_per_worker": (q.busy.max as f64 / interval).round() as u64,
        })
    });
    let depth = q.latest_depth.as_ref().map(|(at, _)| {
        json!({
            "current": q.current_depth(),
            "average": q.depth.mean().map(|m| round(m, 1)),
            "max": q.depth.max,
            "p95": q.depth.percentile(95.0),
            "sampled_at": at.to_rfc3339(),
        })
    });
    let runs = q.run.count;
    let failures = q.failures();
    let mut row = json!({
        "queue": name,
        "status": status(q, utilisation),
        "depth": depth,
        "throughput": {
            "runs": runs,
            "processed": q.outcome("processed"),
            "released": q.outcome("released"),
            "failed": q.outcome("failed"),
            "timed_out": q.outcome("timed_out"),
            "errors": q.outcome("error"),
            "per_minute": round(runs as f64 / minutes, 2),
            "processed_per_minute": round(q.outcome("processed") as f64 / minutes, 2),
            "pushed": q.pushed,
            "pushed_per_minute": round(q.pushed as f64 / minutes, 2),
        },
        "failure_rate": if runs == 0 { 0.0 } else { round(failures as f64 / runs as f64, 4) },
        "concurrency": concurrency,
        "run": q.run.timing(),
        "wait": q.wait.timing(),
    });
    if with_series {
        let period_us = f64::from(period_s.max(1)) * 1_000_000.0;
        let mut points = Vec::new();
        let mut at = first;
        while at <= last {
            let p = q.points.get(&at).cloned().unwrap_or_default();
            points.push(json!({
                "at": at.to_rfc3339(),
                "runs": p.runs,
                "failed": p.failed,
                "pushed": p.pushed,
                "depth": p.depth.mean().map(|m| round(m, 1)),
                "concurrency": sampled.then(|| round(p.busy_us as f64 / period_us, 2)),
            }));
            at += chrono::Duration::seconds(i64::from(period_s.max(1)));
        }
        row["series"] = json!(points);
    }
    row
}

/// Every queue's numbers added up.
#[derive(Debug, Default)]
struct Totals {
    queues: u64,
    runs: u64,
    failures: u64,
    pushed: u64,
    depth: u64,
    busy_us: u64,
    capacity_us: u64,
    by_status: BTreeMap<String, u64>,
}

impl Totals {
    fn add(&mut self, q: &Queue, row: &Value) {
        self.queues += 1;
        self.runs += q.run.count;
        self.failures += q.failures();
        self.pushed += q.pushed;
        self.depth += q.current_depth().unwrap_or(0);
        self.busy_us = self.busy_us.saturating_add(q.busy.sum);
        self.capacity_us = self.capacity_us.saturating_add(q.capacity_us);
        let status = row["status"].as_str().unwrap_or("ok").to_string();
        *self.by_status.entry(status).or_default() += 1;
    }

    fn json(&self, window_us: f64, minutes: f64) -> Value {
        json!({
            "queues": self.queues,
            "depth": self.depth,
            "runs": self.runs,
            "failures": self.failures,
            "per_minute": round(self.runs as f64 / minutes, 2),
            "pushed": self.pushed,
            "concurrency": round(self.busy_us as f64 / window_us, 2),
            "capacity": round(self.capacity_us as f64 / window_us, 2),
            "statuses": self.by_status,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn job(content: Value, us: u64) -> Entry {
        let family = content["family"].as_str().unwrap_or("SendInvoice").to_string();
        Entry {
            id: "e".into(),
            batch_id: "b".into(),
            entry_type: EntryType::Job,
            span_id: None,
            parent_span_id: None,
            family_hash: Some(family.clone()),
            summary: family,
            occurred_at: Utc.timestamp_opt(1_800_000_000, 0).unwrap(),
            duration_us: Some(us),
            tags: vec![],
            content,
        }
    }

    #[test]
    fn a_run_counts_under_its_outcome_and_queue_with_its_wait() {
        let entry = job(
            json!({ "messaging.destination.name": "mail", "rainier.job.outcome": "timed_out",
                    "messaging.queue.wait_ms": 1500 }),
            9_000,
        );
        assert_eq!(
            facets_of(&entry),
            [("run timed_out mail".to_string(), 9_000), ("wait mail".to_string(), 1_500_000)]
        );
    }

    #[test]
    fn a_dispatch_counts_as_pushed_unless_it_failed() {
        let ok = job(json!({ "family": "dispatch:SendInvoice" }), 300);
        assert_eq!(facets_of(&ok), [("pushed default".to_string(), 300)]);
        let failed =
            job(json!({ "family": "dispatch:SendInvoice", "error.type": "internal" }), 300);
        assert!(facets_of(&failed).is_empty());
    }

    fn rows(at: DateTime<Utc>) -> Vec<Aggregate> {
        let mut rows = Vec::new();
        // Two workers, sampled every 15 s, four times each in the minute.
        for (busy, depth) in [(2, 10), (2, 12), (1, 8), (1, 10)] {
            for worker_busy in [busy, 0] {
                rows.extend(sample_rows(
                    &Sample {
                        queue: "mail".into(),
                        depth: Some(depth),
                        busy: worker_busy,
                        capacity: 2,
                        interval_us: 15_000_000,
                    },
                    at,
                ));
            }
        }
        let runs = [
            job(
                json!({ "messaging.destination.name": "mail", "rainier.job.outcome": "processed",
                        "messaging.queue.wait_ms": 2000 }),
                4_000,
            ),
            job(
                json!({ "messaging.destination.name": "mail", "rainier.job.outcome": "failed" }),
                6_000,
            ),
        ];
        for entry in runs.iter().map(|e| Entry { occurred_at: at, ..e.clone() }) {
            for (key, value) in facets_of(&entry) {
                rows.push(Aggregate {
                    bucket_start: aggregate::bucket_start(at, 60),
                    period_s: 60,
                    entry_type: EntryType::QueueFacet,
                    key,
                    count: 1,
                    sum_us: value,
                    max_us: value,
                    histogram: Histogram::from([(aggregate::slot(value), 1)]),
                });
            }
        }
        aggregate::combine(rows)
    }

    #[test]
    fn depth_concurrency_and_throughput_read_back() {
        let at = Utc.timestamp_opt(1_800_000_000, 0).unwrap();
        let since = aggregate::bucket_start(at, 60);
        let until = since + chrono::Duration::seconds(59);
        let query = QueueQuery { limit: 10, series: true, ..QueueQuery::default() };
        let (rows, totals) = summarize(&rows(at), 60, since, until, 1.0, &query);

        assert_eq!(rows.len(), 1);
        let mail = &rows[0];
        assert_eq!(mail["queue"], "mail");
        assert_eq!(mail["depth"]["current"], 10, "the mean of the newest bucket's samples");
        assert_eq!(mail["depth"]["max"], 12);
        // 6 job-samples × 15 s over a minute: 1.5 running on average.
        assert_eq!(mail["concurrency"]["average"], 1.5);
        // 8 samples × 2 slots × 15 s: 4 slots — two workers of two.
        assert_eq!(mail["concurrency"]["capacity"], 4.0);
        assert_eq!(mail["concurrency"]["workers"], 2.0);
        assert_eq!(mail["concurrency"]["utilisation"], 0.375);
        assert_eq!(mail["concurrency"]["peak_per_worker"], 2);
        assert_eq!(mail["throughput"]["runs"], 2);
        assert_eq!(mail["throughput"]["failed"], 1);
        assert_eq!(mail["failure_rate"], 0.5);
        assert_eq!(mail["status"], "failing");
        assert_eq!(mail["wait"]["max_us"], 2_000_000);
        assert_eq!(mail["series"][0]["concurrency"], 1.5);
        assert_eq!(totals["depth"], 10);
        assert_eq!(totals["statuses"]["failing"], 1);
    }

    #[test]
    fn a_queue_with_work_waiting_and_nothing_run_is_stalled() {
        let at = Utc.timestamp_opt(1_800_000_000, 0).unwrap();
        let rows = sample_rows(
            &Sample {
                queue: "x".into(),
                depth: Some(40),
                busy: 0,
                capacity: 1,
                interval_us: 15_000_000,
            },
            at,
        );
        let since = aggregate::bucket_start(at, 60);
        let query = QueueQuery { limit: 10, ..QueueQuery::default() };
        let (rows, _) = summarize(&rows, 60, since, since, 1.0, &query);
        assert_eq!(rows[0]["status"], "failing");
        assert!(rows[0]["run"].is_null());
    }

    #[test]
    fn only_known_keys_are_read() {
        assert_eq!(parse("run failed mail"), Some(("run", Some("failed"), "mail")));
        assert_eq!(parse("depth a b"), Some(("depth", None, "a b")));
        assert_eq!(parse("nonsense x"), None);
        assert_eq!(parse("run"), None);
    }
}
