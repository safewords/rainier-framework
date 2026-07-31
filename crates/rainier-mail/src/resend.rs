//! The Resend API — [`ResendTransport`], behind the `resend` feature.
//!
//! One `POST /emails` with a bearer key.

use std::sync::Arc;
use std::time::Duration;

use rainier_http_client::Transport as HttpTransport;
use rainier_support::{BoxFuture, Result};
use serde_json::{json, Map, Value};

use crate::message::{Address, Message};
use crate::transport::Transport;

/// Delivers through Resend.
pub struct ResendTransport {
    http: Arc<dyn HttpTransport>,
    key: String,
    base: String,
    timeout: Duration,
}

impl ResendTransport {
    /// Deliver with this API key.
    pub fn new(http: Arc<dyn HttpTransport>, key: impl Into<String>) -> Self {
        Self {
            http,
            key: key.into(),
            base: "https://api.resend.com".into(),
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

impl Transport for ResendTransport {
    fn name(&self) -> &str {
        "resend"
    }

    fn send<'a>(&'a self, message: &'a Message) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let body = serde_json::to_vec(&payload(message))?;

            crate::api::deliver(
                &self.http,
                "resend",
                &format!("{}/emails", self.base),
                vec![("authorization", format!("Bearer {}", self.key))],
                "application/json",
                body,
                self.timeout,
            )
            .await
        })
    }
}

impl std::fmt::Debug for ResendTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResendTransport").field("base", &self.base).finish()
    }
}

/// The `/emails` body.
fn payload(message: &Message) -> Value {
    let mut body = Map::new();

    if let Some(from) = &message.envelope.from {
        body.insert("from".into(), from.to_header().into());
    }
    body.insert("to".into(), emails(&message.envelope.to));
    if !message.envelope.cc.is_empty() {
        body.insert("cc".into(), emails(&message.envelope.cc));
    }
    if !message.envelope.bcc.is_empty() {
        body.insert("bcc".into(), emails(&message.envelope.bcc));
    }
    if !message.envelope.reply_to.is_empty() {
        body.insert("reply_to".into(), emails(&message.envelope.reply_to));
    }
    body.insert("subject".into(), message.envelope.subject.clone().into());

    if let Some(html) = &message.html {
        body.insert("html".into(), html.clone().into());
    }
    if let Some(text) = &message.text {
        body.insert("text".into(), text.clone().into());
    }

    if !message.headers.is_empty() {
        let headers: Map<String, Value> = message
            .headers
            .iter()
            .map(|(name, value)| (name.clone(), value.clone().into()))
            .collect();
        body.insert("headers".into(), headers.into());
    }

    if !message.attachments.is_empty() {
        let attachments: Vec<Value> = message
            .attachments
            .iter()
            .map(|attachment| {
                json!({
                    "filename": attachment.file_name,
                    "content": attachment.to_base64(),
                    "content_type": attachment.content_type,
                })
            })
            .collect();
        body.insert("attachments".into(), attachments.into());
    }

    Value::Object(body)
}

fn emails(list: &[Address]) -> Value {
    list.iter().map(Address::to_header).collect::<Vec<_>>().into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Envelope;
    use rainier_http_client::FakeTransport;

    fn message() -> Message {
        let mut message = Message::new(
            Envelope::new("Hello")
                .from(Address::named("app@example.com", "App"))
                .to("ada@example.com")
                .bcc("archive@example.com"),
        );
        message.html = Some("<p>Hi</p>".into());
        message
    }

    #[tokio::test]
    async fn the_request_is_the_shape_resend_documents() {
        let fake = Arc::new(FakeTransport::new());
        let transport = ResendTransport::new(Arc::clone(&fake) as _, "re_key");

        transport.send(&message()).await.unwrap();

        let sent = &fake.recorded()[0];
        assert_eq!(sent.url(), "https://api.resend.com/emails");
        assert_eq!(sent.header("authorization"), Some("Bearer re_key"));

        let body = sent.json().unwrap();
        assert_eq!(body["from"], "App <app@example.com>");
        assert_eq!(body["to"][0], "ada@example.com");
        assert_eq!(body["bcc"][0], "archive@example.com");
        assert_eq!(body["subject"], "Hello");
        assert_eq!(body["html"], "<p>Hi</p>");
    }

    #[tokio::test]
    async fn a_rejection_names_resend() {
        let fake = Arc::new(FakeTransport::new());
        fake.responding(403, r#"{"message":"API key is invalid"}"#);
        let transport = ResendTransport::new(Arc::clone(&fake) as _, "k");

        let err = transport.send(&message()).await.unwrap_err();
        assert!(err.message().contains("resend answered 403"), "{}", err.message());
        assert!(err.message().contains("API key is invalid"), "{}", err.message());
    }
}
