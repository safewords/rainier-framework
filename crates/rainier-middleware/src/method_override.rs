//! Letting an HTML form spell `DELETE` — [`MethodOverride`].
//!
//! ```ignore
//! // Globally, and only if you serve HTML forms.
//! registry.global(MethodOverride::new());
//! ```
//!
//! ```html
//! <form method="post" action="/posts/7">
//!   <input type="hidden" name="_method" value="DELETE">
//! </form>
//! ```
//!
//! A browser form can send `GET` and `POST` and nothing else. Server-side
//! frameworks have long worked around that with a hidden `_method` field the
//! server converts on arrival; this is the same trick, and it is what lets a
//! server-rendered application have a REST-shaped route table without a line
//! of JavaScript.
//!
//! # Off by default, on purpose
//!
//! A JSON API has no use for it — its clients can send any method — and a
//! rewrite nobody needs is a rewrite that can surprise somebody:
//!
//! - Anything upstream that made a decision **by method** made it on the
//!   original. A WAF rule, an audit log, a proxy that only forwards `POST`
//!   to this path, a rate limit keyed on the method — all of them saw a
//!   `POST` and this makes it a `DELETE` afterwards.
//! - It is a body field, so a caller who can shape the body chooses the
//!   method. That is fine when the route is the same route either way, and it
//!   is exactly the point when it is not.
//!
//! So it is a deliberate switch, and it only ever upgrades a `POST`.

use rainier_http::{Method, Request, Response};

use crate::pipeline::{Middleware, Next};

/// The field a form puts the real method in — the spelling PHP frameworks
/// made conventional.
const FIELD: &str = "_method";

/// The header form of the same thing, which some clients and proxies send.
const HEADER: &str = "x-http-method-override";

/// Rewrites a `POST` carrying `_method` into that method.
#[derive(Debug, Clone)]
pub struct MethodOverride {
    trust_header: bool,
}

impl Default for MethodOverride {
    fn default() -> Self {
        Self::new()
    }
}

impl MethodOverride {
    /// Read the override from the form field only.
    pub fn new() -> Self {
        Self { trust_header: false }
    }

    /// Also honour `X-HTTP-Method-Override`.
    ///
    /// For a client behind something that will not forward a `PATCH` — some
    /// corporate proxies still will not. Separate from the default because a
    /// header is easier to attach by accident than a form field: an
    /// intermediary that adds it to every request would rewrite every `POST`
    /// in the application.
    #[must_use = "this returns a configured middleware rather than configuring in place"]
    pub fn trusting_the_header(mut self) -> Self {
        self.trust_header = true;
        self
    }
}

#[async_trait::async_trait]
impl Middleware for MethodOverride {
    async fn handle(&self, mut request: Request, next: Next) -> Response {
        // Only a POST. A GET carrying `_method=DELETE` is a link somebody
        // crafted, and following it would make every crawler a hazard.
        if request.method() == Method::POST {
            if let Some(method) = self.requested(&request) {
                request.set_method(method);
            }
        }

        next.run(request).await
    }

    fn name(&self) -> &'static str {
        "MethodOverride"
    }
}

impl MethodOverride {
    /// The method this request is asking to be, if it is asking for one we
    /// will grant.
    fn requested(&self, request: &Request) -> Option<Method> {
        let asked = request
            .input(FIELD)
            .or_else(|| {
                self.trust_header.then(|| request.header(HEADER).map(str::to_owned)).flatten()
            })?
            .trim()
            .to_ascii_uppercase();

        match asked.as_str() {
            // The three a form actually needs. Not GET or HEAD: turning a POST
            // into a GET would move the body into nowhere and change which
            // route runs in a way no form ever wants. Not POST: that is what
            // it already is.
            "PUT" => Some(Method::PUT),
            "PATCH" => Some(Method::PATCH),
            "DELETE" => Some(Method::DELETE),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::Pipeline;
    use serde_json::json;

    /// Echoes back the method the handler was reached with.
    async fn method_seen(request: Request, middleware: MethodOverride) -> String {
        Pipeline::new()
            .through(middleware)
            .then(|request: Request| async move { Response::ok(request.method().to_string()) })
            .run(request)
            .await
            .into_string()
            .await
            .unwrap()
    }

    fn form(method: Method, body: serde_json::Value) -> Request {
        Request::builder().method(method).uri("/posts/7").json(&body).build()
    }

    #[tokio::test]
    async fn a_post_with_the_field_becomes_that_method() {
        for (asked, expected) in [("PUT", "PUT"), ("PATCH", "PATCH"), ("DELETE", "DELETE")] {
            let request = form(Method::POST, json!({ FIELD: asked }));
            assert_eq!(method_seen(request, MethodOverride::new()).await, expected);
        }
    }

    #[tokio::test]
    async fn the_spelling_does_not_have_to_be_shouted() {
        let request = form(Method::POST, json!({ FIELD: "delete" }));
        assert_eq!(method_seen(request, MethodOverride::new()).await, "DELETE");
    }

    #[tokio::test]
    async fn a_get_is_never_rewritten() {
        // A link with `?_method=DELETE` would otherwise make every crawler a
        // hazard.
        let request = Request::builder().method(Method::GET).uri("/posts/7?_method=DELETE").build();

        assert_eq!(method_seen(request, MethodOverride::new()).await, "GET");
    }

    #[tokio::test]
    async fn a_post_cannot_become_a_get() {
        let request = form(Method::POST, json!({ FIELD: "GET" }));
        assert_eq!(method_seen(request, MethodOverride::new()).await, "POST");
    }

    #[tokio::test]
    async fn nonsense_is_ignored_rather_than_refused() {
        // A 400 here would be a stricter contract than the form ever agreed
        // to, and the route for a plain POST is still the right answer.
        let request = form(Method::POST, json!({ FIELD: "TEAPOT" }));
        assert_eq!(method_seen(request, MethodOverride::new()).await, "POST");
    }

    #[tokio::test]
    async fn the_header_is_ignored_until_it_is_trusted() {
        let request = || {
            Request::builder().method(Method::POST).uri("/posts/7").header(HEADER, "DELETE").build()
        };

        assert_eq!(method_seen(request(), MethodOverride::new()).await, "POST");
        assert_eq!(
            method_seen(request(), MethodOverride::new().trusting_the_header()).await,
            "DELETE"
        );
    }

    #[tokio::test]
    async fn a_request_with_nothing_to_say_is_left_alone() {
        let request = form(Method::POST, json!({ "title": "Hello" }));
        assert_eq!(method_seen(request, MethodOverride::new()).await, "POST");
    }
}
