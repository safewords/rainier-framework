//! Log lines on the bus: a [`tracing_subscriber::Layer`] that publishes a
//! [`LogWritten`] activity for every log event at or above the level a
//! subscriber asked for.
//!
//! A subscriber that wants the application's logs alongside its activities —
//! Lookout's log entries, an error tracker — asks with [`request_level`], and
//! the application adds [`LogLayer`] to its `tracing` subscriber (the
//! framework's boot does). Each log event is then published on the current
//! [`bus`](crate::bus) in the current [`Context`](crate::Context), so it
//! lands in the unit of work that wrote it.
//!
//! ```
//! use tracing_subscriber::layer::SubscriberExt;
//!
//! rainier_events::log_layer::request_level(tracing::Level::WARN);
//! let subscriber = tracing_subscriber::registry().with(rainier_events::log_layer::LogLayer::new());
//! # drop(subscriber);
//! ```
//!
//! **Nothing asked, nothing done.** Until a subscriber calls
//! [`request_level`], the layer returns from every event after one atomic
//! load. Above the level asked for, it returns after two.
//!
//! **It never switches a log off.** The layer does not implement `enabled`
//! or `register_callsite`, so it never answers `false` or
//! `Interest::never()`: in a layered subscriber, one layer's "never"
//! disables the callsite for *every* layer, and this one must not silence
//! the application's own log output or its exporter.
//!
//! **Lookout's own work is skipped**: an event inside a span that declares
//! the field [`INTERNAL_FIELD`] — or inside anything under one — is not
//! published, so Lookout recording a log line cannot log its way into a loop.

use std::cell::Cell;
use std::sync::atomic::{AtomicU8, Ordering};

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context as LayerContext, Layer};
use tracing_subscriber::registry::LookupSpan;

use crate::activity::{Activity, Fields, Value};

/// The span field that marks Lookout's own work: log events inside a span
/// that declares it are not published.
pub const INTERNAL_FIELD: &str = "rainier.lookout.internal";

/// The most verbose level asked for, as [`rank`]; 0 means nobody asked.
static LEVEL: AtomicU8 = AtomicU8::new(0);

/// `ERROR` is 1, `TRACE` is 5: higher is more verbose.
fn rank(level: Level) -> u8 {
    match level {
        Level::ERROR => 1,
        Level::WARN => 2,
        Level::INFO => 3,
        Level::DEBUG => 4,
        Level::TRACE => 5,
    }
}

/// Ask for log events at `level` and above (more severe) to be published as
/// [`LogWritten`].
///
/// Several subscribers may ask; the most verbose level asked for wins, so
/// each gets at least what it asked for — and filters out anything more
/// verbose on its own side, by [`LogWritten::level`].
pub fn request_level(level: Level) {
    LEVEL.fetch_max(rank(level), Ordering::Relaxed);
}

/// The level log events are published at, if anybody asked.
pub fn requested_level() -> Option<Level> {
    Some(match LEVEL.load(Ordering::Relaxed) {
        1 => Level::ERROR,
        2 => Level::WARN,
        3 => Level::INFO,
        4 => Level::DEBUG,
        5 => Level::TRACE,
        _ => return None,
    })
}

/// Withdraw every request (tests).
pub fn reset() {
    LEVEL.store(0, Ordering::Relaxed);
}

/// A log event, as the bus carries it.
///
/// Its [`fields`](Activity::fields) are `level` (lowercase), `target`, and
/// the event's own fields under their own names — `message` among them.
#[derive(Debug, Clone)]
pub struct LogWritten {
    /// The event's level.
    pub level: Level,
    /// The event's target: the module path, unless it set one.
    pub target: &'static str,
    /// The event's own fields, `message` included.
    pub fields: Fields,
}

impl LogWritten {
    /// The `message` field, if the event had one.
    pub fn message(&self) -> Option<&str> {
        self.fields.text("message")
    }
}

impl Activity for LogWritten {
    fn kind(&self) -> &'static str {
        "log"
    }

    fn fields(&self, fields: &mut Fields) {
        fields.set("level", level_name(self.level)).set("target", self.target);
        for (name, value) in self.fields.iter() {
            fields.set(name, value.clone());
        }
    }

    fn failed(&self) -> bool {
        self.level == Level::ERROR
    }
}

fn level_name(level: Level) -> &'static str {
    match level {
        Level::ERROR => "error",
        Level::WARN => "warn",
        Level::INFO => "info",
        Level::DEBUG => "debug",
        Level::TRACE => "trace",
    }
}

/// The layer. Add it to the application's `tracing` subscriber; see the
/// [module docs](self).
#[derive(Debug, Default, Clone, Copy)]
pub struct LogLayer {
    _private: (),
}

impl LogLayer {
    /// The layer.
    pub fn new() -> Self {
        Self::default()
    }
}

thread_local! {
    /// Set while this thread is publishing a log event, so a log line written
    /// on the way (the bus starting a subscriber thread, say) is not
    /// published in turn.
    static PUBLISHING: Cell<bool> = const { Cell::new(false) };
}

impl<S> Layer<S> for LogLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, ctx: LayerContext<'_, S>) {
        let wanted = LEVEL.load(Ordering::Relaxed);
        if wanted == 0 {
            return;
        }
        let meta = event.metadata();
        if rank(*meta.level()) > wanted {
            return;
        }
        if let Some(scope) = ctx.event_scope(event) {
            for span in scope {
                if span.metadata().fields().field(INTERNAL_FIELD).is_some() {
                    return;
                }
            }
        }
        if PUBLISHING.with(Cell::get) {
            return;
        }
        let Some(events) = crate::bus::bus() else { return };
        PUBLISHING.with(|p| p.set(true));
        let _publishing = Publishing;
        events.publish_activity_with(|| {
            let mut fields = Fields::new();
            event.record(&mut Visitor(&mut fields));
            LogWritten { level: *meta.level(), target: meta.target(), fields }
        });
    }
}

/// Clears [`PUBLISHING`] when dropped, panic or not.
struct Publishing;

impl Drop for Publishing {
    fn drop(&mut self) {
        PUBLISHING.with(|p| p.set(false));
    }
}

/// Copies an event's fields into [`Fields`].
struct Visitor<'a>(&'a mut Fields);

impl Visit for Visitor<'_> {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.set(field.name(), Value::from(value.to_string()));
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.0.set(field.name(), value);
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.0.set(field.name(), value);
    }
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.0.set(field.name(), value);
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.0.set(field.name(), value);
    }
    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.0.set(field.name(), Value::from(value.to_string()));
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.set(field.name(), Value::from(format!("{value:?}")));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Dispatcher;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::layer::SubscriberExt;

    // One test: the requested level is process-wide, so tests that set it
    // differently would race each other.
    #[tokio::test]
    async fn log_events_at_the_requested_level_are_published_in_context() {
        let events = Arc::new(Dispatcher::new());
        let _bus = crate::bus::set_default(Arc::clone(&events));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s = Arc::clone(&seen);
        events.on::<LogWritten>().run_sync(move |log, cx| {
            s.lock().unwrap().push(((*log).clone(), cx.unit().map(|u| u.id())));
            Ok(())
        });
        let subscriber = tracing_subscriber::registry().with(LogLayer::new());
        let _tracing = tracing::subscriber::set_default(subscriber);

        // Nobody asked: nothing published.
        reset();
        tracing::error!("unheard");

        request_level(Level::INFO);
        request_level(Level::WARN); // a less verbose ask does not narrow it
        assert_eq!(requested_level(), Some(Level::INFO));

        let unit = crate::Unit::new(crate::UnitKind::Job, None, None);
        unit.scope(async {
            tracing::info!(order = 7_u64, "shipped order {}", 7);
            tracing::debug!("too verbose");
            let internal = tracing::info_span!("lookout.flush", rainier.lookout.internal = true);
            let _entered = internal.enter();
            tracing::warn!("lookout's own");
            let nested = tracing::info_span!("inner");
            let _nested = nested.enter();
            tracing::error!("still lookout's own");
        })
        .await;
        events.settled().await;
        reset();

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "{seen:?}");
        let (log, unit_id) = &seen[0];
        assert_eq!(*unit_id, Some(unit.id()));
        assert_eq!(log.kind(), "log");
        assert_eq!(log.message(), Some("shipped order 7"));
        let fields = log.collect_fields();
        assert_eq!(fields.text("level"), Some("info"));
        assert_eq!(fields.text("target"), Some(module_path!()));
        assert_eq!(fields.get("order"), Some(&Value::U64(7)));
        assert!(!log.failed());
    }
}
