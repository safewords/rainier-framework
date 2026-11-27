//! `GET /api/traces` and `GET /api/traces/{trace}`: this service's part of
//! traces that cross services, for something outside to put back together.
//!
//! Request tracing ([`RequestTracing`](crate::config::RequestTracing)) tags every
//! kept batch that answered another service or called one with the trace
//! (`trace:distributed`). The list is those batches, newest first; one trace
//! is every batch this service kept in it, each as a tree of spans.
//!
//! **What a trace says is the shape, never the content**: each span's type,
//! one-line summary, start, duration and whether it failed or was slow, and
//! for an outbound call the host, the status and the span id it put on the
//! wire. No bodies, headers, bindings, users or guards — a reader may call
//! these routes in request tracing, and a batch's content is the viewers'
//! alone.
//!
//! # Stitching services together
//!
//! Every outbound call names a span of its own in its `traceparent`, and an
//! [`HttpClient`](EntryType::HttpClient) span here carries it as `call.span_id`.
//! The service it reached records the same id as its batch's
//! `remote_parent_span_id`. So a trace across services is: ask each one for
//! `/api/traces/{trace}`, then hang every batch whose `remote_parent_span_id`
//! is some call's `call.span_id` under that call.

use std::sync::Arc;

use rainier_http::Response;
use rainier_routing::Req;
use rainier_support::{Error, Result};
use serde_json::{json, Value};

use super::params::Params;
use super::Shared;
use crate::entry::{Batch, BatchSummary, Entry, EntryType};
use crate::store::BatchQuery;

/// The tag on a kept batch that is part of a trace crossing services.
pub(crate) const DISTRIBUTED: &str = "trace:distributed";

/// The tag on a kept batch whose trace the services agreed to keep whole.
pub(crate) const KEPT: &str = "trace:kept";

/// Where the root entry of a batch that answered another service carries the
/// span it answered.
pub(crate) const PARENT_FIELD: &str = "rainier.trace.parent_span_id";

/// At most this many batches of one trace are read.
const MAX_BATCHES: usize = 200;

/// `GET /api/traces`: kept batches that are part of a trace crossing
/// services, newest first — `kept` (only traces kept whole), `before`,
/// `limit` (≤ 200, default 50).
pub(crate) async fn list(state: Arc<Shared>, request: Req) -> Result<Response> {
    Ok(Response::json(&list_value(&state, request.query()).await?))
}

pub(crate) async fn list_value(state: &Shared, query: &Value) -> Result<Value> {
    let mut params = Params::new(query);
    let kept = params.boolean("kept").unwrap_or(false);
    let before = params.time("before");
    let limit = params.limit(200, 50);
    params.finish()?;

    let found = state
        .store
        .batches(BatchQuery {
            tag: Some(if kept { KEPT } else { DISTRIBUTED }.to_string()),
            before,
            limit: Some(limit),
            ..BatchQuery::default()
        })
        .await?;
    let next = match found.last() {
        Some(last) if found.len() >= limit => json!({ "before": last.started_at.to_rfc3339() }),
        _ => Value::Null,
    };
    let data: Vec<Value> = found.iter().map(listed).collect();
    Ok(json!({ "service": service(state), "data": data, "next": next }))
}

/// `GET /api/traces/{trace}`: every batch this service kept in one trace,
/// each as a tree of spans.
pub(crate) async fn show(state: Arc<Shared>, request: Req) -> Result<Response> {
    let trace = request.route_param("trace").unwrap_or_default().to_string();
    Ok(Response::json(&show_value(&state, &trace).await?))
}

pub(crate) async fn show_value(state: &Shared, trace_id: &str) -> Result<Value> {
    if trace_id.is_empty()
        || trace_id.len() > 64
        || !trace_id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err(Error::bad_request("A trace id is up to 64 letters, digits and dashes."));
    }
    let found = state
        .store
        .batches(BatchQuery {
            trace_id: Some(trace_id.to_string()),
            limit: Some(MAX_BATCHES),
            ..BatchQuery::default()
        })
        .await?;
    let mut batches = Vec::with_capacity(found.len());
    for summary in &found {
        if let Some(batch) = state.store.batch(&summary.id).await? {
            batches.push(batch);
        }
    }
    batches.sort_by_key(|b| b.started_at);
    let data: Vec<Value> = batches.iter().map(shape).collect();
    Ok(json!({
        "data": {
            "service": service(state),
            "trace_id": trace_id,
            "batches": data,
        }
    }))
}

/// The name this service goes by in a trace.
fn service(state: &Shared) -> Option<String> {
    state.config.request_tracing.service.clone()
}

/// One batch in the list: enough to choose a trace to open.
fn listed(summary: &BatchSummary) -> Value {
    json!({
        "trace_id": summary.trace_id,
        "batch_id": summary.id,
        "kind": summary.kind,
        "summary": summary.summary,
        "method": summary.method,
        "status": summary.status,
        "duration_us": summary.duration_us,
        "started_at": summary.started_at,
        "has_exception": summary.has_exception,
        "has_slow": summary.has_slow,
        "kept": summary.tags.iter().any(|t| t == KEPT),
    })
}

/// One batch's shape: its spans, without their content.
pub(crate) fn shape(batch: &Batch) -> Value {
    let root = batch.entries.iter().find(|e| e.span_id.as_deref() == Some(batch.id.as_str()));
    let remote_parent =
        root.and_then(|e| e.content.get(PARENT_FIELD)).and_then(Value::as_str).map(str::to_string);
    let spans: Vec<Value> = batch.entries.iter().map(span).collect();
    json!({
        "id": batch.id,
        "kind": batch.kind,
        "summary": batch.summary,
        "route_name": batch.route_name,
        "method": batch.method,
        "status": batch.status,
        "duration_us": batch.duration_us,
        "started_at": batch.started_at,
        "has_exception": batch.has_exception,
        "has_slow": batch.has_slow,
        "host": batch.host,
        "kept": batch.tags.iter().any(|t| t == KEPT),
        "root_span_id": batch.id,
        "remote_parent_span_id": remote_parent,
        "spans": spans,
    })
}

fn span(entry: &Entry) -> Value {
    let tagged = |tag: &str| entry.tags.iter().any(|t| t == tag);
    let mut value = json!({
        "span_id": entry.span_id,
        "parent_span_id": entry.parent_span_id,
        "type": entry.entry_type,
        "summary": entry.summary,
        "occurred_at": entry.occurred_at,
        "duration_us": entry.duration_us,
        "failed": tagged("failed") || entry.entry_type == EntryType::Exception,
        "slow": tagged("slow"),
    });
    if entry.entry_type == EntryType::HttpClient {
        let field = |name: &str| entry.content.get(name).cloned().unwrap_or(Value::Null);
        value["call"] = json!({
            "span_id": field("rainier.trace.span_id"),
            "host": field("server.address"),
            "status": field("http.response.status_code"),
            "peer_kept": field("rainier.trace.peer_kept"),
            "peer_span_id": field("rainier.trace.peer_span_id"),
        });
    }
    value
}
