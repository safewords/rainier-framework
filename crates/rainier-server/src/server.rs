//! The [`Server`] — hyper on one side, the [`Kernel`] on the other.

use std::net::SocketAddr;
use std::sync::Arc;

use hyper::body::Incoming;
use hyper::service::service_fn;
use rainier_websocket::WebSocketRoutes;

use crate::upgrade;
use hyper_util::rt::TokioIo;
use rainier_container::{with_facade_application, Application};
use rainier_http::{ClientIp, IntoResponse, Request, Response};
use rainier_support::{Error, Result};
use tokio::net::TcpListener;
use tokio::sync::watch;

use crate::kernel::{read_body, Kernel};

/// How the server behaves.
#[derive(Debug, Clone)]
pub struct ServerOptions {
    /// Where to listen.
    pub address: SocketAddr,
    /// The largest request body to accept, in bytes.
    pub max_body_bytes: usize,
    /// Whether to trust `X-Forwarded-For` for the client address.
    ///
    /// Off by default, and it must stay off unless a proxy you control sets
    /// the header — a client can send whatever it likes, so trusting it
    /// without a proxy in front lets anyone forge their own IP and defeat
    /// rate limiting.
    pub trust_forwarded_for: bool,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            address: SocketAddr::from(([127, 0, 0, 1], 8000)),
            max_body_bytes: 2 * 1024 * 1024,
            trust_forwarded_for: false,
        }
    }
}

impl ServerOptions {
    /// Listen on `address`.
    pub fn bind(mut self, address: SocketAddr) -> Self {
        self.address = address;
        self
    }

    /// Listen on `host:port`.
    pub fn bind_to(mut self, host: &str, port: u16) -> Result<Self> {
        let address = format!("{host}:{port}")
            .parse()
            .map_err(|e| Error::internal(format!("`{host}:{port}` is not an address: {e}")))?;
        self.address = address;
        Ok(self)
    }

    /// Set the body-size limit.
    pub fn max_body_bytes(mut self, bytes: usize) -> Self {
        self.max_body_bytes = bytes;
        self
    }

    /// Trust `X-Forwarded-For`. Only behind a proxy you control.
    pub fn trust_forwarded_for(mut self, trust: bool) -> Self {
        self.trust_forwarded_for = trust;
        self
    }
}

/// Serves a [`Kernel`] over HTTP/1.1.
pub struct Server {
    kernel: Arc<Kernel>,
    options: ServerOptions,
    websockets: Option<Arc<WebSocketRoutes>>,
    /// Scoped around each connection, so a handler's facades resolve through
    /// *this* application. See [`Server::for_application`].
    application: Option<Arc<Application>>,
}

impl Server {
    /// A server for `kernel`, listening on the default address.
    pub fn new(kernel: Kernel) -> Self {
        Self::from_arc(Arc::new(kernel))
    }

    /// A server for a kernel that is already shared — how a console command
    /// serves the same kernel the container holds.
    pub fn from_arc(kernel: Arc<Kernel>) -> Self {
        Self { kernel, options: ServerOptions::default(), websockets: None, application: None }
    }

    /// Resolve facades through `app` while serving.
    ///
    /// Every connection is served in a spawned task, and a spawned task
    /// inherits neither the thread scope nor the task scope of whoever started
    /// the server — so without this a handler's facades resolve through
    /// whatever was installed process-wide.
    ///
    /// For a single application that is the same object and nothing changes.
    /// It matters when there is more than one: two servers in one process, or
    /// a test that booted its own application and then started a real listener
    /// to drive it.
    pub fn for_application(mut self, app: Arc<Application>) -> Self {
        self.application = Some(app);
        self
    }

    /// Configure it.
    pub fn with_options(mut self, options: ServerOptions) -> Self {
        self.options = options;
        self
    }

    /// Serve these WebSocket routes as well.
    ///
    /// On the **same listener and the same port**: a socket connection starts
    /// as an HTTP `GET` asking to upgrade, so there is nothing to run
    /// separately. Without this, an upgrade request falls through to the
    /// router and gets whatever an ordinary `GET` of that path would.
    pub fn with_websockets(mut self, routes: Arc<WebSocketRoutes>) -> Self {
        self.websockets = Some(routes);
        self
    }

    /// The WebSocket routes it will serve, if any.
    pub fn websockets(&self) -> Option<&Arc<WebSocketRoutes>> {
        self.websockets.as_ref()
    }

    /// The address it will bind to.
    pub fn address(&self) -> SocketAddr {
        self.options.address
    }

    /// Serve until the process is asked to stop.
    ///
    /// Shuts down on Ctrl-C, and on `SIGTERM` where the platform has one — the
    /// signal a container runtime sends before it kills you.
    pub async fn run(self) -> Result<()> {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        tokio::spawn(async move {
            wait_for_shutdown_signal().await;
            tracing::info!("shutdown signal received; finishing in-flight requests");
            let _ = shutdown_tx.send(true);
        });

        self.run_until(shutdown_rx).await
    }

    /// Serve until `shutdown` turns true.
    ///
    /// In-flight requests are allowed to finish; new connections are not
    /// accepted. Tests use this to stop a server deterministically.
    pub async fn run_until(self, mut shutdown: watch::Receiver<bool>) -> Result<()> {
        let listener = TcpListener::bind(self.options.address).await.map_err(|e| {
            Error::internal(format!("could not bind {}: {e}", self.options.address))
        })?;

        let bound = listener.local_addr().unwrap_or(self.options.address);
        tracing::info!(address = %bound, "Rainier is listening");

        loop {
            let accepted = tokio::select! {
                accepted = listener.accept() => accepted,
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        break;
                    }
                    continue;
                }
            };

            let (stream, peer) = match accepted {
                Ok(accepted) => accepted,
                Err(e) => {
                    // One bad accept (a fd limit, a client that hung up during
                    // the handshake) must not stop the server.
                    tracing::warn!(error = %e, "failed to accept a connection");
                    continue;
                }
            };

            let kernel = Arc::clone(&self.kernel);
            let options = self.options.clone();
            let websockets = self.websockets.clone();
            let application = self.application.clone();

            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let service = service_fn(move |request: hyper::Request<Incoming>| {
                    let kernel = Arc::clone(&kernel);
                    let options = options.clone();
                    let websockets = websockets.clone();
                    async move { serve_one(kernel, options, websockets, request, peer.ip()).await }
                });

                // `with_upgrades`, or hyper closes the connection after the
                // 101 instead of handing it over — which is what makes a
                // WebSocket share this accept loop rather than needing a
                // second listener.
                let connection = hyper::server::conn::http1::Builder::new()
                    .serve_connection(io, service)
                    .with_upgrades();

                // Scoped **inside** the spawn, not around it: a task local set
                // by the accept loop would not have followed the task here.
                let served = match application {
                    Some(app) => with_facade_application(app, connection).await,
                    None => connection.await,
                };

                if let Err(e) = served {
                    // A client disconnecting mid-response is routine, not an
                    // error worth raising the log level for.
                    tracing::debug!(error = %e, "connection closed");
                }
            });
        }

        tracing::info!("Rainier has stopped accepting connections");
        Ok(())
    }
}

/// Turn one hyper request into one hyper response.
async fn serve_one(
    kernel: Arc<Kernel>,
    options: ServerOptions,
    websockets: Option<Arc<WebSocketRoutes>>,
    request: hyper::Request<Incoming>,
    peer: std::net::IpAddr,
) -> std::result::Result<hyper::Response<rainier_http::Body>, std::convert::Infallible> {
    // An upgrade is handled before the body is read, and it has to be: reading
    // the body consumes the request, and `hyper::upgrade::on` needs the
    // original. A `GET` asking to upgrade has no body to lose.
    if let Some(routes) = websockets {
        if upgrade::is_websocket_upgrade(request.headers()) {
            let parsed = headers_only(&request, options.trust_forwarded_for, peer);
            return Ok(upgrade::handle_upgrade(routes, request, parsed).await);
        }
    }

    let (parts, body) = request.into_parts();

    let bytes = match read_body(body, options.max_body_bytes).await {
        Ok(bytes) => bytes,
        // A body we could not read (too large, or a broken stream) never
        // reaches a route: there is nothing for a handler to act on.
        Err(e) => return Ok(e.into_response().into_http()),
    };

    let mut request = Request::from_http(hyper::Request::from_parts(parts, bytes));

    let client_ip =
        if options.trust_forwarded_for { forwarded_for(&request).unwrap_or(peer) } else { peer };
    request.extensions_mut().insert(ClientIp(client_ip));

    let response: Response = kernel.handle_request(request).await;
    Ok(response.into_http())
}

/// A `Request` carrying everything but the body, for a handshake to inspect.
///
/// The real request cannot be consumed — the upgrade needs it — so this is a
/// copy of the head. A socket handler gets the same headers, cookies and
/// client IP an HTTP handler would, which is what
/// [`authorize`](rainier_websocket::WebSocketHandler::authorize) reads.
fn headers_only(
    request: &hyper::Request<Incoming>,
    trust_forwarded_for: bool,
    peer: std::net::IpAddr,
) -> Request {
    let mut builder = hyper::Request::builder().method(request.method()).uri(request.uri());
    if let Some(headers) = builder.headers_mut() {
        headers.clone_from(request.headers());
    }

    let mut parsed = Request::from_http(
        builder.body(bytes::Bytes::new()).expect("a head with no body is always valid"),
    );

    let client_ip = if trust_forwarded_for { forwarded_for(&parsed).unwrap_or(peer) } else { peer };
    parsed.extensions_mut().insert(ClientIp(client_ip));
    parsed
}

/// The first address in `X-Forwarded-For` — the original client, with each
/// proxy appending itself after it.
fn forwarded_for(request: &Request) -> Option<std::net::IpAddr> {
    request.header("x-forwarded-for")?.split(',').next()?.trim().parse().ok()
}

/// Resolve when the process is asked to stop.
async fn wait_for_shutdown_signal() {
    let interrupt = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut stream) => {
                stream.recv().await;
            }
            // Without SIGTERM we still have Ctrl-C; never returning here lets
            // the select fall through to it.
            Err(_) => std::future::pending::<()>().await,
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = interrupt => {},
        _ = terminate => {},
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_routing::Router;

    fn kernel() -> Kernel {
        let mut router = Router::new();
        router.get("/hello", || async { "world" });
        router.post("/echo", |request: rainier_routing::Req| async move { request.body_string() });
        router.get("/ip", |request: rainier_routing::Req| async move {
            request.ip().map(|ip| ip.to_string()).unwrap_or_else(|| "none".into())
        });

        Kernel::new(router.compile(&rainier_container::Container::new()).expect("compiles"))
    }

    /// Start a server on an ephemeral port and return its address plus a
    /// shutdown handle.
    async fn start() -> (SocketAddr, watch::Sender<bool>) {
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);

        let (tx, rx) = watch::channel(false);
        let server = Server::new(kernel())
            .with_options(ServerOptions::default().bind(address).max_body_bytes(64));

        tokio::spawn(async move {
            let _ = server.run_until(rx).await;
        });

        // Wait for the listener to be up rather than racing the first request.
        for _ in 0..100 {
            if tokio::net::TcpStream::connect(address).await.is_ok() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        (address, tx)
    }

    /// A minimal HTTP/1.1 client: enough to drive the server without pulling
    /// in a client library.
    async fn request(address: SocketAddr, raw: &str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        stream.write_all(raw.as_bytes()).await.unwrap();
        stream.flush().await.unwrap();

        let mut response = Vec::new();
        // The server closes the connection after responding to `Connection:
        // close`, which is how we know the body is complete.
        stream.read_to_end(&mut response).await.unwrap();
        String::from_utf8_lossy(&response).into_owned()
    }

    fn get(path: &str) -> String {
        format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
    }

    #[tokio::test]
    async fn serves_a_route_over_a_real_socket() {
        let (address, shutdown) = start().await;

        let response = request(address, &get("/hello")).await;
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
        assert!(response.ends_with("world"), "{response}");

        let _ = shutdown.send(true);
    }

    #[tokio::test]
    async fn an_unmatched_path_is_a_404() {
        let (address, shutdown) = start().await;

        let response = request(address, &get("/nope")).await;
        assert!(response.starts_with("HTTP/1.1 404"), "{response}");

        let _ = shutdown.send(true);
    }

    #[tokio::test]
    async fn a_request_body_reaches_the_handler() {
        let (address, shutdown) = start().await;

        let response = request(
            address,
            "POST /echo HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
             Content-Length: 5\r\n\r\nhello",
        )
        .await;

        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.ends_with("hello"), "{response}");

        let _ = shutdown.send(true);
    }

    #[tokio::test]
    async fn an_oversized_body_is_refused_with_a_413() {
        let (address, shutdown) = start().await;
        let oversized = "x".repeat(200); // the test server's limit is 64

        let response = request(
            address,
            &format!(
                "POST /echo HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
                 Content-Length: {}\r\n\r\n{oversized}",
                oversized.len()
            ),
        )
        .await;

        assert!(response.starts_with("HTTP/1.1 413"), "{response}");

        let _ = shutdown.send(true);
    }

    #[tokio::test]
    async fn the_client_address_reaches_the_request() {
        let (address, shutdown) = start().await;

        let response = request(address, &get("/ip")).await;
        assert!(response.contains("127.0.0.1"), "{response}");

        let _ = shutdown.send(true);
    }

    #[tokio::test]
    async fn the_server_stops_when_asked() {
        let (address, shutdown) = start().await;
        assert!(request(address, &get("/hello")).await.contains("200"));

        let _ = shutdown.send(true);

        // Give the accept loop a moment to notice, then confirm it is gone.
        for _ in 0..100 {
            if tokio::net::TcpStream::connect(address).await.is_err() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("the server was still accepting connections after shutdown");
    }

    #[test]
    fn forwarded_for_is_not_trusted_by_default() {
        // A client can send whatever it likes; trusting it without a proxy in
        // front would let anyone forge their own address.
        assert!(!ServerOptions::default().trust_forwarded_for);
    }

    #[test]
    fn forwarded_for_reads_the_original_client() {
        let request =
            Request::builder().header("x-forwarded-for", "203.0.113.7, 198.51.100.1").build();

        assert_eq!(
            forwarded_for(&request).map(|ip| ip.to_string()).as_deref(),
            Some("203.0.113.7")
        );
        assert!(forwarded_for(&Request::builder().build()).is_none());
        assert!(
            forwarded_for(&Request::builder().header("x-forwarded-for", "junk").build()).is_none()
        );
    }

    #[test]
    fn options_parse_a_host_and_port() {
        let options = ServerOptions::default().bind_to("0.0.0.0", 3000).unwrap();
        assert_eq!(options.address.to_string(), "0.0.0.0:3000");
        assert!(ServerOptions::default().bind_to("not a host", 1).is_err());
    }
}
