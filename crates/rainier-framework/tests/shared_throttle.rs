//! A rate limit counted somewhere both replicas can see it.
//!
//! The unit tests assert the counting. This asserts the wiring that makes it
//! matter: that `limits::shared` reaches the store the bootstrap bound, and
//! that two applications sharing one cache also share one allowance — which is
//! the entire difference between a control and a decoration.

use std::sync::Arc;
use std::time::Duration;

use rainier_cache::{Cache, CacheManager, MemoryCache};
use rainier_framework::limits;
use rainier_framework::middleware::ThrottleRequests;
use rainier_framework::prelude::*;
use rainier_framework::testing::TestApp;
use serde_json::json;

/// One application, optionally sharing `cache` with another.
async fn app(cache: Arc<dyn Cache>) -> TestApp {
    let booted = Rainier::new(".")
        .without_tracing()
        .with_cache(CacheManager::new(cache))
        .with_routes(|router| {
            router.post("/login", || async { Response::ok("in") }).middleware(limits::shared(
                ThrottleRequests::per_minute(2)
                    .named("login")
                    .keyed_by(|request| request.input("email")),
            ));
        })
        .boot()
        .await
        .expect("boots");

    TestApp::new(booted).expect("a kernel")
}

fn login(email: &str) -> serde_json::Value {
    json!({ "email": email, "password": "correct horse" })
}

#[tokio::test]
async fn the_limit_is_enforced_per_key() {
    let app = app(Arc::new(MemoryCache::new()) as Arc<dyn Cache>).await;

    app.post("/login", &login("ada@example.com")).await.assert_ok();
    app.post("/login", &login("ada@example.com")).await.assert_ok();
    app.post("/login", &login("ada@example.com"))
        .await
        .assert_status(StatusCode::TOO_MANY_REQUESTS);

    // A different account is untouched — which is the point of keying on what
    // was submitted rather than on the address.
    app.post("/login", &login("grace@example.com")).await.assert_ok();
}

#[tokio::test]
async fn two_replicas_share_one_allowance() {
    // The failure this whole item exists to fix. With a per-process counter,
    // the third attempt below succeeds, because it arrives at the replica that
    // has only seen one.
    let cache: Arc<dyn Cache> = Arc::new(MemoryCache::new());
    let first = app(Arc::clone(&cache)).await;
    let second = app(Arc::clone(&cache)).await;

    first.post("/login", &login("ada@example.com")).await.assert_ok();
    second.post("/login", &login("ada@example.com")).await.assert_ok();

    second
        .post("/login", &login("ada@example.com"))
        .await
        .assert_status(StatusCode::TOO_MANY_REQUESTS);
    first
        .post("/login", &login("ada@example.com"))
        .await
        .assert_status(StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn a_per_process_throttle_does_not_share_and_that_is_the_difference() {
    // The same two applications, with the throttle left on its own counter.
    // Kept as a test rather than a comment, because it is the behaviour the
    // shared one is being contrasted with — and if it ever changed, the
    // contrast above would be measuring nothing.
    let counted_here = ThrottleRequests::per_minute(2).named("login");
    assert!(!counted_here.is_shared());

    let boot = |name: &'static str| async move {
        let booted = Rainier::new(".")
            .without_tracing()
            .with_routes(move |router| {
                router.post("/login", move || async move { Response::ok(name) }).middleware(
                    ThrottleRequests::per_minute(2)
                        .named("login")
                        .keyed_by(|request| request.input("email")),
                );
            })
            .boot()
            .await
            .expect("boots");

        TestApp::new(booted).expect("a kernel")
    };

    let first = boot("first").await;
    let second = boot("second").await;

    for _ in 0..2 {
        first.post("/login", &login("ada@example.com")).await.assert_ok();
    }
    first
        .post("/login", &login("ada@example.com"))
        .await
        .assert_status(StatusCode::TOO_MANY_REQUESTS);

    // The second replica has its own allowance, entirely unspent.
    second.post("/login", &login("ada@example.com")).await.assert_ok();
}

#[tokio::test]
async fn the_headers_say_what_is_left() {
    let app = app(Arc::new(MemoryCache::new()) as Arc<dyn Cache>).await;

    let first = app.post("/login", &login("ada@example.com")).await;
    assert_eq!(first.header("x-ratelimit-limit"), Some("2"));
    assert_eq!(first.header("x-ratelimit-remaining"), Some("1"));

    app.post("/login", &login("ada@example.com")).await;

    let refused = app.post("/login", &login("ada@example.com")).await;
    assert_eq!(refused.header("x-ratelimit-remaining"), Some("0"));
    assert!(refused.header("retry-after").is_some(), "a 429 has to say when to come back");
}

#[tokio::test]
async fn the_window_rolls_over() {
    let cache: Arc<dyn Cache> = Arc::new(MemoryCache::new());
    let booted = Rainier::new(".")
        .without_tracing()
        .with_cache(CacheManager::new(Arc::clone(&cache)))
        .with_routes(|router| {
            router.post("/login", || async { Response::ok("in") }).middleware(limits::shared(
                ThrottleRequests::new(1, Duration::from_millis(60))
                    .named("login")
                    .keyed_by(|request| request.input("email")),
            ));
        })
        .boot()
        .await
        .expect("boots");

    let app = TestApp::new(booted).expect("a kernel");

    app.post("/login", &login("ada@example.com")).await.assert_ok();
    app.post("/login", &login("ada@example.com"))
        .await
        .assert_status(StatusCode::TOO_MANY_REQUESTS);

    tokio::time::sleep(Duration::from_millis(90)).await;

    app.post("/login", &login("ada@example.com")).await.assert_ok();
}
