//! The WebSocket upgrade — the transport half of
//! [`rainier-websocket`](rainier_websocket).
//!
//! A WebSocket connection begins as an ordinary HTTP `GET` that asks to stop
//! being one. This module recognises that request, answers `101`, and then
//! hands the raw connection to a [`WebSocketHandler`] as a stream of
//! [`Message`]s.
//!
//! ```text
//! GET /ws/rooms/7          hyper accepts it like any request
//!   Upgrade: websocket
//!   Sec-WebSocket-Key: …
//!        │
//!        ▼
//!   101 Switching Protocols        ← this module writes it
//!   Sec-WebSocket-Accept: …          (SHA-1 of the key + the protocol's GUID)
//!        │
//!        ▼
//!   hyper::upgrade::on(request)    ← the socket, once hyper has flushed the 101
//!        │
//!        ▼
//!   on_connect → on_message* → on_close
//! ```
//!
//! It shares the accept loop with HTTP because it *is* the accept loop: the
//! upgrade happens on a connection hyper already accepted, in the task hyper
//! already spawned for it.

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::error::ProtocolError;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::{
    frame::coding::CloseCode, CloseFrame, Role, WebSocketConfig,
};
use tokio_tungstenite::tungstenite::{Error as WsError, Message as WsMessage};
use tokio_tungstenite::WebSocketStream;

use rainier_events::report::{report, WebSocketFailed};
use rainier_events::Exception;
use rainier_http::{Request, Response, StatusCode};
use rainier_websocket::{
    is_disconnect, Message, Outbound, Socket, SocketId, WebSocketHandler, WebSocketRoutes,
};

/// How the server tells a live connection from one whose client has gone.
///
/// A client that leaves without a close frame — a laptop lid, a phone moving
/// between networks, a NAT that forgot the mapping — leaves a TCP connection
/// that still looks open from here. Nothing fails until a write has sat
/// unacknowledged long enough for the kernel to give up, which is many
/// minutes, and every send in the meantime is queued for nobody.
///
/// So the server pings any connection it has not heard from for
/// [`ping_every`](Self::ping_every), and closes one that has been silent —
/// no frame, not even the pong every client sends back on its own — for
/// [`give_up_after`](Self::give_up_after). A write that cannot complete in
/// that time ends the connection too.
#[derive(Debug, Clone, Copy)]
pub struct Liveness {
    /// How long a connection may be quiet before the server pings it.
    pub ping_every: Duration,
    /// How long a connection may be silent before it is taken as gone.
    pub give_up_after: Duration,
}

impl Default for Liveness {
    /// Ping after 25 s of quiet; give up after 70 s of silence — two missed
    /// pongs and some slack. Under the 60 s idle timeout most proxies and
    /// load balancers apply, so the pings keep those from cutting a quiet
    /// but healthy connection.
    fn default() -> Self {
        Self { ping_every: Duration::from_secs(25), give_up_after: Duration::from_secs(70) }
    }
}

/// The bytes each connection reserves for reading before any frame arrives.
///
/// tungstenite allocates its read buffer up front at this size and grows it
/// only when a frame needs more. Its default is 128 KiB, so a server holding
/// idle connections spent 128 KiB on each one before reading a byte. Most
/// socket traffic is small JSON frames, so this starts at 4 KiB. A larger
/// message still arrives whole: the buffer grows to fit it.
pub const INITIAL_READ_BUFFER: usize = 4 * 1024;

/// The transport settings every upgraded connection runs with.
///
/// Only the initial read buffer differs from tungstenite's defaults. The
/// message and frame size limits are unchanged.
pub fn socket_config() -> WebSocketConfig {
    WebSocketConfig::default().read_buffer_size(INITIAL_READ_BUFFER)
}

/// Whether this request is asking to become a WebSocket.
///
/// Both headers, because either alone is something else: `Connection: Upgrade`
/// with no protocol is an HTTP/2 cleartext attempt, and `Upgrade: websocket`
/// without `Connection` is a client that has misread the RFC. Both are
/// compared case-insensitively, and `Connection` may be a list.
pub fn is_websocket_upgrade(headers: &hyper::HeaderMap) -> bool {
    let header =
        |name: &str| headers.get(name).and_then(|value| value.to_str().ok()).unwrap_or_default();

    let connection =
        header("connection").split(',').any(|token| token.trim().eq_ignore_ascii_case("upgrade"));

    connection && header("upgrade").eq_ignore_ascii_case("websocket")
}

/// The `Sec-WebSocket-Accept` value for a client's `Sec-WebSocket-Key`.
///
/// SHA-1 of the key concatenated with a constant from RFC 6455, base64'd. The
/// constant is there so a server that merely echoes the key cannot be mistaken
/// for one that speaks the protocol.
///
/// **Delegated, not reimplemented.** Six lines, and the first version of them
/// here transcribed the constant wrong — one character moved from the start of
/// the last group to the end. It hashed cleanly, it looked right, and every
/// browser would have refused the connection. There is no reason to own this
/// when the library doing the framing already has it.
pub fn accept_key(client_key: &str) -> String {
    derive_accept_key(client_key.as_bytes())
}

/// Answer an upgrade request: route it, authorise it, and take the connection.
///
/// Returns the response hyper should send. When that response is `101`, a task
/// has been spawned that waits for hyper to finish with the connection and
/// then runs the handler over it.
pub async fn handle_upgrade(
    routes: Arc<WebSocketRoutes>,
    mut request: hyper::Request<hyper::body::Incoming>,
    parsed: Request,
) -> hyper::Response<rainier_http::Body> {
    let path = parsed.uri().path().to_string();

    let Some((handler, params)) = routes.match_path(&path) else {
        // No socket route here. A 404 rather than a protocol error: the client
        // asked for a path that does not exist, which is the ordinary meaning.
        return Response::new(StatusCode::NOT_FOUND).into_http();
    };

    if !handler.authorize(&parsed) {
        return Response::new(StatusCode::FORBIDDEN).into_http();
    }

    // The version is checked before the key, because a client speaking an
    // older draft would otherwise get a 101 and then a stream of frames
    // neither side can read.
    let version = parsed.header("sec-websocket-version").unwrap_or_default();
    if version != "13" {
        return Response::new(StatusCode::BAD_REQUEST)
            .with_header("sec-websocket-version", "13")
            .with_body("this server speaks WebSocket version 13.")
            .into_http();
    }

    let Some(key) = parsed.header("sec-websocket-key") else {
        return Response::new(StatusCode::BAD_REQUEST)
            .with_body("no Sec-WebSocket-Key.")
            .into_http();
    };
    let accept = accept_key(key);

    // Taking the upgrade *before* returning: hyper resolves this future once
    // it has written the 101 and stopped using the connection.
    let upgraded = hyper::upgrade::on(&mut request);

    tokio::spawn(async move {
        let upgraded = match upgraded.await {
            Ok(upgraded) => upgraded,
            Err(e) => {
                tracing::debug!(error = %e, path, "the connection never finished upgrading");
                // A client that went away between asking and the 101 —
                // navigated off, lost its network — is not a failure.
                if !(e.is_closed() || e.is_canceled() || e.is_incomplete_message()) {
                    // No socket yet, so no id.
                    report(WebSocketFailed::new(Exception::from_std(&e), "", path, "upgrade"))
                        .await;
                }
                return;
            }
        };

        let stream = WebSocketStream::from_raw_socket(
            hyper_util::rt::TokioIo::new(upgraded),
            Role::Server,
            Some(socket_config()),
        )
        .await;

        run(handler, stream, path, params, Liveness::default()).await;
    });

    Response::new(StatusCode::SWITCHING_PROTOCOLS)
        .with_header("connection", "Upgrade")
        .with_header("upgrade", "websocket")
        .with_header("sec-websocket-accept", &accept)
        .into_http()
}

/// Drive one connection: read frames, write frames, and call the handler.
///
/// Reading and writing are one task with a `select!` rather than two, so the
/// socket's two halves cannot outlive each other — a writer still running
/// after the reader has gone is a task that never ends.
async fn run<S>(
    handler: Arc<dyn WebSocketHandler>,
    stream: WebSocketStream<S>,
    path: String,
    params: Vec<(String, String)>,
    liveness: Liveness,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut writer, mut reader) = stream.split();
    let (outbound, mut queue) = mpsc::unbounded_channel();

    let id = SocketId::next();
    let socket = Socket::new(id, path.as_str(), params, outbound);

    tracing::debug!(socket = %id, path, "socket opened");

    let connected = handler.on_connect(&socket);
    // Connect at `INFO`; each message at `DEBUG`, because a chatty socket is
    // hundreds of them a minute. Never the socket's path or a message's
    // contents — the kind and the size.
    #[cfg(feature = "telemetry")]
    let connected = tracing::Instrument::instrument(
        connected,
        rainier_support::span!("websocket.connect", otel.name = "websocket connect"),
    );

    let started = std::time::Instant::now();
    let connected = connected.await;
    rainier_events::activity::publish_activity_with(|| {
        rainier_websocket::WebSocketConnected::new(started.elapsed(), &connected)
    });
    if let Err(e) = connected {
        if is_disconnect(&e) {
            tracing::debug!(socket = %id, path, "the client left during on_connect");
        } else {
            tracing::warn!(socket = %id, path, error = %e, "on_connect failed; closing");
            report(WebSocketFailed::new(e, id.to_string(), path.as_str(), "connect")).await;
        }
        handler.on_close(&socket).await;
        let _ = writer.close().await;
        return;
    }

    // A write that cannot finish in this long is to a client that has gone:
    // its receive window is full and nothing is reading it.
    macro_rules! write {
        ($frame:expr) => {
            matches!(
                tokio::time::timeout(liveness.give_up_after, writer.send($frame)).await,
                Ok(Ok(()))
            )
        };
    }

    let mut last_heard = tokio::time::Instant::now();
    let mut ticker =
        tokio::time::interval_at(last_heard + liveness.ping_every, liveness.ping_every);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            // Liveness: ping a quiet connection, and let a silent one go.
            _ = ticker.tick() => {
                let quiet = last_heard.elapsed();
                if quiet >= liveness.give_up_after {
                    tracing::debug!(socket = %id, path, ?quiet, "the client stopped answering; closing");
                    break;
                }
                if quiet >= liveness.ping_every && !write!(WsMessage::Ping(Default::default())) {
                    break;
                }
            }

            // Whatever the handler queued, in the order it queued it.
            outgoing = queue.recv() => {
                let Some(outgoing) = outgoing else { break };
                match outgoing {
                    Outbound::Send(message) => {
                        if !write!(to_tungstenite(message)) {
                            tracing::debug!(socket = %id, path, "a write did not reach the client; closing");
                            break;
                        }
                    }
                    Outbound::Close(reason) => {
                        let frame = reason.map(|reason| CloseFrame {
                            code: CloseCode::Normal,
                            reason: reason.into(),
                        });
                        let _ = write!(WsMessage::Close(frame));
                        break;
                    }
                }
            }

            incoming = reader.next() => {
                let Some(incoming) = incoming else { break };
                let frame = match incoming {
                    Ok(frame) => frame,
                    Err(e) => {
                        // A client that vanished — closed, reset, gone
                        // without a closing handshake — is how connections
                        // end, not a failure. Anything else is reported:
                        // whether it matters is the application's call.
                        tracing::debug!(socket = %id, error = %e, "socket read failed");
                        if !is_routine_disconnect(&e) {
                            report(WebSocketFailed::new(
                                Exception::from_std(&e),
                                id.to_string(),
                                path.as_str(),
                                "read",
                            ))
                            .await;
                        }
                        break;
                    }
                };
                // Any frame at all, a pong included, says the client is there.
                last_heard = tokio::time::Instant::now();

                match from_tungstenite(frame) {
                    // Ping and pong are the transport's business; tungstenite
                    // has already answered the ping by the time we see it.
                    None => continue,
                    Some(message) => {
                        let closing = message.is_close();
                        #[cfg(feature = "telemetry")]
                        let span = tracing::debug_span!(
                            "websocket.message",
                            otel.name = "websocket message",
                            rainier.websocket.kind = match &message {
                                rainier_websocket::Message::Text(_) => "text",
                                rainier_websocket::Message::Binary(_) => "binary",
                                rainier_websocket::Message::Close(_) => "close",
                            },
                            rainier.websocket.bytes = match &message {
                                rainier_websocket::Message::Text(text) => text.len(),
                                rainier_websocket::Message::Binary(bytes) => bytes.len(),
                                rainier_websocket::Message::Close(_) => 0,
                            },
                        );
                        // Before the message moves into the handler.
                        let shape = rainier_websocket::WebSocketMessageHandled::shape(&message);
                        let started = std::time::Instant::now();
                        let handled = handler.on_message(&socket, message);
                        #[cfg(feature = "telemetry")]
                        let handled = tracing::Instrument::instrument(handled, span);
                        let handled = handled.await;
                        rainier_events::activity::publish_activity_with(|| {
                            rainier_websocket::WebSocketMessageHandled::new(
                                shape,
                                started.elapsed(),
                                &handled,
                            )
                        });
                        if let Err(e) = handled {
                            if is_disconnect(&e) {
                                // A send found the client gone; the handler
                                // passed that up. The connection has ended.
                                tracing::debug!(socket = %id, path, "the client left mid-message");
                            } else {
                                // Not sent to the client: an error message is
                                // written for you, not for whoever is connected.
                                tracing::warn!(socket = %id, path, error = %e, "on_message failed; closing");
                                report(WebSocketFailed::new(e, id.to_string(), path.as_str(), "message")).await;
                            }
                            break;
                        }
                        if closing {
                            break;
                        }
                    }
                }
            }
        }
    }

    // Exactly once, however the loop ended — a clean close, a dropped
    // connection, a silent one, or a handler error. A registry that only
    // cleaned up on a polite goodbye would leak an entry per closed laptop.
    handler.on_close(&socket).await;
    let _ = tokio::time::timeout(liveness.give_up_after, writer.close()).await;

    tracing::debug!(socket = %id, path, "socket closed");
}

/// Whether a read error is just the client going away.
///
/// A close, a reset, a connection dropped without the closing handshake: the
/// ordinary ways a browser tab, a sleeping laptop or a phone between networks
/// ends a connection. None of them is something the server did wrong.
fn is_routine_disconnect(error: &WsError) -> bool {
    use std::io::ErrorKind as Io;

    match error {
        WsError::ConnectionClosed | WsError::AlreadyClosed => true,
        WsError::Protocol(ProtocolError::ResetWithoutClosingHandshake) => true,
        WsError::Io(io) => matches!(
            io.kind(),
            Io::ConnectionReset
                | Io::ConnectionAborted
                | Io::BrokenPipe
                | Io::UnexpectedEof
                | Io::TimedOut
        ),
        _ => false,
    }
}

fn to_tungstenite(message: Message) -> WsMessage {
    match message {
        Message::Text(text) => WsMessage::Text(text.into()),
        Message::Binary(bytes) => WsMessage::Binary(bytes.into()),
        Message::Close(reason) => WsMessage::Close(
            reason.map(|reason| CloseFrame { code: CloseCode::Normal, reason: reason.into() }),
        ),
    }
}

/// `None` for the frames a handler should never see.
fn from_tungstenite(message: WsMessage) -> Option<Message> {
    match message {
        WsMessage::Text(text) => Some(Message::Text(text.to_string())),
        WsMessage::Binary(bytes) => Some(Message::Binary(bytes.to_vec())),
        WsMessage::Close(frame) => {
            Some(Message::Close(frame.map(|frame| frame.reason.to_string())))
        }
        // Keep-alive, answered by the library. A handler that had to reply to
        // a ping would have a connection that dies when it forgets.
        WsMessage::Ping(_) | WsMessage::Pong(_) => None,
        // A continuation frame reaching here would mean the library failed to
        // reassemble a fragmented message, which it does not.
        WsMessage::Frame(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> hyper::HeaderMap {
        let mut headers = hyper::HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(
                hyper::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
        }
        headers
    }

    #[test]
    fn an_upgrade_needs_both_headers() {
        assert!(is_websocket_upgrade(&headers(&[
            ("connection", "Upgrade"),
            ("upgrade", "websocket"),
        ])));

        // `Connection: Upgrade` alone is an HTTP/2 cleartext attempt, and
        // `Upgrade: websocket` alone is a client that misread the RFC.
        assert!(!is_websocket_upgrade(&headers(&[("connection", "Upgrade")])));
        assert!(!is_websocket_upgrade(&headers(&[("upgrade", "websocket")])));
        assert!(!is_websocket_upgrade(&headers(&[])));
    }

    #[test]
    fn the_headers_are_matched_the_way_browsers_send_them() {
        // Firefox sends `keep-alive, Upgrade`; the casing varies everywhere.
        assert!(is_websocket_upgrade(&headers(&[
            ("connection", "keep-alive, Upgrade"),
            ("upgrade", "WebSocket"),
        ])));
    }

    #[test]
    fn the_accept_key_is_the_one_from_the_rfc() {
        // The pair RFC 6455 §1.3 works through. This assertion earned its
        // place immediately: the first implementation of `accept_key` here
        // mistyped the protocol's constant, and this is what caught it.
        assert_eq!(accept_key("dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }

    #[test]
    fn every_frame_kind_maps_both_ways() {
        for message in [
            Message::text("hi"),
            Message::binary(vec![1, 2, 3]),
            Message::Close(Some("bye".into())),
        ] {
            let round_tripped = from_tungstenite(to_tungstenite(message.clone()));
            assert_eq!(round_tripped, Some(message));
        }
    }

    #[test]
    fn keep_alive_frames_never_reach_a_handler() {
        assert_eq!(from_tungstenite(WsMessage::Ping(Default::default())), None);
        assert_eq!(from_tungstenite(WsMessage::Pong(Default::default())), None);
    }

    #[test]
    fn a_close_with_no_reason_stays_a_close() {
        assert_eq!(from_tungstenite(WsMessage::Close(None)), Some(Message::Close(None)));
    }

    struct Refuses;

    #[async_trait::async_trait]
    impl WebSocketHandler for Refuses {
        async fn on_message(&self, _: &Socket, _: Message) -> rainier_support::Result<()> {
            Err(rainier_support::Error::internal("upgrade-test: on_message refused"))
        }
    }

    #[tokio::test]
    async fn a_failing_on_message_is_reported_as_a_websocket_failure() {
        use rainier_events::report::{capture, Failure};

        let capture = capture();
        let (server, client) = tokio::io::duplex(4096);
        let server = WebSocketStream::from_raw_socket(server, Role::Server, None).await;
        let mut client = WebSocketStream::from_raw_socket(client, Role::Client, None).await;

        let running = tokio::spawn(run(
            Arc::new(Refuses),
            server,
            "/ws/upgrade-test".to_string(),
            Vec::new(),
            Liveness::default(),
        ));
        client.send(WsMessage::Text("hello".into())).await.unwrap();
        running.await.unwrap();

        let ours: Vec<_> = capture
            .reports()
            .into_iter()
            .filter_map(|reported| match reported.failure {
                Failure::WebSocket(failed) if failed.path == "/ws/upgrade-test" => Some(failed),
                _ => None,
            })
            .collect();
        assert_eq!(ours.len(), 1, "{ours:?}");
        assert_eq!(ours[0].stage, "message");
        assert!(!ours[0].socket.is_empty());
        assert_eq!(ours[0].exception.message, "upgrade-test: on_message refused");
    }

    /// Counts closes; answers a message by failing the way a send to a
    /// departed client does.
    #[derive(Default)]
    struct Watches {
        closed: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl WebSocketHandler for Watches {
        async fn on_message(&self, _: &Socket, _: Message) -> rainier_support::Result<()> {
            Err(rainier_support::Error::new(
                rainier_support::ErrorKind::Status(rainier_websocket::socket::CLIENT_CLOSED),
                "socket 1 has gone away",
            ))
        }

        async fn on_close(&self, _: &Socket) {
            self.closed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    fn quick() -> Liveness {
        Liveness {
            ping_every: Duration::from_millis(40),
            give_up_after: Duration::from_millis(150),
        }
    }

    fn reports_for(capture: &rainier_events::report::Capture, path: &str) -> usize {
        use rainier_events::report::Failure;
        capture
            .reports()
            .into_iter()
            .filter(|reported| matches!(&reported.failure, Failure::WebSocket(f) if f.path == path))
            .count()
    }

    #[tokio::test]
    async fn a_client_that_stops_answering_is_closed_and_not_reported() {
        let capture = rainier_events::report::capture();
        let (server, client) = tokio::io::duplex(4096);
        let server = WebSocketStream::from_raw_socket(server, Role::Server, None).await;
        // Never read, so never answers a ping: a laptop with its lid shut.
        let _client = WebSocketStream::from_raw_socket(client, Role::Client, None).await;

        let handler = Arc::new(Watches::default());
        let finished = tokio::time::timeout(
            Duration::from_secs(5),
            run(handler.clone(), server, "/ws/silent".into(), Vec::new(), quick()),
        )
        .await;

        assert!(finished.is_ok(), "the server should have let the silent client go");
        assert_eq!(handler.closed.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(reports_for(&capture, "/ws/silent"), 0, "silence is not a failure");
    }

    #[tokio::test]
    async fn a_client_that_answers_pings_stays_connected() {
        let (server, client) = tokio::io::duplex(4096);
        let server = WebSocketStream::from_raw_socket(server, Role::Server, None).await;
        let mut client = WebSocketStream::from_raw_socket(client, Role::Client, None).await;

        // Reading is what makes a client answer pings, as a browser does.
        let pings = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = pings.clone();
        tokio::spawn(async move {
            while let Some(Ok(frame)) = client.next().await {
                if frame.is_ping() {
                    seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
            }
        });

        let handler = Arc::new(Watches::default());
        let running =
            tokio::spawn(run(handler.clone(), server, "/ws/alive".into(), Vec::new(), quick()));

        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(!running.is_finished(), "a client answering pings is alive");
        assert!(pings.load(std::sync::atomic::Ordering::SeqCst) >= 3, "the server pinged it");
        assert_eq!(handler.closed.load(std::sync::atomic::Ordering::SeqCst), 0);
        running.abort();
    }

    #[tokio::test]
    async fn a_send_to_a_departed_client_is_not_a_failure() {
        let capture = rainier_events::report::capture();
        let (server, client) = tokio::io::duplex(4096);
        let server = WebSocketStream::from_raw_socket(server, Role::Server, None).await;
        let mut client = WebSocketStream::from_raw_socket(client, Role::Client, None).await;

        let handler = Arc::new(Watches::default());
        let running = tokio::spawn(run(
            handler.clone(),
            server,
            "/ws/departed".into(),
            Vec::new(),
            Liveness::default(),
        ));
        client.send(WsMessage::Text("hello".into())).await.unwrap();
        running.await.unwrap();

        assert_eq!(handler.closed.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(reports_for(&capture, "/ws/departed"), 0);
    }

    #[tokio::test]
    async fn a_client_that_drops_the_connection_is_not_a_failure() {
        let capture = rainier_events::report::capture();
        let (server, client) = tokio::io::duplex(4096);
        let server = WebSocketStream::from_raw_socket(server, Role::Server, None).await;

        let handler = Arc::new(Watches::default());
        let running = tokio::spawn(run(
            handler.clone(),
            server,
            "/ws/dropped".into(),
            Vec::new(),
            Liveness::default(),
        ));
        // Gone without a close frame.
        drop(client);
        running.await.unwrap();

        assert_eq!(handler.closed.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(reports_for(&capture, "/ws/dropped"), 0);
    }

    #[test]
    fn routine_disconnects_are_told_apart_from_faults() {
        use std::io::{Error as IoError, ErrorKind as Io};

        assert!(is_routine_disconnect(&WsError::ConnectionClosed));
        assert!(is_routine_disconnect(&WsError::Protocol(
            ProtocolError::ResetWithoutClosingHandshake
        )));
        assert!(is_routine_disconnect(&WsError::Io(IoError::from(Io::ConnectionReset))));
        assert!(is_routine_disconnect(&WsError::Io(IoError::from(Io::BrokenPipe))));
        assert!(!is_routine_disconnect(&WsError::Utf8(String::new())));
        assert!(!is_routine_disconnect(&WsError::Io(IoError::from(Io::PermissionDenied))));
    }

    /// Remembers the size of every text message it is handed.
    #[derive(Default)]
    struct Measures {
        sizes: std::sync::Mutex<Vec<usize>>,
    }

    #[async_trait::async_trait]
    impl WebSocketHandler for Measures {
        async fn on_message(&self, _: &Socket, message: Message) -> rainier_support::Result<()> {
            if let Message::Text(text) = message {
                self.sizes.lock().unwrap().push(text.len());
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_message_larger_than_the_initial_read_buffer_arrives_whole() {
        // The read buffer starts small so idle connections cost little. A
        // message bigger than it must still be read in one piece.
        let big = INITIAL_READ_BUFFER * 50;
        let (server, client) = tokio::io::duplex(4096);
        let server =
            WebSocketStream::from_raw_socket(server, Role::Server, Some(socket_config())).await;
        let mut client = WebSocketStream::from_raw_socket(client, Role::Client, None).await;

        let handler = Arc::new(Measures::default());
        let running = tokio::spawn(run(
            handler.clone(),
            server,
            "/ws/large".into(),
            Vec::new(),
            Liveness::default(),
        ));
        client.send(WsMessage::Text("x".repeat(big).into())).await.unwrap();
        client.send(WsMessage::Text("small".into())).await.unwrap();
        client.close(None).await.unwrap();
        running.await.unwrap();

        assert_eq!(*handler.sizes.lock().unwrap(), vec![big, 5]);
    }

    #[tokio::test]
    async fn connect_and_each_message_are_published_as_activities() {
        use rainier_events::Dispatcher;

        let events = Arc::new(Dispatcher::new());
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        events.on_activity("test", move |activity, _| {
            let fields = activity.collect_fields();
            sink.lock().unwrap().push(format!(
                "{} {} {}",
                activity.kind(),
                fields.text("rainier.websocket.kind").unwrap_or("-"),
                fields.get("rainier.websocket.bytes").map(|v| v.render()).unwrap_or_default(),
            ));
            Ok(())
        });
        // This thread's bus: the test runtime runs the spawned connection here.
        let _bus = rainier_events::set_default(Arc::clone(&events));

        let (server, client) = tokio::io::duplex(4096);
        let server =
            WebSocketStream::from_raw_socket(server, Role::Server, Some(socket_config())).await;
        let mut client = WebSocketStream::from_raw_socket(client, Role::Client, None).await;
        let running = tokio::spawn(run(
            Arc::new(Measures::default()),
            server,
            "/ws/events".into(),
            Vec::new(),
            Liveness::default(),
        ));
        client.send(WsMessage::Text("hello".into())).await.unwrap();
        client.close(None).await.unwrap();
        running.await.unwrap();
        events.settled().await;

        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen[0], "websocket.connect - ");
        assert_eq!(seen[1], "websocket.message text 5");
    }

    #[test]
    fn upgraded_connections_reserve_a_small_read_buffer() {
        let config = socket_config();
        assert_eq!(config.read_buffer_size, INITIAL_READ_BUFFER);
        // Everything else stays at tungstenite's defaults.
        let defaults = WebSocketConfig::default();
        assert_eq!(config.max_message_size, defaults.max_message_size);
        assert_eq!(config.max_frame_size, defaults.max_frame_size);
        assert_eq!(config.write_buffer_size, defaults.write_buffer_size);
    }
}
