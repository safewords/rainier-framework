//! What this crate publishes on the event bus: [`HttpClientRequestSent`], one
//! per attempt a transport makes.

use std::backtrace::Backtrace;
use std::sync::Arc;
use std::time::Duration;

use rainier_events::{code_trace, Activity, Dispatcher, Fields};
use rainier_support::{ErrorKind, Result};

use crate::telemetry::Target;
use crate::transport::RawResponse;

/// One attempt at an outgoing request: kind `http.client.request`, with the
/// fields the span of that name records.
///
/// Published by every transport — the real one and the fake — whether or not
/// the `telemetry` feature is on. A retried request is one event per attempt.
/// Like the span, the URL carries **no query string and no credentials**.
#[derive(Debug, Clone)]
pub struct HttpClientRequestSent {
    /// `http.request.method`, upper-cased.
    pub method: String,
    /// `url.scheme`.
    pub scheme: String,
    /// `server.address`.
    pub host: String,
    /// `server.port`.
    pub port: u16,
    /// `url.full`: `scheme://host[:port]/path` — no query, no fragment, no
    /// userinfo.
    pub url: String,
    /// `http.response.status_code`, when a response came back.
    pub status: Option<u16>,
    /// The error's kind, when the transport failed rather than answering.
    pub error: Option<ErrorKind>,
    /// How long the attempt took.
    pub duration: Duration,
    /// Where it was sent from, when it was slow enough for a subscriber to
    /// have asked ([`code_trace`]).
    pub code_trace: Option<Arc<Backtrace>>,
    /// The span id this attempt named as the callee's parent in its
    /// `traceparent`, when it carried one — `rainier.trace.span_id`. A
    /// recorder on the other side sees the same id as its work's parent.
    pub trace_span_id: Option<String>,
    /// What a callee taking part in keeping traces whole answered: whether it
    /// kept its part (`rainier.trace.peer_kept`), and the span its work hangs
    /// from (`rainier.trace.peer_span_id`). See [`crate::telemetry`].
    pub peer: Option<(bool, String)>,
}

impl HttpClientRequestSent {
    /// Describe an attempt at `method url` that ended in `result`.
    ///
    /// Takes the code trace here, on the thread that sent it.
    pub fn new(method: &str, url: &str, result: &Result<RawResponse>, duration: Duration) -> Self {
        let Target { scheme, host, port, redacted } = Target::parse(url);
        let method = method.to_ascii_uppercase();
        let code_trace = code_trace::capture(duration, || format!("{method} {redacted}"));
        Self {
            method,
            scheme,
            host,
            port,
            url: redacted,
            status: result.as_ref().ok().map(|response| response.status),
            error: result.as_ref().err().map(|error| error.kind()),
            duration,
            code_trace,
            trace_span_id: None,
            peer: None,
        }
    }

    /// The trace this attempt carried, and what the callee said about it.
    pub fn with_trace(mut self, span_id: Option<String>, peer: Option<(bool, String)>) -> Self {
        self.trace_span_id = span_id;
        self.peer = peer;
        self
    }
}

impl Activity for HttpClientRequestSent {
    fn kind(&self) -> &'static str {
        "http.client.request"
    }

    fn fields(&self, fields: &mut Fields) {
        fields
            .set("otel.name", self.method.clone())
            .set("otel.kind", "client")
            .set("http.request.method", self.method.clone())
            .set("server.address", self.host.clone())
            .set("server.port", self.port)
            .set("url.scheme", self.scheme.clone())
            .set("url.full", self.url.clone())
            .maybe("http.response.status_code", self.status)
            .maybe("rainier.trace.span_id", self.trace_span_id.clone());
        if let Some((kept, span)) = &self.peer {
            fields
                .set("rainier.trace.peer_kept", *kept)
                .set("rainier.trace.peer_span_id", span.clone());
        }
        match (self.status, self.error) {
            (Some(status), _) if status >= 400 => {
                fields.set("otel.status_code", "ERROR").set("error.type", status.to_string());
            }
            (_, Some(kind)) => {
                fields.set("otel.status_code", "ERROR").set("error.type", kind.name());
            }
            _ => {}
        }
    }

    fn duration(&self) -> Option<Duration> {
        Some(self.duration)
    }

    fn failed(&self) -> bool {
        self.error.is_some() || self.status.is_some_and(|status| status >= 400)
    }

    fn code_trace(&self) -> Option<&Arc<Backtrace>> {
        self.code_trace.as_ref()
    }
}

/// The bus, when something on it would hear an [`HttpClientRequestSent`].
#[inline]
pub(crate) fn listening() -> Option<Arc<Dispatcher>> {
    // Asked first by borrowing the bus — no lock, no reference count — so
    // the answer "nobody" costs an atomic load; only a yes clones it.
    if !rainier_events::activity::has_activity_subscribers::<HttpClientRequestSent>() {
        return None;
    }
    rainier_events::bus()
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use rainier_events::Value;

    use super::*;
    use crate::{Backoff, FakeTransport, Http};

    fn heard(events: &Dispatcher) -> Arc<Mutex<Vec<(Fields, bool, bool)>>> {
        let heard = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&heard);
        events.on::<HttpClientRequestSent>().run_sync(move |sent, _| {
            sink.lock().unwrap().push((
                sent.collect_fields(),
                sent.failed(),
                sent.code_trace.is_some(),
            ));
            Ok(())
        });
        heard
    }

    #[tokio::test]
    async fn every_attempt_is_published_with_the_spans_fields_and_never_the_query() {
        let events = Arc::new(Dispatcher::new());
        let _bus = rainier_events::bus::set_default(Arc::clone(&events));
        let heard = heard(&events);
        let fake = Arc::new(FakeTransport::new());
        fake.responding(503, "busy").responding(200, "ok");

        Http::get("https://user:hunter2@api.example.com:8443/v1/things?api_key=secret")
            .retry(2, Backoff::fixed(Duration::ZERO))
            .through(Arc::clone(&fake) as _)
            .send()
            .await
            .unwrap();
        events.settled().await;

        let heard = heard.lock().unwrap();
        assert_eq!(heard.len(), 2, "one event per attempt");
        let (busy, failed, _) = &heard[0];
        assert!(failed);
        assert_eq!(busy.get("http.response.status_code"), Some(&Value::U64(503)));
        assert_eq!(busy.text("otel.status_code"), Some("ERROR"));
        assert_eq!(busy.text("error.type"), Some("503"));
        let (ok, failed, _) = &heard[1];
        assert!(!failed);
        assert_eq!(ok.text("otel.name"), Some("GET"));
        assert_eq!(ok.text("otel.kind"), Some("client"));
        assert_eq!(ok.text("http.request.method"), Some("GET"));
        assert_eq!(ok.text("server.address"), Some("api.example.com"));
        assert_eq!(ok.get("server.port"), Some(&Value::U64(8443)));
        assert_eq!(ok.text("url.scheme"), Some("https"));
        assert_eq!(ok.text("url.full"), Some("https://api.example.com:8443/v1/things"));
        assert_eq!(ok.text("otel.status_code"), None);
        let everything = format!("{heard:?}");
        assert!(!everything.contains("secret"), "never the query");
        assert!(!everything.contains("hunter2"), "never the credentials");
    }

    #[tokio::test]
    async fn a_slow_attempt_carries_a_code_trace_when_one_was_asked_for() {
        let events = Arc::new(Dispatcher::new());
        let _bus = rainier_events::bus::set_default(Arc::clone(&events));
        let heard = heard(&events);
        let fake = Arc::new(FakeTransport::new());

        code_trace::request_code_traces(Duration::ZERO);
        Http::get("https://example.com/a").through(Arc::clone(&fake) as _).send().await.unwrap();
        code_trace::reset();
        events.settled().await;

        assert!(heard.lock().unwrap()[0].2);
    }

    #[tokio::test]
    async fn with_nobody_subscribed_nothing_is_built() {
        let events = Arc::new(Dispatcher::new());
        let _bus = rainier_events::bus::set_default(Arc::clone(&events));
        events.on::<String>().run_sync(|_, _| Ok(()));
        assert!(listening().is_none());

        let fake = Arc::new(FakeTransport::new());
        Http::get("https://example.com").through(Arc::clone(&fake) as _).send().await.unwrap();
        assert_eq!(fake.recorded().len(), 1);
    }
}
