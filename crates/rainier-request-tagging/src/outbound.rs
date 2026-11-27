//! Tagging one outbound call — what an HTTP client calls before each attempt.
//!
//! [`tag`] writes `traceparent` onto the call's headers: the trace in force and
//! a span id naming this call ([`propagation::outbound`]), unless the caller
//! set its own. That is all: the service it reaches joins the trace, and
//! whoever records both sides can tell which call that work answered.
//!
//! [`propagation::outbound`]: crate::propagation::outbound

use std::collections::BTreeMap;

use crate::context::{TraceContext, TRACEPARENT};

/// What one attempt carried.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Tagged {
    /// The `traceparent` it carried, when it carried one.
    pub traceparent: Option<String>,
}

impl Tagged {
    /// Whether the attempt carried no trace.
    pub fn is_empty(&self) -> bool {
        self.traceparent.is_none()
    }

    /// The span id this attempt named as the callee's parent.
    pub fn span_id(&self) -> Option<String> {
        let context = TraceContext::parse(self.traceparent.as_deref()?).ok()?;
        Some(context.parent_id().to_string())
    }
}

/// Write the trace onto an outbound call's `headers`: `traceparent` naming
/// this call, unless the caller set its own.
///
/// `span` is the call's own span, if the client opened one: with a span
/// source ([`set_span_source`](crate::propagation::set_span_source)) its id
/// is the one sent.
pub fn tag(headers: &mut BTreeMap<String, String>, span: &tracing::Span) -> Tagged {
    let traceparent = match headers.get(TRACEPARENT) {
        // The caller's own: it propagates a trace this process does not know.
        Some(theirs) => Some(theirs.clone()),
        None => crate::propagation::outbound(span).inspect(|header| {
            headers.insert(TRACEPARENT.to_string(), header.clone());
        }),
    };
    Tagged { traceparent }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UPSTREAM: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

    #[tokio::test]
    async fn a_call_inside_a_trace_names_a_span_of_its_own() {
        let mut headers = BTreeMap::new();
        let tagged = crate::propagation::scope(Some(UPSTREAM.to_string()), async {
            tag(&mut headers, &tracing::Span::none())
        })
        .await;

        let sent = TraceContext::parse(&headers[TRACEPARENT]).unwrap();
        assert_eq!(sent.trace_id(), "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(tagged.span_id().as_deref(), Some(sent.parent_id()));
        assert_ne!(sent.parent_id(), "00f067aa0ba902b7");
    }

    #[test]
    fn outside_a_trace_nothing_is_written() {
        let mut headers = BTreeMap::new();
        assert!(tag(&mut headers, &tracing::Span::none()).is_empty());
        assert!(headers.is_empty());
    }

    #[tokio::test]
    async fn a_traceparent_the_caller_set_is_left_alone() {
        let mut headers: BTreeMap<String, String> =
            [(TRACEPARENT.to_string(), "00-theirs".to_string())].into();
        let tagged = crate::propagation::scope(Some(UPSTREAM.to_string()), async {
            tag(&mut headers, &tracing::Span::none())
        })
        .await;
        assert_eq!(headers[TRACEPARENT], "00-theirs");
        assert_eq!(tagged.traceparent.as_deref(), Some("00-theirs"));
    }
}
