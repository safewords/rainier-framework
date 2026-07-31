//! The metrics an application records — [`Metrics`].

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// One metric's label set, ordered so two spellings of the same labels are one
/// series.
///
/// `BTreeMap` rather than a `Vec` for exactly that: `{method,route}` and
/// `{route,method}` are the same series to Prometheus, and two entries here
/// would be two lines that never add up.
pub type Labels = BTreeMap<String, String>;

/// Build a label set.
///
/// ```
/// # use rainier_metrics::labels;
/// let labels = labels([("method", "GET"), ("status", "200")]);
/// assert_eq!(labels.len(), 2);
/// ```
pub fn labels<K, V, I>(pairs: I) -> Labels
where
    I: IntoIterator<Item = (K, V)>,
    K: Into<String>,
    V: Into<String>,
{
    pairs.into_iter().map(|(key, value)| (key.into(), value.into())).collect()
}

/// What a metric measures, which decides how Prometheus may aggregate it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Only goes up, and resets to zero when the process restarts. A count of
    /// things that happened.
    Counter,
    /// Goes up and down. A current value — connections open, queue depth.
    Gauge,
    /// Observations in buckets, plus a sum and a count. A distribution.
    Histogram,
}

impl Kind {
    /// The word Prometheus expects in a `# TYPE` line.
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Counter => "counter",
            Kind::Gauge => "gauge",
            Kind::Histogram => "histogram",
        }
    }
}

/// The bucket boundaries a request-duration histogram uses, in seconds.
///
/// Prometheus's own defaults. They are weighted towards the fast end because
/// that is where a web request lives, and the point of a bucket is to be near
/// the value you care about — a p99 of 300ms cannot be read off buckets that
/// jump from 100ms to 10s.
pub const DEFAULT_BUCKETS: &[f64] =
    &[0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0];

/// One series: a label set and what has been recorded for it.
#[derive(Debug, Default)]
struct Series {
    /// Counters and gauges. Stored as bits so it is one atomic.
    value: AtomicU64,
    /// Histogram buckets, cumulative — `le` semantics are applied at render.
    buckets: Vec<AtomicU64>,
    /// The sum of every observation, as bits.
    sum: AtomicU64,
    /// How many observations.
    count: AtomicU64,
}

impl Series {
    fn new(buckets: usize) -> Self {
        Self {
            value: AtomicU64::new(0),
            buckets: (0..buckets).map(|_| AtomicU64::new(0)).collect(),
            sum: AtomicU64::new(0),
            count: AtomicU64::new(0),
        }
    }

    fn add(&self, delta: f64) {
        // A compare-and-swap loop, because floats have no atomic add. Contended
        // only by two threads recording the same series in the same instant,
        // which is rare enough that a spin is cheaper than a mutex per series.
        let mut current = self.value.load(Ordering::Relaxed);
        loop {
            let next = (f64::from_bits(current) + delta).to_bits();
            match self.value.compare_exchange_weak(
                current,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(actual) => current = actual,
            }
        }
    }

    fn set(&self, value: f64) {
        self.value.store(value.to_bits(), Ordering::Relaxed);
    }

    fn get(&self) -> f64 {
        f64::from_bits(self.value.load(Ordering::Relaxed))
    }

    fn observe(&self, value: f64, boundaries: &[f64]) {
        for (index, boundary) in boundaries.iter().enumerate() {
            if value <= *boundary {
                self.buckets[index].fetch_add(1, Ordering::Relaxed);
            }
        }

        self.count.fetch_add(1, Ordering::Relaxed);
        self.add_sum(value);
    }

    fn add_sum(&self, delta: f64) {
        let mut current = self.sum.load(Ordering::Relaxed);
        loop {
            let next = (f64::from_bits(current) + delta).to_bits();
            match self.sum.compare_exchange_weak(
                current,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(actual) => current = actual,
            }
        }
    }
}

/// One named metric and every series recorded under it.
struct Metric {
    kind: Kind,
    help: String,
    buckets: Vec<f64>,
    series: Mutex<BTreeMap<Labels, Arc<Series>>>,
}

/// Everything this process has recorded.
///
/// ```
/// # use rainier_metrics::{labels, Metrics};
/// let metrics = Metrics::new();
/// metrics.counter("jobs_processed_total", "Jobs the worker finished");
/// metrics.increment("jobs_processed_total", labels([("queue", "mail")]));
///
/// assert!(metrics.render().contains("jobs_processed_total{queue=\"mail\"} 1"));
/// ```
///
/// # Cardinality is the thing to be careful about
///
/// Every distinct label set is a series Prometheus stores forever. A label
/// holding a user id, a request id or a raw URL will multiply your storage by
/// however many of those exist — the classic way to take down a monitoring
/// system with a one-line change. Labels should have a small, bounded set of
/// values: a method, a status, a route **pattern**.
#[derive(Default)]
pub struct Metrics {
    metrics: Mutex<BTreeMap<String, Arc<Metric>>>,
}

impl Metrics {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Declare a counter.
    ///
    /// Declaring is separate from recording so the `# HELP` and `# TYPE` lines
    /// exist even before anything has happened — a dashboard querying a metric
    /// that has never fired should see zero, not an unknown series.
    pub fn counter(&self, name: impl Into<String>, help: impl Into<String>) {
        self.declare(name, help, Kind::Counter, Vec::new());
    }

    /// Declare a gauge.
    pub fn gauge(&self, name: impl Into<String>, help: impl Into<String>) {
        self.declare(name, help, Kind::Gauge, Vec::new());
    }

    /// Declare a histogram with [`DEFAULT_BUCKETS`].
    pub fn histogram(&self, name: impl Into<String>, help: impl Into<String>) {
        self.declare(name, help, Kind::Histogram, DEFAULT_BUCKETS.to_vec());
    }

    /// Declare a histogram with its own buckets, in ascending order.
    pub fn histogram_with(
        &self,
        name: impl Into<String>,
        help: impl Into<String>,
        buckets: impl Into<Vec<f64>>,
    ) {
        let mut buckets = buckets.into();
        // Out-of-order boundaries make the cumulative counts nonsense, and it
        // is the kind of mistake nobody sees until a quantile looks wrong.
        buckets.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        self.declare(name, help, Kind::Histogram, buckets);
    }

    fn declare(
        &self,
        name: impl Into<String>,
        help: impl Into<String>,
        kind: Kind,
        buckets: Vec<f64>,
    ) {
        let name = name.into();
        let mut metrics = self.lock();

        // Declaring twice is not an error — two components may both want a
        // metric — but the first declaration wins, so a later one cannot
        // silently change the type out from under the first.
        metrics.entry(name).or_insert_with(|| {
            Arc::new(Metric {
                kind,
                help: help.into(),
                buckets,
                series: Mutex::new(BTreeMap::new()),
            })
        });
    }

    /// Add one to a counter.
    pub fn increment(&self, name: &str, labels: Labels) {
        self.add(name, labels, 1.0);
    }

    /// Add `delta` to a counter.
    pub fn add(&self, name: &str, labels: Labels, delta: f64) {
        if let Some(series) = self.series(name, labels) {
            series.add(delta);
        }
    }

    /// Set a gauge.
    pub fn set(&self, name: &str, labels: Labels, value: f64) {
        if let Some(series) = self.series(name, labels) {
            series.set(value);
        }
    }

    /// Record one observation in a histogram.
    pub fn observe(&self, name: &str, labels: Labels, value: f64) {
        let Some(metric) = self.metric(name) else { return };
        let Some(series) = self.series(name, labels) else { return };

        series.observe(value, &metric.buckets);
    }

    /// The current value of one series, for a test or a health check.
    pub fn value(&self, name: &str, labels: &Labels) -> Option<f64> {
        let metric = self.metric(name)?;
        let series = metric.series.lock().ok()?;
        let series = series.get(labels)?;

        Some(match metric.kind {
            Kind::Histogram => series.count.load(Ordering::Relaxed) as f64,
            _ => series.get(),
        })
    }

    /// The declared metric names.
    pub fn names(&self) -> Vec<String> {
        self.lock().keys().cloned().collect()
    }

    /// Everything, in the Prometheus text exposition format.
    ///
    /// What a `/metrics` endpoint returns. Ordered — metrics by name, series by
    /// labels — because a scrape that differs only in ordering makes every diff
    /// useless.
    pub fn render(&self) -> String {
        let metrics = self.lock().clone();
        let mut out = String::new();

        for (name, metric) in metrics {
            out.push_str(&format!("# HELP {name} {}\n", metric.help));
            out.push_str(&format!("# TYPE {name} {}\n", metric.kind.as_str()));

            let Ok(series) = metric.series.lock() else { continue };
            for (labels, values) in series.iter() {
                match metric.kind {
                    Kind::Histogram => {
                        render_histogram(&mut out, &name, labels, values, metric.buckets.as_slice())
                    }
                    _ => {
                        out.push_str(&format!(
                            "{name}{} {}\n",
                            render_labels(labels, None),
                            format_value(values.get())
                        ));
                    }
                }
            }
        }
        out
    }

    fn metric(&self, name: &str) -> Option<Arc<Metric>> {
        self.lock().get(name).cloned()
    }

    fn series(&self, name: &str, labels: Labels) -> Option<Arc<Series>> {
        let metric = self.metric(name)?;
        let buckets = metric.buckets.len();
        let mut series = metric.series.lock().ok()?;

        Some(Arc::clone(series.entry(labels).or_insert_with(|| Arc::new(Series::new(buckets)))))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Arc<Metric>>> {
        self.metrics.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl std::fmt::Debug for Metrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Metrics").field("names", &self.names()).finish()
    }
}

/// `{method="GET",status="200"}`, or empty when there are none.
///
/// `extra` appends one more pair, which is how a bucket's `le` gets in without
/// the label map having to carry it.
fn render_labels(labels: &Labels, extra: Option<(&str, String)>) -> String {
    if labels.is_empty() && extra.is_none() {
        return String::new();
    }

    let mut pairs: Vec<String> =
        labels.iter().map(|(key, value)| format!("{key}=\"{}\"", escape(value))).collect();

    if let Some((key, value)) = extra {
        pairs.push(format!("{key}=\"{value}\""));
    }
    format!("{{{}}}", pairs.join(","))
}

/// A histogram is several lines: one per bucket, then the sum and the count.
fn render_histogram(
    out: &mut String,
    name: &str,
    labels: &Labels,
    series: &Series,
    buckets: &[f64],
) {
    for (index, boundary) in buckets.iter().enumerate() {
        let count = series.buckets[index].load(Ordering::Relaxed);
        out.push_str(&format!(
            "{name}_bucket{} {count}\n",
            render_labels(labels, Some(("le", format_value(*boundary))))
        ));
    }

    // `+Inf` is required, and it is the total: every observation is at most
    // infinity. A scrape without it is rejected.
    let count = series.count.load(Ordering::Relaxed);
    out.push_str(&format!(
        "{name}_bucket{} {count}\n",
        render_labels(labels, Some(("le", "+Inf".to_string())))
    ));

    let sum = f64::from_bits(series.sum.load(Ordering::Relaxed));
    out.push_str(&format!("{name}_sum{} {}\n", render_labels(labels, None), format_value(sum)));
    out.push_str(&format!("{name}_count{} {count}\n", render_labels(labels, None)));
}

/// Prometheus wants a plain decimal, and an integer-valued float should not
/// render as `1` when it is a count and `1.5` when it is a duration.
fn format_value(value: f64) -> String {
    if value.is_infinite() {
        return if value.is_sign_positive() { "+Inf".into() } else { "-Inf".into() };
    }
    if value == value.trunc() && value.abs() < 1e15 {
        return format!("{}", value as i64);
    }
    format!("{value}")
}

/// A label value is quoted, so a quote, a backslash or a newline in one would
/// end the line early.
fn escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_counter_counts() {
        let metrics = Metrics::new();
        metrics.counter("things_total", "Things");

        metrics.increment("things_total", labels([("kind", "a")]));
        metrics.increment("things_total", labels([("kind", "a")]));
        metrics.increment("things_total", labels([("kind", "b")]));

        assert_eq!(metrics.value("things_total", &labels([("kind", "a")])), Some(2.0));
        assert_eq!(metrics.value("things_total", &labels([("kind", "b")])), Some(1.0));
    }

    #[test]
    fn label_order_does_not_make_a_second_series() {
        // Two spellings of one series would be two lines that never add up.
        let metrics = Metrics::new();
        metrics.counter("things_total", "Things");

        metrics.increment("things_total", labels([("a", "1"), ("b", "2")]));
        metrics.increment("things_total", labels([("b", "2"), ("a", "1")]));

        assert_eq!(metrics.value("things_total", &labels([("a", "1"), ("b", "2")])), Some(2.0));
        assert_eq!(metrics.render().matches("things_total{").count(), 1);
    }

    #[test]
    fn a_gauge_goes_both_ways() {
        let metrics = Metrics::new();
        metrics.gauge("connections", "Open connections");

        metrics.set("connections", Labels::new(), 5.0);
        metrics.set("connections", Labels::new(), 3.0);

        assert_eq!(metrics.value("connections", &Labels::new()), Some(3.0));
    }

    #[test]
    fn a_histogram_buckets_cumulatively() {
        let metrics = Metrics::new();
        metrics.histogram_with("latency", "Latency", vec![0.1, 1.0]);

        metrics.observe("latency", Labels::new(), 0.05);
        metrics.observe("latency", Labels::new(), 0.5);
        metrics.observe("latency", Labels::new(), 5.0);

        let rendered = metrics.render();

        // Cumulative: 0.05 is in both buckets, 0.5 only in the second.
        assert!(rendered.contains("latency_bucket{le=\"0.1\"} 1"), "{rendered}");
        assert!(rendered.contains("latency_bucket{le=\"1\"} 2"), "{rendered}");
        assert!(rendered.contains("latency_bucket{le=\"+Inf\"} 3"), "{rendered}");
        assert!(rendered.contains("latency_count 3"), "{rendered}");
        assert!(rendered.contains("latency_sum 5.55"), "{rendered}");
    }

    #[test]
    fn buckets_out_of_order_are_sorted_rather_than_believed() {
        let metrics = Metrics::new();
        metrics.histogram_with("latency", "Latency", vec![1.0, 0.1]);

        metrics.observe("latency", Labels::new(), 0.05);
        let rendered = metrics.render();

        let first = rendered.find("le=\"0.1\"").expect("the small bucket");
        let second = rendered.find("le=\"1\"").expect("the large one");
        assert!(first < second, "{rendered}");
    }

    #[test]
    fn a_declared_metric_appears_before_anything_is_recorded() {
        // A dashboard querying it should see the metric exists, not an unknown
        // series.
        let metrics = Metrics::new();
        metrics.counter("never_fired_total", "Nothing yet");

        let rendered = metrics.render();
        assert!(rendered.contains("# HELP never_fired_total Nothing yet"), "{rendered}");
        assert!(rendered.contains("# TYPE never_fired_total counter"), "{rendered}");
    }

    #[test]
    fn recording_an_undeclared_metric_does_nothing_rather_than_inventing_a_type() {
        // Prometheus needs a TYPE. Guessing one from the first call would make
        // a typo into a metric with the wrong semantics.
        let metrics = Metrics::new();
        metrics.increment("typo_total", Labels::new());

        assert!(metrics.render().is_empty());
        assert_eq!(metrics.value("typo_total", &Labels::new()), None);
    }

    #[test]
    fn declaring_twice_keeps_the_first_type() {
        let metrics = Metrics::new();
        metrics.counter("shared", "A counter");
        metrics.gauge("shared", "Now a gauge?");

        assert!(metrics.render().contains("# TYPE shared counter"));
    }

    #[test]
    fn a_hostile_label_value_cannot_end_the_line() {
        let metrics = Metrics::new();
        metrics.counter("things_total", "Things");
        metrics.increment("things_total", labels([("path", "a\"b\nc\\d")]));

        let rendered = metrics.render();
        let line = rendered.lines().find(|l| l.starts_with("things_total{")).expect("a line");

        assert!(line.contains(r#"a\"b\nc\\d"#), "{line}");
        assert_eq!(rendered.lines().filter(|l| l.starts_with("things_total{")).count(), 1);
    }

    #[test]
    fn a_whole_number_renders_without_a_decimal_point() {
        assert_eq!(format_value(1.0), "1");
        assert_eq!(format_value(0.5), "0.5");
        assert_eq!(format_value(f64::INFINITY), "+Inf");
    }

    #[test]
    fn the_output_is_stable_across_scrapes() {
        // A diff between two scrapes should show what changed, not a reordering.
        let metrics = Metrics::new();
        metrics.counter("a_total", "A");
        metrics.counter("b_total", "B");
        metrics.increment("b_total", labels([("z", "1")]));
        metrics.increment("a_total", labels([("y", "2")]));

        assert_eq!(metrics.render(), metrics.render());
        assert!(
            metrics.render().find("a_total").unwrap() < metrics.render().find("b_total").unwrap()
        );
    }
}
