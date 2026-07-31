//! The test harness, exercised the way an application would use it.
//!
//! These boot a real application, send real requests through the real kernel,
//! and assert on what comes back — so if the harness stops matching the
//! framework, this fails rather than every downstream test suite.

use std::sync::Arc;

use rainier_container::Application;
use rainier_framework::testing::TestApp;
use rainier_framework::Rainier;
use rainier_http::extract::Json;
use rainier_http::{Method, Request, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct UserView {
    id: u32,
    name: String,
}

/// Long enough and repetitive enough to be worth compressing — which is what
/// a JSON list actually looks like.
fn long_list() -> Vec<UserView> {
    (0..200).map(|id| UserView { id, name: format!("Ada Lovelace {id}") }).collect()
}

async fn app() -> Arc<Application> {
    Rainier::new(".")
        .without_tracing()
        .with_routes(|router| {
            router.get("/health/ready", || async {
                rainier_http::Response::json(&json!({
                    "database": { "status": "ok", "latency_ms": 3 },
                }))
            });
            router.get("/api/me", || async {
                rainier_http::Response::json(&UserView { id: 7, name: "Ada".into() })
            });
            router.get("/api/list", || async { rainier_http::Response::json(&long_list()) });
            router.get("/missing", || async {
                rainier_http::Response::new(StatusCode::NOT_FOUND).with_body("gone")
            });
            router.post("/echo", |Json(body): Json<serde_json::Value>| async move {
                rainier_http::Response::json(&body)
            });
            router.get("/whoami", |request: Arc<Request>| async move {
                let token = request.header("authorization").unwrap_or("anonymous").to_string();
                rainier_http::Response::ok(token)
            });
        })
        .boot()
        .await
        .expect("boots")
}

#[tokio::test]
async fn the_documented_three_lines_work() {
    let app = TestApp::new(app().await).unwrap();

    let response = app.get("/health/ready").await;

    response.assert_ok().assert_json_path("database.status", "ok");

    let latency = response.json()["database"]["latency_ms"].as_u64();
    assert_eq!(latency, Some(3));
}

#[tokio::test]
async fn a_response_deserialises_into_your_own_type() {
    let app = TestApp::new(app().await).unwrap();

    let user: UserView = app.get("/api/me").await.assert_ok().json_as();

    assert_eq!(user, UserView { id: 7, name: "Ada".into() });
}

#[tokio::test]
async fn a_body_makes_the_round_trip() {
    let app = TestApp::new(app().await).unwrap();

    app.post("/echo", &json!({ "title": "Hello" }))
        .await
        .assert_ok()
        .assert_json_path("title", "Hello");
}

#[tokio::test]
async fn default_headers_ride_along_on_every_request() {
    let app = TestApp::new(app().await).unwrap().with_token("secret-token");

    app.get("/whoami").await.assert_ok().assert_contains("Bearer secret-token");
}

#[tokio::test]
async fn the_status_assertions_read_the_real_status() {
    let app = TestApp::new(app().await).unwrap();

    app.get("/missing").await.assert_not_found();
    // And a route that is not declared at all — the router's own 404, not the
    // handler's, so this proves the request went through routing.
    app.get("/no-such-route").await.assert_not_found();
}

#[tokio::test]
async fn a_request_built_by_hand_still_goes_through_the_kernel() {
    let app = TestApp::new(app().await).unwrap();

    let response = app.send(app.request(Method::GET, "/api/me").build()).await;

    response.assert_ok().assert_json_path("name", "Ada");
}

#[tokio::test]
async fn the_harness_scopes_the_facades_to_itself() {
    // Two applications in one process. Booting the second replaces the global
    // facade application — but this thread is inside the first's scope, so
    // facades here still resolve through the first.
    //
    // Asserted through the scope rather than through the global, because the
    // global belongs to the whole test binary: every other test in this file
    // boots an application too, and they run on their own threads at the same
    // time as this one.
    let first = TestApp::new(app().await).unwrap();
    let first_app = Arc::clone(first.app());

    let _second = app().await;

    assert!(
        Arc::ptr_eq(&rainier_container::facade_application(), &first_app),
        "a scope in effect must win over a later global"
    );
    assert!(rainier_container::scoped_facade_application().is_some());

    drop(first);

    // With the scope gone, resolution falls back to the global — whichever
    // application booted last, which is certainly not this one.
    assert!(rainier_container::scoped_facade_application().is_none());
    assert!(!Arc::ptr_eq(&rainier_container::facade_application(), &first_app));
}

#[tokio::test]
async fn a_configured_request_timeout_reaches_the_kernel() {
    // The point of the setting: `SERVER_REQUEST_TIMEOUT` in the environment
    // has to produce a 408 for a handler that overruns, without the
    // application registering any middleware itself.
    let app = Rainier::new(".")
        .without_tracing()
        .configure(|config| {
            config.set(rainier_framework::keys::SERVER_REQUEST_TIMEOUT_SECS, 1u64).unwrap();
        })
        .with_routes(|router| {
            router.get("/slow", || async {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                rainier_http::Response::ok("far too late")
            });
            router.get("/fast", || async { rainier_http::Response::ok("in time") });
        })
        .boot()
        .await
        .expect("boots");

    let app = TestApp::new(app).unwrap();

    app.get("/slow").await.assert_status(StatusCode::REQUEST_TIMEOUT);
    app.get("/fast").await.assert_ok();
}

#[tokio::test]
async fn no_timeout_is_configured_by_default() {
    // Adding one silently would cancel work that used to finish, in an upgrade
    // nobody read the changelog for.
    let app = TestApp::new(app().await).unwrap();

    assert_eq!(
        app.resolve::<rainier_framework::config::Config>()
            .unwrap()
            .get_or(rainier_framework::keys::SERVER_REQUEST_TIMEOUT_SECS, 99u64),
        0
    );
}

#[tokio::test]
async fn compression_is_off_until_it_is_configured_on() {
    let plain = TestApp::new(app().await).unwrap();
    let big = plain.get("/api/list").await;
    assert_eq!(big.header("content-encoding"), None);

    let compressed = Rainier::new(".")
        .without_tracing()
        .configure(|config| {
            config.set(rainier_framework::keys::SERVER_COMPRESSION, true).unwrap();
        })
        .with_routes(|router| {
            router.get("/api/list", || async { rainier_http::Response::json(&long_list()) });
        })
        .boot()
        .await
        .expect("boots");

    let response = TestApp::new(compressed)
        .unwrap()
        .send(
            rainier_http::Request::builder()
                .method(Method::GET)
                .uri("/api/list")
                .header("accept-encoding", "gzip")
                .build(),
        )
        .await;

    response.assert_ok().assert_header("content-encoding", "gzip");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_harness_works_on_a_multi_threaded_runtime() {
    let app = TestApp::new(app().await).unwrap();

    for _ in 0..20 {
        app.get("/api/me").await.assert_ok().assert_json_path("name", "Ada");
    }
}
