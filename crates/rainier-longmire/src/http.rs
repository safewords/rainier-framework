//! The routes an application may mount: the same two answers as JSON, behind
//! a check the application gives.

use std::sync::Arc;

use rainier_http::{IntoResponse as _, Request, Response};
use rainier_routing::{Req, Router};
use rainier_support::{BoxFuture, Error};
use serde_json::{json, Value};

use crate::{Longmire, RecentQuery};

/// Who may read: a check over the request, run before every answer. Anyone
/// it refuses gets the router's own 404 — the same as a path that does not
/// exist.
pub type Gate = Arc<dyn Fn(&Request) -> BoxFuture<'static, bool> + Send + Sync>;

/// `GET /traces` and `GET /traces/{trace}` — mount them under a group of your
/// own (`/ops/longmire`). Lists answer `{"data": {"services", "traces"}}`,
/// one trace `{"data": {"trace_id", "services", "batches", "unanswered"}}`;
/// a malformed id is a 400.
///
/// ```
/// use std::sync::Arc;
/// use rainier_longmire::{routes, Longmire, Source};
///
/// let longmire = Arc::new(Longmire::new([Source::new("orders", "http://orders.internal")]));
/// let router = routes(longmire, Arc::new(|request| {
///     let ok = request.header("authorization") == Some("Bearer staff");
///     Box::pin(async move { ok })
/// }));
/// # let _ = router;
/// ```
pub fn routes(longmire: Arc<Longmire>, gate: Gate) -> Router {
    let mut router = Router::new();
    {
        let (longmire, gate) = (Arc::clone(&longmire), Arc::clone(&gate));
        router
            .get("/traces", move |request: Req| {
                let (longmire, gate) = (Arc::clone(&longmire), Arc::clone(&gate));
                Box::pin(async move {
                    if !gate(&request).await {
                        return denied(&request);
                    }
                    let query = recent_query(request.query());
                    Response::json(&json!({ "data": longmire.recent(&query).await }))
                }) as BoxFuture<'static, Response>
            })
            .name("longmire.traces");
    }
    router
        .get("/traces/{trace}", move |request: Req| {
            let (longmire, gate) = (Arc::clone(&longmire), Arc::clone(&gate));
            Box::pin(async move {
                if !gate(&request).await {
                    return denied(&request);
                }
                let id = request.route_param("trace").unwrap_or_default().to_string();
                match longmire.trace(&id).await {
                    Ok(trace) => Response::json(&json!({ "data": trace })),
                    Err(error) => error.into_response(),
                }
            }) as BoxFuture<'static, Response>
        })
        .name("longmire.trace");
    router
}

fn denied(request: &Request) -> Response {
    Error::not_found(format!("No route matches {}", request.path())).into_response()
}

/// `kept`, `before` and `limit` from a query string; anything else ignored.
fn recent_query(query: &Value) -> RecentQuery {
    let text = |name: &str| query.get(name).and_then(Value::as_str).map(str::trim);
    RecentQuery {
        kept: matches!(text("kept"), Some("1" | "true" | "yes")),
        before: text("before").filter(|b| !b.is_empty()).map(str::to_string),
        limit: text("limit").and_then(|l| l.parse().ok()),
    }
}
