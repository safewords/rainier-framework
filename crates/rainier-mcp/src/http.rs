//! The Streamable HTTP transport.
//!
//! One path. A client `POST`s each JSON-RPC message (or a batch) to it and
//! gets the answer as `application/json` in the response — or `202
//! Accepted`, with no body, for notifications. The server keeps no session
//! and never starts a stream of its own, so `GET` and `DELETE` answer `405`,
//! which the protocol allows; any replica can answer any request.

use std::future::Future;
use std::sync::Arc;

use rainier_http::{IntoResponse, Method, Request, Response, StatusCode};
use rainier_routing::{Req, Router};
use rainier_support::{BoxFuture, Error};
use subtle::ConstantTimeEq;

use crate::server::{Access, McpServer, SUPPORTED_PROTOCOL_VERSIONS};

/// The application's answer to "may this request use the MCP server?".
pub type AccessCheck = Arc<dyn Fn(&Request) -> BoxFuture<'static, bool> + Send + Sync>;

/// Who may reach the server.
///
/// In order, a request is let in when:
///
/// 1. the environment is `local` and [`open_locally`](Self::open_locally) is
///    on (the default);
/// 2. it carries `Authorization: Bearer <token>` with one of the
///    [`token`](Self::token)s, compared in constant time;
/// 3. it carries the token of a [`read_only_client`](Self::read_only_client),
///    and is let in as that client, with [`Access::ReadOnly`];
/// 4. the application's check says yes.
///
/// Anything else is answered with the router's own 404, as if nothing were
/// there.
#[derive(Clone)]
pub struct McpGate {
    check: AccessCheck,
    tokens: Vec<String>,
    read_only_clients: Vec<(String, String)>,
    open_locally: bool,
}

impl std::fmt::Debug for McpGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpGate")
            .field("tokens", &self.tokens.len())
            .field(
                "read_only_clients",
                &self.read_only_clients.iter().map(|(name, _)| name).collect::<Vec<_>>(),
            )
            .field("open_locally", &self.open_locally)
            .finish_non_exhaustive()
    }
}

impl McpGate {
    /// A gate whose last word is `check`.
    pub fn new(
        check: impl Fn(&Request) -> BoxFuture<'static, bool> + Send + Sync + 'static,
    ) -> Self {
        Self::from_check(Arc::new(check))
    }

    /// A gate from an already-shared check.
    pub fn from_check(check: AccessCheck) -> Self {
        Self { check, tokens: Vec::new(), read_only_clients: Vec::new(), open_locally: true }
    }

    /// Nobody, but for a token or the local environment. The default.
    pub fn deny_all() -> Self {
        Self::new(|_| Box::pin(async { false }))
    }

    /// Everybody, everywhere. For tests, and a server on an internal port.
    pub fn allow_all() -> Self {
        Self::new(|_| Box::pin(async { true }))
    }

    /// Also let in a request bearing `token`. Empty tokens are ignored, so
    /// an unset variable never matches an empty header.
    #[must_use]
    pub fn token(mut self, token: impl Into<String>) -> Self {
        let token = token.into();
        if !token.trim().is_empty() {
            self.tokens.push(token);
        }
        self
    }

    /// Also let in a request bearing `token`, as the client `name`, to read
    /// only: it is shown and may call only the tools marked
    /// [`read_only`](crate::Tool::read_only), and every method it calls is
    /// logged with its name. One token per client, so each can be told apart
    /// and taken away on its own. An empty token is ignored.
    #[must_use]
    pub fn read_only_client(mut self, name: impl Into<String>, token: impl Into<String>) -> Self {
        let token = token.into();
        if !token.trim().is_empty() {
            self.read_only_clients.push((name.into(), token));
        }
        self
    }

    /// Whether everybody is let in when the environment is `local`.
    #[must_use]
    pub fn open_locally(mut self, open: bool) -> Self {
        self.open_locally = open;
        self
    }

    /// Whether `request` may use the server in `environment`.
    pub async fn allows(&self, request: &Request, environment: &str) -> bool {
        self.admit(request, environment).await.is_some()
    }

    /// How much of the server `request` may use in `environment` — `None`
    /// when it may not use it at all.
    pub async fn admit(&self, request: &Request, environment: &str) -> Option<Access> {
        if self.open_locally && environment.eq_ignore_ascii_case("local") {
            return Some(Access::Full);
        }
        if let Some(given) = request.bearer_token() {
            let given = given.as_bytes();
            // Every token is compared, matched or not, so the time taken says
            // nothing about which one — or whether any — was close.
            let full = self
                .tokens
                .iter()
                .fold(false, |found, token| found | bool::from(token.as_bytes().ct_eq(given)));
            let mut client = None;
            for (name, token) in &self.read_only_clients {
                if bool::from(token.as_bytes().ct_eq(given)) && client.is_none() {
                    client = Some(name);
                }
            }
            if full {
                return Some(Access::Full);
            }
            if let Some(name) = client {
                return Some(Access::ReadOnly { client: name.clone() });
            }
        }
        (self.check)(request).await.then_some(Access::Full)
    }
}

/// The server, mounted at a path.
///
/// ```
/// use std::sync::Arc;
/// use rainier_mcp::{McpEndpoint, McpGate, McpServer};
///
/// let server = Arc::new(McpServer::new("app", "1.0.0"));
/// let router = McpEndpoint::new(server)
///     .path("/_mcp")
///     .gate(McpGate::deny_all().token("a-long-random-string"))
///     .environment("production")
///     .routes();
/// # let _ = router;
/// ```
#[derive(Debug, Clone)]
pub struct McpEndpoint {
    server: Arc<McpServer>,
    path: String,
    gate: McpGate,
    environment: String,
    allowed_origins: Vec<String>,
}

impl McpEndpoint {
    /// `server` at `/_mcp`, behind [`McpGate::deny_all`], in production.
    pub fn new(server: Arc<McpServer>) -> Self {
        Self {
            server,
            path: "/_mcp".into(),
            gate: McpGate::deny_all(),
            environment: "production".into(),
            allowed_origins: Vec::new(),
        }
    }

    /// Where it answers.
    #[must_use]
    pub fn path(mut self, path: impl Into<String>) -> Self {
        self.path = path.into();
        self
    }

    /// Who may use it.
    #[must_use]
    pub fn gate(mut self, gate: McpGate) -> Self {
        self.gate = gate;
        self
    }

    /// The application's environment, for [`McpGate::open_locally`].
    #[must_use]
    pub fn environment(mut self, environment: impl Into<String>) -> Self {
        self.environment = environment.into();
        self
    }

    /// Accept requests a browser sends from `origin` (`https://app.example.com`).
    ///
    /// A request carrying an `Origin` header is refused with `403` unless the
    /// origin is listed or is the server's own host — the protocol's defence
    /// against a web page rebinding a name to a local server. Clients that
    /// are not browsers send no `Origin` and are not affected.
    #[must_use]
    pub fn allow_origin(mut self, origin: impl Into<String>) -> Self {
        self.allowed_origins.push(origin.into().trim_end_matches('/').to_string());
        self
    }

    /// The routes, on a new [`Router`] to merge into the application's. The
    /// `POST` route is named `mcp`.
    pub fn routes(self) -> Router {
        let base = rainier_routing::route::normalise_uri(&self.path);
        let state = Arc::new(self);
        let mut router = Router::new();
        router.post(base.as_str(), guarded(&state, post)).name("mcp");
        router.add(
            vec![Method::GET, Method::PUT, Method::PATCH, Method::DELETE, Method::OPTIONS],
            base.as_str(),
            guarded(&state, |_: Arc<McpEndpoint>, _: Req, _: Access| async {
                Response::new(StatusCode::METHOD_NOT_ALLOWED).with_header("allow", "POST")
            }),
        );
        router
    }
}

/// `POST {path}`: one message or a batch in, the answer out.
async fn post(state: Arc<McpEndpoint>, request: Req, access: Access) -> Response {
    if let Some(origin) = request.header("origin") {
        if !origin_allowed(origin, request.header("host"), &state.allowed_origins) {
            return error_response(StatusCode::FORBIDDEN, "This origin may not use the server.");
        }
    }
    if let Some(version) = request.header("mcp-protocol-version") {
        if !SUPPORTED_PROTOCOL_VERSIONS.contains(&version) {
            return error_response(
                StatusCode::BAD_REQUEST,
                &format!("Unsupported MCP-Protocol-Version: {version}"),
            );
        }
    }
    let body = String::from_utf8_lossy(request.body()).into_owned();
    match state.server.handle_str_as(&body, Some(Arc::clone(&request)), &access).await {
        Some(answer) => Response::json(&answer),
        None => Response::new(StatusCode::ACCEPTED),
    }
}

fn error_response(status: StatusCode, message: &str) -> Response {
    Response::json(&crate::protocol::error(
        serde_json::Value::Null,
        crate::protocol::codes::INVALID_REQUEST,
        message,
    ))
    .with_status(status)
}

/// Whether a request from a browser at `origin` may proceed: it is listed, or
/// it names the host the request was sent to.
fn origin_allowed(origin: &str, host: Option<&str>, allowed: &[String]) -> bool {
    let origin = origin.trim_end_matches('/');
    if allowed.iter().any(|a| a.eq_ignore_ascii_case(origin)) {
        return true;
    }
    let origin_host = origin.split_once("://").map_or(origin, |(_, rest)| rest);
    host.is_some_and(|host| host.eq_ignore_ascii_case(origin_host))
}

/// Wrap a handler in the gate and the response headers.
fn guarded<F, Fut, R>(
    state: &Arc<McpEndpoint>,
    handler: F,
) -> impl Fn(Req) -> BoxFuture<'static, Response> + Clone + Send + Sync + 'static
where
    F: Fn(Arc<McpEndpoint>, Req, Access) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = R> + Send + 'static,
    R: IntoResponse,
{
    let state = Arc::clone(state);
    move |request: Req| {
        let state = Arc::clone(&state);
        let handler = handler.clone();
        Box::pin(async move {
            let Some(access) = state.gate.admit(&request, &state.environment).await else {
                tracing::info!(target: "rainier_mcp::gate", "mcp: request refused");
                return Error::not_found(format!("No route matches {}", request.path()))
                    .into_response();
            };
            handler(state, request, access)
                .await
                .into_response()
                .with_header("cache-control", "no-store")
                .with_header("x-content-type-options", "nosniff")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::origin_allowed;

    #[test]
    fn an_origin_is_allowed_when_listed_or_same_host() {
        let listed = vec!["https://console.example.com".to_string()];
        assert!(origin_allowed("https://console.example.com/", None, &listed));
        assert!(origin_allowed("https://app.example.com", Some("app.example.com"), &[]));
        assert!(!origin_allowed("https://evil.example", Some("localhost:8000"), &listed));
        assert!(!origin_allowed("null", Some("localhost:8000"), &[]));
    }
}
