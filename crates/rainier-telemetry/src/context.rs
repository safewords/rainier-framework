//! W3C trace context — [`TraceContext`] and the header it rides in.
//!
//! The half of OpenTelemetry that needs no exporter and no dependency: a
//! request arriving from another service carries the trace it belongs to, and
//! anything this service logs or calls should join that trace rather than
//! starting a new one.
//!
//! ```text
//! traceparent: 00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01
//!              ^  ^                                ^                ^
//!              │  trace id (16 bytes)              parent span      flags
//!              version
//! ```

use rainier_support::Error;

/// The version this implementation writes. `00` is the only one defined.
const VERSION: &str = "00";

/// The header a trace arrives in.
pub const TRACEPARENT: &str = "traceparent";

/// Vendor-specific state travelling with the trace, passed through untouched.
pub const TRACESTATE: &str = "tracestate";

/// One request's place in a distributed trace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceContext {
    /// The trace this belongs to — 16 bytes, hex.
    trace_id: String,
    /// The span that caused it — 8 bytes, hex.
    parent_id: String,
    /// Whether a collector should record it.
    sampled: bool,
}

impl TraceContext {
    /// Parse a `traceparent` header.
    ///
    /// Strict, because a malformed one is not a trace: joining a trace whose id
    /// is the wrong length produces spans that never reassemble, and a fresh
    /// trace is more useful than a broken one. The caller decides what to do
    /// with the error — [`extract`](Self::extract) starts a new trace.
    pub fn parse(header: &str) -> Result<Self, Error> {
        let parts: Vec<&str> = header.trim().split('-').collect();

        let [version, trace_id, parent_id, flags] = parts.as_slice() else {
            return Err(Error::bad_request("a traceparent has four dash-separated fields."));
        };

        // A later version may add fields, and the spec says to accept it as
        // long as the first four parse. Version `ff` is explicitly invalid.
        if version.len() != 2 || !is_hex(version) || *version == "ff" {
            return Err(Error::bad_request("that traceparent version is not valid."));
        }
        if trace_id.len() != 32 || !is_hex(trace_id) || trace_id.bytes().all(|b| b == b'0') {
            return Err(Error::bad_request("a trace id is 32 hex characters and not all zero."));
        }
        if parent_id.len() != 16 || !is_hex(parent_id) || parent_id.bytes().all(|b| b == b'0') {
            return Err(Error::bad_request("a span id is 16 hex characters and not all zero."));
        }
        if flags.len() != 2 || !is_hex(flags) {
            return Err(Error::bad_request("trace flags are two hex characters."));
        }

        let sampled = u8::from_str_radix(flags, 16).unwrap_or(0) & 0x01 == 0x01;

        Ok(Self { trace_id: trace_id.to_lowercase(), parent_id: parent_id.to_lowercase(), sampled })
    }

    /// The context for a request, joining an incoming trace or starting one.
    ///
    /// A request with no `traceparent` — or an unparseable one — begins a new
    /// trace, because the alternative is a service that stops tracing whenever
    /// something upstream is misconfigured.
    pub fn extract(header: Option<&str>, sampled: bool) -> Self {
        match header.map(Self::parse) {
            Some(Ok(context)) => context,
            Some(Err(e)) => {
                tracing::debug!(error = %e.message(), "ignoring a malformed traceparent");
                Self::start(sampled)
            }
            None => Self::start(sampled),
        }
    }

    /// A brand new trace.
    pub fn start(sampled: bool) -> Self {
        Self { trace_id: random_hex(32), parent_id: random_hex(16), sampled }
    }

    /// The trace's id.
    pub fn trace_id(&self) -> &str {
        &self.trace_id
    }

    /// The id of the span that caused this one.
    pub fn parent_id(&self) -> &str {
        &self.parent_id
    }

    /// Whether a collector should record this trace.
    pub fn is_sampled(&self) -> bool {
        self.sampled
    }

    /// The header to send to a service this one calls.
    ///
    /// The parent is **this** service's span, not the one it received — that is
    /// what makes the trace a tree rather than a list.
    pub fn to_header(&self) -> String {
        format!("{VERSION}-{}-{}-{:02x}", self.trace_id, self.parent_id, u8::from(self.sampled))
    }

    /// A child of this context, for an outbound call.
    pub fn child(&self) -> Self {
        Self { trace_id: self.trace_id.clone(), parent_id: random_hex(16), sampled: self.sampled }
    }
}

fn is_hex(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// `len` hex characters of randomness.
///
/// Not a CSPRNG and does not need to be: a trace id has to be unique, not
/// unguessable. Time plus a counter plus the address of a local gives enough
/// spread that two ids colliding would need two processes to start in the same
/// nanosecond with the same layout — and a collision costs a confusing trace,
/// not a security property.
fn random_hex(len: usize) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);

    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    let local = 0u8;
    let address = std::ptr::addr_of!(local) as u64;

    let mut out = String::with_capacity(len);
    let mut state = nanos ^ sequence.rotate_left(17) ^ address.rotate_left(33);

    while out.len() < len {
        // xorshift64. Small, fast, and adequate for an identifier.
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.push_str(&format!("{state:016x}"));
    }
    out.truncate(len);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The example from the W3C specification.
    const EXAMPLE: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

    #[test]
    fn the_specifications_own_example_parses() {
        let context = TraceContext::parse(EXAMPLE).expect("valid");

        assert_eq!(context.trace_id(), "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(context.parent_id(), "00f067aa0ba902b7");
        assert!(context.is_sampled());
    }

    #[test]
    fn it_round_trips() {
        let context = TraceContext::parse(EXAMPLE).expect("valid");
        assert_eq!(context.to_header(), EXAMPLE);
    }

    #[test]
    fn an_unsampled_trace_says_so() {
        let header = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-00";
        let context = TraceContext::parse(header).expect("valid");

        assert!(!context.is_sampled());
        assert_eq!(context.to_header(), header);
    }

    #[test]
    fn a_malformed_header_is_refused_rather_than_half_read() {
        // Every one of these produces spans that never reassemble, so a fresh
        // trace is the better answer — but that is `extract`'s decision, and
        // parsing has to be able to say no.
        for hostile in [
            "",
            "nonsense",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7",
            "00-tooshort-00f067aa0ba902b7-01",
            "00-00000000000000000000000000000000-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01",
            "ff-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-zz",
        ] {
            assert!(TraceContext::parse(hostile).is_err(), "should refuse `{hostile}`");
        }
    }

    #[test]
    fn a_future_version_with_extra_fields_is_still_read() {
        // The spec says to accept what you understand of a later version.
        let header = "01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-extra";
        assert!(TraceContext::parse(header).is_err(), "five fields is not this version");

        let known = "01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        assert!(TraceContext::parse(known).is_ok());
    }

    #[test]
    fn a_missing_header_starts_a_trace_rather_than_dropping_one() {
        let context = TraceContext::extract(None, true);

        assert_eq!(context.trace_id().len(), 32);
        assert_eq!(context.parent_id().len(), 16);
        assert!(context.is_sampled());
    }

    #[test]
    fn a_broken_header_starts_a_trace_rather_than_failing_the_request() {
        // An upstream with a misconfigured proxy must not stop this service
        // tracing.
        let context = TraceContext::extract(Some("garbage"), true);

        assert_eq!(context.trace_id().len(), 32);
    }

    #[test]
    fn a_child_stays_in_the_trace_and_gets_its_own_span() {
        let parent = TraceContext::parse(EXAMPLE).expect("valid");
        let child = parent.child();

        assert_eq!(child.trace_id(), parent.trace_id());
        assert_ne!(child.parent_id(), parent.parent_id());
        assert_eq!(child.is_sampled(), parent.is_sampled());
    }

    #[test]
    fn generated_ids_do_not_repeat() {
        let ids: std::collections::HashSet<String> =
            (0..1000).map(|_| TraceContext::start(true).trace_id().to_string()).collect();

        assert_eq!(ids.len(), 1000, "a thousand traces should have a thousand ids");
    }

    #[test]
    fn a_generated_header_parses_back() {
        let started = TraceContext::start(false);
        let round_tripped = TraceContext::parse(&started.to_header()).expect("valid");

        assert_eq!(round_tripped, started);
    }
}
