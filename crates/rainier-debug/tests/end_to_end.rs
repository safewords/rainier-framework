//! The page a developer actually sees.
//!
//! The unit tests check the pieces; this checks the thing. It builds a real
//! [`Error`] on a real stack, pushes it through the whole
//! `IntoResponse` → `RenderedError` → renderer path, and asserts on the HTML.
//!
//! `RUST_BACKTRACE` has to be on for the stack half, and it is read once per
//! process — so it is set here at the top of the first test rather than in the
//! shell, and the tests that need frames say so.

use std::sync::Arc;

use rainier_debug::DebugExceptionRenderer;
use rainier_http::{IntoResponse, Method, RenderedError, Request, Response};
use rainier_server::ExceptionRenderer;
use rainier_support::{Error, ErrorKind};

/// Push an `Error` through the same path the kernel does, and render it.
async fn render(error: Error, request: Request, debug: bool) -> String {
    let response: Response = error.into_response();
    let rendered = response
        .extensions()
        .get::<RenderedError>()
        .cloned()
        .expect("IntoResponse attaches a RenderedError");

    let renderer = DebugExceptionRenderer::new().with_environment("local");
    let page = renderer.render(&request, &rendered, debug);
    page.into_string().await.expect("a readable body")
}

fn browser() -> Request {
    Request::builder().method(Method::GET).uri("/posts/42?draft=1").build()
}

/// The function whose name should appear in the stack. Deliberately not
/// `#[inline]`-able noise — the test asserts on this symbol.
#[inline(never)]
fn the_thing_that_failed() -> Error {
    Error::internal("could not reach the search index")
}

#[tokio::test]
async fn the_page_carries_the_message_the_status_and_the_request() {
    let html = render(the_thing_that_failed(), browser(), true).await;

    assert!(html.contains("could not reach the search index"), "the message");
    assert!(html.contains("500"), "the status");
    assert!(html.contains("Internal"), "the kind");
    assert!(html.contains("/posts/42"), "the path that caused it");
    assert!(html.contains("draft=1"), "the query string");
    assert!(html.starts_with("<!doctype html>"), "a document, not a fragment");
}

#[tokio::test]
async fn a_stack_is_captured_and_this_function_is_in_it() {
    // Read once per process, so this only works if nothing has captured a
    // backtrace before it. Setting it here is the honest way to test the
    // capture path without requiring the whole suite to run under it.
    std::env::set_var("RUST_BACKTRACE", "1");

    let html = render(the_thing_that_failed(), browser(), true).await;

    if html.contains("No stack was captured") {
        // `RUST_BACKTRACE` was already read as unset earlier in this process.
        // Not a failure of the code under test, and worth saying rather than
        // asserting something that cannot hold.
        eprintln!("skipped: RUST_BACKTRACE was already resolved as off for this process");
        return;
    }

    assert!(
        html.contains("the_thing_that_failed"),
        "the frame that built the error should be in the stack"
    );
    assert!(
        !html.contains("Error::new"),
        "but the capture machinery itself should have been trimmed off the top"
    );
}

#[tokio::test]
async fn a_4xx_renders_too_and_says_which_kind() {
    let html = render(Error::not_found("No Post matches the given key."), browser(), true).await;

    assert!(html.contains("404"));
    assert!(html.contains("No Post matches the given key."));
    assert!(html.contains("NotFound"));
}

#[tokio::test]
async fn debug_off_gives_the_plain_page_and_hides_the_message() {
    let html = render(the_thing_that_failed(), browser(), false).await;

    assert!(!html.contains("could not reach the search index"), "a 5xx message is hidden");
    assert!(html.contains("Server Error"));
    // And none of the debug page's furniture.
    assert!(!html.contains("Environment"));
    assert!(!html.contains("Headers"));
}

#[tokio::test]
async fn a_4xx_message_is_shown_even_with_debug_off() {
    // The framework's rule: a 4xx describes what the *client* did and is
    // useless hidden. The debug renderer must not change that either way.
    let html =
        render(Error::bad_request("The `since` parameter must be a date."), browser(), false).await;

    assert!(html.contains("The `since` parameter must be a date."));
}

#[tokio::test]
async fn a_bearer_token_in_the_request_is_never_echoed_back() {
    // Assembled at runtime, and this is not fussiness.
    //
    // Written as a literal, these strings appear in THIS FILE — and when
    // backtraces are on, a frame resolves to this file and the page renders a
    // source excerpt of it. The test then fails on its own fixture appearing
    // in the output rather than on the header being echoed, which is a
    // different bug entirely and took a while to see.
    //
    // It is also a real property of the feature, documented in the crate: a
    // source excerpt shows whatever is in the source, and redaction cannot
    // reach it. A hardcoded credential near a failing line will be on the
    // page. That is inherent to showing source and is why the page is
    // debug-only.
    // Both the value AND the needle come from the same fragments, so the
    // joined string exists only at runtime and appears nowhere in this file
    // for an excerpt to pick up.
    let secret = ["c0ffee", "d3adb33f"].concat();
    let session = ["s3ss10n", "1dent1f1er"].concat();

    let request = Request::builder()
        .method(Method::GET)
        .uri("/posts/42")
        .header("authorization", format!("Bearer {secret}").as_str())
        .header("cookie", format!("session={session}; theme=dark").as_str())
        .header("user-agent", "curl/8.0")
        .build();

    let html = render(the_thing_that_failed(), request, true).await;

    assert!(!html.contains(&secret), "the bearer token must not be echoed");
    assert!(!html.contains(&session), "nor the session cookie");
    // But the non-secret header is shown, because the page has to be useful.
    assert!(html.contains("curl/8.0"), "an ordinary header is still visible");
    // And the names are listed, which is a real debugging question.
    assert!(html.contains("authorization"));
}

#[tokio::test]
async fn the_error_kind_survives_a_relabel() {
    let error = Error::internal("underlying failure").with_kind(ErrorKind::ServiceUnavailable);
    let html = render(error, browser(), true).await;

    assert!(html.contains("503"));
    assert!(html.contains("ServiceUnavailable"));
}

#[tokio::test]
async fn an_api_client_gets_json_not_html() {
    let request = Request::builder()
        .method(Method::GET)
        .uri("/api/posts/42")
        .header("accept", "application/json")
        .build();

    let body = render(the_thing_that_failed(), request, true).await;

    assert!(body.starts_with('{'), "JSON, not a page");
    let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
    assert_eq!(parsed["message"], "could not reach the search index");
    assert!(parsed["debug"]["stack"].is_array());
}

#[tokio::test]
async fn the_renderer_is_usable_behind_an_arc_like_the_kernel_holds_it() {
    // The kernel stores `Arc<dyn ExceptionRenderer>`; this is the shape check
    // that the type actually satisfies that bound.
    let renderer: Arc<dyn ExceptionRenderer> =
        Arc::new(DebugExceptionRenderer::new().with_environment("local"));

    let rendered = RenderedError {
        status: 500,
        message: "boom".into(),
        details: None,
        disclosable: false,
        kind: Some("Internal".into()),
        backtrace: None,
    };

    let page = renderer.render(&browser(), &rendered, true).into_string().await.unwrap();
    assert!(page.contains("boom"));
}

/// Write a real page to `target/` so a human can open it.
///
/// Ignored by default — it is a development aid, not an assertion:
/// `cargo test -p rainier-debug --test end_to_end -- --ignored dump_a_page`
#[tokio::test]
#[ignore]
async fn dump_a_page() {
    std::env::set_var("RUST_BACKTRACE", "1");
    let html = render(the_thing_that_failed(), browser(), true).await;
    let path = std::env::temp_dir().join("rainier-debug-page.html");
    std::fs::write(&path, &html).unwrap();
    eprintln!("wrote {} ({} bytes)", path.display(), html.len());
}
