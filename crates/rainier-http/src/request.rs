//! The incoming [`Request`].
//!
//! One object carrying the
//! HTTP message *and* the conveniences an application actually reaches for —
//! `input()`, `file()`, `cookie()`, `bearer_token()`, `expects_json()`.
//!
//! Parsing is **lazy but synchronous**. The body arrives already buffered, and
//! the query string, cookies and body are each parsed on first access and
//! cached in a `OnceLock`. A request that only reads a header never runs the
//! form parser; a request that reads `input()` twice runs it once.

use std::collections::HashMap;
use std::sync::OnceLock;

use bytes::Bytes;
use http::{HeaderMap, Method, Uri, Version};
use rainier_support::{Error, Extensions, Result};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::cookie::parse_cookie_header;
use crate::input;
use crate::upload::{self, Multipart, UploadedFile};

/// The client's address, put into the request's extensions by the server.
///
/// A newtype rather than a bare `SocketAddr` so it cannot collide with another
/// address an application stores in the same type-keyed bag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientIp(pub std::net::IpAddr);

/// How a request body was interpreted.
#[derive(Debug)]
enum ParsedBody {
    /// `application/json`, parsed.
    Json(Value),
    /// `application/x-www-form-urlencoded`, lifted into a JSON tree.
    Form(Value),
    /// `multipart/form-data`, split into fields and files.
    Multipart(Box<Multipart>),
    /// No body, an unrecognised content type, or a body that failed to parse.
    /// Failures land here rather than erroring, because a handler that never
    /// looks at the body should not care that it was malformed — the error
    /// surfaces from [`Request::json`], which is where it can be reported.
    Opaque,
}

/// An inbound HTTP request.
#[derive(Debug)]
pub struct Request {
    method: Method,
    uri: Uri,
    version: Version,
    headers: HeaderMap,
    body: Bytes,
    extensions: Extensions,
    route_params: HashMap<String, String>,
    /// Set by input-rewriting middleware (`TrimStrings`,
    /// `ConvertEmptyStringsToNull`) and by `merge`. When present it *replaces*
    /// the query-plus-body view entirely rather than layering over it — it was
    /// derived from that view, so consulting the originals underneath it would
    /// resurrect exactly the values the rewrite removed.
    input_override: Option<Value>,

    query: OnceLock<Value>,
    parsed_body: OnceLock<ParsedBody>,
    cookies: OnceLock<HashMap<String, String>>,
}

impl Request {
    /// Build a request from its parts. The server calls this; tests should
    /// prefer [`Request::builder`].
    pub fn new(method: Method, uri: Uri, headers: HeaderMap, body: Bytes) -> Self {
        Self {
            method,
            uri,
            version: Version::HTTP_11,
            headers,
            body,
            extensions: Extensions::new(),
            route_params: HashMap::new(),
            input_override: None,
            query: OnceLock::new(),
            parsed_body: OnceLock::new(),
            cookies: OnceLock::new(),
        }
    }

    /// A fluent builder, mainly for tests.
    pub fn builder() -> RequestBuilder {
        RequestBuilder::new()
    }

    /// Adopt an `http::Request` whose body has already been buffered.
    pub fn from_http(request: http::Request<Bytes>) -> Self {
        let (parts, body) = request.into_parts();
        let mut request = Self::new(parts.method, parts.uri, parts.headers, body);
        request.version = parts.version;
        request
    }

    // --- the message ------------------------------------------------------

    /// The HTTP method.
    pub fn method(&self) -> &Method {
        &self.method
    }

    /// The full request URI.
    pub fn uri(&self) -> &Uri {
        &self.uri
    }

    /// The path, without the query string. Always begins with `/`.
    pub fn path(&self) -> &str {
        self.uri.path()
    }

    /// The raw query string, without the `?`.
    pub fn query_string(&self) -> &str {
        self.uri.query().unwrap_or("")
    }

    /// The HTTP version.
    pub fn version(&self) -> Version {
        self.version
    }

    /// Every header.
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    /// Mutable access to the headers — for middleware that normalises them.
    pub fn headers_mut(&mut self) -> &mut HeaderMap {
        &mut self.headers
    }

    /// One header as a string, or `None` if absent or not valid UTF-8.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name)?.to_str().ok()
    }

    /// Replace the method.
    ///
    /// For the one middleware that legitimately needs it: turning a browser
    /// form's `POST` with `_method=DELETE` into a real `DELETE` before routing
    /// sees it. See
    /// [`MethodOverride`](https://docs.rs/rainier-middleware) — and note that
    /// it must run **before** the router, or the route has already been chosen
    /// for the old method.
    pub fn set_method(&mut self, method: Method) {
        self.method = method;
    }

    /// Whether the method is `method`.
    pub fn is_method(&self, method: &Method) -> bool {
        self.method == method
    }

    /// The raw body bytes.
    pub fn body(&self) -> &Bytes {
        &self.body
    }

    /// The body as UTF-8, lossily.
    pub fn body_string(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// The `Content-Type` header, lowercased and without parameters.
    pub fn content_type(&self) -> Option<String> {
        let raw = self.header("content-type")?;
        let base = raw.split(';').next().unwrap_or(raw);
        Some(base.trim().to_ascii_lowercase())
    }

    /// Whether the body claims to be JSON. Also true for `+json` suffixes such
    /// as `application/ld+json`.
    pub fn is_json(&self) -> bool {
        self.content_type().is_some_and(|ct| ct == "application/json" || ct.ends_with("+json"))
    }

    /// Whether the client would rather have JSON back.
    ///
    /// True when it sent JSON, when it asked for JSON in `Accept`, or when it
    /// looks like an XHR. This is what the exception handler consults to decide
    /// between a JSON error body and an HTML error page.
    pub fn expects_json(&self) -> bool {
        if self.is_json() {
            return true;
        }
        if self.header("x-requested-with").is_some_and(|v| v.eq_ignore_ascii_case("XMLHttpRequest"))
        {
            return true;
        }
        match self.header("accept") {
            // `*/*` is what a plain browser navigation and curl both send, so
            // it must not count as "wants JSON".
            Some(accept) => accept.split(',').any(|part| {
                let part = part.split(';').next().unwrap_or(part).trim();
                part == "application/json" || part.ends_with("+json")
            }),
            None => false,
        }
    }

    // --- input ------------------------------------------------------------

    /// The query string parsed into a JSON tree.
    pub fn query(&self) -> &Value {
        self.query.get_or_init(|| input::parse_urlencoded(self.query_string().as_bytes()))
    }

    fn parsed_body(&self) -> &ParsedBody {
        self.parsed_body.get_or_init(|| {
            if self.body.is_empty() {
                return ParsedBody::Opaque;
            }
            match self.content_type().as_deref() {
                Some(ct) if ct == "application/json" || ct.ends_with("+json") => {
                    match serde_json::from_slice(&self.body) {
                        Ok(value) => ParsedBody::Json(value),
                        Err(_) => ParsedBody::Opaque,
                    }
                }
                Some("application/x-www-form-urlencoded") => {
                    ParsedBody::Form(input::parse_urlencoded(&self.body))
                }
                Some("multipart/form-data") => {
                    let boundary = self.header("content-type").and_then(upload::boundary_of);
                    match boundary.and_then(|b| upload::parse(&self.body, &b).ok()) {
                        Some(parsed) => ParsedBody::Multipart(Box::new(parsed)),
                        None => ParsedBody::Opaque,
                    }
                }
                _ => ParsedBody::Opaque,
            }
        })
    }

    /// The body's fields as a JSON tree — `{}` when there is no parseable body.
    pub fn body_input(&self) -> &Value {
        static EMPTY: OnceLock<Value> = OnceLock::new();
        if let Some(overridden) = &self.input_override {
            return overridden;
        }
        match self.parsed_body() {
            ParsedBody::Json(value) => value,
            ParsedBody::Form(value) => value,
            ParsedBody::Multipart(parsed) => &parsed.fields,
            ParsedBody::Opaque => EMPTY.get_or_init(|| Value::Object(serde_json::Map::new())),
        }
    }

    /// Every input: the query string with the body merged over it.
    ///
    /// Route parameters are **not** included — they are part
    /// of the URL's shape rather than user input, and mixing them in would let
    /// a query string shadow a route binding. Read them with
    /// [`route_param`](Self::route_param).
    pub fn all(&self) -> Value {
        match &self.input_override {
            Some(overridden) => overridden.clone(),
            None => input::merge(self.query().clone(), self.body_input().clone()),
        }
    }

    /// One input by dotted key, as a string. Body wins over query string.
    pub fn input(&self, key: &str) -> Option<String> {
        if let Some(overridden) = &self.input_override {
            return input::lookup(overridden, key).and_then(input::scalar_to_string);
        }
        input::lookup(self.body_input(), key)
            .or_else(|| input::lookup(self.query(), key))
            .and_then(input::scalar_to_string)
    }

    /// Rewrite every input through `transform`.
    ///
    /// The seam input-normalising middleware works through: `TrimStrings` and
    /// `ConvertEmptyStringsToNull` both take [`all`](Self::all), reshape it,
    /// and hand it back. Later reads see only the rewritten values.
    pub fn transform_input(&mut self, transform: impl FnOnce(Value) -> Value) {
        self.input_override = Some(transform(self.all()));
    }

    /// Replace every input outright.
    pub fn set_input(&mut self, value: Value) {
        self.input_override = Some(value);
    }

    /// Merge extra values over the current inputs, for a middleware that
    /// resolves something the handler should read as ordinary input.
    pub fn merge_input(&mut self, extra: Value) {
        self.transform_input(|current| input::merge(current, extra));
    }

    /// One input, or `default`.
    pub fn input_or(&self, key: &str, default: impl Into<String>) -> String {
        self.input(key).unwrap_or_else(|| default.into())
    }

    /// The raw JSON value at `key`, for non-scalar inputs.
    pub fn input_value(&self, key: &str) -> Option<&Value> {
        if let Some(overridden) = &self.input_override {
            return input::lookup(overridden, key);
        }
        input::lookup(self.body_input(), key).or_else(|| input::lookup(self.query(), key))
    }

    /// Whether `key` is present at all (even if empty).
    pub fn has(&self, key: &str) -> bool {
        self.input_value(key).is_some()
    }

    /// Whether input-rewriting middleware has replaced the inputs.
    pub fn input_was_rewritten(&self) -> bool {
        self.input_override.is_some()
    }

    /// Whether `key` is present and not empty.
    pub fn filled(&self, key: &str) -> bool {
        match self.input_value(key) {
            Some(Value::String(s)) => !s.trim().is_empty(),
            Some(Value::Null) | None => false,
            Some(Value::Array(items)) => !items.is_empty(),
            Some(Value::Object(map)) => !map.is_empty(),
            Some(_) => true,
        }
    }

    /// Deserialise the JSON body into `T`.
    ///
    /// Fails with a 400 when the body is absent, is not JSON, or does not fit
    /// `T` — unlike [`input`](Self::input), which shrugs at a bad body.
    pub fn json<T: DeserializeOwned>(&self) -> Result<T> {
        if self.body.is_empty() {
            return Err(Error::bad_request("a JSON body is required but the request had none"));
        }
        if !self.is_json() {
            return Err(Error::bad_request(format!(
                "expected a JSON body but the Content-Type was `{}`",
                self.content_type().unwrap_or_else(|| "(none)".into())
            )));
        }
        serde_json::from_slice(&self.body)
            .map_err(|e| Error::bad_request(format!("malformed JSON body: {e}")))
    }

    /// Deserialise the merged inputs into `T`.
    ///
    /// Every value from a form or query string is a string, so `T` should
    /// either use string fields or `#[serde(deserialize_with = ..)]` coercions.
    /// For typed coercion from forms, validate instead.
    pub fn form<T: DeserializeOwned>(&self) -> Result<T> {
        serde_json::from_value(self.all())
            .map_err(|e| Error::bad_request(format!("could not read the request input: {e}")))
    }

    /// Deserialise the query string into `T`.
    pub fn query_as<T: DeserializeOwned>(&self) -> Result<T> {
        serde_json::from_value(self.query().clone())
            .map_err(|e| Error::bad_request(format!("could not read the query string: {e}")))
    }

    // --- files -------------------------------------------------------------

    /// Every file uploaded under `field`.
    pub fn files(&self, field: &str) -> &[UploadedFile] {
        match self.parsed_body() {
            ParsedBody::Multipart(parsed) => {
                parsed.files.get(field).map(Vec::as_slice).unwrap_or(&[])
            }
            _ => &[],
        }
    }

    /// The first file uploaded under `field`.
    pub fn file(&self, field: &str) -> Option<&UploadedFile> {
        self.files(field).first()
    }

    /// Whether a non-empty file arrived under `field`.
    pub fn has_file(&self, field: &str) -> bool {
        self.file(field).is_some_and(|f| !f.is_empty())
    }

    // --- cookies -----------------------------------------------------------

    /// Every cookie the client sent.
    pub fn cookies(&self) -> &HashMap<String, String> {
        self.cookies.get_or_init(|| match self.header("cookie") {
            Some(header) => parse_cookie_header(header),
            None => HashMap::new(),
        })
    }

    /// One cookie's value.
    pub fn cookie(&self, name: &str) -> Option<&str> {
        self.cookies().get(name).map(String::as_str)
    }

    /// The `Authorization: Bearer …` token, if present.
    pub fn bearer_token(&self) -> Option<&str> {
        let header = self.header("authorization")?;
        let (scheme, token) = header.split_once(' ')?;
        scheme.eq_ignore_ascii_case("bearer").then(|| token.trim())
    }

    // --- route parameters --------------------------------------------------

    /// A parameter captured by the matched route (`/posts/{post}`).
    pub fn route_param(&self, name: &str) -> Option<&str> {
        self.route_params.get(name).map(String::as_str)
    }

    /// Every captured route parameter.
    pub fn route_params(&self) -> &HashMap<String, String> {
        &self.route_params
    }

    /// Replace the captured route parameters. The router calls this on match.
    pub fn set_route_params(&mut self, params: HashMap<String, String>) {
        self.route_params = params;
    }

    // --- extensions --------------------------------------------------------

    /// Per-request attributes, keyed by type: the authenticated user, the
    /// matched route, a resolved model binding.
    pub fn extensions(&self) -> &Extensions {
        &self.extensions
    }

    /// Mutable per-request attributes.
    pub fn extensions_mut(&mut self) -> &mut Extensions {
        &mut self.extensions
    }

    /// Store a per-request attribute, returning `self` for chaining.
    pub fn with_extension<T: Send + Sync + 'static>(mut self, value: T) -> Self {
        self.extensions.insert(value);
        self
    }

    /// Read a per-request attribute.
    pub fn extension<T: Send + Sync + 'static>(&self) -> Option<&T> {
        self.extensions.get::<T>()
    }

    /// The client's IP, if the server recorded one.
    pub fn ip(&self) -> Option<std::net::IpAddr> {
        self.extensions.get::<ClientIp>().map(|ip| ip.0)
    }
}

/// A fluent [`Request`] builder.
///
/// ```
/// # use rainier_http::Request;
/// # use http::Method;
/// let request = Request::builder()
///     .method(Method::POST)
///     .uri("/posts?draft=1")
///     .json(&serde_json::json!({ "title": "Hello" }))
///     .build();
///
/// assert_eq!(request.input("title").as_deref(), Some("Hello"));
/// assert_eq!(request.input("draft").as_deref(), Some("1"));
/// ```
#[derive(Debug)]
pub struct RequestBuilder {
    method: Method,
    uri: String,
    headers: HeaderMap,
    body: Bytes,
    route_params: HashMap<String, String>,
}

impl Default for RequestBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl RequestBuilder {
    /// A `GET /` request with no headers or body.
    pub fn new() -> Self {
        Self {
            method: Method::GET,
            uri: "/".to_string(),
            headers: HeaderMap::new(),
            body: Bytes::new(),
            route_params: HashMap::new(),
        }
    }

    /// Set the method.
    pub fn method(mut self, method: Method) -> Self {
        self.method = method;
        self
    }

    /// Set the URI (path and optional query string).
    pub fn uri(mut self, uri: impl Into<String>) -> Self {
        self.uri = uri.into();
        self
    }

    /// Add a header. Ignored if the name or value is not a legal header.
    pub fn header(mut self, name: &str, value: &str) -> Self {
        if let (Ok(name), Ok(value)) =
            (http::header::HeaderName::try_from(name), http::header::HeaderValue::from_str(value))
        {
            self.headers.insert(name, value);
        }
        self
    }

    /// Set a raw body.
    pub fn body(mut self, body: impl Into<Bytes>) -> Self {
        self.body = body.into();
        self
    }

    /// Set a JSON body and the matching `Content-Type`.
    pub fn json(self, value: &impl serde::Serialize) -> Self {
        let encoded = serde_json::to_vec(value).unwrap_or_default();
        self.header("content-type", "application/json").body(encoded)
    }

    /// Set a urlencoded form body and the matching `Content-Type`.
    pub fn form(self, pairs: &[(&str, &str)]) -> Self {
        let encoded: String =
            form_urlencoded::Serializer::new(String::new()).extend_pairs(pairs).finish();
        self.header("content-type", "application/x-www-form-urlencoded").body(encoded)
    }

    /// Pre-set a route parameter, as the router would after matching.
    pub fn route_param(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.route_params.insert(name.into(), value.into());
        self
    }

    /// Build the request.
    pub fn build(self) -> Request {
        let uri: Uri = self.uri.parse().unwrap_or_else(|_| Uri::from_static("/"));
        let mut request = Request::new(self.method, uri, self.headers, self.body);
        request.route_params = self.route_params;
        request
    }
}

#[cfg(test)]
mod input_rewriting_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn transform_input_replaces_what_later_reads_see() {
        let mut request = Request::builder().uri("/x?name=+ada+&age=36").build();
        assert_eq!(request.input("name").as_deref(), Some(" ada "));

        request.transform_input(|value| {
            let mut object = value.as_object().cloned().unwrap_or_default();
            if let Some(Value::String(name)) = object.get_mut("name") {
                *name = name.trim().to_string();
            }
            Value::Object(object)
        });

        assert!(request.input_was_rewritten());
        assert_eq!(request.input("name").as_deref(), Some("ada"));
        assert_eq!(request.all(), json!({ "name": "ada", "age": "36" }));
    }

    #[test]
    fn a_rewrite_is_not_undone_by_the_original_query_string() {
        // Regression guard: if `input()` fell back to the query string after a
        // rewrite, removing a value would be impossible.
        let mut request = Request::builder().uri("/x?secret=leak").build();
        request.set_input(json!({}));
        assert_eq!(request.input("secret"), None);
        assert!(!request.has("secret"));
    }

    #[test]
    fn merge_input_layers_over_the_current_values() {
        let mut request = Request::builder().uri("/x?a=1").build();
        request.merge_input(json!({ "b": "2" }));
        assert_eq!(request.all(), json!({ "a": "1", "b": "2" }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn exposes_the_message_basics() {
        let request = Request::builder().method(Method::POST).uri("/posts?page=2").build();
        assert_eq!(request.method(), Method::POST);
        assert_eq!(request.path(), "/posts");
        assert_eq!(request.query_string(), "page=2");
        assert!(request.is_method(&Method::POST));
    }

    #[test]
    fn reads_query_string_input() {
        let request = Request::builder().uri("/search?q=rust&tags[]=a&tags[]=b").build();
        assert_eq!(request.input("q").as_deref(), Some("rust"));
        assert_eq!(request.input_value("tags"), Some(&json!(["a", "b"])));
    }

    #[test]
    fn reads_json_body_input() {
        let request = Request::builder()
            .method(Method::POST)
            .json(&json!({ "title": "Hi", "meta": { "tag": "rust" } }))
            .build();

        assert_eq!(request.input("title").as_deref(), Some("Hi"));
        assert_eq!(request.input("meta.tag").as_deref(), Some("rust"));
        assert!(request.is_json());
    }

    #[test]
    fn reads_form_body_input() {
        let request = Request::builder()
            .method(Method::POST)
            .form(&[("title", "Hi"), ("body", "there")])
            .build();
        assert_eq!(request.input("title").as_deref(), Some("Hi"));
        assert_eq!(request.input("body").as_deref(), Some("there"));
    }

    #[test]
    fn the_body_wins_over_the_query_string() {
        let request = Request::builder()
            .method(Method::POST)
            .uri("/x?name=query")
            .json(&json!({ "name": "body" }))
            .build();
        assert_eq!(request.input("name").as_deref(), Some("body"));
        assert_eq!(request.all(), json!({ "name": "body" }));
    }

    #[test]
    fn all_merges_query_and_body() {
        let request = Request::builder()
            .method(Method::POST)
            .uri("/x?page=2")
            .json(&json!({ "name": "ada" }))
            .build();
        assert_eq!(request.all(), json!({ "page": "2", "name": "ada" }));
    }

    #[test]
    fn route_params_stay_out_of_input() {
        let request = Request::builder().uri("/posts/7").route_param("post", "7").build();
        assert_eq!(request.route_param("post"), Some("7"));
        assert!(!request.has("post"), "route params must not shadow or appear as input");
    }

    #[test]
    fn has_and_filled_differ_on_blank_values() {
        let request = Request::builder().uri("/x?a=&b=v&c[]=").build();
        assert!(request.has("a"));
        assert!(!request.filled("a"));
        assert!(request.filled("b"));
        assert!(!request.has("missing"));
    }

    #[test]
    fn json_reports_why_it_failed() {
        let empty = Request::builder().build();
        assert!(empty.json::<Value>().unwrap_err().message().contains("had none"));

        let wrong_type = Request::builder().body("hello").build();
        assert!(wrong_type.json::<Value>().unwrap_err().message().contains("Content-Type"));

        let malformed =
            Request::builder().header("content-type", "application/json").body("{oops").build();
        let err = malformed.json::<Value>().unwrap_err();
        assert_eq!(err.status(), 400);
        assert!(err.message().contains("malformed JSON"));
    }

    #[test]
    fn a_malformed_body_does_not_break_unrelated_accessors() {
        let request = Request::builder()
            .header("content-type", "application/json")
            .uri("/x?page=2")
            .body("{oops")
            .build();
        // The handler never looks at the body; reading a query param must work.
        assert_eq!(request.input("page").as_deref(), Some("2"));
        assert_eq!(request.body_input(), &json!({}));
    }

    #[test]
    fn detects_json_content_types_including_suffixes() {
        let plain =
            Request::builder().header("content-type", "application/json; charset=utf-8").build();
        assert!(plain.is_json());
        assert_eq!(plain.content_type().as_deref(), Some("application/json"));

        let suffixed = Request::builder().header("content-type", "application/ld+json").build();
        assert!(suffixed.is_json());

        let html = Request::builder().header("content-type", "text/html").build();
        assert!(!html.is_json());
    }

    #[test]
    fn expects_json_reads_the_accept_header() {
        let asked = Request::builder().header("accept", "application/json").build();
        assert!(asked.expects_json());

        let xhr = Request::builder().header("x-requested-with", "XMLHttpRequest").build();
        assert!(xhr.expects_json());

        let browser = Request::builder()
            .header("accept", "text/html,application/xhtml+xml,*/*;q=0.8")
            .build();
        assert!(!browser.expects_json(), "`*/*` must not count as wanting JSON");

        assert!(!Request::builder().build().expects_json());
    }

    #[test]
    fn reads_cookies() {
        let request = Request::builder().header("cookie", "session=abc; theme=dark").build();
        assert_eq!(request.cookie("session"), Some("abc"));
        assert_eq!(request.cookie("nope"), None);
        assert_eq!(request.cookies().len(), 2);
    }

    #[test]
    fn reads_a_bearer_token() {
        let request = Request::builder().header("authorization", "Bearer tok_123").build();
        assert_eq!(request.bearer_token(), Some("tok_123"));

        // Case-insensitive scheme, per RFC 7235.
        let lower = Request::builder().header("authorization", "bearer tok_123").build();
        assert_eq!(lower.bearer_token(), Some("tok_123"));

        let basic = Request::builder().header("authorization", "Basic abc").build();
        assert_eq!(basic.bearer_token(), None);
        assert_eq!(Request::builder().build().bearer_token(), None);
    }

    #[test]
    fn extensions_carry_typed_attributes() {
        #[derive(Debug, PartialEq)]
        struct CurrentUser(u64);

        let request = Request::builder().build().with_extension(CurrentUser(7));
        assert_eq!(request.extension::<CurrentUser>(), Some(&CurrentUser(7)));
        assert_eq!(request.extension::<ClientIp>(), None);
    }

    #[test]
    fn deserialises_the_json_body_into_a_struct() {
        #[derive(serde::Deserialize, PartialEq, Debug)]
        struct NewPost {
            title: String,
            draft: bool,
        }

        let request = Request::builder()
            .method(Method::POST)
            .json(&json!({ "title": "Hi", "draft": true }))
            .build();
        assert_eq!(request.json::<NewPost>().unwrap(), NewPost { title: "Hi".into(), draft: true });
    }

    #[test]
    fn parses_uploads_from_a_multipart_body() {
        let boundary = "X";
        let body = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"title\"\r\n\r\nHi\r\n\
             --{boundary}\r\nContent-Disposition: form-data; name=\"doc\"; filename=\"a.txt\"\r\n\r\nDATA\r\n\
             --{boundary}--\r\n"
        );
        let request = Request::builder()
            .method(Method::POST)
            .header("content-type", &format!("multipart/form-data; boundary={boundary}"))
            .body(body)
            .build();

        assert_eq!(request.input("title").as_deref(), Some("Hi"));
        assert!(request.has_file("doc"));
        assert_eq!(request.file("doc").unwrap().bytes().as_ref(), b"DATA");
        assert!(!request.has_file("missing"));
        assert!(request.files("missing").is_empty());
    }

    #[test]
    fn parsing_is_cached_across_calls() {
        let request = Request::builder().uri("/x?a=1").build();
        let first = request.query() as *const Value;
        let second = request.query() as *const Value;
        assert_eq!(first, second, "the query string must be parsed once");
    }

    #[test]
    fn adopts_an_http_request() {
        let inner = http::Request::builder()
            .method(Method::PUT)
            .uri("/things/1")
            .header("x-test", "yes")
            .body(Bytes::from("payload"))
            .unwrap();

        let request = Request::from_http(inner);
        assert_eq!(request.method(), Method::PUT);
        assert_eq!(request.path(), "/things/1");
        assert_eq!(request.header("x-test"), Some("yes"));
        assert_eq!(request.body_string(), "payload");
    }
}
