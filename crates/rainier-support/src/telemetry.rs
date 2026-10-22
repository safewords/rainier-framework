//! The span vocabulary every instrumented crate shares — behind `telemetry`.
//!
//! Each Rainier crate that does something worth timing has a `telemetry`
//! feature of its own, and with it off the crate compiles no span at all. With
//! it on, the crate wraps its real seams — a query, a cache read, a job, a mail
//! send — in a [`tracing`] span named and attributed after the OpenTelemetry
//! semantic conventions, so the OTLP exporter in `rainier-telemetry` turns each
//! into a span a trace viewer already knows how to draw.
//!
//! This module is the part they would otherwise each write: [`span!`](crate::span)
//! declares the two fields an outcome is recorded into, and [`record_error`] /
//! [`record_result`] fill them in.
//!
//! # What an error records, and what it does not
//!
//! `otel.status_code = "ERROR"` and `error.type` — the [`ErrorKind`], in
//! `snake_case`. **Not** the message. A driver's error message is the one place
//! a bound value leaks back out — "Duplicate entry 'ada@example.com' for key
//! 'users_email_unique'" — and a span is exported to a system with a different
//! retention policy and a different set of readers than the database. The
//! message is still on the log line, where it always was.

pub use tracing;

use crate::{Error, ErrorKind};

/// Declare a span with the outcome fields already in place.
///
/// `tracing` can only record a field that was declared when the span was
/// created, so every Rainier span declares `otel.status_code` and `error.type`
/// up front — empty until [`record_error`](crate::telemetry::record_error)
/// fills them.
///
/// ```
/// let span = rainier_support::span!(
///     "cache.get",
///     otel.name = "cache get",
///     cache.hit = rainier_support::telemetry::tracing::field::Empty,
/// );
/// span.record("cache.hit", true);
/// ```
#[macro_export]
macro_rules! span {
    ($name:expr $(, $($fields:tt)*)?) => {
        $crate::telemetry::tracing::info_span!(
            $name,
            otel.status_code = $crate::telemetry::tracing::field::Empty,
            error.type = $crate::telemetry::tracing::field::Empty,
            $($($fields)*)?
        )
    };
}

/// Mark `span` as failed by `error`.
pub fn record_error(span: &tracing::Span, error: &Error) {
    span.record("otel.status_code", "ERROR");
    span.record("error.type", kind_name(error.kind()));
}

/// Mark `span` failed when `result` is an error; leave it alone otherwise.
///
/// Returns `result`, so it can sit at the end of an expression.
pub fn record_result<T>(span: &tracing::Span, result: crate::Result<T>) -> crate::Result<T> {
    if let Err(error) = &result {
        record_error(span, error);
    }
    result
}

/// The `error.type` an [`ErrorKind`] records as.
///
/// Stable, low-cardinality strings — `error.type` is a dimension a backend
/// groups by, so it must not carry anything that varies per request.
pub fn kind_name(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::BadRequest => "bad_request",
        ErrorKind::Unauthenticated => "unauthenticated",
        ErrorKind::Unauthorized => "unauthorized",
        ErrorKind::NotFound => "not_found",
        ErrorKind::MethodNotAllowed => "method_not_allowed",
        ErrorKind::Conflict => "conflict",
        ErrorKind::PayloadTooLarge => "payload_too_large",
        ErrorKind::Validation => "validation",
        ErrorKind::TooManyRequests => "too_many_requests",
        ErrorKind::Internal => "internal",
        ErrorKind::ServiceUnavailable => "service_unavailable",
        ErrorKind::Status(_) => "status",
    }
}

/// A subscriber that records spans, for tests — [`Recorder`](testing::Recorder).
///
/// Every instrumented crate wants to assert "this operation produced a span
/// with these fields, and **not** that one", and none of them should need
/// `tracing-subscriber` for it. Public and `#[doc(hidden)]` so the crates' own
/// tests can reach it; not a stable API.
#[doc(hidden)]
pub mod testing {
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::fmt::Debug;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex, OnceLock};

    use tracing::field::{Field, Visit};
    use tracing::span::{Attributes, Id, Record};
    use tracing::{Event, Metadata, Subscriber};
    use tracing_core::span::Current;

    /// One recorded span: its name, and every field it was given.
    #[derive(Debug, Clone)]
    pub struct RecordedSpan {
        /// The span's name.
        pub name: String,
        /// Its fields, by name.
        pub fields: BTreeMap<String, String>,
        /// The id of the span it was opened inside, if any.
        pub parent: Option<u64>,
        /// Its own id.
        pub id: u64,
        metadata: &'static Metadata<'static>,
    }

    impl RecordedSpan {
        /// A field's value.
        pub fn field(&self, name: &str) -> Option<&str> {
            self.fields.get(name).map(String::as_str)
        }

        /// The name and every field, joined — for "this never appears
        /// anywhere" assertions.
        pub fn everything(&self) -> String {
            let mut out = self.name.clone();
            for (key, value) in &self.fields {
                out.push_str(&format!(" {key}={value}"));
            }
            out
        }
    }

    /// Records the spans opened on the thread it is installed on.
    ///
    /// # Why it is not a subscriber of its own
    ///
    /// `tracing` caches, per callsite and **process-wide**, whether any
    /// subscriber is interested. A thread-scoped subscriber installed while a
    /// test on another thread is registering the same callsite can lose that
    /// race and find the callsite cached as "nobody cares" — a span that
    /// silently never appears, one run in five. So one global subscriber is
    /// installed once, is interested in everything, and hands each span to
    /// whichever recorder is active on the thread that opened it.
    #[derive(Clone, Default)]
    pub struct Recorder {
        spans: Arc<Mutex<Vec<RecordedSpan>>>,
    }

    /// Deactivates a [`Recorder`] when dropped.
    pub struct RecorderGuard {
        _private: (),
    }

    impl Drop for RecorderGuard {
        fn drop(&mut self) {
            ACTIVE.with(|active| {
                active.borrow_mut().pop();
            });
        }
    }

    thread_local! {
        /// The recorders active on this thread, innermost last.
        static ACTIVE: RefCell<Vec<Recorder>> = const { RefCell::new(Vec::new()) };
        /// The spans entered on this thread, innermost last.
        static STACK: RefCell<Vec<(u64, &'static Metadata<'static>)>> =
            const { RefCell::new(Vec::new()) };
    }

    impl Recorder {
        /// A new recorder.
        pub fn new() -> Self {
            Self::default()
        }

        /// Every span recorded so far.
        pub fn spans(&self) -> Vec<RecordedSpan> {
            self.spans.lock().expect("recorder lock").clone()
        }

        /// The names of every span recorded, in order.
        pub fn names(&self) -> Vec<String> {
            self.spans().into_iter().map(|span| span.name).collect()
        }

        /// Every span with this name.
        pub fn named(&self, name: &str) -> Vec<RecordedSpan> {
            self.spans().into_iter().filter(|span| span.name == name).collect()
        }

        /// The first span with this name.
        ///
        /// # Panics
        ///
        /// When there is none, listing the ones there are.
        pub fn span(&self, name: &str) -> RecordedSpan {
            self.named(name)
                .into_iter()
                .next()
                .unwrap_or_else(|| panic!("no `{name}` span; recorded: {:?}", self.names()))
        }

        /// Every span's name and fields, joined — for "this never appears
        /// anywhere" assertions.
        pub fn everything(&self) -> String {
            self.spans().iter().map(RecordedSpan::everything).collect::<Vec<_>>().join("\n")
        }

        /// Run `f` with this recorder active on this thread.
        pub fn record<T>(&self, f: impl FnOnce() -> T) -> T {
            let _guard = self.install();
            f()
        }

        /// Make this recorder active on this thread until the guard drops —
        /// for an `async` test on a current-thread runtime.
        ///
        /// # Panics
        ///
        /// When something else already installed a global subscriber in this
        /// test binary, since then there is nowhere for spans to arrive.
        pub fn install(&self) -> RecorderGuard {
            static INSTALLED: OnceLock<bool> = OnceLock::new();
            let installed = *INSTALLED.get_or_init(|| {
                let ok = tracing::subscriber::set_global_default(Global).is_ok();
                tracing::callsite::rebuild_interest_cache();
                ok
            });
            assert!(installed, "another global subscriber is installed; the recorder sees nothing");

            ACTIVE.with(|active| active.borrow_mut().push(self.clone()));
            RecorderGuard { _private: () }
        }
    }

    fn active() -> Option<Recorder> {
        ACTIVE.with(|active| active.borrow().last().cloned())
    }

    /// The one global subscriber — see [`Recorder`].
    struct Global;

    static NEXT: AtomicU64 = AtomicU64::new(1);

    struct Fields<'a>(&'a mut BTreeMap<String, String>);

    impl Visit for Fields<'_> {
        fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
            self.0.insert(field.name().to_string(), format!("{value:?}"));
        }

        fn record_str(&mut self, field: &Field, value: &str) {
            self.0.insert(field.name().to_string(), value.to_string());
        }
    }

    impl Subscriber for Global {
        fn enabled(&self, _: &Metadata<'_>) -> bool {
            true
        }

        fn new_span(&self, attributes: &Attributes<'_>) -> Id {
            let id = NEXT.fetch_add(1, Ordering::Relaxed);

            if let Some(recorder) = active() {
                let parent = if attributes.is_root() {
                    None
                } else {
                    attributes
                        .parent()
                        .map(Id::into_u64)
                        .or_else(|| STACK.with(|stack| stack.borrow().last().map(|(id, _)| *id)))
                };
                let mut span = RecordedSpan {
                    name: attributes.metadata().name().to_string(),
                    fields: BTreeMap::new(),
                    parent,
                    id,
                    metadata: attributes.metadata(),
                };
                attributes.record(&mut Fields(&mut span.fields));
                recorder.spans.lock().expect("recorder lock").push(span);
            }

            Id::from_u64(id)
        }

        fn record(&self, span: &Id, values: &Record<'_>) {
            let Some(recorder) = active() else { return };
            let mut spans = recorder.spans.lock().expect("recorder lock");
            if let Some(recorded) = spans.iter_mut().find(|recorded| recorded.id == span.into_u64())
            {
                values.record(&mut Fields(&mut recorded.fields));
            }
        }

        fn record_follows_from(&self, _: &Id, _: &Id) {}

        fn event(&self, _: &Event<'_>) {}

        fn enter(&self, span: &Id) {
            let id = span.into_u64();
            let metadata = active().and_then(|recorder| {
                recorder
                    .spans
                    .lock()
                    .expect("recorder lock")
                    .iter()
                    .find(|recorded| recorded.id == id)
                    .map(|recorded| recorded.metadata)
            });
            if let Some(metadata) = metadata {
                STACK.with(|stack| stack.borrow_mut().push((id, metadata)));
            }
        }

        fn exit(&self, span: &Id) {
            STACK.with(|stack| {
                let mut stack = stack.borrow_mut();
                if let Some(position) = stack.iter().rposition(|(id, _)| *id == span.into_u64()) {
                    stack.remove(position);
                }
            });
        }

        fn current_span(&self) -> Current {
            STACK.with(|stack| match stack.borrow().last() {
                Some((id, metadata)) => Current::new(Id::from_u64(*id), metadata),
                None => Current::none(),
            })
        }

        fn clone_span(&self, id: &Id) -> Id {
            id.clone()
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn it_records_spans_and_their_late_fields_but_never_the_message() {
            let recorder = Recorder::new();
            recorder.record(|| {
                let span = crate::span!("work", thing = 1);
                let _entered = span.enter();
                crate::telemetry::record_error(&span, &crate::Error::internal("secret"));
                tracing::Span::current().record("thing", 2);
            });

            let span = recorder.span("work");
            assert_eq!(span.field("thing"), Some("2"), "recorded through Span::current()");
            assert_eq!(span.field("otel.status_code"), Some("ERROR"));
            assert_eq!(span.field("error.type"), Some("internal"));
            assert!(!recorder.everything().contains("secret"));
        }

        #[test]
        fn a_span_opened_inside_another_is_its_child() {
            let recorder = Recorder::new();
            recorder.record(|| {
                let outer = crate::span!("outer");
                let _entered = outer.enter();
                let _inner = crate::span!("inner");
            });

            assert_eq!(recorder.span("inner").parent, Some(recorder.span("outer").id));
        }
    }
}
