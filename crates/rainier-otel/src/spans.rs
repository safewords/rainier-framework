//! OpenTelemetry's side of request tagging: span ids that are real.
//!
//! Request tagging (`rainier-request-tagging`) carries a trace between services on
//! its own. With the `otlp` exporter installed, spans have real
//! OpenTelemetry ids, and two things get better:
//!
//! - an outbound call names **its client span** in `traceparent`, so the
//!   callee's spans hang off it — installed as tagging's span source when the
//!   exporter is ([`Otlp::layer`](crate::Otlp::layer));
//! - [`adopt`] makes a span the child of the remote span an incoming
//!   `traceparent` names — a server's request span, a worker's job span.
//!
//! Without the `otlp` feature both are no-ops: there are no ids to cite.

/// Make `span` a child of the remote span `traceparent` names.
///
/// Must be called **before** the span is first entered: OpenTelemetry fixes a
/// span's parent when it starts. A no-op without the `otlp` feature, or when
/// the header does not parse — a broken parent is not a reason to lose the
/// span.
pub fn adopt(span: &tracing::Span, traceparent: &str) {
    #[cfg(feature = "otlp")]
    otel::adopt_otel(span, traceparent);

    #[cfg(not(feature = "otlp"))]
    let _ = (span, traceparent);
}

#[cfg(feature = "otlp")]
pub(crate) mod otel {
    use opentelemetry::trace::{
        SpanContext, SpanId, TraceContextExt, TraceFlags, TraceId, TraceState,
    };
    use tracing_opentelemetry::OpenTelemetrySpanExt;

    use rainier_request_tagging::TraceContext;

    pub(crate) fn header_of(span: &tracing::Span) -> Option<String> {
        if span.is_disabled() {
            return None;
        }

        let context = span.context();
        let span_context = context.span().span_context().clone();
        if !span_context.is_valid() {
            return None;
        }

        Some(format!(
            "00-{}-{}-{:02x}",
            span_context.trace_id(),
            span_context.span_id(),
            span_context.trace_flags().to_u8()
        ))
    }

    pub(crate) fn adopt_otel(span: &tracing::Span, traceparent: &str) {
        let Ok(incoming) = TraceContext::parse(traceparent) else {
            return;
        };
        let (Ok(trace_id), Ok(span_id)) =
            (TraceId::from_hex(incoming.trace_id()), SpanId::from_hex(incoming.parent_id()))
        else {
            return;
        };

        let flags = if incoming.is_sampled() { TraceFlags::SAMPLED } else { TraceFlags::default() };
        let remote = SpanContext::new(trace_id, span_id, flags, true, TraceState::default());

        // An error means the layer is not installed or the span has started;
        // either way the span is still worth having without its parent.
        let _ = span.set_parent(opentelemetry::Context::new().with_remote_span_context(remote));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adopting_a_broken_parent_is_harmless() {
        adopt(&tracing::Span::none(), "garbage");
        adopt(&tracing::Span::none(), "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01");
    }
}
