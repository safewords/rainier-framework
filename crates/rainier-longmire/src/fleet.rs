//! The fleet: every source's health, requests, queues, real-user timings and
//! exceptions in one answer, and the map of who calls whom.
//!
//! Each answer is assembled from what the sources' Lookouts already serve to
//! a reader — their rollups count every request, call and page view, kept or
//! not — merged here with each row naming its service. Nothing is stored.

use std::collections::BTreeMap;

use serde_json::{json, Map, Value};

use crate::{Longmire, Part, Source};

/// What Longmire may ask a source's Lookout for, and the query parameters
/// each takes — nothing else is forwarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Endpoint {
    /// `/api/health`: the service's status, rates and latency over a window.
    Health,
    /// `/api/requests`: requests grouped by route, method and status class.
    Requests,
    /// `/api/rum/vitals`: real users' page metrics.
    RumVitals,
    /// `/api/rum/errors`: real users' script errors.
    RumErrors,
    /// `/api/cases`: exceptions, by fingerprint.
    Cases,
    /// `/api/queues`: each job queue's depth, concurrency, throughput and
    /// waits.
    Queues,
}

const WINDOW: [&str; 3] = ["since", "until", "period"];
const RUM_FILTERS: [&str; 6] = ["page", "browser", "os", "device", "connection", "country"];
/// The longest value forwarded: a route glob or a page path, not a payload.
const MAX_VALUE: usize = 200;

impl Endpoint {
    /// Every endpoint.
    pub const ALL: [Endpoint; 6] = [
        Endpoint::Health,
        Endpoint::Requests,
        Endpoint::RumVitals,
        Endpoint::RumErrors,
        Endpoint::Cases,
        Endpoint::Queues,
    ];

    /// Its path under `{lookout}/api/`.
    pub fn path(self) -> &'static str {
        match self {
            Self::Health => "health",
            Self::Requests => "requests",
            Self::RumVitals => "rum/vitals",
            Self::RumErrors => "rum/errors",
            Self::Cases => "cases",
            Self::Queues => "queues",
        }
    }

    /// The endpoint at `path` (`rum/vitals`), if it is one.
    pub fn parse(path: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|e| e.path() == path.trim_matches('/'))
    }

    /// The query parameters forwarded for it.
    pub fn params(self) -> Vec<&'static str> {
        let mut params: Vec<&'static str> = WINDOW.to_vec();
        match self {
            Self::Health => params.push("apdex_t_ms"),
            Self::Requests => params
                .extend(["group", "route", "method", "status", "q", "order", "limit", "series"]),
            Self::RumVitals => {
                params.extend(["group", "order", "limit"]);
                params.extend(RUM_FILTERS);
            }
            Self::RumErrors => {
                params.push("limit");
                params.extend(RUM_FILTERS);
            }
            Self::Cases => return vec!["status", "severity", "q", "limit"],
            Self::Queues => params.extend(["queue", "order", "limit", "series"]),
        }
        params
    }

    /// `raw` (a query string, still percent-encoded) with only this
    /// endpoint's parameters, each once (the first), none empty and none
    /// longer than 200 characters. Values pass through still encoded, so
    /// nothing in one can start another parameter.
    pub fn filter(self, raw: &str) -> String {
        let allowed = self.params();
        let mut seen: Vec<&str> = Vec::new();
        let mut out: Vec<String> = Vec::new();
        for pair in raw.split('&') {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            let Some(name) = allowed.iter().find(|a| **a == key) else { continue };
            if value.is_empty() || value.len() > MAX_VALUE || seen.contains(name) {
                continue;
            }
            seen.push(name);
            out.push(format!("{key}={value}"));
        }
        out.join("&")
    }
}

/// The `data` of one source's answer.
fn data(body: &Value) -> &Value {
    body.get("data").unwrap_or(&Value::Null)
}

fn source_row(source: &Source, outcome: &Result<Value, String>) -> Value {
    match outcome {
        Ok(_) => json!({ "id": source.id, "name": source.name, "status": "ok", "reason": null }),
        Err(reason) => {
            json!({ "id": source.id, "name": source.name, "status": "unknown", "reason": reason })
        }
    }
}

/// `rows` from every source, each with `service` and `service_name`, sorted
/// by `order` (largest first; rows without it last), at most `limit`.
fn merged_rows(parts: &[Part], rows_at: &str, order: &str, limit: usize) -> Vec<Value> {
    let mut rows: Vec<Value> = Vec::new();
    for (source, outcome) in parts {
        let Ok(body) = outcome else { continue };
        let found = match rows_at {
            "" => data(body).as_array(),
            at => data(body).get(at).and_then(Value::as_array),
        };
        for row in found.into_iter().flatten() {
            let mut row = row.clone();
            if let Some(object) = row.as_object_mut() {
                object.insert("service".into(), json!(source.id));
                object.insert("service_name".into(), json!(source.name));
            }
            rows.push(row);
        }
    }
    rows.sort_by(|a, b| {
        let key = |row: &Value| row.get(order).and_then(Value::as_f64);
        key(b).partial_cmp(&key(a)).unwrap_or(std::cmp::Ordering::Equal).then_with(|| {
            // Rows without the measure last.
            key(a).is_none().cmp(&key(b).is_none())
        })
    });
    rows.truncate(limit);
    rows
}

impl Longmire {
    /// One source's answer for `endpoint`, as its Lookout gave it — for a
    /// page about one service. `Err` with the reason when there is none, or
    /// when `source` is not one.
    pub async fn one(
        &self,
        source: &str,
        endpoint: Endpoint,
        raw_query: &str,
    ) -> Result<Value, String> {
        let Some(source) = self.sources.iter().find(|s| s.id == source) else {
            return Err("No such service is a source here.".into());
        };
        let query = endpoint.filter(raw_query);
        tokio::time::timeout(
            self.timeout,
            crate::fetch::fetch(self, source, endpoint.path(), &query),
        )
        .await
        .unwrap_or_else(|_| Err(format!("No answer within {} seconds.", self.timeout.as_secs())))
    }

    /// Every source's health over the window in `raw_query`: one row each —
    /// its `status` (`ok`, `degraded`, `failing`, `idle`, or `unknown` with
    /// the `reason` it has none) and its `health` as Lookout gave it — and
    /// how many are in each status.
    pub async fn health(&self, raw_query: &str) -> Value {
        let parts =
            self.ask_all(Endpoint::Health.path(), &Endpoint::Health.filter(raw_query)).await;
        let mut summary: BTreeMap<String, u64> = BTreeMap::new();
        let services: Vec<Value> = parts
            .iter()
            .map(|(source, outcome)| {
                let mut row = source_row(source, outcome);
                let health = outcome.as_ref().ok().map(data).filter(|h| h.is_object());
                let status = health
                    .and_then(|h| h.get("status"))
                    .and_then(Value::as_str)
                    .filter(|s| ["ok", "degraded", "failing", "idle"].contains(s))
                    .unwrap_or("unknown");
                if health.is_none() && row["reason"].is_null() {
                    row["reason"] = json!("Lookout's answer had no health.");
                }
                row["status"] = json!(status);
                row["health"] = health.cloned().unwrap_or(Value::Null);
                *summary.entry(status.to_string()).or_default() += 1;
                row
            })
            .collect();
        json!({ "services": services, "summary": summary })
    }

    /// Requests across every source: each source's grouped rows (by route,
    /// method and status class, as asked), each naming its service, busiest
    /// first; and each source's own totals.
    pub async fn requests(&self, raw_query: &str) -> Value {
        self.merged(Endpoint::Requests, raw_query, "rows", "count", "totals").await
    }

    /// Real users' page metrics across every source, most viewed first.
    pub async fn rum_vitals(&self, raw_query: &str) -> Value {
        self.merged(Endpoint::RumVitals, raw_query, "rows", "views", "totals").await
    }

    /// Real users' script errors across every source, most frequent first.
    pub async fn rum_errors(&self, raw_query: &str) -> Value {
        self.merged(Endpoint::RumErrors, raw_query, "rows", "count", "").await
    }

    /// Exceptions across every source, each naming its service, most
    /// frequent first.
    pub async fn cases(&self, raw_query: &str) -> Value {
        self.merged(Endpoint::Cases, raw_query, "", "occurrences", "").await
    }

    /// Every job queue across every source: each source's rows, each naming
    /// its service, deepest backlog first (then busiest); each source's own
    /// totals; and the fleet's — the backlog, attempts, failures and jobs
    /// running against slots added up, and how many queues are in each
    /// status.
    ///
    /// A queue's depth and concurrency come from its workers' samples, so a
    /// source whose workers do not sample has its queues with throughput and
    /// waits only (`depth` and `concurrency` null).
    pub async fn queues(&self, raw_query: &str) -> Value {
        let parts =
            self.ask_all(Endpoint::Queues.path(), &Endpoint::Queues.filter(raw_query)).await;
        let limit = raw_query
            .split('&')
            .find_map(|p| p.strip_prefix("limit=")?.parse::<usize>().ok())
            .unwrap_or(200)
            .clamp(1, 500);
        let mut rows: Vec<Value> = Vec::new();
        let mut totals = Map::new();
        let mut fleet = QueueTotals::default();
        for (source, outcome) in &parts {
            let Ok(body) = outcome else { continue };
            if let Some(t) = data(body).get("totals") {
                totals.insert(source.id.clone(), t.clone());
                fleet.add(t);
            }
            for row in data(body).get("rows").and_then(Value::as_array).into_iter().flatten() {
                let mut row = row.clone();
                if let Some(object) = row.as_object_mut() {
                    object.insert("service".into(), json!(source.id));
                    object.insert("service_name".into(), json!(source.name));
                }
                rows.push(row);
            }
        }
        let measure = |row: &Value, at: &str| row.pointer(at).and_then(Value::as_f64);
        rows.sort_by(|a, b| {
            let key = |row: &Value| {
                (
                    measure(row, "/depth/current").unwrap_or(-1.0),
                    measure(row, "/throughput/runs").unwrap_or(0.0),
                )
            };
            key(b).partial_cmp(&key(a)).unwrap_or(std::cmp::Ordering::Equal)
        });
        rows.truncate(limit);
        json!({
            "services": parts.iter().map(|(s, o)| source_row(s, o)).collect::<Vec<_>>(),
            "rows": rows,
            "totals": totals,
            "fleet": fleet.json(),
        })
    }

    async fn merged(
        &self,
        endpoint: Endpoint,
        raw_query: &str,
        rows_at: &str,
        order: &str,
        totals_at: &str,
    ) -> Value {
        let parts = self.ask_all(endpoint.path(), &endpoint.filter(raw_query)).await;
        let limit = raw_query
            .split('&')
            .find_map(|p| p.strip_prefix("limit=")?.parse::<usize>().ok())
            .unwrap_or(200)
            .clamp(1, 500);
        let mut totals = Map::new();
        if !totals_at.is_empty() {
            for (source, outcome) in &parts {
                if let Some(t) = outcome.as_ref().ok().and_then(|b| data(b).get(totals_at)) {
                    totals.insert(source.id.clone(), t.clone());
                }
            }
        }
        json!({
            "services": parts.iter().map(|(s, o)| source_row(s, o)).collect::<Vec<_>>(),
            "rows": merged_rows(&parts, rows_at, order, limit),
            "totals": totals,
        })
    }

    /// The map of who calls whom over the window in `raw_query` (`since`,
    /// `until`): every source a node with its health; every host a source
    /// calls that is not a source, an external node; and every caller and
    /// callee an edge with its calls, calls a minute, server errors and
    /// failures (`errors`), client errors, mean latency and a p95.
    ///
    /// Built from each Lookout's `call_facet` rollup — every outbound call
    /// counted by host and status class, kept or not — so the rates are the
    /// real ones. A host is a source when it is the host of the source's URL
    /// or one of its [`hosts`](Source::hosts).
    pub async fn map(&self, raw_query: &str) -> Value {
        let calls_query = call_facet_query(raw_query, chrono::Utc::now());
        let (calls, health) = futures_util::future::join(
            self.ask_all("aggregates/call_facet", &calls_query),
            self.health(raw_query),
        )
        .await;
        let minutes = health["services"]
            .as_array()
            .into_iter()
            .flatten()
            .find_map(|s| window_minutes(s.pointer("/health/window")?))
            .unwrap_or(60.0)
            .max(1.0);

        let owner: BTreeMap<String, String> = self
            .sources
            .iter()
            .flat_map(|s| s.all_hosts().into_iter().map(move |h| (h, s.id.clone())))
            .collect();

        #[derive(Default)]
        struct Edge {
            calls: u64,
            errors: u64,
            client_errors: u64,
            sum_us: f64,
            p95_us: u64,
        }
        let mut edges: BTreeMap<(String, String), Edge> = BTreeMap::new();
        let mut externals: BTreeMap<String, u64> = BTreeMap::new();
        for (source, outcome) in &calls {
            let Ok(body) = outcome else { continue };
            for row in data(body).as_array().into_iter().flatten() {
                let Some((class, host)) =
                    row.get("key").and_then(Value::as_str).and_then(|k| k.split_once(' '))
                else {
                    continue;
                };
                let host = host.to_ascii_lowercase();
                let target = match owner.get(&host) {
                    Some(id) => id.clone(),
                    None => {
                        *externals.entry(host.clone()).or_default() += 1;
                        host
                    }
                };
                let count = row.get("count").and_then(Value::as_u64).unwrap_or(0);
                let edge = edges.entry((source.id.clone(), target)).or_default();
                edge.calls += count;
                match class {
                    "5xx" | "failed" => edge.errors += count,
                    "4xx" => edge.client_errors += count,
                    _ => {}
                }
                let average = row.get("average_us").and_then(Value::as_f64).unwrap_or(0.0);
                edge.sum_us += average * count as f64;
                edge.p95_us =
                    edge.p95_us.max(row.get("p95_us").and_then(Value::as_u64).unwrap_or(0));
            }
        }

        let mut nodes: Vec<Value> = health["services"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|s| {
                json!({
                    "id": s["id"], "name": s["name"], "kind": "service", "status": s["status"],
                    "reason": s["reason"],
                    "requests_per_minute": s.pointer("/health/requests/per_minute"),
                    "error_rate": s.pointer("/health/requests/error_rate"),
                    "p95_us": s.pointer("/health/requests/p95_us"),
                })
            })
            .collect();
        nodes.extend(
            externals.keys().map(
                |host| json!({ "id": host, "name": host, "kind": "external", "status": null }),
            ),
        );
        let edges: Vec<Value> = edges
            .into_iter()
            .map(|((from, to), e)| {
                json!({
                    "from": from, "to": to, "calls": e.calls,
                    "per_minute": (e.calls as f64 / minutes * 100.0).round() / 100.0,
                    "errors": e.errors, "client_errors": e.client_errors,
                    "error_rate": if e.calls == 0 { 0.0 } else { (e.errors as f64 / e.calls as f64 * 10_000.0).round() / 10_000.0 },
                    "average_us": if e.calls == 0 { 0 } else { (e.sum_us / e.calls as f64).round() as u64 },
                    "p95_us": e.p95_us,
                })
            })
            .collect();
        json!({
            "services": calls.iter().map(|(s, o)| source_row(s, o)).collect::<Vec<_>>(),
            "nodes": nodes,
            "edges": edges,
        })
    }
}

/// Every source's queue totals added up.
#[derive(Debug, Default)]
struct QueueTotals {
    queues: u64,
    depth: u64,
    runs: u64,
    failures: u64,
    pushed: u64,
    per_minute: f64,
    concurrency: f64,
    capacity: f64,
    statuses: BTreeMap<String, u64>,
}

impl QueueTotals {
    fn add(&mut self, totals: &Value) {
        let n = |key: &str| totals.get(key).and_then(Value::as_u64).unwrap_or(0);
        let f = |key: &str| totals.get(key).and_then(Value::as_f64).unwrap_or(0.0);
        self.queues += n("queues");
        self.depth += n("depth");
        self.runs += n("runs");
        self.failures += n("failures");
        self.pushed += n("pushed");
        self.per_minute += f("per_minute");
        self.concurrency += f("concurrency");
        self.capacity += f("capacity");
        for (status, count) in
            totals.get("statuses").and_then(Value::as_object).into_iter().flatten()
        {
            *self.statuses.entry(status.clone()).or_default() += count.as_u64().unwrap_or(0);
        }
    }

    fn json(&self) -> Value {
        let two = |x: f64| (x * 100.0).round() / 100.0;
        json!({
            "queues": self.queues,
            "depth": self.depth,
            "runs": self.runs,
            "failures": self.failures,
            "pushed": self.pushed,
            "per_minute": two(self.per_minute),
            "concurrency": two(self.concurrency),
            "capacity": two(self.capacity),
            "utilisation": if self.capacity > 0.0 { Some((self.concurrency / self.capacity * 10_000.0).round() / 10_000.0) } else { None },
            "statuses": self.statuses,
        })
    }
}

/// The aggregates endpoint takes exact times: `since=1h` (or `15m`, `7d`, as
/// the health endpoint takes them) becomes an RFC 3339 time before `now`; an
/// RFC 3339 `since` or `until` is passed on. Every host, busiest first.
fn call_facet_query(raw_query: &str, now: chrono::DateTime<chrono::Utc>) -> String {
    let mut parts = Vec::new();
    for pair in raw_query.split('&') {
        let Some((key, value)) = pair.split_once('=') else { continue };
        if !matches!(key, "since" | "until") || value.is_empty() || value.len() > 40 {
            continue;
        }
        let value = value.replace("%3A", ":").replace("%2B", "+");
        let at = match relative(&value) {
            Some(span) => now - span,
            None => match chrono::DateTime::parse_from_rfc3339(&value) {
                Ok(at) => at.with_timezone(&chrono::Utc),
                Err(_) => continue,
            },
        };
        if !parts.iter().any(|p: &String| p.starts_with(&format!("{key}="))) {
            parts.push(format!("{key}={}", at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)));
        }
    }
    if !parts.iter().any(|p| p.starts_with("since=")) {
        let hour = now - chrono::Duration::hours(1);
        parts.insert(
            0,
            format!("since={}", hour.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
        );
    }
    parts.push("order=count".into());
    parts.push("limit=100".into());
    parts.join("&")
}

/// `15m`, `6h`, `7d` as a duration.
fn relative(value: &str) -> Option<chrono::Duration> {
    let (number, unit) = value.split_at(value.len().checked_sub(1)?);
    let n: i64 = number.parse().ok().filter(|n| *n > 0 && *n <= 10_000)?;
    match unit {
        "m" => Some(chrono::Duration::minutes(n)),
        "h" => Some(chrono::Duration::hours(n)),
        "d" => Some(chrono::Duration::days(n)),
        _ => None,
    }
}

/// How long a Lookout `window` (`{since, until}`, RFC 3339) is, in minutes.
fn window_minutes(window: &Value) -> Option<f64> {
    let at = |key: &str| chrono::DateTime::parse_from_rfc3339(window.get(key)?.as_str()?).ok();
    let span = at("until")? - at("since")?;
    Some(span.num_seconds() as f64 / 60.0).filter(|m| *m > 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_calls_query_is_in_exact_times() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-11-27T10:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(
            call_facet_query("since=15m&q=x", now),
            "since=2026-11-27T09:45:00Z&order=count&limit=100"
        );
        assert_eq!(
            call_facet_query("since=2026-11-27T08%3A00%3A00Z&until=7d", now),
            "since=2026-11-27T08:00:00Z&until=2026-11-20T10:00:00Z&order=count&limit=100"
        );
        assert_eq!(call_facet_query("", now), "since=2026-11-27T09:00:00Z&order=count&limit=100");
        assert_eq!(
            call_facet_query("since=soon", now),
            "since=2026-11-27T09:00:00Z&order=count&limit=100"
        );
    }

    #[test]
    fn a_window_is_measured_in_minutes() {
        let window =
            json!({ "since": "2026-11-27T09:00:00+00:00", "until": "2026-11-27T10:30:00+00:00" });
        assert_eq!(window_minutes(&window), Some(90.0));
        assert_eq!(window_minutes(&json!({})), None);
    }

    #[test]
    fn only_an_endpoints_own_parameters_are_forwarded() {
        assert_eq!(Endpoint::Health.filter("since=1h&q=x&since=7d&apdex_t_ms="), "since=1h");
        assert_eq!(Endpoint::Cases.filter("status=open&since=1h&limit=5"), "status=open&limit=5");
        assert_eq!(Endpoint::parse("rum/vitals"), Some(Endpoint::RumVitals));
        assert_eq!(Endpoint::parse("batches"), None);
        assert_eq!(Endpoint::parse("queues"), Some(Endpoint::Queues));
        assert_eq!(
            Endpoint::Queues.filter("since=1h&queue=mail&apdex_t_ms=9&series=1"),
            "since=1h&queue=mail&series=1"
        );
        let long = "x".repeat(201);
        assert_eq!(Endpoint::Requests.filter(&format!("q={long}")), "");
    }

    #[test]
    fn rows_from_every_source_name_their_service_and_sort_together() {
        let parts: Vec<Part> = vec![
            (
                Source::new("a", "http://a.test"),
                Ok(json!({ "data": { "rows": [{ "route": "x", "count": 5 }] } })),
            ),
            (
                Source::new("b", "http://b.test"),
                Ok(json!({ "data": { "rows": [{ "route": "y", "count": 9 }, { "route": "z" }] } })),
            ),
            (Source::new("c", "http://c.test"), Err("down".into())),
        ];
        let rows = merged_rows(&parts, "rows", "count", 10);
        let order: Vec<(&str, &str)> = rows
            .iter()
            .map(|r| (r["service"].as_str().unwrap(), r["route"].as_str().unwrap()))
            .collect();
        assert_eq!(order, [("b", "y"), ("a", "x"), ("b", "z")]);
        assert_eq!(merged_rows(&parts, "rows", "count", 1).len(), 1);
    }
}
