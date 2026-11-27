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
    /// The header a frontend may start a trace with, by sending an id.
    request_id_header: Option<String>,
}

impl Default for Trace {
    fn default() -> Self {
        Self::new()
    }
}

impl Trace {
    /// Sampling everything, and echoing the header back.
    pub fn new() -> Self {
        Self { sample: true, respond_with_header: true, request_id_header: None }
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

    /// Let a frontend start the trace: a request carrying `header` with a
    /// UUID (or 32 hex digits), and no `traceparent`, is traced under that id
    /// — in this service, in every service it calls, and in every job it
    /// queues — and the response carries the header back with it.
    ///
    /// The id is only a tag: it links the work, and decides nothing about
    /// what is recorded or kept. A `traceparent` the request also carries
    /// wins, because it names the caller's span as well.
    ///
    /// ```
    /// use rainier_telemetry::Trace;
    ///
    /// let trace = Trace::new().request_id_header("x-request-id");
    /// # let _ = trace;
    /// ```
    pub fn request_id_header(mut self, header: impl Into<String>) -> Self {
        let header = header.into().trim().to_ascii_lowercase();
        self.request_id_header = (!header.is_empty()).then_some(header);
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
        let incoming = request.header(TRACEPARENT).and_then(|h| TraceContext::parse(h).ok());
        let chosen = || {
            let header = self.request_id_header.as_deref()?;
            TraceContext::from_request_id(request.header(header)?, self.sample)
        };
        let context = match incoming {
            Some(context) => context,
            None => chosen().unwrap_or_else(|| TraceContext::extract(None, self.sample)),
        };
        let state = request.header(TRACESTATE).map(str::to_string);

        let header = context.to_header();
        let trace_id = context.trace_id().to_string();
        // What anything this request calls should name as its parent — this
        // service's own span in the trace, not the caller's.
        let outbound = crate::propagation::child_of(&context);

        // On the request's unit of work, so every event published while it is
        // handled carries the same trace id the log lines do. A trace the
        // kernel already took from the caller's `traceparent` stands — it is
        // this one.
        if let Some(unit) = rainier_events::Context::current_unit() {
            unit.set_trace(Some(&trace_id), Some(&header));
        }

        // On the request, so a handler can propagate it to whatever it calls.
        request.extensions_mut().insert(context);
        if let Some(state) = state {
            request.extensions_mut().insert(TraceState(state));
        }

        // A span rather than a field on each event: everything logged inside
        // it inherits the id, including from code that knows nothing about
        // tracing contexts.
        //
        // Unless the server already opened one. With the `telemetry` feature
        // the kernel wraps every request in an `http.server.request` span that
        // declares `trace_id`; a second span around the same work would only
        // be a duplicate row in every trace, so the id goes onto that one.
        let current = tracing::Span::current();
        let span = if current.has_field("trace_id") {
            current.record("trace_id", trace_id.as_str());
            current
        } else {
            tracing::info_span!("http.request", trace_id = %trace_id)
        };

        // `instrument`, not `enter`: a guard held across an `.await` stays
        // entered on whichever thread the future happens to be polled on, and
        // attributes other requests' log lines to this one.
        let response = tracing::Instrument::instrument(
            crate::propagation::scope(Some(outbound), next.run(request)),
            span,
        )
        .await;

        let response = match &self.request_id_header {
            Some(name) => response.with_header(name, &trace_id),
            None => response,
        };
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

    const UUID: &str = "1b4e28ba-2fa1-11d2-883f-0016d3cca427";

    #[tokio::test]
    async fn a_frontend_starts_the_trace_with_an_id_of_its_own() {
        let trace = || Trace::new().request_id_header("X-Request-Id");
        let request = Request::builder().header("x-request-id", UUID).build();

        let response = through(trace(), request).await;

        assert_eq!(
            response.headers().get("x-request-id").and_then(|v| v.to_str().ok()),
            Some("1b4e28ba2fa111d2883f0016d3cca427"),
            "echoed, as the trace id it became"
        );
        assert_eq!(body_of(response).await, "1b4e28ba2fa111d2883f0016d3cca427");
    }

    #[tokio::test]
    async fn a_traceparent_wins_over_a_request_id_and_a_bad_id_is_ignored() {
        let trace = || Trace::new().request_id_header("x-request-id");

        let both =
            Request::builder().header(TRACEPARENT, EXAMPLE).header("x-request-id", UUID).build();
        assert_eq!(body_of(through(trace(), both).await).await, "4bf92f3577b34da6a3ce929d0e0e4736");

        for bad in [
            "not-an-id",
            "00000000-0000-0000-0000-000000000000",
            "1b4e28ba-2fa1-11d2-883f-0016d3cca42",
        ] {
            let request = Request::builder().header("x-request-id", bad).build();
            let id = body_of(through(trace(), request).await).await;
            assert_eq!(id.len(), 32, "{bad}: a fresh trace");
            assert_ne!(id, bad.replace('-', ""), "{bad}");
        }

        // Not configured: the header means nothing.
        let request = Request::builder().header("x-request-id", UUID).build();
        assert_ne!(body_of(through(Trace::new(), request).await).await, UUID.replace('-', ""));
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
    async fn the_trace_is_in_force_for_anything_the_handler_calls() {
        let response = Pipeline::new()
            .through(Trace::new())
            .then(|_request: Request| {
                Box::pin(async move {
                    tokio::task::yield_now().await;
                    Response::ok(crate::propagation::current().unwrap_or_default())
                })
            })
            .run(Request::builder().header(TRACEPARENT, EXAMPLE).build())
            .await;

        let outbound = TraceContext::parse(&body_of(response).await).expect("a traceparent");
        assert_eq!(outbound.trace_id(), "4bf92f3577b34da6a3ce929d0e0e4736", "the same trace");
        assert_ne!(outbound.parent_id(), "00f067aa0ba902b7", "with this service as the parent");
        assert_eq!(crate::propagation::current(), None, "and only for the request");
    }

    #[tokio::test]
    async fn the_trace_is_recorded_on_the_requests_unit_of_work() {
        use rainier_events::{Unit, UnitKind};

        let unit = Unit::new(UnitKind::Request, None, Some("/".into()));
        unit.scope(through(Trace::new(), Request::builder().header(TRACEPARENT, EXAMPLE).build()))
            .await;
        assert_eq!(unit.trace_id(), Some("4bf92f3577b34da6a3ce929d0e0e4736"));
        assert_eq!(unit.traceparent(), Some(EXAMPLE));

        // And one it starts, when nothing came in.
        let fresh = Unit::new(UnitKind::Request, None, Some("/".into()));
        let response = fresh.scope(through(Trace::new(), Request::builder().build())).await;
        let header = response.header(TRACEPARENT).expect("a header").to_string();
        assert_eq!(fresh.trace_id().map(str::len), Some(32));
        assert!(header.contains(fresh.trace_id().unwrap_or("?")), "{header}");
    }

    #[tokio::test]
    async fn a_locally_started_trace_follows_the_sampling_setting() {
        let response = through(Trace::new().sampling(false), Request::builder().build()).await;

        let header = response.header(TRACEPARENT).expect("a header").to_string();
        assert!(header.ends_with("-00"), "{header}");
    }
}
