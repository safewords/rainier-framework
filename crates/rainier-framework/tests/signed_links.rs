//! Signed links, generated and followed through a real application.
//!
//! The unit tests assert the signature. This asserts the thing that has to be
//! wired for it to be useful: that the bootstrap binds a signer over the
//! application's own key, that a link built from a route name verifies at that
//! route, and that the middleware refuses everything else.

use std::sync::Arc;

use rainier_framework::prelude::*;
use rainier_framework::signed::{SignedUrls, ValidateSignature};
use rainier_framework::testing::TestApp;

async fn app() -> (TestApp, Arc<SignedUrls>) {
    let booted = Rainier::new(".")
        .without_tracing()
        .configure(|config| {
            // A fixed key, so the links a test builds outlive the boot that
            // built them.
            config
                .set(rainier_framework::keys::APP_URL, "https://app.example.com".to_string())
                .unwrap();
        })
        .with_routes(|router| {
            router
                .get("/unsubscribe/{user}", || async { Response::ok("unsubscribed") })
                .name("unsubscribe")
                .middleware(ValidateSignature::resolved());

            router
                .get("/verify", || async { Response::ok("verified") })
                .name("verify")
                .middleware(ValidateSignature::resolved());
        })
        .boot()
        .await
        .expect("boots");

    let signed = booted.resolve::<SignedUrls>().expect("signed urls are bound");
    let app = TestApp::new(booted).expect("a kernel");

    (app, signed)
}

fn in_an_hour() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
        + 3600
}

#[tokio::test]
async fn a_signed_link_is_followed() {
    let (app, signed) = app().await;

    let link = signed.route("unsubscribe", &[("user", "42")]).unwrap();

    app.get(&link).await.assert_ok().assert_contains("unsubscribed");
}

#[tokio::test]
async fn the_same_route_without_a_signature_is_refused() {
    let (app, _) = app().await;

    app.get("/unsubscribe/42").await.assert_forbidden();
}

#[tokio::test]
async fn changing_the_id_is_refused() {
    // Unsubscribing somebody else, which is the attack this exists to stop.
    let (app, signed) = app().await;

    let link = signed.route("unsubscribe", &[("user", "42")]).unwrap();
    let tampered = link.replace("/unsubscribe/42", "/unsubscribe/43");

    app.get(&tampered).await.assert_forbidden();
}

#[tokio::test]
async fn a_temporary_link_works_until_it_expires() {
    let (app, signed) = app().await;

    let live = signed.temporary_route("verify", in_an_hour(), &[("user", "42")]).unwrap();
    app.get(&live).await.assert_ok().assert_contains("verified");

    let expired = signed.temporary_route("verify", 1, &[("user", "42")]).unwrap();
    app.get(&expired).await.assert_forbidden().assert_contains("expired");
}

#[tokio::test]
async fn an_absolute_link_is_what_goes_in_an_email_and_still_verifies() {
    let (app, signed) = app().await;

    let absolute = signed.absolute_route("verify", &[("user", "42")]).unwrap();
    assert!(absolute.starts_with("https://app.example.com/verify"), "{absolute}");

    // Arriving as the server sees it: a path and a query.
    let path = absolute.trim_start_matches("https://app.example.com");
    app.get(path).await.assert_ok();
}

#[tokio::test]
async fn two_applications_do_not_accept_each_others_links() {
    // Different boots, different generated keys. A link from one is forged as
    // far as the other is concerned, which is what makes the signature mean
    // "this application issued it" rather than "some Rainier issued it".
    let (_, first) = app().await;
    let (second_app, _) = app().await;

    let link = first.route("unsubscribe", &[("user", "42")]).unwrap();

    second_app.get(&link).await.assert_forbidden();
}
