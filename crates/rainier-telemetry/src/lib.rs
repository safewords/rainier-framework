//! OpenTelemetry — trace context in, spans out.
//!
//! Two halves, and the first needs no dependency at all:
//!
//! ```ignore
//! // app/http/kernel.rs — outermost, so every log line carries the trace
//! MiddlewareStack::new().push(Trace::new())
//!
//! // bootstrap.rs, behind the `otlp` feature
//! let layer = Otlp::new("http://localhost:4317", "rainier-sample")
//!     .environment("production")
//!     .sample_ratio(0.1)
//!     .layer()?;
//! ```
//!
//! # Propagation is most of the value
//!
//! [`Trace`] reads the W3C `traceparent` header, joins the trace it names, puts
//! the id on every log line emitted while handling the request, and echoes it
//! back on the response. That is enough to follow one request across four
//! services with `grep`, and it costs nothing — no exporter, no collector, no
//! dependency tree.
//!
//! Reach for the [`otlp`] feature when you want the waterfall view. The
//! rest of the time, an id on a log line is what you actually use.
//!
//! # Sampling belongs upstream
//!
//! A trace that arrives with a decision keeps it, whatever this service is
//! configured to do. A trace sampled in half its services is a trace with holes
//! in it, and holes are worse than no trace at all — you cannot tell a missing
//! span from a call that never happened.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod context;
pub mod logging;
pub mod middleware;

#[cfg(feature = "otlp")]
pub mod otlp;

pub use context::{TraceContext, TRACEPARENT, TRACESTATE};
pub use logging::LogFormat;
pub use middleware::{Trace, TraceState};

#[cfg(feature = "otlp")]
pub use otlp::{shutdown, Otlp};
