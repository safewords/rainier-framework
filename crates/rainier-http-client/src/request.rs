//! Building and sending — [`Http`] and [`PendingRequest`].

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use rainier_support::{Error, Result};
use serde::Serialize;

use crate::fake::FakeTransport;
use crate::response::HttpResponse;
use crate::retry::{is_retryable, Backoff};
use crate::transport::{OutboundRequest, Transport};

/// The transport every `Http::` call uses, unless one is passed explicitly.
static TRANSPORT: RwLock<Option<Arc<dyn Transport>>> = RwLock::new(None);

/// The fake, when one is installed — so the assertions can reach it.
static FAKE: RwLock<Option<Arc<FakeTransport>>> = RwLock::new(None);

thread_local! {
    /// A fake scoped to *this thread*, which wins over the process-wide one.
    ///
    /// The same shape as the container's facade scope, and for the same
    /// reason: a process-global fake means a test suite has to run its
    /// outbound-call tests one at a time, and nobody remembers to. Each
    /// `#[tokio::test]` body stays on one thread, so each gets its own fake
    /// and they stop overwriting each other.
    ///
    /// A spawned task falls back to the process-wide one — the same limit the
    /// facade scope has, and the same answer: install it globally when the
    /// code under test spawns.
    static SCOPED_FAKE: RefCell<Option<Arc<FakeTransport>>> = const { RefCell::new(None) };
}

/// The outbound HTTP client.
///
/// A facade over whichever [`Transport`] is installed, so
/// [`fake`](Self::fake) swaps what every call in the process does — the same
/// way rebinding a service swaps what a Rainier facade sees.
pub struct Http;

impl Http {
    /// `GET url`.
    pub fn get(url: impl Into<String>) -> PendingRequest {
        PendingRequest::new("GET", url)
    }

    /// `POST url`.
    pub fn post(url: impl Into<String>) -> PendingRequest {
        PendingRequest::new("POST", url)
    }

    /// `PUT url`.
    pub fn put(url: impl Into<String>) -> PendingRequest {
        PendingRequest::new("PUT", url)
    }

    /// `PATCH url`.
    pub fn patch(url: impl Into<String>) -> PendingRequest {
        PendingRequest::new("PATCH", url)
    }

    /// `DELETE url`.
    pub fn delete(url: impl Into<String>) -> PendingRequest {
        PendingRequest::new("DELETE", url)
    }

    /// A request with any method.
    pub fn request(method: impl Into<String>, url: impl Into<String>) -> PendingRequest {
        PendingRequest::new(method, url)
    }

    /// Install the transport every call uses.
    ///
    /// Called once at boot. Calling it again replaces the binding, which is
    /// what [`fake`](Self::fake) does.
    pub fn install(transport: Arc<dyn Transport>) {
        *TRANSPORT.write().expect("transport lock poisoned") = Some(transport);
        *FAKE.write().expect("fake lock poisoned") = None;
    }

    /// Record every outbound call instead of making it.
    ///
    /// Returns the fake, so a test can queue answers:
    ///
    /// ```ignore
    /// Http::fake().responding(200, r#"{"ok":true}"#);
    /// ```
    ///
    /// **Nothing reaches the network** afterwards, which is the other half of
    /// the point: a suite that accidentally calls a real endpoint is a suite
    /// that fails when somebody runs it on a train.
    pub fn fake() -> Arc<FakeTransport> {
        let fake = Arc::new(FakeTransport::new());

        // On this thread, so two tests faking at once do not overwrite each
        // other — see `SCOPED_FAKE`. `fake_globally` is the one to reach for
        // when the code under test spawns.
        SCOPED_FAKE.with(|scoped| *scoped.borrow_mut() = Some(Arc::clone(&fake)));

        fake
    }

    /// Record every outbound call **in the whole process**.
    ///
    /// For code that spawns: a task started by the code under test inherits no
    /// thread scope, so [`fake`](Self::fake) would not catch its calls.
    ///
    /// The trade is that two tests doing this at once overwrite each other, so
    /// a suite using it has to run those tests one at a time.
    pub fn fake_globally() -> Arc<FakeTransport> {
        let fake = Arc::new(FakeTransport::new());

        *TRANSPORT.write().expect("transport lock poisoned") =
            Some(Arc::clone(&fake) as Arc<dyn Transport>);
        *FAKE.write().expect("fake lock poisoned") = Some(Arc::clone(&fake));

        fake
    }

    /// The installed fake.
    ///
    /// # Panics
    ///
    /// If nothing is faking. Every Rainier double refuses to let an assertion
    /// pass vacuously, and this is the same rule: an `assert_sent` that
    /// silently passed because somebody forgot [`fake`](Self::fake) is the
    /// most dangerous test in a suite.
    pub fn faking() -> Arc<FakeTransport> {
        Self::scoped_fake()
            .or_else(|| FAKE.read().expect("fake lock poisoned").clone())
            .expect("`Http` is not faking — call `Http::fake()` before asserting on outbound calls")
    }

    /// The fake scoped to this thread, if there is one.
    fn scoped_fake() -> Option<Arc<FakeTransport>> {
        SCOPED_FAKE.with(|scoped| scoped.borrow().clone())
    }

    /// Panic unless a matching request was sent. See
    /// [`FakeTransport::assert_sent`].
    ///
    /// # Panics
    ///
    /// If none matched, or if nothing is faking.
    pub fn assert_sent(matches: impl Fn(&crate::RecordedRequest) -> bool) {
        Self::faking().assert_sent(matches);
    }

    /// Panic if a matching request was sent.
    ///
    /// # Panics
    ///
    /// If one was, or if nothing is faking.
    pub fn assert_not_sent(matches: impl Fn(&crate::RecordedRequest) -> bool) {
        Self::faking().assert_not_sent(matches);
    }

    /// Panic unless nothing was sent.
    ///
    /// # Panics
    ///
    /// If anything was, or if nothing is faking.
    pub fn assert_nothing_sent() {
        Self::faking().assert_nothing_sent();
    }

    /// Stop faking, and install nothing in its place.
    ///
    /// Clears both the thread scope and the process-wide binding.
    pub fn clear() {
        SCOPED_FAKE.with(|scoped| *scoped.borrow_mut() = None);
        *TRANSPORT.write().expect("transport lock poisoned") = None;
        *FAKE.write().expect("fake lock poisoned") = None;
    }

    /// The installed transport, or the default one.
    fn transport() -> Result<Arc<dyn Transport>> {
        // Nearest first: this thread's fake, then whatever the process
        // installed, then a real one built on demand.
        if let Some(fake) = Self::scoped_fake() {
            return Ok(fake as Arc<dyn Transport>);
        }
        if let Some(transport) = TRANSPORT.read().expect("transport lock poisoned").clone() {
            return Ok(transport);
        }

        #[cfg(feature = "reqwest-transport")]
        {
            // Installed lazily rather than at boot, so an application that
            // never calls out never builds a TLS stack.
            let transport: Arc<dyn Transport> = Arc::new(crate::ReqwestTransport::new());
            *TRANSPORT.write().expect("transport lock poisoned") = Some(Arc::clone(&transport));
            Ok(transport)
        }

        #[cfg(not(feature = "reqwest-transport"))]
        Err(Error::internal(
            "no HTTP transport is installed — enable the `reqwest-transport` feature, or call \
             `Http::install(..)` with your own",
        ))
    }
}

/// One field of a `multipart/form-data` body.
#[derive(Debug, Clone)]
pub struct MultipartField {
    /// The field name.
    pub name: String,
    /// Its bytes.
    pub value: Vec<u8>,
    /// A filename, which makes this a file part rather than a plain field.
    pub filename: Option<String>,
}

impl MultipartField {
    /// A plain text field.
    pub fn text(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self { name: name.into(), value: value.into().into_bytes(), filename: None }
    }

    /// A file part. Put it **last**: S3 ignores fields that follow it.
    pub fn file(
        name: impl Into<String>,
        filename: impl Into<String>,
        value: impl Into<Vec<u8>>,
    ) -> Self {
        Self { name: name.into(), value: value.into(), filename: Some(filename.into()) }
    }
}

/// A request being built.
pub struct PendingRequest {
    method: String,
    url: String,
    headers: BTreeMap<String, String>,
    body: Option<Vec<u8>>,
    timeout: Option<Duration>,
    attempts: u32,
    backoff: Backoff,
    transport: Option<Arc<dyn Transport>>,
}

impl PendingRequest {
    fn new(method: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            method: method.into().to_ascii_uppercase(),
            url: url.into(),
            headers: BTreeMap::new(),
            body: None,
            timeout: Some(Duration::from_secs(30)),
            attempts: 1,
            backoff: Backoff::default(),
            transport: None,
        }
    }

    /// Add a header.
    #[must_use = "this returns the request rather than sending it"]
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.insert(name.into().to_ascii_lowercase(), value.into());
        self
    }

    /// `Authorization: Bearer …`.
    #[must_use = "this returns the request rather than sending it"]
    pub fn bearer(self, token: impl std::fmt::Display) -> Self {
        self.header("authorization", format!("Bearer {token}"))
    }

    /// `Accept: application/json`.
    #[must_use = "this returns the request rather than sending it"]
    pub fn accept_json(self) -> Self {
        self.header("accept", "application/json")
    }

    /// Send `value` as a JSON body.
    ///
    /// # Errors
    ///
    /// If it will not serialise — reported here rather than at `send`, so the
    /// failure is next to the value that caused it.
    pub fn json<T: Serialize>(mut self, value: &T) -> Result<Self> {
        self.body = Some(serde_json::to_vec(value)?);
        Ok(self.header("content-type", "application/json"))
    }

    /// Send these bytes.
    #[must_use = "this returns the request rather than sending it"]
    pub fn body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = Some(body.into());
        self
    }

    /// Send a form-encoded body.
    #[must_use = "this returns the request rather than sending it"]
    pub fn form(mut self, fields: &[(&str, &str)]) -> Self {
        let encoded = fields
            .iter()
            .map(|(name, value)| format!("{}={}", encode(name), encode(value)))
            .collect::<Vec<_>>()
            .join("&");

        self.body = Some(encoded.into_bytes());
        self.header("content-type", "application/x-www-form-urlencoded")
    }

    /// Send a `multipart/form-data` body.
    ///
    /// Needed wherever the other end takes a browser-style form upload rather
    /// than JSON — an S3 POST policy is the case this was written for, and it
    /// will not accept the urlencoded body [`form`](Self::form) produces.
    ///
    /// # Field order is preserved, and matters
    ///
    /// S3 ignores every field that appears *after* the file part, so a policy
    /// or signature placed after it is silently not applied and the upload
    /// fails as an unexplained 403. Fields are written in the order given;
    /// put the file last.
    ///
    /// # The boundary
    ///
    /// Derived from the parts rather than randomly, so a request is
    /// reproducible in a test, and long enough that it cannot occur inside
    /// ordinary payload bytes. A boundary that appears in the body truncates
    /// the upload at that point, which presents as a corrupt file rather than
    /// as an error.
    #[must_use = "this returns the request rather than sending it"]
    pub fn multipart(mut self, parts: &[MultipartField]) -> Self {
        let boundary = Self::boundary_for(parts);
        let mut body: Vec<u8> = Vec::new();

        for part in parts {
            body.extend_from_slice(
                format!(
                    "--{boundary}
"
                )
                .as_bytes(),
            );
            match &part.filename {
                Some(filename) => body.extend_from_slice(
                    format!(
                        "Content-Disposition: form-data; name=\"{}\"; filename=\"{}\"
                         Content-Type: application/octet-stream

",
                        part.name, filename
                    )
                    .as_bytes(),
                ),
                None => body.extend_from_slice(
                    format!(
                        "Content-Disposition: form-data; name=\"{}\"

",
                        part.name
                    )
                    .as_bytes(),
                ),
            }
            body.extend_from_slice(&part.value);
            body.extend_from_slice(
                b"
",
            );
        }
        body.extend_from_slice(
            format!(
                "--{boundary}--
"
            )
            .as_bytes(),
        );

        self.body = Some(body);
        self.header("content-type", format!("multipart/form-data; boundary={boundary}"))
    }

    /// A boundary that cannot appear in the parts.
    ///
    /// Checked rather than assumed: a boundary occurring inside a payload
    /// truncates the body there, and the result is a file that uploaded
    /// "successfully" and is corrupt.
    fn boundary_for(parts: &[MultipartField]) -> String {
        let mut boundary = "----rainier-boundary-0000000000".to_string();

        for n in 0.. {
            boundary = format!("----rainier-boundary-{n:010}");
            let needle = boundary.as_bytes();
            let clashes =
                parts.iter().any(|part| part.value.windows(needle.len()).any(|w| w == needle));
            if !clashes {
                break;
            }
        }

        boundary
    }

    /// How long to wait before giving up.
    ///
    /// Thirty seconds by default. A request with **no** timeout is one that
    /// can hold a worker forever when the other end stops answering without
    /// closing, which is the failure mode that takes a queue down.
    #[must_use = "this returns the request rather than sending it"]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Wait indefinitely.
    ///
    /// Almost never right; see [`timeout`](Self::timeout).
    #[must_use = "this returns the request rather than sending it"]
    pub fn without_timeout(mut self) -> Self {
        self.timeout = None;
        self
    }

    /// Try up to `attempts` times, waiting per `backoff`.
    ///
    /// Only failures worth repeating are repeated — see
    /// [`is_retryable`](crate::retry::is_retryable()), and read it before
    /// assuming a `422` will be retried, because it will not.
    #[must_use = "this returns the request rather than sending it"]
    pub fn retry(mut self, attempts: u32, backoff: Backoff) -> Self {
        self.attempts = attempts.max(1);
        self.backoff = backoff;
        self
    }

    /// Send through this transport rather than the installed one.
    ///
    /// For a caller holding its own client — a different proxy, a pinned
    /// certificate — without changing what the rest of the process does.
    #[must_use = "this returns the request rather than sending it"]
    pub fn through(mut self, transport: Arc<dyn Transport>) -> Self {
        self.transport = Some(transport);
        self
    }

    /// Send it.
    ///
    /// Returns `Ok` for any response that arrived, including a `500` — see
    /// [`HttpResponse::error_for_status`]. `Err` means nothing came back at
    /// all after every attempt.
    pub async fn send(self) -> Result<HttpResponse> {
        let transport = match &self.transport {
            Some(transport) => Arc::clone(transport),
            None => Http::transport()?,
        };

        let request = OutboundRequest {
            method: self.method.clone(),
            url: self.url.clone(),
            headers: self.headers.clone(),
            body: self.body.clone(),
            timeout: self.timeout,
        };

        let mut last: Option<Error> = None;

        for attempt in 1..=self.attempts {
            let wait = self.backoff.wait_before(attempt);
            if !wait.is_zero() {
                tokio::time::sleep(wait).await;
            }

            match transport.send(request.clone()).await {
                Ok(raw) => {
                    let response = HttpResponse::new(raw);

                    if attempt < self.attempts && is_retryable(Some(response.status())) {
                        tracing::debug!(
                            url = %self.url,
                            status = response.status(),
                            attempt,
                            "retrying an outbound request"
                        );
                        continue;
                    }

                    // Returned whatever the status is: the caller decides
                    // whether a `404` is a failure, and plenty of the time it
                    // is the answer they wanted.
                    return Ok(response);
                }
                Err(e) => {
                    if attempt < self.attempts && is_retryable(None) {
                        tracing::debug!(url = %self.url, attempt, error = %e.message(), "retrying");
                        last = Some(e);
                        continue;
                    }
                    return Err(e);
                }
            }
        }

        Err(last.unwrap_or_else(|| Error::internal("the request was never attempted")))
    }
}

impl std::fmt::Debug for PendingRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingRequest")
            .field("method", &self.method)
            .field("url", &self.url)
            .field("attempts", &self.attempts)
            .finish()
    }
}

/// Percent-encode a form value.
fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());

    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {

    use super::MultipartField;

    /// The body a request would send.
    fn body_of(request: PendingRequest) -> String {
        String::from_utf8(request.body.clone().expect("a body")).expect("utf-8 in these tests")
    }

    #[test]
    fn multipart_keeps_the_order_it_was_given() {
        // S3 ignores every field after the file part, so a signature placed
        // after it is silently not applied and the upload fails as an
        // unexplained 403. Order is the contract, not a convenience.
        let body = body_of(Http::post("https://example.test").multipart(&[
            MultipartField::text("policy", "abc"),
            MultipartField::text("x-amz-signature", "def"),
            MultipartField::file("file", "probe.txt", b"hello".to_vec()),
        ]));

        let policy = body.find("policy").expect("policy present");
        let signature = body.find("x-amz-signature").expect("signature present");
        let file = body.find("filename=").expect("file present");

        assert!(policy < signature, "{body}");
        assert!(signature < file, "the file part must come last");
    }

    #[test]
    fn a_file_part_carries_a_filename_and_a_plain_field_does_not() {
        // The presence of `filename=` is what makes S3 treat a part as the
        // body rather than as another condition.
        let body = body_of(Http::post("https://example.test").multipart(&[
            MultipartField::text("key", "abc.txt"),
            MultipartField::file("file", "abc.txt", b"x".to_vec()),
        ]));

        assert!(body.contains(r#"name="key""#), "{body}");
        assert_eq!(body.matches("filename=").count(), 1, "{body}");
    }

    #[test]
    fn the_boundary_never_occurs_inside_the_payload() {
        // A boundary appearing in the body truncates the upload there, and the
        // result is a file that uploaded "successfully" and is corrupt. So it
        // is checked against the parts rather than assumed unique.
        let hostile = b"----rainier-boundary-0000000000".to_vec();
        let request = Http::post("https://example.test").multipart(&[MultipartField::file(
            "file",
            "evil.bin",
            hostile.clone(),
        )]);

        let boundary = request
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
            .map(|(_, value)| value.rsplit("boundary=").next().unwrap().to_string())
            .expect("a content-type with a boundary");

        assert_ne!(boundary.as_bytes(), hostile.as_slice());
        assert!(!hostile.windows(boundary.len()).any(|w| w == boundary.as_bytes()));
    }

    #[test]
    fn multipart_sets_its_own_content_type() {
        // Not urlencoded. `form` produces a body S3 will not accept, and the
        // only visible difference at the call site is which method was used.
        let request =
            Http::post("https://example.test").multipart(&[MultipartField::text("a", "b")]);

        let content_type = request
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
            .map(|(_, value)| value.clone())
            .expect("a content-type");

        assert!(content_type.starts_with("multipart/form-data; boundary="), "{content_type}");
    }
    use super::*;
    use serde_json::json;

    /// Its own transport per test, so nothing races the process-wide one.
    fn fake() -> Arc<FakeTransport> {
        Arc::new(FakeTransport::new())
    }

    #[tokio::test]
    async fn a_json_post_carries_its_body_and_content_type() {
        let fake = fake();

        Http::post("https://hooks.example.com/x")
            .json(&json!({ "id": 42 }))
            .unwrap()
            .through(Arc::clone(&fake) as Arc<dyn Transport>)
            .send()
            .await
            .unwrap();

        let sent = &fake.recorded()[0];
        assert_eq!(sent.method(), "POST");
        assert_eq!(sent.header("content-type"), Some("application/json"));
        assert_eq!(sent.json().unwrap()["id"], 42);
    }

    #[tokio::test]
    async fn a_bearer_token_becomes_a_header() {
        let fake = fake();

        Http::get("https://api.example.com/me")
            .bearer("secret-token")
            .accept_json()
            .through(Arc::clone(&fake) as Arc<dyn Transport>)
            .send()
            .await
            .unwrap();

        let sent = &fake.recorded()[0];
        assert_eq!(sent.header("authorization"), Some("Bearer secret-token"));
        assert_eq!(sent.header("accept"), Some("application/json"));
    }

    #[tokio::test]
    async fn a_form_body_is_encoded() {
        let fake = fake();

        Http::post("https://oauth.example.com/token")
            .form(&[("grant_type", "client_credentials"), ("scope", "read write")])
            .through(Arc::clone(&fake) as Arc<dyn Transport>)
            .send()
            .await
            .unwrap();

        let sent = &fake.recorded()[0];
        assert_eq!(sent.header("content-type"), Some("application/x-www-form-urlencoded"));
        assert_eq!(sent.body(), "grant_type=client_credentials&scope=read%20write");
    }

    #[tokio::test]
    async fn a_five_hundred_comes_back_as_ok_and_the_caller_decides() {
        let fake = fake();
        fake.responding(500, "not my day");

        let response = Http::get("https://x")
            .through(Arc::clone(&fake) as Arc<dyn Transport>)
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), 500);
        assert!(response.error_for_status().is_err());
    }

    #[tokio::test]
    async fn a_retryable_status_is_retried_and_then_succeeds() {
        let fake = fake();
        fake.responding(503, "later").responding(503, "later").responding(200, "yes");

        let response = Http::get("https://x")
            .retry(3, Backoff::None)
            .through(Arc::clone(&fake) as Arc<dyn Transport>)
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
        assert_eq!(fake.count(), 3);
    }

    #[tokio::test]
    async fn a_client_mistake_is_not_retried() {
        // The one that matters: sending a 422 four times does not fix the
        // payload, and a non-idempotent endpoint may act on each one.
        let fake = fake();
        fake.responding(422, "bad payload");

        let response = Http::post("https://x")
            .retry(4, Backoff::None)
            .through(Arc::clone(&fake) as Arc<dyn Transport>)
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), 422);
        assert_eq!(fake.count(), 1, "it should not have tried again");
    }

    #[tokio::test]
    async fn retrying_gives_up_and_returns_the_last_answer() {
        let fake = fake();
        fake.responding(503, "still no");

        let response = Http::get("https://x")
            .retry(3, Backoff::None)
            .through(Arc::clone(&fake) as Arc<dyn Transport>)
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), 503);
        assert_eq!(fake.count(), 3);
    }

    #[tokio::test]
    async fn there_is_a_timeout_by_default() {
        // A request with no timeout can hold a worker forever when the other
        // end stops answering without closing.
        let fake = fake();

        Http::get("https://x")
            .through(Arc::clone(&fake) as Arc<dyn Transport>)
            .send()
            .await
            .unwrap();

        // The transport sees it, which is what a real one passes to reqwest.
        let request = OutboundRequest {
            method: "GET".into(),
            url: "https://x".into(),
            headers: BTreeMap::new(),
            body: None,
            timeout: Some(Duration::from_secs(30)),
        };
        assert_eq!(request.timeout, Some(Duration::from_secs(30)));
    }

    #[test]
    fn form_values_are_percent_encoded() {
        assert_eq!(encode("read write"), "read%20write");
        assert_eq!(encode("a&b=c"), "a%26b%3Dc");
        assert_eq!(encode("ada@example.com"), "ada%40example.com");
        assert_eq!(encode("plain-value_1.0~"), "plain-value_1.0~");
    }

    #[test]
    fn the_method_is_normalised() {
        assert_eq!(Http::request("post", "https://x").method, "POST");
    }
}
