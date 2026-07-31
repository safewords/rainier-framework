//! # rainier-mail
//!
//! Email as an object: a [`Mailable`] describes the message, the [`Mailer`]
//! renders and delivers it, and a [`Transport`] decides where it actually
//! goes.
//!
//! ```
//! use rainier_mail::{Content, Envelope, Mailable, Mailer};
//! use rainier_view::MemoryEngine;
//! use std::sync::Arc;
//!
//! struct WelcomeEmail { name: String, email: String }
//!
//! impl Mailable for WelcomeEmail {
//!     fn envelope(&self) -> Envelope {
//!         Envelope::new("Welcome!").to(self.email.clone())
//!     }
//!     fn content(&self) -> rainier_support::Result<Content> {
//!         Content::view("mail.welcome", serde_json::json!({ "name": self.name }))
//!     }
//! }
//!
//! # #[tokio::main] async fn main() -> rainier_support::Result<()> {
//! let views = Arc::new(MemoryEngine::new().with("mail.welcome", "<p>Hi {{ name }}</p>"));
//! let mailer = Mailer::fake(views)
//!     .with_default_from(rainier_mail::Address::new("app@example.com"));
//!
//! mailer.send(&WelcomeEmail { name: "Ada".into(), email: "ada@example.com".into() }).await?;
//! mailer.assert_sent_to("ada@example.com");
//! # Ok(()) }
//! ```
//!
//! ## Why a mailable is a value
//!
//! `Mailable::build` produces a [`Message`] and nothing else — no socket, no
//! I/O, no configuration. So the interesting part of an email (does it address
//! the right person, does the template render, does the subject read correctly)
//! is testable without a mail server, and the part that needs one is a
//! [`Transport`] you can swap.
//!
//! ## Transports
//!
//! | Transport | Goes to | For |
//! |---|---|---|
//! | [`LogTransport`] | the log | development — the default, so nothing escapes |
//! | [`MemoryTransport`] | memory | tests |
//! | [`FileTransport`] | `.eml` files | opening the real rendered HTML in a browser |
//!
//! ## Two safety valves
//!
//! [`Mailer::always_to`] redirects every message to one address, which is the
//! difference between testing a flow against production data and emailing all
//! of those customers. And a [`MessageSending`] listener can veto a send,
//! which is where a suppression list belongs.

#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

pub mod driver;
pub mod mailable;
pub mod mailer;
pub mod message;
pub mod transport;

pub use driver::MailDriver;
pub use mailable::Mailable;
pub use mailer::{Mailer, MessageSending, MessageSent, ORIGINAL_TO};
pub use message::{Address, Attachment, Content, Envelope, Message};
pub use transport::{render_eml, FileTransport, LogTransport, MemoryTransport, Transport};
