//! The bytes-on-the-wire half — [`Transport`].
//!
//! The same split the D1 executor and the KV cache use: the client owns the
//! ergonomics, the retries and the recording, and a transport owns the socket.
//! Keeping them apart is what lets [`FakeTransport`](crate::FakeTransport)
//! exist without pretending to be a web server.

use std::collections::BTreeMap;
use std::time::Duration;

use rainier_support::{BoxFuture, Result};

/// One outbound request, ready to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundRequest {
    /// `GET`, `POST`, …
    pub method: String,
    /// The full URL.
    pub url: String,
    /// Headers, lowercased.
    pub headers: BTreeMap<String, String>,
    /// The body, if there is one.
    pub body: Option<Vec<u8>>,
    /// How long to wait before giving up.
    pub timeout: Option<Duration>,
}

impl OutboundRequest {
    /// A header's value.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(&name.to_ascii_lowercase()).map(String::as_str)
    }

    /// The body as text.
    pub fn body_string(&self) -> String {
        self.body
            .as_ref()
            .map(|body| String::from_utf8_lossy(body).into_owned())
            .unwrap_or_default()
    }

    /// The body, parsed as JSON.
    ///
    /// `None` when there is no body or it is not JSON — a test asserting on a
    /// field wants to say so rather than unwrap two levels.
    pub fn json(&self) -> Option<serde_json::Value> {
        serde_json::from_slice(self.body.as_ref()?).ok()
    }
}

/// What actually sends a request.
pub trait Transport: Send + Sync + 'static {
    /// Send it.
    ///
    /// A transport reports **transport** failures — a refused connection, a
    /// DNS failure, a timeout. A `500` from the other end is a successful
    /// send of a request that got an unhappy answer, and comes back as `Ok`;
    /// see [`HttpResponse::error_for_status`](crate::HttpResponse::error_for_status).
    fn send<'a>(&'a self, request: OutboundRequest) -> BoxFuture<'a, Result<RawResponse>>;

    /// A label, for diagnostics.
    fn name(&self) -> &str;
}

/// What came back.
#[derive(Debug, Clone)]
pub struct RawResponse {
    /// The status.
    pub status: u16,
    /// Headers, lowercased.
    pub headers: BTreeMap<String, String>,
    /// The body.
    pub body: Vec<u8>,
}

#[cfg(feature = "reqwest-transport")]
mod real {
    use super::*;
    use rainier_support::Error;

    /// The real one, over `reqwest` with rustls.
    pub struct ReqwestTransport {
        client: reqwest::Client,
    }

    impl ReqwestTransport {
        /// A transport over a default client.
        ///
        /// # Panics
        ///
        /// If the TLS backend cannot be initialised, which is a build problem
        /// rather than a runtime one.
        pub fn new() -> Self {
            Self { client: reqwest::Client::new() }
        }

        /// A transport over a client you configured — a proxy, a custom root
        /// store, a connection pool sized for your traffic.
        pub fn with_client(client: reqwest::Client) -> Self {
            Self { client }
        }

        /// The client underneath, for whatever this crate does not wrap.
        pub fn client(&self) -> &reqwest::Client {
            &self.client
        }
    }

    impl Default for ReqwestTransport {
        fn default() -> Self {
            Self::new()
        }
    }

    impl Transport for ReqwestTransport {
        fn send<'a>(&'a self, request: OutboundRequest) -> BoxFuture<'a, Result<RawResponse>> {
            Box::pin(async move {
                let method =
                    reqwest::Method::from_bytes(request.method.as_bytes()).map_err(|_| {
                        Error::internal(format!("`{}` is not a method", request.method))
                    })?;

                let mut building = self.client.request(method, &request.url);

                for (name, value) in &request.headers {
                    building = building.header(name, value);
                }
                if let Some(body) = request.body {
                    building = building.body(body);
                }
                if let Some(timeout) = request.timeout {
                    building = building.timeout(timeout);
                }

                let response = building.send().await.map_err(|e| {
                    // The URL is in the message on purpose: "connection
                    // refused" with no address is the least useful line in any
                    // log.
                    Error::service_unavailable(format!("could not reach {}: {e}", request.url))
                })?;

                let status = response.status().as_u16();
                let headers = response
                    .headers()
                    .iter()
                    .filter_map(|(name, value)| {
                        Some((name.as_str().to_ascii_lowercase(), value.to_str().ok()?.to_string()))
                    })
                    .collect();

                let body = response
                    .bytes()
                    .await
                    .map_err(|e| {
                        Error::service_unavailable(format!("could not read the response body: {e}"))
                    })?
                    .to_vec();

                Ok(RawResponse { status, headers, body })
            })
        }

        fn name(&self) -> &str {
            "reqwest"
        }
    }
}

#[cfg(feature = "reqwest-transport")]
pub use real::ReqwestTransport;

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> OutboundRequest {
        OutboundRequest {
            method: "POST".into(),
            url: "https://hooks.example.com/user-updated".into(),
            headers: [("content-type".to_string(), "application/json".to_string())]
                .into_iter()
                .collect(),
            body: Some(br#"{"id":42}"#.to_vec()),
            timeout: None,
        }
    }

    #[test]
    fn headers_are_found_whatever_case_they_are_asked_for_in() {
        let request = request();

        assert_eq!(request.header("Content-Type"), Some("application/json"));
        assert_eq!(request.header("content-type"), Some("application/json"));
        assert_eq!(request.header("x-missing"), None);
    }

    #[test]
    fn the_body_reads_as_text_and_as_json() {
        let request = request();

        assert_eq!(request.body_string(), r#"{"id":42}"#);
        assert_eq!(request.json().unwrap()["id"], 42);
    }

    #[test]
    fn a_body_that_is_not_json_is_none_rather_than_an_error() {
        // A test asserting on a field should say `is_none()`, not unwrap two
        // levels of Result to find out.
        let mut request = request();
        request.body = Some(b"not json".to_vec());

        assert!(request.json().is_none());
        assert_eq!(request.body_string(), "not json");
    }

    #[test]
    fn no_body_is_an_empty_string_and_no_json() {
        let mut request = request();
        request.body = None;

        assert_eq!(request.body_string(), "");
        assert!(request.json().is_none());
    }
}
