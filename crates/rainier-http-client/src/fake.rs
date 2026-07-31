//! Recording outbound calls instead of making them — [`FakeTransport`].
//!
//! ```ignore
//! Http::fake().responding(200, r#"{"ok":true}"#);
//!
//! notify_application(&user).await?;
//!
//! Http::assert_sent(|request| request.url().ends_with("/hooks/user-updated"));
//! ```
//!
//! Follows the rule every other Rainier double follows: **it refuses to let an
//! assertion pass vacuously**. `assert_sent` against a real transport panics
//! rather than quietly passing, because the most dangerous test in a suite is
//! the one asserting something did *not* happen — forget the fake and it
//! passes forever, for the wrong reason, while the thing it guards breaks.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use rainier_support::{BoxFuture, Result};

use crate::transport::{OutboundRequest, RawResponse, Transport};

/// One request the fake was asked to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedRequest {
    inner: OutboundRequest,
}

impl RecordedRequest {
    /// The method.
    pub fn method(&self) -> &str {
        &self.inner.method
    }

    /// The full URL.
    pub fn url(&self) -> &str {
        &self.inner.url
    }

    /// A header's value.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.inner.header(name)
    }

    /// The body, as text.
    pub fn body(&self) -> String {
        self.inner.body_string()
    }

    /// The body, as JSON.
    pub fn json(&self) -> Option<serde_json::Value> {
        self.inner.json()
    }

    /// Whether the URL contains this.
    ///
    /// The assertion nine tests in ten want, spelled once.
    pub fn url_contains(&self, fragment: &str) -> bool {
        self.inner.url.contains(fragment)
    }
}

/// What the fake answers with.
#[derive(Debug, Clone)]
struct Canned {
    status: u16,
    body: Vec<u8>,
    headers: BTreeMap<String, String>,
}

/// A transport that records instead of sending.
#[derive(Default)]
pub struct FakeTransport {
    recorded: Mutex<Vec<RecordedRequest>>,
    /// Answers, in order. The last one repeats once they run out.
    responses: Mutex<Vec<Canned>>,
}

impl FakeTransport {
    /// A fake answering `200` with an empty body.
    pub fn new() -> Self {
        Self::default()
    }

    /// Answer the next call with this.
    ///
    /// Queue several to answer a sequence differently — which is how a retry
    /// test says "fail, fail, then work".
    pub fn responding(&self, status: u16, body: impl Into<Vec<u8>>) -> &Self {
        self.responses.lock().expect("fake lock poisoned").push(Canned {
            status,
            body: body.into(),
            headers: BTreeMap::new(),
        });
        self
    }

    /// Answer with a header as well.
    pub fn responding_with_header(
        &self,
        status: u16,
        body: impl Into<Vec<u8>>,
        name: impl Into<String>,
        value: impl Into<String>,
    ) -> &Self {
        self.responses.lock().expect("fake lock poisoned").push(Canned {
            status,
            body: body.into(),
            headers: [(name.into().to_ascii_lowercase(), value.into())].into_iter().collect(),
        });
        self
    }

    /// Everything that was sent, in order.
    pub fn recorded(&self) -> Vec<RecordedRequest> {
        self.recorded.lock().expect("fake lock poisoned").clone()
    }

    /// How many calls were made.
    pub fn count(&self) -> usize {
        self.recorded.lock().expect("fake lock poisoned").len()
    }

    /// Forget everything recorded, keeping the queued answers.
    pub fn clear(&self) {
        self.recorded.lock().expect("fake lock poisoned").clear();
    }

    /// Panic unless a request matching `matches` was sent.
    ///
    /// # Panics
    ///
    /// If none matched — listing what *was* sent, because "no matching
    /// request" with no list is the least useful assertion failure there is.
    pub fn assert_sent(&self, matches: impl Fn(&RecordedRequest) -> bool) {
        let recorded = self.recorded();

        assert!(
            recorded.iter().any(&matches),
            "no outbound request matched. {} were sent:\n{}",
            recorded.len(),
            describe(&recorded)
        );
    }

    /// Panic if any request matching `matches` was sent.
    ///
    /// # Panics
    ///
    /// If one did, naming it.
    pub fn assert_not_sent(&self, matches: impl Fn(&RecordedRequest) -> bool) {
        if let Some(sent) = self.recorded().iter().find(|request| matches(request)) {
            panic!(
                "an outbound request matched when none should have: {} {}",
                sent.method(),
                sent.url()
            );
        }
    }

    /// Panic unless exactly `count` requests were sent.
    ///
    /// # Panics
    ///
    /// If a different number were, listing them.
    pub fn assert_sent_count(&self, count: usize) {
        let recorded = self.recorded();

        assert_eq!(
            recorded.len(),
            count,
            "expected {count} outbound request(s), got {}:\n{}",
            recorded.len(),
            describe(&recorded)
        );
    }

    /// Panic unless nothing was sent.
    ///
    /// # Panics
    ///
    /// If anything was.
    pub fn assert_nothing_sent(&self) {
        self.assert_sent_count(0);
    }
}

/// A readable list of what was sent, for an assertion message.
fn describe(recorded: &[RecordedRequest]) -> String {
    if recorded.is_empty() {
        return "  (nothing)".to_string();
    }

    recorded
        .iter()
        .map(|request| format!("  {} {}", request.method(), request.url()))
        .collect::<Vec<_>>()
        .join("\n")
}

impl Transport for FakeTransport {
    fn send<'a>(&'a self, request: OutboundRequest) -> BoxFuture<'a, Result<RawResponse>> {
        Box::pin(async move {
            self.recorded
                .lock()
                .expect("fake lock poisoned")
                .push(RecordedRequest { inner: request });

            let mut responses = self.responses.lock().expect("fake lock poisoned");

            // Queued answers are consumed in order; the last one repeats. So a
            // test that queues one answer gets it for every call, and one that
            // queues three describes a sequence.
            let canned = if responses.len() > 1 {
                responses.remove(0)
            } else {
                responses.first().cloned().unwrap_or(Canned {
                    status: 200,
                    body: Vec::new(),
                    headers: BTreeMap::new(),
                })
            };

            Ok(RawResponse { status: canned.status, headers: canned.headers, body: canned.body })
        })
    }

    fn name(&self) -> &str {
        "fake"
    }
}

impl std::fmt::Debug for FakeTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakeTransport").field("recorded", &self.count()).finish()
    }
}

/// A fake, shared.
pub type SharedFake = Arc<FakeTransport>;

#[cfg(test)]
mod tests {
    use super::*;

    fn request(url: &str) -> OutboundRequest {
        OutboundRequest {
            method: "POST".into(),
            url: url.into(),
            headers: [("x-signature".to_string(), "abc".to_string())].into_iter().collect(),
            body: Some(br#"{"id":42}"#.to_vec()),
            timeout: None,
        }
    }

    #[tokio::test]
    async fn it_records_instead_of_sending() {
        let fake = FakeTransport::new();

        fake.send(request("https://hooks.example.com/user-updated")).await.unwrap();

        assert_eq!(fake.count(), 1);
        assert_eq!(fake.recorded()[0].url(), "https://hooks.example.com/user-updated");
        assert_eq!(fake.recorded()[0].header("x-signature"), Some("abc"));
        assert_eq!(fake.recorded()[0].json().unwrap()["id"], 42);
    }

    #[tokio::test]
    async fn it_answers_200_by_default() {
        let fake = FakeTransport::new();

        let response = fake.send(request("https://x")).await.unwrap();

        assert_eq!(response.status, 200);
        assert!(response.body.is_empty());
    }

    #[tokio::test]
    async fn one_queued_answer_repeats() {
        let fake = FakeTransport::new();
        fake.responding(201, "created");

        for _ in 0..3 {
            assert_eq!(fake.send(request("https://x")).await.unwrap().status, 201);
        }
    }

    #[tokio::test]
    async fn several_queued_answers_describe_a_sequence() {
        // Which is how a retry test says "fail, fail, then work".
        let fake = FakeTransport::new();
        fake.responding(500, "no").responding(500, "no").responding(200, "yes");

        let statuses: Vec<u16> = {
            let mut seen = Vec::new();
            for _ in 0..4 {
                seen.push(fake.send(request("https://x")).await.unwrap().status);
            }
            seen
        };

        assert_eq!(statuses, vec![500, 500, 200, 200], "the last answer repeats");
    }

    #[tokio::test]
    async fn assert_sent_finds_a_matching_request() {
        let fake = FakeTransport::new();
        fake.send(request("https://hooks.example.com/user-updated")).await.unwrap();

        fake.assert_sent(|request| request.url_contains("/user-updated"));
        fake.assert_sent(|request| request.header("x-signature").is_some());
    }

    #[tokio::test]
    #[should_panic(expected = "no outbound request matched")]
    async fn assert_sent_lists_what_was_sent_when_nothing_matches() {
        let fake = FakeTransport::new();
        fake.send(request("https://hooks.example.com/other")).await.unwrap();

        fake.assert_sent(|request| request.url_contains("/user-updated"));
    }

    #[tokio::test]
    async fn assert_not_sent_is_the_inverse() {
        let fake = FakeTransport::new();
        fake.send(request("https://hooks.example.com/other")).await.unwrap();

        fake.assert_not_sent(|request| request.url_contains("/user-updated"));
    }

    #[tokio::test]
    #[should_panic(expected = "matched when none should have")]
    async fn assert_not_sent_names_the_one_it_found() {
        let fake = FakeTransport::new();
        fake.send(request("https://hooks.example.com/user-updated")).await.unwrap();

        fake.assert_not_sent(|request| request.url_contains("/user-updated"));
    }

    #[tokio::test]
    async fn counting_and_clearing() {
        let fake = FakeTransport::new();
        fake.assert_nothing_sent();

        fake.send(request("https://x")).await.unwrap();
        fake.send(request("https://y")).await.unwrap();
        fake.assert_sent_count(2);

        fake.clear();
        fake.assert_nothing_sent();
    }

    #[tokio::test]
    #[should_panic(expected = "expected 1 outbound request(s), got 2")]
    async fn a_wrong_count_says_both_numbers() {
        let fake = FakeTransport::new();
        fake.send(request("https://x")).await.unwrap();
        fake.send(request("https://y")).await.unwrap();

        fake.assert_sent_count(1);
    }
}
