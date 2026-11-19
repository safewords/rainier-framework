//! What a published event carries besides itself: the [`Context`] it was
//! raised in.
//!
//! A subscriber runs later, on another task and usually another thread, so it
//! cannot ask "which request am I part of?" of its surroundings. The publisher
//! answers it at publish time, and the answer travels with the event.
//!
//! The answer is a [`Unit`] of work — an HTTP request, a queued job's run, a
//! console command, a scheduled task — plus the innermost [`Operation`] inside
//! it (a mail being sent, which renders a view). The framework opens both;
//! application code rarely does.
//!
//! ```
//! use rainier_events::{Context, Unit, UnitKind};
//!
//! # #[tokio::main] async fn main() {
//! let unit = Unit::new(UnitKind::Request, Some("GET /posts".into()), None);
//! let seen = unit.scope(async { Context::current() }).await;
//! assert_eq!(seen.unit().map(|u| u.id()), Some(unit.id()));
//! # }
//! ```
//!
//! **The current unit is a thread-local stack** that [`Unit::scope`] and
//! [`Operation::scope`] push around every poll of the future they wrap — so it
//! follows an `.await` from one worker thread to another, and does not follow
//! a `tokio::spawn` unless the spawned future is wrapped too
//! ([`Context::current`] + [`Context::scope`]).
//!
//! **A unit is complete when it and every event raised in it are gone.**
//! Events hold their unit; when the last of them has been delivered to the
//! last subscriber and the unit's own scope has ended, [`UnitCompleted`] is
//! published. A subscriber that gathers a unit's events — Lookout's batches —
//! closes on it, and never sees a straggler afterwards.

use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::task::{Context as TaskContext, Poll};
use std::time::{Instant, SystemTime};

use crate::Dispatcher;

/// What kind of work a [`Unit`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum UnitKind {
    /// An HTTP request.
    Request,
    /// One run (attempt) of a queued job.
    Job,
    /// A console command.
    Command,
    /// A scheduled task.
    ScheduledTask,
}

impl UnitKind {
    /// The kind's lowercase name: `request`, `job`, `command`, `scheduled_task`.
    pub fn as_str(self) -> &'static str {
        match self {
            UnitKind::Request => "request",
            UnitKind::Job => "job",
            UnitKind::Command => "command",
            UnitKind::ScheduledTask => "scheduled_task",
        }
    }
}

/// A unit of work: what a subscriber groups events by.
///
/// Cheap to share — events hold it by `Arc`. Its trace ids can be filled in
/// after it opened ([`set_trace`](Self::set_trace)), because an HTTP request
/// learns its trace from headers its span reads.
pub struct Unit {
    id: u64,
    kind: UnitKind,
    name: Option<String>,
    path: Option<String>,
    trace_id: OnceLock<String>,
    traceparent: OnceLock<String>,
    started: Instant,
    started_at: SystemTime,
    /// The bus to publish [`UnitCompleted`] on, and to resolve
    /// [`bus`](crate::bus) to while this unit is current.
    events: OnceLock<Weak<Dispatcher>>,
    /// Families a code trace has been taken for in this unit.
    traced: std::sync::Mutex<Vec<u64>>,
}

impl Unit {
    /// A new unit, not yet current anywhere: run work inside it with
    /// [`scope`](Self::scope).
    pub fn new(kind: UnitKind, name: Option<String>, path: Option<String>) -> Arc<Self> {
        Arc::new(Self {
            id: next_id(),
            kind,
            name,
            path,
            trace_id: OnceLock::new(),
            traceparent: OnceLock::new(),
            started: Instant::now(),
            started_at: SystemTime::now(),
            events: OnceLock::new(),
            traced: std::sync::Mutex::new(Vec::new()),
        })
    }

    /// A new unit that publishes on `events`: events raised inside it go to
    /// `events` (see [`bus`](crate::bus)), and [`UnitCompleted`] is published
    /// there when it is done.
    pub fn on(
        events: &Arc<Dispatcher>,
        kind: UnitKind,
        name: Option<String>,
        path: Option<String>,
    ) -> Arc<Self> {
        let unit = Self::new(kind, name, path);
        let _ = unit.events.set(Arc::downgrade(events));
        unit
    }

    /// Open the unit a piece of `kind` work runs in, or join the current one.
    ///
    /// A job or scheduled task run **inside a request** (a synchronous queue
    /// driver, a test) is part of that request; inside anything else —
    /// `queue:work` and `schedule:work` are commands that run forever — it is
    /// a unit of its own. A request or command is always its own unit unless
    /// one is already current.
    pub fn open_or_join(
        kind: UnitKind,
        name: Option<String>,
        path: Option<String>,
    ) -> Option<Arc<Self>> {
        let current = Context::current_unit();
        let opens = match (&current, kind) {
            (None, _) => true,
            (Some(cur), UnitKind::Job | UnitKind::ScheduledTask) => cur.kind != UnitKind::Request,
            (Some(_), _) => false,
        };
        if !opens {
            return None;
        }
        let events = current
            .as_ref()
            .and_then(|u| u.events.get().and_then(Weak::upgrade))
            .or_else(crate::bus::installed);
        Some(match events {
            Some(events) => Self::on(&events, kind, name, path),
            None => Self::new(kind, name, path),
        })
    }

    /// This unit's id: unique within the process, and what Lookout calls the
    /// batch id.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// [`id`](Self::id) as the sixteen hex digits Lookout shows.
    pub fn id_string(&self) -> String {
        format!("{:016x}", self.id)
    }

    /// What kind of work this is.
    pub fn kind(&self) -> UnitKind {
        self.kind
    }

    /// The job, command or task name; the route for a request, once known.
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// The request path, for a request.
    pub fn path(&self) -> Option<&str> {
        self.path.as_deref()
    }

    /// The OpenTelemetry trace id (32 hex digits), when known.
    pub fn trace_id(&self) -> Option<&str> {
        self.trace_id.get().map(String::as_str)
    }

    /// The W3C `traceparent` this unit continues, when known.
    pub fn traceparent(&self) -> Option<&str> {
        self.traceparent.get().map(String::as_str)
    }

    /// Record the trace this unit belongs to. The first call wins: a unit is
    /// in one trace.
    pub fn set_trace(&self, trace_id: Option<&str>, traceparent: Option<&str>) {
        if let Some(id) = trace_id.filter(|s| !s.is_empty()) {
            let _ = self.trace_id.set(id.to_string());
        }
        if let Some(parent) = traceparent.filter(|s| !s.is_empty()) {
            let _ = self.traceparent.set(parent.to_string());
            if self.trace_id.get().is_none() {
                if let Some(id) = parent.split('-').nth(1).filter(|s| s.len() == 32) {
                    let _ = self.trace_id.set(id.to_string());
                }
            }
        }
    }

    /// When the unit opened.
    pub fn started_at(&self) -> SystemTime {
        self.started_at
    }

    /// How long since the unit opened.
    pub fn elapsed(&self) -> std::time::Duration {
        self.started.elapsed()
    }

    /// Claim the one code trace this unit takes for `family` (a statement, a
    /// URL): `true` the first time, `false` after.
    pub fn claim_trace(&self, family: &str) -> bool {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        family.hash(&mut hasher);
        let key = hasher.finish();
        let Ok(mut traced) = self.traced.lock() else { return false };
        if traced.contains(&key) {
            return false;
        }
        traced.push(key);
        true
    }

    /// The bus this unit publishes on, if it was opened with one.
    pub fn events(&self) -> Option<Arc<Dispatcher>> {
        self.events.get().and_then(Weak::upgrade)
    }

    /// Run `future` with this unit current.
    pub fn scope<F: Future>(self: &Arc<Self>, future: F) -> Scoped<F> {
        Scoped {
            frame: Frame { unit: Some(Arc::clone(self)), op: None },
            inherit: false,
            inner: future,
        }
    }

    /// Make this unit current until the guard drops — for synchronous code,
    /// such as a console command's body that is not a future.
    ///
    /// The guard must drop on the thread that made it, before anything else
    /// it made current is dropped.
    pub fn enter(self: &Arc<Self>) -> Entered {
        push(Frame { unit: Some(Arc::clone(self)), op: None });
        Entered { _not_send: std::marker::PhantomData }
    }
}

impl Drop for Unit {
    fn drop(&mut self) {
        let Some(events) = self.events.get().and_then(Weak::upgrade) else { return };
        events.publish_in(
            Context::none(),
            UnitCompleted {
                id: self.id,
                kind: self.kind,
                name: self.name.take(),
                path: self.path.take(),
                trace_id: self.trace_id.take(),
                started_at: self.started_at,
                duration: self.started.elapsed(),
            },
        );
    }
}

impl std::fmt::Debug for Unit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Unit")
            .field("id", &self.id_string())
            .field("kind", &self.kind)
            .field("name", &self.name)
            .field("trace_id", &self.trace_id.get())
            .finish()
    }
}

/// Published once a [`Unit`] is over **and** every event raised in it has
/// been delivered to every subscriber. Carries no [`Unit`] — it is gone — so
/// its [`Context`] is empty; it names the unit by [`id`](Self::id).
#[derive(Debug, Clone)]
pub struct UnitCompleted {
    /// The [`Unit::id`] that completed.
    pub id: u64,
    /// What kind of work it was.
    pub kind: UnitKind,
    /// Its name, if it had one.
    pub name: Option<String>,
    /// Its request path, for a request.
    pub path: Option<String>,
    /// Its trace id, if one was recorded.
    pub trace_id: Option<String>,
    /// When it opened.
    pub started_at: SystemTime,
    /// From opening until the last of its events was delivered.
    pub duration: std::time::Duration,
}

/// An operation inside a unit that other operations run inside: sending a
/// notification (which sends a mail, which renders a view). Events raised
/// inside it name it as their [`parent`](Context::parent), which is how a
/// subscriber nests them.
#[derive(Debug, Clone)]
pub struct Operation {
    id: u64,
}

impl Operation {
    /// A new operation id, not yet current.
    pub fn new() -> Self {
        Self { id: next_id() }
    }

    /// The id to put on the operation's own event, so the events under it can
    /// be matched to it.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Run `future` with this operation as the parent of what it raises,
    /// inside whatever unit is current now.
    pub fn scope<F: Future>(&self, future: F) -> Scoped<F> {
        Scoped { frame: Frame { unit: None, op: Some(self.id) }, inherit: true, inner: future }
    }
}

impl Default for Operation {
    fn default() -> Self {
        Self::new()
    }
}

/// The context an event was published in: its unit, its parent operation,
/// and when.
#[derive(Clone)]
pub struct Context {
    unit: Option<Arc<Unit>>,
    parent: Option<u64>,
    at: SystemTime,
}

impl Context {
    /// The context of the calling code, right now.
    ///
    /// One thread-local read and an `Arc` clone; publishers call it only once
    /// they know somebody listens.
    pub fn current() -> Self {
        let (unit, parent) = STACK.with(|stack| match stack.borrow().last() {
            Some(frame) => (frame.unit.clone(), frame.op),
            None => (None, None),
        });
        Self { unit, parent, at: SystemTime::now() }
    }

    /// No unit, no parent: an event raised outside every unit of work.
    pub fn none() -> Self {
        Self { unit: None, parent: None, at: SystemTime::now() }
    }

    /// The innermost unit the calling code runs in.
    pub fn current_unit() -> Option<Arc<Unit>> {
        STACK.with(|stack| stack.borrow().last().and_then(|f| f.unit.clone()))
    }

    /// The unit the event was raised in, if any.
    pub fn unit(&self) -> Option<&Arc<Unit>> {
        self.unit.as_ref()
    }

    /// The innermost [`Operation`] the event was raised inside, if any.
    pub fn parent(&self) -> Option<u64> {
        self.parent
    }

    /// When the event was published.
    pub fn at(&self) -> SystemTime {
        self.at
    }

    /// The trace id of the event's unit, if known.
    pub fn trace_id(&self) -> Option<&str> {
        self.unit.as_ref().and_then(|u| u.trace_id())
    }

    /// Run `future` in this context — what carries a unit across a
    /// `tokio::spawn`: `tokio::spawn(Context::current().scope(work))`.
    pub fn scope<F: Future>(&self, future: F) -> Scoped<F> {
        Scoped {
            frame: Frame { unit: self.unit.clone(), op: self.parent },
            inherit: false,
            inner: future,
        }
    }
}

impl std::fmt::Debug for Context {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Context").field("unit", &self.unit).field("parent", &self.parent).finish()
    }
}

/// One entry of the thread's context stack.
#[derive(Clone)]
struct Frame {
    unit: Option<Arc<Unit>>,
    op: Option<u64>,
}

thread_local! {
    static STACK: RefCell<Vec<Frame>> = const { RefCell::new(Vec::new()) };
}

fn push(frame: Frame) {
    STACK.with(|stack| stack.borrow_mut().push(frame));
}

fn pop() {
    STACK.with(|stack| {
        stack.borrow_mut().pop();
    });
}

/// The guard [`Unit::enter`] returns.
pub struct Entered {
    _not_send: std::marker::PhantomData<*const ()>,
}

impl Drop for Entered {
    fn drop(&mut self) {
        pop();
    }
}

impl std::fmt::Debug for Entered {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Entered")
    }
}

/// A future run with a [`Unit`] or [`Operation`] current. See
/// [`Unit::scope`].
pub struct Scoped<F> {
    frame: Frame,
    /// An operation's scope: the unit is whatever is current when it runs.
    inherit: bool,
    inner: F,
}

impl<F: Future> Future for Scoped<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<F::Output> {
        // SAFETY: `inner` is structurally pinned — never moved out of `self`,
        // and `Scoped` has no `Drop` that would move it; `frame` is not pinned.
        let this = unsafe { self.get_unchecked_mut() };
        let mut frame = this.frame.clone();
        if this.inherit {
            frame.unit = Context::current_unit();
        }
        push(frame);
        struct PopOnDrop;
        impl Drop for PopOnDrop {
            fn drop(&mut self) {
                pop();
            }
        }
        let _pop = PopOnDrop;
        // SAFETY: see above.
        unsafe { Pin::new_unchecked(&mut this.inner) }.poll(cx)
    }
}

impl<F> std::fmt::Debug for Scoped<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scoped").field("unit", &self.frame.unit).finish_non_exhaustive()
    }
}

fn next_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    static SALT: OnceLock<u64> = OnceLock::new();
    let salt = *SALT.get_or_init(|| {
        // Not cryptographic: ids only need to differ between processes that
        // write to one Lookout store.
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let mixed = (nanos ^ u64::from(std::process::id()).rotate_left(32))
            .wrapping_mul(0x9E37_79B9_7F4A_7C15);
        mixed & 0xffff_ff00_0000_0000
    });
    salt ^ NEXT.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_scope_makes_its_unit_current_only_inside() {
        let unit = Unit::new(UnitKind::Job, Some("Ping".into()), None);
        assert!(Context::current_unit().is_none());
        let inside = unit.scope(async { Context::current_unit().map(|u| u.id()) }).await;
        assert_eq!(inside, Some(unit.id()));
        assert!(Context::current_unit().is_none());
    }

    #[tokio::test]
    async fn an_operation_is_the_parent_of_what_runs_inside_it() {
        let unit = Unit::new(UnitKind::Request, None, None);
        let op = Operation::new();
        let cx = unit.scope(op.scope(async { Context::current() })).await;
        assert_eq!(cx.parent(), Some(op.id()));
        assert_eq!(cx.unit().map(|u| u.id()), Some(unit.id()));
    }

    #[tokio::test]
    async fn a_job_inside_a_request_joins_it_and_inside_a_command_opens_its_own() {
        let request = Unit::new(UnitKind::Request, None, None);
        let joined = request.scope(async { Unit::open_or_join(UnitKind::Job, None, None) }).await;
        assert!(joined.is_none());

        let command = Unit::new(UnitKind::Command, Some("queue:work".into()), None);
        let own = command.scope(async { Unit::open_or_join(UnitKind::Job, None, None) }).await;
        assert!(own.is_some());
    }

    #[test]
    fn the_trace_id_comes_from_a_traceparent_when_not_given() {
        let unit = Unit::new(UnitKind::Job, None, None);
        unit.set_trace(None, Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"));
        assert_eq!(unit.trace_id(), Some("4bf92f3577b34da6a3ce929d0e0e4736"));
    }
}
