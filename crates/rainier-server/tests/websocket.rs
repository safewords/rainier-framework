//! WebSockets against a real listener, with a real client.
//!
//! The unit tests in `upgrade.rs` check the pieces. They cannot check the one
//! thing that matters — that a client which speaks the protocol can actually
//! connect — and the first version of the handshake here passed every unit
//! test while being unable to complete a single connection.
//!
//! So this file starts the server and connects to it.

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use rainier_routing::Router;
use rainier_server::{Kernel, Server, ServerOptions};
use rainier_support::Result;
use rainier_websocket::{Message, Rooms, Socket, WebSocketHandler, WebSocketRoutes};
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// Echoes what it is sent, prefixed, and greets on connect.
struct Echo;

#[async_trait::async_trait]
impl WebSocketHandler for Echo {
    async fn on_connect(&self, socket: &Socket) -> Result<()> {
        socket.send(format!("welcome to {}", socket.path()))
    }

    async fn on_message(&self, socket: &Socket, message: Message) -> Result<()> {
        match message {
            Message::Text(text) => socket.send(format!("echo: {text}")),
            Message::Binary(bytes) => socket.send(Message::Binary(bytes)),
            Message::Close(_) => Ok(()),
        }
    }
}

/// Reads a route parameter, and refuses anyone without a token.
struct Guarded;

#[async_trait::async_trait]
impl WebSocketHandler for Guarded {
    fn authorize(&self, request: &rainier_http::Request) -> bool {
        request.header("authorization") == Some("Bearer secret")
    }

    async fn on_message(&self, socket: &Socket, _: Message) -> Result<()> {
        socket.send(format!("room {}", socket.param("room").unwrap_or("?")))
    }
}

/// Broadcasts to a room — the reason `Rooms` exists.
struct Chat {
    rooms: Arc<Rooms>,
}

#[async_trait::async_trait]
impl WebSocketHandler for Chat {
    async fn on_connect(&self, socket: &Socket) -> Result<()> {
        self.rooms.join("lobby", socket.clone());
        Ok(())
    }

    async fn on_message(&self, socket: &Socket, message: Message) -> Result<()> {
        self.rooms.send_except("lobby", socket.id(), message);
        Ok(())
    }

    async fn on_close(&self, socket: &Socket) {
        self.rooms.leave_all(socket.id());
    }
}

/// Closes the connection from the server's side on the first frame.
struct Rude;

#[async_trait::async_trait]
impl WebSocketHandler for Rude {
    async fn on_message(&self, socket: &Socket, _: Message) -> Result<()> {
        socket.close_with("that is enough")
    }
}

struct Running {
    port: u16,
    rooms: Arc<Rooms>,
    shutdown: tokio::sync::watch::Sender<bool>,
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
    }
}

/// Start a server on a free port, serving both HTTP and sockets.
async fn start() -> Running {
    let mut router = Router::new();
    router.get("/health", || async { "ok" }).name("health");

    let kernel =
        Kernel::new(router.compile(&rainier_container::Container::new()).expect("compiles"));
    let rooms = Arc::new(Rooms::new());

    let routes = WebSocketRoutes::new()
        .add("/ws/echo", Echo)
        .add("/ws/rooms/{room}", Guarded)
        .add("/ws/chat", Chat { rooms: Arc::clone(&rooms) })
        .add("/ws/rude", Rude);

    // Port 0: the OS picks a free one, so tests can run at the same time.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    drop(listener);

    let server = Server::new(kernel)
        .with_options(ServerOptions {
            address: ([127, 0, 0, 1], port).into(),
            ..Default::default()
        })
        .with_websockets(Arc::new(routes));

    let (shutdown, rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        let _ = server.run_until(rx).await;
    });

    // Wait for the listener rather than sleeping a fixed amount.
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    Running { port, rooms, shutdown }
}

type Client =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(port: u16, path: &str) -> Client {
    let (client, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}{path}"))
        .await
        .expect("the handshake should complete");
    client
}

/// The next text frame, or a panic if one does not arrive.
async fn next_text(client: &mut Client) -> String {
    let frame = tokio::time::timeout(Duration::from_secs(5), client.next())
        .await
        .expect("a frame should arrive")
        .expect("the stream should still be open")
        .expect("the frame should be readable");

    match frame {
        WsMessage::Text(text) => text,
        other => panic!("expected text, got {other:?}"),
    }
}

#[tokio::test]
async fn a_client_can_complete_the_handshake_and_talk() {
    let server = start().await;
    let mut client = connect(server.port, "/ws/echo").await;

    assert_eq!(next_text(&mut client).await, "welcome to /ws/echo");

    client.send(WsMessage::Text("hello".into())).await.expect("send");
    assert_eq!(next_text(&mut client).await, "echo: hello");
}

#[tokio::test]
async fn http_still_works_on_the_same_port_while_a_socket_is_open() {
    // The claim the whole design rests on: one listener, both protocols.
    let server = start().await;
    let mut client = connect(server.port, "/ws/echo").await;
    let _ = next_text(&mut client).await;

    let response = reqwest_get(server.port, "/health").await;

    assert!(response.contains("200 OK"), "{response}");
    assert!(response.contains("ok"), "{response}");

    // And the socket is still usable afterwards.
    client.send(WsMessage::Text("still here".into())).await.expect("send");
    assert_eq!(next_text(&mut client).await, "echo: still here");
}

#[tokio::test]
async fn binary_frames_survive_the_round_trip() {
    let server = start().await;
    let mut client = connect(server.port, "/ws/echo").await;
    let _ = next_text(&mut client).await;

    client.send(WsMessage::Binary(vec![0, 159, 146, 150])).await.expect("send");

    let frame = client.next().await.expect("open").expect("readable");
    assert_eq!(frame, WsMessage::Binary(vec![0, 159, 146, 150]), "not valid UTF-8, on purpose");
}

#[tokio::test]
async fn a_route_parameter_reaches_the_handler() {
    let server = start().await;

    let request =
        http_upgrade_request(server.port, "/ws/rooms/lobby", &[("authorization", "Bearer secret")]);
    let (mut client, _) = tokio_tungstenite::connect_async(request).await.expect("handshake");

    client.send(WsMessage::Text("hi".into())).await.expect("send");
    assert_eq!(next_text(&mut client).await, "room lobby");
}

#[tokio::test]
async fn authorize_runs_before_the_handshake() {
    let server = start().await;

    // No token: the upgrade is refused, so there is no socket to close.
    let attempt =
        tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{}/ws/rooms/lobby", server.port))
            .await;

    assert!(attempt.is_err(), "an unauthorised upgrade should not become a socket");
}

#[tokio::test]
async fn an_unrouted_path_is_a_404_not_a_hanging_socket() {
    let server = start().await;

    let attempt =
        tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{}/ws/nope", server.port)).await;

    assert!(attempt.is_err(), "there is no handler there");
}

#[tokio::test]
async fn one_client_hears_another_through_a_room() {
    let server = start().await;
    let mut first = connect(server.port, "/ws/chat").await;
    let mut second = connect(server.port, "/ws/chat").await;

    // Both have to be registered before the send, and `on_connect` runs on the
    // server's task — so wait for the room to actually hold two.
    for _ in 0..100 {
        if server.rooms.count("lobby") == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(server.rooms.count("lobby"), 2);

    first.send(WsMessage::Text("anyone there?".into())).await.expect("send");

    assert_eq!(next_text(&mut second).await, "anyone there?");
    assert!(
        tokio::time::timeout(Duration::from_millis(300), first.next()).await.is_err(),
        "the sender should not hear their own message back"
    );
}

#[tokio::test]
async fn closing_a_client_empties_the_room() {
    let server = start().await;
    let mut client = connect(server.port, "/ws/chat").await;

    for _ in 0..100 {
        if server.rooms.count("lobby") == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    client.close(None).await.expect("close");
    drop(client);

    // `on_close` runs whatever ended the connection, which is what stops a
    // registry leaking an entry per closed laptop.
    for _ in 0..100 {
        if server.rooms.count("lobby") == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the room still holds a socket that has gone");
}

#[tokio::test]
async fn the_server_can_close_a_connection() {
    let server = start().await;
    let mut client = connect(server.port, "/ws/rude").await;

    client.send(WsMessage::Text("hello".into())).await.expect("send");

    let frame = client.next().await.expect("open").expect("readable");
    match frame {
        WsMessage::Close(Some(frame)) => assert_eq!(frame.reason, "that is enough"),
        other => panic!("expected a close frame, got {other:?}"),
    }
}

/// A one-shot HTTP request, so a test can prove the port still serves HTTP
/// without pulling in an HTTP client.
async fn reqwest_get(port: u16, path: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.expect("connect");
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .expect("write");

    let mut response = String::new();
    stream.read_to_string(&mut response).await.expect("read");
    response
}

/// An upgrade request carrying extra headers, which `connect_async` cannot do
/// from a URL alone.
fn http_upgrade_request(
    port: u16,
    path: &str,
    headers: &[(&str, &str)],
) -> tokio_tungstenite::tungstenite::handshake::client::Request {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let mut request =
        format!("ws://127.0.0.1:{port}{path}").into_client_request().expect("a valid request");

    for (name, value) in headers {
        request.headers_mut().insert(
            http::header::HeaderName::from_bytes(name.as_bytes()).expect("a valid header name"),
            value.parse().expect("a valid header value"),
        );
    }
    request
}
