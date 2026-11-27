//! Lookout's request tracing turns request tagging on by itself: one switch,
//! and every request is tagged — including with the id a frontend chose.
//!
//! Its own binary: booting installs the process-wide bus.

#![cfg(feature = "lookout")]

use rainier_framework::lookout::config::RequestTracing;
use rainier_framework::testing::TestApp;
use rainier_framework::Rainier;

async fn app(tracing: bool) -> TestApp {
    let app = Rainier::new(".")
        .without_facades()
        .without_tracing()
        .with_lookout(move |config| {
            config
                .enabled(true)
                .request_tracing(RequestTracing { enabled: tracing, ..RequestTracing::default() })
        })
        .with_routes(|router| {
            router.get("/", || async { "home" }).name("home");
        })
        .boot()
        .await
        .expect("boots");
    TestApp::new(app).unwrap()
}

#[tokio::test]
async fn request_tracing_tags_every_request_without_a_second_switch() {
    let on = app(true).await;
    let response = on.get("/").await;
    response.assert_ok();
    let header = response.header("traceparent").expect("tagged");
    assert_eq!(header.split('-').nth(1).map(str::len), Some(32), "{header}");

    let off = app(false).await;
    off.get("/").await.assert_header_missing("traceparent");
}
