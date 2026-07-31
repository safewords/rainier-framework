//! The message model — [`Address`], [`Envelope`], [`Content`], [`Attachment`]
//! and the assembled [`Message`].

use rainier_support::{Error, Result};
use serde::{Deserialize, Serialize};

/// An email address, with an optional display name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Address {
    /// The address itself.
    pub email: String,
    /// The display name, if any.
    pub name: Option<String>,
}

impl Address {
    /// A bare address.
    pub fn new(email: impl Into<String>) -> Self {
        Self { email: email.into(), name: None }
    }

    /// An address with a display name.
    pub fn named(email: impl Into<String>, name: impl Into<String>) -> Self {
        Self { email: email.into(), name: Some(name.into()) }
    }

    /// Render as a header value: `Ada Lovelace <ada@example.com>`.
    ///
    /// A display name containing a quote, a newline or a comma is dropped
    /// rather than escaped. Names come from user data often enough that
    /// getting the escaping subtly wrong would be a header-injection hole, and
    /// an email that arrives without a display name is a much smaller problem
    /// than one that arrives with an extra `Bcc:`.
    pub fn to_header(&self) -> String {
        match &self.name {
            Some(name) if is_safe_display_name(name) => format!("{name} <{}>", self.email),
            _ => self.email.clone(),
        }
    }

    /// Whether this looks like a deliverable address.
    pub fn is_valid(&self) -> bool {
        let Some((local, domain)) = self.email.split_once('@') else {
            return false;
        };
        !local.is_empty()
            && domain.contains('.')
            && !domain.starts_with('.')
            && !domain.ends_with('.')
            && !self.email.chars().any(|c| c.is_whitespace() || c == '\r' || c == '\n')
    }
}

fn is_safe_display_name(name: &str) -> bool {
    !name.is_empty()
        && !name
            .chars()
            .any(|c| c == '"' || c == '<' || c == '>' || c == ',' || c == ':' || c.is_control())
}

impl From<&str> for Address {
    fn from(email: &str) -> Self {
        Address::new(email)
    }
}

impl From<String> for Address {
    fn from(email: String) -> Self {
        Address::new(email)
    }
}

impl std::fmt::Display for Address {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_header())
    }
}

/// Who a message is from, to, and about.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    /// The sender. Filled from configuration when the mailable leaves it out.
    pub from: Option<Address>,
    /// Where replies should go.
    pub reply_to: Vec<Address>,
    /// Primary recipients.
    pub to: Vec<Address>,
    /// Carbon copies.
    pub cc: Vec<Address>,
    /// Blind carbon copies.
    pub bcc: Vec<Address>,
    /// The subject line.
    pub subject: String,
}

impl Envelope {
    /// An envelope with a subject and no recipients yet.
    pub fn new(subject: impl Into<String>) -> Self {
        Self { subject: subject.into(), ..Self::default() }
    }

    /// Set the sender.
    pub fn from(mut self, address: impl Into<Address>) -> Self {
        self.from = Some(address.into());
        self
    }

    /// Add a recipient.
    pub fn to(mut self, address: impl Into<Address>) -> Self {
        self.to.push(address.into());
        self
    }

    /// Add a carbon copy.
    pub fn cc(mut self, address: impl Into<Address>) -> Self {
        self.cc.push(address.into());
        self
    }

    /// Add a blind carbon copy.
    pub fn bcc(mut self, address: impl Into<Address>) -> Self {
        self.bcc.push(address.into());
        self
    }

    /// Add a reply-to address.
    pub fn reply_to(mut self, address: impl Into<Address>) -> Self {
        self.reply_to.push(address.into());
        self
    }

    /// Every address that will actually receive the message.
    pub fn recipients(&self) -> Vec<&Address> {
        self.to.iter().chain(&self.cc).chain(&self.bcc).collect()
    }

    /// Whether anyone will receive it.
    pub fn has_recipients(&self) -> bool {
        !self.recipients().is_empty()
    }
}

/// A message's body: either a rendered view or literal text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Content {
    /// Render this view for the HTML body, optionally with a plain-text twin.
    View {
        /// The HTML template's name.
        html: String,
        /// The plain-text template's name, if there is one.
        text: Option<String>,
        /// The data both are rendered with.
        data: serde_json::Value,
    },
    /// Literal bodies, already rendered.
    Literal {
        /// The HTML body, if any.
        html: Option<String>,
        /// The plain-text body, if any.
        text: Option<String>,
    },
}

impl Content {
    /// An HTML body from a view.
    pub fn view(name: impl Into<String>, data: impl Serialize) -> Result<Self> {
        Ok(Content::View { html: name.into(), text: None, data: serde_json::to_value(data)? })
    }

    /// Add a plain-text alternative view.
    pub fn with_text_view(mut self, name: impl Into<String>) -> Self {
        if let Content::View { text, .. } = &mut self {
            *text = Some(name.into());
        }
        self
    }

    /// A literal HTML body.
    pub fn html(body: impl Into<String>) -> Self {
        Content::Literal { html: Some(body.into()), text: None }
    }

    /// A literal plain-text body.
    pub fn text(body: impl Into<String>) -> Self {
        Content::Literal { html: None, text: Some(body.into()) }
    }
}

/// A file travelling with a message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Attachment {
    /// The name the recipient sees.
    pub file_name: String,
    /// Its MIME type.
    pub content_type: String,
    /// Its bytes.
    pub bytes: Vec<u8>,
}

impl Attachment {
    /// An attachment from bytes already in hand.
    pub fn from_bytes(
        file_name: impl Into<String>,
        content_type: impl Into<String>,
        bytes: impl Into<Vec<u8>>,
    ) -> Self {
        Self { file_name: file_name.into(), content_type: content_type.into(), bytes: bytes.into() }
    }

    /// An attachment read from disk, guessing the type from the extension.
    pub fn from_path(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let path = path.as_ref();
        let bytes = std::fs::read(path)
            .map_err(|e| Error::internal(format!("could not attach {}: {e}", path.display())))?;

        let file_name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "attachment".to_string());

        let content_type = guess_content_type(&file_name);
        Ok(Self { file_name, content_type, bytes })
    }

    /// The attachment's size in bytes.
    pub fn size(&self) -> usize {
        self.bytes.len()
    }

    /// Its content, base64-encoded for a MIME part.
    pub fn to_base64(&self) -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(&self.bytes)
    }
}

fn guess_content_type(file_name: &str) -> String {
    let extension =
        file_name.rsplit_once('.').map(|(_, ext)| ext.to_ascii_lowercase()).unwrap_or_default();

    match extension.as_str() {
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "csv" => "text/csv",
        "txt" => "text/plain",
        "html" | "htm" => "text/html",
        "json" => "application/json",
        "zip" => "application/zip",
        // The generic fallback: better than guessing wrong, and every client
        // handles it.
        _ => "application/octet-stream",
    }
    .to_string()
}

/// A fully assembled message, ready for a transport.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// Who it is from, to and about.
    pub envelope: Envelope,
    /// The rendered HTML body, if any.
    pub html: Option<String>,
    /// The rendered plain-text body, if any.
    pub text: Option<String>,
    /// Files travelling with it.
    pub attachments: Vec<Attachment>,
    /// Extra headers.
    pub headers: Vec<(String, String)>,
}

impl Message {
    /// A message with `envelope` and no body yet.
    pub fn new(envelope: Envelope) -> Self {
        Self { envelope, html: None, text: None, attachments: Vec::new(), headers: Vec::new() }
    }

    /// Add an extra header. Values containing CR or LF are rejected, since
    /// that is how header injection works.
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        let (name, value) = (name.into(), value.into());
        if !name.contains(['\r', '\n']) && !value.contains(['\r', '\n']) {
            self.headers.push((name, value));
        }
        self
    }

    /// Whether the message has any body at all.
    pub fn has_body(&self) -> bool {
        self.html.is_some() || self.text.is_some()
    }

    /// The body a text-only client would see: the plain-text part if there is
    /// one, otherwise the HTML stripped of its tags.
    pub fn text_body(&self) -> String {
        match (&self.text, &self.html) {
            (Some(text), _) => text.clone(),
            (None, Some(html)) => strip_tags(html),
            (None, None) => String::new(),
        }
    }

    /// Check the message is deliverable.
    pub fn validate(&self) -> Result<()> {
        if !self.envelope.has_recipients() {
            return Err(Error::internal("the message has no recipients"));
        }
        if self.envelope.from.is_none() {
            return Err(Error::internal(
                "the message has no sender — set one on the mailable or in `mail.from`",
            ));
        }
        for address in self.envelope.recipients() {
            if !address.is_valid() {
                return Err(Error::internal(format!(
                    "`{}` is not a deliverable address",
                    address.email
                )));
            }
        }
        if !self.has_body() {
            return Err(Error::internal("the message has no body"));
        }
        Ok(())
    }
}

/// A crude tag stripper for the plain-text fallback.
///
/// Not a converter: it produces something readable, not something pretty. A
/// mailable that cares should supply its own text view.
fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut inside = false;

    for character in html.chars() {
        match character {
            '<' => inside = true,
            '>' => {
                inside = false;
                // A tag boundary is a word boundary; without this,
                // `<p>a</p><p>b</p>` would run together as `ab`.
                if !out.ends_with(char::is_whitespace) && !out.is_empty() {
                    out.push(' ');
                }
            }
            other if !inside => out.push(other),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_render_with_and_without_a_name() {
        assert_eq!(Address::new("ada@example.com").to_header(), "ada@example.com");
        assert_eq!(
            Address::named("ada@example.com", "Ada Lovelace").to_header(),
            "Ada Lovelace <ada@example.com>"
        );
    }

    #[test]
    fn a_hostile_display_name_is_dropped_rather_than_escaped() {
        // Header injection: a name carrying a newline could add a `Bcc:`.
        for hostile in ["Ada\r\nBcc: attacker@evil.test", "Ada \"quoted\"", "a<b>", "a,b", "a:b"] {
            let rendered = Address::named("ada@example.com", hostile).to_header();
            assert_eq!(rendered, "ada@example.com", "for `{hostile}`");
        }
    }

    #[test]
    fn address_validity() {
        assert!(Address::new("ada@example.com").is_valid());
        assert!(Address::new("a.b+c@sub.example.co.uk").is_valid());

        for bad in ["", "ada", "@example.com", "ada@example", "ada @example.com", "a@.com", "a@b."]
        {
            assert!(!Address::new(bad).is_valid(), "`{bad}` should be invalid");
        }
        assert!(!Address::new("ada@example.com\r\nBcc: x@y.z").is_valid());
    }

    #[test]
    fn an_envelope_collects_every_recipient() {
        let envelope = Envelope::new("Hello")
            .from(Address::named("app@example.com", "App"))
            .to("a@example.com")
            .cc("b@example.com")
            .bcc("c@example.com")
            .reply_to("support@example.com");

        assert_eq!(envelope.recipients().len(), 3);
        assert!(envelope.has_recipients());
        assert_eq!(envelope.reply_to.len(), 1);
        assert_eq!(envelope.subject, "Hello");
    }

    #[test]
    fn an_empty_envelope_has_no_recipients() {
        assert!(!Envelope::new("Hello").has_recipients());
    }

    #[test]
    fn content_variants() {
        let view = Content::view("mail.welcome", serde_json::json!({ "name": "Ada" }))
            .unwrap()
            .with_text_view("mail.welcome_text");

        let Content::View { html, text, data } = &view else { panic!("expected a view") };
        assert_eq!(html, "mail.welcome");
        assert_eq!(text.as_deref(), Some("mail.welcome_text"));
        assert_eq!(data["name"], "Ada");

        assert_eq!(
            Content::html("<p>hi</p>"),
            Content::Literal { html: Some("<p>hi</p>".into()), text: None }
        );
    }

    #[test]
    fn attachments_carry_their_bytes_and_type() {
        let attachment = Attachment::from_bytes("report.pdf", "application/pdf", b"%PDF".to_vec());

        assert_eq!(attachment.size(), 4);
        assert_eq!(attachment.to_base64(), "JVBERg==");
    }

    #[test]
    fn content_types_are_guessed_from_the_extension() {
        assert_eq!(guess_content_type("a.pdf"), "application/pdf");
        assert_eq!(guess_content_type("a.PNG"), "image/png");
        assert_eq!(guess_content_type("a.jpeg"), "image/jpeg");
        assert_eq!(guess_content_type("noextension"), "application/octet-stream");
        assert_eq!(guess_content_type("a.unknownext"), "application/octet-stream");
    }

    fn deliverable() -> Message {
        let mut message =
            Message::new(Envelope::new("Hi").from("app@example.com").to("ada@example.com"));
        message.text = Some("Hello".into());
        message
    }

    #[test]
    fn a_deliverable_message_validates() {
        assert!(deliverable().validate().is_ok());
    }

    #[test]
    fn validation_catches_each_way_a_message_can_be_undeliverable() {
        let mut no_recipients = deliverable();
        no_recipients.envelope.to.clear();
        assert!(no_recipients.validate().unwrap_err().message().contains("recipients"));

        let mut no_sender = deliverable();
        no_sender.envelope.from = None;
        assert!(no_sender.validate().unwrap_err().message().contains("sender"));

        let mut no_body = deliverable();
        no_body.text = None;
        assert!(no_body.validate().unwrap_err().message().contains("body"));

        let mut bad_address = deliverable();
        bad_address.envelope.to = vec![Address::new("not-an-address")];
        assert!(bad_address.validate().unwrap_err().message().contains("deliverable"));
    }

    #[test]
    fn headers_with_newlines_are_refused() {
        let message = deliverable()
            .with_header("X-Ok", "fine")
            .with_header("X-Bad", "value\r\nBcc: attacker@evil.test");

        assert_eq!(message.headers.len(), 1);
        assert_eq!(message.headers[0].0, "X-Ok");
    }

    #[test]
    fn the_text_body_falls_back_to_stripped_html() {
        let mut message = deliverable();
        message.text = None;
        message.html = Some("<p>Hello</p><p>Ada</p>".into());

        assert_eq!(message.text_body(), "Hello Ada");
    }

    #[test]
    fn an_explicit_text_body_wins_over_the_html() {
        let mut message = deliverable();
        message.html = Some("<p>html</p>".into());
        message.text = Some("plain".into());

        assert_eq!(message.text_body(), "plain");
    }

    #[test]
    fn stripping_tags_keeps_word_boundaries() {
        assert_eq!(strip_tags("<h1>A</h1><p>B</p>"), "A B");
        assert_eq!(strip_tags("no tags here"), "no tags here");
        assert_eq!(strip_tags("<br/>"), "");
    }
}
