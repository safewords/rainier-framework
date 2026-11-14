//! The HTML shell the Vue app mounts into, and the two built assets.
//!
//! The app is built from `frontend/` into `assets/dist/` — committed, with
//! stable file names — and compiled in here, so a `cargo build` never needs
//! Node. The shell carries no inline script or style: its one `<script>`
//! element is `type="application/json"`, data the app reads, which a CSP does
//! not govern and a browser never runs.

use std::hash::{Hash, Hasher};
use std::sync::{Arc, OnceLock};

use rainier_http::Response;
use rainier_routing::Req;
use rainier_support::Error;
use serde_json::json;

use super::Shared;

/// The built app.
pub(crate) static SCRIPT: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/dist/lookout.js"));
/// The built stylesheet.
pub(crate) static STYLESHEET: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/dist/lookout.css"));

/// The Content-Security-Policy the dashboard is served with.
///
/// Nothing inline and nothing from anywhere else: the app's script and
/// stylesheet from this origin, `fetch` to this origin, no plugins, no
/// framing, no form posts. An entry that smuggled markup past the app's
/// text-only rendering still could not run a script.
pub fn content_security_policy() -> &'static str {
    "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
     connect-src 'self'; base-uri 'none'; frame-ancestors 'none'; form-action 'none'"
}

/// A short fingerprint of both assets, for the shell's `?v=` — so an
/// upgrade is never served a year-old cached copy.
fn version() -> &'static str {
    static VERSION: OnceLock<String> = OnceLock::new();
    VERSION.get_or_init(|| {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        SCRIPT.hash(&mut hasher);
        STYLESHEET.hash(&mut hasher);
        format!("{:012x}", hasher.finish() & 0xffff_ffff_ffff)
    })
}

/// Escape text for an HTML attribute value.
fn attribute(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// JSON safe to place inside a `<script>` element: nothing in it can close
/// the element (`</script>`), open a comment (`<!--`), or — for a reader
/// that treats the text as JavaScript — end a line (U+2028, U+2029). Each is
/// replaced by its `\u` escape, which `JSON.parse` reads back unchanged.
pub(crate) fn script_json(value: &serde_json::Value) -> String {
    let encoded = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
    let mut out = String::with_capacity(encoded.len());
    for character in encoded.chars() {
        match character {
            '<' => out.push_str("\\u003c"),
            '>' => out.push_str("\\u003e"),
            '&' => out.push_str("\\u0026"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            other => out.push(other),
        }
    }
    out
}

fn render(state: &Shared) -> String {
    let config = json!({
        "base": state.base,
        "api": format!("{}/api", state.base),
        "environment": state.environment,
        "store": state.store.name(),
        "version": version(),
    });
    let assets = attribute(&format!("{}/assets", state.base));
    let version = version();
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <meta name=\"robots\" content=\"noindex,nofollow\">\
         <meta name=\"referrer\" content=\"no-referrer\">\
         <meta name=\"color-scheme\" content=\"light dark\">\
         <title>Lookout</title>\
         <link rel=\"stylesheet\" href=\"{assets}/lookout.css?v={version}\">\
         <script type=\"module\" src=\"{assets}/lookout.js?v={version}\"></script>\
         </head><body><div id=\"lookout\"></div>\
         <script type=\"application/json\" id=\"lookout-config\">{}</script>\
         <noscript>Lookout needs JavaScript. The JSON API is under {}.</noscript>\
         </body></html>",
        script_json(&config),
        attribute(&format!("{}/api", state.base)),
    )
}

/// `GET {path}`.
pub(crate) async fn page(state: Arc<Shared>, _request: Req) -> Response {
    Response::html(render(&state))
}

/// `GET {path}/{rest*}` — the shell again, for a client-side route, but
/// never under `/api/` or `/assets/`.
pub(crate) async fn fallback(state: Arc<Shared>, request: Req) -> Response {
    use rainier_http::IntoResponse as _;
    let rest = request.route_param("rest").unwrap_or_default();
    let reserved = ["api", "assets"]
        .iter()
        .any(|prefix| rest == *prefix || rest.starts_with(&format!("{prefix}/")));
    if reserved {
        return Error::not_found(format!("No route matches {}", request.path())).into_response();
    }
    Response::html(render(&state))
}

fn asset(bytes: &'static [u8], content_type: &str) -> Response {
    Response::ok(bytes.to_vec())
        .with_content_type(content_type)
        // `private`: the gate ran for this request, so no shared cache may
        // keep it. Long-lived, because the shell links it with `?v=`.
        .with_header("cache-control", "private, max-age=31536000, immutable")
}

/// `GET {path}/assets/lookout.js`.
pub(crate) async fn script(_state: Arc<Shared>, _request: Req) -> Response {
    asset(SCRIPT, "text/javascript; charset=utf-8")
}

/// `GET {path}/assets/lookout.css`.
pub(crate) async fn stylesheet(_state: Arc<Shared>, _request: Req) -> Response {
    asset(STYLESHEET, "text/css; charset=utf-8")
}
