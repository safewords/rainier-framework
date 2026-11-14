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
use crate::config::StoreKind;
use crate::entry::{Aggregate, EntryType};

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

/// `GET /api/entries/{type}`.
pub(crate) async fn entries(state: Arc<Shared>, request: Req) -> Result<Response> {
    let mut params = Params::new(request.query());
    let entry_type = params.parse_type("type", request.route_param("type").unwrap_or_default());
    let (before, limit) = params::entries_query(&mut params);
    params.finish()?;
    let Some(entry_type) = entry_type else {
        return Err(Error::internal("validated entry type missing"));
    };
    let found = state.store.entries(entry_type, before, limit).await?;
    Ok(Response::json(&page(&found, limit, |e| e.occurred_at)))
}

/// One aggregate row as the API shows it: the stored fields plus the average.
fn aggregate_json(row: &Aggregate) -> Value {
    let mut value = serde_json::to_value(row).unwrap_or(Value::Null);
    if let Value::Object(map) = &mut value {
        let average = row.sum_us.checked_div(row.count).unwrap_or(0);
        map.insert("average_us".into(), json!(average));
    }
    value
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
