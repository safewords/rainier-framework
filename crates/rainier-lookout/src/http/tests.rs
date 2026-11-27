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
    "/_lookout/api/models",
    "/_lookout/api/models/Post",
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
            "lookout.api.case",
            "lookout.api.case.act",
            "lookout.api.cases",
            "lookout.api.clear",
            "lookout.api.entries",
            "lookout.api.family",
            "lookout.api.health",
            "lookout.api.insights",
            "lookout.api.model",
            "lookout.api.models",
            "lookout.api.requests",
            "lookout.api.rum.errors",
            "lookout.api.rum.vitals",
            "lookout.api.series",
            "lookout.api.status",
            "lookout.api.stream",
            "lookout.api.trace",
            "lookout.api.traces",
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
            "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; \
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
async fn lists_models_by_writes_and_one_models_changes() {
    let e = epoch();
    let change = |id: &str, key: &str, offset: i64| {
        let mut change = entry("m1", id, EntryType::Model, key, e, offset, None);
        change.content = serde_json::json!({ "batch": { "trace_id": "trace-m1" } });
        change
    };
    let one = request(
        "m1",
        "posts.store",
        201,
        20,
        vec![
            change("p1", "Post created", 1),
            change("t1", "Tag created", 2),
            change("p2", "Post updated", 3),
        ],
    );
    let router = app(Arc::new(FakeStore::with(vec![one])));

    let body = get(&router, "/_lookout/api/models").await.json();
    let data = body["data"].as_array().unwrap();
    assert_eq!(data.len(), 2, "{body}");
    assert_eq!(data[0]["model"], "Post", "most written first");
    assert_eq!(data[0]["total"], 2);
    assert_eq!(data[0]["actions"]["created"], 1);
    assert_eq!(data[0]["actions"]["updated"], 1);
    assert_eq!(body["period_s"], 600);

    let body = get(&router, "/_lookout/api/models/Post").await.json();
    let ids: Vec<&str> =
        body["data"].as_array().unwrap().iter().map(|e| e["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["p2", "p1"], "newest first, every action, no other model");
    assert_eq!(body["data"][0]["batch_id"], "m1");
    assert_eq!(body["data"][0]["content"]["batch"]["trace_id"], "trace-m1");

    let body = get(&router, "/_lookout/api/models/Post?limit=1").await.json();
    assert!(body["next"]["before"].is_string());
    let answer = get(&router, "/_lookout/api/models?period=7").await;
    assert_eq!(answer.status, StatusCode::UNPROCESSABLE_ENTITY);
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

#[tokio::test]
async fn narrows_entries_to_one_family_and_searches_them() {
    let router = app(sample());
    let body =
        get(&router, "/_lookout/api/entries/query?key=select%20*%20from%20posts").await.json();
    let data = body["data"].as_array().unwrap();
    assert_eq!(data.len(), 1, "{body}");
    assert_eq!(data[0]["id"], "q1");

    let body = get(&router, "/_lookout/api/entries/query?q=USERS").await.json();
    let data = body["data"].as_array().unwrap();
    assert_eq!(data.len(), 1, "{body}");
    assert_eq!(data[0]["id"], "q2");
    assert!(body["next"].is_null());

    let body = get(&router, "/_lookout/api/entries/query?key=nothing").await.json();
    assert_eq!(body["data"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn searching_batches_passes_the_text_to_the_store() {
    let store = sample();
    let router = app(Arc::clone(&store));
    get(&router, "/_lookout/api/batches?q=posts").await;
    let query = store.last_query.lock().unwrap().clone().unwrap();
    assert_eq!(query.search.as_deref(), Some("posts"));
}

#[tokio::test]
async fn draws_a_zero_filled_series_with_a_forecast_and_per_key_series() {
    let router = app(sample());
    let since = (chrono::Utc::now() - chrono::Duration::minutes(10)).to_rfc3339();
    let since = since.replace('+', "%2B");
    let body = get(&router, &format!("/_lookout/api/series/query?since={since}&keys=5&forecast=3"))
        .await
        .json();
    assert_eq!(body["period"], 60, "{body}");
    let points = body["data"].as_array().unwrap();
    assert!((10..=12).contains(&points.len()), "{}", points.len());
    let total: u64 = points.iter().map(|p| p["count"].as_u64().unwrap()).sum();
    assert_eq!(total, 2);
    assert_eq!(body["forecast"].as_array().unwrap().len(), 3);
    assert_eq!(body["keys"].as_object().unwrap().len(), 2);

    let body = get(
        &router,
        &format!("/_lookout/api/series/query?since={since}&key=select%20*%20from%20posts"),
    )
    .await
    .json();
    let total: u64 =
        body["data"].as_array().unwrap().iter().map(|p| p["count"].as_u64().unwrap()).sum();
    assert_eq!(total, 1);
    // The range's percentiles, for that one key: its only run reads exactly.
    for field in ["p50_us", "p90_us", "p95_us", "p99_us"] {
        assert_eq!(body["percentiles"][field], 3_000, "{field}: {body}");
    }
    // And the durations behind them, for the issue page's distribution: one
    // occurrence, in the slot that holds 3 ms.
    let slot = crate::aggregate::slot(3_000).to_string();
    assert_eq!(body["histogram"], serde_json::json!({ slot: 1 }), "{body}");

    let answer =
        get(&router, "/_lookout/api/series/log?period=60&since=2001-01-01T00:00:00Z&keys=99").await;
    assert_eq!(answer.status, StatusCode::UNPROCESSABLE_ENTITY);
    let body = answer.json();
    for field in ["type", "since", "keys"] {
        assert!(body["errors"][field].is_array(), "{field}: {body}");
    }
}

#[tokio::test]
async fn a_family_id_resolves_to_its_key_and_aggregates_carry_it() {
    use crate::entry::family_id;
    let router = app(sample());

    let id = family_id(EntryType::Query, "select * from posts");
    let body = get(&router, &format!("/_lookout/api/families/query/{id}")).await.json();
    assert_eq!(body["data"]["key"], "select * from posts", "{body}");
    assert_eq!(body["data"]["id"], id.as_str());

    // An exception's family, too: from the aggregates or the entries.
    let id = family_id(EntryType::Exception, "Boom@app");
    let body = get(&router, &format!("/_lookout/api/families/exception/{id}")).await.json();
    assert_eq!(body["data"]["key"], "Boom@app", "{body}");

    // The same SQL under another type is another family.
    let other = family_id(EntryType::Cache, "select * from posts");
    let answer = get(&router, &format!("/_lookout/api/families/cache/{other}")).await;
    assert_eq!(answer.status, StatusCode::NOT_FOUND);

    let rows = get(&router, "/_lookout/api/aggregates/query").await.json();
    for row in rows["data"].as_array().unwrap() {
        let key = row["key"].as_str().unwrap();
        assert_eq!(row["family_id"], family_id(EntryType::Query, key).as_str(), "{row}");
    }
}

#[tokio::test]
async fn the_stream_sends_a_batch_stored_after_the_viewer_connected() {
    use futures_util::StreamExt as _;
    use rainier_http::Body;

    let store = sample();
    let router = app(Arc::clone(&store));
    let mut response = router
        .dispatch(Request::builder().method(Method::GET).uri("/_lookout/api/stream").build())
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.header("content-type"), Some("text/event-stream"));
    let Body::Stream(mut frames) = response.take_body() else { panic!("not a stream") };
    async fn next<S, E>(frames: &mut S) -> String
    where
        S: futures_util::Stream<Item = Result<bytes::Bytes, E>> + Unpin,
        E: std::fmt::Debug,
    {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), frames.next())
            .await
            .expect("a frame within five seconds")
            .expect("the stream is open")
            .expect("not an error");
        String::from_utf8_lossy(&frame).into_owned()
    }

    assert!(next(&mut frames).await.starts_with("retry:"), "it opens with a retry interval");
    // The first read learns what is already stored, so this one is new.
    tokio::time::sleep(stream::TAIL_EVERY + std::time::Duration::from_millis(200)).await;
    let fresh = request("b3", "posts.store", 201, 20, Vec::new());
    crate::store::Store::store(&*store, vec![fresh]).await.unwrap();

    // Storing a batch also moves the counts, so a pulse may come first.
    let mut frame = next(&mut frames).await;
    while frame.starts_with("event: pulse") {
        frame = next(&mut frames).await;
    }
    assert!(frame.starts_with("event: batch\ndata: "), "{frame}");
    assert!(frame.contains("\"id\":\"b3\""), "the new batch, and only it: {frame}");
}

#[tokio::test]
async fn the_stream_pulses_when_requests_are_only_counted() {
    // Most requests are counted and not kept. Nothing is stored that a
    // `batch` event could announce, and the dashboard's numbers must still
    // move: that is what the pulse is for.
    use futures_util::StreamExt;
    use rainier_http::Body;

    // Empty, so the only request in this minute is the one counted below:
    // the sample's batches are stamped seconds ago, often this same minute.
    let store = Arc::new(FakeStore::with(Vec::new()));
    let router = app(Arc::clone(&store));
    let mut response = router
        .dispatch(Request::builder().method(Method::GET).uri("/_lookout/api/stream").build())
        .await;
    let Body::Stream(mut frames) = response.take_body() else { panic!("not a stream") };
    async fn next<S, E>(frames: &mut S) -> String
    where
        S: futures_util::Stream<Item = Result<bytes::Bytes, E>> + Unpin,
        E: std::fmt::Debug,
    {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(8), frames.next())
            .await
            .expect("a frame in time")
            .expect("the stream is open")
            .expect("not an error");
        String::from_utf8_lossy(&frame).into_owned()
    }
    assert!(next(&mut frames).await.starts_with("retry:"));

    // The tail's first pulse only learns the numbers; this request is news.
    tokio::time::sleep(stream::TAIL_EVERY + std::time::Duration::from_millis(200)).await;
    let root = entry("c1", "r", EntryType::Request, "GET /posts 200", epoch(), 0, Some(9_000));
    let counted = crate::aggregate::rollup(&[request("c1", "posts.index", 200, 0, vec![root])]);
    crate::store::Store::store_counting(&*store, Vec::new(), counted).await.unwrap();

    let frame = next(&mut frames).await;
    assert!(frame.starts_with("event: pulse\ndata: "), "{frame}");
    let data: Value =
        serde_json::from_str(frame.trim_start_matches("event: pulse\ndata: ").trim()).unwrap();
    // Summed, not the last bucket: when the minute turns between counting and
    // the pulse, the request is in the previous minute, which the pulse carries.
    let minute = data["request"]["60"].as_array().expect("the minute buckets");
    let counted: u64 = minute.iter().map(|b| b["count"].as_u64().unwrap_or(0)).sum();
    assert_eq!(counted, 1, "the counted request: {data}");
    assert!(data["request"]["600"].is_array() && data["job"]["3600"].is_array(), "{data}");
    assert!(
        !store.batches.lock().unwrap().iter().any(|b| b.id == "c1"),
        "counted, not kept: no batch event could have said so"
    );
}

#[tokio::test]
async fn the_stream_is_a_404_when_streaming_is_off() {
    let mut config = LookoutConfig::default().enabled(true);
    config.dashboard.stream = false;
    let mut router = Router::new();
    router.merge(routes(sample(), Arc::new(config), LookoutGate::allow_all(), "production"));
    let router = router.compile(&Container::new()).unwrap();
    assert_eq!(get(&router, "/_lookout/api/stream").await.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn reports_insights_with_the_thresholds_it_used() {
    let router = app(sample());
    let body = get(&router, "/_lookout/api/insights?window=30&min=1&change=10").await.json();
    assert_eq!(body["enabled"], true, "{body}");
    assert_eq!(body["config"]["window_minutes"], 30);
    assert_eq!(body["config"]["min_count"], 1);
    // Everything in the sample is new in this window.
    let data = body["data"].as_array().unwrap();
    assert!(
        data.iter().any(|i| i["kind"] == "new" && i["key"] == "Boom@app" && i["severity"] == "bad"),
        "{body}"
    );

    let answer = get(&router, "/_lookout/api/insights?window=0&change=x").await;
    assert_eq!(answer.status, StatusCode::UNPROCESSABLE_ENTITY);
}

async fn post(router: &CompiledRouter, uri: &str, header: bool, body: &str) -> Answer {
    let mut builder =
        Request::builder().method(Method::POST).uri(uri).header("content-type", "application/json");
    if header {
        builder = builder.header("x-lookout", "1");
    }
    let mut response = router.dispatch(builder.body(body.to_string()).build()).await;
    let bytes = response.take_body().collect().await.unwrap();
    Answer {
        status: response.status(),
        text: String::from_utf8_lossy(&bytes).into_owned(),
        response,
    }
}

#[tokio::test]
async fn lists_cases_and_closes_one() {
    let router = app(sample());
    let body = get(&router, "/_lookout/api/cases").await.json();
    let data = body["data"].as_array().unwrap();
    assert_eq!(data.len(), 1, "{body}");
    let id = data[0]["id"].as_str().unwrap().to_string();
    assert_eq!(data[0]["key"], "Boom@app");
    assert_eq!(data[0]["status"], "open");

    let answer = get(&router, &format!("/_lookout/api/cases/{id}")).await;
    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(get(&router, "/_lookout/api/cases/nope").await.status, StatusCode::NOT_FOUND);

    let uri = format!("/_lookout/api/cases/{id}");
    let refused = post(&router, &uri, false, r#"{"action":"resolve"}"#).await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    let bad = post(&router, &uri, true, r#"{"action":"explode"}"#).await;
    assert_eq!(bad.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", bad.text);
    let done = post(&router, &uri, true, r#"{"action":"resolve","note":"fixed"}"#).await;
    assert_eq!(done.status, StatusCode::OK, "{}", done.text);
    assert_eq!(done.json()["data"]["status"], "resolved");

    let open = get(&router, "/_lookout/api/cases?status=open").await.json();
    assert_eq!(open["data"].as_array().unwrap().len(), 0);
    let bad = get(&router, "/_lookout/api/cases?status=sideways&severity=huge").await;
    assert_eq!(bad.status, StatusCode::UNPROCESSABLE_ENTITY);
}

// --- the service-health API ---------------------------------------------------

/// A request's root entry as the subscriber records it, `ago_s` seconds ago.
fn served(method: &str, status: u16, route: &str, ago_s: i64, us: u64) -> crate::entry::Entry {
    let mut e = entry("h", "r", EntryType::Request, route, Utc::now(), 0, Some(us));
    e.occurred_at = Utc::now() - chrono::Duration::seconds(ago_s);
    e.content = serde_json::json!({
        "http.request.method": method,
        "http.response.status_code": status,
        "rainier.route.name": route,
    });
    e
}

use chrono::Utc;

/// Ten requests — eight fine, one failing, one refused — a failed job and an
/// exception, all counted and none kept, as in production.
fn traffic() -> Arc<FakeStore> {
    let mut entries: Vec<crate::entry::Entry> =
        (0..8).map(|i| served("GET", 200, "posts.index", 30 + i, 100_000)).collect();
    entries.push(served("GET", 500, "posts.show", 40, 2_000_000));
    entries.push(served("POST", 422, "posts.store", 50, 50_000));
    let mut job = entry("j", "j", EntryType::Job, "SendInvoice", Utc::now(), 0, Some(10));
    job.tags.push("failed".into());
    entries.push(job);
    entries.push(entry("x", "x", EntryType::Exception, "Boom@app", Utc::now(), 0, None));
    let store = FakeStore::default();
    *store.aggregates.lock().unwrap() = crate::aggregate::rollup_entries(&entries);
    Arc::new(store)
}

#[tokio::test]
async fn health_judges_every_request_counted() {
    let router = app(traffic());
    let answer = get(&router, "/_lookout/api/health").await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.text);
    let data = &answer.json()["data"];
    let requests = &data["requests"];
    assert_eq!(requests["count"], 10, "{data}");
    assert_eq!(requests["errors_5xx"], 1);
    assert_eq!(requests["client_errors_4xx"], 1);
    assert_eq!(requests["error_rate"], 0.1);
    assert_eq!(requests["apdex_t_ms"], 300);
    // Nine at or under 300 ms, one at 2 s (over 4T): (9 + 0/2) / 10.
    assert_eq!(requests["apdex"], 0.9);
    assert!(requests["p50_us"].as_u64().unwrap() > 90_000);
    assert_eq!(data["status"], "failing", "10% is failing");
    assert_eq!(data["jobs"], serde_json::json!({ "count": 1, "failed": 1 }));
    assert_eq!(data["exceptions"]["count"], 1);
    assert_eq!(data["window"]["period"], 60, "an hour in minutes");
    assert!(data["series"].as_array().unwrap().len() >= 60);
    assert_eq!(data["top_routes"][0]["route"], "posts.index");
    assert_eq!(data["top_routes"][0]["count"], 8);
    assert_eq!(data["slowest_routes"][0]["route"], "posts.show");
    assert_eq!(data["failing_routes"].as_array().unwrap().len(), 1);
    assert_eq!(data["failing_routes"][0]["route"], "posts.show");

    // A tighter target, and a window with nothing in it.
    let strict = get(&router, "/_lookout/api/health?apdex_t_ms=10").await.json();
    assert_eq!(strict["data"]["requests"]["apdex"], 0.0);
    let idle = get(&router, "/_lookout/api/health?since=7d&until=6d").await.json();
    assert_eq!(idle["data"]["status"], "idle", "{idle}");
    assert_eq!(idle["data"]["requests"]["apdex"], Value::Null);
    assert_eq!(idle["data"]["window"]["period"], 3600, "a day in hours");
}

#[tokio::test]
async fn requests_group_filter_and_order() {
    let router = app(traffic());
    let body = get(&router, "/_lookout/api/requests").await.json();
    let rows = body["data"]["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 3, "{body}");
    assert_eq!(rows[0]["route"], "posts.index");
    assert_eq!(rows[0]["method"], Value::Null, "grouped away");
    assert_eq!(body["data"]["totals"]["count"], 10);
    assert_eq!(body["data"]["totals"]["route"], Value::Null);
    assert!(body["data"].get("series").is_none());

    let body = get(&router, "/_lookout/api/requests?group=method,status&order=error_rate&series=1")
        .await
        .json();
    let rows = body["data"]["rows"].as_array().unwrap();
    assert_eq!(rows[0]["method"], "GET");
    assert_eq!(rows[0]["status"], "5xx");
    assert_eq!(rows[0]["error_rate"], 1.0);
    assert_eq!(rows[0]["route"], Value::Null);
    assert!(body["data"]["series"].as_array().unwrap().len() >= 60);

    let body =
        get(&router, "/_lookout/api/requests?route=posts.*&method=post&limit=1").await.json();
    assert_eq!(body["data"]["rows"][0]["route"], "posts.store", "{body}");
    assert_eq!(body["data"]["totals"]["count"], 1);
    let body = get(&router, "/_lookout/api/requests?q=SHOW&status=5xx&order=p95").await.json();
    assert_eq!(body["data"]["totals"]["errors_5xx"], 1);
    assert_eq!(body["data"]["rows"][0]["max_us"], 2_000_000);
}

#[tokio::test]
async fn bad_health_parameters_are_a_422_in_rainiers_shape() {
    let router = app(traffic());
    let answer = get(
        &router,
        "/_lookout/api/requests?group=route,planet&order=loudest&status=6xx&limit=501&since=yesterday&series=perhaps",
    )
    .await;
    assert_eq!(answer.status, StatusCode::UNPROCESSABLE_ENTITY);
    let body = answer.json();
    assert_eq!(body["message"], "The given data was invalid.");
    for field in ["group", "order", "status", "limit", "since", "series"] {
        assert!(body["errors"][field].is_array(), "{field}: {body}");
    }
    for uri in [
        "/_lookout/api/health?apdex_t_ms=0",
        "/_lookout/api/health?period=61",
        "/_lookout/api/health?since=1h&until=2h",
        "/_lookout/api/health?since=60d&period=60",
        "/_lookout/api/rum/vitals?group=planet",
        "/_lookout/api/rum/vitals?order=vibes&limit=201",
        "/_lookout/api/rum/errors?limit=0",
    ] {
        assert_eq!(get(&router, uri).await.status, StatusCode::UNPROCESSABLE_ENTITY, "{uri}");
    }
}

#[tokio::test]
async fn accepted_page_reports_roll_up_into_vitals_and_errors() {
    let mut config = LookoutConfig::default().enabled(true);
    config.rum.enabled = true;
    config.rum.origins = vec!["https://app.example.com".into()];
    config.rum.secret = b"k".to_vec();
    config.rum.country_header = Some("cf-ipcountry".into());
    let store = Arc::new(crate::store::memory::MemoryStore::new(10));
    let mut router = Router::new();
    router.merge(routes(store, Arc::new(config), LookoutGate::allow_all(), "production"));
    let router = router.compile(&Container::new()).unwrap();

    let report = |proof: &str| {
        serde_json::json!({
            "view": "0123456789abcdef0123456789abcdef",
            "proof": proof,
            "page": { "url": "https://app.example.com/posts/42" },
            "device": { "connection": { "type": "3g" } },
            "activity": {
                "sampled": true,
                "load": { "ttfb": 200.0, "load": 1500.0 },
                "vitals": { "fcp": 400.0, "lcp": 1800.0, "cls": 0.1, "inp": 120.0 },
                "settled": true,
                "landing": "/posts/42",
                "errors": [{ "message": "x is undefined", "source": "https://app.example.com/app.js", "line": 7, "page": "/posts/42" }],
                "long_tasks": { "count": 2 },
            }
        })
        .to_string()
    };
    let post = |body: String| {
        Request::builder()
            .method(Method::POST)
            .uri("/_lookout/rum")
            .header("origin", "https://app.example.com")
            .header("cf-ipcountry", "nz")
            .header(
                "user-agent",
                "Mozilla/5.0 (iPhone; CPU iPhone OS 18_1 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.1 Mobile/15E148 Safari/604.1",
            )
            .body(body)
            .build()
    };
    // A report that cannot prove its page talked to this application counts
    // for nothing.
    router.dispatch(post(report("forged.0000000000000000"))).await;
    let empty = get(&router, "/_lookout/api/rum/vitals").await.json();
    assert_eq!(empty["data"]["totals"]["views"], 0, "{empty}");
    assert_eq!(empty["data"]["enabled"], true);

    let proof = crate::rum::token(b"k", "trace-1");
    assert_eq!(router.dispatch(post(report(&proof))).await.status().as_u16(), 204);

    let vitals = get(&router, "/_lookout/api/rum/vitals?group=country").await.json();
    let row = &vitals["data"]["rows"][0];
    assert_eq!(row["key"], "NZ", "{vitals}");
    assert_eq!(row["views"], 1);
    assert_eq!(row["js_errors"], 1);
    assert_eq!(row["long_tasks"], 2);
    assert!(row["lcp_p75_ms"].as_f64().unwrap() > 1700.0);
    let by_page = get(&router, "/_lookout/api/rum/vitals?device=mobile&connection=3g").await.json();
    assert_eq!(by_page["data"]["rows"][0]["key"], "/posts/{id}", "{by_page}");
    let none = get(&router, "/_lookout/api/rum/vitals?browser=Firefox").await.json();
    assert_eq!(none["data"]["rows"].as_array().unwrap().len(), 0);

    let errors = get(&router, "/_lookout/api/rum/errors?country=NZ").await.json();
    let row = &errors["data"]["rows"][0];
    assert_eq!(row["message"], "x is undefined", "{errors}");
    assert_eq!(row["file"], "https://app.example.com/app.js");
    assert_eq!(row["line"], 7);
    assert_eq!(row["count"], 1);
    assert_eq!(row["pages"], 1);
    assert_eq!(row["browsers"], serde_json::json!([["Safari", 1]]));
    assert!(row["first_seen"].is_string() && row["last_seen"].is_string());
}

// --- readers -----------------------------------------------------------------

const TOKEN: &str = "Bearer service-token";

fn readers() -> LookoutGate {
    LookoutGate::deny_all().with_readers(|request| {
        let ok = request.header("authorization") == Some(TOKEN);
        Box::pin(async move { ok })
    })
}

const READER_GETS: &[&str] = &[
    "/_lookout/api/health",
    "/_lookout/api/requests?group=route,status",
    "/_lookout/api/rum/vitals",
    "/_lookout/api/rum/errors",
    "/_lookout/api/aggregates/request",
    "/_lookout/api/series/request",
    "/_lookout/api/cases",
    "/_lookout/api/status",
];

#[test]
fn the_reader_routes_are_the_contracts() {
    let declared: Vec<String> = READER_GETS
        .iter()
        .map(|u| u.trim_start_matches("/_lookout").split('?').next().unwrap().to_string())
        .collect();
    let expected: Vec<String> =
        READER_ROUTES.iter().map(|r| r.replace("{type}", "request")).collect();
    assert_eq!(declared, expected);
}

#[tokio::test]
async fn a_reader_may_call_the_read_only_api_and_nothing_else() {
    let router = app_with(traffic(), readers(), "production");
    let auth = [("authorization", TOKEN)];
    for uri in READER_GETS {
        let answer = send(&router, Method::GET, uri, &auth).await;
        assert_eq!(answer.status, StatusCode::OK, "{uri}: {}", answer.text);
        assert_eq!(answer.header("cache-control"), Some("no-store"), "{uri}");
    }

    let mut nothing = Router::new();
    nothing.get("/elsewhere", || async { "x" });
    let nothing = nothing.compile(&Container::new()).unwrap();
    // Without the token, or with another, the reader routes are not there.
    for uri in READER_GETS {
        for headers in [&[][..], &[("authorization", "Bearer guess")][..]] {
            let answer = send(&router, Method::GET, uri, headers).await;
            assert_eq!(answer.status, StatusCode::NOT_FOUND, "{uri}");
            assert_eq!(answer.text, get(&nothing, uri).await.text, "{uri}");
        }
    }
    // With it, everything else is not there either: the shell, batches with
    // their bodies, entries, a case, the stream, and every write.
    for (method, uri) in [
        (Method::GET, "/_lookout"),
        (Method::GET, "/_lookout/requests"),
        (Method::GET, "/_lookout/assets/lookout.js"),
        (Method::GET, "/_lookout/api/batches"),
        (Method::GET, "/_lookout/api/batches/b1"),
        (Method::GET, "/_lookout/api/entries/query"),
        (Method::GET, "/_lookout/api/families/request/0000000000000000"),
        (Method::GET, "/_lookout/api/models"),
        (Method::GET, "/_lookout/api/insights"),
        (Method::GET, "/_lookout/api/cases/abc"),
        (Method::GET, "/_lookout/api/stream"),
        (Method::POST, "/_lookout/api/cases/abc"),
        (Method::DELETE, "/_lookout/api/batches"),
        (Method::POST, "/_lookout/api/health"),
        (Method::DELETE, "/_lookout/api/cases"),
    ] {
        let headers = [("authorization", TOKEN), ("x-lookout", "1")];
        let answer = send(&router, method.clone(), uri, &headers).await;
        assert_eq!(answer.status, StatusCode::NOT_FOUND, "{method} {uri}");
        assert_eq!(
            answer.text,
            send(&nothing, method.clone(), uri, &[]).await.text,
            "{method} {uri}"
        );
    }
}

#[tokio::test]
async fn readers_are_off_unless_registered_and_viewers_keep_everything() {
    // No readers: the token opens nothing.
    let router = app_with(traffic(), LookoutGate::deny_all(), "production");
    let answer =
        send(&router, Method::GET, "/_lookout/api/health", &[("authorization", TOKEN)]).await;
    assert_eq!(answer.status, StatusCode::NOT_FOUND);
    // A viewer reaches the reader routes as before, readers or not.
    let viewer = LookoutGate::allow_all().with_readers(|_| Box::pin(async { false }));
    let router = app_with(traffic(), viewer, "production");
    for uri in
        READER_GETS.iter().chain(&["/_lookout", "/_lookout/api/batches", "/_lookout/api/insights"])
    {
        assert_eq!(get(&router, uri).await.status, StatusCode::OK, "{uri}");
    }
}

// --- traces across services ---------------------------------------------------

const TRACE_ID: &str = "4bf92f3577b34da6a3ce929d0e0e4736";

/// A batch that answered a call from another service and made one of its own.
fn distributed() -> Arc<FakeStore> {
    let e = epoch();
    let mut root =
        entry("b9", "r", EntryType::Request, "GET /orders/{order} 200", e, 0, Some(9_000));
    root.span_id = Some("b9".into());
    root.content = serde_json::json!({
        "rainier.trace.parent_span_id": "00f067aa0ba902b7",
        "http.request.header.authorization": "Bearer secret-token",
    });
    let mut call =
        entry("b9", "h1", EntryType::HttpClient, "GET http://billing/x 200", e, 1, Some(4_000));
    call.parent_span_id = Some("b9".into());
    call.content = serde_json::json!({
        "server.address": "billing",
        "http.response.status_code": 200,
        "rainier.trace.span_id": "b7ad6b7169203331",
    });
    let mut batch = request("b9", "orders.show", 200, 5, vec![root, call]);
    batch.trace_id = TRACE_ID.into();
    batch.user_id = Some("42".into());
    batch.tags = vec!["trace:distributed".into(), "trace:kept".into(), "user:42".into()];
    let store = FakeStore::default();
    store.batches.lock().unwrap().push(batch);
    Arc::new(store)
}

fn aggregating(store: Arc<FakeStore>, gate: LookoutGate) -> CompiledRouter {
    let mut config = LookoutConfig::default().enabled(true);
    config.request_tracing.enabled = true;
    config.request_tracing.service = Some("orders".into());
    let mut router = Router::new();
    router.merge(routes(store, Arc::new(config), gate, "production"));
    router.compile(&Container::new()).unwrap()
}

#[tokio::test]
async fn a_trace_is_its_shape_and_never_its_content() {
    let router = aggregating(distributed(), readers());
    let auth = [("authorization", TOKEN)];

    let answer =
        send(&router, Method::GET, &format!("/_lookout/api/traces/{TRACE_ID}"), &auth).await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.text);
    let body: serde_json::Value = serde_json::from_str(&answer.text).unwrap();
    let trace = &body["data"];
    assert_eq!(trace["service"], "orders");
    let batch = &trace["batches"][0];
    assert_eq!(batch["root_span_id"], "b9");
    assert_eq!(batch["remote_parent_span_id"], "00f067aa0ba902b7", "the call it answered");
    assert_eq!(batch["kept"], true);
    let call = &batch["spans"][1]["call"];
    assert_eq!(call["span_id"], "b7ad6b7169203331", "the call it made");
    assert_eq!(call["host"], "billing");
    assert!(!answer.text.contains("secret-token"), "no content: {}", answer.text);
    assert!(!answer.text.contains("\"42\""), "no user: {}", answer.text);

    let listed = send(&router, Method::GET, "/_lookout/api/traces?kept=1", &auth).await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.text);
    let body: serde_json::Value = serde_json::from_str(&listed.text).unwrap();
    assert_eq!(body["data"][0]["trace_id"], TRACE_ID);
    assert_eq!(body["data"][0]["kept"], true);

    let broken = send(&router, Method::GET, "/_lookout/api/traces/a%20b", &auth).await;
    assert_eq!(broken.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn without_request_tracing_a_reader_cannot_read_a_trace() {
    let router = app_with(distributed(), readers(), "production");
    let auth = [("authorization", TOKEN)];
    for uri in ["/_lookout/api/traces", &format!("/_lookout/api/traces/{TRACE_ID}")] {
        let answer = send(&router, Method::GET, uri, &auth).await;
        assert_eq!(answer.status, StatusCode::NOT_FOUND, "{uri}");
    }
    // A viewer still can.
    let viewer = app(distributed());
    assert_eq!(
        get(&viewer, &format!("/_lookout/api/traces/{TRACE_ID}")).await.status,
        StatusCode::OK
    );
}
