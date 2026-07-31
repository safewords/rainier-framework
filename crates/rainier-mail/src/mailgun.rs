//! The Mailgun API — [`MailgunTransport`], behind the `mailgun` feature.
//!
//! Mailgun's `messages.mime` endpoint takes the rendered MIME document
//! whole, which is the honest fit here: [`render_eml`](crate::render_eml())
//! already produces exactly that, headers, parts and attachments included.
//! The form carries only the envelope recipients — which is also how `Bcc`
//! stays blind, since the MIME never contains the header.

use std::sync::Arc;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use rainier_http_client::Transport as HttpTransport;
use rainier_support::{BoxFuture, Result};

use crate::message::Message;
use crate::transport::{render_eml, Transport};

/// Delivers through Mailgun.
pub struct MailgunTransport {
    http: Arc<dyn HttpTransport>,
    domain: String,
    secret: String,
    base: String,
    timeout: Duration,
}

impl MailgunTransport {
    /// Deliver from this sending domain with this API key.
    pub fn new(
        http: Arc<dyn HttpTransport>,
        domain: impl Into<String>,
        secret: impl Into<String>,
    ) -> Self {
        Self {
            http,
            domain: domain.into(),
            secret: secret.into(),
            base: "https://api.mailgun.net".into(),
            timeout: Duration::from_secs(30),
        }
    }

    /// Talk to a different base URL — `https://api.eu.mailgun.net` for a
    /// domain in the EU region, or a test double.
    pub fn with_base_url(mut self, base: impl Into<String>) -> Self {
        self.base = base.into();
        self
    }

    /// The wall clock on the API call. Thirty seconds by default.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

impl Transport for MailgunTransport {
    fn name(&self) -> &str {
        "mailgun"
    }

    fn send<'a>(&'a self, message: &'a Message) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let boundary = format!(
                "rainier-mailgun-{}",
                chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
            );
            let body = multipart(message, &boundary);

            crate::api::deliver(
                &self.http,
                "mailgun",
                &format!("{}/v3/{}/messages.mime", self.base, self.domain),
                vec![(
                    "authorization",
                    format!("Basic {}", B64.encode(format!("api:{}", self.secret))),
                )],
                &format!("multipart/form-data; boundary=\"{boundary}\""),
                body,
                self.timeout,
            )
            .await
        })
    }
}

impl std::fmt::Debug for MailgunTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MailgunTransport")
            .field("domain", &self.domain)
            .field("base", &self.base)
            .finish()
    }
}

/// The `messages.mime` form: one `to` field per envelope recipient, and the
/// MIME document as a file part.
fn multipart(message: &Message, boundary: &str) -> Vec<u8> {
    let mut body = String::new();

    for recipient in message.envelope.recipients() {
        body.push_str(&format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"to\"\r\n\r\n{}\r\n",
            recipient.email
        ));
    }

    body.push_str(&format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"message\"; \
         filename=\"message.mime\"\r\nContent-Type: message/rfc822\r\n\r\n{}\r\n--{boundary}--\r\n",
        render_eml(message)
    ));

    body.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{Address, Envelope};
    use rainier_http_client::FakeTransport;

    fn message() -> Message {
        let mut message = Message::new(
            Envelope::new("Hello")
                .from(Address::named("app@example.com", "App"))
                .to("ada@example.com")
                .bcc("secret@example.com"),
        );
        message.text = Some("Hi Ada".into());
        message
    }

    #[tokio::test]
    async fn the_request_reaches_the_domains_mime_endpoint_with_basic_auth() {
        let fake = Arc::new(FakeTransport::new());
        let transport = MailgunTransport::new(Arc::clone(&fake) as _, "mg.example.com", "key-x");

        transport.send(&message()).await.unwrap();

        let sent = &fake.recorded()[0];
        assert_eq!(sent.url(), "https://api.mailgun.net/v3/mg.example.com/messages.mime");
        // `api:key-x`, base64'd — the credential scheme Mailgun documents.
        assert_eq!(sent.header("authorization"), Some("Basic YXBpOmtleS14"));
        assert!(sent.header("content-type").unwrap().starts_with("multipart/form-data"));
    }

    #[tokio::test]
    async fn every_envelope_recipient_is_a_to_field_and_bcc_stays_out_of_the_mime() {
        let fake = Arc::new(FakeTransport::new());
        let transport = MailgunTransport::new(Arc::clone(&fake) as _, "mg.example.com", "k");

        transport.send(&message()).await.unwrap();

        let body = fake.recorded()[0].body();
        // The blind recipient is delivered to…
        assert!(body.contains("name=\"to\"\r\n\r\nsecret@example.com"), "{body}");
        assert!(body.contains("name=\"to\"\r\n\r\nada@example.com"), "{body}");

        // …but the MIME part never names them.
        let mime = body.split("name=\"message\"").nth(1).expect("a message part");
        assert!(mime.contains("Subject: Hello"), "{mime}");
        assert!(!mime.contains("secret@example.com"), "{mime}");
    }

    #[tokio::test]
    async fn the_eu_region_is_a_base_url_away() {
        let fake = Arc::new(FakeTransport::new());
        let transport = MailgunTransport::new(Arc::clone(&fake) as _, "mg.example.com", "k")
            .with_base_url("https://api.eu.mailgun.net");

        transport.send(&message()).await.unwrap();
        assert!(fake.recorded()[0].url().starts_with("https://api.eu.mailgun.net/"));
    }

    #[tokio::test]
    async fn a_rejection_names_mailgun() {
        let fake = Arc::new(FakeTransport::new());
        fake.responding(401, "Forbidden");
        let transport = MailgunTransport::new(Arc::clone(&fake) as _, "mg.example.com", "k");

        let err = transport.send(&message()).await.unwrap_err();
        assert!(err.message().contains("mailgun answered 401"), "{}", err.message());
    }

    #[test]
    fn the_secret_stays_out_of_debug_output() {
        let fake = Arc::new(FakeTransport::new());
        let transport = MailgunTransport::new(fake as _, "mg.example.com", "extremely-secret");

        assert!(!format!("{transport:?}").contains("extremely-secret"));
    }
}
