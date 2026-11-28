//! # Lookout
//!
//! Rainier's opt-in diagnostics suite. Lookout records what each unit of work
//! did — a request, a job, a command, a scheduled task — as a **batch** of
//! entries: the queries it ran, the cache it touched, the calls it made out,
//! the jobs it queued, the failures it reported, how long each took. You open
//! `/_lookout` and look.
//!
//! - **Capture** is one subscriber on the event bus, on threads of its own
//!   (`ingest.threads`): the activities the framework publishes about its own
//!   work (queries, cache calls, requests, jobs — named as the framework's
//!   spans are, whether or not OpenTelemetry is compiled in), failures, model
//!   changes, and the end of each unit of work.
//! - **Scope** is per route, middleware group, guard and user, decided once
//!   per batch; every watcher can be toggled and sampled on its own.
//! - **Nothing waits on Lookout.** The request path only publishes onto the
//!   bus, which is a lock-free push; batches are built on Lookout's own
//!   thread, and closed ones go over a bounded channel to a background
//!   writer, dropped and counted when it is full.
//! - **Storage** is pluggable ([`store::Store`]): in memory, Lookout's own
//!   tables with their own migrations, or a ring buffer in the cache.
//!
//! See `docs/lookout.md`.

#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

pub mod aggregate;
pub mod cases;
pub mod client;
pub mod commands;
pub mod config;
pub mod database;
pub mod entry;
pub mod facets;
pub mod http;
#[cfg(feature = "mcp")]
pub mod mcp;
pub mod middleware;
pub mod queues;
pub mod recorder;
pub mod redact;
pub mod rum;
pub mod rum_rollup;
pub mod store;
pub mod subscriber;
#[cfg(test)]
pub(crate) mod testing;
pub mod trends;
pub mod user;
pub mod watchers;

pub use config::{LongmireReach, LookoutConfig};
pub use entry::{Aggregate, Batch, BatchKind, BatchSummary, Entry, EntryType};
pub use http::{routes, LookoutGate, LINK_VIEWER_ROUTES, READER_ROUTES, TRACING_READER_ROUTES};
pub use middleware::{LookoutClientMiddleware, LookoutMiddleware};
pub use recorder::{Lookout, WriterHandle};
pub use store::cache::CacheStore;
pub use store::database::DatabaseStore;
pub use store::memory::MemoryStore;
pub use store::{AggregateOrder, AggregateQuery, BatchQuery, Store};
pub use user::LookoutUser;
