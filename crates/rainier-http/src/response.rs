//! The outgoing [`Response`], and [`IntoResponse`] for returning one from a
//! handler.
//!
//! A controller action rarely wants to build a `Response` by hand, so anything
//! reasonable converts into one: a string, a `serde_json::Value`, a
//! [`Json<T>`], a [`Redirect`], a `StatusCode`, a `Result`, or a tuple pairing
//! a status with any of those.

use bytes::Bytes;
use http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use rainier_support::{Error, Extensions};
use serde::Serialize;

use crate::body::Body;
use crate::cookie::Cookie;

/// An outgoing HTTP response.
#[derive(Debug)]
pub struct Response {
    status: StatusCode,
    headers: HeaderMap,
    body: Body,
    extensions: Extensions,
}

impl Response {
    /// An empty response with `status`.
    pub fn new(status: StatusCode) -> Self {
        Self { status, headers: HeaderMap::new(), body: Body::Empty, extensions: Extensions::new() }
    }

    /// `200 OK` with `body`.
    pub fn ok(body: impl Into<Body>) -> Self {
        Self::new(StatusCode::OK).with_body(body)
    }

    /// `204 No Content`.
    pub fn no_content() -> Self {
        Self::new(StatusCode::NO_CONTENT)
    }

    /// `200 OK` with a `text/plain` body.
    pub fn text(body: impl Into<String>) -> Self {
        Self::ok(body.into()).with_content_type("text/plain; charset=utf-8")
    }

    /// `200 OK` with a `text/html` body.
    pub fn html(body: impl Into<String>) -> Self {
        Self::ok(body.into()).with_content_type("text/html; charset=utf-8")
    }

    /// `200 OK` with `value` serialised as JSON.
    ///
    /// A value that cannot be serialised becomes a `500` describing the
    /// failure, because there is nowhere else for the error to go — the
    /// signature has to produce a response.
    pub fn json(value: &impl Serialize) -> Self {
        match serde_json::to_vec(value) {
            Ok(encoded) => Self::ok(encoded).with_content_type("application/json; charset=utf-8"),
            Err(e) => Error::internal(format!("could not serialise the response: {e}")).into(),
        }
    }

    /// `201 Created`, with a `Location` header.
    pub fn created(location: impl AsRef<str>) -> Self {
        Self::new(StatusCode::CREATED).with_header("location", location.as_ref())
    }

    /// A response that offers the body as a file download.
    pub fn download(bytes: impl Into<Bytes>, file_name: &str) -> Self {
        // Quote the name and strip quotes/newlines from it: an unescaped `"`
        // or CRLF here would let a caller inject header content.
        let safe: String =
            file_name.chars().filter(|c| *c != '"' && *c != '\r' && *c != '\n').collect();
        Self::ok(bytes.into())
            .with_content_type("application/octet-stream")
            .with_header("content-disposition", &format!("attachment; filename=\"{safe}\""))
    }

    /// A streaming response.
    pub fn stream<S>(stream: S) -> Self
    where
        S: futures_core::Stream<Item = Result<Bytes, Error>> + Send + 'static,
    {
        Self::new(StatusCode::OK).with_body(Body::from_stream(stream))
    }

    // --- accessors ---------------------------------------------------------

    /// The status code.
    pub fn status(&self) -> StatusCode {
        self.status
    }

    /// The headers.
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    /// Mutable headers.
    pub fn headers_mut(&mut self) -> &mut HeaderMap {
        &mut self.headers
    }

    /// One header as a string.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name)?.to_str().ok()
    }

    /// The body.
    pub fn body(&self) -> &Body {
        &self.body
    }

    /// Take the body, leaving an empty one behind.
    pub fn take_body(&mut self) -> Body {
        std::mem::take(&mut self.body)
    }

    /// Response attributes, keyed by type.
    pub fn extensions(&self) -> &Extensions {
        &self.extensions
    }

    /// Mutable response attributes.
    pub fn extensions_mut(&mut self) -> &mut Extensions {
        &mut self.extensions
    }

    /// Whether the status is 2xx.
    pub fn is_successful(&self) -> bool {
        self.status.is_success()
    }

    // --- builders ----------------------------------------------------------

    /// Replace the status.
    pub fn with_status(mut self, status: StatusCode) -> Self {
        self.status = status;
        self
    }

    /// Replace the body.
    pub fn with_body(mut self, body: impl Into<Body>) -> Self {
        self.body = body.into();
        self
    }

    /// Set a header, replacing any existing one of that name.
    ///
    /// An invalid name or value is dropped rather than panicking: header
    /// content often comes from configuration or user data, and taking down a
    /// request for it would be worse than omitting it.
    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        if let (Ok(name), Ok(value)) = (HeaderName::try_from(name), HeaderValue::from_str(value)) {
            self.headers.insert(name, value);
        }
        self
    }

    /// Append a header, keeping any existing ones of that name.
    pub fn with_added_header(mut self, name: &str, value: &str) -> Self {
        if let (Ok(name), Ok(value)) = (HeaderName::try_from(name), HeaderValue::from_str(value)) {
            self.headers.append(name, value);
        }
        self
    }

    /// Set `Content-Type`.
    pub fn with_content_type(self, content_type: &str) -> Self {
        self.with_header("content-type", content_type)
    }

    /// Attach a `Set-Cookie` header. Several cookies may be attached.
    pub fn with_cookie(self, cookie: &Cookie) -> Self {
        self.with_added_header("set-cookie", &cookie.to_set_cookie())
    }

    /// Instruct the client to drop a cookie.
    pub fn without_cookie(self, name: &str) -> Self {
        self.with_cookie(&Cookie::removal(name))
    }

    /// The body, collected into bytes.
    ///
    /// Consumes the response, because a streaming body can only be read once.
    /// For a test, [`into_string`](Self::into_string) is usually what you want.
    pub async fn into_bytes(self) -> Result<Bytes, Error> {
        self.body.collect().await
    }

    /// The body, collected and decoded as UTF-8.
    ///
    /// The one an assertion reaches for. Without it, reading a body in a test
    /// means `response.into_http().into_body().collect().await` — correct for a
    /// server, wrong ergonomics for `assert_eq!`.
    ///
    /// Invalid UTF-8 is replaced rather than refused: a test that is about to
    /// print this wants to see what arrived, not an error about it.
    pub async fn into_string(self) -> Result<String, Error> {
        Ok(String::from_utf8_lossy(&self.into_bytes().await?).into_owned())
    }

    /// The body, parsed as JSON.
    pub async fn into_json<T: serde::de::DeserializeOwned>(self) -> Result<T, Error> {
        let bytes = self.into_bytes().await?;

        serde_json::from_slice(&bytes).map_err(|e| {
            // Quote what actually arrived: a JSON parse failure in a test is
            // nearly always an error response nobody expected, and the message
            // is the fastest way to see that.
            let body = String::from_utf8_lossy(&bytes);
            Error::internal(format!(
                "the response body is not the JSON that was expected: {e} — body was: {}",
                body.chars().take(400).collect::<String>()
            ))
        })
    }

    /// Convert into an `http::Response`, ready for the server to write.
    pub fn into_http(self) -> http::Response<Body> {
        let mut builder = http::Response::builder().status(self.status);
        if let Some(headers) = builder.headers_mut() {
            *headers = self.headers;
        }
        builder.body(self.body).unwrap_or_else(|_| {
            // Unreachable: the status came from a `StatusCode` and the headers
            // from a `HeaderMap`, both already validated.
            http::Response::new(Body::Empty)
        })
    }
}

impl Default for Response {
    fn default() -> Self {
        Self::new(StatusCode::OK)
    }
}

/// A redirect response.
#[derive(Debug, Clone)]
pub struct Redirect {
    status: StatusCode,
    location: String,
}

impl Redirect {
    /// `302 Found` — a temporary redirect that clients may re-issue as GET.
    pub fn to(location: impl Into<String>) -> Self {
        Self { status: StatusCode::FOUND, location: location.into() }
    }

    /// `301 Moved Permanently`.
    pub fn permanent(location: impl Into<String>) -> Self {
        Self { status: StatusCode::MOVED_PERMANENTLY, location: location.into() }
    }

    /// `303 See Other` — the correct redirect after a successful POST, since
    /// it tells the client to follow up with a GET.
    pub fn see_other(location: impl Into<String>) -> Self {
        Self { status: StatusCode::SEE_OTHER, location: location.into() }
    }

    /// `307 Temporary Redirect` — like 302 but the method is preserved.
    pub fn temporary(location: impl Into<String>) -> Self {
        Self { status: StatusCode::TEMPORARY_REDIRECT, location: location.into() }
    }
}

/// A `text/html` body.
#[derive(Debug, Clone)]
pub struct Html<T>(pub T);

/// An `application/json` body.
#[derive(Debug, Clone)]
pub struct Json<T>(pub T);

/// Anything a handler can return.
pub trait IntoResponse {
    /// Build the response.
    fn into_response(self) -> Response;
}

impl IntoResponse for Response {
    fn into_response(self) -> Response {
        self
    }
}

impl IntoResponse for () {
    fn into_response(self) -> Response {
        Response::no_content()
    }
}

impl IntoResponse for StatusCode {
    fn into_response(self) -> Response {
        Response::new(self)
    }
}

impl IntoResponse for String {
    fn into_response(self) -> Response {
        Response::text(self)
    }
}

impl IntoResponse for &'static str {
    fn into_response(self) -> Response {
        Response::text(self)
    }
}

impl IntoResponse for serde_json::Value {
    fn into_response(self) -> Response {
        Response::json(&self)
    }
}

impl<T: Serialize> IntoResponse for Json<T> {
    fn into_response(self) -> Response {
        Response::json(&self.0)
    }
}

impl<T: Into<String>> IntoResponse for Html<T> {
    fn into_response(self) -> Response {
        Response::html(self.0)
    }
}

impl IntoResponse for Redirect {
    fn into_response(self) -> Response {
        Response::new(self.status).with_header("location", &self.location)
    }
}

impl IntoResponse for Body {
    fn into_response(self) -> Response {
        Response::ok(self)
    }
}

impl<T: IntoResponse> IntoResponse for (StatusCode, T) {
    fn into_response(self) -> Response {
        let (status, inner) = self;
        inner.into_response().with_status(status)
    }
}

impl<T: IntoResponse> IntoResponse for Option<T> {
    /// `None` becomes a `404` — the usual meaning of "the handler found
    /// nothing".
    fn into_response(self) -> Response {
        match self {
            Some(inner) => inner.into_response(),
            None => Error::not_found("Not Found").into_response(),
        }
    }
}

impl<T, E> IntoResponse for Result<T, E>
where
    T: IntoResponse,
    E: IntoResponse,
{
    fn into_response(self) -> Response {
        match self {
            Ok(inner) => inner.into_response(),
            Err(e) => e.into_response(),
        }
    }
}

/// Marks a response as having been rendered from a framework [`Error`], and
/// carries the parts needed to render it *differently*.
///
/// Without this, the HTTP kernel could not tell a JSON error body it produced
/// from one a handler deliberately returned, and so could not offer a browser
/// an HTML error page instead. Attached to the response's extensions, so it
/// never reaches the client.
#[derive(Debug, Clone)]
pub struct RenderedError {
    /// The status the error rendered as.
    pub status: u16,
    /// The human-readable message.
    pub message: String,
    /// The structured details, if any.
    pub details: Option<serde_json::Value>,
    /// Whether the message is safe to show a client.
    ///
    /// `false` for 5xx: an internal error's message routinely contains a
    /// connection string, a file path or a query. The kernel substitutes a
    /// generic message unless the application is in debug mode.
    pub disclosable: bool,
}

/// Renders a framework error as JSON.
///
/// The default, and what an API client should get. The HTTP kernel wraps it
/// with content negotiation — an HTML error page for a browser — using the
/// [`RenderedError`] this attaches.
impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let status =
            StatusCode::from_u16(self.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let disclosable = !self.kind().is_server_error();

        let mut payload = serde_json::Map::new();
        payload.insert("message".into(), serde_json::Value::String(self.message().to_string()));
        if let Some(details) = self.details() {
            payload.insert("errors".into(), details.clone());
        }

        let mut response = Response::json(&serde_json::Value::Object(payload)).with_status(status);

        response.extensions_mut().insert(RenderedError {
            status: self.status(),
            message: self.message().to_string(),
            details: self.details().cloned(),
            disclosable,
        });
        response
    }
}

impl From<Error> for Response {
    fn from(error: Error) -> Self {
        error.into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    async fn body_of(response: Response) -> String {
        String::from_utf8(response.body.collect().await.unwrap().to_vec()).unwrap()
    }

    #[tokio::test]
    async fn builds_the_common_shapes() {
        let text = Response::text("hi");
        assert_eq!(text.status(), StatusCode::OK);
        assert_eq!(text.header("content-type"), Some("text/plain; charset=utf-8"));
        assert_eq!(body_of(text).await, "hi");

        let html = Response::html("<p>hi</p>");
        assert_eq!(html.header("content-type"), Some("text/html; charset=utf-8"));

        let json = Response::json(&json!({ "a": 1 }));
        assert_eq!(json.header("content-type"), Some("application/json; charset=utf-8"));
        assert_eq!(body_of(json).await, r#"{"a":1}"#);

        assert_eq!(Response::no_content().status(), StatusCode::NO_CONTENT);
        assert_eq!(Response::created("/posts/1").header("location"), Some("/posts/1"));
    }

    #[test]
    fn headers_replace_or_append() {
        let response = Response::ok(()).with_header("x-a", "1").with_header("x-a", "2");
        assert_eq!(response.headers().get_all("x-a").iter().count(), 1);
        assert_eq!(response.header("x-a"), Some("2"));

        let response = Response::ok(()).with_added_header("x-b", "1").with_added_header("x-b", "2");
        assert_eq!(response.headers().get_all("x-b").iter().count(), 2);
    }

    #[test]
    fn an_invalid_header_is_dropped_rather_than_panicking() {
        let response = Response::ok(()).with_header("bad header name", "v");
        assert!(response.headers().is_empty());

        let response = Response::ok(()).with_header("x-ok", "line\nbreak");
        assert!(response.headers().is_empty());
    }

    #[test]
    fn cookies_stack_up_as_separate_headers() {
        let response = Response::ok(())
            .with_cookie(&Cookie::new("a", "1"))
            .with_cookie(&Cookie::new("b", "2"))
            .without_cookie("c");

        assert_eq!(response.headers().get_all("set-cookie").iter().count(), 3);
    }

    #[test]
    fn downloads_escape_the_file_name() {
        let response = Response::download("data", "re\"port\r\n.csv");
        assert_eq!(
            response.header("content-disposition"),
            Some("attachment; filename=\"report.csv\"")
        );
    }

    #[tokio::test]
    async fn into_response_covers_the_handler_return_types() {
        assert_eq!(().into_response().status(), StatusCode::NO_CONTENT);
        assert_eq!("hi".into_response().status(), StatusCode::OK);
        assert_eq!(String::from("hi").into_response().status(), StatusCode::OK);
        assert_eq!(StatusCode::IM_A_TEAPOT.into_response().status(), StatusCode::IM_A_TEAPOT);

        let tuple = (StatusCode::ACCEPTED, "queued").into_response();
        assert_eq!(tuple.status(), StatusCode::ACCEPTED);
        assert_eq!(body_of(tuple).await, "queued");

        let json = Json(json!({ "ok": true })).into_response();
        assert_eq!(body_of(json).await, r#"{"ok":true}"#);

        let html = Html("<b>x</b>").into_response();
        assert_eq!(html.header("content-type"), Some("text/html; charset=utf-8"));
    }

    #[test]
    fn options_and_results_carry_their_failure() {
        let none: Option<&'static str> = None;
        assert_eq!(none.into_response().status(), StatusCode::NOT_FOUND);
        assert_eq!(Some("x").into_response().status(), StatusCode::OK);

        let ok: Result<&'static str, Error> = Ok("x");
        assert_eq!(ok.into_response().status(), StatusCode::OK);

        let err: Result<&'static str, Error> = Err(Error::not_found("no post"));
        assert_eq!(err.into_response().status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn errors_render_as_json_with_their_details() {
        let error = Error::validation(json!({ "email": ["is required"] }));
        let response = error.into_response();

        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body: serde_json::Value = serde_json::from_str(&body_of(response).await).unwrap();
        assert_eq!(body["message"], "The given data was invalid.");
        assert_eq!(body["errors"]["email"][0], "is required");
    }

    #[test]
    fn redirects_set_the_location_and_status() {
        assert_eq!(Redirect::to("/a").into_response().status(), StatusCode::FOUND);
        assert_eq!(
            Redirect::permanent("/a").into_response().status(),
            StatusCode::MOVED_PERMANENTLY
        );
        assert_eq!(Redirect::see_other("/a").into_response().status(), StatusCode::SEE_OTHER);
        assert_eq!(
            Redirect::temporary("/a").into_response().status(),
            StatusCode::TEMPORARY_REDIRECT
        );
        assert_eq!(Redirect::to("/dash").into_response().header("location"), Some("/dash"));
    }

    #[tokio::test]
    async fn converts_into_an_http_response() {
        let response = Response::text("body").with_header("x-a", "1").into_http();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers().get("x-a").unwrap(), "1");
    }
}
