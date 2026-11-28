//! The routes an application may mount: the same two answers as JSON, behind
//! a check the application gives.

use std::sync::Arc;

use rainier_http::{IntoResponse as _, Request, Response};
use rainier_routing::{Req, Router};
use rainier_support::{BoxFuture, Error};
use serde_json::{json, Value};

use crate::{Endpoint, Longmire, RecentQuery, Resource};

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
    fleet_routes(&mut router, &longmire, &gate);
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

/// A route answering `answer(longmire, query string)` as `{"data": …}`.
macro_rules! fleet_route {
    ($router:expr, $longmire:expr, $gate:expr, $path:expr, $name:expr, $method:ident) => {{
        let (longmire, gate) = (Arc::clone($longmire), Arc::clone($gate));
        $router
            .get($path, move |request: Req| {
                let (longmire, gate) = (Arc::clone(&longmire), Arc::clone(&gate));
                Box::pin(async move {
                    if !gate(&request).await {
                        return denied(&request);
                    }
                    let query = request.query_string().to_string();
                    Response::json(&json!({ "data": longmire.$method(&query).await }))
                }) as BoxFuture<'static, Response>
            })
            .name($name);
    }};
}

/// `/health`, `/requests`, `/queues`, `/rum/vitals`, `/rum/errors`, `/cases`, `/map`
/// across every source, and `/sources/{source}/{endpoint}` for one.
fn fleet_routes(router: &mut Router, longmire: &Arc<Longmire>, gate: &Gate) {
    fleet_route!(router, longmire, gate, "/health", "longmire.health", health);
    fleet_route!(router, longmire, gate, "/requests", "longmire.requests", requests);
    fleet_route!(router, longmire, gate, "/queues", "longmire.queues", queues);
    fleet_route!(router, longmire, gate, "/rum/vitals", "longmire.rum.vitals", rum_vitals);
    fleet_route!(router, longmire, gate, "/rum/errors", "longmire.rum.errors", rum_errors);
    fleet_route!(router, longmire, gate, "/cases", "longmire.cases", cases);
    fleet_route!(router, longmire, gate, "/map", "longmire.map", map);
    fleet_route!(router, longmire, gate, "/batches", "longmire.batches", batches);
    fleet_route!(router, longmire, gate, "/models", "longmire.models", models);
    fleet_route!(router, longmire, gate, "/insights", "longmire.insights", insights);
    typed_routes(router, longmire, gate);
    resource_routes(router, longmire, gate);
    for endpoint in Endpoint::ALL {
        let (longmire, gate) = (Arc::clone(longmire), Arc::clone(gate));
        router
            .get(format!("/sources/{{source}}/{}", endpoint.path()), move |request: Req| {
                let (longmire, gate) = (Arc::clone(&longmire), Arc::clone(&gate));
                Box::pin(async move {
                    if !gate(&request).await {
                        return denied(&request);
                    }
                    let source = request.route_param("source").unwrap_or_default().to_string();
                    match longmire.one(&source, endpoint, request.query_string()).await {
                        Ok(answer) => Response::json(&answer),
                        Err(reason) => Response::json(&json!({ "message": reason }))
                            .with_status(rainier_http::StatusCode::BAD_GATEWAY),
                    }
                }) as BoxFuture<'static, Response>
            })
            .name(format!("longmire.source.{}", endpoint.path().replace('/', ".")));
    }
}

/// `/entries/{type}`, `/aggregates/{type}`, `/series/{type}` across every
/// source, and `/status`.
fn typed_routes(router: &mut Router, longmire: &Arc<Longmire>, gate: &Gate) {
    for (path, name) in [
        ("/entries/{type}", "longmire.entries"),
        ("/aggregates/{type}", "longmire.aggregates"),
        ("/series/{type}", "longmire.series"),
        ("/status", "longmire.status"),
    ] {
        let (longmire, gate) = (Arc::clone(longmire), Arc::clone(gate));
        router
            .get(path, move |request: Req| {
                let (longmire, gate) = (Arc::clone(&longmire), Arc::clone(&gate));
                Box::pin(async move {
                    if !gate(&request).await {
                        return denied(&request);
                    }
                    let t = request.route_param("type").unwrap_or_default().to_string();
                    let query = request.query_string().to_string();
                    let answer = match name {
                        "longmire.entries" => longmire.entries(&t, &query).await,
                        "longmire.aggregates" => longmire.aggregates(&t, &query).await,
                        "longmire.series" => longmire.series(&t, &query).await,
                        _ => longmire.status().await,
                    };
                    Response::json(&json!({ "data": answer }))
                }) as BoxFuture<'static, Response>
            })
            .name(name);
    }
}

/// `/sources/{source}/…` for every [`Resource`]: one source's own answer, or
/// a 502 with the reason; and `POST /sources/{source}/cases/{id}`, a case
/// action.
fn resource_routes(router: &mut Router, longmire: &Arc<Longmire>, gate: &Gate) {
    for (pattern, name) in [
        ("batches", "batches"),
        ("batches/{a}", "batch"),
        ("entries/{a}", "entries"),
        ("aggregates/{a}", "aggregates"),
        ("series/{a}", "series"),
        ("families/{a}/{b}", "family"),
        ("models", "models"),
        ("models/{a}", "model"),
        ("insights", "insights"),
        ("cases/{a}", "case"),
        ("status", "status"),
        ("traces", "traces"),
        ("traces/{a}", "trace"),
    ] {
        let (longmire, gate) = (Arc::clone(longmire), Arc::clone(gate));
        router
            .get(format!("/sources/{{source}}/{pattern}"), move |request: Req| {
                let (longmire, gate) = (Arc::clone(&longmire), Arc::clone(&gate));
                Box::pin(async move {
                    if !gate(&request).await {
                        return denied(&request);
                    }
                    let segment = |at: &str| request.route_param(at).unwrap_or_default();
                    let tail = pattern.replace("{a}", segment("a")).replace("{b}", segment("b"));
                    let Some(resource) = Resource::parse(&tail) else {
                        return denied(&request);
                    };
                    let source = segment("source").to_string();
                    answer(longmire.read(&source, &resource, request.query_string()).await)
                }) as BoxFuture<'static, Response>
            })
            .name(format!("longmire.source.{name}"));
    }
    let (longmire, gate) = (Arc::clone(longmire), Arc::clone(gate));
    router
        .post("/sources/{source}/cases/{a}", move |request: Req| {
            let (longmire, gate) = (Arc::clone(&longmire), Arc::clone(&gate));
            Box::pin(async move {
                if !gate(&request).await {
                    return denied(&request);
                }
                let action: Value = match request.json() {
                    Ok(action) => action,
                    Err(error) => return error.into_response(),
                };
                let source = request.route_param("source").unwrap_or_default().to_string();
                let case = request.route_param("a").unwrap_or_default().to_string();
                answer(longmire.act_on_case(&source, &case, &action).await)
            }) as BoxFuture<'static, Response>
        })
        .name("longmire.source.case.act");
}

fn answer(outcome: Result<Value, String>) -> Response {
    match outcome {
        Ok(answer) => Response::json(&answer),
        Err(reason) => Response::json(&json!({ "message": reason }))
            .with_status(rainier_http::StatusCode::BAD_GATEWAY),
    }
}
