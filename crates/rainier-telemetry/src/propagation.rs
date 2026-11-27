//! Carrying a trace across an await, a socket and a queue — [`current`],
//! [`scope`] and [`adopt`].
//!
//! The [`Trace`](crate::Trace) middleware knows which trace a request belongs
//! to. The outbound HTTP client and the queue, three crates away, need to know
//! it too — to write `traceparent` onto a call to another service, or into a
//! job's payload so the worker that runs it joins the same trace. Threading a
//! context argument through every call between them would touch every
//! signature in the framework, so the context rides **beside** the future
//! instead:
//!
//! - [`scope`] runs a future with a `traceparent` in force.
//! - [`current`] reads the one in force — what an outbound call should send.
//! - [`adopt`] makes a span the child of a remote parent, so a job's span joins
//!   the trace of the request that queued it.
//!
//! # Two sources, best first
//!
//! With the `otlp` feature and the exporter installed, every span has a real
//! OpenTelemetry span context, and [`current`] answers from the **current
//! span** — so a downstream service's span hangs off the `HTTP GET` client
//! span that called it, which is the tree a trace viewer should draw.
//!
//! Without it there are no span ids to cite, and [`current`] answers from the
//! scope: the trace id the request arrived with, and the span id this service
//! minted for itself. One level shallower, the same trace — which is what lets
//! `grep <trace id>` find every log line across every service, with no
//! collector at all.
//!
//! # Why not a tokio task-local
//!
//! It would work, and it would make this crate — and through it the HTTP
//! client — depend on tokio's runtime for a value that only has to survive a
//! `poll`. [`Scoped`] sets a thread-local around each poll of the future it
//! wraps and restores the previous one after, which is all a task-local does.

use std::cell::RefCell;
use std::future::Future;
use std::mem::DropGuard;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use crate::context::TraceContext;

thread_local! {
    static CURRENT: RefCell<Option<Arc<str>>> = const { RefCell::new(None) };
}

/// The `traceparent` an outbound call made right now should carry.
///
/// `None` outside any trace — a console command, a test, a process with
/// telemetry off — and an outbound call then sends no header, which is what it
/// sent before any of this existed.
pub fn current() -> Option<String> {
    #[cfg(feature = "otlp")]
    if let Some(header) = otel::current_span_header() {
        return Some(header);
    }

    CURRENT.with(|current| current.borrow().as_deref().map(str::to_string))
}

/// The `traceparent` naming `span` itself as the parent, when it has a real
/// span context; otherwise whatever [`current`] would say.
///
/// What an outbound call puts on the wire: the client span it opened, so the
/// server it reaches hangs off that span rather than off the request that
/// caused it.
pub fn for_span(span: &tracing::Span) -> Option<String> {
    #[cfg(feature = "otlp")]
    if let Some(header) = otel::header_of(span) {
        return Some(header);
    }

    #[cfg(not(feature = "otlp"))]
    let _ = span;

    CURRENT.with(|current| current.borrow().as_deref().map(str::to_string))
}

/// The `traceparent` one outbound call should carry: the same trace, and a
/// span id that names **this call** and no other.
///
/// With a real span context (the `otlp` feature, exporter installed) that is
/// `span`'s own id — the client span the call opened. Without one it is a
/// fresh span id under the trace in force ([`current`]), or, outside any
/// [`scope`], under the trace of the current unit of work — a job, a
/// scheduled task — when it has one.
///
/// A fresh id per call is what lets a recorder on the other side say which of
/// several calls to it a piece of its work answered: two calls from one
/// request to the same service would otherwise name the same parent.
///
/// `None` outside any trace, and the call then sends no header.
pub fn outbound(span: &tracing::Span) -> Option<String> {
    #[cfg(feature = "otlp")]
    if let Some(header) = otel::header_of(span) {
        return Some(header);
    }

    #[cfg(not(feature = "otlp"))]
    let _ = span;

    let in_force = CURRENT.with(|current| current.borrow().as_deref().map(str::to_string));
    let in_force = in_force.or_else(|| {
        let unit = rainier_events::Context::current_unit()?;
        match (unit.traceparent(), unit.trace_id()) {
            (Some(parent), _) => Some(parent.to_string()),
            // A trace id with no header: a job that learned only the id.
            (None, Some(id)) => Some(format!("00-{id}-0000000000000001-01")),
            (None, None) => None,
        }
    })?;

    Some(match TraceContext::parse(&in_force) {
        Ok(context) => context.child().to_header(),
        // Not ours to fix: pass on what is in force.
        Err(_) => in_force,
    })
}

/// Run `future` with `traceparent` in force for [`current`].
///
/// `None` clears it, which is what a job running for a request with no trace
/// should see — not the trace of whatever ran on this thread before.
pub fn scope<F: Future>(traceparent: Option<String>, future: F) -> Scoped<F> {
    Scoped { traceparent: traceparent.map(Arc::from), inner: Box::pin(future) }
}

/// Make `span` a child of the remote span `traceparent` names.
///
/// Must be called **before** the span is first entered: OpenTelemetry fixes a
/// span's parent when it starts. A no-op without the `otlp` feature, or when
/// the header does not parse — a broken parent is not a reason to lose the
/// span.
pub fn adopt(span: &tracing::Span, traceparent: &str) {
    #[cfg(feature = "otlp")]
    otel::adopt(span, traceparent);

    #[cfg(not(feature = "otlp"))]
    let _ = (span, traceparent);
}

/// The header for **this** service's work inside a trace that arrived as
/// `incoming`: the same trace, a span id of its own.
///
/// What a [`scope`] should hold for a request or a job: anything it calls
/// names this service as its parent rather than the caller's caller.
pub fn child_of(incoming: &TraceContext) -> String {
    incoming.child().to_header()
}

/// A future with a `traceparent` in force — see [`scope`].
pub struct Scoped<F> {
    traceparent: Option<Arc<str>>,
    inner: Pin<Box<F>>,
}

impl<F: Future> Future for Scoped<F> {
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mine = self.traceparent.clone();
        let previous = CURRENT.with(|current| current.replace(mine));

        // Restored even if the inner future panics, so a caught panic does not
        // leave this trace in force for whatever the thread polls next.
        let _restore = DropGuard::new(previous, |previous| {
            CURRENT.with(|current| *current.borrow_mut() = previous);
        });

        self.inner.as_mut().poll(cx)
    }
}

impl<F> std::fmt::Debug for Scoped<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scoped").field("traceparent", &self.traceparent).finish()
    }
}

#[cfg(feature = "otlp")]
mod otel {
    use opentelemetry::trace::{
        SpanContext, SpanId, TraceContextExt, TraceFlags, TraceId, TraceState,
    };
    use tracing_opentelemetry::OpenTelemetrySpanExt;

    use crate::context::TraceContext;

    /// The current span's own context, as a header — when it has one.
    pub(super) fn current_span_header() -> Option<String> {
        header_of(&tracing::Span::current())
    }

    pub(super) fn header_of(span: &tracing::Span) -> Option<String> {
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

    pub(super) fn adopt(span: &tracing::Span, traceparent: &str) {
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

    const EXAMPLE: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

    #[tokio::test]
    async fn a_scope_is_in_force_across_awaits_and_gone_after() {
        assert_eq!(current(), None);

        let seen = scope(Some(EXAMPLE.to_string()), async {
            tokio::task::yield_now().await;
            current()
        })
        .await;

        assert_eq!(seen.as_deref(), Some(EXAMPLE));
        assert_eq!(current(), None, "restored once the future is done");
    }

    #[tokio::test]
    async fn an_inner_scope_wins_and_the_outer_one_returns() {
        let (inner, outer) = scope(Some("outer".to_string()), async {
            let inner = scope(Some("inner".to_string()), async { current() }).await;
            (inner, current())
        })
        .await;

        assert_eq!(inner.as_deref(), Some("inner"));
        assert_eq!(outer.as_deref(), Some("outer"));
    }

    #[tokio::test]
    async fn a_none_scope_clears_rather_than_inherits() {
        let seen = scope(Some(EXAMPLE.to_string()), scope(None, async { current() })).await;
        assert_eq!(seen, None);
    }

    #[test]
    fn a_child_keeps_the_trace_and_mints_its_own_span() {
        let incoming = TraceContext::parse(EXAMPLE).unwrap();
        let child = TraceContext::parse(&child_of(&incoming)).unwrap();

        assert_eq!(child.trace_id(), incoming.trace_id());
        assert_ne!(child.parent_id(), incoming.parent_id());
        assert_eq!(child.is_sampled(), incoming.is_sampled());
    }

    #[tokio::test]
    async fn every_outbound_call_names_a_span_of_its_own_in_the_same_trace() {
        let (first, second) = scope(Some(EXAMPLE.to_string()), async {
            (outbound(&tracing::Span::none()), outbound(&tracing::Span::none()))
        })
        .await;
        let (first, second) = (
            TraceContext::parse(&first.unwrap()).unwrap(),
            TraceContext::parse(&second.unwrap()).unwrap(),
        );

        assert_eq!(first.trace_id(), "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(second.trace_id(), first.trace_id());
        assert_ne!(first.parent_id(), second.parent_id(), "one span per call");
        assert_ne!(first.parent_id(), "00f067aa0ba902b7", "not the scope's own");
    }

    #[tokio::test]
    async fn outside_a_scope_the_units_trace_is_used() {
        assert_eq!(outbound(&tracing::Span::none()), None, "no trace, no header");

        let unit = rainier_events::Unit::new(rainier_events::UnitKind::Job, None, None);
        unit.set_trace(Some("4bf92f3577b34da6a3ce929d0e0e4736"), None);
        let header = unit.scope(async { outbound(&tracing::Span::none()) }).await;
        assert_eq!(
            TraceContext::parse(&header.unwrap()).unwrap().trace_id(),
            "4bf92f3577b34da6a3ce929d0e0e4736"
        );
    }

    #[test]
    fn adopting_a_broken_parent_is_harmless() {
        adopt(&tracing::Span::none(), "garbage");
        adopt(&tracing::Span::none(), EXAMPLE);
    }
}
