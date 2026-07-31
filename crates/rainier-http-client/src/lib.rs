//! # rainier-http-client
//!
//! Calling somebody else's HTTP API, and — more importantly — asserting that
//! you did.
//!
//! ```ignore
//! let response = Http::post("https://hooks.example.com/user-updated")
//!     .json(&payload)?
//!     .timeout(Duration::from_secs(10))
//!     .retry(3, Backoff::exponential())
//!     .send()
//!     .await?;
//!
//! response.error_for_status()?;
//! ```
//!
//! Without a framework-supplied client, every
//! application that calls out — a webhook, an OAuth token exchange, a
//! geolocation lookup — builds its own, with its own timeout and its own
//! idea of what to retry.
//!
//! # The fake is the point
//!
//! ```ignore
//! Http::fake();                       // record instead of sending
//!
//! notify_application(&user).await?;
//!
//! Http::assert_sent(|request| {
//!     request.url().ends_with("/hooks/user-updated")
//!         && request.header("x-signature").is_some()
//! });
//! ```
//!
//! Without one, asserting that an outbound call happened means standing up a
//! real server in a test — so nobody does, and the code that signs the webhook
//! is the code nothing exercises. That is not hypothetical: it is how a
//! webhook signing bug survives a port.
//!
//! `Http::fake()` also **refuses to reach the network**, which is the other
//! half. A suite that accidentally calls a real endpoint is a suite that fails
//! when somebody runs it on a train.
//!
//! # What it deliberately does not do
//!
//! No connection pooling knobs, no proxy configuration, no cookie jar, no
//! streaming upload. `ReqwestTransport::with_client` takes a client you
//! configured for anything this does not cover — the point of this crate is
//! the ergonomics and the fake, not hiding a good library.
//!
//! # The transport is opt-in
//!
//! `reqwest-transport` is **not** a default feature. It brings a TLS stack, an
//! HTTP/2 implementation and an IDNA table, and a build that only needs the
//! client API and the fake — a test suite, a Worker, a crate that takes a
//! [`Transport`] as an argument — should not pay for any of it. Without it,
//! `send()` reports that nothing is installed rather than silently doing
//! nothing.
//!
//! The framework turns it on through its own `http-client` feature.

#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

pub mod fake;
pub mod request;
pub mod response;
pub mod retry;
pub mod transport;

pub use fake::{FakeTransport, RecordedRequest};
pub use request::{Http, PendingRequest};
pub use response::HttpResponse;
pub use retry::Backoff;
pub use transport::Transport;

#[cfg(feature = "reqwest-transport")]
pub use transport::ReqwestTransport;
