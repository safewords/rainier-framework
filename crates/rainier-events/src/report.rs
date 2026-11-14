//! Exception reporting: a typed catalogue of everything that can fail, an
//! event for each, and the [`Reporter`] that puts them on the bus.
//!
//! Rendering and reporting are two different jobs. The HTTP kernel's
//! exception renderer decides what a *client* sees; reporting is how the
//! *application* hears that something failed. The framework does not decide
//! which failures matter — it publishes **every** failure it would otherwise
//! swallow, and the application decides what to subscribe to and what to do.
//!
//! ## The catalogue
//!
//! Each kind of failure has its own event struct with fixed, typed fields —
//! [`HttpRequestFailed`] carries the method, path and route;
//! [`JobAttemptFailed`] the job, queue, attempt and whether it was the last.
//! Every one of them also carries the [`Exception`] (what failed) and
//! [`ReportMeta`] (when, where, and what the application attached), and
//! implements [`FailureEvent`].
//!
//! The same catalogue is the [`Failure`] enum — one variant per struct — and
//! [`FailureKind`], the same list without data. See [`Failure`] for the full
//! list.
//!
//! ## Subscribing
//!
//! Every report is dispatched **twice** on the bus: once as its own typed
//! event, and once wrapped in [`ExceptionReported`]. Listen for the typed
//! event to handle one kind of failure; listen for `ExceptionReported` to see
//! all of them.
//!
//! ```
//! use rainier_events::report::{ExceptionReported, Failure, HttpRequestFailed, JobAttemptFailed, Reporter};
//! use rainier_events::Dispatcher;
//! use rainier_support::Error;
//! use std::sync::Arc;
//!
//! # #[tokio::main] async fn main() {
//! let events = Arc::new(Dispatcher::new());
//!
//! // Only failed requests.
//! events.listen(|failed: Arc<HttpRequestFailed>| async move {
//!     println!("{} {} -> {}", failed.method, failed.path, failed.exception.status);
//!     Ok(())
//! });
//!
//! // Only jobs that have run out of attempts.
//! events.listen(|failed: Arc<JobAttemptFailed>| async move {
//!     if failed.final_attempt {
//!         println!("job {} is dead: {}", failed.job, failed.exception);
//!     }
//!     Ok(())
//! });
//!
//! // Everything, e.g. for an error tracker.
//! events.listen(|reported: Arc<ExceptionReported>| async move {
//!     match &reported.failure {
//!         Failure::HttpRequest(failed) => println!("request: {}", failed.exception),
//!         other => println!("{}: {}", other.kind(), other.exception()),
//!     }
//!     Ok(())
//! });
//!
//! let reporter = Reporter::new(Arc::clone(&events));
//! reporter
//!     .report(HttpRequestFailed::new(Error::internal("disk full"), "GET", "/posts/1", Some("/posts/{post}".into())))
//!     .await;
//! # }
//! ```
//!
//! ## Across processes
//!
//! Every failure struct, [`Failure`] and [`ExceptionReported`] is
//! `Serialize + Deserialize`, so a listener can forward a report to a queue,
//! a broadcast channel or another service as JSON. The original
//! `Arc<Error>` ([`Exception::error`]) stays behind; everything else —
//! kind, status, message, cause chain, backtrace — travels. [`ReportMeta`]
//! gives each report an `id`, a timestamp, the process id and host.
//!
//! ## Where reports come from
//!
//! Components that hold a [`Reporter`] — the HTTP kernel, the queue worker,
//! the console — use it directly. Components deep in the stack that have no
//! handle to the application (a cache, a queue driver, the event dispatcher
//! itself) report through the **process-wide** reporter: [`install`] sets it
//! (the framework does this at boot), and [`report`] / [`report_detached`]
//! use it, doing nothing when none is installed.
//!
//! ## Delivery
//!
//! Reports are dispatched with
//! [`dispatch_quietly`](crate::Dispatcher::dispatch_quietly): a failing
//! listener is logged (and reported as a [`ListenerFailed`], except for the
//! listeners of failure events themselves, which would loop) and the others
//! still run. A failure to report never changes the outcome of what failed.
//! Listeners run inline — a failed request's response waits for them — so a
//! listener that talks to the network should hand the work to the queue.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use rainier_support::{Error, ErrorKind};
use serde::{Deserialize, Serialize};

use crate::Dispatcher;

// --- what failed -------------------------------------------------------------

/// A snapshot of a failure, in the form every listener can use without
/// knowing where it came from.
///
/// `Clone`, unlike [`Error`], because one report is shared by every listener.
/// The original error, when there was one, rides along in
/// [`error`](Exception::error) for a listener that wants to downcast its
/// source; it is not serialised.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Exception {
    /// The error's kind. `None` for a panic, which has no kind.
    pub kind: Option<ErrorKind>,
    /// The HTTP status the failure maps to. `500` for a panic.
    pub status: u16,
    /// The error's own message — never the generic one a client was shown.
    pub message: String,
    /// The structured details payload, if any.
    pub details: Option<serde_json::Value>,
    /// The cause chain, outermost first, not including `message` itself.
    pub sources: Vec<String>,
    /// The stack the error was built on (or the `panic!` site), rendered.
    /// `None` unless `RUST_BACKTRACE` / `RUST_LIB_BACKTRACE` is set.
    pub backtrace: Option<String>,
    /// Whether this was a panic rather than a returned error.
    pub panic: bool,
    /// The original error, when the failure was one. Not serialised.
    #[serde(skip)]
    pub error: Option<Arc<Error>>,
}

impl Exception {
    /// Snapshot a framework error.
    pub fn from_error(error: Error) -> Self {
        Self::from_shared(Arc::new(error))
    }

    /// Snapshot an error that is already shared.
    pub fn from_shared(error: Arc<Error>) -> Self {
        let sources = error
            .source_error()
            .map(|source| source.chain().map(ToString::to_string).collect())
            .unwrap_or_default();
        Self {
            kind: Some(error.kind()),
            status: error.status(),
            message: error.message().to_string(),
            details: error.details().cloned(),
            sources,
            backtrace: error.backtrace().map(ToString::to_string),
            panic: false,
            error: Some(error),
        }
    }

    /// Snapshot an error that somebody else keeps — a receipt, a result being
    /// returned. Everything is copied except the original error itself,
    /// which [`error`](Exception::error) cannot hold without owning it.
    pub fn from_ref(error: &Error) -> Self {
        let sources = error
            .source_error()
            .map(|source| source.chain().map(ToString::to_string).collect())
            .unwrap_or_default();
        Self {
            kind: Some(error.kind()),
            status: error.status(),
            message: error.message().to_string(),
            details: error.details().cloned(),
            sources,
            backtrace: error.backtrace().map(ToString::to_string),
            panic: false,
            error: None,
        }
    }

    /// Snapshot any `std::error::Error` — a driver's own error type, an
    /// `io::Error` — as an internal error, keeping its cause chain.
    pub fn from_std(error: &(dyn std::error::Error + 'static)) -> Self {
        let mut sources = Vec::new();
        let mut next = error.source();
        while let Some(source) = next {
            sources.push(source.to_string());
            next = source.source();
        }
        Self { sources, ..Self::message(ErrorKind::Internal, error.to_string()) }
    }

    /// A failure that has no error value — only a description.
    pub fn message(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind: Some(kind),
            status: kind.status(),
            message: message.into(),
            details: None,
            sources: Vec::new(),
            backtrace: None,
            panic: false,
            error: None,
        }
    }

    /// A panic, from its payload message and (when the panic hook caught
    /// one) its stack.
    pub fn panic(message: impl Into<String>, backtrace: Option<String>) -> Self {
        Self {
            kind: None,
            status: 500,
            message: message.into(),
            details: None,
            sources: Vec::new(),
            backtrace,
            panic: true,
            error: None,
        }
    }

    /// Whether this is a server-side fault (a 5xx or a panic).
    pub fn is_server_error(&self) -> bool {
        self.panic || self.status >= 500
    }

    /// Whether this is a client's mistake (a 4xx).
    pub fn is_client_error(&self) -> bool {
        !self.panic && (400..500).contains(&self.status)
    }

    /// A short label for the kind: `"Internal"`, `"NotFound"`, `"Panic"`, ….
    pub fn kind_label(&self) -> String {
        match self.kind {
            _ if self.panic => "Panic".to_string(),
            Some(ErrorKind::Status(code)) => format!("Status({code})"),
            Some(kind) => format!("{kind:?}"),
            None => "Unknown".to_string(),
        }
    }
}

impl From<Error> for Exception {
    fn from(error: Error) -> Self {
        Self::from_error(error)
    }
}

impl From<Arc<Error>> for Exception {
    fn from(error: Arc<Error>) -> Self {
        Self::from_shared(error)
    }
}

impl fmt::Display for Exception {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.panic {
            write!(f, "panic: ")?;
        }
        write!(f, "{}", self.message)?;
        for source in &self.sources {
            write!(f, ": {source}")?;
        }
        Ok(())
    }
}

// --- when and where ----------------------------------------------------------

/// When and where a failure was reported, and what the application attached.
///
/// Filled in by the [`Reporter`]: leave it [`Default`] when building a
/// failure. Fields a caller sets in `extra` survive; the reporter's context
/// providers fill in around them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReportMeta {
    /// Unique per report, so a listener that forwards reports can
    /// de-duplicate them downstream.
    pub id: String,
    /// When it was reported, in milliseconds since the Unix epoch.
    pub reported_at_ms: u64,
    /// The reporting process's id.
    pub pid: u32,
    /// The reporting host (`HOSTNAME`, which is the pod name on Kubernetes).
    pub host: Option<String>,
    /// Free-form context: a release, a tenant, a user id. Filled by
    /// [`Reporter::with_context`] providers and by whoever built the failure.
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl ReportMeta {
    /// Add one extra field.
    pub fn with(mut self, key: impl Into<String>, value: impl Into<serde_json::Value>) -> Self {
        self.extra.insert(key.into(), value.into());
        self
    }
}

// --- the catalogue -----------------------------------------------------------

/// Implemented by every failure event in the catalogue.
pub trait FailureEvent:
    Clone + fmt::Debug + Serialize + for<'de> Deserialize<'de> + Send + Sync + 'static
{
    /// Which kind of failure this is.
    const KIND: FailureKind;

    /// What failed.
    fn exception(&self) -> &Exception;

    /// When and where it was reported.
    fn meta(&self) -> &ReportMeta;

    /// When and where it was reported, mutably — for the reporter, and for a
    /// caller that wants to attach `extra` fields before reporting.
    fn meta_mut(&mut self) -> &mut ReportMeta;

    /// Wrap it in the catalogue enum.
    fn into_failure(self) -> Failure;

    /// Attach one extra field before reporting.
    fn with_extra(mut self, key: impl Into<String>, value: impl Into<serde_json::Value>) -> Self
    where
        Self: Sized,
    {
        self.meta_mut().extra.insert(key.into(), value.into());
        self
    }
}

/// Declares the catalogue: one struct per failure, the [`Failure`] enum, the
/// [`FailureKind`] enum, and the glue between them. Adding a failure is adding
/// one entry here.
macro_rules! failures {
    ($(
        $(#[$doc:meta])*
        $variant:ident => $name:ident {
            $( $(#[$fdoc:meta])* $field:ident : $ty:ty ),* $(,)?
        }
    )*) => {
        $(
            $(#[$doc])*
            #[derive(Debug, Clone, Serialize, Deserialize)]
            #[non_exhaustive]
            pub struct $name {
                $( $(#[$fdoc])* pub $field: $ty, )*
                /// What failed.
                pub exception: Exception,
                /// When and where it was reported.
                #[serde(default)]
                pub meta: ReportMeta,
            }

            impl $name {
                /// Build the failure; the [`ReportMeta`] is filled in by the
                /// reporter.
                #[allow(clippy::too_many_arguments)]
                pub fn new(exception: impl Into<Exception> $(, $field: impl Into<$ty>)*) -> Self {
                    Self {
                        $( $field: $field.into(), )*
                        exception: exception.into(),
                        meta: ReportMeta::default(),
                    }
                }
            }

            impl FailureEvent for $name {
                const KIND: FailureKind = FailureKind::$variant;
                fn exception(&self) -> &Exception { &self.exception }
                fn meta(&self) -> &ReportMeta { &self.meta }
                fn meta_mut(&mut self) -> &mut ReportMeta { &mut self.meta }
                fn into_failure(self) -> Failure { Failure::$variant(self) }
            }

            impl From<$name> for Failure {
                fn from(failure: $name) -> Self { Failure::$variant(failure) }
            }
        )*

        /// Every kind of failure the framework reports, each with its own
        /// event struct.
        ///
        /// This is the catalogue: match on it in an [`ExceptionReported`]
        /// listener, or listen for one of the structs directly.
        #[derive(Debug, Clone, Serialize, Deserialize)]
        #[serde(tag = "type")]
        #[non_exhaustive]
        pub enum Failure {
            $( $(#[$doc])* $variant($name), )*
        }

        /// The catalogue without its data — for filtering, metrics labels
        /// and [`Reporter::ignore`].
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[non_exhaustive]
        pub enum FailureKind {
            $( $(#[$doc])* $variant, )*
        }

        impl FailureKind {
            /// Every kind, in declaration order.
            pub const ALL: &'static [FailureKind] = &[$( FailureKind::$variant, )*];

            /// The kind's name, as written: `"HttpRequest"`, `"JobAttempt"`, ….
            pub fn as_str(self) -> &'static str {
                match self { $( FailureKind::$variant => stringify!($variant), )* }
            }
        }

        impl Failure {
            /// Which kind of failure this is.
            pub fn kind(&self) -> FailureKind {
                match self { $( Failure::$variant(_) => FailureKind::$variant, )* }
            }

            /// What failed.
            pub fn exception(&self) -> &Exception {
                match self { $( Failure::$variant(f) => &f.exception, )* }
            }

            /// When and where it was reported.
            pub fn meta(&self) -> &ReportMeta {
                match self { $( Failure::$variant(f) => &f.meta, )* }
            }

            fn meta_mut(&mut self) -> &mut ReportMeta {
                match self { $( Failure::$variant(f) => &mut f.meta, )* }
            }

            /// Dispatch the typed event for this failure.
            async fn dispatch_typed(self, events: &Dispatcher) {
                match self { $( Failure::$variant(f) => { events.dispatch_quietly(f).await; } )* }
            }
        }

        /// Whether `type_id` is one of the failure events, or the catch-all —
        /// the events whose failing listeners must not be reported again.
        pub(crate) fn is_failure_event(type_id: std::any::TypeId) -> bool {
            type_id == std::any::TypeId::of::<ExceptionReported>()
                $( || type_id == std::any::TypeId::of::<$name>() )*
        }
    };
}

failures! {
    // --- HTTP -----------------------------------------------------------------

    /// An HTTP request ended in an error response or a panic.
    HttpRequest => HttpRequestFailed {
        /// The request method.
        method: String,
        /// The request path, without the query string — a query is where a
        /// client puts a token.
        path: String,
        /// The matched route's pattern (`/posts/{post}`), when one matched.
        route: Option<String>,
    }

    /// A WebSocket handler failed and the socket was closed.
    WebSocket => WebSocketFailed {
        /// The socket's id.
        socket: String,
        /// The path the socket was opened on.
        path: String,
        /// Which callback failed: `"connect"`, `"message"`, `"close"`.
        stage: String,
    }

    /// The server itself failed at something outside any one request —
    /// accepting a connection, binding, serving.
    Server => ServerFailed {
        /// What it was doing: `"accept"`, `"serve_connection"`, ….
        operation: String,
    }

    /// Middleware failed in a way that did not become the response's error —
    /// a timeout fired, a proxy header was unusable, a stage misbehaved.
    Middleware => MiddlewareFailed {
        /// The middleware's name.
        middleware: String,
        /// What it was doing.
        operation: String,
    }

    // --- auth and sessions ----------------------------------------------------

    /// An authentication guard, token or confirmation check failed
    /// internally — not a wrong password, which is the client's business, but
    /// the guard being unable to decide.
    Auth => AuthFailed {
        /// The guard or component: `"session"`, `"token"`, `"confirm"`, ….
        guard: String,
        /// What it was doing.
        operation: String,
    }

    /// A session or CSRF operation failed: loading, saving, sealing a cookie.
    Session => SessionFailed {
        /// What it was doing: `"load"`, `"save"`, `"csrf_seal"`, ….
        operation: String,
    }

    /// The OAuth authorization server failed while handling a request.
    OAuth => OAuthFailed {
        /// The endpoint or grant: `"token"`, `"authorize"`, ….
        operation: String,
    }

    /// An accounts flow failed partway: a verification or reset link not sent,
    /// a password hash not upgraded.
    Account => AccountFailed {
        /// The flow: `"registration"`, `"login"`, `"password_reset"`, ….
        flow: String,
        /// What it was doing.
        operation: String,
    }

    // --- queues ---------------------------------------------------------------

    /// A queued job's attempt failed — returned an error, panicked, timed out,
    /// or could not be run at all.
    JobAttempt => JobAttemptFailed {
        /// The job's name.
        job: String,
        /// The job's id.
        id: String,
        /// The queue it ran on.
        queue: String,
        /// The connection it came from, when known.
        connection: Option<String>,
        /// Which attempt this was, counting from 1.
        attempt: u32,
        /// Whether this was the last attempt — the job is now failed for good
        /// rather than released to try again.
        final_attempt: bool,
    }

    /// A message was taken off a queue and thrown away without becoming a
    /// job — it could not be read.
    QueueMessageDiscarded => QueueMessageDiscarded {
        /// The driver or connection: `"redis"`, `"sqs"`, `"kafka"`, ….
        connection: String,
        /// The queue, stream or topic.
        queue: String,
        /// The broker's id for the message, when there is one.
        message_id: Option<String>,
    }

    /// A queue operation failed outside a job's own run: acknowledging,
    /// releasing, failing, dead-lettering, cleaning up a Kubernetes job.
    QueueOperation => QueueOperationFailed {
        /// The driver or connection, when known.
        connection: Option<String>,
        /// The queue, when known.
        queue: Option<String>,
        /// What it was doing: `"ack"`, `"release"`, `"fail"`,
        /// `"dead_letter"`, `"push"`, `"kubernetes_cleanup"`, ….
        operation: String,
        /// The job it concerned, when there was one.
        job_id: Option<String>,
    }

    // --- console and schedule -------------------------------------------------

    /// A console command returned an error or panicked.
    Command => CommandFailed {
        /// The command's name.
        command: String,
    }

    /// A scheduled task failed.
    ScheduledTask => ScheduledTaskFailed {
        /// The task's name or description.
        task: String,
    }

    // --- events and background work -------------------------------------------

    /// An event listener failed while its event was being dispatched quietly
    /// — the other listeners still ran.
    Listener => ListenerFailed {
        /// The event's name.
        event: String,
    }

    /// A background task — something spawned to run beside requests — failed.
    BackgroundTask => BackgroundTaskFailed {
        /// The task's name.
        task: String,
    }

    // --- delivery -------------------------------------------------------------

    /// Broadcasting failed: a message could not be published, read or
    /// delivered, or a subscription was refused.
    Broadcast => BroadcastFailed {
        /// The channel, when known.
        channel: Option<String>,
        /// What it was doing: `"publish"`, `"receive"`, `"authorize"`, ….
        operation: String,
    }

    /// A notification could not be delivered on one of its channels.
    Notification => NotificationFailed {
        /// The channel: `"mail"`, `"database"`, `"broadcast"`, ….
        channel: String,
        /// The notification's name.
        notification: String,
    }

    /// Mail could not be sent.
    Mail => MailFailed {
        /// The mailer or transport.
        mailer: String,
        /// What it was doing.
        operation: String,
    }

    /// An event could not be published to an external stream (Kafka, the
    /// relay).
    Publish => PublishFailed {
        /// The topic or destination.
        topic: String,
        /// The event's name.
        event: String,
    }

    /// An incoming webhook could not be handled.
    Webhook => WebhookFailed {
        /// The sender: `"stripe"`, ….
        provider: String,
        /// The sender's id for the event, when known.
        event_id: Option<String>,
        /// What it was doing.
        operation: String,
    }

    // --- storage --------------------------------------------------------------

    /// A cache operation failed and was worked around — a value unreadable, a
    /// write lost, an invalidation missed.
    Cache => CacheFailed {
        /// The store or cache: `"cache"`, `"model_cache"`, ….
        store: String,
        /// The key, when there is one.
        key: Option<String>,
        /// What it was doing.
        operation: String,
    }

    /// A filesystem operation failed and was worked around — a temporary
    /// file not cleaned up, a write retried.
    Filesystem => FilesystemFailed {
        /// The disk: `"local"`, `"s3"`, ….
        disk: String,
        /// What it was doing.
        operation: String,
    }

    /// A database operation failed and was worked around.
    Database => DatabaseFailed {
        /// The connection, when known.
        connection: Option<String>,
        /// What it was doing.
        operation: String,
    }

    // --- outside services and the process -------------------------------------

    /// A call to an outside service failed: a captcha provider, an identity
    /// provider, an HTTP API.
    Integration => IntegrationFailed {
        /// The service.
        service: String,
        /// What it was doing.
        operation: String,
    }

    /// Telemetry failed: an exporter did not flush, a metric could not be
    /// recorded.
    Telemetry => TelemetryFailed {
        /// What it was doing.
        operation: String,
    }

    /// Configuration was invalid or unsafe and was ignored or replaced with a
    /// default.
    Configuration => ConfigurationFailed {
        /// The setting.
        setting: String,
    }

    /// Reported by application code with [`Reporter::report_error`] or
    /// [`report_error`].
    Application => ApplicationFailed {}
}

/// The catch-all event: every report, whatever its kind.
///
/// Dispatched alongside the typed event, so a listener for this sees every
/// failure once.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExceptionReported {
    /// The failure, as its catalogue variant.
    pub failure: Failure,
}

impl ExceptionReported {
    /// Which kind of failure this is.
    pub fn kind(&self) -> FailureKind {
        self.failure.kind()
    }

    /// What failed.
    pub fn exception(&self) -> &Exception {
        self.failure.exception()
    }

    /// When and where it was reported.
    pub fn meta(&self) -> &ReportMeta {
        self.failure.meta()
    }
}

impl fmt::Display for FailureKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// --- the reporter ------------------------------------------------------------

type ContextProvider = Arc<dyn Fn(FailureKind, &mut ReportMeta) + Send + Sync>;

/// Puts failures on the bus.
///
/// Cheap to clone: clones share the same dispatcher. Build one at boot,
/// register it in the container, hand it to the kernel and the worker, and
/// [`install`] it for the components that have no handle to the application.
///
/// By default **everything is reported** — client errors included — and the
/// application decides what to do by what it listens for. [`ignore`](Self::ignore)
/// and [`dont_report`](Self::dont_report) exist for the failures an
/// application never wants on the bus at all.
#[derive(Clone)]
pub struct Reporter {
    events: Arc<Dispatcher>,
    dont_report: Vec<ErrorKind>,
    ignore: Vec<FailureKind>,
    providers: Vec<ContextProvider>,
}

impl Reporter {
    /// A reporter dispatching on `events`, reporting everything.
    pub fn new(events: Arc<Dispatcher>) -> Self {
        Self { events, dont_report: Vec::new(), ignore: Vec::new(), providers: Vec::new() }
    }

    /// Never report errors of `kind`. A panic is still reported.
    pub fn dont_report(mut self, kind: ErrorKind) -> Self {
        if !self.dont_report.contains(&kind) {
            self.dont_report.push(kind);
        }
        self
    }

    /// Never report failures of `kind`.
    pub fn ignore(mut self, kind: FailureKind) -> Self {
        if !self.ignore.contains(&kind) {
            self.ignore.push(kind);
        }
        self
    }

    /// Add context to every report — the release, the environment. Providers
    /// run in the order they were added; a field the failure already carries
    /// in `extra` wins over a provider's.
    pub fn with_context(
        mut self,
        provider: impl Fn(FailureKind, &mut ReportMeta) + Send + Sync + 'static,
    ) -> Self {
        self.providers.push(Arc::new(provider));
        self
    }

    /// The dispatcher reports go out on.
    pub fn events(&self) -> &Arc<Dispatcher> {
        &self.events
    }

    /// Whether a failure of `kind` with `exception` would be reported.
    pub fn should_report(&self, kind: FailureKind, exception: &Exception) -> bool {
        if self.ignore.contains(&kind) {
            return false;
        }
        if exception.panic {
            return true;
        }
        !exception.kind.is_some_and(|k| self.dont_report.contains(&k))
    }

    /// Report a failure. Returns whether it was reported.
    ///
    /// Dispatches the typed event, then [`ExceptionReported`].
    pub async fn report(&self, failure: impl Into<Failure>) -> bool {
        let mut failure = failure.into();
        let kind = failure.kind();
        if !self.should_report(kind, failure.exception()) {
            return false;
        }

        let meta = failure.meta_mut();
        let own = std::mem::take(&mut meta.extra);
        for provider in &self.providers {
            provider(kind, meta);
        }
        meta.extra.extend(own);
        stamp(meta);

        failure.clone().dispatch_typed(&self.events).await;
        self.events.dispatch_quietly(ExceptionReported { failure }).await;
        true
    }

    /// Report an error from application code, as an [`ApplicationFailed`].
    pub async fn report_error(&self, exception: impl Into<Exception>) -> bool {
        self.report(ApplicationFailed::new(exception)).await
    }
}

impl fmt::Debug for Reporter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Reporter")
            .field("dont_report", &self.dont_report)
            .field("ignore", &self.ignore)
            .field("providers", &self.providers.len())
            .finish()
    }
}

/// Fill in the parts of `meta` nobody set.
fn stamp(meta: &mut ReportMeta) {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    if meta.reported_at_ms == 0 {
        meta.reported_at_ms = u64::try_from(now.as_millis()).unwrap_or(u64::MAX);
    }
    if meta.pid == 0 {
        meta.pid = std::process::id();
    }
    if meta.host.is_none() {
        meta.host = std::env::var("HOSTNAME").ok().filter(|h| !h.is_empty());
    }
    if meta.id.is_empty() {
        let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        meta.id = format!("{:x}-{:x}-{:x}", now.as_nanos(), meta.pid, sequence);
    }
}

// --- the process-wide reporter -----------------------------------------------

static INSTALLED: RwLock<Option<Reporter>> = RwLock::new(None);
static CAPTURING: AtomicBool = AtomicBool::new(false);
static CAPTURE_LOCK: Mutex<()> = Mutex::new(());

/// Make `reporter` the process-wide reporter, returning the one it replaces.
///
/// The framework installs the application's reporter at boot. While a
/// [`capture`] is active this does nothing and returns `None`, so a test
/// booting an application cannot steal the capture's reports.
pub fn install(reporter: Reporter) -> Option<Reporter> {
    if CAPTURING.load(Ordering::SeqCst) {
        return None;
    }
    INSTALLED.write().unwrap_or_else(|e| e.into_inner()).replace(reporter)
}

/// Remove the process-wide reporter, returning it.
pub fn uninstall() -> Option<Reporter> {
    if CAPTURING.load(Ordering::SeqCst) {
        return None;
    }
    INSTALLED.write().unwrap_or_else(|e| e.into_inner()).take()
}

/// The process-wide reporter, if one is installed.
pub fn installed() -> Option<Reporter> {
    INSTALLED.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Report through the process-wide reporter. Returns `false` when none is
/// installed or the failure was filtered out.
pub async fn report(failure: impl Into<Failure>) -> bool {
    match installed() {
        Some(reporter) => reporter.report(failure).await,
        None => false,
    }
}

/// Report an error from application code through the process-wide reporter.
pub async fn report_error(exception: impl Into<Exception>) -> bool {
    report(ApplicationFailed::new(exception)).await
}

/// Report through the process-wide reporter **without waiting** — for code
/// that is not `async`, or must not be held up by listeners.
///
/// Spawns onto the current Tokio runtime. Outside one, or with no reporter
/// installed, the failure is only logged.
pub fn report_detached(failure: impl Into<Failure>) {
    let failure = failure.into();
    let Some(reporter) = installed() else {
        return;
    };
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            handle.spawn(async move {
                reporter.report(failure).await;
            });
        }
        Err(_) => tracing::error!(
            kind = failure.kind().as_str(),
            error = %failure.exception(),
            "could not report a failure: no async runtime"
        ),
    }
}

/// Install a collecting reporter for the duration of a test, and read back
/// what was reported through the process-wide reporter.
///
/// Holds a process-wide lock, so captures in parallel tests run one at a
/// time; [`install`] is ignored while one is active. Dropping it restores the
/// reporter that was installed before.
///
/// ```
/// # use rainier_events::report::{self, BackgroundTaskFailed};
/// # use rainier_support::Error;
/// # #[tokio::main] async fn main() {
/// let capture = report::capture();
/// report::report(BackgroundTaskFailed::new(Error::internal("x"), "sweeper")).await;
/// assert_eq!(capture.reports().len(), 1);
/// # }
/// ```
pub fn capture() -> Capture {
    let guard = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let events = Arc::new(Dispatcher::new());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    events.listen(move |reported: Arc<ExceptionReported>| {
        let sink = Arc::clone(&sink);
        async move {
            sink.lock().unwrap_or_else(|e| e.into_inner()).push((*reported).clone());
            Ok(())
        }
    });
    let previous = INSTALLED
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .replace(Reporter::new(Arc::clone(&events)));
    CAPTURING.store(true, Ordering::SeqCst);
    Capture { _guard: guard, events, seen, previous }
}

/// An active [`capture`].
pub struct Capture {
    _guard: MutexGuard<'static, ()>,
    events: Arc<Dispatcher>,
    seen: Arc<Mutex<Vec<ExceptionReported>>>,
    previous: Option<Reporter>,
}

impl Capture {
    /// Everything reported so far.
    pub fn reports(&self) -> Vec<ExceptionReported> {
        self.seen.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The reports of one kind.
    pub fn of_kind(&self, kind: FailureKind) -> Vec<Failure> {
        self.reports().into_iter().filter(|r| r.kind() == kind).map(|r| r.failure).collect()
    }

    /// The capture's dispatcher, to listen for typed events on.
    pub fn events(&self) -> &Arc<Dispatcher> {
        &self.events
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        CAPTURING.store(false, Ordering::SeqCst);
        *INSTALLED.write().unwrap_or_else(|e| e.into_inner()) = self.previous.take();
    }
}

impl fmt::Debug for Capture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Capture").field("reports", &self.reports().len()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collecting() -> (Arc<Dispatcher>, Arc<Mutex<Vec<ExceptionReported>>>) {
        let events = Arc::new(Dispatcher::new());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        events.listen(move |reported: Arc<ExceptionReported>| {
            let sink = Arc::clone(&sink);
            async move {
                sink.lock().unwrap().push((*reported).clone());
                Ok(())
            }
        });
        (events, seen)
    }

    fn http(error: Error) -> HttpRequestFailed {
        HttpRequestFailed::new(error, "GET", "/posts/1", Some("/posts/{post}".to_string()))
    }

    #[tokio::test]
    async fn a_failure_is_dispatched_as_its_typed_event_and_as_the_catch_all() {
        let (events, seen) = collecting();
        let typed = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&typed);
        events.listen(move |failed: Arc<HttpRequestFailed>| {
            let sink = Arc::clone(&sink);
            async move {
                sink.lock().unwrap().push((*failed).clone());
                Ok(())
            }
        });

        assert!(Reporter::new(events).report(http(Error::internal("disk full"))).await);

        let typed = typed.lock().unwrap();
        assert_eq!(typed.len(), 1);
        assert_eq!(typed[0].method, "GET");
        assert_eq!(typed[0].route.as_deref(), Some("/posts/{post}"));
        assert_eq!(typed[0].exception.message, "disk full");
        assert!(!typed[0].meta.id.is_empty(), "the typed event is stamped too");

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].kind(), FailureKind::HttpRequest);
        assert_eq!(seen[0].meta().id, typed[0].meta.id, "one report, one id");
        assert!(matches!(&seen[0].failure, Failure::HttpRequest(f) if f.path == "/posts/1"));
    }

    #[tokio::test]
    async fn everything_is_reported_by_default_client_errors_included() {
        let (events, seen) = collecting();
        let reporter = Reporter::new(events);

        assert!(reporter.report(http(Error::not_found("no post"))).await);
        assert!(reporter.report_error(Error::internal("x")).await);
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn dont_report_and_ignore_filter_but_a_panic_still_gets_through() {
        let (events, seen) = collecting();
        let reporter =
            Reporter::new(events).dont_report(ErrorKind::NotFound).ignore(FailureKind::Cache);

        assert!(!reporter.report(http(Error::not_found("no post"))).await);
        assert!(
            !reporter
                .report(CacheFailed::new(
                    Error::internal("x"),
                    "cache",
                    Some("k".to_string()),
                    "get"
                ))
                .await
        );
        assert!(
            reporter
                .report(HttpRequestFailed::new(Exception::panic("boom", None), "GET", "/", None))
                .await
        );

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert!(seen[0].exception().panic);
        assert_eq!(seen[0].exception().kind_label(), "Panic");
    }

    #[tokio::test]
    async fn context_providers_fill_in_around_the_failures_own_fields() {
        let (events, seen) = collecting();
        let reporter = Reporter::new(events).with_context(|kind, meta| {
            meta.extra.insert("release".into(), "1.2.3".into());
            meta.extra.insert("tenant".into(), "default".into());
            meta.extra.insert("kind".into(), kind.as_str().into());
        });

        let failure =
            CommandFailed::new(Error::internal("x"), "migrate").with_extra("tenant", "acme");
        assert!(reporter.report(failure).await);

        let seen = seen.lock().unwrap();
        let meta = seen[0].meta();
        assert_eq!(meta.extra["release"], "1.2.3");
        assert_eq!(meta.extra["tenant"], "acme");
        assert_eq!(meta.extra["kind"], "Command");
        assert_eq!(meta.pid, std::process::id());
        assert!(meta.reported_at_ms > 0);
    }

    #[tokio::test]
    async fn the_cause_chain_is_kept() {
        let (events, seen) = collecting();
        let source = anyhow::anyhow!("connection refused").context("connecting to mariadb");

        Reporter::new(events)
            .report_error(Error::internal("query failed").with_source(source))
            .await;

        let seen = seen.lock().unwrap();
        assert_eq!(
            seen[0].exception().sources,
            vec!["connecting to mariadb", "connection refused"]
        );
        assert_eq!(
            seen[0].exception().to_string(),
            "query failed: connecting to mariadb: connection refused"
        );
    }

    #[tokio::test]
    async fn a_report_survives_a_round_trip_through_json_without_the_original_error() {
        let (events, seen) = collecting();
        Reporter::new(events)
            .report(JobAttemptFailed::new(
                Error::internal("boom"),
                "SendInvoice",
                "42",
                "default",
                Some("redis".to_string()),
                3u32,
                true,
            ))
            .await;

        let reported = seen.lock().unwrap()[0].clone();
        let json = serde_json::to_value(&reported).unwrap();
        assert_eq!(json["failure"]["type"], "JobAttempt");
        assert_eq!(json["failure"]["job"], "SendInvoice");
        assert_eq!(json["failure"]["exception"]["kind"], "Internal");

        let back: ExceptionReported = serde_json::from_value(json).unwrap();
        let Failure::JobAttempt(job) = back.failure else { panic!("wrong variant") };
        assert_eq!(job.attempt, 3);
        assert!(job.final_attempt);
        assert_eq!(job.exception.message, "boom");
        assert!(job.exception.error.is_none());
        assert_eq!(job.meta, reported.meta().clone());
    }

    #[tokio::test]
    async fn a_failing_listener_does_not_stop_the_others() {
        let (events, seen) = collecting();
        events.listen(|_: Arc<HttpRequestFailed>| async { Err(Error::internal("tracker down")) });

        assert!(Reporter::new(events).report(http(Error::internal("x"))).await);
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_faked_dispatcher_records_both_events() {
        let events = Arc::new(Dispatcher::fake());
        Reporter::new(Arc::clone(&events)).report_error(Error::internal("x")).await;
        events.assert_dispatched_times::<ApplicationFailed>(1);
        events.assert_dispatched_times::<ExceptionReported>(1);
    }

    #[test]
    fn the_catalogue_is_complete_and_named() {
        assert_eq!(FailureKind::ALL.len(), 27);
        assert_eq!(FailureKind::HttpRequest.as_str(), "HttpRequest");
        assert_eq!(FailureKind::QueueMessageDiscarded.to_string(), "QueueMessageDiscarded");
        assert!(is_failure_event(std::any::TypeId::of::<JobAttemptFailed>()));
        assert!(is_failure_event(std::any::TypeId::of::<ExceptionReported>()));
        assert!(!is_failure_event(std::any::TypeId::of::<String>()));
    }

    #[tokio::test]
    async fn the_process_wide_reporter_and_capture() {
        let capture = capture();
        assert!(report(BackgroundTaskFailed::new(Error::internal("x"), "sweeper")).await);
        assert!(report_error(Error::internal("y")).await);

        // Installing while capturing is ignored.
        assert!(install(Reporter::new(Arc::new(Dispatcher::new()))).is_none());
        assert!(report_error(Error::internal("z")).await);

        assert_eq!(capture.reports().len(), 3);
        assert_eq!(capture.of_kind(FailureKind::BackgroundTask).len(), 1);
    }

    #[tokio::test]
    async fn report_detached_reaches_the_bus() {
        let capture = capture();
        report_detached(BackgroundTaskFailed::new(Error::internal("x"), "sweeper"));
        for _ in 0..100 {
            if !capture.reports().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(capture.reports().len(), 1);
    }

    #[tokio::test]
    async fn a_failing_listener_is_reported_but_a_failing_report_listener_is_not() {
        struct Ping;
        let capture = capture();
        let events = Dispatcher::new();
        events.listen(|_: Arc<Ping>| async { Err(Error::internal("listener broke")) });
        events.listen(|_: Arc<ExceptionReported>| async { Err(Error::internal("tracker down")) });

        assert_eq!(events.dispatch_quietly(Ping).await, 1);
        Reporter::new(Arc::new(events)).report_error(Error::internal("x")).await;

        // Other tests' listeners fail in parallel and land here too, so look
        // only for this test's events.
        let listener_failures: Vec<ListenerFailed> = capture
            .of_kind(FailureKind::Listener)
            .into_iter()
            .filter_map(|f| match f {
                Failure::Listener(l) => Some(l),
                _ => None,
            })
            .collect();
        let ping: Vec<_> = listener_failures.iter().filter(|l| l.event.ends_with("Ping")).collect();
        assert_eq!(ping.len(), 1);
        assert_eq!(ping[0].exception.message, "listener broke");
        assert!(
            !listener_failures.iter().any(|l| l.event.ends_with("ExceptionReported")),
            "the catch-all's own failing listener does not loop back"
        );
    }
}
