//! Instrumenting requests — [`RecordMetrics`] and the `/metrics` endpoint.

use std::sync::Arc;
use std::time::Instant;

use rainier_http::{Request, Response, StatusCode};
use rainier_middleware::{Middleware, Next};
use rainier_routing::{MatchedRoute, Req};

use crate::registry::{labels, Labels, Metrics};

/// How many requests, by method, route and status.
pub const REQUESTS: &str = "http_requests_total";

/// How long they took, in seconds.
pub const DURATION: &str = "http_request_duration_seconds";

/// How many are being handled right now.
pub const IN_FLIGHT: &str = "http_requests_in_flight";

/// The route label for a request that matched nothing.
///
/// One series for every 404, rather than one per made-up URL. The path of an
/// unmatched request is chosen by whoever sent it, so labelling by it would let
/// anyone create unbounded series — the fastest way to fill a Prometheus.
pub const UNMATCHED: &str = "<unmatched>";

/// Times every request and counts it by method, route and status.
///
/// # Where to put it
///
/// **In a group's stack**, not the global one:
///
/// ```ignore
/// pub fn api() -> MiddlewareStack {
///     MiddlewareStack::new()
///         .with(RecordMetrics::new(Arc::clone(&metrics)))
///         .with(ThrottleRequests::per_minute(60))
/// }
/// ```
///
/// The router attaches the matched route just before a route's own pipeline
/// runs — so a group-level middleware can read the **pattern**, and a global
/// one cannot, because global middleware runs before anything has been matched.
///
/// Placed globally it still works and still times the whole request; every
/// series is simply labelled [`UNMATCHED`]. That is the trade, and it is worth
/// knowing rather than discovering from a dashboard where every route is the
/// same one.
pub struct RecordMetrics {
    metrics: Arc<Metrics>,
}

impl RecordMetrics {
    /// Record into `metrics`, declaring the three HTTP metrics.
    ///
    /// Declared here rather than on first request, so a scrape before any
    /// traffic shows zeroes instead of nothing.
    pub fn new(metrics: Arc<Metrics>) -> Self {
        metrics.counter(REQUESTS, "Requests handled, by method, route and status");
        metrics.histogram(DURATION, "Request duration in seconds, by method and route");
        metrics.gauge(IN_FLIGHT, "Requests being handled right now");

        Self { metrics }
    }

    /// The route pattern this request matched, or [`UNMATCHED`].
    fn route_of(request: &Request) -> String {
        request
            .extension::<MatchedRoute>()
            .map(|matched| matched.uri.clone())
            .unwrap_or_else(|| UNMATCHED.to_string())
    }
}

#[async_trait::async_trait]
impl Middleware for RecordMetrics {
    async fn handle(&self, request: Request, next: Next) -> Response {
        // Both read before `next`, because it takes the request.
        let method = request.method().to_string();
        let route = Self::route_of(&request);

        self.metrics.add(IN_FLIGHT, Labels::new(), 1.0);
        let started = Instant::now();

        let response = next.run(request).await;

        let elapsed = started.elapsed().as_secs_f64();
        self.metrics.add(IN_FLIGHT, Labels::new(), -1.0);

        self.metrics.increment(
            REQUESTS,
            labels([
                ("method", method.clone()),
                ("route", route.clone()),
                ("status", response.status().as_u16().to_string()),
            ]),
        );

        // No status on the histogram: it would multiply the series by every
        // status a route can return, and a duration is worth knowing per route
        // rather than per outcome.
        self.metrics.observe(DURATION, labels([("method", method), ("route", route)]), elapsed);

        response
    }

    fn name(&self) -> &'static str {
        "RecordMetrics"
    }
}

/// `GET /metrics` — the scrape endpoint.
///
/// # Do not expose this publicly
///
/// It tells anyone who reads it your traffic shape, your error rate and every
/// route you serve. Put it behind whatever your admin routes are behind, or on
/// an interface only the scraper can reach.
pub async fn endpoint(request: Req) -> Response {
    let Some(metrics) = request.extension::<Arc<Metrics>>().cloned() else {
        // Nothing bound. A 503 rather than an empty 200: an empty scrape looks
        // like an idle application, and a monitoring system that cannot tell
        // those apart is worse than one that is plainly broken.
        return Response::new(StatusCode::SERVICE_UNAVAILABLE)
            .with_body("metrics are not configured.");
    };

    Response::ok(metrics.render())
        .with_header("content-type", "text/plain; version=0.0.4; charset=utf-8")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_http::Method;
    use rainier_middleware::Pipeline;

    async fn through(metrics: &Arc<Metrics>, request: Request, status: StatusCode) -> Response {
        let pipeline = Pipeline::new()
            .through(RecordMetrics::new(Arc::clone(metrics)))
            .then(move |_| Box::pin(async move { Response::new(status) }));

        pipeline.run(request).await
    }

    fn matched(uri: &str) -> Request {
        Request::builder().method(Method::GET).uri("/posts/1").build().with_extension(
            MatchedRoute { name: None, uri: uri.to_string(), methods: vec![Method::GET] },
        )
    }

    #[tokio::test]
    async fn a_request_is_counted_and_timed_under_its_route_pattern() {
        let metrics = Arc::new(Metrics::new());

        through(&metrics, matched("/posts/{post}"), StatusCode::OK).await;

        let rendered = metrics.render();
        assert!(rendered.contains(r#"route="/posts/{post}""#), "{rendered}");
        assert!(rendered.contains(r#"status="200""#), "{rendered}");
        assert!(rendered.contains("http_request_duration_seconds_count"), "{rendered}");
    }

    #[tokio::test]
    async fn two_requests_to_one_pattern_are_one_series() {
        // The property that keeps a monitoring system alive: `/posts/1` and
        // `/posts/2` must not be two series.
        let metrics = Arc::new(Metrics::new());

        through(&metrics, matched("/posts/{post}"), StatusCode::OK).await;
        through(&metrics, matched("/posts/{post}"), StatusCode::OK).await;

        let count = metrics.value(
            REQUESTS,
            &labels([("method", "GET"), ("route", "/posts/{post}"), ("status", "200")]),
        );
        assert_eq!(count, Some(2.0));
    }

    #[tokio::test]
    async fn an_unmatched_request_gets_one_shared_label() {
        // Otherwise anyone could mint series by making up URLs.
        let metrics = Arc::new(Metrics::new());

        through(&metrics, Request::builder().uri("/nope").build(), StatusCode::NOT_FOUND).await;
        through(&metrics, Request::builder().uri("/also-nope").build(), StatusCode::NOT_FOUND)
            .await;

        let rendered = metrics.render();

        // One *series*, not one line — a histogram is a dozen lines. The
        // property is that two made-up URLs did not become two label sets.
        let counters =
            rendered.lines().filter(|line| line.starts_with("http_requests_total{")).count();

        assert_eq!(counters, 1, "{rendered}");
        assert_eq!(
            metrics.value(
                REQUESTS,
                &labels([("method", "GET"), ("route", UNMATCHED), ("status", "404"),])
            ),
            Some(2.0)
        );
        assert!(!rendered.contains("/nope"), "the path a client chose must not be a label");
    }

    #[tokio::test]
    async fn the_status_is_on_the_counter_and_not_on_the_histogram() {
        let metrics = Arc::new(Metrics::new());

        through(&metrics, matched("/x"), StatusCode::INTERNAL_SERVER_ERROR).await;

        let rendered = metrics.render();
        let duration = rendered
            .lines()
            .find(|line| line.starts_with("http_request_duration_seconds_count"))
            .expect("a duration line");

        assert!(rendered.contains(r#"status="500""#), "{rendered}");
        assert!(!duration.contains("status"), "{duration}");
    }

    #[tokio::test]
    async fn the_in_flight_gauge_returns_to_zero() {
        let metrics = Arc::new(Metrics::new());

        through(&metrics, matched("/x"), StatusCode::OK).await;

        assert_eq!(metrics.value(IN_FLIGHT, &Labels::new()), Some(0.0));
    }

    #[tokio::test]
    async fn the_endpoint_says_so_when_nothing_is_bound() {
        let response = endpoint(Arc::new(Request::builder().build())).await;

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn the_endpoint_answers_in_the_format_prometheus_expects() {
        let metrics = Arc::new(Metrics::new());
        metrics.counter("things_total", "Things");
        metrics.increment("things_total", Labels::new());

        let request = Arc::new(Request::builder().build().with_extension(Arc::clone(&metrics)));
        let response = endpoint(request).await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.header("content-type"),
            Some("text/plain; version=0.0.4; charset=utf-8")
        );
    }
}
