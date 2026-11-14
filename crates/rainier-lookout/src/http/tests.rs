//! The dashboard and API, driven through a compiled router over [`FakeStore`].

use std::sync::Arc;

use rainier_container::Container;
use rainier_http::{Method, Request, Response, StatusCode};
use rainier_routing::{CompiledRouter, Router};
use serde_json::Value;

use super::fake::{entry, epoch, request, FakeStore};
use super::*;
use crate::entry::EntryType;

struct Answer {
    status: StatusCode,
    response: Response,
    text: String,
}

impl Answer {
    fn json(&self) -> Value {
        serde_json::from_str(&self.text).unwrap_or(Value::Null)
    }
    fn header(&self, name: &str) -> Option<&str> {
        self.response.header(name)
    }
}

fn sample() -> Arc<FakeStore> {
    let e = epoch();
    let mut root = entry("b1", "r", EntryType::Request, "GET /posts 200", e, 0, Some(20_000));
    root.span_id = Some("root".into());
    let mut q1 = entry("b1", "q1", EntryType::Query, "select * from posts", e, 2, Some(3_000));
    q1.parent_span_id = Some("root".into());
    q1.tags = vec!["slow".into()];
    let mut q2 =
        entry("b1", "q2", EntryType::Query, "select * from users where id = ?", e, 6, Some(500));
    q2.parent_span_id = Some("span-q1".into());
    q2.tags = vec!["n_plus_one".into()];
    let mut cache = entry("b1", "c1", EntryType::Cache, "get posts:index", e, 1, Some(80));
    cache.tags = vec!["miss".into()];
    let one = request("b1", "posts.index", 200, 30, vec![root, q1, q2, cache]);

    let mut boom = entry("b2", "x1", EntryType::Exception, "<script>alert(1)</script>", e, 4, None);
    boom.family_hash = Some("Boom@app".into());
    boom.content = serde_json::json!({ "message": "</pre><script>alert(2)</script>" });
    let mut two = request("b2", "posts.show", 500, 10, vec![boom]);
    two.summary = "GET /posts/<img src=x onerror=alert(3)> 500".into();
    two.user_id = Some("42".into());

    Arc::new(FakeStore::with(vec![one, two]))
}

fn app_with(store: Arc<FakeStore>, gate: LookoutGate, environment: &str) -> CompiledRouter {
    let config = LookoutConfig::default().enabled(true);
    let mut router = Router::new();
    router.merge(routes(store, Arc::new(config), gate, environment));
    router.compile(&Container::new()).unwrap()
}

fn app(store: Arc<FakeStore>) -> CompiledRouter {
    app_with(store, LookoutGate::allow_all(), "production")
}

async fn send(
    router: &CompiledRouter,
    method: Method,
    uri: &str,
    headers: &[(&str, &str)],
) -> Answer {
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(name, value);
    }
    let mut response = router.dispatch(builder.build()).await;
    let bytes = response.take_body().collect().await.unwrap();
    Answer {
        status: response.status(),
        text: String::from_utf8_lossy(&bytes).into_owned(),
        response,
    }
}

async fn get(router: &CompiledRouter, uri: &str) -> Answer {
    send(router, Method::GET, uri, &[]).await
}

const EVERY_GET: &[&str] = &[
    "/_lookout",
    "/_lookout/batches",
    "/_lookout/batches/b1",
    "/_lookout/entries/query",
    "/_lookout/assets/lookout.js",
    "/_lookout/assets/lookout.css",
    "/_lookout/api/batches",
    "/_lookout/api/batches/b1",
    "/_lookout/api/entries/query",
    "/_lookout/api/aggregates/query",
    "/_lookout/api/status",
];

// --- the gate -----------------------------------------------------------------

#[tokio::test]
async fn a_refused_request_gets_the_routers_own_404_everywhere() {
    let router = app_with(sample(), LookoutGate::deny_all(), "production");
    let mut nothing = Router::new();
    nothing.get("/elsewhere", || async { "x" });
    let nothing = nothing.compile(&Container::new()).unwrap();

    for uri in EVERY_GET {
        let answer = get(&router, uri).await;
        let baseline = get(&nothing, uri).await;
        assert_eq!(answer.status, StatusCode::NOT_FOUND, "{uri}");
        assert_eq!(answer.text, baseline.text, "{uri}");
        assert_eq!(answer.header("cache-control"), None, "{uri} must not look like Lookout");
        assert_eq!(answer.header("x-robots-tag"), None, "{uri}");
    }
    // Other methods too — a 405 would say something lives here.
    for (method, uri) in [
        (Method::POST, "/_lookout"),
        (Method::PUT, "/_lookout/api/batches"),
        (Method::DELETE, "/_lookout/api/batches"),
        (Method::DELETE, "/_lookout/api/status"),
    ] {
        let answer = send(&router, method.clone(), uri, &[("x-lookout", "1")]).await;
        assert_eq!(answer.status, StatusCode::NOT_FOUND, "{method} {uri}");
    }
}

#[tokio::test]
async fn an_allowed_request_gets_every_page() {
    let router = app(sample());
    for uri in EVERY_GET {
        let answer = get(&router, uri).await;
        assert_eq!(answer.status, StatusCode::OK, "{uri}: {}", answer.text);
    }
}

#[tokio::test]
async fn the_check_sees_the_request() {
    let gate = LookoutGate::new(|request| {
        let ok = request.header("x-operator") == Some("yes");
        Box::pin(async move { ok })
    });
    let router = app_with(sample(), gate, "production");
    assert_eq!(get(&router, "/_lookout/api/status").await.status, StatusCode::NOT_FOUND);
    let answer = send(&router, Method::GET, "/_lookout/api/status", &[("x-operator", "yes")]).await;
    assert_eq!(answer.status, StatusCode::OK);
}

#[tokio::test]
async fn local_is_open_when_configured_and_only_locally() {
    let router = app_with(sample(), LookoutGate::deny_all(), "local");
    assert_eq!(get(&router, "/_lookout").await.status, StatusCode::OK);

    let router = app_with(sample(), LookoutGate::deny_all(), "testing");
    assert_eq!(get(&router, "/_lookout").await.status, StatusCode::NOT_FOUND);

    let mut config = LookoutConfig::default();
    config.dashboard.open_locally = false;
    let mut router = Router::new();
    router.merge(routes(sample(), Arc::new(config), LookoutGate::deny_all(), "local"));
    let router = router.compile(&Container::new()).unwrap();
    assert_eq!(get(&router, "/_lookout").await.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_disabled_dashboard_declares_nothing_and_the_path_is_configurable() {
    let mut config = LookoutConfig::default();
    config.dashboard.enabled = false;
    assert!(routes(sample(), Arc::new(config), LookoutGate::allow_all(), "local").is_empty());

    let mut config = LookoutConfig::default();
    config.dashboard.path = "ops/lookout/".into();
    let router = routes(sample(), Arc::new(config), LookoutGate::allow_all(), "local");
    assert_eq!(router.uri_for("lookout.dashboard"), Some("/ops/lookout"));
    assert_eq!(router.uri_for("lookout.api.batch"), Some("/ops/lookout/api/batches/{id}"));
    let compiled = router.compile(&Container::new()).unwrap();
    let page = get(&compiled, "/ops/lookout/batches").await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.text.contains("src=\"/ops/lookout/assets/lookout.js?v="), "{}", page.text);
    assert!(page.text.contains("\"base\":\"/ops/lookout\""), "{}", page.text);
    assert!(page.text.contains("\"api\":\"/ops/lookout/api\""), "{}", page.text);
    assert_eq!(get(&compiled, "/ops/lookout/assets/lookout.css").await.status, StatusCode::OK);
}

#[test]
fn routes_are_named_under_lookout() {
    let router = routes(sample(), Arc::new(LookoutConfig::default()), LookoutGate::deny_all(), "x");
    let mut names: Vec<&str> = router.named_routes().into_iter().map(|(n, _)| n).collect();
    names.sort();
    assert_eq!(
        names,
        [
            "lookout.api.aggregates",
            "lookout.api.batch",
            "lookout.api.batches",
            "lookout.api.clear",
            "lookout.api.entries",
            "lookout.api.status",
            "lookout.app",
            "lookout.assets.css",
            "lookout.assets.js",
            "lookout.dashboard",
        ]
    );
}

// --- headers ------------------------------------------------------------------

#[tokio::test]
async fn every_response_is_unindexed_and_the_shell_is_locked_down() {
    let router = app(sample());
    for uri in
        EVERY_GET.iter().chain(["/_lookout/api/batches?limit=0", "/_lookout/api/nope"].iter())
    {
        let answer = get(&router, uri).await;
        let caching = answer.header("cache-control").unwrap_or_default().to_string();
        if uri.contains("/assets/") {
            assert_eq!(caching, "private, max-age=31536000, immutable", "{uri}");
        } else {
            assert_eq!(caching, "no-store", "{uri}");
        }
        assert_eq!(answer.header("x-robots-tag"), Some("noindex, nofollow"), "{uri}");
        assert_eq!(answer.header("x-content-type-options"), Some("nosniff"), "{uri}");
        assert!(answer.header("content-security-policy").is_some(), "{uri}");
    }
    let page = get(&router, "/_lookout").await;
    assert_eq!(
        page.header("content-security-policy"),
        Some(
            "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
             connect-src 'self'; base-uri 'none'; frame-ancestors 'none'; form-action 'none'"
        )
    );
    assert_eq!(page.header("x-frame-options"), Some("DENY"));
}

#[tokio::test]
async fn the_shell_has_nothing_inline_to_run() {
    let page = get(&app(sample()), "/_lookout").await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.header("content-type").unwrap().starts_with("text/html"));
    assert!(!page.text.contains("<style"));
    assert!(!page.text.contains(" style="));
    // Two script elements: the external module, and the JSON data block —
    // which a browser does not execute and a CSP does not govern.
    assert_eq!(page.text.matches("<script").count(), 2, "{}", page.text);
    assert!(page.text.contains("<script type=\"module\" src=\"/_lookout/assets/lookout.js?v="));
    assert!(page.text.contains("<script type=\"application/json\" id=\"lookout-config\">"));
    assert!(page.text.contains("<link rel=\"stylesheet\" href=\"/_lookout/assets/lookout.css?v="));
}

#[tokio::test]
async fn the_config_cannot_break_out_of_its_script_element() {
    let environment = "</script><script>alert(1)</script><!--\u{2028}";
    let router = app_with(sample(), LookoutGate::allow_all(), environment);
    let page = get(&router, "/_lookout").await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.text);
    // Only the two script elements the shell writes, each closed once.
    assert_eq!(page.text.matches("<script").count(), 2, "{}", page.text);
    assert_eq!(page.text.matches("</script>").count(), 2, "{}", page.text);

    let start = page.text.find("id=\"lookout-config\">").unwrap() + "id=\"lookout-config\">".len();
    let end = start + page.text[start..].find("</script>").unwrap();
    let json: Value = serde_json::from_str(&page.text[start..end]).unwrap();
    assert_eq!(json["environment"], environment);
    assert_eq!(json["base"], "/_lookout");
    assert_eq!(json["api"], "/_lookout/api");
    assert_eq!(json["store"], "fake");
}

#[test]
fn script_json_escapes_what_could_end_the_element() {
    let out = super::shell::script_json(&serde_json::json!({
        "a": "</script><!--",
        "b": "&",
        "c": "\u{2028}\u{2029}",
    }));
    for bad in ["<", ">", "&", "\u{2028}", "\u{2029}"] {
        assert!(!out.contains(bad), "{bad:?} in {out}");
    }
    let back: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(back["a"], "</script><!--");
    assert_eq!(back["c"], "\u{2028}\u{2029}");
}

#[tokio::test]
async fn serves_the_built_assets_with_their_types() {
    let router = app(sample());
    let js = get(&router, "/_lookout/assets/lookout.js").await;
    assert_eq!(js.status, StatusCode::OK);
    assert_eq!(js.header("content-type"), Some("text/javascript; charset=utf-8"));
    assert!(js.text.len() > 1000);
    let css = get(&router, "/_lookout/assets/lookout.css").await;
    assert_eq!(css.status, StatusCode::OK);
    assert_eq!(css.header("content-type"), Some("text/css; charset=utf-8"));
    assert!(css.text.len() > 1000);
    assert_eq!(get(&router, "/_lookout/assets/other.js").await.status, StatusCode::NOT_FOUND);
}

#[test]
fn the_built_frontend_is_committed() {
    // `include_bytes!` already refuses to compile without them; this says
    // what to do about an empty or half-written build.
    let dist = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/dist");
    for (name, bytes) in
        [("lookout.js", super::shell::SCRIPT), ("lookout.css", super::shell::STYLESHEET)]
    {
        assert!(
            dist.join(name).is_file() && bytes.len() > 1000,
            "assets/dist/{name} is missing or empty: run `npm ci && npm run build` in \
             crates/rainier-lookout/frontend and commit the result"
        );
    }
    let js = std::str::from_utf8(super::shell::SCRIPT).unwrap();
    assert!(js.contains("lookout-config"), "the build does not read the shell's config");
    assert!(!js.contains("new Function("), "the build needs 'unsafe-eval'");
}

#[tokio::test]
async fn the_app_fallback_serves_client_routes_but_never_shadows_the_api() {
    let router = app(sample());
    for uri in
        ["/_lookout/batches/b1", "/_lookout/entries/query", "/_lookout/status", "/_lookout/a/b/c"]
    {
        let page = get(&router, uri).await;
        assert_eq!(page.status, StatusCode::OK, "{uri}");
        assert!(page.text.contains("id=\"lookout-config\""), "{uri}");
    }
    for uri in [
        "/_lookout/api",
        "/_lookout/api/",
        "/_lookout/api/nope",
        "/_lookout/api/batches/b1/x",
        "/_lookout/assets",
        "/_lookout/assets/x.css",
    ] {
        let answer = get(&router, uri).await;
        assert_eq!(answer.status, StatusCode::NOT_FOUND, "{uri}");
        assert!(!answer.text.contains("lookout-config"), "{uri}");
        assert!(
            answer.header("content-type").unwrap_or_default().starts_with("application/json"),
            "{uri}"
        );
    }
    // The real API still answers JSON, not the shell.
    let status = get(&router, "/_lookout/api/status").await;
    assert_eq!(status.json()["data"]["store"], "fake");
}

// --- the API ------------------------------------------------------------------

#[tokio::test]
async fn lists_batches_with_filters_mapped_onto_the_query() {
    let store = sample();
    let router = app(store.clone());
    let answer = get(&router, "/_lookout/api/batches").await;
    let body = answer.json();
    assert_eq!(body["data"].as_array().unwrap().len(), 2);
    assert_eq!(body["data"][0]["id"], "b2", "newest first");
    assert_eq!(body["next"], Value::Null);

    let answer = get(
        &router,
        "/_lookout/api/batches?kind=request&type=exception&route=posts.show&user=42&tag=t\
         &exception=1&slow=false&before=2030-01-01T00:00:00Z&limit=1",
    )
    .await;
    assert_eq!(answer.status, StatusCode::OK);
    let query = store.last_query.lock().unwrap().clone().unwrap();
    assert_eq!(query.kind, Some(crate::entry::BatchKind::Request));
    assert_eq!(query.entry_type, Some(EntryType::Exception));
    assert_eq!(query.route_name.as_deref(), Some("posts.show"));
    assert_eq!(query.user_id.as_deref(), Some("42"));
    assert_eq!(query.tag.as_deref(), Some("t"));
    assert_eq!(query.has_exception, Some(true));
    assert_eq!(query.has_slow, Some(false));
    assert_eq!(query.before.unwrap().to_rfc3339(), "2030-01-01T00:00:00+00:00");
    assert_eq!(query.limit, Some(1));

    get(&router, "/_lookout/api/batches?trace=trace-b2").await;
    let query = store.last_query.lock().unwrap().clone().unwrap();
    assert_eq!(query.trace_id.as_deref(), Some("trace-b2"));
    let related = get(&router, "/_lookout/api/batches?trace=trace-b2").await.json();
    assert_eq!(related["data"].as_array().unwrap().len(), 1);
    assert_eq!(related["data"][0]["trace_id"], "trace-b2");

    let page = get(&router, "/_lookout/api/batches?limit=1").await.json();
    assert_eq!(page["data"].as_array().unwrap().len(), 1);
    let before = page["next"]["before"].as_str().unwrap().to_string();
    let next = get(
        &router,
        &format!("/_lookout/api/batches?limit=1&before={}", before.replace('+', "%2B")),
    )
    .await
    .json();
    assert_eq!(next["data"][0]["id"], "b1");
}

#[tokio::test]
async fn bad_list_parameters_are_a_422_in_rainiers_shape() {
    let router = app(sample());
    let answer = get(
        &router,
        "/_lookout/api/batches?kind=nope&type=bogus&exception=maybe&before=yesterday&limit=500",
    )
    .await;
    assert_eq!(answer.status, StatusCode::UNPROCESSABLE_ENTITY);
    let body = answer.json();
    assert_eq!(body["message"], "The given data was invalid.");
    for field in ["kind", "type", "exception", "before", "limit"] {
        assert!(body["errors"][field].is_array(), "{field}: {body}");
    }
    let answer = get(&router, "/_lookout/api/batches?limit=0").await;
    assert_eq!(answer.status, StatusCode::UNPROCESSABLE_ENTITY);
    let answer = get(&router, "/_lookout/api/batches?limit[]=1").await;
    assert_eq!(answer.status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn shows_one_batch_with_its_entries_or_404s() {
    let router = app(sample());
    let body = get(&router, "/_lookout/api/batches/b1").await.json();
    assert_eq!(body["data"]["id"], "b1");
    assert_eq!(body["data"]["entries"].as_array().unwrap().len(), 4);
    assert_eq!(body["data"]["entries"][1]["type"], "query");

    let answer = get(&router, "/_lookout/api/batches/missing").await;
    assert_eq!(answer.status, StatusCode::NOT_FOUND);
    assert_eq!(answer.json()["message"], "No such batch.");
}

#[tokio::test]
async fn lists_entries_of_a_type() {
    let router = app(sample());
    let body = get(&router, "/_lookout/api/entries/query").await.json();
    let data = body["data"].as_array().unwrap();
    assert_eq!(data.len(), 2);
    assert_eq!(data[0]["id"], "q2", "newest first");

    let body = get(&router, "/_lookout/api/entries/query?limit=1").await.json();
    assert_eq!(body["data"].as_array().unwrap().len(), 1);
    assert!(body["next"]["before"].is_string());

    let answer = get(&router, "/_lookout/api/entries/nonsense?limit=x").await;
    assert_eq!(answer.status, StatusCode::UNPROCESSABLE_ENTITY);
    let body = answer.json();
    assert!(body["errors"]["type"].is_array() && body["errors"]["limit"].is_array(), "{body}");
}

#[tokio::test]
async fn rolls_up_aggregates() {
    let router = app(sample());
    let body =
        get(&router, "/_lookout/api/aggregates/query?period=60&order=max&limit=5").await.json();
    let data = body["data"].as_array().unwrap();
    assert_eq!(data.len(), 2);
    assert_eq!(data[0]["key"], "select * from posts");
    assert_eq!(data[0]["max_us"], 3000);
    assert_eq!(data[0]["average_us"], 3000);

    let body = get(&router, "/_lookout/api/aggregates/exception?order=count").await.json();
    assert_eq!(body["data"][0]["key"], "Boom@app");
    assert_eq!(body["data"][0]["count"], 1);

    let answer =
        get(&router, "/_lookout/api/aggregates/log?period=7&order=sideways&since=no&limit=101")
            .await;
    assert_eq!(answer.status, StatusCode::UNPROCESSABLE_ENTITY);
    let body = answer.json();
    for field in ["type", "period", "order", "since", "limit"] {
        assert!(body["errors"][field].is_array(), "{field}: {body}");
    }
}

#[tokio::test]
async fn reports_status_without_secrets() {
    let router = app(sample());
    let body = get(&router, "/_lookout/api/status").await.json();
    let data = &body["data"];
    assert_eq!(data["store"], "fake");
    assert_eq!(data["store_kind"], "memory");
    assert_eq!(data["enabled"], true);
    assert_eq!(data["environment"], "production");
    assert_eq!(data["sample_rates"]["request"], 1.0);
    assert_eq!(data["retention"]["batches_hours"], 24);
    assert_eq!(data["watchers"]["query"]["slow_ms"], 100);
    assert_eq!(data["watchers"]["log"]["enabled"], true);
    let text = body.to_string();
    for secret in ["password", "authorization", "redaction", "keep"] {
        assert!(!text.contains(secret), "{secret} leaked: {text}");
    }
}

#[tokio::test]
async fn clearing_needs_the_header() {
    let store = sample();
    let router = app(store.clone());
    let answer = send(&router, Method::DELETE, "/_lookout/api/batches", &[]).await;
    assert_eq!(answer.status, StatusCode::FORBIDDEN);
    assert_eq!(store.batches.lock().unwrap().len(), 2);

    let answer =
        send(&router, Method::DELETE, "/_lookout/api/batches", &[("x-lookout", "yes")]).await;
    assert_eq!(answer.status, StatusCode::FORBIDDEN);

    let answer = send(&router, Method::POST, "/_lookout/api/batches", &[("x-lookout", "1")]).await;
    assert_eq!(answer.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(answer.header("allow"), Some("GET, HEAD, DELETE"));
    assert_eq!(store.batches.lock().unwrap().len(), 2);

    let answer =
        send(&router, Method::DELETE, "/_lookout/api/batches", &[("x-lookout", "1")]).await;
    assert_eq!(answer.status, StatusCode::NO_CONTENT);
    assert!(store.batches.lock().unwrap().is_empty());
    assert_eq!(answer.header("cache-control"), Some("no-store"));
}
