//! Prometheus metrics — a registry, the text format, and request
//! instrumentation.
//!
//! ```ignore
//! // bootstrap.rs
//! Rainier::new(".").with_metrics(Metrics::new())
//!
//! // app/http/kernel.rs — first in the stack, so it times everything
//! MiddlewareStack::new().push(RecordMetrics::new(metrics))
//!
//! // routes/api.rs
//! router.get("/metrics", metrics::endpoint).name("metrics");
//! ```
//!
//! Scrape `/metrics` and you get `http_requests_total`,
//! `http_request_duration_seconds` and whatever else the application recorded.
//!
//! # No client library
//!
//! The Prometheus text exposition format is a dozen lines of rules — a `#
//! HELP`, a `# TYPE`, one line per series, cumulative histogram buckets and a
//! required `+Inf`. Writing it is cheaper than a dependency tree, and it keeps
//! this crate compilable for a wasm target, where a worker exporting metrics
//! over a push gateway still wants the same registry.
//!
//! What that costs: no exemplars, no native histograms, no protobuf exposition.
//! None of them are things a scrape needs.
//!
//! # Cardinality
//!
//! Every distinct label set is a series stored forever. The route label is the
//! **pattern** — `/posts/{post}` — and never the URI, because `/posts/1` and
//! `/posts/2` as separate series is how a one-line change fills a monitoring
//! system's disk. [`RecordMetrics`] enforces that by reading the matched
//! route's pattern rather than the request's path.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod middleware;
pub mod registry;

pub use middleware::{endpoint, RecordMetrics};
pub use registry::{labels, Kind, Labels, Metrics, DEFAULT_BUCKETS};
