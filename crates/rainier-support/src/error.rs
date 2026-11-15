//! The framework error type.
//!
//! Every Rainier component fails with the same [`Error`], and every `Error`
//! carries the two things the HTTP layer needs to render it without knowing
//! where it came from: an [`ErrorKind`] that maps to a status code, and an
//! optional structured `details` payload.
//!
//! That is what keeps the crate graph acyclic. `rainier-validation` cannot
//! add a `Validation` variant to an enum in `rainier-support` (that would be a
//! cycle), so instead it builds `Error::validation(details)` and the exception
//! handler in `rainier-http` renders `details` as JSON. The same trick serves
//! auth, authorization, and model-not-found — no component needs to know the
//! others' error types.

use std::backtrace::{Backtrace, BacktraceStatus};
use std::fmt;
use std::sync::{Arc, OnceLock};

/// The class of failure, which is also the HTTP status it renders as.
///
/// Each domain failure names its status (authentication → 401, validation
/// → 422, …) so the "throw a domain error, get the right response" flow
/// works without any component knowing the renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ErrorKind {
    /// 400 — the request was malformed.
    BadRequest,
    /// 401 — no (or invalid) credentials. Raised by guards.
    Unauthenticated,
    /// 403 — authenticated but not permitted. Raised by gates/policies.
    Unauthorized,
    /// 404 — no such route or model.
    NotFound,
    /// 405 — the route exists but not for this method.
    MethodNotAllowed,
    /// 409 — the request conflicts with current state.
    Conflict,
    /// 413 — the body exceeded the configured limit.
    PayloadTooLarge,
    /// 422 — the request was well-formed but failed validation.
    Validation,
    /// 429 — throttled.
    TooManyRequests,
    /// 500 — an unhandled internal failure.
    Internal,
    /// 503 — the app is down for maintenance or a dependency is unavailable.
    ServiceUnavailable,
    /// Any other status, set explicitly.
    Status(u16),
}

impl ErrorKind {
    /// The HTTP status this kind renders as.
    pub fn status(self) -> u16 {
        match self {
            ErrorKind::BadRequest => 400,
            ErrorKind::Unauthenticated => 401,
            ErrorKind::Unauthorized => 403,
            ErrorKind::NotFound => 404,
            ErrorKind::MethodNotAllowed => 405,
            ErrorKind::Conflict => 409,
            ErrorKind::PayloadTooLarge => 413,
            ErrorKind::Validation => 422,
            ErrorKind::TooManyRequests => 429,
            ErrorKind::Internal => 500,
            ErrorKind::ServiceUnavailable => 503,
            ErrorKind::Status(code) => code,
        }
    }

    /// `true` for 5xx — the errors worth logging as faults rather than as
    /// ordinary client mistakes.
    pub fn is_server_error(self) -> bool {
        self.status() >= 500
    }
}

/// A framework error: a [`kind`](Error::kind), a human message, an optional
/// structured `details` payload, and an optional underlying cause.
#[derive(Debug)]
pub struct Error {
    kind: ErrorKind,
    message: String,
    details: Option<serde_json::Value>,
    source: Option<anyhow::Error>,
    backtrace: Option<Arc<Stack>>,
}

/// A captured backtrace and its text, rendered the first time somebody asks.
///
/// Behind the one `Arc` so the cache costs `Error` nothing: every `Result` in
/// an application carries an `Error`, and an inline `OnceLock<String>` (32
/// bytes) pushed it past clippy's `result_large_err` limit.
#[derive(Debug)]
struct Stack {
    backtrace: Arc<Backtrace>,
    rendered: OnceLock<String>,
}

impl Stack {
    fn new(backtrace: Arc<Backtrace>) -> Arc<Self> {
        Arc::new(Self { backtrace, rendered: OnceLock::new() })
    }
}

impl Error {
    /// Build an error of an explicit kind.
    ///
    /// Captures a backtrace, which is what lets a debug error page show where
    /// the failure came from rather than where it was rendered. Those are
    /// different stacks and only the first one is useful: by the time the
    /// kernel's exception renderer runs, the handler that failed has already
    /// returned.
    ///
    /// **This is free unless somebody asked for it.**
    /// [`Backtrace::capture`] consults `RUST_BACKTRACE` / `RUST_LIB_BACKTRACE`
    /// once, and with neither set it returns [`BacktraceStatus::Disabled`]
    /// without walking a single frame. So the cost in production is one
    /// already-cached atomic read and an `Option` that is always `None` — and
    /// a developer gets the stack by exporting one variable, with no rebuild.
    ///
    /// **A client error captures none in a release build**, even with
    /// `RUST_BACKTRACE` set: a 404, a 401 or a failed validation is the
    /// client's doing, and its stack says nothing worth the walk — tens of
    /// microseconds, and several times that again to render it — on every
    /// one of them. A debug build still captures one, for the debug error
    /// page; [`with_kind`](Self::with_kind) captures one when it turns an
    /// error into a server error.
    ///
    /// `Arc`, because `Error` is cloned onto a `RenderedError` and moved
    /// between tasks, and a backtrace is comparatively large.
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        let backtrace =
            if kind.is_server_error() || cfg!(debug_assertions) { capture() } else { None };
        Self { kind, message: message.into(), details: None, source: None, backtrace }
    }

    /// 500 — an internal failure.
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Internal, message)
    }

    /// 400 — a malformed request.
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::BadRequest, message)
    }

    /// 401 — missing or invalid credentials.
    pub fn unauthenticated(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Unauthenticated, message)
    }

    /// 403 — authenticated but not permitted.
    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Unauthorized, message)
    }

    /// 404 — nothing matched.
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::NotFound, message)
    }

    /// 409 — conflicting state.
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Conflict, message)
    }

    /// 408 — the request took longer than this service is prepared to wait.
    ///
    /// What a [`Timeout`](https://docs.rs/rainier-middleware) middleware
    /// returns. `408` rather than `504`: the timeout is this service's own
    /// decision about its own handler, not a report about something upstream.
    pub fn request_timeout(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Status(408), message)
    }

    /// 429 — throttled.
    ///
    /// The caller should also send a `retry-after`; nothing here can know what
    /// to put in it.
    pub fn too_many_requests(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::TooManyRequests, message)
    }

    /// 503 — a dependency is down, or the application is in maintenance.
    ///
    /// The honest answer when a health check cannot reach the database. A 500
    /// says "this service is broken"; a 503 says "try again", which is what a
    /// load balancer and a retrying client both need to hear.
    pub fn service_unavailable(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::ServiceUnavailable, message)
    }

    /// 422 with a structured field-error payload — what a failed
    /// `FormRequest` produces.
    pub fn validation(details: serde_json::Value) -> Self {
        Self::new(ErrorKind::Validation, "The given data was invalid.").with_details(details)
    }

    /// Attach a structured payload rendered alongside the message.
    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = Some(details);
        self
    }

    /// Attach the underlying cause.
    pub fn with_source(mut self, source: impl Into<anyhow::Error>) -> Self {
        self.source = Some(source.into());
        self
    }

    /// Re-label the kind (and therefore the status) without losing the rest.
    pub fn with_kind(mut self, kind: ErrorKind) -> Self {
        // A client error built in a release build has no stack; one that
        // becomes a server error gets this one, the nearest there is.
        if self.backtrace.is_none() && kind.is_server_error() && !self.kind.is_server_error() {
            self.backtrace = capture();
        }
        self.kind = kind;
        self
    }

    /// This error's kind.
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// The HTTP status this error renders as.
    pub fn status(&self) -> u16 {
        self.kind.status()
    }

    /// The human-readable message.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The structured payload, if any.
    pub fn details(&self) -> Option<&serde_json::Value> {
        self.details.as_ref()
    }

    /// The underlying cause, if one was attached.
    pub fn source_error(&self) -> Option<&anyhow::Error> {
        self.source.as_ref()
    }

    /// The stack this error was constructed on, when one was captured.
    ///
    /// `None` unless `RUST_BACKTRACE` (or `RUST_LIB_BACKTRACE`) was set for
    /// the process, and for a client error in a release build — see
    /// [`Error::new`]. Read by the debug error page in
    /// `rainier-debug`; nothing in a production path looks at it.
    pub fn backtrace(&self) -> Option<&Backtrace> {
        self.backtrace.as_deref().map(|stack| &*stack.backtrace)
    }

    /// [`backtrace`](Self::backtrace) rendered as text — symbolised once,
    /// however many times it is asked for.
    ///
    /// Symbolising is most of a backtrace's cost, and one failed request asks
    /// for the text two or three times: the rendered response, the failure
    /// report, an auth failure's own report. Use this rather than
    /// `backtrace().map(ToString::to_string)`.
    pub fn backtrace_text(&self) -> Option<&str> {
        let stack = self.backtrace.as_deref()?;
        Some(stack.rendered.get_or_init(|| stack.backtrace.to_string()))
    }

    /// Adopt a backtrace captured somewhere else.
    ///
    /// For the one case that cannot capture its own: a panic. The kernel
    /// catches panics with `catch_unwind`, which discards the stack — so a
    /// panic hook records it and the kernel attaches it here. Without this the
    /// error page for a panic would show the unwinding site rather than the
    /// `panic!`.
    pub fn with_backtrace(mut self, backtrace: Arc<Backtrace>) -> Self {
        self.backtrace = Some(Stack::new(backtrace));
        self
    }
}

/// A backtrace of the calling stack, when `RUST_BACKTRACE` asks for one.
fn capture() -> Option<Arc<Stack>> {
    match Backtrace::capture() {
        captured if captured.status() == BacktraceStatus::Captured => {
            Some(Stack::new(Arc::new(captured)))
        }
        _ => None,
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)?;
        if let Some(source) = &self.source {
            write!(f, ": {source}")?;
        }
        Ok(())
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.as_ref().map(|e| e.as_ref() as &(dyn std::error::Error + 'static))
    }
}

// `anyhow::Error` is what rainier_orm fails with, so this conversion is the
// seam between the ORM's errors and the framework's. A database failure is
// internal until something upstream re-labels it (e.g. a repository turning a
// missing row into `not_found`).
impl From<anyhow::Error> for Error {
    fn from(source: anyhow::Error) -> Self {
        Self::internal(source.to_string()).with_source(source)
    }
}

impl From<std::io::Error> for Error {
    fn from(source: std::io::Error) -> Self {
        let kind = match source.kind() {
            std::io::ErrorKind::NotFound => ErrorKind::NotFound,
            std::io::ErrorKind::PermissionDenied => ErrorKind::Unauthorized,
            _ => ErrorKind::Internal,
        };
        Self::new(kind, source.to_string()).with_source(anyhow::Error::new(source))
    }
}

impl From<serde_json::Error> for Error {
    fn from(source: serde_json::Error) -> Self {
        Self::bad_request(format!("malformed JSON: {source}"))
            .with_source(anyhow::Error::new(source))
    }
}

impl From<std::fmt::Error> for Error {
    fn from(source: std::fmt::Error) -> Self {
        Self::internal(source.to_string())
    }
}

/// The framework result type.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Adds `.context(..)` to any `Result` whose error converts into [`Error`],
/// the way `anyhow::Context` does — but preserving the [`ErrorKind`] when the
/// error already is a Rainier error, so adding context to a 404 does not
/// silently turn it into a 500.
pub trait Context<T> {
    /// Prefix the error message with `context`.
    fn context(self, context: impl fmt::Display) -> Result<T>;

    /// Prefix the error message with a lazily built `context`.
    fn with_context<C: fmt::Display>(self, context: impl FnOnce() -> C) -> Result<T>;
}

impl<T, E> Context<T> for Result<T, E>
where
    E: Into<Error>,
{
    fn context(self, context: impl fmt::Display) -> Result<T> {
        self.map_err(|e| {
            let e = e.into();
            let kind = e.kind();
            Error::new(kind, format!("{context}: {e}"))
        })
    }

    fn with_context<C: fmt::Display>(self, context: impl FnOnce() -> C) -> Result<T> {
        self.map_err(|e| {
            let e = e.into();
            let kind = e.kind();
            Error::new(kind, format!("{}: {e}", context()))
        })
    }
}

impl<T> Context<T> for Option<T> {
    fn context(self, context: impl fmt::Display) -> Result<T> {
        self.ok_or_else(|| Error::internal(context.to_string()))
    }

    fn with_context<C: fmt::Display>(self, context: impl FnOnce() -> C) -> Result<T> {
        self.ok_or_else(|| Error::internal(context().to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_error_stays_small_enough_to_return() {
        // Clippy's `result_large_err` threshold: every application `Result`
        // carries an `Error`, so outgrowing it fails their lint by the
        // thousand.
        assert!(std::mem::size_of::<Error>() <= 128, "{}", std::mem::size_of::<Error>());
    }

    #[test]
    fn the_backtrace_text_is_rendered_once_and_is_the_backtrace() {
        let error = Error::internal("x").with_backtrace(Arc::new(Backtrace::force_capture()));
        let first = error.backtrace_text().expect("adopted a stack");
        assert_eq!(first, error.backtrace().expect("adopted a stack").to_string());
        let again = error.backtrace_text().expect("adopted a stack");
        assert!(std::ptr::eq(first, again), "rendered once, then reused");
        let plain = Error::internal("y");
        assert_eq!(plain.backtrace_text().is_some(), plain.backtrace().is_some());
    }

    #[test]
    fn a_client_error_promoted_to_a_server_error_keeps_or_gains_a_stack() {
        // With `RUST_BACKTRACE` unset nothing captures, so this only checks
        // that promoting never loses the stack a client error had.
        let error = Error::not_found("x");
        let had = error.backtrace().is_some();
        let promoted = error.with_kind(ErrorKind::Internal);
        assert!(!had || promoted.backtrace().is_some());
        assert_eq!(promoted.status(), 500);
    }

    #[test]
    fn kinds_map_to_statuses() {
        assert_eq!(ErrorKind::Validation.status(), 422);
        assert_eq!(ErrorKind::Unauthenticated.status(), 401);
        assert_eq!(ErrorKind::Status(418).status(), 418);
        assert!(ErrorKind::Internal.is_server_error());
        assert!(!ErrorKind::NotFound.is_server_error());
    }

    #[test]
    fn context_preserves_the_kind() {
        let err: Result<()> = Err(Error::not_found("no such post"));
        let err = err.context("loading the post").unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);
        assert_eq!(err.message(), "loading the post: no such post");
    }

    #[test]
    fn validation_errors_carry_their_details() {
        let err = Error::validation(serde_json::json!({ "email": ["required"] }));
        assert_eq!(err.status(), 422);
        assert!(err.details().is_some());
    }

    #[test]
    fn anyhow_errors_arrive_as_internal() {
        let err: Error = anyhow::anyhow!("connection reset").into();
        assert_eq!(err.status(), 500);
        assert!(err.source_error().is_some());
    }
}
