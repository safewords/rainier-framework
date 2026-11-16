//! The JSON API under `{path}/api`.
//!
//! Lists answer `{"data": [...], "next": {"before": "<RFC 3339>"} | null}`;
//! `next` is the cursor for the following page, present when the page was
//! full. Single objects answer `{"data": {...}}`.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use rainier_http::{Response, StatusCode};
use rainier_routing::Req;
use rainier_support::{Error, Result};
use serde_json::{json, Map, Value};

use super::params::{self, Params};
use super::{Shared, CLEAR_HEADER};
use std::collections::BTreeMap;

use crate::aggregate::{self, AGGREGATED};
use crate::cases::{self, CaseAction, CaseSeverity};
use crate::config::StoreKind;
use crate::entry::{family_id, Aggregate, Entry, EntryType};
use crate::trends;

fn page<T: serde::Serialize>(
    items: &[T],
    limit: usize,
    started: impl Fn(&T) -> DateTime<Utc>,
) -> Value {
    let next = match items.last() {
        Some(last) if items.len() >= limit => json!({ "before": started(last).to_rfc3339() }),
        _ => Value::Null,
    };
    json!({ "data": items, "next": next })
}

/// `GET /api/batches`.
pub(crate) async fn batches(state: Arc<Shared>, request: Req) -> Result<Response> {
    let mut params = Params::new(request.query());
    let query = params::batch_query(&mut params);
    params.finish()?;
    let limit = query.limit.unwrap_or(params::DEFAULT_LIMIT);
    let found = state.store.batches(query).await?;
    Ok(Response::json(&page(&found, limit, |b| b.started_at)))
}

/// `GET /api/batches/{id}`.
pub(crate) async fn batch(state: Arc<Shared>, request: Req) -> Result<Response> {
    let id = request.route_param("id").unwrap_or_default();
    match state.store.batch(id).await? {
        Some(batch) => Ok(Response::json(&json!({ "data": batch }))),
        None => Err(Error::not_found("No such batch.")),
    }
}

/// `GET /api/entries/{type}`: newest first; `family` (or `key`) narrows to
/// one family, `q` to entries whose summary or family key contains the text.
pub(crate) async fn entries(state: Arc<Shared>, request: Req) -> Result<Response> {
    let mut params = Params::new(request.query());
    let entry_type = params.parse_type("type", request.route_param("type").unwrap_or_default());
    let mut query = params::entries_query(&mut params);
    params.finish()?;
    let Some(entry_type) = entry_type else {
        return Err(Error::internal("validated entry type missing"));
    };
    if let Some(id) = query.family.clone() {
        query.key = Some(require_family(&state, entry_type, &id).await?);
    }
    let limit = query.limit;
    let Some(search) = query.search.map(|q| q.to_lowercase()) else {
        let found = match &query.key {
            Some(key) => state.store.family_entries(entry_type, key, query.before, limit).await?,
            None => state.store.entries(entry_type, query.before, limit).await?,
        };
        return Ok(Response::json(&page(&found, limit, |e| e.occurred_at)));
    };

    // A text search reads at most this many pages, so it is bounded however
    // rare the text is; `next` carries on from where it stopped.
    const PAGES: usize = 5;
    let matches = |e: &Entry| {
        e.summary.to_lowercase().contains(&search)
            || e.family_key().to_lowercase().contains(&search)
    };
    let mut found = Vec::new();
    let mut cursor = query.before;
    let mut exhausted = false;
    for _ in 0..PAGES {
        let page = match &query.key {
            Some(key) => {
                state.store.family_entries(entry_type, key, cursor, params::MAX_LIMIT).await?
            }
            None => state.store.entries(entry_type, cursor, params::MAX_LIMIT).await?,
        };
        exhausted = page.len() < params::MAX_LIMIT;
        cursor = page.last().map(|e| e.occurred_at);
        found.extend(page.into_iter().filter(|e| matches(e)));
        if exhausted || found.len() >= limit {
            break;
        }
    }
    found.truncate(limit);
    let next = match (exhausted, found.len() >= limit, found.last(), cursor) {
        (_, true, Some(last), _) => json!({ "before": last.occurred_at.to_rfc3339() }),
        (false, false, _, Some(cursor)) => json!({ "before": cursor.to_rfc3339() }),
        _ => Value::Null,
    };
    Ok(Response::json(&json!({ "data": found, "next": next })))
}

/// One aggregate row as the API shows it: the stored fields plus the average
/// and the p50/p90/p95/p99 read off its histogram (`null` without one).
fn aggregate_json(row: &Aggregate) -> Value {
    let mut value = serde_json::to_value(row).unwrap_or(Value::Null);
    if let Value::Object(map) = &mut value {
        let average = row.sum_us.checked_div(row.count).unwrap_or(0);
        map.insert("average_us".into(), json!(average));
        map.insert("family_id".into(), json!(family_id(row.entry_type, &row.key)));
        for (name, p) in [("p50_us", 50.0), ("p90_us", 90.0), ("p95_us", 95.0), ("p99_us", 99.0)] {
            map.insert(name.into(), json!(aggregate::percentile(&row.histogram, p, row.max_us)));
        }
    }
    value
}

/// `GET /api/families/{type}/{id}`: the key a [`family_id`] names.
pub(crate) async fn family(state: Arc<Shared>, request: Req) -> Result<Response> {
    let mut params = Params::new(request.query());
    let entry_type = params.parse_type("type", request.route_param("type").unwrap_or_default());
    params.finish()?;
    let Some(entry_type) = entry_type else {
        return Err(Error::internal("validated entry type missing"));
    };
    let id = request.route_param("id").unwrap_or_default().to_ascii_lowercase();
    let key = require_family(&state, entry_type, &id).await?;
    Ok(Response::json(&json!({ "data": { "id": id, "key": key } })))
}

/// The key a [`family_id`] names, or 404.
///
/// From the aggregates of the retention window first — every family with a
/// duration is in them — then from the newest entries of the type, for the
/// derived ones (`slow`, `n_plus_one`) that are not aggregated.
async fn require_family(state: &Shared, entry_type: EntryType, id: &str) -> Result<String> {
    let id = id.to_ascii_lowercase();
    if AGGREGATED.contains(&entry_type) {
        let days = i64::try_from(state.config.retention.aggregates_days).unwrap_or(i64::MAX);
        let since = Utc::now() - chrono::Duration::days(days.min(3650));
        let rows = state.store.aggregate_rows(entry_type, 3600, since).await?;
        if let Some(row) = rows.iter().find(|r| family_id(entry_type, &r.key) == id) {
            return Ok(row.key.clone());
        }
    }
    let recent = state.store.entries(entry_type, None, 1000).await?;
    if let Some(entry) = recent.iter().find(|e| family_id(entry_type, e.family_key()) == id) {
        return Ok(entry.family_key().to_string());
    }
    Err(Error::not_found("No such family."))
}

/// `GET /api/aggregates/{type}`.
pub(crate) async fn aggregates(state: Arc<Shared>, request: Req) -> Result<Response> {
    let mut params = Params::new(request.query());
    let entry_type = params.parse_type("type", request.route_param("type").unwrap_or_default());
    let query = params::aggregate_query(&mut params, entry_type, Utc::now());
    params.finish()?;
    let Some(query) = query else {
        return Err(Error::internal("validated aggregate query missing"));
    };
    let rows = state.store.aggregates(query).await?;
    let data: Vec<Value> = rows.iter().map(aggregate_json).collect();
    Ok(Response::json(&json!({ "data": data })))
}

/// `GET /api/series/{type}`: one zero-filled point per bucket from `since`
/// to now, the projection past it, and with `keys=N` a series for each of the
/// N busiest keys.
pub(crate) async fn series(state: Arc<Shared>, request: Req) -> Result<Response> {
    let mut params = Params::new(request.query());
    let entry_type = params.parse_type("type", request.route_param("type").unwrap_or_default());
    let now = Utc::now();
    let forecast_default =
        if state.config.insights.enabled { state.config.insights.forecast_buckets } else { 0 };
    let query = params::series_query(&mut params, entry_type, now, forecast_default);
    params.finish()?;
    let Some(query) = query else {
        return Err(Error::internal("validated series query missing"));
    };
    let mut rows =
        state.store.aggregate_rows(query.entry_type, query.period_s, query.since).await?;
    let key = if let Some(id) = &query.family {
        Some(require_family(&state, query.entry_type, id).await?)
    } else {
        query.key.clone()
    };
    if let Some(key) = &key {
        rows.retain(|r| &r.key == key);
    }
    // A custom window ends at `until`. Anything later than now is not a
    // bucket yet; the forecast, when asked for, is what extends past the end.
    let end = query.until.unwrap_or(now).min(now);
    let last = aggregate::bucket_start(end, query.period_s);
    rows.retain(|r| r.bucket_start <= last);
    let points = trends::series(&rows, query.period_s, query.since, end);
    let forecast = trends::forecast(&points, query.period_s, query.forecast);

    // Over the whole range: every bucket's histogram added, so p50 to p99
    // cover every occurrence in it — not the stored ones, which in
    // production are mostly the slow.
    let first = aggregate::bucket_start(query.since, query.period_s);
    let in_range: Vec<&Aggregate> = rows
        .iter()
        .filter(|r| {
            r.period_s == query.period_s && r.bucket_start >= first && r.bucket_start <= last
        })
        .collect();
    let mut histogram = aggregate::Histogram::new();
    for row in &in_range {
        aggregate::add_histogram(&mut histogram, &row.histogram);
    }
    let worst = in_range.iter().map(|r| r.max_us).max().unwrap_or(0);
    let mut percentiles = Map::new();
    for (name, p) in [("p50_us", 50.0), ("p90_us", 90.0), ("p95_us", 95.0), ("p99_us", 99.0)] {
        percentiles.insert(name.into(), json!(aggregate::percentile(&histogram, p, worst)));
    }

    let mut keys = Map::new();
    if query.keys > 0 {
        let mut totals: BTreeMap<&str, u64> = BTreeMap::new();
        for row in &rows {
            *totals.entry(row.key.as_str()).or_default() += row.count;
        }
        let mut busiest: Vec<(&str, u64)> = totals.into_iter().collect();
        busiest.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        for (key, _) in busiest.into_iter().take(query.keys) {
            let own: Vec<Aggregate> = rows.iter().filter(|r| r.key == key).cloned().collect();
            keys.insert(
                key.to_string(),
                json!(trends::series(&own, query.period_s, query.since, end)),
            );
        }
    }
    Ok(Response::json(&json!({
        "data": points,
        "forecast": forecast,
        "period": query.period_s,
        "percentiles": percentiles,
        // The durations themselves, every occurrence in the range, as
        // `{ slot: count }` — what an issue page draws its distribution from.
        "histogram": histogram,
        "keys": keys,
    })))
}

/// `GET /api/insights`: what changed between the last window and the one
/// before, by the configured thresholds or the ones in the query.
pub(crate) async fn insights(state: Arc<Shared>, request: Req) -> Result<Response> {
    let mut params = Params::new(request.query());
    let (config, limit) = params::insights_query(&mut params, state.config.insights);
    params.finish()?;
    if !state.config.insights.enabled {
        return Ok(Response::json(&json!({ "data": [], "config": config, "enabled": false })));
    }
    let now = Utc::now();
    let period = config.period_s();
    let window = chrono::Duration::minutes(config.window_minutes as i64);
    let since = now - window - window - chrono::Duration::seconds(i64::from(period));
    // One read per type, all at once.
    let rows: Vec<Aggregate> = futures_util::future::try_join_all(
        AGGREGATED.iter().map(|&entry_type| state.store.aggregate_rows(entry_type, period, since)),
    )
    .await?
    .into_iter()
    .flatten()
    .collect();
    let found = trends::insights(&rows, &config, now, limit);
    Ok(Response::json(&json!({ "data": found, "config": config, "enabled": true })))
}

/// `GET /api/cases`.
pub(crate) async fn cases(state: Arc<Shared>, request: Req) -> Result<Response> {
    let mut params = Params::new(request.query());
    let query = params::case_query(&mut params);
    params.finish()?;
    let found = state.store.cases(query).await?;
    Ok(Response::json(&json!({ "data": found })))
}

/// `GET /api/cases/{id}`.
pub(crate) async fn case(state: Arc<Shared>, request: Req) -> Result<Response> {
    let id = request.route_param("id").unwrap_or_default();
    match state.store.case(id).await? {
        Some(case) => Ok(Response::json(&json!({ "data": case }))),
        None => Err(Error::not_found("No such case.")),
    }
}

#[derive(serde::Deserialize)]
struct CaseActionBody {
    action: String,
    #[serde(default)]
    severity: Option<String>,
    #[serde(default)]
    note: Option<String>,
}

/// `POST /api/cases/{id}`: resolve, reopen, ignore, or set the severity.
/// Needs the same header as clearing, so a form on another site cannot.
pub(crate) async fn act_on_case(state: Arc<Shared>, request: Req) -> Result<Response> {
    if request.header(CLEAR_HEADER.0) != Some(CLEAR_HEADER.1) {
        return Err(Error::unauthorized(format!(
            "Changing a case needs the `{}: {}` header.",
            CLEAR_HEADER.0, CLEAR_HEADER.1
        )));
    }
    let body: CaseActionBody = request.json()?;
    let mut errors = Map::new();
    let action = match body.action.as_str() {
        "resolve" => Some(CaseAction::Resolve),
        "reopen" => Some(CaseAction::Reopen),
        "ignore" => Some(CaseAction::Ignore),
        "severity" => match body.severity.as_deref().and_then(CaseSeverity::parse) {
            Some(severity) => Some(CaseAction::Severity(severity)),
            None => {
                errors.insert(
                    "severity".into(),
                    json!(["The severity must be one of: low, medium, high, critical."]),
                );
                None
            }
        },
        _ => {
            errors.insert(
                "action".into(),
                json!(["The action must be one of: resolve, reopen, ignore, severity."]),
            );
            None
        }
    };
    let note = body.note.map(|n| n.trim().to_string()).filter(|n| !n.is_empty());
    if note.as_ref().is_some_and(|n| n.chars().count() > 2000) {
        errors.insert("note".into(), json!(["The note may not be longer than 2000 characters."]));
    }
    let Some(action) = action.filter(|_| errors.is_empty()) else {
        return Err(Error::validation(Value::Object(errors)));
    };
    let id = request.route_param("id").unwrap_or_default();
    let Some(case) = state.store.case(id).await? else {
        return Err(Error::not_found("No such case."));
    };
    let case = cases::act(case, &action, note, Utc::now());
    state.store.save_case(case.clone()).await?;
    Ok(Response::json(&json!({ "data": case })))
}

/// `GET /api/status`.
pub(crate) async fn status(state: Arc<Shared>, _request: Req) -> Result<Response> {
    Ok(Response::json(&json!({ "data": status_json(&state) })))
}

/// What `lookout:status` and `GET /api/status` both say. Nothing secret:
/// no connection names, no redaction lists, no keep rule.
pub(crate) fn status_json(state: &Shared) -> Value {
    status_value(state.store.name(), &state.config, Some(&state.environment))
}

pub(crate) fn status_value(
    store_name: &str,
    config: &crate::config::LookoutConfig,
    environment: Option<&str>,
) -> Value {
    let store_kind = match config.store {
        StoreKind::Memory => "memory",
        StoreKind::Database { .. } => "database",
        StoreKind::Cache { .. } => "cache",
    };
    let mut watchers = Map::new();
    for &t in EntryType::ALL {
        let w = config.watcher(t);
        watchers.insert(
            t.as_str().to_string(),
            json!({ "enabled": w.enabled, "sample_rate": w.sample_rate, "slow_ms": w.slow_ms }),
        );
    }
    json!({
        "enabled": config.enabled,
        "store": store_name,
        "store_kind": store_kind,
        "environment": environment,
        "sample_rates": {
            "request": config.request_sample_rate,
            "job": config.job_sample_rate,
            "command": config.command_sample_rate,
        },
        "watchers": watchers,
        "retention": {
            "batches_hours": config.retention.batches_hours,
            "aggregates_days": config.retention.aggregates_days,
        },
        "n_plus_one": config.n_plus_one,
        "insights": config.insights,
        "log_level": config.log_level,
    })
}

/// `DELETE /api/batches`.
pub(crate) async fn clear(state: Arc<Shared>, request: Req) -> Result<Response> {
    if request.header(CLEAR_HEADER.0) != Some(CLEAR_HEADER.1) {
        return Err(Error::unauthorized(format!(
            "Clearing Lookout needs the `{}: {}` header.",
            CLEAR_HEADER.0, CLEAR_HEADER.1
        )));
    }
    state.store.clear().await?;
    Ok(Response::new(StatusCode::NO_CONTENT))
}
