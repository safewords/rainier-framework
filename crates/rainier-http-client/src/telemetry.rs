//! `traceparent` on the wire, an `http.client.request` span per attempt behind
//! the `telemetry` feature, and an
//! [`HttpClientRequestSent`](crate::HttpClientRequestSent) event per attempt.
//!
//! Applied inside the **transports**, not the [`PendingRequest`](crate::PendingRequest)
//! builder, because the builder is not the only way out: the mail APIs, the
//! Stripe client and the SSO token exchange each hand an
//! [`OutboundRequest`] straight to a transport. Instrumenting the socket is
//! what makes every one of them visible without any of them knowing.
//!
//! The fake is instrumented too. It records what *would have left the
//! building*, including the `traceparent` header — so a test can assert the
//! trace reaches the other service.
//!
//! # Request tagging
//!
//! Each attempt is tagged by `rainier-request-tagging` ([`outbound::tag`]):
//! inside a trace it carries `traceparent` with the same trace id and a span
//! id naming this attempt. This crate decides nothing about traces. The event
//! carries the span id sent, as `rainier.trace.span_id`.
//!
//! # What is recorded
//!
//! `http.request.method`, `server.address`, `server.port`, `url.scheme` and
//! `url.full` **without its query string or credentials** — a query is where
//! an API key or a signature goes, and `user:pass@` is a credential by
//! definition. `http.response.status_code`, and `error.type` for a 4xx or 5xx
//! (the status) or a transport failure (the error's kind). One span per
//! attempt, as the OpenTelemetry conventions ask, so three retries are three
//! spans and the backoff between them is the gap you can see. The event
//! carries the same, under the same names.
//!
//! # With nobody listening
//!
//! Without the `telemetry` feature, outside any trace and with nothing
//! subscribed, a send reads a thread-local, asks the bus once and returns the
//! transport's own future: no copy of the URL, no clock read, no extra future.
//!
//! [`outbound::tag`]: rainier_request_tagging::outbound::tag

use std::sync::Arc;
use std::time::Instant;

use rainier_events::Dispatcher;
use rainier_request_tagging::outbound::{self, Tagged};
use rainier_support::{BoxFuture, Result};

use crate::transport::{OutboundRequest, RawResponse};

/// What an attempt's event needs, taken before the request is handed over —
/// only when something will hear it.
struct Sending {
    events: Arc<Dispatcher>,
    method: String,
    url: String,
    started: Instant,
}

impl Sending {
    fn start(request: &OutboundRequest) -> Option<Self> {
        let events = crate::events::listening()?;
        Some(Self {
            events,
            method: request.method.clone(),
            url: request.url.clone(),
            started: Instant::now(),
        })
    }

    fn finish(self, result: &Result<RawResponse>, tagged: &Tagged) {
        let duration = self.started.elapsed();
        let Self { events, method, url, .. } = self;
        events.publish_activity_with(|| {
            crate::events::HttpClientRequestSent::new(&method, &url, result, duration)
                .with_trace_span(tagged.span_id())
        });
    }
}

/// Observe one send: its tag, its event, and with `telemetry` its client span.
///
/// The built-in transports call it around every send. A [`Transport`] of
/// your own should too, or what it sends carries no trace and is recorded
/// nowhere:
///
/// ```
/// use rainier_http_client::transport::{OutboundRequest, RawResponse, Transport};
/// use rainier_support::{BoxFuture, Result};
///
/// struct Loopback;
///
/// impl Transport for Loopback {
///     fn send<'a>(&'a self, request: OutboundRequest) -> BoxFuture<'a, Result<RawResponse>> {
///         rainier_http_client::telemetry::instrument(request, |request| {
///             Box::pin(async move {
///                 let _ = request;
///                 Ok(RawResponse { status: 204, headers: Default::default(), body: Vec::new() })
///             })
///         })
///     }
///
///     fn name(&self) -> &str {
///         "loopback"
///     }
/// }
/// ```
///
/// [`Transport`]: crate::Transport
pub fn instrument<'a>(
    request: OutboundRequest,
    send: impl FnOnce(OutboundRequest) -> BoxFuture<'a, Result<RawResponse>> + Send + 'a,
) -> BoxFuture<'a, Result<RawResponse>> {
    #[cfg(feature = "telemetry")]
    return traced(request, send);

    #[cfg(not(feature = "telemetry"))]
    {
        let mut request = request;
        let tagged = outbound::tag(&mut request.headers, &tracing::Span::none());
        let sending = Sending::start(&request);
        if sending.is_none() && tagged.is_empty() {
            // Nobody to tell and nothing to hear: the transport's own future.
            return send(request);
        }
        Box::pin(async move {
            let result = send(request).await;
            if let Some(sending) = sending {
                sending.finish(&result, &tagged);
            }
            result
        })
    }
}

/// Wrap one send in a client span, tagging the request.
#[cfg(feature = "telemetry")]
fn traced<'a>(
    mut request: OutboundRequest,
    send: impl FnOnce(OutboundRequest) -> BoxFuture<'a, Result<RawResponse>> + Send + 'a,
) -> BoxFuture<'a, Result<RawResponse>> {
    use tracing::field::Empty;

    let target = Target::parse(&request.url);
    let method = request.method.to_ascii_uppercase();
    let span = rainier_support::span!(
        "http.client.request",
        otel.name = %method,
        otel.kind = "client",
        http.request.method = %method,
        server.address = %target.host,
        server.port = target.port,
        url.scheme = %target.scheme,
        url.full = %target.redacted,
        http.response.status_code = Empty,
    );

    let tagged = outbound::tag(&mut request.headers, &span);
    let sending = Sending::start(&request);

    let traced = span.clone();
    Box::pin(tracing::Instrument::instrument(
        async move {
            let result = send(request).await;
            match &result {
                Ok(response) => {
                    traced.record("http.response.status_code", response.status);
                    if response.status >= 400 {
                        traced.record("otel.status_code", "ERROR");
                        traced.record("error.type", response.status.to_string().as_str());
                    }
                }
                Err(error) => rainier_support::telemetry::record_error(&traced, error),
            }
            if let Some(sending) = sending {
                sending.finish(&result, &tagged);
            }
            result
        },
        span,
    ))
}

/// The parts of a URL a span may carry.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Target {
    pub(crate) scheme: String,
    pub(crate) host: String,
    pub(crate) port: u16,
    /// `scheme://host[:port]/path` — no query, no fragment, no userinfo.
    pub(crate) redacted: String,
}

impl Target {
    /// A forgiving parse: a URL this cannot make sense of becomes an empty
    /// host rather than a failed request — the span is not worth the call.
    pub(crate) fn parse(url: &str) -> Self {
        let (scheme, rest) = url.split_once("://").unwrap_or(("", url));
        let rest = rest.split(['?', '#']).next().unwrap_or_default();
        let (authority, path) = match rest.find('/') {
            Some(index) => (&rest[..index], &rest[index..]),
            None => (rest, ""),
        };
        let authority = authority.rsplit('@').next().unwrap_or_default();

        let default_port = if scheme.eq_ignore_ascii_case("http") { 80 } else { 443 };
        let (host, port) = match authority.rsplit_once(':') {
            // Not an IPv6 literal's colon: those sit inside brackets.
            Some((host, port)) if !port.contains(']') => {
                (host, port.parse().unwrap_or(default_port))
            }
            _ => (authority, default_port),
        };

        Self {
            scheme: scheme.to_ascii_lowercase(),
            host: host.to_string(),
            port,
            redacted: format!("{scheme}://{authority}{path}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FakeTransport, Http};
    use rainier_request_tagging::TraceContext;
    #[cfg(feature = "telemetry")]
    use rainier_support::telemetry::testing::Recorder;
    use std::sync::Arc;

    const UPSTREAM: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
    const TRACE: &str = "4bf92f3577b34da6a3ce929d0e0e4736";

    #[test]
    fn a_url_loses_its_query_and_its_credentials() {
        let target =
            Target::parse("https://user:hunter2@api.example.com:8443/v1/charges?key=sk_live");

        assert_eq!(target.host, "api.example.com");
        assert_eq!(target.port, 8443);
        assert_eq!(target.scheme, "https");
        assert_eq!(target.redacted, "https://api.example.com:8443/v1/charges");

        assert_eq!(Target::parse("http://localhost/x").port, 80);
        assert_eq!(Target::parse("https://example.com").redacted, "https://example.com");
    }

    #[tokio::test]
    async fn every_attempt_carries_the_trace_and_a_span_id_of_its_own() {
        let fake = Arc::new(FakeTransport::new());
        fake.responding(503, "busy").responding(200, "ok");

        rainier_request_tagging::propagation::scope(Some(UPSTREAM.to_string()), async {
            Http::get("https://api.example.com/v1/things")
                .retry(2, crate::Backoff::fixed(std::time::Duration::ZERO))
                .through(Arc::clone(&fake) as _)
                .send()
                .await
                .unwrap();
        })
        .await;

        let sent: Vec<TraceContext> = fake
            .recorded()
            .iter()
            .map(|r| TraceContext::parse(r.header("traceparent").expect("on the wire")).unwrap())
            .collect();
        assert_eq!(sent.len(), 2);
        assert!(sent.iter().all(|c| c.trace_id() == TRACE), "the request's trace");
        assert_ne!(sent[0].parent_id(), sent[1].parent_id(), "one span per attempt");
        assert_eq!(fake.recorded()[0].header("tracestate"), None, "not taking part");
    }

    #[cfg(feature = "telemetry")]
    #[tokio::test]
    async fn every_attempt_is_a_client_span() {
        let recorder = Recorder::new();
        let _guard = recorder.install();
        let fake = Arc::new(FakeTransport::new());
        fake.responding(503, "busy").responding(200, "ok");

        rainier_request_tagging::propagation::scope(Some(UPSTREAM.to_string()), async {
            Http::get("https://api.example.com/v1/things?api_key=secret")
                .retry(2, crate::Backoff::fixed(std::time::Duration::ZERO))
                .through(Arc::clone(&fake) as _)
                .send()
                .await
                .unwrap();
        })
        .await;

        let spans = recorder.named("http.client.request");
        assert_eq!(spans.len(), 2, "one span per attempt");
        assert_eq!(spans[0].field("http.response.status_code"), Some("503"));
        assert_eq!(spans[0].field("error.type"), Some("503"));
        assert_eq!(spans[1].field("http.response.status_code"), Some("200"));
        assert_eq!(spans[1].field("otel.status_code"), None);
        assert_eq!(spans[1].field("server.address"), Some("api.example.com"));
        assert_eq!(spans[1].field("url.full"), Some("https://api.example.com/v1/things"));
        assert!(!recorder.everything().contains("secret"));
    }

    #[tokio::test]
    async fn a_traceparent_the_caller_set_is_left_alone() {
        let fake = Arc::new(FakeTransport::new());
        rainier_request_tagging::propagation::scope(Some(UPSTREAM.to_string()), async {
            Http::get("https://example.com")
                .header("traceparent", "00-theirs")
                .through(Arc::clone(&fake) as _)
                .send()
                .await
                .unwrap();
        })
        .await;

        assert_eq!(fake.recorded()[0].header("traceparent"), Some("00-theirs"));
    }

    #[tokio::test]
    async fn outside_a_trace_nothing_is_added() {
        let fake = Arc::new(FakeTransport::new());
        Http::get("https://example.com").through(Arc::clone(&fake) as _).send().await.unwrap();

        assert_eq!(fake.recorded()[0].header("traceparent"), None);
        assert_eq!(fake.recorded()[0].header("tracestate"), None);
    }

    #[tokio::test]
    async fn the_span_id_sent_is_recorded_on_the_event() {
        let events = Arc::new(rainier_events::Dispatcher::new());
        let _bus = rainier_events::bus::set_default(Arc::clone(&events));
        let heard = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Arc::clone(&heard);
        events.on::<crate::HttpClientRequestSent>().run_sync(move |sent, _| {
            sink.lock().unwrap().push(rainier_events::Activity::collect_fields(sent));
            Ok(())
        });
        let fake = Arc::new(FakeTransport::new());

        rainier_request_tagging::propagation::scope(Some(UPSTREAM.to_string()), async {
            Http::get("https://b.example.com/x")
                .through(Arc::clone(&fake) as _)
                .send()
                .await
                .unwrap();
        })
        .await;
        events.settled().await;

        let sent = TraceContext::parse(fake.recorded()[0].header("traceparent").unwrap()).unwrap();
        assert_eq!(heard.lock().unwrap()[0].text("rainier.trace.span_id"), Some(sent.parent_id()));
    }
}
