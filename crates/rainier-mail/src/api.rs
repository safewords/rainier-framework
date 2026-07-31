//! What the HTTP API transports share: sending one request through the
//! framework's [HTTP transport port](rainier_http_client::Transport) and
//! turning the answer into a delivery verdict.
//!
//! The port, not the socket — a mail API transport is a **request shape**, and
//! keeping it behind the port is what lets a test hand it a
//! [`FakeTransport`](rainier_http_client::FakeTransport) and assert on the
//! exact JSON that would have left the building.

use std::sync::Arc;
use std::time::Duration;

use rainier_http_client::{transport::RawResponse, Transport as HttpTransport};
use rainier_support::{Error, Result};

/// One provider call: POST `url` with `headers` and a `body`, expect 2xx.
pub(crate) async fn deliver(
    http: &Arc<dyn HttpTransport>,
    service: &str,
    url: &str,
    headers: Vec<(&str, String)>,
    content_type: &str,
    body: Vec<u8>,
    timeout: Duration,
) -> Result<()> {
    let mut request = rainier_http_client::transport::OutboundRequest {
        method: "POST".into(),
        url: url.to_string(),
        headers: [("content-type".to_string(), content_type.to_string())].into_iter().collect(),
        body: Some(body),
        timeout: Some(timeout),
    };
    for (name, value) in headers {
        request.headers.insert(name.to_ascii_lowercase(), value);
    }

    verdict(service, url, http.send(request).await?)
}

/// A 2xx is a delivery; anything else is the provider's refusal, quoted.
fn verdict(service: &str, url: &str, response: RawResponse) -> Result<()> {
    if (200..300).contains(&response.status) {
        return Ok(());
    }

    let body = excerpt(&String::from_utf8_lossy(&response.body));
    let message = format!("{service} answered {} for {url}: {body}", response.status);

    // A 5xx or a 429 is the provider having a bad minute — the retryable kind,
    // which is what a queued job's retry policy keys on. A 4xx is this
    // message or these credentials, and retrying it changes nothing.
    if response.status >= 500 || response.status == 429 {
        Err(Error::service_unavailable(message))
    } else {
        Err(Error::internal(message))
    }
}

/// Enough of an error body to act on, flattened to one log line.
fn excerpt(body: &str) -> String {
    let flat = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.is_empty() {
        return "(empty body)".into();
    }
    if flat.chars().count() <= 300 {
        return flat;
    }
    let cut: String = flat.chars().take(300).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn response(status: u16, body: &str) -> RawResponse {
        RawResponse { status, headers: BTreeMap::new(), body: body.as_bytes().to_vec() }
    }

    #[test]
    fn a_2xx_is_a_delivery() {
        assert!(verdict("postmark", "https://x/email", response(200, "")).is_ok());
        assert!(verdict("sendgrid", "https://x", response(202, "")).is_ok());
    }

    #[test]
    fn a_refusal_quotes_the_provider() {
        let err = verdict(
            "postmark",
            "https://api.postmarkapp.com/email",
            response(422, r#"{"ErrorCode":300,"Message":"Invalid 'To' address."}"#),
        )
        .unwrap_err();

        assert!(err.message().contains("postmark answered 422"), "{}", err.message());
        assert!(err.message().contains("Invalid 'To'"), "{}", err.message());
    }

    #[test]
    fn a_provider_outage_reads_as_retryable_and_a_rejection_does_not() {
        use rainier_support::ErrorKind;

        let outage = verdict("resend", "https://x", response(503, "down")).unwrap_err();
        assert_eq!(outage.kind(), ErrorKind::ServiceUnavailable);

        let throttled = verdict("resend", "https://x", response(429, "slow down")).unwrap_err();
        assert_eq!(throttled.kind(), ErrorKind::ServiceUnavailable);

        let rejected = verdict("resend", "https://x", response(401, "bad key")).unwrap_err();
        assert_ne!(rejected.kind(), ErrorKind::ServiceUnavailable);
    }

    #[test]
    fn an_error_body_is_flattened_and_bounded() {
        assert_eq!(excerpt("a\r\nb"), "a b");
        assert_eq!(excerpt("  "), "(empty body)");

        let long = "x".repeat(500);
        let cut = excerpt(&long);
        assert!(cut.chars().count() <= 301, "300 plus the ellipsis");
        assert!(cut.ends_with('…'));
    }
}
