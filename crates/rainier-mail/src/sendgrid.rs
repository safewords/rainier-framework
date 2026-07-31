//! The SendGrid API — [`SendGridTransport`], behind the `sendgrid` feature.
//!
//! One `POST /v3/mail/send` with a bearer key. SendGrid answers `202` with an
//! empty body on success, which [the shared verdict](crate::api) treats the
//! same as any other 2xx.

use std::sync::Arc;
use std::time::Duration;

use rainier_http_client::Transport as HttpTransport;
use rainier_support::{BoxFuture, Result};
use serde_json::{json, Map, Value};

use crate::message::{Address, Message};
use crate::transport::Transport;

/// Delivers through SendGrid.
pub struct SendGridTransport {
    http: Arc<dyn HttpTransport>,
    key: String,
    base: String,
    timeout: Duration,
}

impl SendGridTransport {
    /// Deliver with this API key.
    pub fn new(http: Arc<dyn HttpTransport>, key: impl Into<String>) -> Self {
        Self {
            http,
            key: key.into(),
            base: "https://api.sendgrid.com".into(),
            timeout: Duration::from_secs(30),
        }
    }

    /// Talk to a different base URL — a regional endpoint, a test double.
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

impl Transport for SendGridTransport {
    fn name(&self) -> &str {
        "sendgrid"
    }

    fn send<'a>(&'a self, message: &'a Message) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let body = serde_json::to_vec(&payload(message))?;

            crate::api::deliver(
                &self.http,
                "sendgrid",
                &format!("{}/v3/mail/send", self.base),
                vec![("authorization", format!("Bearer {}", self.key))],
                "application/json",
                body,
                self.timeout,
            )
            .await
        })
    }
}

impl std::fmt::Debug for SendGridTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SendGridTransport").field("base", &self.base).finish()
    }
}

/// The `/v3/mail/send` body.
fn payload(message: &Message) -> Value {
    let mut personalization = Map::new();
    personalization.insert("to".into(), addresses(&message.envelope.to));
    if !message.envelope.cc.is_empty() {
        personalization.insert("cc".into(), addresses(&message.envelope.cc));
    }
    if !message.envelope.bcc.is_empty() {
        personalization.insert("bcc".into(), addresses(&message.envelope.bcc));
    }

    let mut body = Map::new();
    body.insert("personalizations".into(), json!([personalization]));
    if let Some(from) = &message.envelope.from {
        body.insert("from".into(), address(from));
    }
    if let Some(reply_to) = message.envelope.reply_to.first() {
        body.insert("reply_to".into(), address(reply_to));
    }
    body.insert("subject".into(), message.envelope.subject.clone().into());

    // Plain before HTML: the API wants parts in ascending order of preference.
    let mut content = Vec::new();
    if let Some(text) = &message.text {
        content.push(json!({ "type": "text/plain", "value": text }));
    }
    if let Some(html) = &message.html {
        content.push(json!({ "type": "text/html", "value": html }));
    }
    body.insert("content".into(), content.into());

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
                    "content": attachment.to_base64(),
                    "type": attachment.content_type,
                    "filename": attachment.file_name,
                })
            })
            .collect();
        body.insert("attachments".into(), attachments.into());
    }

    Value::Object(body)
}

fn address(address: &Address) -> Value {
    match &address.name {
        Some(name) => json!({ "email": address.email, "name": name }),
        None => json!({ "email": address.email }),
    }
}

fn addresses(list: &[Address]) -> Value {
    list.iter().map(address).collect::<Vec<_>>().into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Envelope;
    use rainier_http_client::FakeTransport;

    fn message() -> Message {
        let mut message = Message::new(
            Envelope::new("Welcome!")
                .from(Address::named("app@example.com", "App"))
                .to(Address::named("ada@example.com", "Ada"))
                .cc("cc@example.com"),
        );
        message.html = Some("<p>Hi</p>".into());
        message.text = Some("Hi".into());
        message
    }

    #[tokio::test]
    async fn the_request_is_the_shape_sendgrid_documents() {
        let fake = Arc::new(FakeTransport::new());
        fake.responding(202, "");
        let transport = SendGridTransport::new(Arc::clone(&fake) as _, "SG.key");

        transport.send(&message()).await.unwrap();

        let sent = &fake.recorded()[0];
        assert_eq!(sent.url(), "https://api.sendgrid.com/v3/mail/send");
        assert_eq!(sent.header("authorization"), Some("Bearer SG.key"));

        let body = sent.json().unwrap();
        assert_eq!(body["personalizations"][0]["to"][0]["email"], "ada@example.com");
        assert_eq!(body["personalizations"][0]["to"][0]["name"], "Ada");
        assert_eq!(body["personalizations"][0]["cc"][0]["email"], "cc@example.com");
        assert_eq!(body["from"]["email"], "app@example.com");
        assert_eq!(body["subject"], "Welcome!");
    }

    #[tokio::test]
    async fn plain_text_comes_before_html_because_the_api_requires_it() {
        let fake = Arc::new(FakeTransport::new());
        fake.responding(202, "");
        let transport = SendGridTransport::new(Arc::clone(&fake) as _, "k");

        transport.send(&message()).await.unwrap();

        let body = fake.recorded()[0].json().unwrap();
        assert_eq!(body["content"][0]["type"], "text/plain");
        assert_eq!(body["content"][1]["type"], "text/html");
    }

    #[tokio::test]
    async fn a_bare_address_carries_no_name_field() {
        let fake = Arc::new(FakeTransport::new());
        fake.responding(202, "");
        let transport = SendGridTransport::new(Arc::clone(&fake) as _, "k");

        let mut simple = Message::new(Envelope::new("Hi").from("a@b.co").to("c@d.co"));
        simple.text = Some("hello".into());
        transport.send(&simple).await.unwrap();

        let body = fake.recorded()[0].json().unwrap();
        assert!(body["personalizations"][0]["to"][0].get("name").is_none(), "{body}");
    }

    #[tokio::test]
    async fn a_rejection_names_sendgrid() {
        let fake = Arc::new(FakeTransport::new());
        fake.responding(
            401,
            r#"{"errors":[{"message":"The provided authorization grant is invalid"}]}"#,
        );
        let transport = SendGridTransport::new(Arc::clone(&fake) as _, "k");

        let err = transport.send(&message()).await.unwrap_err();
        assert!(err.message().contains("sendgrid answered 401"), "{}", err.message());
    }
}
