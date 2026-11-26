//! Request and job facets: every request counted by route, method and status
//! class, and every job run by job and outcome — what the health API groups,
//! filters and judges by.
//!
//! In production most requests are not kept (only the slow, the failed and
//! the sampled), so nothing here reads batches. A facet is an aggregate row
//! like any other — a [`RequestFacet`](EntryType::RequestFacet) or
//! [`JobFacet`](EntryType::JobFacet) row, rolled up by
//! [`rollup_entries`](crate::aggregate::rollup_entries) beside the request's
//! or job's own row, on the writer, from every unit of work counted — so
//! every store keeps them without a table or a key of their own, prunes them
//! with the rest, and the numbers cover every request.
//!
//! # Keys
//!
//! | Type | Key | Example |
//! |---|---|---|
//! | `request_facet` | `{METHOD} {class} {route}` | `GET 2xx posts.index` |
//! | `job_facet` | `{ok\|failed} {job}` | `failed SendInvoice` |
//!
//! The method is one of the standard nine or `OTHER`; the class is `1xx` to
//! `5xx`, or `unknown` when the request recorded no status; the route is the
//! matched route's name, else its pattern, else `(no route)` — never the
//! path, so a facet's cardinality is the application's routes, whatever a
//! client sends.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use crate::aggregate::{self, add_histogram, percentile, Histogram};
use crate::entry::{Aggregate, Entry, EntryType};

/// What a request with no matched route is filed under.
pub const NO_ROUTE: &str = "(no route)";

/// The methods a facet names; anything else is `OTHER`.
const METHODS: &[&str] =
    &["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS", "TRACE", "CONNECT"];

/// The status classes a facet can carry.
pub const STATUS_CLASSES: &[&str] = &["1xx", "2xx", "3xx", "4xx", "5xx", "unknown"];

/// The facet `entry` is also counted under, with its key: a request's route,
/// method and status class; a job run's outcome. `None` for anything else —
/// a job's dispatch included, which is not a run.
pub fn facet_of(entry: &Entry) -> Option<(EntryType, String)> {
    let text = |key: &str| entry.content.get(key).and_then(Value::as_str);
    match entry.entry_type {
        EntryType::Request => {
            let method = method(text("http.request.method").unwrap_or(""));
            let status = entry.content.get("http.response.status_code").and_then(|v| {
                v.as_u64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
            });
            let route = text("rainier.route.name")
                .or_else(|| text("http.route"))
                .filter(|r| !r.is_empty())
                .unwrap_or(NO_ROUTE);
            Some((EntryType::RequestFacet, request_key(method, status_class(status), route)))
        }
        EntryType::Job => {
            let job = entry.family_key();
            if job.starts_with("dispatch:") {
                return None;
            }
            let failed = entry.tags.iter().any(|t| t == "failed")
                || text("rainier.job.outcome")
                    .is_some_and(|o| matches!(o, "failed" | "timed_out" | "error"));
            let outcome = if failed { "failed" } else { "ok" };
            Some((EntryType::JobFacet, format!("{outcome} {job}")))
        }
        _ => None,
    }
}

/// `GET`, or `OTHER` for a method outside the standard nine.
fn method(raw: &str) -> &'static str {
    let upper = raw.trim().to_ascii_uppercase();
    METHODS.iter().copied().find(|m| *m == upper).unwrap_or("OTHER")
}

/// `2xx` for 200 to 299, and so on; `unknown` without a status, or outside
/// 100 to 599.
pub fn status_class(status: Option<u64>) -> &'static str {
    match status {
        Some(100..=199) => "1xx",
        Some(200..=299) => "2xx",
        Some(300..=399) => "3xx",
        Some(400..=499) => "4xx",
        Some(500..=599) => "5xx",
        _ => "unknown",
    }
}

/// A request facet's key.
pub fn request_key(method: &str, class: &str, route: &str) -> String {
    format!("{method} {class} {route}")
}

/// A request facet's key, read back: (method, status class, route).
pub fn parse_request_key(key: &str) -> Option<(&str, &str, &str)> {
    let mut parts = key.splitn(3, ' ');
    Some((parts.next()?, parts.next()?, parts.next()?))
}

/// Which of the three a `/api/requests` grouping keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Grouping {
    /// Group by route.
    pub route: bool,
    /// Group by method.
    pub method: bool,
    /// Group by status class.
    pub status: bool,
}

/// What `/api/requests` and `/api/health` narrow to. Every field narrows;
/// `None` means any.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RequestFilter {
    /// The route, exactly — or a glob, with `*`.
    pub route: Option<String>,
    /// The method, any case.
    pub method: Option<String>,
    /// The status class (`5xx`).
    pub status: Option<String>,
    /// Text in the route, any case.
    pub search: Option<String>,
}

impl RequestFilter {
    /// Whether the facet (method, class, route) passes.
    pub fn matches(&self, method: &str, class: &str, route: &str) -> bool {
        if self.route.as_ref().is_some_and(|r| !crate::config::glob(r, route)) {
            return false;
        }
        if self.method.as_ref().is_some_and(|m| !m.eq_ignore_ascii_case(method)) {
            return false;
        }
        if self.status.as_ref().is_some_and(|s| !s.eq_ignore_ascii_case(class)) {
            return false;
        }
        if let Some(q) = &self.search {
            if !route.to_lowercase().contains(&q.to_lowercase()) {
                return false;
            }
        }
        true
    }
}

/// How `/api/requests` orders its rows: each descending.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RequestOrder {
    /// Most requests first.
    #[default]
    Count,
    /// Slowest p95 first.
    P95,
    /// Most 5xx first.
    Errors,
    /// Highest share of 5xx first.
    ErrorRate,
}

/// Requests added up: what one `/api/requests` row, or the totals, is made
/// from.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RequestTotals {
    /// How many.
    pub count: u64,
    /// How many answered 5xx.
    pub errors_5xx: u64,
    /// How many answered 4xx.
    pub client_errors_4xx: u64,
    /// The slowest, in microseconds.
    pub max_us: u64,
    /// Every duration's slot.
    pub histogram: Histogram,
}

impl RequestTotals {
    /// Add one facet row, of status class `class`.
    pub fn add(&mut self, row: &Aggregate, class: &str) {
        self.count += row.count;
        match class {
            "5xx" => self.errors_5xx += row.count,
            "4xx" => self.client_errors_4xx += row.count,
            _ => {}
        }
        self.max_us = self.max_us.max(row.max_us);
        add_histogram(&mut self.histogram, &row.histogram);
    }

    /// The share answered 5xx, `0.0` with no requests.
    pub fn error_rate(&self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.errors_5xx as f64 / self.count as f64
        }
    }

    /// The `p`th percentile of the durations, in microseconds.
    pub fn percentile(&self, p: f64) -> Option<u64> {
        percentile(&self.histogram, p, self.max_us)
    }

    /// As a `/api/requests` row: the three grouped fields, `null` when
    /// grouped away, then the numbers.
    pub fn row_json(
        &self,
        route: Option<&str>,
        method: Option<&str>,
        status: Option<&str>,
    ) -> Value {
        json!({
            "route": route,
            "method": method,
            "status": status,
            "count": self.count,
            "errors_5xx": self.errors_5xx,
            "error_rate": round(self.error_rate(), 4),
            "p50_us": self.percentile(50.0),
            "p95_us": self.percentile(95.0),
            "p99_us": self.percentile(99.0),
            "max_us": self.max_us,
        })
    }
}

/// `x` to `places` decimal places.
pub(crate) fn round(x: f64, places: i32) -> f64 {
    let scale = 10f64.powi(places);
    (x * scale).round() / scale
}

/// Facet rows in `[since, until]` passing `filter`, each with its parsed
/// key. A row is in the window when its bucket starts at or after the bucket
/// holding `since` and at or before the one holding `until`.
pub fn select<'a>(
    rows: &'a [Aggregate],
    period_s: u32,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    filter: &RequestFilter,
) -> Vec<(&'a Aggregate, &'a str, &'a str, &'a str)> {
    let first = aggregate::bucket_start(since, period_s);
    let last = aggregate::bucket_start(until, period_s);
    rows.iter()
        .filter(|r| {
            r.entry_type == EntryType::RequestFacet
                && r.period_s == period_s
                && r.bucket_start >= first
                && r.bucket_start <= last
        })
        .filter_map(|r| {
            let (method, class, route) = parse_request_key(&r.key)?;
            filter.matches(method, class, route).then_some((r, method, class, route))
        })
        .collect()
}

/// The `/api/requests` rows: `selected` grouped by `grouping`, ordered and
/// cut to `limit`, as JSON; and the totals over all of them.
pub fn group(
    selected: &[(&Aggregate, &str, &str, &str)],
    grouping: Grouping,
    order: RequestOrder,
    limit: usize,
) -> (Vec<Value>, RequestTotals) {
    type GroupKey<'a> = (Option<&'a str>, Option<&'a str>, Option<&'a str>);
    let mut groups: BTreeMap<GroupKey<'_>, RequestTotals> = BTreeMap::new();
    let mut totals = RequestTotals::default();
    for &(row, method, class, route) in selected {
        let key = (
            grouping.route.then_some(route),
            grouping.method.then_some(method),
            grouping.status.then_some(class),
        );
        groups.entry(key).or_default().add(row, class);
        totals.add(row, class);
    }
    let mut ordered: Vec<(GroupKey<'_>, RequestTotals)> = groups.into_iter().collect();
    let score = |t: &RequestTotals| -> f64 {
        match order {
            RequestOrder::Count => t.count as f64,
            RequestOrder::P95 => t.percentile(95.0).unwrap_or(0) as f64,
            RequestOrder::Errors => t.errors_5xx as f64,
            RequestOrder::ErrorRate => t.error_rate(),
        }
    };
    ordered.sort_by(|(ka, a), (kb, b)| {
        score(b).total_cmp(&score(a)).then_with(|| b.count.cmp(&a.count)).then_with(|| ka.cmp(kb))
    });
    let rows = ordered
        .into_iter()
        .take(limit)
        .map(|((route, method, status), t)| t.row_json(route, method, status))
        .collect();
    (rows, totals)
}

/// One point per bucket of `period_s` from the bucket holding `since` to the
/// one holding `until`, zero-filled: `{"at", "count", "errors_5xx",
/// "p95_us"}`.
pub fn series(
    selected: &[(&Aggregate, &str, &str, &str)],
    period_s: u32,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
) -> Vec<Value> {
    let mut per_bucket: BTreeMap<DateTime<Utc>, RequestTotals> = BTreeMap::new();
    for &(row, _, class, _) in selected {
        per_bucket.entry(row.bucket_start).or_default().add(row, class);
    }
    let first = aggregate::bucket_start(since, period_s);
    let last = aggregate::bucket_start(until, period_s);
    let step = chrono::Duration::seconds(i64::from(period_s));
    let mut points = Vec::new();
    let mut at = first;
    let empty = RequestTotals::default();
    while at <= last {
        let t = per_bucket.get(&at).unwrap_or(&empty);
        points.push(json!({
            "at": at.to_rfc3339(),
            "count": t.count,
            "errors_5xx": t.errors_5xx,
            "p95_us": t.percentile(95.0),
        }));
        at += step;
    }
    points
}

/// The health status of a window: `idle` without requests; `failing` at an
/// error rate of 5% or more; `ok` under 1% with the p95 under four times the
/// Apdex target; `degraded` otherwise.
pub fn status(totals: &RequestTotals, apdex_t_ms: u64) -> &'static str {
    if totals.count == 0 {
        return "idle";
    }
    let rate = totals.error_rate();
    if rate >= 0.05 {
        return "failing";
    }
    let p95 = totals.percentile(95.0).unwrap_or(0);
    if rate < 0.01 && p95 < apdex_t_ms.saturating_mul(4_000) {
        "ok"
    } else {
        "degraded"
    }
}

/// Job-facet rows in the window: (runs, failed).
pub fn job_counts(
    rows: &[Aggregate],
    period_s: u32,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
) -> (u64, u64) {
    let first = aggregate::bucket_start(since, period_s);
    let last = aggregate::bucket_start(until, period_s);
    rows.iter()
        .filter(|r| {
            r.entry_type == EntryType::JobFacet
                && r.period_s == period_s
                && r.bucket_start >= first
                && r.bucket_start <= last
        })
        .fold((0, 0), |(runs, failed), r| {
            let f = if r.key.starts_with("failed ") { r.count } else { 0 };
            (runs + r.count, failed + f)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn request(method: &str, status: Option<u64>, route: Option<&str>, us: u64) -> Entry {
        let mut content = serde_json::Map::new();
        content.insert("http.request.method".into(), json!(method));
        if let Some(status) = status {
            content.insert("http.response.status_code".into(), json!(status));
        }
        if let Some(route) = route {
            content.insert("rainier.route.name".into(), json!(route));
        }
        Entry {
            id: "e".into(),
            batch_id: "b".into(),
            entry_type: EntryType::Request,
            span_id: None,
            parent_span_id: None,
            family_hash: route.map(str::to_string),
            summary: String::new(),
            occurred_at: Utc.timestamp_opt(0, 0).unwrap(),
            duration_us: Some(us),
            tags: vec![],
            content: Value::Object(content),
        }
    }

    #[test]
    fn a_request_is_faceted_by_method_class_and_route() {
        let (t, key) = facet_of(&request("get", Some(503), Some("posts.index"), 10)).unwrap();
        assert_eq!(t, EntryType::RequestFacet);
        assert_eq!(key, "GET 5xx posts.index");
        assert_eq!(parse_request_key(&key), Some(("GET", "5xx", "posts.index")));
        // A client's invented method and no route stay bounded.
        let (_, key) = facet_of(&request("BREW", None, None, 10)).unwrap();
        assert_eq!(key, "OTHER unknown (no route)");
        // A status given as text still classes.
        let mut e = request("POST", None, Some("a"), 1);
        e.content["http.response.status_code"] = json!("404");
        assert_eq!(facet_of(&e).unwrap().1, "POST 4xx a");
    }

    #[test]
    fn a_job_run_is_faceted_by_outcome_and_a_dispatch_is_not() {
        let mut job = request("", None, None, 5);
        job.entry_type = EntryType::Job;
        job.family_hash = Some("SendInvoice".into());
        assert_eq!(facet_of(&job).unwrap(), (EntryType::JobFacet, "ok SendInvoice".into()));
        job.tags.push("failed".into());
        assert_eq!(facet_of(&job).unwrap().1, "failed SendInvoice");
        job.family_hash = Some("dispatch:SendInvoice".into());
        assert_eq!(facet_of(&job), None);
    }

    #[test]
    fn rollup_counts_facets_beside_the_route() {
        let entries = vec![
            request("GET", Some(200), Some("a"), 100),
            request("GET", Some(500), Some("a"), 900),
            request("POST", Some(201), Some("b"), 50),
        ];
        let rows = aggregate::rollup_entries(&entries);
        let facets: Vec<_> = rows
            .iter()
            .filter(|r| r.entry_type == EntryType::RequestFacet && r.period_s == 60)
            .map(|r| (r.key.as_str(), r.count))
            .collect();
        assert_eq!(facets, vec![("GET 2xx a", 1), ("GET 5xx a", 1), ("POST 2xx b", 1)]);
        let routes = rows.iter().filter(|r| r.entry_type == EntryType::Request).count();
        assert_eq!(routes, 2 * aggregate::PERIODS.len(), "the route rows are unchanged");
    }

    #[test]
    fn grouping_filtering_and_ordering() {
        let entries = vec![
            request("GET", Some(200), Some("posts.index"), 100),
            request("GET", Some(200), Some("posts.index"), 200),
            request("GET", Some(500), Some("posts.index"), 300),
            request("POST", Some(422), Some("posts.store"), 50),
            request("GET", Some(200), Some("users.show"), 5_000),
        ];
        let rows = aggregate::rollup_entries(&entries);
        let t0 = Utc.timestamp_opt(0, 0).unwrap();
        let all = select(&rows, 60, t0, t0, &RequestFilter::default());
        let (by_route, totals) =
            group(&all, Grouping { route: true, ..Default::default() }, RequestOrder::Count, 10);
        assert_eq!(totals.count, 5);
        assert_eq!((totals.errors_5xx, totals.client_errors_4xx), (1, 1));
        assert_eq!(by_route[0]["route"], "posts.index");
        assert_eq!(by_route[0]["count"], 3);
        assert_eq!(by_route[0]["method"], Value::Null);
        assert_eq!(by_route[0]["error_rate"], json!(0.3333));

        let (by_p95, _) =
            group(&all, Grouping { route: true, ..Default::default() }, RequestOrder::P95, 1);
        assert_eq!(by_p95.len(), 1);
        assert_eq!(by_p95[0]["route"], "users.show");

        let posts = RequestFilter {
            route: Some("posts.*".into()),
            method: Some("get".into()),
            ..Default::default()
        };
        let some = select(&rows, 60, t0, t0, &posts);
        let (by_status, totals) =
            group(&some, Grouping { status: true, ..Default::default() }, RequestOrder::Errors, 10);
        assert_eq!(totals.count, 3);
        assert_eq!(by_status[0]["status"], "5xx");
        assert_eq!(by_status[0]["route"], Value::Null);

        let q = RequestFilter {
            search: Some("USERS".into()),
            status: Some("2XX".into()),
            ..Default::default()
        };
        assert_eq!(select(&rows, 60, t0, t0, &q).len(), 1);
    }

    #[test]
    fn status_follows_the_contract() {
        let mut t = RequestTotals::default();
        assert_eq!(status(&t, 300), "idle");
        t.count = 1000;
        t.max_us = 10_000;
        t.histogram.insert(aggregate::slot(10_000), 1000);
        assert_eq!(status(&t, 300), "ok");
        t.errors_5xx = 20;
        assert_eq!(status(&t, 300), "degraded", "2% is not ok");
        t.errors_5xx = 50;
        assert_eq!(status(&t, 300), "failing", "5% fails");
        t.errors_5xx = 0;
        t.histogram = Histogram::from([(aggregate::slot(2_000_000), 1000)]);
        t.max_us = 2_000_000;
        assert_eq!(status(&t, 300), "degraded", "p95 over 4T");
    }

    #[test]
    fn series_is_zero_filled() {
        let mut late = request("GET", Some(500), Some("a"), 10);
        late.occurred_at = Utc.timestamp_opt(150, 0).unwrap();
        let rows = aggregate::rollup_entries(&[request("GET", Some(200), Some("a"), 10), late]);
        let t0 = Utc.timestamp_opt(0, 0).unwrap();
        let until = Utc.timestamp_opt(179, 0).unwrap();
        let all = select(&rows, 60, t0, until, &RequestFilter::default());
        let points = series(&all, 60, t0, until);
        let counts: Vec<_> =
            points.iter().map(|p| (p["count"].clone(), p["errors_5xx"].clone())).collect();
        assert_eq!(counts, vec![(json!(1), json!(0)), (json!(0), json!(0)), (json!(1), json!(1))]);
        assert_eq!(points[1]["p95_us"], Value::Null);
    }
}
