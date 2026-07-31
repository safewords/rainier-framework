//! The ambient fake, which is what an application actually uses.
//!
//! `Http::fake()` scopes to the calling thread, so these run in parallel like
//! any other test — the first version of this file did not, and three of six
//! failed on every other run because they were overwriting one process-global
//! transport. That is what `SCOPED_FAKE` exists for.

use rainier_http_client::{Backoff, Http};
use serde_json::json;

/// The code under test: something that calls out.
async fn notify_application(user_id: u64) -> rainier_support::Result<()> {
    Http::post("https://hooks.example.com/user-updated")
        .header("x-signature", "computed-elsewhere")
        .json(&json!({ "user_id": user_id }))?
        .send()
        .await?
        .error_for_status()?;

    Ok(())
}

#[tokio::test]
async fn an_outbound_call_can_be_asserted_on() {
    // The whole item. Without this, proving the webhook was signed means
    // standing up a server — so nobody does, and the signing code is the code
    // nothing exercises.
    let fake = Http::fake();

    notify_application(42).await.expect("notify");

    Http::assert_sent(|request| request.url_contains("/user-updated"));
    Http::assert_sent(|request| request.header("x-signature").is_some());

    assert_eq!(fake.recorded()[0].json().unwrap()["user_id"], 42);
}

#[tokio::test]
async fn nothing_reaches_the_network_while_faking() {
    // A suite that accidentally calls a real endpoint is a suite that fails on
    // a train. The URL below does not resolve, and this still passes.
    Http::fake();

    let response = Http::get("https://this-host-does-not-exist.invalid/anything")
        .send()
        .await
        .expect("the fake answers rather than resolving anything");

    assert_eq!(response.status(), 200);
}

#[tokio::test]
async fn a_queued_answer_is_what_the_caller_sees() {
    Http::fake().responding(500, "the other end is unwell");

    let failed = notify_application(42).await;

    assert!(failed.is_err());
    assert!(failed.unwrap_err().message().contains("unwell"));
}

#[tokio::test]
async fn a_retry_sequence_can_be_described() {
    let fake = Http::fake();
    fake.responding(503, "later").responding(200, "yes");

    let response = Http::get("https://api.example.com/thing")
        .retry(3, Backoff::None)
        .send()
        .await
        .expect("send");

    assert_eq!(response.status(), 200);
    fake.assert_sent_count(2);
}

#[tokio::test]
async fn asserting_nothing_was_sent_actually_checks() {
    Http::fake();

    Http::assert_nothing_sent();

    Http::get("https://x").send().await.expect("send");

    let panicked = std::panic::catch_unwind(Http::assert_nothing_sent);
    assert!(panicked.is_err(), "it should have noticed the request");
}

#[tokio::test]
#[should_panic(expected = "not faking")]
async fn asserting_without_a_fake_panics_rather_than_passing() {
    // The rule every Rainier double follows. An `assert_sent` that silently
    // passed because somebody forgot `Http::fake()` is the most dangerous test
    // in a suite: it guards something and reports success forever.
    Http::clear();

    Http::assert_sent(|_| true);
}
