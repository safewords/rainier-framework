//! What came back — [`HttpResponse`].

use std::collections::BTreeMap;

use rainier_support::{Error, Result};
use serde::de::DeserializeOwned;

use crate::transport::RawResponse;

/// One response, with its body already read.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    status: u16,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

impl HttpResponse {
    /// Wrap what a transport returned.
    pub fn new(raw: RawResponse) -> Self {
        Self { status: raw.status, headers: raw.headers, body: raw.body }
    }

    /// Build one directly. For a fake, and for a test.
    pub fn with(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self { status, headers: BTreeMap::new(), body: body.into() }
    }

    /// Add a header.
    #[must_use = "this returns the response with the header added"]
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.insert(name.into().to_ascii_lowercase(), value.into());
        self
    }

    /// The status.
    pub fn status(&self) -> u16 {
        self.status
    }

    /// Whether the status is `2xx`.
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// Whether the status is `4xx`.
    pub fn is_client_error(&self) -> bool {
        (400..500).contains(&self.status)
    }

    /// Whether the status is `5xx`.
    pub fn is_server_error(&self) -> bool {
        self.status >= 500
    }

    /// A header's value, however it was capitalised.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(&name.to_ascii_lowercase()).map(String::as_str)
    }

    /// Every header.
    pub fn headers(&self) -> &BTreeMap<String, String> {
        &self.headers
    }

    /// The body, as bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.body
    }

    /// The body, as text.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// The body, deserialised.
    ///
    /// # Errors
    ///
    /// Quoting the start of the body, because a parse failure here is almost
    /// always an error page or a rate-limit notice where JSON was expected,
    /// and `expected value at line 1 column 1` says nothing about which.
    pub fn json<T: DeserializeOwned>(&self) -> Result<T> {
        serde_json::from_slice(&self.body).map_err(|e| {
            let preview: String = self.text().chars().take(400).collect();
            Error::internal(format!(
                "the response body is not the expected JSON: {e}\n  status: {}\n  body: {preview}",
                self.status
            ))
        })
    }

    /// Turn a `4xx` or `5xx` into an error.
    ///
    /// Not automatic, and deliberately: plenty of callers want to look at a
    /// `404` rather than propagate it, and a client that made that decision
    /// for them would have them parsing the error back apart.
    ///
    /// The message carries the status and the start of the body, which is
    /// where the other end put the reason.
    pub fn error_for_status(self) -> Result<Self> {
        if self.is_success() {
            return Ok(self);
        }

        let preview: String = self.text().chars().take(400).collect();
        let message = format!("the request failed with {}: {preview}", self.status);

        Err(if self.is_server_error() {
            Error::service_unavailable(message)
        } else {
            Error::bad_request(message)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[test]
    fn a_status_reads_as_the_class_it_is_in() {
        assert!(HttpResponse::with(200, "").is_success());
        assert!(HttpResponse::with(204, "").is_success());
        assert!(HttpResponse::with(404, "").is_client_error());
        assert!(HttpResponse::with(503, "").is_server_error());

        assert!(!HttpResponse::with(302, "").is_success());
        assert!(!HttpResponse::with(500, "").is_client_error());
    }

    #[test]
    fn headers_are_found_whatever_case_they_are_asked_for_in() {
        let response = HttpResponse::with(200, "").with_header("Content-Type", "application/json");

        assert_eq!(response.header("content-type"), Some("application/json"));
        assert_eq!(response.header("CONTENT-TYPE"), Some("application/json"));
    }

    #[test]
    fn a_json_body_deserialises() {
        #[derive(Debug, Deserialize, PartialEq)]
        struct Body {
            id: u64,
        }

        let response = HttpResponse::with(200, r#"{"id":42}"#);

        assert_eq!(response.json::<Body>().unwrap(), Body { id: 42 });
    }

    #[test]
    fn a_body_that_is_not_json_says_what_it_was() {
        // The failure this message exists for: an HTML error page where JSON
        // was expected, which `expected value at line 1 column 1` describes
        // uselessly.
        let response = HttpResponse::with(502, "<html><body>Bad Gateway</body></html>");

        let error = response.json::<serde_json::Value>().unwrap_err();

        assert!(error.message().contains("502"), "{}", error.message());
        assert!(error.message().contains("Bad Gateway"), "{}", error.message());
    }

    #[test]
    fn error_for_status_leaves_a_success_alone() {
        let response = HttpResponse::with(200, "fine").error_for_status().unwrap();

        assert_eq!(response.text(), "fine");
    }

    #[test]
    fn error_for_status_distinguishes_whose_fault_it_was() {
        // A 4xx is this caller's problem to fix; a 5xx is worth retrying.
        // Rendering both as the same kind loses that.
        let ours = HttpResponse::with(422, "bad payload").error_for_status().unwrap_err();
        let theirs = HttpResponse::with(503, "try later").error_for_status().unwrap_err();

        assert_eq!(ours.status(), 400);
        assert_eq!(theirs.status(), 503);
    }

    #[test]
    fn the_error_carries_what_the_other_end_said() {
        let error = HttpResponse::with(400, r#"{"error":"missing field: email"}"#)
            .error_for_status()
            .unwrap_err();

        assert!(error.message().contains("missing field: email"), "{}", error.message());
    }

    #[test]
    fn a_very_long_body_is_truncated_in_the_message() {
        // An error message is read in a log line, and a megabyte of HTML in
        // one is a log nobody can read.
        let error = HttpResponse::with(500, "x".repeat(100_000)).error_for_status().unwrap_err();

        assert!(error.message().len() < 600, "{} characters", error.message().len());
    }
}
