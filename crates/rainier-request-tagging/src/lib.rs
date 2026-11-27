//! Request tagging — a trace id on every request, and on every call it makes.
//!
//! The whole of it is the W3C trace context headers:
//!
//! - [`Trace`], the middleware, gives each request a trace: the one its
//!   caller's `traceparent` names, the id a frontend sent
//!   ([`Trace::request_id_header`]), or a new one. The id goes on every log
//!   line and on the request's unit of work, and back on the response.
//! - [`propagation`] keeps that trace in force across every `.await` of the
//!   request, and gives each outbound call a `traceparent` with a span id of
//!   its own ([`propagation::outbound`]).
//! - [`outbound`] is what an HTTP client calls before each attempt, to write
//!   the header.
//!
//! That is all it does. It records nothing and decides nothing about what is
//! kept; it knows no tracer and no exporter. OpenTelemetry
//! (`rainier-otel`) and Lookout's request tracing (`rainier-lookout`)
//! build on it.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod context;
pub mod middleware;
pub mod outbound;
pub mod propagation;

pub use context::{TraceContext, TRACEPARENT, TRACESTATE};
pub use middleware::{Trace, TraceState};
