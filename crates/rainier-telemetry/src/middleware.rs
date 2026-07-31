//! [`Trace`] — the middleware that puts a request in a trace.

use rainier_http::{Request, Response};
use rainier_middleware::{Middleware, Next};

use crate::context::{TraceContext, TRACEPARENT, TRACESTATE};

/// Joins each request to its trace, and puts the trace on the log line.
///
/// Outermost in the global stack, so every log emitted while handling the
/// request carries the trace id — including the ones from middleware that
/// rejected it, which are the lines you most want to find later.
///
/// ```ignore
/// MiddlewareStack::new().push(Trace::new()).push(RecordMetrics::new(metrics))
/// ```
///
/// # What it does without an exporter
///
/// Everything except send spans anywhere. The trace id is on the request, on
/// the response and on every log line, so a request can be followed across
/// services by grepping — which is most of the value, and it costs no
/// dependency. The [`otlp`](crate) feature adds the exporter.
pub struct Trace {
    /// Whether to sample when nothing upstream has decided.
    sample: bool,
    /// Echo the trace id back on the response.
    respond_with_header: bool,
}

impl Default for Trace {
    fn default() -> Self {
        Self::new()
    }
}

impl Trace {
    /// Sampling everything, and echoing the header back.
    pub fn new() -> Self {
        Self { sample: true, respond_with_header: true }
    }

    /// Whether to sample a trace this service starts.
    ///
    /// Only applies when there is no incoming `traceparent`: a decision made
    /// upstream is honoured, because a trace sampled in half its services is a
    /// trace with holes in it.
    pub fn sampling(mut self, sample: bool) -> Self {
        self.sample = sample;
        self
    }

    /// Whether to return `traceparent` on the response.
    ///
    /// On by default, and it is what lets somebody paste a trace id from a
    /// browser's network tab into a trace viewer. Turn it off if you would
    /// rather not tell a client your trace ids.
    pub fn respond_with_header(mut self, respond: bool) -> Self {
        self.respond_with_header = respond;
        self
    }
}

#[async_trait::async_trait]
impl Middleware for Trace {
    async fn handle(&self, mut request: Request, next: Next) -> Response {
        let context = TraceContext::extract(request.header(TRACEPARENT), self.sample);
        let state = request.header(TRACESTATE).map(str::to_string);

        let header = context.to_header();
        let trace_id = context.trace_id().to_string();

        // On the request, so a handler can propagate it to whatever it calls.
        request.extensions_mut().insert(context);
        if let Some(state) = state {
            request.extensions_mut().insert(TraceState(state));
        }

        // A span rather than a field on each event: everything logged inside
        // it inherits the id, including from code that knows nothing about
        // tracing contexts.
        let span = tracing::info_span!("http.request", trace_id = %trace_id);
        let _entered = span.enter();

        let response = next.run(request).await;

        if self.respond_with_header {
            return response.with_header(TRACEPARENT, &header);
        }
        response
    }

    fn name(&self) -> &'static str {
        "Trace"
    }
}

/// The `tracestate` header, carried through untouched.
///
/// Vendor-specific, and the specification is explicit that a service which does
/// not understand an entry must pass it on rather than drop it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceState(pub String);

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_http::StatusCode;
    use rainier_middleware::Pipeline;

    const EXAMPLE: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

    async fn through(trace: Trace, request: Request) -> Response {
        Pipeline::new()
            .through(trace)
            .then(|request: Request| {
                Box::pin(async move {
                    let context = request
                        .extension::<TraceContext>()
                        .map(|c| c.trace_id().to_string())
                        .unwrap_or_default();

                    Response::ok(context)
                })
            })
            .run(request)
            .await
    }

    async fn body_of(response: Response) -> String {
        let bytes = response.into_http().into_body().collect().await.expect("a body");
        String::from_utf8_lossy(&bytes).to_string()
    }

    #[tokio::test]
    async fn an_incoming_trace_is_joined_rather_than_replaced() {
        let request = Request::builder().header(TRACEPARENT, EXAMPLE).build();

        let response = through(Trace::new(), request).await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_of(response).await, "4bf92f3577b34da6a3ce929d0e0e4736");
    }

    #[tokio::test]
    async fn a_request_with_no_trace_gets_one() {
        let response = through(Trace::new(), Request::builder().build()).await;

        assert_eq!(body_of(response).await.len(), 32);
    }

    #[tokio::test]
    async fn a_broken_trace_does_not_fail_the_request() {
        // An upstream with a misconfigured proxy is not this service's problem
        // to turn into a 400.
        let request = Request::builder().header(TRACEPARENT, "garbage").build();

        let response = through(Trace::new(), request).await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_of(response).await.len(), 32, "a fresh trace");
    }

    #[tokio::test]
    async fn the_trace_comes_back_on_the_response() {
        let request = Request::builder().header(TRACEPARENT, EXAMPLE).build();

        let response = through(Trace::new(), request).await;

        assert_eq!(response.header(TRACEPARENT), Some(EXAMPLE));
    }

    #[tokio::test]
    async fn the_response_header_can_be_turned_off() {
        let request = Request::builder().header(TRACEPARENT, EXAMPLE).build();

        let response = through(Trace::new().respond_with_header(false), request).await;

        assert_eq!(response.header(TRACEPARENT), None);
    }

    #[tokio::test]
    async fn an_upstream_sampling_decision_is_honoured_over_the_local_one() {
        // A trace sampled in half its services is a trace with holes in it.
        let unsampled = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-00";
        let request = Request::builder().header(TRACEPARENT, unsampled).build();

        let response = through(Trace::new().sampling(true), request).await;

        assert_eq!(response.header(TRACEPARENT), Some(unsampled));
    }

    #[tokio::test]
    async fn a_locally_started_trace_follows_the_sampling_setting() {
        let response = through(Trace::new().sampling(false), Request::builder().build()).await;

        let header = response.header(TRACEPARENT).expect("a header").to_string();
        assert!(header.ends_with("-00"), "{header}");
    }
}
