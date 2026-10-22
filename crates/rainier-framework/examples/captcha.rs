//! A form behind a human check, offline.
//!
//! What it shows:
//!
//! 1. a [`Captcha`] for Cloudflare Turnstile over the fake transport, pinned
//!    to a hostname and an action;
//! 2. a `POST /register` route behind [`RequireCaptcha`], compiled into a real
//!    router;
//! 3. one submission the provider refuses (a `422` on the widget's field) and
//!    one it accepts (the handler runs), with the `siteverify` request that
//!    went out;
//! 4. [`Captcha::always_pass`], for a feature test that is about something
//!    else.
//!
//! ```sh
//! cargo run -p rainier-framework --example captcha --features captcha
//! ```
//!
//! In an application the fake becomes the real transport and the secret
//! comes from the environment:
//!
//! ```ignore
//! use rainier_framework::http_client::ReqwestTransport;
//!
//! let captcha = Captcha::turnstile(Arc::new(ReqwestTransport::new()), env("CAPTCHA_SECRET"))
//!     .expect_hostname("app.example.com")
//!     .expect_action("register");
//! ```

use std::sync::Arc;

use rainier_framework::container::Container;
use rainier_framework::http::{Method, Request, Response};
use rainier_framework::http_client::{FakeTransport, Transport};
use rainier_framework::routing::{Req, Router};
use rainier_framework::{Captcha, RequireCaptcha};

async fn register(request: Req) -> Response {
    Response::text(format!("welcome, {}", request.input("email").unwrap_or_default()))
}

fn submission(token: &str) -> Request {
    Request::builder()
        .method(Method::POST)
        .uri("/register")
        .form(&[("email", "ada@example.com"), ("cf-turnstile-response", token)])
        .build()
}

async fn body(response: Response) -> String {
    let status = response.status();
    let bytes = response.into_http().into_body().collect().await.unwrap_or_default();
    format!("{status} {}", String::from_utf8_lossy(&bytes))
}

#[tokio::main]
async fn main() -> rainier_framework::support::Result<()> {
    // What Cloudflare would answer: first a refusal (a replayed token), then a
    // pass solved on our hostname with our action.
    let fake = Arc::new(FakeTransport::new());
    fake.responding(200, r#"{"success":false,"error-codes":["timeout-or-duplicate"]}"#);
    fake.responding(200, r#"{"success":true,"hostname":"app.example.com","action":"register"}"#);

    let captcha = Arc::new(
        Captcha::turnstile(Arc::clone(&fake) as Arc<dyn Transport>, "0x4AAA-secret")
            .expect_hostname("app.example.com")
            .expect_action("register"),
    );

    let mut router = Router::new();
    router
        .post("/register", register)
        .name("register")
        .middleware(RequireCaptcha::new(Arc::clone(&captcha)));
    let router = router.compile(&Container::new())?;

    println!("1. a replayed token:");
    println!("   {}", body(router.dispatch(submission("replayed-token")).await).await);

    println!("2. a fresh token:");
    println!("   {}", body(router.dispatch(submission("fresh-token")).await).await);

    let sent = fake.recorded();
    println!("3. what went to siteverify ({} requests):", sent.len());
    println!("   POST {}", sent[1].url());
    // The secret is in the body — print only its shape.
    println!("   body fields: secret=…, {}", sent[1].body().split_once('&').unwrap().1);

    println!("4. in a test about something else, no request at all:");
    let verdict = Captcha::always_pass().verify("", None).await;
    println!("   always_pass → success = {}", verdict.success);

    Ok(())
}
