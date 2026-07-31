//! Serving the document — `GET /openapi.json`.

use std::sync::Arc;

use rainier_http::{Response, StatusCode};
use rainier_routing::{CompiledRouter, Req};

use crate::document::OpenApi;

/// The document, rendered once at boot.
///
/// Built ahead of the first request rather than per request: it walks every
/// route and every rule, and a scraper polling `/openapi.json` should not make
/// that the most expensive endpoint you serve.
pub struct Rendered {
    json: String,
}

impl Rendered {
    /// Render `document` against `router`.
    pub fn new(document: &OpenApi, router: &CompiledRouter) -> Self {
        let dangling = document.dangling(router);
        if !dangling.is_empty() {
            // A rename orphaned some documentation. Not fatal — the rest of the
            // document is still right — but silent would mean nobody ever
            // notices the endpoint has no summary any more.
            tracing::warn!(
                routes = ?dangling,
                "the OpenAPI document describes routes that do not exist; they were renamed or removed"
            );
        }

        Self { json: document.to_json(router) }
    }

    /// The rendered JSON.
    pub fn json(&self) -> &str {
        &self.json
    }
}

/// `GET /openapi.json`.
///
/// Returns `404` when no document is bound, which is what an application that
/// has turned the feature off should look like from outside — not a `500`, and
/// not an empty document that a client would try to use.
pub async fn serve(request: Req) -> Response {
    let Some(document) = request.extension::<Arc<Rendered>>() else {
        return Response::new(StatusCode::NOT_FOUND);
    };

    Response::ok(document.json().to_string())
        .with_header("content-type", "application/json; charset=utf-8")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_container::Container;
    use rainier_http::Request;
    use rainier_routing::Router;

    fn router() -> CompiledRouter {
        let mut router = Router::new();
        router.get("/posts", || async { "ok" }).name("posts.index");

        router.compile(&Container::new()).expect("compiles")
    }

    #[tokio::test]
    async fn the_document_is_served_as_json() {
        let rendered = Arc::new(Rendered::new(&OpenApi::new("Test", "1.0.0"), &router()));
        let request = Arc::new(Request::builder().build().with_extension(rendered));

        let response = serve(request).await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.header("content-type"), Some("application/json; charset=utf-8"));
    }

    #[tokio::test]
    async fn nothing_bound_is_a_404_not_an_empty_document() {
        // An application with the feature off should look like one that has no
        // such endpoint, rather than one serving a document with no paths.
        let response = serve(Arc::new(Request::builder().build())).await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn rendering_happens_once() {
        let rendered = Rendered::new(&OpenApi::new("Test", "1.0.0"), &router());

        assert!(rendered.json().contains("\"openapi\": \"3.1.0\""));
        assert!(rendered.json().contains("/posts"));
    }
}
