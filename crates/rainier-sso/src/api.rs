//! The calls a provider makes — [`Api`], over the framework's
//! [HTTP transport port](rainier_http_client::Transport).
//!
//! The port, not the socket: every request here is a **shape**, and keeping it
//! behind the port is what lets a test hand the provider a
//! [`FakeTransport`](rainier_http_client::FakeTransport) and assert on the
//! exact form that would have been posted to a token endpoint.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use rainier_http_client::transport::{OutboundRequest, RawResponse};
use rainier_http_client::Transport;
use rainier_support::{Error, Result};
use serde_json::Value;

/// One provider's outbound calls.
#[derive(Clone)]
pub struct Api {
    transport: Arc<dyn Transport>,
    timeout: Duration,
}

impl Api {
    /// Calls through `transport`, with a thirty-second wall clock.
    pub fn new(transport: Arc<dyn Transport>) -> Self {
        Self { transport, timeout: Duration::from_secs(30) }
    }

    /// The wall clock on each call.
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    fn request(&self, method: &str, url: &str) -> OutboundRequest {
        OutboundRequest {
            method: method.into(),
            url: url.into(),
            // GitHub refuses a request with no User-Agent, and every other
            // provider is happier knowing who is calling.
            headers: [
                ("accept".to_string(), "application/json".to_string()),
                ("user-agent".to_string(), "rainier-sso".to_string()),
            ]
            .into_iter()
            .collect(),
            body: None,
            timeout: Some(self.timeout),
        }
    }

    /// `GET url`, with `token` as a bearer when given, as it came back.
    pub async fn get(&self, url: &str, token: Option<&str>) -> Result<RawResponse> {
        let mut request = self.request("GET", url);
        if let Some(token) = token {
            request.headers.insert("authorization".into(), format!("Bearer {token}"));
        }
        self.transport.send(request).await
    }

    /// `GET url` and read a 2xx answer as JSON.
    pub async fn get_json(&self, service: &str, url: &str, token: Option<&str>) -> Result<Value> {
        json_of(service, url, self.get(url, token).await?)
    }

    /// `POST url` with a form body and extra headers, as it came back.
    pub async fn post_form(
        &self,
        url: &str,
        form: &[(&str, &str)],
        headers: &[(&str, String)],
    ) -> Result<RawResponse> {
        let mut request = self.request("POST", url);
        request.headers.insert("content-type".into(), "application/x-www-form-urlencoded".into());
        for (name, value) in headers {
            request.headers.insert(name.to_ascii_lowercase(), value.clone());
        }

        // Built in its own scope: the serializer holds a non-`Sync` encoder
        // and must not live across the await below.
        request.body = Some({
            let mut body = form_urlencoded::Serializer::new(String::new());
            for (key, value) in form {
                body.append_pair(key, value);
            }
            body.finish().into_bytes()
        });

        self.transport.send(request).await
    }
}

impl std::fmt::Debug for Api {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Api")
            .field("transport", &self.transport.name())
            .field("timeout", &self.timeout)
            .finish()
    }
}

/// A 2xx body as JSON; anything else as the provider's refusal, quoted.
///
/// A 5xx or a 429 is the provider having a bad minute and reads as
/// `service_unavailable`; a 4xx is this code, this token or these
/// credentials, and reads as `unauthenticated` — the sign-in did not happen,
/// and retrying it changes nothing.
pub(crate) fn json_of(service: &str, url: &str, response: RawResponse) -> Result<Value> {
    if !(200..300).contains(&response.status) {
        let message = format!(
            "{service} answered {} for {}: {}",
            response.status,
            strip_query(url),
            excerpt(&String::from_utf8_lossy(&response.body))
        );
        return Err(if response.status >= 500 || response.status == 429 {
            Error::service_unavailable(message)
        } else {
            Error::unauthenticated(message)
        });
    }

    parse_body(&response.body).ok_or_else(|| {
        Error::internal(format!(
            "{service} answered {} with a body that is not JSON",
            strip_query(url)
        ))
    })
}

/// JSON, or — for the token endpoints that still default to it — a
/// form-encoded body read into a flat object.
pub(crate) fn parse_body(body: &[u8]) -> Option<Value> {
    if let Ok(value) = serde_json::from_slice::<Value>(body) {
        return Some(value);
    }

    let pairs: BTreeMap<String, Value> = form_urlencoded::parse(body)
        .map(|(key, value)| (key.into_owned(), Value::String(value.into_owned())))
        .collect();
    if pairs.is_empty() {
        None
    } else {
        Some(Value::Object(pairs.into_iter().collect()))
    }
}

/// A URL without its query, which is where an access token would be if a
/// provider took one there.
fn strip_query(url: &str) -> &str {
    url.split('?').next().unwrap_or(url)
}

/// Enough of an error body to act on, flattened to one line.
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
    use rainier_support::ErrorKind;

    fn response(status: u16, body: &str) -> RawResponse {
        RawResponse { status, headers: BTreeMap::new(), body: body.as_bytes().to_vec() }
    }

    #[test]
    fn a_refusal_is_unauthenticated_and_an_outage_is_retryable() {
        let refused = json_of("github", "https://x/token", response(400, "bad_verification_code"))
            .unwrap_err();
        assert_eq!(refused.kind(), ErrorKind::Unauthenticated);
        assert!(refused.message().contains("bad_verification_code"));

        let down = json_of("github", "https://x/token", response(502, "")).unwrap_err();
        assert_eq!(down.kind(), ErrorKind::ServiceUnavailable);
    }

    #[test]
    fn a_form_encoded_body_is_read_as_an_object() {
        let value = parse_body(b"access_token=abc&scope=user%2Cemail").unwrap();
        assert_eq!(value["access_token"], "abc");
        assert_eq!(value["scope"], "user,email");
    }

    #[test]
    fn the_query_stays_out_of_error_messages() {
        let err =
            json_of("x", "https://x/me?access_token=secret", response(401, "no")).unwrap_err();
        assert!(!err.message().contains("secret"), "{}", err.message());
    }
}
