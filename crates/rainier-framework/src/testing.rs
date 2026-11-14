//! A test harness — [`TestApp`] and [`TestResponse`].
//!
//! ```ignore
//! let app = TestApp::new(boot(Mode::Testing).await?)?;
//!
//! app.get("/health/ready").await
//!     .assert_ok()
//!     .assert_json_path("database.status", "ok");
//!
//! let user: UserView = app.get("/api/me").await.json();
//! ```
//!
//! A feature test is three lines — make a
//! request, assert the status, assert something about the body — and anything
//! longer is the harness leaking.
//!
//! # It scopes the facades to *this* test
//!
//! The container facades resolve through is process-wide, so booting one
//! application per test used to race: two tests boot, the second wins, and the
//! first resolves out of a container that has been replaced. The symptom is
//! `nothing is bound for rainier_config::repository::Config` on a random
//! subset of tests on a random subset of runs.
//!
//! [`TestApp`] holds a [`FacadeScope`], which
//! overrides the global one **for the thread the test runs on**. So each test
//! gets its own application and its own configuration, and they do not
//! interfere.
//!
//! `#[tokio::test]` uses a current-thread runtime, so a test and everything it
//! awaits stay on that thread. Under `#[tokio::test(flavor = "multi_thread")]`
//! a future can move, and after the move a facade resolves through whatever
//! was installed globally — so a multi-threaded test wanting its own container
//! should keep the work on one task, or install its application globally and
//! not run in parallel with another that does.
//!
//! # It records what was reported
//!
//! A booted application publishes every failure through its [`Reporter`] —
//! see [`crate::reporting`]. The harness listens on that reporter's
//! dispatcher from the moment it is built, so a test can assert a failure was
//! reported without wiring anything — as the catch-all, or as its own type:
//!
//! ```ignore
//! use rainier_framework::report::HttpRequestFailed;
//!
//! app.get("/explodes").await.assert_status(StatusCode::INTERNAL_SERVER_ERROR);
//! assert_eq!(app.reported()[0].exception().message, "disk full");
//! assert_eq!(app.reported_of::<HttpRequestFailed>()[0].path, "/explodes");
//! ```
//!
//! What it sees is what reached **this application's** reporter. A failure
//! reported through the process-wide reporter — by a component with no handle
//! to the application, or a failing event listener — reaches it only when
//! this application was installed process-wide, and a test application
//! booted [`without_facades`](crate::Rainier::without_facades) is not. For
//! those, take a [`rainier_events::report::capture`] in the test.
//!
//! Reports made **during** boot happened before the harness existed; listen
//! with [`Rainier::with_events`](crate::Rainier::with_events) for them.
//!
//! Swapping a [`Dispatcher::fake()`](rainier_events::Dispatcher::fake) into
//! the container after boot does **not** capture reports: the reporter was
//! handed the real dispatcher at boot. A [`Reporter`] built over a fake, in a
//! unit test, records them as usual.
//!
//! [`FacadeScope`]: rainier_container::FacadeScope

use std::any::Any;
use std::sync::{Arc, Mutex};

use serde::de::DeserializeOwned;
use serde_json::Value;

use rainier_container::{scope_facade_application, Application, FacadeScope};
use rainier_events::{ExceptionReported, Failure, FailureEvent, Reporter};
use rainier_http::{Method, Request, Response, StatusCode};
use rainier_server::Kernel;
use rainier_support::Result;

/// A booted application, ready to be asked for responses.
///
/// Not `Send`: it holds a thread-local facade scope, and moving it to another
/// thread would unscope on one and pop on another.
pub struct TestApp {
    app: Arc<Application>,
    kernel: Arc<Kernel>,
    /// Dropped with the harness, which is what ends the scope.
    _scope: FacadeScope,
    /// Headers added to every request — a bearer token, usually.
    defaults: Vec<(String, String)>,
    /// Every report the application's reporter dispatched since the harness
    /// was built.
    reported: Arc<Mutex<Vec<ExceptionReported>>>,
}

/// The catalogue variant's struct, as `F` — the original error included.
fn downcast<F: FailureEvent>(failure: Failure) -> Option<F> {
    fn cast<T: Any, F: Any>(value: T) -> Option<F> {
        (Box::new(value) as Box<dyn Any>).downcast::<F>().ok().map(|boxed| *boxed)
    }

    macro_rules! variants {
        ($($variant:ident),* $(,)?) => {
            match failure {
                $( Failure::$variant(failure) => cast(failure), )*
                // A kind added to the catalogue after this list: through its
                // JSON, which keeps everything but the original error.
                other => serde_json::to_value(&other).ok().and_then(|v| serde_json::from_value(v).ok()),
            }
        };
    }

    variants!(
        HttpRequest,
        WebSocket,
        Server,
        Middleware,
        Auth,
        Session,
        OAuth,
        Account,
        JobAttempt,
        QueueMessageDiscarded,
        QueueOperation,
        Command,
        ScheduledTask,
        Listener,
        BackgroundTask,
        Broadcast,
        Notification,
        Mail,
        Publish,
        Webhook,
        Cache,
        Database,
        Integration,
        Telemetry,
        Filesystem,
        Configuration,
        Application,
    )
}

impl TestApp {
    /// Wrap an application your own `boot` produced.
    ///
    /// Takes the booted application rather than booting one, because what a
    /// test wants exercised is *your* bootstrap — its providers, its
    /// configuration, its routes — not a generic one this crate could build.
    pub fn new(app: Arc<Application>) -> Result<Self> {
        let kernel = app.resolve::<Kernel>()?;
        let scope = scope_facade_application(Arc::clone(&app));

        let reported = Arc::new(Mutex::new(Vec::new()));
        if let Ok(reporter) = app.resolve::<Reporter>() {
            let sink = Arc::clone(&reported);
            reporter.events().listen(move |report: Arc<ExceptionReported>| {
                let sink = Arc::clone(&sink);
                async move {
                    sink.lock().expect("report log poisoned").push((*report).clone());
                    Ok(())
                }
            });
        }

        Ok(Self { app, kernel, _scope: scope, defaults: Vec::new(), reported })
    }

    /// Every failure the application reported since this harness was built,
    /// in order, as the catch-all [`ExceptionReported`] — the HTTP kernel's,
    /// and anything the test triggered through [`crate::report()`], the
    /// console, the scheduler or the worker.
    pub fn reported(&self) -> Vec<ExceptionReported> {
        self.reported.lock().expect("report log poisoned").clone()
    }

    /// The reported failures of one type, in order —
    /// `app.reported_of::<HttpRequestFailed>()` — with their own fields to
    /// assert on.
    pub fn reported_of<F: FailureEvent>(&self) -> Vec<F> {
        self.reported()
            .into_iter()
            .filter(|reported| reported.kind() == F::KIND)
            .filter_map(|reported| downcast::<F>(reported.failure))
            .collect()
    }

    /// Panic if anything was reported.
    ///
    /// # Panics
    ///
    /// If a failure was reported, naming each one.
    pub fn assert_nothing_reported(&self) -> &Self {
        let reported = self.reported();
        assert!(
            reported.is_empty(),
            "expected nothing to be reported, but {} failure(s) were: {:?}",
            reported.len(),
            reported.iter().map(|r| format!("{}: {}", r.kind(), r.exception())).collect::<Vec<_>>()
        );
        self
    }

    /// The application, for resolving a service to assert on.
    pub fn app(&self) -> &Arc<Application> {
        &self.app
    }

    /// Resolve a service — `app.resolve::<PostRepository>()?`.
    pub fn resolve<T: Send + Sync + 'static>(&self) -> Result<Arc<T>> {
        self.app.resolve::<T>()
    }

    /// Send this header on every subsequent request.
    ///
    /// ```ignore
    /// let app = app.with_header("authorization", format!("Bearer {token}"));
    /// ```
    #[must_use = "this returns a configured harness rather than configuring in place"]
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.defaults.push((name.into(), value.into()));
        self
    }

    /// Authenticate every subsequent request with a bearer token.
    #[must_use = "this returns a configured harness rather than configuring in place"]
    pub fn with_token(self, token: impl std::fmt::Display) -> Self {
        self.with_header("authorization", format!("Bearer {token}"))
    }

    /// `GET uri`.
    pub async fn get(&self, uri: &str) -> TestResponse {
        self.send(self.request(Method::GET, uri).build()).await
    }

    /// `DELETE uri`.
    pub async fn delete(&self, uri: &str) -> TestResponse {
        self.send(self.request(Method::DELETE, uri).build()).await
    }

    /// `POST uri` with a JSON body.
    pub async fn post(&self, uri: &str, body: &impl serde::Serialize) -> TestResponse {
        self.send(self.request(Method::POST, uri).json(body).build()).await
    }

    /// `PUT uri` with a JSON body.
    pub async fn put(&self, uri: &str, body: &impl serde::Serialize) -> TestResponse {
        self.send(self.request(Method::PUT, uri).json(body).build()).await
    }

    /// `PATCH uri` with a JSON body.
    pub async fn patch(&self, uri: &str, body: &impl serde::Serialize) -> TestResponse {
        self.send(self.request(Method::PATCH, uri).json(body).build()).await
    }

    /// `POST uri` with no body — a form action, a publish, a logout.
    pub async fn post_empty(&self, uri: &str) -> TestResponse {
        self.send(self.request(Method::POST, uri).build()).await
    }

    /// A request builder carrying this harness's default headers.
    ///
    /// For the request the helpers above do not cover — a form encoding, an
    /// odd header, a deliberately malformed body.
    pub fn request(&self, method: Method, uri: &str) -> rainier_http::RequestBuilder {
        let mut builder = Request::builder().method(method).uri(uri);
        for (name, value) in &self.defaults {
            builder = builder.header(name.as_str(), value.as_str());
        }
        builder
    }

    /// Send a request you built yourself, through the whole kernel.
    ///
    /// The real pipeline: global middleware, routing, route middleware, the
    /// handler, and the exception renderer. A test that bypassed it would be
    /// asserting on something no client can reach.
    pub async fn send(&self, request: Request) -> TestResponse {
        TestResponse::of(self.kernel.handle_request(request).await).await
    }
}

/// One response, with its body already read.
///
/// The body is collected on construction so every assertion can be `&self` and
/// chain. A streaming body can only be read once, and a harness that made you
/// think about that would be a harness that made you think about the wrong
/// thing.
pub struct TestResponse {
    status: StatusCode,
    headers: Vec<(String, String)>,
    body: String,
}

impl TestResponse {
    /// Read `response` into an assertable one.
    pub async fn of(response: Response) -> Self {
        let status = response.status();
        let headers = response
            .headers()
            .iter()
            .filter_map(|(name, value)| {
                Some((name.as_str().to_string(), value.to_str().ok()?.to_string()))
            })
            .collect();

        let body = response.into_string().await.unwrap_or_default();

        Self { status, headers, body }
    }

    /// The status.
    pub fn status(&self) -> StatusCode {
        self.status
    }

    /// The body, as text.
    pub fn text(&self) -> &str {
        &self.body
    }

    /// A header's value.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header, _)| header.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// The body as JSON.
    ///
    /// # Panics
    ///
    /// If it will not parse — printing what arrived, because that is nearly
    /// always an error response the test did not expect.
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| {
            panic!(
                "the response body is not JSON: {e}\n  status: {}\n  body: {}",
                self.status, self.body
            )
        })
    }

    /// The body deserialised into your own type.
    ///
    /// # Panics
    ///
    /// If it will not deserialise, printing the body.
    pub fn json_as<T: DeserializeOwned>(&self) -> T {
        serde_json::from_str(&self.body).unwrap_or_else(|e| {
            panic!("the response body is not the expected shape: {e}\n  body: {}", self.body)
        })
    }

    // --- assertions --------------------------------------------------------
    //
    // Every one takes and returns `&Self`, so they chain and the response is
    // still there afterwards to read.

    /// Panic unless the status is exactly this.
    ///
    /// # Panics
    ///
    /// If it is not — printing the body, which is where the reason is.
    pub fn assert_status(&self, expected: StatusCode) -> &Self {
        assert_eq!(
            self.status, expected,
            "expected {expected}, got {}\n  body: {}",
            self.status, self.body
        );
        self
    }

    /// Panic unless the status is `2xx`.
    ///
    /// # Panics
    ///
    /// If it is not.
    pub fn assert_ok(&self) -> &Self {
        assert!(
            self.status.is_success(),
            "expected a 2xx, got {}\n  body: {}",
            self.status,
            self.body
        );
        self
    }

    /// Panic unless the status is `201`.
    ///
    /// # Panics
    ///
    /// If it is not.
    pub fn assert_created(&self) -> &Self {
        self.assert_status(StatusCode::CREATED)
    }

    /// Panic unless the status is `204`.
    ///
    /// # Panics
    ///
    /// If it is not.
    pub fn assert_no_content(&self) -> &Self {
        self.assert_status(StatusCode::NO_CONTENT)
    }

    /// Panic unless the status is `404`.
    ///
    /// # Panics
    ///
    /// If it is not.
    pub fn assert_not_found(&self) -> &Self {
        self.assert_status(StatusCode::NOT_FOUND)
    }

    /// Panic unless the status is `401`.
    ///
    /// # Panics
    ///
    /// If it is not.
    pub fn assert_unauthorized(&self) -> &Self {
        self.assert_status(StatusCode::UNAUTHORIZED)
    }

    /// Panic unless the status is `403`.
    ///
    /// # Panics
    ///
    /// If it is not.
    pub fn assert_forbidden(&self) -> &Self {
        self.assert_status(StatusCode::FORBIDDEN)
    }

    /// Panic unless the status is `422` — a failed request contract.
    ///
    /// # Panics
    ///
    /// If it is not.
    pub fn assert_invalid(&self) -> &Self {
        self.assert_status(StatusCode::UNPROCESSABLE_ENTITY)
    }

    /// Panic unless the JSON at `path` equals `expected`.
    ///
    /// `path` is dotted, and an index is a segment: `data.0.title`.
    ///
    /// # Panics
    ///
    /// If the path is absent or holds something else.
    pub fn assert_json_path(&self, path: &str, expected: impl Into<Value>) -> &Self {
        let expected = expected.into();
        let document = self.json();

        match json_path(&document, path) {
            Some(actual) => assert_eq!(
                *actual, expected,
                "at `{path}`: expected {expected}, got {actual}\n  body: {}",
                self.body
            ),
            None => panic!("`{path}` is not in the response\n  body: {}", self.body),
        }
        self
    }

    /// Panic unless there is nothing at `path`.
    ///
    /// For asserting a field was **not** serialised — a password hash, an
    /// internal id.
    ///
    /// # Panics
    ///
    /// If something is there.
    pub fn assert_json_missing(&self, path: &str) -> &Self {
        assert!(
            json_path(&self.json(), path).is_none(),
            "`{path}` should not be in the response\n  body: {}",
            self.body
        );
        self
    }

    /// Panic unless the body contains `text`.
    ///
    /// # Panics
    ///
    /// If it does not.
    pub fn assert_contains(&self, text: &str) -> &Self {
        assert!(
            self.body.contains(text),
            "the body does not contain {text:?}\n  body: {}",
            self.body
        );
        self
    }

    /// Panic unless the header is present with this value.
    ///
    /// # Panics
    ///
    /// If it is absent or different.
    pub fn assert_header(&self, name: &str, expected: &str) -> &Self {
        match self.header(name) {
            Some(actual) => assert_eq!(actual, expected, "header `{name}`"),
            None => panic!("`{name}` is not on the response — it has {:?}", self.header_names()),
        }
        self
    }

    /// Panic unless the header is absent.
    ///
    /// # Panics
    ///
    /// If it is present.
    pub fn assert_header_missing(&self, name: &str) -> &Self {
        assert!(self.header(name).is_none(), "`{name}` should not be on the response");
        self
    }

    fn header_names(&self) -> Vec<&str> {
        self.headers.iter().map(|(name, _)| name.as_str()).collect()
    }
}

/// Walk a dotted path into a JSON value. A numeric segment indexes an array.
fn json_path<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = value;

    for segment in path.split('.') {
        current = match current {
            Value::Array(items) => items.get(segment.parse::<usize>().ok()?)?,
            other => other.get(segment)?,
        };
    }
    Some(current)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn document() -> Value {
        json!({
            "database": { "status": "ok", "latency_ms": 3 },
            "data": [{ "title": "First" }, { "title": "Second" }],
            "nothing": null,
        })
    }

    #[test]
    fn a_dotted_path_walks_objects_and_arrays() {
        let value = document();

        assert_eq!(json_path(&value, "database.status"), Some(&json!("ok")));
        assert_eq!(json_path(&value, "data.1.title"), Some(&json!("Second")));
        assert_eq!(json_path(&value, "database.latency_ms"), Some(&json!(3)));
    }

    #[test]
    fn an_absent_path_is_none_rather_than_a_panic() {
        let value = document();

        assert_eq!(json_path(&value, "database.nope"), None);
        assert_eq!(json_path(&value, "data.9.title"), None);
        assert_eq!(json_path(&value, "data.title"), None, "an array indexed by a name");
    }

    #[test]
    fn a_null_is_present_and_not_missing() {
        // `"nothing": null` is a field the API sends. `assert_json_missing`
        // must not confuse it with a field the API omits.
        assert_eq!(json_path(&document(), "nothing"), Some(&json!(null)));
    }

    #[tokio::test]
    async fn a_response_is_read_once_and_asserted_many_times() {
        let response = TestResponse::of(
            Response::ok(r#"{"database":{"status":"ok"}}"#)
                .with_header("content-type", "application/json"),
        )
        .await;

        response
            .assert_ok()
            .assert_status(StatusCode::OK)
            .assert_json_path("database.status", "ok")
            .assert_json_missing("database.error")
            .assert_header("content-type", "application/json")
            .assert_contains("status");

        assert_eq!(response.json()["database"]["status"], "ok");
    }

    #[tokio::test]
    #[should_panic(expected = "expected 404 Not Found, got 200 OK")]
    async fn a_failed_status_assertion_prints_the_body() {
        TestResponse::of(Response::ok("hello")).await.assert_not_found();
    }

    #[tokio::test]
    #[should_panic(expected = "`database.status` is not in the response")]
    async fn a_missing_path_says_which_path() {
        TestResponse::of(Response::ok("{}")).await.assert_json_path("database.status", "ok");
    }
}
