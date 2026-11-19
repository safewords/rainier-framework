//! Test-only activities: what the framework's components publish, by the
//! same kinds and field names, for driving Lookout through a real bus.

use std::backtrace::Backtrace;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rainier_events::{Activity, Fields, Operation, Value};

/// A trace id the request helpers put their units in.
pub(crate) const TRACE: &str = "4bf92f3577b34da6a3ce929d0e0e4736";

/// Some work the framework did: a kind, its span's fields, how long it took.
#[derive(Debug, Clone)]
pub(crate) struct Did {
    pub(crate) kind: &'static str,
    pub(crate) fields: Vec<(&'static str, Value)>,
    pub(crate) duration: Option<Duration>,
    pub(crate) failed: bool,
    pub(crate) operation: Option<u64>,
    pub(crate) code_trace: Option<Arc<Backtrace>>,
}

impl Did {
    pub(crate) fn new(kind: &'static str) -> Self {
        Self {
            kind,
            fields: Vec::new(),
            // Shorter than anything around it really takes, so the entry
            // starts inside whatever it ran in.
            duration: Some(Duration::from_micros(1)),
            failed: false,
            operation: None,
            code_trace: None,
        }
    }

    pub(crate) fn with(mut self, name: &'static str, value: impl Into<Value>) -> Self {
        self.fields.push((name, value.into()));
        self
    }

    pub(crate) fn took(mut self, duration: Duration) -> Self {
        self.duration = Some(duration);
        self
    }

    pub(crate) fn for_operation(mut self, op: &Operation) -> Self {
        self.operation = Some(op.id());
        self
    }

    /// Publish on the bus of the unit the calling code runs in.
    pub(crate) fn publish(self) {
        rainier_events::activity::publish_activity(self);
    }
}

impl Activity for Did {
    fn kind(&self) -> &'static str {
        self.kind
    }

    fn fields(&self, fields: &mut Fields) {
        for (name, value) in &self.fields {
            fields.set(name, value.clone());
        }
    }

    fn duration(&self) -> Option<Duration> {
        self.duration
    }

    fn failed(&self) -> bool {
        self.failed
    }

    fn operation(&self) -> Option<u64> {
        self.operation
    }

    fn code_trace(&self) -> Option<&Arc<Backtrace>> {
        self.code_trace.as_ref()
    }
}

/// Run `work` synchronously inside `op`: what it publishes names `op` as its
/// parent. (An [`Operation`] has no guard to enter; its scope is a future.)
pub(crate) fn inside<T>(op: &Operation, work: impl FnOnce() -> T) -> T {
    use futures_util::FutureExt;
    op.scope(async { work() }).now_or_never().expect("a ready future")
}

/// Run `work`, timing it, a little after whatever ran before — so that
/// entries' start times order the way the work did.
pub(crate) fn timed<T>(work: impl FnOnce() -> T) -> (T, Duration) {
    std::thread::sleep(Duration::from_micros(150));
    let started = Instant::now();
    let out = work();
    (out, started.elapsed())
}

/// A `db.query`, as the database publishes it.
pub(crate) fn query(sql: &'static str) {
    let ((), took) = timed(|| ());
    Did::new("db.query")
        .with("db.system.name", "sqlite")
        .with("db.query.text", sql)
        .took(took)
        .publish();
}

/// The root `http.server.request` activity, as the server publishes it.
pub(crate) fn served(
    path: &str,
    status: Option<u16>,
    route: Option<(&'static str, &'static str)>,
    took: Duration,
) -> Did {
    let mut did = Did::new("http.server.request")
        .with("otel.name", "GET")
        .with("http.request.method", "GET")
        .with("url.path", path.to_string())
        .took(took);
    if let Some((pattern, name)) = route {
        did = did.with("http.route", pattern).with("rainier.route.name", name);
    }
    if let Some(status) = status {
        did = did.with("http.response.status_code", status);
    }
    did
}
