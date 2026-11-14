//! # Lookout
//!
//! Rainier's opt-in diagnostics suite. Lookout records what each unit of work
//! did — a request, a job, a command, a scheduled task — as a **batch** of
//! entries: the queries it ran, the cache it touched, the calls it made out,
//! the jobs it queued, the failures it reported, how long each took. You open
//! `/_lookout` and look.
//!
//! - **Capture** reads the spans the framework already emits (the
//!   `telemetry` span features, without the OTLP exporter) through a
//!   `tracing` layer, and listens on the event bus for what spans do not
//!   carry: failures, model changes, job outcomes.
//! - **Scope** is per route, middleware group, guard and user, decided once
//!   per batch; every watcher can be toggled and sampled on its own.
//! - **Nothing waits on Lookout.** The request path only appends to a buffer;
//!   closed batches go over a bounded channel to a background writer, and are
//!   dropped and counted when it is full.
//! - **Storage** is pluggable ([`store::Store`]): in memory, Lookout's own
//!   tables with their own migrations, or a ring buffer in the cache.
//!
//! See `docs/lookout.md`.

#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

pub mod aggregate;
pub mod commands;
pub mod config;
pub mod entry;
pub mod http;
pub mod layer;
pub mod middleware;
pub mod recorder;
pub mod redact;
pub mod store;
pub mod watchers;

pub use config::LookoutConfig;
pub use entry::{Aggregate, Batch, BatchKind, BatchSummary, Entry, EntryType};
pub use http::{routes, LookoutGate};
pub use layer::LookoutLayer;
pub use middleware::LookoutMiddleware;
pub use recorder::{Lookout, WriterHandle};
pub use store::cache::CacheStore;
pub use store::database::DatabaseStore;
pub use store::memory::MemoryStore;
pub use store::{AggregateOrder, AggregateQuery, BatchQuery, Store};
