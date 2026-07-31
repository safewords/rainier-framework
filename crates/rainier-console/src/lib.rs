//! # rainier-console
//!
//! The console kernel: an [`Arguments`] parser, a [`Command`] contract, and the
//! [`Console`] that dispatches between them — Rainier's command-line entry
//! point.
//!
//! ```
//! use rainier_console::{Arguments, Command, Console};
//! use rainier_container::Application;
//! use rainier_support::Result;
//!
//! struct Inspire;
//!
//! #[async_trait::async_trait]
//! impl Command for Inspire {
//!     fn name(&self) -> &str { "inspire" }
//!     fn description(&self) -> &str { "Print an inspiring quote" }
//!
//!     async fn handle(&self, _args: &Arguments, _app: &Application) -> Result<i32> {
//!         println!("Simplicity is the ultimate sophistication.");
//!         Ok(0)
//!     }
//! }
//!
//! # #[tokio::main] async fn main() {
//! let console = Console::new("rainier").register(Inspire);
//! let app = Application::new(".");
//!
//! assert_eq!(console.run_argv(&app, ["inspire"]).await, 0);
//! # }
//! ```
//!
//! This crate deliberately knows nothing about routing, queues or databases —
//! it depends only on the container. The concrete commands (`route:list`,
//! `serve`, `migrate`, `queue:work`) live in the `rainier` crate, which already
//! depends on everything they need. That keeps a console usable in an
//! application that has no HTTP layer at all.

#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

pub mod arguments;
pub mod console;
pub mod io;

pub use arguments::Arguments;
pub use console::{exit, Command, Console};

// Re-exported so command implementations get the attribute macro without
// adding the dependency themselves.
pub use async_trait::async_trait;
