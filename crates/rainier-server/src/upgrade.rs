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

use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::{frame::coding::CloseCode, CloseFrame, Role};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::WebSocketStream;

use rainier_http::{Request, Response, StatusCode};
use rainier_websocket::{Message, Outbound, Socket, SocketId, WebSocketHandler, WebSocketRoutes};

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
                return;
            }
        };

        let stream = WebSocketStream::from_raw_socket(
            hyper_util::rt::TokioIo::new(upgraded),
            Role::Server,
            None,
        )
        .await;

        run(handler, stream, path, params).await;
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
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut writer, mut reader) = stream.split();
    let (outbound, mut queue) = mpsc::unbounded_channel();

    let id = SocketId::next();
    let socket = Socket::new(id, path.as_str(), params, outbound);

    tracing::debug!(socket = %id, path, "socket opened");

    if let Err(e) = handler.on_connect(&socket).await {
        tracing::warn!(socket = %id, path, error = %e, "on_connect failed; closing");
        handler.on_close(&socket).await;
        let _ = writer.close().await;
        return;
    }

    loop {
        tokio::select! {
            // Whatever the handler queued, in the order it queued it.
            outgoing = queue.recv() => {
                let Some(outgoing) = outgoing else { break };
                match outgoing {
                    Outbound::Send(message) => {
                        if writer.send(to_tungstenite(message)).await.is_err() {
                            break;
                        }
                    }
                    Outbound::Close(reason) => {
                        let frame = reason.map(|reason| CloseFrame {
                            code: CloseCode::Normal,
                            reason: reason.into(),
                        });
                        let _ = writer.send(WsMessage::Close(frame)).await;
                        break;
                    }
                }
            }

            incoming = reader.next() => {
                let Some(incoming) = incoming else { break };
                let frame = match incoming {
                    Ok(frame) => frame,
                    Err(e) => {
                        // A client that vanished mid-frame is routine.
                        tracing::debug!(socket = %id, error = %e, "socket read failed");
                        break;
                    }
                };

                match from_tungstenite(frame) {
                    // Ping and pong are the transport's business; tungstenite
                    // has already answered the ping by the time we see it.
                    None => continue,
                    Some(message) => {
                        let closing = message.is_close();
                        if let Err(e) = handler.on_message(&socket, message).await {
                            // Not sent to the client: an error message is
                            // written for you, not for whoever is connected.
                            tracing::warn!(socket = %id, path, error = %e, "on_message failed; closing");
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
    // connection, or a handler error. A registry that only cleaned up on a
    // polite goodbye would leak an entry per closed laptop.
    handler.on_close(&socket).await;
    let _ = writer.close().await;

    tracing::debug!(socket = %id, path, "socket closed");
}

fn to_tungstenite(message: Message) -> WsMessage {
    match message {
        Message::Text(text) => WsMessage::Text(text),
        Message::Binary(bytes) => WsMessage::Binary(bytes),
        Message::Close(reason) => WsMessage::Close(
            reason.map(|reason| CloseFrame { code: CloseCode::Normal, reason: reason.into() }),
        ),
    }
}

/// `None` for the frames a handler should never see.
fn from_tungstenite(message: WsMessage) -> Option<Message> {
    match message {
        WsMessage::Text(text) => Some(Message::Text(text)),
        WsMessage::Binary(bytes) => Some(Message::Binary(bytes)),
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
        assert_eq!(from_tungstenite(WsMessage::Ping(vec![])), None);
        assert_eq!(from_tungstenite(WsMessage::Pong(vec![])), None);
    }

    #[test]
    fn a_close_with_no_reason_stays_a_close() {
        assert_eq!(from_tungstenite(WsMessage::Close(None)), Some(Message::Close(None)));
    }
}
