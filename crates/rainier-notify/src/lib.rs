//! # rainier-notify
//!
//! Notifications: a message **to somebody**, over whichever channels
//! they can be reached on.
//!
//! ## This is not an event
//!
//! The two get confused because both fan out, and they fan out for different
//! reasons and to different things.
//!
//! | | An event | A notification |
//! |---|---|---|
//! | Says | "this happened" | "tell **them** this" |
//! | Recipient | none | exactly one, per send |
//! | Fan-out decided by | who subscribed, at boot | the notification and the recipient, per send |
//! | Payload | one shape | one per channel |
//! | Nobody handles it | normal | almost certainly a bug |
//! | Direction | producer → whoever is listening | producer → a named recipient |
//!
//! `PostPublished` is an event: a fact, with no opinion about who should hear
//! about it. `NewPostFromAuthorYouFollow` is a notification: it has a
//! recipient, and it knows it should be an email *and* a row in their bell
//! menu.
//!
//! They compose, and that composition is the usual wiring:
//!
//! ```ignore
//! // The event says what happened.
//! events.listen(|event: Arc<PostPublished>| async move {
//!     let notifier = resolve::<Notifier>()?;
//!
//!     // The notification says who to tell, and how.
//!     for follower in followers_of(event.post.author_id).await? {
//!         notifier.send(&follower, &NewPost { post: event.post.clone() }).await?;
//!     }
//!     Ok(())
//! });
//! ```
//!
//! Collapsing them loses something either way. An event with a recipient cannot
//! be listened to by anything else — an analytics listener has no business
//! receiving "tell Ada about this". A notification with no recipient cannot ask
//! Ada whether she wanted an email.
//!
//! ## The shape
//!
//! ```
//! use rainier_notify::{Channels, LogChannel, Notifiable, Notification, Notifier};
//!
//! struct User { id: u64, email: String }
//!
//! impl Notifiable for User {
//!     fn notifiable_id(&self) -> String { self.id.to_string() }
//!     fn notifiable_type(&self) -> &'static str { "User" }
//!     fn route_for(&self, channel: &str) -> Option<String> {
//!         match channel {
//!             "mail" => Some(self.email.clone()),
//!             _ => None,
//!         }
//!     }
//! }
//!
//! struct InvoicePaid { amount: u64 }
//!
//! impl Notification<User> for InvoicePaid {
//!     fn notification_name(&self) -> &'static str { "billing.invoice-paid" }
//!
//!     fn via(&self, _: &User) -> Channels {
//!         Channels::new().with::<LogChannel>()
//!     }
//!
//!     fn to_text(&self, user: &User) -> Option<String> {
//!         Some(format!("Thanks — we received {} from user {}.", self.amount, user.id))
//!     }
//! }
//!
//! # #[tokio::main] async fn main() -> rainier_support::Result<()> {
//! let notifier = Notifier::new().with(LogChannel);
//! let receipt = notifier.send(&User { id: 7, email: "ada@example.com".into() },
//!                             &InvoicePaid { amount: 4200 }).await?;
//!
//! assert!(receipt.delivered_anywhere());
//! # Ok(()) }
//! ```
//!
//! ## Channels are selected by type
//!
//! `Channels::new().with::<MailChannel>()`, not `["mail"]`. The same reasoning
//! as middleware and configuration keys: a misspelled
//! `"mial"` is a notification that silently goes nowhere, and deleting a
//! channel should break every notification that used it in the compiler.
//!
//! ## Failure is per channel
//!
//! An SMTP outage must not also cost the database row that puts the
//! notification in the recipient's bell menu, so a channel failing does not
//! stop the others. [`Receipt`] says what happened on each;
//! [`Receipt::into_result`] turns "nothing got through at all" into an error.
//!
//! A recipient with no address on a channel is **skipped**, not failed — a user
//! with no phone number should still get the email.

#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

pub mod channel;
pub mod notification;
pub mod notifier;

pub use channel::{Channel, LogChannel, MailChannel, MemoryChannel, Recorded};
pub use notification::{Channels, Delivery, Notifiable, Notification, Receipt, Sent};
pub use notifier::Notifier;
