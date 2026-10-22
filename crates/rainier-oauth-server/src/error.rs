//! The error an authorization server answers with — [`OAuthError`].
//!
//! RFC 6749 §5.2 fixes the shape: `error`, `error_description`, and an
//! optional hint. The descriptions here are the long-standing ones the
//! widely deployed PHP server (league/oauth2-server) writes, word for word, so
//! a client that matches on a message — they exist — sees the same text from
//! this server as from the one it replaces.

use rainier_http::{Response, StatusCode};
use serde_json::{Map, Value};

/// A refusal from the authorization server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthError {
    /// The RFC 6749 error code — `invalid_grant`, `invalid_client`, …
    pub error: &'static str,
    /// The human-readable description.
    pub description: String,
    /// What to check, when there is something specific.
    pub hint: Option<String>,
    /// The HTTP status.
    pub status: u16,
    /// Where to send the browser instead of answering JSON — set for errors
    /// on the authorization endpoint once a redirect URI is known.
    pub redirect_uri: Option<String>,
    /// Whether the client authenticated with an `Authorization` header, which
    /// RFC 6749 §5.2 says earns a `WWW-Authenticate` on `invalid_client`.
    pub(crate) authorization_header: Option<&'static str>,
}

impl OAuthError {
    fn new(error: &'static str, description: impl Into<String>, status: u16) -> Self {
        Self {
            error,
            description: description.into(),
            hint: None,
            status,
            redirect_uri: None,
            authorization_header: None,
        }
    }

    /// Attach a hint.
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    /// Answer by redirecting to `uri` with the error in the query string.
    pub fn redirecting_to(mut self, uri: impl Into<String>) -> Self {
        self.redirect_uri = Some(uri.into());
        self
    }

    /// `unsupported_grant_type`.
    pub fn unsupported_grant_type() -> Self {
        Self::new(
            "unsupported_grant_type",
            "The authorization grant type is not supported by the authorization server.",
            400,
        )
        .with_hint("Check that all required parameters have been provided")
    }

    /// `invalid_request`, naming the parameter at fault.
    pub fn invalid_request(parameter: &str) -> Self {
        Self::new(
            "invalid_request",
            "The request is missing a required parameter, includes an invalid parameter value, \
             includes a parameter more than once, or is otherwise malformed.",
            400,
        )
        .with_hint(format!("Check the `{parameter}` parameter"))
    }

    /// `invalid_request` with a specific hint.
    pub fn invalid_request_because(hint: impl Into<String>) -> Self {
        Self::invalid_request("").with_hint(hint)
    }

    /// `invalid_client` — 401.
    pub fn invalid_client() -> Self {
        Self::new("invalid_client", "Client authentication failed", 401)
    }

    /// `invalid_scope`, naming the scope.
    pub fn invalid_scope(scope: &str) -> Self {
        let hint = if scope.is_empty() {
            "Specify a scope in the request or set a default scope".to_string()
        } else {
            format!("Check the `{}` scope", escape(scope))
        };
        Self::new("invalid_scope", "The requested scope is invalid, unknown, or malformed", 400)
            .with_hint(hint)
    }

    /// `invalid_grant` for wrong resource-owner credentials.
    pub fn invalid_credentials() -> Self {
        Self::new("invalid_grant", "The user credentials were incorrect.", 400)
    }

    /// `invalid_grant` for a refresh token that cannot be used.
    pub fn invalid_refresh_token(hint: impl Into<String>) -> Self {
        Self::new("invalid_grant", "The refresh token is invalid.", 400).with_hint(hint)
    }

    /// `invalid_grant`.
    pub fn invalid_grant(hint: impl Into<String>) -> Self {
        Self::new(
            "invalid_grant",
            "The provided authorization grant (e.g., authorization code, resource owner \
             credentials) or refresh token is invalid, expired, revoked, does not match the \
             redirection URI used in the authorization request, or was issued to another client.",
            400,
        )
        .with_hint(hint)
    }

    /// `unauthorized_client` — the client may not use this grant.
    pub fn unauthorized_client() -> Self {
        Self::new(
            "unauthorized_client",
            "The authenticated client is not authorized to use this authorization grant type.",
            400,
        )
    }

    /// `access_denied` — 401.
    pub fn access_denied(hint: impl Into<String>) -> Self {
        Self::new(
            "access_denied",
            "The resource owner or authorization server denied the request.",
            401,
        )
        .with_hint(hint)
    }

    /// `server_error` — 500. The detail goes to the log, not the client.
    pub fn server_error(detail: impl std::fmt::Display) -> Self {
        tracing::error!(error = %detail, "the authorization server failed");
        Self::new(
            "server_error",
            "The authorization server encountered an unexpected condition which prevented it \
             from fulfilling the request.",
            500,
        )
    }

    /// The JSON body: `error`, `error_description`, and `hint` when there is one.
    pub fn payload(&self) -> Map<String, Value> {
        let mut payload = Map::new();
        payload.insert("error".into(), self.error.into());
        payload.insert("error_description".into(), self.description.clone().into());
        if let Some(hint) = &self.hint {
            payload.insert("hint".into(), hint.clone().into());
        }
        payload
    }

    /// The HTTP answer: a redirect when there is somewhere to send the
    /// browser, JSON otherwise.
    pub fn into_response(self) -> Response {
        let payload = self.payload();

        if let Some(uri) = &self.redirect_uri {
            let pairs: Vec<(String, String)> = payload
                .iter()
                .map(|(key, value)| (key.clone(), value.as_str().unwrap_or_default().to_string()))
                .collect();
            return found(&append_query(uri, &pairs));
        }

        let mut response = Response::json(&Value::Object(payload))
            .with_status(StatusCode::from_u16(self.status).unwrap_or(StatusCode::BAD_REQUEST));
        if self.error == "invalid_client" {
            if let Some(scheme) = self.authorization_header {
                response =
                    response.with_header("www-authenticate", &format!("{scheme} realm=\"OAuth\""));
            }
        }
        response
    }
}

impl std::fmt::Display for OAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.error, self.description)?;
        if let Some(hint) = &self.hint {
            write!(f, " ({hint})")?;
        }
        Ok(())
    }
}

impl std::error::Error for OAuthError {}

impl From<rainier_support::Error> for OAuthError {
    fn from(error: rainier_support::Error) -> Self {
        Self::server_error(error)
    }
}

/// So `?` works in a handler that returns the framework's `Result`. The
/// RFC 6749 shape is lost on the way — answer the token endpoint with
/// [`OAuthError::into_response`] instead — but the status and the description
/// survive.
impl From<OAuthError> for rainier_support::Error {
    fn from(error: OAuthError) -> Self {
        let message = error.to_string();
        match error.status {
            401 => rainier_support::Error::unauthenticated(message),
            400..=499 => rainier_support::Error::bad_request(message),
            _ => rainier_support::Error::internal(message),
        }
    }
}

impl From<OAuthError> for Response {
    fn from(error: OAuthError) -> Self {
        error.into_response()
    }
}

/// A `302 Found` to `location` — what RFC 6749 §4.1.2 sends the browser back
/// with.
pub(crate) fn found(location: &str) -> Response {
    Response::new(StatusCode::FOUND).with_header("location", location)
}

/// `uri` with `pairs` appended to its query string.
pub(crate) fn append_query(uri: &str, pairs: &[(String, String)]) -> String {
    let encoded: Vec<String> =
        pairs.iter().map(|(key, value)| format!("{}={}", encode(key), encode(value))).collect();
    if encoded.is_empty() {
        return uri.to_string();
    }
    let separator = if uri.contains('?') { '&' } else { '?' };
    format!("{uri}{separator}{}", encoded.join("&"))
}

/// Form-style percent-encoding (spaces as `+`), as a query string carries it.
pub(crate) fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'*' => {
                out.push(byte as char)
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// The HTML-escaping the hint applies to a scope a client sent.
fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('\'', "&#039;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// A JSON body for a successful token response, with the headers RFC 6749
/// §5.1 requires.
pub(crate) fn token_response(body: &Value) -> Response {
    Response::json(body)
        .with_header("content-type", "application/json; charset=UTF-8")
        .with_header("cache-control", "no-store")
        .with_header("pragma", "no-cache")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_payload_is_the_rfc_shape() {
        let error = OAuthError::invalid_request("code");
        let payload = error.payload();
        assert_eq!(payload["error"], "invalid_request");
        assert_eq!(payload["hint"], "Check the `code` parameter");
        assert!(payload["error_description"]
            .as_str()
            .unwrap()
            .starts_with("The request is missing"));
    }

    #[test]
    fn invalid_client_is_a_401_with_a_challenge_when_basic_auth_was_used() {
        let mut error = OAuthError::invalid_client();
        error.authorization_header = Some("Basic");
        let response = error.into_response();
        assert_eq!(response.status().as_u16(), 401);
        assert_eq!(response.header("www-authenticate"), Some("Basic realm=\"OAuth\""));
    }

    #[test]
    fn a_redirecting_error_puts_the_payload_in_the_query() {
        let response = OAuthError::access_denied("The user denied the request")
            .redirecting_to("https://app.test/cb?state=xyz")
            .into_response();
        let location = response.header("location").unwrap();
        assert!(location.starts_with("https://app.test/cb?state=xyz&error=access_denied"));
        assert!(location.contains("hint=The+user+denied+the+request"));
    }

    #[test]
    fn a_scope_in_a_hint_is_escaped() {
        let error = OAuthError::invalid_scope("<script>");
        assert_eq!(error.hint.as_deref(), Some("Check the `&lt;script&gt;` scope"));
    }
}
