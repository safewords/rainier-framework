//! **Rainier** — a batteries-included MVC framework for Rust.
//!
//! ```no_run
//! use rainier::prelude::*;
//!
//! async fn index() -> &'static str {
//!     "Hello from Rainier"
//! }
//!
//! # async fn run() -> Result<()> {
//! let app = Rainier::new(".")
//!     .with_routes(|router| {
//!         router.get("/", index).name("home");
//!     })
//!     .boot()
//!     .await?;
//! # let _ = app; Ok(()) }
//! ```
//!
//! # This crate is a name
//!
//! Everything here is [`rainier-framework`](rainier_framework), re-exported.
//! The two are the same crate for every purpose except the line in your
//! `Cargo.toml`, and they are versioned together — `rainier` 1.2.3 is
//! `rainier-framework` 1.2.3 and nothing else.
//!
//! Depend on whichever reads better. `rainier` is shorter and makes
//! `use rainier::prelude::*` the obvious first line; `rainier-framework` says
//! what it is next to the twenty-odd `rainier-*` crates it is assembled from.
//!
//! # Where to start
//!
//! [`prelude`] has what an application uses constantly. The
//! [documentation](https://github.com/safewords/rainier-framework/tree/main/docs)
//! is one page per concept, and each page explains the design decisions other
//! MVC frameworks leave implicit — including the places Rainier deliberately
//! disagrees with them.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/safewords/rainier-framework/main/assets/rainier_framework_mark.svg"
)]

pub use rainier_framework::*;
