//! A served request resolves facades through the server's own application.
//!
//! Its own binary because it installs a process-wide application and then
//! asserts that a handler ignores it — a neighbouring test installing another
//! would make that meaningless.

use std::net::SocketAddr;
use std::sync::Arc;

use rainier_container::{facade_application, set_facade_application, Application};
use rainier_http::Response;
use rainier_routing::Router;
use rainier_server::{Kernel, Server, ServerOptions};

/// A marker resolved from whichever container the handler ends up in.
struct Name(&'static str);

fn application(name: &'static str) -> Arc<Application> {
    let app = Arc::new(Application::new("."));
    app.instance(Name(name));
    app
}

/// One route, answering with the name of the application it resolved through.
fn kernel() -> Kernel {
    let mut router = Router::new();
    router.get("/whose", || async {
        // A facade-style resolution: no request, no container argument, just
        // the ambient application. Which is exactly the thing that goes wrong
        // silently inside a spawned task.
        let name =
            facade_application().resolve::<Name>().map(|name| name.0).unwrap_or("nothing is bound");

        Response::ok(name)
    });

    Kernel::new(router.compile(&Application::new(".")).expect("compiles"))
}

async fn free_port() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("a port");
    let address = listener.local_addr().expect("an address");
    drop(listener);
    address
}

/// One `GET`, spelled by hand so this test needs no HTTP client.
async fn get(address: SocketAddr, path: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut stream = tokio::net::TcpStream::connect(address).await.expect("connect");
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .expect("write");

    let mut response = String::new();
    stream.read_to_string(&mut response).await.expect("read");
    response
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_handler_resolves_through_the_servers_application_and_not_the_global() {
    // The process-wide one, which is what a spawned connection task would
    // otherwise find.
    set_facade_application(application("the global one"));

    let address = free_port().await;
    let (tx, rx) = tokio::sync::watch::channel(false);

    let server = Server::new(kernel())
        .with_options(ServerOptions::default().bind(address))
        .for_application(application("the server's own"));

    let serving = tokio::spawn(async move { server.run_until(rx).await });

    // Wait for the listener rather than racing it.
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(address).await.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    // Several, so at least one connection is served by a worker other than the
    // one that accepted it.
    for _ in 0..10 {
        let response = get(address, "/whose").await;
        assert!(
            response.contains("the server's own"),
            "a connection resolved through the wrong application: {response}"
        );
    }

    let _ = tx.send(true);
    let _ = serving.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn without_one_it_falls_back_to_the_global() {
    // The documented behaviour, and the reason `for_application` exists: this
    // is right for a single-application process and wrong for any other.
    set_facade_application(application("the global one"));

    let address = free_port().await;
    let (tx, rx) = tokio::sync::watch::channel(false);

    let server = Server::new(kernel()).with_options(ServerOptions::default().bind(address));
    let serving = tokio::spawn(async move { server.run_until(rx).await });

    for _ in 0..100 {
        if tokio::net::TcpStream::connect(address).await.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let response = get(address, "/whose").await;
    assert!(response.contains("the global one"), "{response}");

    let _ = tx.send(true);
    let _ = serving.await;
}
