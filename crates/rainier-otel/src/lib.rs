//! OpenTelemetry for Rainier — spans out, to a collector — and the shape of a
//! log line.
//!
//! Request tagging — the trace id on every request and every call it makes —
//! is `rainier-request-tagging`, and needs none of this. This crate adds what only a
//! tracer can: real span ids ([`spans`]), and an OTLP exporter behind the
//! `otlp` feature:
//!
//! ```ignore
//! // bootstrap.rs, behind the `otlp` feature
//! let layer = Otlp::new("http://localhost:4317", "rainier-sample")
//!     .environment("production")
//!     .sample_ratio(0.1)
//!     .layer()?;
//! ```
//!
//! # Sampling belongs upstream
//!
//! A trace that arrives with a decision keeps it, whatever this service is
//! configured to do. A trace sampled in half its services is a trace with holes
//! in it, and holes are worse than no trace at all — you cannot tell a missing
//! span from a call that never happened.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod logging;
pub mod spans;

#[cfg(feature = "otlp")]
pub mod otlp;

pub use logging::LogFormat;
pub use spans::adopt;

#[cfg(feature = "otlp")]
pub use otlp::{shutdown, Otlp};
