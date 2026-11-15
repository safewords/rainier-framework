//! The two public routes real user monitoring needs: the browser script and
//! the endpoint it reports to. See [`crate::rum`].
//!
//! Neither is behind the dashboard's gate — every visitor's browser loads one
//! and posts to the other — and both exist only while `LOOKOUT_RUM` is on.

use std::sync::Arc;

use rainier_http::{Response, StatusCode};
use rainier_routing::Req;

use super::Shared;
use crate::rum::{self, Report, MAX_CALLS, MAX_REPORT_BYTES};

/// `GET {path}/rum.js`.
pub(crate) async fn script(state: Arc<Shared>, _request: Req) -> Response {
    Response::text(rum::script(&state.config.rum))
        .with_content_type("application/javascript; charset=utf-8")
        .with_header("cache-control", "public, max-age=3600")
        // Loaded by the frontend's pages, which are another origin.
        .with_header("cross-origin-resource-policy", "cross-origin")
        .with_header("x-content-type-options", "nosniff")
}

/// `POST {path}/rum`: a report from a page.
///
/// Answers `204` whatever happened to the report — a browser does not read
/// the answer to a beacon, and a sender probing the endpoint learns nothing.
/// What is refused is refused silently: a report from an origin not listed,
/// one too large, one over its sender's rate, one that does not parse.
pub(crate) async fn ingest(state: Arc<Shared>, request: Req) -> Response {
    let done = || Response::new(StatusCode::NO_CONTENT).with_header("cache-control", "no-store");
    let rum = &state.config.rum;

    let Some(origin) = request.header("origin").map(|o| o.trim_end_matches('/').to_string()) else {
        return done();
    };
    if !rum.origins.iter().any(|allowed| allowed == &origin) {
        return done();
    }
    let body = request.body();
    if body.is_empty() || body.len() > MAX_REPORT_BYTES {
        return done();
    }
    let Ok(report) = serde_json::from_slice::<Report>(body) else {
        return done();
    };
    let calls = report.calls.len().min(MAX_CALLS) as u32;
    if calls == 0 || !state.rum_limiter.allow(request.ip(), calls) {
        return done();
    }

    let batches = rum::batches(&state.config, report, request.ip(), request.header("user-agent"));
    if !batches.is_empty() {
        // Stored as they are: a browser batch adds nothing to the request
        // counts, and is never sampled away — the browser already sampled.
        let _ = state.store.store_counting(batches, Vec::new()).await;
    }
    done().with_header("access-control-allow-origin", &origin).with_header("vary", "origin")
}
