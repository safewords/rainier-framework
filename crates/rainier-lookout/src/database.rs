//! Database connections: how full each process's pool is, and — on MySQL and
//! MariaDB — how many connections the application's user holds on the
//! server against its cap, and the server against its own.
//!
//! Everything here is a [`DatabaseFacet`](EntryType::DatabaseFacet)
//! aggregate row, so every store keeps it without a table of its own and
//! prunes it with the rest. One thing writes them: the framework's sampler,
//! which publishes a `db.pool.sample` activity per connection on its interval
//! (fifteen seconds) in every process. The subscriber rolls each sample
//! straight into the counts the writer flushes; a sample belongs to no unit of
//! work.
//!
//! The pool's numbers are the pool's own bookkeeping and need no connection,
//! so they are recorded even when every connection is checked out. The
//! server's are asked on an idle connection only: with none idle the sample
//! says so (`unanswered`) instead of queueing behind the requests waiting for
//! one.
//!
//! # Keys
//!
//! | Key | One per | Its "duration" |
//! |---|---|---|
//! | `in_use {connection}` | sample | connections checked out × the interval, µs |
//! | `open {connection}` | sample | connections open × the interval, µs |
//! | `capacity {connection}` | sample | the pool's `max_connections` × the interval, µs |
//! | `processes {connection}` | sample | the interval, µs |
//! | `saturation {connection}` | sample | that process's pool in use, **percent** |
//! | `user {connection}` | sample the server answered | the user's connections |
//! | `user_limit {connection}` | sample the server answered, the user capped | the cap |
//! | `server {connection}` | sample the server answered | the server's connections |
//! | `server_max {connection}` | sample the server answered | `max_connections` |
//! | `unanswered {connection}` | sample the server did not answer | 0 |
//!
//! The first four are **time**, so they add across replicas, as the queues'
//! slots do: a window's `in_use` summed over every process, divided by the
//! window, is how many connections the service held on average, however many
//! replicas there were. `saturation` is per process, so its maximum is the
//! fullest any one pool got. The server's are gauges every replica sees the
//! same of: averaged and maximised, never added.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use crate::aggregate::{self, Histogram, PERIODS};
use crate::entry::{Aggregate, EntryType};

/// The longest connection name kept; a longer one is cut.
const MAX_NAME: usize = 100;

/// A user or server at or above this share of its cap is `failing`.
const FAILING_SHARE: f64 = 0.95;
/// At or above this, `degraded`.
const DEGRADED_SHARE: f64 = 0.8;

/// One process's sample of one connection — a `db.pool.sample` activity's
/// fields.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sample {
    /// The connection's name.
    pub connection: String,
    /// The backend (`mysql`, `postgresql`, `sqlite`).
    pub system: Option<String>,
    /// Connections the pool has open.
    pub open: u64,
    /// Of those, idle.
    pub idle: u64,
    /// The pool's `max_connections`.
    pub max: u64,
    /// How long the sample stands for, in microseconds.
    pub interval_us: u64,
    /// The user's connections on the server, when it answered.
    pub user: Option<u64>,
    /// The user's cap, when it answered and there is one.
    pub user_limit: Option<u64>,
    /// The server's connections, when it answered.
    pub server: Option<u64>,
    /// The server's `max_connections`, when it answered.
    pub server_max: Option<u64>,
    /// Why the server did not answer (`busy`, or the error), when it did not.
    pub server_error: Option<String>,
}

fn name(raw: &str) -> String {
    let name = raw.trim();
    let name = if name.is_empty() { "default" } else { name };
    name.chars().filter(|c| !c.is_whitespace()).take(MAX_NAME).collect()
}

/// The aggregate rows `sample`, taken at `at`, adds: one per key and period.
pub fn sample_rows(sample: &Sample, at: DateTime<Utc>) -> Vec<Aggregate> {
    if sample.interval_us == 0 {
        return Vec::new();
    }
    let c = name(&sample.connection);
    let i = sample.interval_us;
    let in_use = sample.open.saturating_sub(sample.idle);
    let mut values = vec![
        (format!("in_use {c}"), in_use.saturating_mul(i)),
        (format!("open {c}"), sample.open.saturating_mul(i)),
        (format!("capacity {c}"), sample.max.saturating_mul(i)),
        (format!("processes {c}"), i),
    ];
    if let Some(pct) = in_use.saturating_mul(100).checked_div(sample.max) {
        values.push((format!("saturation {c}"), pct));
    }
    let answered = sample.user.is_some() && sample.server.is_some();
    for (key, value) in [
        ("user", sample.user),
        ("user_limit", sample.user_limit),
        ("server", sample.server),
        ("server_max", sample.server_max),
    ] {
        if let Some(v) = value {
            values.push((format!("{key} {c}"), v));
        }
    }
    if !answered {
        values.push((format!("unanswered {c}"), 0));
    }
    let mut rows = Vec::new();
    for (key, value) in values {
        for &period_s in PERIODS {
            rows.push(Aggregate {
                bucket_start: aggregate::bucket_start(at, period_s),
                period_s,
                entry_type: EntryType::DatabaseFacet,
                key: key.clone(),
                count: 1,
                sum_us: value,
                max_us: value,
                histogram: Histogram::new(),
            });
        }
    }
    rows
}

/// A key's parts: what it measures, and the connection.
fn parse(key: &str) -> Option<(&str, &str)> {
    let (kind, connection) = key.split_once(' ')?;
    matches!(
        kind,
        "in_use"
            | "open"
            | "capacity"
            | "processes"
            | "saturation"
            | "user"
            | "user_limit"
            | "server"
            | "server_max"
            | "unanswered"
    )
    .then_some((kind, connection))
}

/// Count, sum and max of one measure.
#[derive(Debug, Default, Clone, Copy)]
struct Measure {
    count: u64,
    sum: u64,
    max: u64,
}

impl Measure {
    fn add(&mut self, row: &Aggregate) {
        self.count += row.count;
        self.sum = self.sum.saturating_add(row.sum_us);
        self.max = self.max.max(row.max_us);
    }

    fn mean(&self) -> Option<f64> {
        (self.count > 0).then(|| self.sum as f64 / self.count as f64)
    }

    fn max(&self) -> Option<u64> {
        (self.count > 0).then_some(self.max)
    }
}

/// Everything read about one connection in the window, and in its newest
/// bucket.
#[derive(Debug, Default, Clone)]
struct Connection {
    measures: BTreeMap<String, Measure>,
    latest: Option<(DateTime<Utc>, BTreeMap<String, Measure>)>,
}

impl Connection {
    fn add(&mut self, kind: &str, row: &Aggregate) {
        self.measures.entry(kind.to_string()).or_default().add(row);
        match &mut self.latest {
            Some((at, _)) if *at > row.bucket_start => {}
            Some((at, m)) if *at == row.bucket_start => {
                m.entry(kind.to_string()).or_default().add(row)
            }
            latest => {
                let mut m = BTreeMap::new();
                m.entry(kind.to_string()).or_insert_with(Measure::default).add(row);
                *latest = Some((row.bucket_start, m));
            }
        }
    }

    fn get(&self, kind: &str) -> Measure {
        self.measures.get(kind).copied().unwrap_or_default()
    }
}

fn round(x: f64, places: i32) -> f64 {
    let f = 10f64.powi(places);
    (x * f).round() / f
}

fn share(used: Option<u64>, limit: Option<u64>) -> Option<f64> {
    match (used, limit) {
        (Some(u), Some(l)) if l > 0 => Some(round(u as f64 / l as f64, 4)),
        _ => None,
    }
}

/// Every connection over the window, busiest pool first, and the totals:
/// see `docs/lookout.md`, "Database connections".
pub fn summarize(
    rows: &[Aggregate],
    period_s: u32,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    minutes: f64,
) -> (Vec<Value>, Value) {
    let first = aggregate::bucket_start(since, period_s);
    let last = aggregate::bucket_start(until, period_s);
    let mut connections: BTreeMap<String, Connection> = BTreeMap::new();
    for row in rows {
        if row.entry_type != EntryType::DatabaseFacet
            || row.period_s != period_s
            || row.bucket_start < first
            || row.bucket_start > last
        {
            continue;
        }
        let Some((kind, connection)) = parse(&row.key) else { continue };
        connections.entry(connection.to_string()).or_default().add(kind, row);
    }

    let window_us = (minutes * 60_000_000.0).max(1.0);
    let mut worst = "ok";
    let mut out: Vec<(f64, Value)> = connections
        .into_iter()
        .map(|(connection, c)| {
            let average = |kind: &str| round(c.get(kind).sum as f64 / window_us, 2);
            let in_use = average("in_use");
            let capacity = average("capacity");
            let samples = c.get("processes").count;
            let unanswered = c.get("unanswered").count;
            let user = c.get("user");
            let user_limit = c.get("user_limit").max();
            let server = c.get("server");
            let server_max = c.get("server_max").max();
            let user_share = share(user.max(), user_limit);
            let server_share = share(server.max(), server_max);
            let peak = c.get("saturation").max();
            let shares = [user_share, server_share, peak.map(|p| p as f64 / 100.0)];
            let top = shares.iter().flatten().fold(0.0_f64, |a, b| a.max(*b));
            let status = if top >= FAILING_SHARE {
                "failing"
            } else if top >= DEGRADED_SHARE {
                "degraded"
            } else {
                "ok"
            };
            if status == "failing" || (status == "degraded" && worst == "ok") {
                worst = status;
            }
            let latest = c.latest.as_ref().map(|(at, m)| {
                let mean = |k: &str| m.get(k).and_then(Measure::mean).map(|v| round(v, 2));
                json!({
                    "at": at.to_rfc3339(),
                    "user": mean("user"),
                    "server": mean("server"),
                    "saturation_pct": m.get("saturation").and_then(Measure::max),
                })
            });
            let value = json!({
                "connection": connection,
                "status": status,
                "samples": samples,
                "processes": round(c.get("processes").sum as f64 / window_us, 2),
                "pool": {
                    "in_use": in_use,
                    "open": average("open"),
                    "capacity": capacity,
                    "utilisation": (capacity > 0.0).then(|| round(in_use / capacity, 4)),
                    "peak_saturation_pct": peak,
                },
                "user": {
                    "connections": user.mean().map(|v| round(v, 2)),
                    "max": user.max(),
                    "limit": user_limit,
                    "share": user_share,
                },
                "server": {
                    "connections": server.mean().map(|v| round(v, 2)),
                    "max": server.max(),
                    "limit": server_max,
                    "share": server_share,
                },
                "unanswered": unanswered,
                "latest": latest,
            });
            (top, value)
        })
        .collect();
    out.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    let totals = json!({ "connections": out.len(), "status": worst });
    (out.into_iter().map(|(_, v)| v).collect(), totals)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn sample(open: u64, idle: u64, user: Option<u64>) -> Sample {
        Sample {
            connection: "mysql".into(),
            system: Some("mysql".into()),
            open,
            idle,
            max: 10,
            interval_us: 15_000_000,
            user,
            user_limit: user.map(|_| 20),
            server: user.map(|u| u + 1),
            server_max: user.map(|_| 151),
            server_error: if user.is_none() { Some("busy".into()) } else { None },
        }
    }

    #[test]
    fn two_replicas_add_their_pools_and_share_the_server() {
        let t = at("2026-11-28T12:00:10Z");
        let mut rows = Vec::new();
        // Four samples a minute from each of two replicas.
        for k in 0..4 {
            let t = t + chrono::Duration::seconds(15 * k);
            rows.extend(sample_rows(&sample(6, 2, Some(18)), t));
            rows.extend(sample_rows(&sample(10, 0, None), t));
        }
        let (out, totals) =
            summarize(&rows, 60, at("2026-11-28T12:00:00Z"), at("2026-11-28T12:00:59Z"), 1.0);
        assert_eq!(out.len(), 1);
        let c = &out[0];
        assert_eq!(c["pool"]["in_use"], 14.0, "4 + 10, averaged over the minute");
        assert_eq!(c["pool"]["capacity"], 20.0);
        assert_eq!(c["pool"]["peak_saturation_pct"], 100, "the full replica");
        assert_eq!(c["processes"], 2.0);
        assert_eq!(c["user"]["max"], 18);
        assert_eq!(c["user"]["limit"], 20);
        assert_eq!(c["user"]["share"], 0.9);
        assert_eq!(c["server"]["limit"], 151);
        assert_eq!(c["unanswered"], 4, "the full replica could not ask");
        assert_eq!(c["status"], "failing", "one pool is full");
        assert_eq!(totals["status"], "failing");
        assert_eq!(c["latest"]["user"], 18.0);
    }

    #[test]
    fn a_quiet_pool_is_ok_and_rows_outside_the_window_are_left_out() {
        let rows = sample_rows(&sample(2, 2, Some(3)), at("2026-11-28T12:00:10Z"));
        let (out, totals) =
            summarize(&rows, 60, at("2026-11-28T12:00:00Z"), at("2026-11-28T12:00:59Z"), 1.0);
        assert_eq!(out[0]["status"], "ok");
        assert_eq!(out[0]["pool"]["utilisation"], 0.0);
        assert_eq!(totals["connections"], 1);
        let (none, _) =
            summarize(&rows, 60, at("2026-11-28T12:05:00Z"), at("2026-11-28T12:05:59Z"), 1.0);
        assert!(none.is_empty());
        assert!(sample_rows(&Sample::default(), at("2026-11-28T12:00:10Z")).is_empty());
        assert_eq!(name("  "), "default");
    }
}
