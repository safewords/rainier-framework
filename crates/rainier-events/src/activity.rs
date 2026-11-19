//! [`Activity`]: what every event the framework publishes about its own work
//! has in common.
//!
//! Each component publishes its own typed event — `QueryExecuted`,
//! `CacheOperation`, `RequestHandled`, … — and an application subscribes to
//! the ones it cares about by type. Some subscribers want **all** of them
//! without knowing every type: Lookout, a metrics exporter, a log shipper.
//! For those, every framework event also describes itself as an activity: a
//! [`kind`](Activity::kind) (`db.query`, `cache.get`, `http.server.request` —
//! the same names as the framework's `tracing` spans), its
//! [`fields`](Activity::fields) under the same OpenTelemetry names the span
//! carries, how long it took and whether it failed.
//!
//! Publish one with [`Dispatcher::publish_activity`]; hear every one with
//! [`Subscriber::on_activity`](crate::bus::Subscriber::on_activity) or
//! [`Dispatcher::on_activity`].

use std::backtrace::Backtrace;
use std::borrow::Cow;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::context::Context;
use crate::event::Event;

/// One field of an [`Activity`].
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// Text.
    Str(Cow<'static, str>),
    /// A signed number.
    I64(i64),
    /// An unsigned number.
    U64(u64),
    /// A float.
    F64(f64),
    /// A flag.
    Bool(bool),
}

impl Value {
    /// The text, if this is text.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    /// The value as a string, whatever it is.
    pub fn render(&self) -> String {
        match self {
            Value::Str(s) => s.to_string(),
            Value::I64(n) => n.to_string(),
            Value::U64(n) => n.to_string(),
            Value::F64(n) => n.to_string(),
            Value::Bool(b) => b.to_string(),
        }
    }
}

impl From<&'static str> for Value {
    fn from(s: &'static str) -> Self {
        Value::Str(Cow::Borrowed(s))
    }
}
impl From<String> for Value {
    fn from(s: String) -> Self {
        Value::Str(Cow::Owned(s))
    }
}
impl From<Cow<'static, str>> for Value {
    fn from(s: Cow<'static, str>) -> Self {
        Value::Str(s)
    }
}
impl From<i64> for Value {
    fn from(n: i64) -> Self {
        Value::I64(n)
    }
}
impl From<u64> for Value {
    fn from(n: u64) -> Self {
        Value::U64(n)
    }
}
impl From<u32> for Value {
    fn from(n: u32) -> Self {
        Value::U64(u64::from(n))
    }
}
impl From<u16> for Value {
    fn from(n: u16) -> Self {
        Value::U64(u64::from(n))
    }
}
impl From<usize> for Value {
    fn from(n: usize) -> Self {
        Value::U64(n as u64)
    }
}
impl From<i32> for Value {
    fn from(n: i32) -> Self {
        Value::I64(i64::from(n))
    }
}
impl From<f64> for Value {
    fn from(n: f64) -> Self {
        Value::F64(n)
    }
}
impl From<bool> for Value {
    fn from(b: bool) -> Self {
        Value::Bool(b)
    }
}

/// The fields an [`Activity`] fills in: `(name, value)` pairs, names being
/// the OpenTelemetry attribute names its span uses.
#[derive(Debug, Default, Clone)]
pub struct Fields(Vec<(&'static str, Value)>);

impl Fields {
    /// No fields.
    pub fn new() -> Self {
        Self(Vec::new())
    }

    /// Set `name` to `value`, replacing an earlier value.
    pub fn set(&mut self, name: &'static str, value: impl Into<Value>) -> &mut Self {
        let value = value.into();
        match self.0.iter_mut().find(|(k, _)| *k == name) {
            Some(slot) => slot.1 = value,
            None => self.0.push((name, value)),
        }
        self
    }

    /// Set `name` when there is a value; leave it out otherwise.
    pub fn maybe(&mut self, name: &'static str, value: Option<impl Into<Value>>) -> &mut Self {
        if let Some(value) = value {
            self.set(name, value);
        }
        self
    }

    /// The value of `name`.
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.0.iter().find(|(k, _)| *k == name).map(|(_, v)| v)
    }

    /// The text of `name`, when it is non-empty text.
    pub fn text(&self, name: &str) -> Option<&str> {
        self.get(name).and_then(Value::as_str).filter(|s| !s.is_empty())
    }

    /// Every field, in the order set.
    pub fn iter(&self) -> impl Iterator<Item = &(&'static str, Value)> {
        self.0.iter()
    }

    /// Every field, owned.
    pub fn into_vec(self) -> Vec<(&'static str, Value)> {
        self.0
    }
}

/// An event that describes a piece of the framework's own work. See the
/// [module docs](self).
pub trait Activity: Event {
    /// What kind of work: the framework's span name for it — `db.query`,
    /// `cache.get`, `http.server.request`, `queue.process`, …
    fn kind(&self) -> &'static str;

    /// Its fields, under the attribute names its span uses.
    fn fields(&self, fields: &mut Fields);

    /// How long it took, if it took time.
    fn duration(&self) -> Option<Duration> {
        None
    }

    /// Whether it failed.
    fn failed(&self) -> bool {
        false
    }

    /// The [`Operation`](crate::Operation) id it ran as, when other
    /// activities ran inside it and name it as their
    /// [`parent`](Context::parent).
    fn operation(&self) -> Option<u64> {
        None
    }

    /// Where in the code it was called from, when it was slow enough to be
    /// worth asking ([`code_trace`]).
    fn code_trace(&self) -> Option<&Arc<Backtrace>> {
        None
    }

    /// The activity's fields, collected.
    fn collect_fields(&self) -> Fields {
        let mut fields = Fields::new();
        self.fields(&mut fields);
        fields
    }
}

/// Recovers an `&dyn Activity` from a type-erased payload. Monomorphised per
/// event type at publish, carried with the event.
pub(crate) type AsActivity =
    for<'a> fn(&'a (dyn std::any::Any + Send + Sync)) -> Option<&'a dyn Activity>;

pub(crate) fn as_activity<E: Activity>(
    payload: &(dyn std::any::Any + Send + Sync),
) -> Option<&dyn Activity> {
    payload.downcast_ref::<E>().map(|e| e as &dyn Activity)
}

impl crate::Dispatcher {
    /// [`publish`](Self::publish) an [`Activity`]: heard by its own type's
    /// subscribers, by wildcards, and by every
    /// [`on_activity`](crate::bus::Subscriber::on_activity) handler.
    #[inline]
    pub fn publish_activity<E: Activity>(&self, event: E) {
        self.publish_activity_with(move || event);
    }

    /// [`publish_activity`](Self::publish_activity), building the event only
    /// if somebody will hear it.
    #[inline]
    pub fn publish_activity_with<E: Activity>(&self, build: impl FnOnce() -> E) {
        if !self.has_subscribers::<E>() && !self.hears_activities() {
            return;
        }
        self.publish_activity_built(Context::current(), Arc::new(build()));
    }

    /// Whether anything would hear an [`Activity`] of type `E`.
    #[inline]
    pub fn has_activity_subscribers<E: Activity>(&self) -> bool {
        self.has_subscribers::<E>() || self.hears_activities()
    }
}

/// [`Dispatcher::publish_activity_with`] on the current [`bus`](crate::bus).
#[inline]
pub fn publish_activity_with<E: Activity>(build: impl FnOnce() -> E) {
    if let Some(events) = crate::bus::bus() {
        events.publish_activity_with(build);
    }
}

/// [`publish_activity_with`] for an event already built.
#[inline]
pub fn publish_activity<E: Activity>(event: E) {
    publish_activity_with(move || event);
}

/// Whether the current [`bus`](crate::bus) has a subscriber for activity
/// `E` — what a publisher that must gather data first asks.
#[inline]
pub fn has_activity_subscribers<E: Activity>() -> bool {
    crate::bus::bus().is_some_and(|events| events.has_activity_subscribers::<E>())
}

// --- code traces ----------------------------------------------------------------

/// Code traces: a backtrace taken on the publisher's thread for an activity
/// slower than a threshold a subscriber asked for.
///
/// Only the thread that ran a query can say where in the application it was
/// called from, so the publisher captures it — but only for an operation
/// already slower than the threshold (a slow query already cost far more than
/// the capture), only when a subscriber asked ([`request_code_traces`]), and
/// at most once per kind of statement per unit of work
/// ([`Unit::claim_trace`](crate::Unit::claim_trace)).
pub mod code_trace {
    use super::*;

    /// Microseconds; `u64::MAX` means nobody asked.
    static THRESHOLD_US: AtomicU64 = AtomicU64::new(u64::MAX);

    /// Ask publishers to capture a code trace for activities slower than
    /// `threshold`. The lowest threshold asked for wins.
    pub fn request_code_traces(threshold: Duration) {
        let us = u64::try_from(threshold.as_micros()).unwrap_or(u64::MAX);
        THRESHOLD_US.fetch_min(us, Ordering::Relaxed);
    }

    /// Withdraw every request (tests).
    pub fn reset() {
        THRESHOLD_US.store(u64::MAX, Ordering::Relaxed);
    }

    /// A backtrace of the calling code, when `elapsed` is over the requested
    /// threshold and this unit has not already had one for `family`.
    #[inline]
    pub fn capture(elapsed: Duration, family: &str) -> Option<Arc<Backtrace>> {
        let threshold = THRESHOLD_US.load(Ordering::Relaxed);
        if threshold == u64::MAX || (elapsed.as_micros() as u64) < threshold {
            return None;
        }
        capture_slow(family)
    }

    #[cold]
    fn capture_slow(family: &str) -> Option<Arc<Backtrace>> {
        if let Some(unit) = Context::current_unit() {
            if !unit.claim_trace(family) {
                return None;
            }
        }
        Some(Arc::new(Backtrace::force_capture()))
    }
}
