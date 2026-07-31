//! # rainier-server
//!
//! The transport: a [`Kernel`] that turns a request into a response, and a
//! hyper-backed [`Server`] that puts it on a socket.
//!
//! ```no_run
//! use rainier_container::Container;
//! use rainier_middleware::MiddlewareRegistry;
//! use rainier_routing::Router;
//! use rainier_server::{Kernel, Server, ServerOptions};
//!
//! # #[tokio::main] async fn main() -> rainier_support::Result<()> {
//! let container = Container::new();
//! let registry = MiddlewareRegistry::new();
//!
//! let mut router = Router::new();
//! router.get("/", || async { "Hello from Rainier" });
//!
//! let kernel = Kernel::from_registry(router.compile(&container)?, &registry, &container)?;
//!
//! Server::new(kernel)
//!     .with_options(ServerOptions::default().bind_to("127.0.0.1", 8000)?)
//!     .run()
//!     .await
//! # }
//! ```
//!
//! ## What the kernel guarantees
//!
//! - **A panic is a 500.** One request must not take the process down, or
//!   poison a shared lock and take every later request with it. The guard
//!   wraps each poll, not just the call, so a panic *after* an await is caught
//!   too.
//! - **A 5xx does not leak.** An internal error's message routinely contains a
//!   connection string or a query; the client gets `Server Error` and the log
//!   gets the truth, unless [`Kernel::with_debug`] says otherwise. A 4xx is
//!   always shown, because it describes what the *client* did.
//! - **Content negotiation.** The same error is JSON for an API client and an
//!   HTML page for a browser, decided from `Accept` — see
//!   [`ExceptionRenderer`].

#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

pub mod kernel;
pub mod server;
pub mod upgrade;

pub use kernel::{read_body, DefaultExceptionRenderer, ExceptionRenderer, Kernel};
pub use server::{Server, ServerOptions};
pub use upgrade::{accept_key, is_websocket_upgrade};
