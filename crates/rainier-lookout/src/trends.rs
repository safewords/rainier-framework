//! Series, forecasts and insights, computed from aggregate rows.
//!
//! Everything here is a pure function over [`Aggregate`] rows a store has
//! already returned, so every store draws the same graph and raises the same
//! insight. Nothing is learned or kept between calls.
//!
//! - **A series** is one point per bucket from `since` to now, zero-filled,
//!   so a quiet minute shows as a zero rather than as a gap the chart joins
//!   across.
//! - **A forecast** extends a series by a least-squares line through its
//!   points, for the count and the average separately, never below zero. It
//!   is a trend, not a model: it says where the last window was heading.
//! - **Insights** compare the current window with the one before it, per type
//!   and per key, and report what is new, what changed by more than the
//!   configured percentage, and where the forecast is heading. The thresholds
//!   are [`InsightsConfig`].

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::aggregate::{add_histogram, bucket_start, percentile, Histogram, AGGREGATED};
use crate::entry::{Aggregate, EntryType};

/// One bucket of a series.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Point {
    /// When the bucket starts.
    pub bucket_start: DateTime<Utc>,
    /// How many were recorded in it.
    pub count: u64,
    /// Their total duration, in microseconds.
    pub sum_us: u64,
    /// The longest, in microseconds.
    pub max_us: u64,
    /// `sum_us / count`, or zero for an empty bucket.
    pub average_us: u64,
    /// The median, in microseconds. Absent for an empty bucket, and for one
    /// recorded before buckets kept a duration histogram.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p50_us: Option<u64>,
    /// The 90th percentile, in microseconds; absent as `p50_us` is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p90_us: Option<u64>,
    /// The 95th percentile, in microseconds; absent as `p50_us` is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p95_us: Option<u64>,
    /// The 99th percentile, in microseconds; absent as `p50_us` is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p99_us: Option<u64>,
}

/// One projected bucket.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Projection {
    /// When the bucket starts.
    pub bucket_start: DateTime<Utc>,
    /// The projected count, never negative.
    pub count: f64,
    /// The projected average duration, in microseconds, never negative.
    pub average_us: f64,
}

/// The thresholds insights are raised at. Read from `LOOKOUT_INSIGHTS_*`, and
/// a viewer may override each one per request.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct InsightsConfig {
    /// Off, the dashboard shows no insights and draws no forecast.
    pub enabled: bool,
    /// The window compared with the one before it, in minutes.
    pub window_minutes: u64,
    /// A key needs at least this many in a window before a change in it is
    /// worth reporting.
    pub min_count: u64,
    /// A change smaller than this, in percent, is not reported.
    pub change_percent: u64,
    /// How many buckets a forecast extends a series by.
    pub forecast_buckets: usize,
}

impl Default for InsightsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            window_minutes: 60,
            min_count: 5,
            change_percent: 50,
            forecast_buckets: 6,
        }
    }
}

impl InsightsConfig {
    /// The bucket length that gives a window at least a dozen points.
    pub fn period_s(&self) -> u32 {
        period_for(Duration::minutes(self.window_minutes as i64))
    }
}

/// The bucket length that gives `span` at least a dozen points, from the
/// lengths aggregates are kept at.
pub fn period_for(span: Duration) -> u32 {
    let minutes = span.num_minutes();
    if minutes <= 120 {
        60
    } else if minutes <= 5 * 24 * 60 {
        600
    } else {
        3600
    }
}

/// A series of `rows` from `since` to `now`, one point per `period_s` bucket.
/// Rows of other periods are ignored; rows of every key in a bucket are added
/// together, so filter by key first for one key's series.
pub fn series(
    rows: &[Aggregate],
    period_s: u32,
    since: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Vec<Point> {
    let first = bucket_start(since, period_s);
    let last = bucket_start(now, period_s);
    #[derive(Default)]
    struct Bucket {
        count: u64,
        sum_us: u64,
        max_us: u64,
        histogram: Histogram,
    }
    let mut totals: BTreeMap<DateTime<Utc>, Bucket> = BTreeMap::new();
    for row in rows.iter().filter(|r| r.period_s == period_s) {
        if row.bucket_start < first || row.bucket_start > last {
            continue;
        }
        let slot = totals.entry(row.bucket_start).or_default();
        slot.count += row.count;
        slot.sum_us = slot.sum_us.saturating_add(row.sum_us);
        slot.max_us = slot.max_us.max(row.max_us);
        // Every key's histogram in the bucket, added: the percentiles of
        // everything in it, not an average of each key's percentiles.
        add_histogram(&mut slot.histogram, &row.histogram);
    }
    let step = Duration::seconds(i64::from(period_s.max(1)));
    let mut points = Vec::new();
    let mut at = first;
    while at <= last {
        let bucket = totals.remove(&at).unwrap_or_default();
        let p = |q: f64| percentile(&bucket.histogram, q, bucket.max_us);
        points.push(Point {
            bucket_start: at,
            count: bucket.count,
            sum_us: bucket.sum_us,
            max_us: bucket.max_us,
            average_us: bucket.sum_us.checked_div(bucket.count).unwrap_or(0),
            p50_us: p(50.0),
            p90_us: p(90.0),
            p95_us: p(95.0),
            p99_us: p(99.0),
        });
        at += step;
    }
    points
}

/// The least-squares line through `values`, as (intercept, slope) over the
/// indexes `0..n`. A single value is a flat line.
fn line(values: &[f64]) -> (f64, f64) {
    let n = values.len() as f64;
    if values.len() < 2 {
        return (values.first().copied().unwrap_or(0.0), 0.0);
    }
    let mean_x = (n - 1.0) / 2.0;
    let mean_y = values.iter().sum::<f64>() / n;
    let (mut num, mut den) = (0.0, 0.0);
    for (i, y) in values.iter().enumerate() {
        let dx = i as f64 - mean_x;
        num += dx * (y - mean_y);
        den += dx * dx;
    }
    let slope = if den == 0.0 { 0.0 } else { num / den };
    (mean_y - slope * mean_x, slope)
}

/// `horizon` buckets past the end of `points`, along the least-squares line
/// through them. The current, still-filling bucket is left out of the fit,
/// so a half-counted minute does not read as a fall. The average is fitted
/// over the buckets that recorded something.
pub fn forecast(points: &[Point], period_s: u32, horizon: usize) -> Vec<Projection> {
    let Some(last) = points.last() else { return Vec::new() };
    let complete = &points[..points.len() - 1];
    if complete.len() < 2 || horizon == 0 {
        return Vec::new();
    }
    let counts: Vec<f64> = complete.iter().map(|p| p.count as f64).collect();
    let (c0, c1) = line(&counts);
    let busy: Vec<(usize, f64)> = complete
        .iter()
        .enumerate()
        .filter(|(_, p)| p.count > 0)
        .map(|(i, p)| (i, p.average_us as f64))
        .collect();
    let average_at = |x: f64| -> f64 {
        if busy.len() < 2 {
            return busy.first().map_or(0.0, |(_, v)| *v);
        }
        let n = busy.len() as f64;
        let mean_x = busy.iter().map(|(i, _)| *i as f64).sum::<f64>() / n;
        let mean_y = busy.iter().map(|(_, v)| v).sum::<f64>() / n;
        let (mut num, mut den) = (0.0, 0.0);
        for (i, v) in &busy {
            let dx = *i as f64 - mean_x;
            num += dx * (v - mean_y);
            den += dx * dx;
        }
        let slope = if den == 0.0 { 0.0 } else { num / den };
        mean_y + slope * (x - mean_x)
    };
    let step = Duration::seconds(i64::from(period_s.max(1)));
    (0..horizon)
        .map(|h| {
            // The last point is index `complete.len()`; projections follow it.
            let x = (complete.len() + 1 + h) as f64;
            Projection {
                bucket_start: last.bucket_start + step * (h as i32 + 1),
                count: (c0 + c1 * x).max(0.0),
                average_us: average_at(x).max(0.0),
            }
        })
        .collect()
}

/// How much an insight matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Something got worse: more failures, slower work.
    Bad,
    /// Worth a look either way.
    Notice,
    /// Something got better.
    Good,
}

/// What an insight says happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InsightKind {
    /// Seen in this window, not in the one before.
    New,
    /// Happening more often.
    Rising,
    /// Happening less often.
    Falling,
    /// Taking longer on average.
    Slower,
    /// Taking less time on average.
    Faster,
    /// The trend line says the next window will be this far from this one.
    Projected,
}

/// One thing the numbers say.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Insight {
    /// What happened.
    pub kind: InsightKind,
    /// How much it matters.
    pub severity: Severity,
    /// What was measured.
    #[serde(rename = "type")]
    pub entry_type: EntryType,
    /// The family key, or `None` for the type as a whole.
    pub key: Option<String>,
    /// The value in the current window: a count, or an average in
    /// microseconds for [`Slower`](InsightKind::Slower) and
    /// [`Faster`](InsightKind::Faster).
    pub current: f64,
    /// The same in the previous window (or, for a projection, the current).
    pub previous: f64,
    /// The change from `previous` to `current`, in percent; `None` for new.
    pub change_percent: Option<f64>,
}

#[derive(Default, Clone, Copy)]
struct Window {
    count: u64,
    sum_us: u64,
}

impl Window {
    fn average(self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.sum_us as f64 / self.count as f64
        }
    }
}

fn change(previous: f64, current: f64) -> f64 {
    if previous == 0.0 {
        return 0.0;
    }
    (current - previous) / previous * 100.0
}

/// Failures are bad when they rise; everything else is bad when it slows.
fn worse_when_more(entry_type: EntryType) -> bool {
    entry_type == EntryType::Exception
}

/// The insights in `rows` — aggregate rows of every type, at
/// `config.period_s()`, covering at least the last two windows before `now`.
/// Most severe first, then by size of change; at most `limit`.
pub fn insights(
    rows: &[Aggregate],
    config: &InsightsConfig,
    now: DateTime<Utc>,
    limit: usize,
) -> Vec<Insight> {
    let period = config.period_s();
    let window = Duration::minutes(config.window_minutes.max(1) as i64);
    let current_from = bucket_start(now - window, period);
    let previous_from = bucket_start(now - window - window, period);
    let threshold = config.change_percent as f64;
    let min = config.min_count.max(1);

    // (type, key) → (previous, current); key "" is the type as a whole.
    let mut windows: BTreeMap<(EntryType, Option<String>), (Window, Window)> = BTreeMap::new();
    for row in rows.iter().filter(|r| r.period_s == period) {
        if row.bucket_start < previous_from {
            continue;
        }
        let current = row.bucket_start >= current_from;
        for key in [None, Some(row.key.clone())] {
            let slot = windows.entry((row.entry_type, key)).or_default();
            let w = if current { &mut slot.1 } else { &mut slot.0 };
            w.count += row.count;
            w.sum_us = w.sum_us.saturating_add(row.sum_us);
        }
    }

    let mut found = Vec::new();
    for ((entry_type, key), (previous, current)) in &windows {
        let entry_type = *entry_type;
        let more_is_bad = worse_when_more(entry_type);
        let make = |kind, severity, current: f64, previous: f64, pct: Option<f64>| Insight {
            kind,
            severity,
            entry_type,
            key: key.clone(),
            current,
            previous,
            change_percent: pct,
        };

        if key.is_some() && previous.count == 0 && current.count >= min {
            let severity = if more_is_bad { Severity::Bad } else { Severity::Notice };
            found.push(make(InsightKind::New, severity, current.count as f64, 0.0, None));
            continue;
        }
        if previous.count >= min || current.count >= min {
            let pct = change(previous.count as f64, current.count as f64);
            if previous.count > 0 && pct.abs() >= threshold {
                let rising = pct > 0.0;
                let severity = match (rising, more_is_bad) {
                    (true, true) => Severity::Bad,
                    (false, true) => Severity::Good,
                    _ => Severity::Notice,
                };
                let kind = if rising { InsightKind::Rising } else { InsightKind::Falling };
                found.push(make(
                    kind,
                    severity,
                    current.count as f64,
                    previous.count as f64,
                    Some(pct),
                ));
            }
        }
        // Exceptions have no duration worth comparing.
        if entry_type != EntryType::Exception && previous.count >= min && current.count >= min {
            let pct = change(previous.average(), current.average());
            if pct.abs() >= threshold {
                let (kind, severity) = if pct > 0.0 {
                    (InsightKind::Slower, Severity::Bad)
                } else {
                    (InsightKind::Faster, Severity::Good)
                };
                found.push(make(kind, severity, current.average(), previous.average(), Some(pct)));
            }
        }
    }

    // Where each type as a whole is heading over the next window.
    if config.forecast_buckets > 0 {
        let buckets = (window.num_seconds() / i64::from(period)).max(1) as usize;
        for &entry_type in AGGREGATED {
            let typed: Vec<Aggregate> =
                rows.iter().filter(|r| r.entry_type == entry_type).cloned().collect();
            let points = series(&typed, period, now - window, now);
            let complete = points.len().saturating_sub(1);
            let observed: u64 = points[..complete].iter().map(|p| p.count).sum();
            if complete < 3 || observed < min {
                continue;
            }
            let projected: f64 =
                forecast(&points, period, buckets).iter().map(|p| p.count).sum::<f64>();
            let current = observed as f64 * buckets as f64 / complete as f64;
            let pct = change(current, projected);
            if pct.abs() >= threshold {
                let severity = match (pct > 0.0, worse_when_more(entry_type)) {
                    (true, true) => Severity::Bad,
                    (false, true) => Severity::Good,
                    _ => Severity::Notice,
                };
                found.push(Insight {
                    kind: InsightKind::Projected,
                    severity,
                    entry_type,
                    key: None,
                    current: projected,
                    previous: current,
                    change_percent: Some(pct),
                });
            }
        }
    }

    found.sort_by(|a, b| {
        a.severity.cmp(&b.severity).then_with(|| {
            let size = |i: &Insight| i.change_percent.map_or(f64::INFINITY, f64::abs);
            size(b).total_cmp(&size(a))
        })
    });
    found.truncate(limit);
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn row(t: EntryType, key: &str, at: i64, count: u64, sum_us: u64) -> Aggregate {
        Aggregate {
            bucket_start: Utc.timestamp_opt(at, 0).unwrap(),
            period_s: 60,
            entry_type: t,
            key: key.into(),
            count,
            sum_us,
            max_us: sum_us,
            histogram: Default::default(),
        }
    }

    fn at(s: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(s, 0).unwrap()
    }

    #[test]
    fn a_series_zero_fills_quiet_buckets_and_adds_keys_together() {
        let rows = vec![
            row(EntryType::Request, "a", 60, 2, 200),
            row(EntryType::Request, "b", 60, 1, 400),
            row(EntryType::Request, "a", 180, 1, 50),
        ];
        let points = series(&rows, 60, at(60), at(200));
        let counts: Vec<u64> = points.iter().map(|p| p.count).collect();
        assert_eq!(counts, vec![3, 0, 1]);
        assert_eq!(points[0].average_us, 200);
        assert_eq!(points[0].max_us, 400);
        assert_eq!(points[1].average_us, 0);
        // Rows written before buckets kept a histogram have no percentiles,
        // and neither does an empty bucket.
        assert_eq!(points[0].p99_us, None);
        assert_eq!(points[1].p50_us, None);
    }

    #[test]
    fn a_series_carries_each_buckets_percentiles_across_its_keys() {
        use crate::aggregate::slot;

        // One bucket: 98 fast requests on one route, 2 slow on another. The
        // percentiles are of the bucket's 100 requests together.
        let mut fast = row(EntryType::Request, "a", 60, 98, 1_000);
        fast.histogram.insert(slot(1_000), 98);
        fast.max_us = 1_000;
        let mut slow = row(EntryType::Request, "b", 60, 2, 500_000);
        slow.histogram.insert(slot(500_000), 2);
        slow.max_us = 500_000;

        let points = series(&[fast, slow], 60, at(60), at(70));
        assert_eq!(points.len(), 1);
        let p = &points[0];
        let near = |v: Option<u64>, want: u64| {
            let v = v.expect("a percentile") as f64;
            (v - want as f64).abs() / want as f64 <= 0.1
        };
        assert!(near(p.p50_us, 1_000), "{p:?}");
        assert!(near(p.p90_us, 1_000), "{p:?}");
        assert!(near(p.p95_us, 1_000), "{p:?}");
        assert!(near(p.p99_us, 500_000), "{p:?}");
        assert!(p.p99_us.unwrap() <= p.max_us);
    }

    #[test]
    fn a_forecast_follows_the_trend_of_complete_buckets_and_never_goes_below_zero() {
        let rising: Vec<Aggregate> =
            (0..5).map(|i| row(EntryType::Job, "j", i * 60, (i as u64 + 1) * 10, 1000)).collect();
        // The last bucket (index 5) is still filling: one entry.
        let mut rows = rising;
        rows.push(row(EntryType::Job, "j", 300, 1, 100));
        let points = series(&rows, 60, at(0), at(310));
        let next = forecast(&points, 60, 2);
        assert_eq!(next.len(), 2);
        assert_eq!(next[0].bucket_start, at(360));
        // 10, 20, 30, 40, 50 → 70 at index 6, 80 at 7 — the half bucket ignored.
        assert!((next[0].count - 70.0).abs() < 1e-6, "{next:?}");
        assert!((next[1].count - 80.0).abs() < 1e-6);

        let falling: Vec<Aggregate> =
            (0..4).map(|i| row(EntryType::Job, "j", i * 60, 40 - i as u64 * 10, 0)).collect();
        let points = series(&falling, 60, at(0), at(240));
        assert!(forecast(&points, 60, 3).iter().all(|p| p.count >= 0.0));
    }

    #[test]
    fn insights_report_new_rising_and_slower_with_their_severity() {
        let config = InsightsConfig {
            window_minutes: 10,
            min_count: 3,
            change_percent: 50,
            forecast_buckets: 0,
            ..InsightsConfig::default()
        };
        let now = at(1200);
        let rows = vec![
            // Previous window (600..1200 is current; 0..600 previous).
            row(EntryType::Exception, "old", 60, 4, 0),
            row(EntryType::Job, "send", 60, 5, 5 * 1000),
            // Current window.
            row(EntryType::Exception, "old", 660, 12, 0),
            row(EntryType::Exception, "fresh", 720, 3, 0),
            row(EntryType::Job, "send", 660, 5, 5 * 3000),
        ];
        let found = insights(&rows, &config, now, 50);
        let has = |kind, key: &str, severity| {
            found
                .iter()
                .any(|i| i.kind == kind && i.key.as_deref() == Some(key) && i.severity == severity)
        };
        assert!(has(InsightKind::New, "fresh", Severity::Bad), "{found:#?}");
        assert!(has(InsightKind::Rising, "old", Severity::Bad));
        assert!(has(InsightKind::Slower, "send", Severity::Bad));
        assert_eq!(found[0].severity, Severity::Bad);
        assert!(!found.iter().any(|i| i.key.as_deref() == Some("send")
            && matches!(i.kind, InsightKind::Rising | InsightKind::Falling)));
    }

    #[test]
    fn small_numbers_and_small_changes_raise_nothing() {
        let config = InsightsConfig {
            window_minutes: 10,
            min_count: 5,
            change_percent: 50,
            forecast_buckets: 0,
            ..InsightsConfig::default()
        };
        let rows = vec![
            row(EntryType::Exception, "rare", 660, 2, 0),
            row(EntryType::Job, "steady", 60, 10, 10_000),
            row(EntryType::Job, "steady", 660, 12, 12_600),
        ];
        assert!(insights(&rows, &config, at(1200), 50).is_empty());
    }

    #[test]
    fn a_climbing_type_is_projected_to_keep_climbing() {
        let config = InsightsConfig {
            window_minutes: 10,
            min_count: 1,
            change_percent: 20,
            forecast_buckets: 6,
            ..InsightsConfig::default()
        };
        let rows: Vec<Aggregate> = (0..11)
            .map(|i| row(EntryType::Exception, "x", 600 + i * 60, 1 + i as u64 * 3, 0))
            .collect();
        let found = insights(&rows, &config, at(1260), 50);
        assert!(
            found.iter().any(|i| i.kind == InsightKind::Projected
                && i.entry_type == EntryType::Exception
                && i.severity == Severity::Bad),
            "{found:#?}"
        );
    }

    #[test]
    fn the_period_gives_a_window_enough_points() {
        assert_eq!(period_for(Duration::minutes(60)), 60);
        assert_eq!(period_for(Duration::hours(24)), 600);
        assert_eq!(period_for(Duration::days(7)), 3600);
    }
}
