//! The service-health API: `/api/health`, `/api/requests`, `/api/queues`,
//! `/api/rum/vitals` and `/api/rum/errors` — one service's health, its
//! requests grouped and filtered, its queues, and what its real users saw. Every answer is read from the
//! rollups ([`facets`](crate::facets), [`rum_rollup`](crate::rum_rollup)),
//! which count every request and every accepted page report, kept or not.

use std::sync::Arc;

use chrono::Utc;
use rainier_http::Response;
use rainier_routing::Req;
use rainier_support::Result;
use serde_json::{json, Value};

use super::params::{self, Params};
use super::Shared;
use crate::cases::{CaseQuery, CaseStatus};
use crate::entry::EntryType;
use crate::facets::{self, Grouping, RequestFilter, RequestOrder, STATUS_CLASSES};
use crate::rum_rollup::{self, RumFilter, VitalsOrder, DIMENSIONS};

/// The Apdex target when the query does not name one, in milliseconds.
pub(crate) const DEFAULT_APDEX_T_MS: u64 = 300;
/// The most rows `/api/requests` returns.
const MAX_REQUEST_ROWS: usize = 500;
/// The most rows `/api/rum/*` returns.
const MAX_RUM_ROWS: usize = 200;
/// How many routes each of the health answer's three lists holds.
const ROUTES: usize = 5;
/// The most cases read per status to count them.
const CASES_READ: usize = 200;

/// `GET /api/health`.
pub(crate) async fn health(state: Arc<Shared>, request: Req) -> Result<Response> {
    Ok(Response::json(&health_value(&state, request.query()).await?))
}

/// One service's health over the window. See `docs/lookout.md`, "Service
/// health".
pub(crate) async fn health_value(state: &Shared, query: &Value) -> Result<Value> {
    let mut params = Params::new(query);
    let now = Utc::now();
    let window = params::window(&mut params, now);
    let apdex_t_ms = params.integer("apdex_t_ms", 1, 600_000).unwrap_or(DEFAULT_APDEX_T_MS);
    params.finish()?;
    let (since, until, period) = (window.since, window.until, window.period_s);

    let store = &state.store;
    let (requests, jobs, exceptions, queue_rows) = futures_util::future::try_join4(
        store.aggregate_rows(EntryType::RequestFacet, period, since),
        store.aggregate_rows(EntryType::JobFacet, period, since),
        store.aggregate_rows(EntryType::Exception, period, since),
        store.aggregate_rows(EntryType::QueueFacet, period, since),
    )
    .await?;
    let (_, queues) = crate::queues::summarize(
        &queue_rows,
        period,
        since,
        until,
        window.minutes(),
        &crate::queues::QueueQuery { limit: 1, ..Default::default() },
    );

    let selected = facets::select(&requests, period, since, until, &RequestFilter::default());
    let by_route = |order| {
        facets::group(&selected, Grouping { route: true, ..Grouping::default() }, order, usize::MAX)
            .0
    };
    let (_, totals) = facets::group(&selected, Grouping::default(), RequestOrder::Count, 0);
    let top_routes: Vec<Value> = by_route(RequestOrder::Count).into_iter().take(ROUTES).collect();
    let slowest_routes: Vec<Value> = by_route(RequestOrder::P95).into_iter().take(ROUTES).collect();
    let failing_routes: Vec<Value> = by_route(RequestOrder::Errors)
        .into_iter()
        .filter(|r| r["errors_5xx"].as_u64().unwrap_or(0) > 0)
        .take(ROUTES)
        .collect();

    let (job_runs, job_failures) = facets::job_counts(&jobs, period, since, until);
    let first = crate::aggregate::bucket_start(since, period);
    let last = crate::aggregate::bucket_start(until, period);
    let exception_count: u64 = exceptions
        .iter()
        .filter(|r| r.period_s == period && r.bucket_start >= first && r.bucket_start <= last)
        .map(|r| r.count)
        .sum();

    let mut open = 0usize;
    let mut new_in_window = 0usize;
    for status in [CaseStatus::Open, CaseStatus::Resolved, CaseStatus::Ignored] {
        let found = store
            .cases(CaseQuery {
                status: Some(status),
                limit: Some(CASES_READ),
                ..CaseQuery::default()
            })
            .await?;
        if status == CaseStatus::Open {
            open = found.len();
        }
        new_in_window +=
            found.iter().filter(|c| c.first_seen >= since && c.first_seen <= until).count();
    }

    let apdex = crate::aggregate::apdex(&totals.histogram, apdex_t_ms.saturating_mul(1000));
    Ok(json!({ "data": {
        "window": window.json(),
        "requests": {
            "count": totals.count,
            "per_minute": facets::round(totals.count as f64 / window.minutes(), 2),
            "errors_5xx": totals.errors_5xx,
            "client_errors_4xx": totals.client_errors_4xx,
            "error_rate": facets::round(totals.error_rate(), 4),
            "p50_us": totals.percentile(50.0),
            "p95_us": totals.percentile(95.0),
            "p99_us": totals.percentile(99.0),
            "apdex": apdex.map(|a| facets::round(a, 3)),
            "apdex_t_ms": apdex_t_ms,
        },
        "jobs": { "count": job_runs, "failed": job_failures },
        "queues": queues,
        "exceptions": { "count": exception_count },
        "cases": { "open": open, "new_in_window": new_in_window },
        "status": facets::status(&totals, apdex_t_ms),
        "series": facets::series(&selected, period, since, until),
        "top_routes": top_routes,
        "slowest_routes": slowest_routes,
        "failing_routes": failing_routes,
    }}))
}

/// `GET /api/requests`.
pub(crate) async fn requests(state: Arc<Shared>, request: Req) -> Result<Response> {
    Ok(Response::json(&requests_value(&state, request.query()).await?))
}

/// Request aggregates, grouped and filtered.
pub(crate) async fn requests_value(state: &Shared, query: &Value) -> Result<Value> {
    let mut params = Params::new(query);
    let now = Utc::now();
    let window = params::window(&mut params, now);
    let group = params::names(&mut params, "group", &["route", "method", "status"])
        .unwrap_or_else(|| vec!["route".to_string()]);
    let grouping = Grouping {
        route: group.iter().any(|g| g == "route"),
        method: group.iter().any(|g| g == "method"),
        status: group.iter().any(|g| g == "status"),
    };
    let filter = RequestFilter {
        route: params.text("route"),
        method: params.text("method"),
        status: params::one_of(&mut params, "status", STATUS_CLASSES),
        search: params.text("q"),
    };
    let order =
        match params::one_of(&mut params, "order", &["count", "p95", "errors", "error_rate"])
            .as_deref()
        {
            Some("p95") => RequestOrder::P95,
            Some("errors") => RequestOrder::Errors,
            Some("error_rate") => RequestOrder::ErrorRate,
            _ => RequestOrder::Count,
        };
    let limit = params.limit(MAX_REQUEST_ROWS, 100);
    let with_series = params.boolean("series").unwrap_or(false);
    params.finish()?;

    let rows =
        state.store.aggregate_rows(EntryType::RequestFacet, window.period_s, window.since).await?;
    let selected = facets::select(&rows, window.period_s, window.since, window.until, &filter);
    let (rows, totals) = facets::group(&selected, grouping, order, limit);
    let mut data = json!({
        "window": window.json(),
        "rows": rows,
        "totals": totals.row_json(None, None, None),
    });
    if with_series {
        data["series"] =
            json!(facets::series(&selected, window.period_s, window.since, window.until));
    }
    Ok(json!({ "data": data }))
}

/// The most rows `/api/queues` returns.
const MAX_QUEUE_ROWS: usize = 200;

/// `GET /api/queues`.
pub(crate) async fn queues(state: Arc<Shared>, request: Req) -> Result<Response> {
    Ok(Response::json(&queues_value(&state, request.query()).await?))
}

/// Every queue over the window: its depth, concurrency, throughput, waits and
/// run times, and a status. See `docs/lookout.md`, "Queues".
pub(crate) async fn queues_value(state: &Shared, query: &Value) -> Result<Value> {
    let mut params = Params::new(query);
    let window = params::window(&mut params, Utc::now());
    let queue = params.text("queue");
    let order =
        params::one_of(&mut params, "order", &["depth", "runs", "wait", "failures", "queue"]);
    let limit = params.limit(MAX_QUEUE_ROWS, 50);
    let series = params.boolean("series").unwrap_or(false);
    params.finish()?;

    let rows =
        state.store.aggregate_rows(EntryType::QueueFacet, window.period_s, window.since).await?;
    let (rows, totals) = crate::queues::summarize(
        &rows,
        window.period_s,
        window.since,
        window.until,
        window.minutes(),
        &crate::queues::QueueQuery { queue, order, limit, series },
    );
    Ok(json!({ "data": {
        "window": window.json(),
        "rows": rows,
        "totals": totals,
    }}))
}

/// The RUM filters, shared by vitals and errors.
fn rum_filter(params: &mut Params<'_>) -> RumFilter {
    let mut filter = RumFilter::default();
    for (i, name) in DIMENSIONS.iter().enumerate() {
        filter.dims[i] = params.text(name);
    }
    filter
}

/// `GET /api/rum/vitals`.
pub(crate) async fn rum_vitals(state: Arc<Shared>, request: Req) -> Result<Response> {
    Ok(Response::json(&rum_vitals_value(&state, request.query()).await?))
}

/// Real-user page metrics, grouped by one dimension and filtered.
pub(crate) async fn rum_vitals_value(state: &Shared, query: &Value) -> Result<Value> {
    let mut params = Params::new(query);
    let window = params::window(&mut params, Utc::now());
    let group = params::one_of(&mut params, "group", &DIMENSIONS)
        .and_then(|g| DIMENSIONS.iter().position(|d| *d == g))
        .unwrap_or(0);
    let filter = rum_filter(&mut params);
    let order =
        match params::one_of(&mut params, "order", &["views", "lcp", "inp", "cls", "errors"])
            .as_deref()
        {
            Some("lcp") => VitalsOrder::Lcp,
            Some("inp") => VitalsOrder::Inp,
            Some("cls") => VitalsOrder::Cls,
            Some("errors") => VitalsOrder::Errors,
            _ => VitalsOrder::Views,
        };
    let limit = params.limit(MAX_RUM_ROWS, 50);
    params.finish()?;

    let rows =
        state.store.aggregate_rows(EntryType::RumView, window.period_s, window.since).await?;
    let (rows, totals) = rum_rollup::vitals(
        &rows,
        window.period_s,
        window.since,
        window.until,
        &filter,
        group,
        order,
        limit,
    );
    Ok(json!({ "data": {
        "window": window.json(),
        "group": DIMENSIONS[group],
        "rows": rows,
        "totals": totals,
        "enabled": state.config.rum.enabled,
    }}))
}

/// `GET /api/rum/errors`.
pub(crate) async fn rum_errors(state: Arc<Shared>, request: Req) -> Result<Response> {
    Ok(Response::json(&rum_errors_value(&state, request.query()).await?))
}

/// Script errors from browsers, grouped by message, file and line.
pub(crate) async fn rum_errors_value(state: &Shared, query: &Value) -> Result<Value> {
    let mut params = Params::new(query);
    let window = params::window(&mut params, Utc::now());
    let filter = rum_filter(&mut params);
    let limit = params.limit(MAX_RUM_ROWS, 50);
    params.finish()?;

    let rows =
        state.store.aggregate_rows(EntryType::RumError, window.period_s, window.since).await?;
    let rows =
        rum_rollup::errors(&rows, window.period_s, window.since, window.until, &filter, limit);
    Ok(json!({ "data": {
        "window": window.json(),
        "rows": rows,
        "enabled": state.config.rum.enabled,
    }}))
}
