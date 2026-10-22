//! An `http.client.request` span per attempt, and `traceparent` on the wire —
//! behind the `telemetry` feature.
//!
//! Applied inside the **transports**, not the [`PendingRequest`](crate::PendingRequest)
//! builder, because the builder is not the only way out: the mail APIs, the
//! Stripe client and the SSO token exchange each hand an
//! [`OutboundRequest`] straight to a transport. Instrumenting the socket is
//! what makes every one of them visible without any of them knowing.
//!
//! The fake is instrumented too. It records what *would have left the
//! building*, and with telemetry on, that includes the `traceparent` header —
//! so a test can assert the trace reaches the other service.
//!
//! # What is recorded
//!
//! `http.request.method`, `server.address`, `server.port`, `url.scheme` and
//! `url.full` **without its query string or credentials** — a query is where
//! an API key or a signature goes, and `user:pass@` is a credential by
//! definition. `http.response.status_code`, and `error.type` for a 4xx or 5xx
//! (the status) or a transport failure (the error's kind). One span per
//! attempt, as the OpenTelemetry conventions ask, so three retries are three
//! spans and the backoff between them is the gap you can see.

use rainier_support::{BoxFuture, Result};

use crate::transport::{OutboundRequest, RawResponse};

/// Wrap one send in a client span, writing `traceparent` onto the request.
pub(crate) fn instrument<'a>(
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

    // A caller that set its own keeps it: it is propagating a trace this
    // process does not know about, which is its business.
    if !request.headers.contains_key("traceparent") {
        if let Some(header) = rainier_telemetry::propagation::for_span(&span) {
            request.headers.insert("traceparent".to_string(), header);
        }
    }

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
            result
        },
        span,
    ))
}

/// The parts of a URL a span may carry.
#[derive(Debug, PartialEq, Eq)]
struct Target {
    scheme: String,
    host: String,
    port: u16,
    /// `scheme://host[:port]/path` — no query, no fragment, no userinfo.
    redacted: String,
}

impl Target {
    /// A forgiving parse: a URL this cannot make sense of becomes an empty
    /// host rather than a failed request — the span is not worth the call.
    fn parse(url: &str) -> Self {
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
    use std::sync::Arc;

    use super::*;
    use crate::{FakeTransport, Http};
    use rainier_support::telemetry::testing::Recorder;

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
    async fn every_attempt_is_a_client_span_and_the_trace_goes_on_the_wire() {
        let recorder = Recorder::new();
        let _guard = recorder.install();
        let fake = Arc::new(FakeTransport::new());
        fake.responding(503, "busy").responding(200, "ok");

        let upstream = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        rainier_telemetry::propagation::scope(Some(upstream.to_string()), async {
            Http::get("https://api.example.com/v1/things?api_key=secret")
                .retry(2, crate::Backoff::fixed(std::time::Duration::ZERO))
                .through(Arc::clone(&fake) as _)
                .send()
                .await
                .unwrap();
        })
        .await;

        let sent = fake.recorded();
        assert_eq!(sent.len(), 2);
        for request in &sent {
            assert_eq!(request.header("traceparent"), Some(upstream), "the request's trace");
        }

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
        rainier_telemetry::propagation::scope(Some("00-mine".to_string()), async {
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
    }
}
