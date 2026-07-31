//! Amazon SES — [`SesTransport`], behind the `ses` feature.
//!
//! The raw sending path: the rendered MIME document goes up whole, so what
//! SES delivers is byte-for-byte what [`render_eml`](crate::render_eml())
//! produced and what every other transport here sends. Region and credentials
//! come from the AWS default chain — environment, profile, IMDS — exactly as
//! the other AWS drivers in this workspace resolve theirs.

use aws_sdk_sesv2::error::DisplayErrorContext;
use aws_sdk_sesv2::primitives::Blob;
use aws_sdk_sesv2::types::{Destination, EmailContent, RawMessage};
use rainier_support::{BoxFuture, Error, Result};

use crate::message::{Address, Message};
use crate::transport::{render_eml, Transport};

/// Delivers through Amazon SES.
pub struct SesTransport {
    client: tokio::sync::OnceCell<aws_sdk_sesv2::Client>,
}

impl SesTransport {
    /// Over a client you configured — a specific region, an endpoint override,
    /// a role.
    pub fn new(client: aws_sdk_sesv2::Client) -> Self {
        Self { client: tokio::sync::OnceCell::new_with(Some(client)) }
    }

    /// Over the AWS default chain: `AWS_REGION`, `AWS_ACCESS_KEY_ID` and
    /// friends, a profile, or the instance's own role.
    ///
    /// The chain is walked **lazily, on the first send** — walking it cannot
    /// fail, only produce credentials the first request will reject — which
    /// keeps construction synchronous, and a service provider is a
    /// synchronous place.
    pub fn from_env() -> Self {
        Self { client: tokio::sync::OnceCell::new() }
    }

    async fn client(&self) -> &aws_sdk_sesv2::Client {
        self.client
            .get_or_init(|| async {
                let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
                aws_sdk_sesv2::Client::new(&config)
            })
            .await
    }
}

impl Transport for SesTransport {
    fn name(&self) -> &str {
        "ses"
    }

    fn send<'a>(&'a self, message: &'a Message) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let raw = RawMessage::builder()
                .data(Blob::new(render_eml(message).into_bytes()))
                .build()
                .map_err(|e| Error::internal(format!("could not build the raw message: {e}")))?;

            let mut request = self
                .client()
                .await
                .send_email()
                .content(EmailContent::builder().raw(raw).build())
                .destination(destination(message));

            if let Some(from) = &message.envelope.from {
                request = request.from_email_address(from.to_header());
            }

            request.send().await.map_err(|e| {
                // DisplayErrorContext is the difference between "service
                // error" and the rejection reason SES actually gave.
                Error::service_unavailable(format!(
                    "SES did not accept the message: {}",
                    DisplayErrorContext(&e)
                ))
            })?;
            Ok(())
        })
    }
}

impl std::fmt::Debug for SesTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SesTransport").finish()
    }
}

/// The delivery list SES acts on — `to`, `cc` **and** `bcc`. The raw MIME
/// carries no `Bcc` header, so this split is what keeps a blind copy blind,
/// the same division of labour the SMTP envelope makes.
fn destination(message: &Message) -> Destination {
    Destination::builder()
        .set_to_addresses(Some(emails(&message.envelope.to)))
        .set_cc_addresses(non_empty(emails(&message.envelope.cc)))
        .set_bcc_addresses(non_empty(emails(&message.envelope.bcc)))
        .build()
}

fn emails(list: &[Address]) -> Vec<String> {
    list.iter().map(|address| address.email.clone()).collect()
}

fn non_empty(list: Vec<String>) -> Option<Vec<String>> {
    if list.is_empty() {
        None
    } else {
        Some(list)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Envelope;

    fn message() -> Message {
        let mut message = Message::new(
            Envelope::new("Hello")
                .from(Address::named("app@example.com", "App"))
                .to("ada@example.com")
                .cc("cc@example.com")
                .bcc("secret@example.com"),
        );
        message.text = Some("Hi".into());
        message
    }

    #[test]
    fn the_destination_carries_every_recipient_including_bcc() {
        let destination = destination(&message());

        assert_eq!(destination.to_addresses(), ["ada@example.com"]);
        assert_eq!(destination.cc_addresses(), ["cc@example.com"]);
        assert_eq!(destination.bcc_addresses(), ["secret@example.com"]);
    }

    #[test]
    fn absent_recipient_kinds_are_absent_rather_than_empty_lists() {
        let simple = Message::new(Envelope::new("Hi").from("a@b.co").to("c@d.co"));
        let destination = destination(&simple);

        assert!(destination.cc_addresses().is_empty());
        assert!(destination.bcc_addresses().is_empty());
    }

    #[test]
    fn the_raw_document_never_contains_the_blind_recipients() {
        // The property the destination/document split exists for.
        let eml = render_eml(&message());
        assert!(!eml.contains("secret@example.com"), "{eml}");
    }
}
