//! The Postmark API — [`PostmarkTransport`], behind the `postmark` feature.
//!
//! One `POST /email` with the server token in a header. The request is built
//! over the framework's [HTTP transport port](rainier_http_client::Transport),
//! so a test hands it the fake and asserts on the exact JSON.

use std::sync::Arc;
use std::time::Duration;

use rainier_http_client::Transport as HttpTransport;
use rainier_support::{BoxFuture, Result};
use serde_json::{json, Map, Value};

use crate::message::{Address, Message};
use crate::transport::Transport;

/// Delivers through Postmark.
pub struct PostmarkTransport {
    http: Arc<dyn HttpTransport>,
    token: String,
    base: String,
    timeout: Duration,
}

impl PostmarkTransport {
    /// Deliver with this server token.
    pub fn new(http: Arc<dyn HttpTransport>, token: impl Into<String>) -> Self {
        Self {
            http,
            token: token.into(),
            base: "https://api.postmarkapp.com".into(),
            timeout: Duration::from_secs(30),
        }
    }

    /// Talk to a different base URL — a proxy, a test double.
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

impl Transport for PostmarkTransport {
    fn name(&self) -> &str {
        "postmark"
    }

    fn send<'a>(&'a self, message: &'a Message) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let body = serde_json::to_vec(&payload(message))?;

            crate::api::deliver(
                &self.http,
                "postmark",
                &format!("{}/email", self.base),
                vec![
                    ("accept", "application/json".into()),
                    ("x-postmark-server-token", self.token.clone()),
                ],
                "application/json",
                body,
                self.timeout,
            )
            .await
        })
    }
}

impl std::fmt::Debug for PostmarkTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The token stays out of Debug for the same reason it stays out of logs.
        f.debug_struct("PostmarkTransport").field("base", &self.base).finish()
    }
}

/// The `/email` body. Absent things are absent, not `null` — Postmark rejects
/// explicit nulls in some fields, and an empty `Bcc` is not a field.
fn payload(message: &Message) -> Value {
    let mut body = Map::new();

    if let Some(from) = &message.envelope.from {
        body.insert("From".into(), from.to_header().into());
    }
    body.insert("To".into(), join(&message.envelope.to).into());
    if !message.envelope.cc.is_empty() {
        body.insert("Cc".into(), join(&message.envelope.cc).into());
    }
    if !message.envelope.bcc.is_empty() {
        body.insert("Bcc".into(), join(&message.envelope.bcc).into());
    }
    if let Some(reply_to) = message.envelope.reply_to.first() {
        body.insert("ReplyTo".into(), reply_to.email.clone().into());
    }
    body.insert("Subject".into(), message.envelope.subject.clone().into());

    if let Some(html) = &message.html {
        body.insert("HtmlBody".into(), html.clone().into());
    }
    if let Some(text) = &message.text {
        body.insert("TextBody".into(), text.clone().into());
    }

    if !message.headers.is_empty() {
        let headers: Vec<Value> = message
            .headers
            .iter()
            .map(|(name, value)| json!({ "Name": name, "Value": value }))
            .collect();
        body.insert("Headers".into(), headers.into());
    }

    if !message.attachments.is_empty() {
        let attachments: Vec<Value> = message
            .attachments
            .iter()
            .map(|attachment| {
                json!({
                    "Name": attachment.file_name,
                    "Content": attachment.to_base64(),
                    "ContentType": attachment.content_type,
                })
            })
            .collect();
        body.insert("Attachments".into(), attachments.into());
    }

    Value::Object(body)
}

fn join(addresses: &[Address]) -> String {
    addresses.iter().map(Address::to_header).collect::<Vec<_>>().join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{Attachment, Envelope};
    use rainier_http_client::FakeTransport;

    fn message() -> Message {
        let mut message = Message::new(
            Envelope::new("Your invoice")
                .from(Address::named("billing@example.com", "Billing"))
                .to(Address::named("ada@example.com", "Ada"))
                .bcc("archive@example.com")
                .reply_to("support@example.com"),
        );
        message.html = Some("<p>Attached.</p>".into());
        message.text = Some("Attached.".into());
        message.headers.push(("X-Campaign".into(), "invoices".into()));
        message.attachments =
            vec![Attachment::from_bytes("invoice.pdf", "application/pdf", b"pdf".to_vec())];
        message
    }

    #[tokio::test]
    async fn the_request_is_the_shape_postmark_documents() {
        let fake = Arc::new(FakeTransport::new());
        let transport = PostmarkTransport::new(Arc::clone(&fake) as _, "server-token");

        transport.send(&message()).await.unwrap();

        let sent = &fake.recorded()[0];
        assert_eq!(sent.method(), "POST");
        assert_eq!(sent.url(), "https://api.postmarkapp.com/email");
        assert_eq!(sent.header("x-postmark-server-token"), Some("server-token"));
        assert_eq!(sent.header("content-type"), Some("application/json"));

        let body = sent.json().unwrap();
        assert_eq!(body["From"], "Billing <billing@example.com>");
        assert_eq!(body["To"], "Ada <ada@example.com>");
        assert_eq!(body["Bcc"], "archive@example.com");
        assert_eq!(body["ReplyTo"], "support@example.com");
        assert_eq!(body["Subject"], "Your invoice");
        assert_eq!(body["HtmlBody"], "<p>Attached.</p>");
        assert_eq!(body["TextBody"], "Attached.");
        assert_eq!(body["Headers"][0]["Name"], "X-Campaign");
        assert_eq!(body["Attachments"][0]["Name"], "invoice.pdf");
        assert_eq!(body["Attachments"][0]["Content"], "cGRm");
        assert_eq!(body["Attachments"][0]["ContentType"], "application/pdf");
    }

    #[tokio::test]
    async fn absent_fields_are_absent_rather_than_null() {
        let fake = Arc::new(FakeTransport::new());
        let transport = PostmarkTransport::new(Arc::clone(&fake) as _, "t");

        let mut simple = Message::new(Envelope::new("Hi").from("a@b.co").to("c@d.co"));
        simple.text = Some("hello".into());
        transport.send(&simple).await.unwrap();

        let body = fake.recorded()[0].json().unwrap();
        let object = body.as_object().unwrap();
        for absent in ["Cc", "Bcc", "ReplyTo", "HtmlBody", "Headers", "Attachments"] {
            assert!(!object.contains_key(absent), "{absent} should be absent: {body}");
        }
    }

    #[tokio::test]
    async fn a_rejection_names_postmark_and_quotes_the_body() {
        let fake = Arc::new(FakeTransport::new());
        fake.responding(422, r#"{"Message":"Invalid 'To' address."}"#);
        let transport = PostmarkTransport::new(Arc::clone(&fake) as _, "t");

        let err = transport.send(&message()).await.unwrap_err();
        assert!(err.message().contains("postmark answered 422"), "{}", err.message());
        assert!(err.message().contains("Invalid 'To'"), "{}", err.message());
    }

    #[tokio::test]
    async fn the_base_url_can_be_pointed_elsewhere() {
        let fake = Arc::new(FakeTransport::new());
        let transport = PostmarkTransport::new(Arc::clone(&fake) as _, "t")
            .with_base_url("http://localhost:9999");

        transport.send(&message()).await.unwrap();
        assert_eq!(fake.recorded()[0].url(), "http://localhost:9999/email");
    }

    #[test]
    fn the_token_stays_out_of_debug_output() {
        let fake = Arc::new(FakeTransport::new());
        let transport = PostmarkTransport::new(fake as _, "extremely-secret");

        assert!(!format!("{transport:?}").contains("extremely-secret"));
    }
}
