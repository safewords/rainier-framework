//! Transports — the [`Transport`] port and the drivers Rainier ships.

use std::path::PathBuf;
use std::sync::Mutex;

use rainier_support::{BoxFuture, Error, Result};

use crate::message::Message;

/// Delivers a message.
pub trait Transport: Send + Sync + 'static {
    /// A label for diagnostics — `"log"`, `"smtp"`.
    fn name(&self) -> &str;

    /// Deliver `message`.
    fn send<'a>(&'a self, message: &'a Message) -> BoxFuture<'a, Result<()>>;
}

/// Writes messages to the log instead of sending them.
///
/// The right default for development: an application under development sends
/// plenty of mail, and none of it should reach a real inbox.
#[derive(Debug, Default)]
pub struct LogTransport;

impl Transport for LogTransport {
    fn name(&self) -> &str {
        "log"
    }

    fn send<'a>(&'a self, message: &'a Message) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            tracing::info!(
                to = %message
                    .envelope
                    .to
                    .iter()
                    .map(|a| a.email.clone())
                    .collect::<Vec<_>>()
                    .join(", "),
                subject = %message.envelope.subject,
                body = %message.text_body(),
                "mail (log transport — not actually sent)"
            );
            Ok(())
        })
    }
}

/// Keeps messages in memory so a test can assert on them.
#[derive(Debug, Default)]
pub struct MemoryTransport {
    sent: Mutex<Vec<Message>>,
    /// When set, every send fails with this message.
    failure: Option<String>,
}

impl MemoryTransport {
    /// A transport that accepts everything.
    pub fn new() -> Self {
        Self::default()
    }

    /// A transport that rejects everything — for testing failure handling.
    pub fn failing(message: impl Into<String>) -> Self {
        Self { sent: Mutex::new(Vec::new()), failure: Some(message.into()) }
    }

    /// Every message sent so far, in order.
    pub fn sent(&self) -> Vec<Message> {
        self.sent.lock().expect("transport lock poisoned").clone()
    }

    /// How many messages were sent.
    pub fn count(&self) -> usize {
        self.sent.lock().expect("transport lock poisoned").len()
    }

    /// Forget everything sent so far.
    pub fn clear(&self) {
        self.sent.lock().expect("transport lock poisoned").clear();
    }
}

impl Transport for MemoryTransport {
    fn name(&self) -> &str {
        "memory"
    }

    fn send<'a>(&'a self, message: &'a Message) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if let Some(failure) = &self.failure {
                return Err(Error::internal(failure.clone()));
            }
            self.sent.lock().expect("transport lock poisoned").push(message.clone());
            Ok(())
        })
    }
}

/// Writes each message to a file, as `.eml`.
///
/// Useful when a developer wants to open the real rendered HTML in a browser,
/// which the log transport cannot give them.
#[derive(Debug)]
pub struct FileTransport {
    directory: PathBuf,
}

impl FileTransport {
    /// Write messages into `directory`, creating it if needed.
    pub fn new(directory: impl Into<PathBuf>) -> Result<Self> {
        let directory = directory.into();
        std::fs::create_dir_all(&directory).map_err(|e| {
            Error::internal(format!("could not create {}: {e}", directory.display()))
        })?;
        Ok(Self { directory })
    }

    /// Where messages are written.
    pub fn directory(&self) -> &std::path::Path {
        &self.directory
    }
}

impl Transport for FileTransport {
    fn name(&self) -> &str {
        "file"
    }

    fn send<'a>(&'a self, message: &'a Message) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S%.6f");
            let path = self.directory.join(format!("{stamp}.eml"));

            std::fs::write(&path, render_eml(message))
                .map_err(|e| Error::internal(format!("could not write {}: {e}", path.display())))?;
            Ok(())
        })
    }
}

/// Render a message as RFC 5322 text.
///
/// Enough for a `.eml` a mail client will open, and for the SMTP `DATA`
/// payload. Multipart is emitted only when there is more than one part to
/// carry, so a plain-text message stays a plain-text message.
pub fn render_eml(message: &Message) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();

    if let Some(from) = &message.envelope.from {
        let _ = writeln!(out, "From: {}", from.to_header());
    }
    let _ = writeln!(out, "To: {}", join(&message.envelope.to));
    if !message.envelope.cc.is_empty() {
        let _ = writeln!(out, "Cc: {}", join(&message.envelope.cc));
    }
    if !message.envelope.reply_to.is_empty() {
        let _ = writeln!(out, "Reply-To: {}", join(&message.envelope.reply_to));
    }
    // `Bcc` is deliberately absent: the whole point of a blind copy is that
    // the header does not travel with the message. Recipients come from the
    // SMTP envelope instead.
    let _ = writeln!(out, "Subject: {}", sanitize_header(&message.envelope.subject));
    let _ = writeln!(out, "Date: {}", chrono::Utc::now().format("%a, %d %b %Y %H:%M:%S +0000"));
    let _ = writeln!(out, "MIME-Version: 1.0");

    for (name, value) in &message.headers {
        let _ = writeln!(out, "{}: {}", sanitize_header(name), sanitize_header(value));
    }

    let parts = message.html.is_some() as usize
        + message.text.is_some() as usize
        + message.attachments.len();

    if parts <= 1 && message.attachments.is_empty() {
        let (content_type, body) = match (&message.html, &message.text) {
            (Some(html), _) => ("text/html; charset=utf-8", html.clone()),
            (None, Some(text)) => ("text/plain; charset=utf-8", text.clone()),
            (None, None) => ("text/plain; charset=utf-8", String::new()),
        };
        let _ = writeln!(out, "Content-Type: {content_type}\r\n");
        out.push_str(&body);
        return out;
    }

    let boundary = format!("rainier-{}", chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0));
    let _ = writeln!(out, "Content-Type: multipart/mixed; boundary=\"{boundary}\"\r\n");

    if let Some(text) = &message.text {
        let _ = writeln!(out, "--{boundary}");
        let _ = writeln!(out, "Content-Type: text/plain; charset=utf-8\r\n");
        let _ = writeln!(out, "{text}");
    }
    if let Some(html) = &message.html {
        let _ = writeln!(out, "--{boundary}");
        let _ = writeln!(out, "Content-Type: text/html; charset=utf-8\r\n");
        let _ = writeln!(out, "{html}");
    }
    for attachment in &message.attachments {
        let _ = writeln!(out, "--{boundary}");
        let _ = writeln!(out, "Content-Type: {}", attachment.content_type);
        let _ = writeln!(out, "Content-Transfer-Encoding: base64");
        let _ = writeln!(
            out,
            "Content-Disposition: attachment; filename=\"{}\"\r\n",
            sanitize_header(&attachment.file_name).replace('"', "")
        );
        let _ = writeln!(out, "{}", attachment.to_base64());
    }
    let _ = writeln!(out, "--{boundary}--");

    out
}

fn join(addresses: &[crate::message::Address]) -> String {
    addresses.iter().map(|address| address.to_header()).collect::<Vec<_>>().join(", ")
}

/// Strip CR and LF, which are how a header injection is spelled.
fn sanitize_header(value: &str) -> String {
    value.replace(['\r', '\n'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{Address, Attachment, Envelope};

    fn message() -> Message {
        let mut message = Message::new(
            Envelope::new("Hello")
                .from(Address::named("app@example.com", "App"))
                .to("ada@example.com"),
        );
        message.text = Some("Hi Ada".into());
        message
    }

    #[tokio::test]
    async fn the_memory_transport_keeps_what_it_was_given() {
        let transport = MemoryTransport::new();

        transport.send(&message()).await.unwrap();
        assert_eq!(transport.count(), 1);
        assert_eq!(transport.sent()[0].envelope.subject, "Hello");

        transport.clear();
        assert_eq!(transport.count(), 0);
    }

    #[tokio::test]
    async fn a_failing_transport_reports_why() {
        let transport = MemoryTransport::failing("connection refused");
        let err = transport.send(&message()).await.unwrap_err();

        assert!(err.message().contains("connection refused"));
        assert_eq!(transport.count(), 0);
    }

    #[tokio::test]
    async fn the_log_transport_accepts_everything() {
        assert!(LogTransport.send(&message()).await.is_ok());
        assert_eq!(LogTransport.name(), "log");
    }

    #[tokio::test]
    async fn the_file_transport_writes_an_eml() {
        let directory = std::env::temp_dir().join("rainier-mail-file-transport");
        let _ = std::fs::remove_dir_all(&directory);

        let transport = FileTransport::new(&directory).unwrap();
        transport.send(&message()).await.unwrap();

        let written: Vec<_> = std::fs::read_dir(&directory).unwrap().flatten().collect();
        assert_eq!(written.len(), 1);

        let contents = std::fs::read_to_string(written[0].path()).unwrap();
        assert!(contents.contains("Subject: Hello"), "{contents}");
        assert!(contents.contains("Hi Ada"), "{contents}");
    }

    #[test]
    fn a_simple_message_renders_as_a_single_part() {
        let eml = render_eml(&message());

        assert!(eml.contains("From: App <app@example.com>"), "{eml}");
        assert!(eml.contains("To: ada@example.com"), "{eml}");
        assert!(eml.contains("Subject: Hello"), "{eml}");
        assert!(eml.contains("Content-Type: text/plain"), "{eml}");
        assert!(!eml.contains("multipart"), "{eml}");
    }

    #[test]
    fn html_and_text_together_render_as_multipart() {
        let mut message = message();
        message.html = Some("<p>Hi</p>".into());

        let eml = render_eml(&message);
        assert!(eml.contains("multipart/mixed"), "{eml}");
        assert!(eml.contains("text/plain"), "{eml}");
        assert!(eml.contains("text/html"), "{eml}");
    }

    #[test]
    fn attachments_render_as_base64_parts() {
        let mut message = message();
        message.attachments = vec![Attachment::from_bytes("a.txt", "text/plain", b"data".to_vec())];

        let eml = render_eml(&message);
        assert!(eml.contains("multipart/mixed"), "{eml}");
        assert!(eml.contains("Content-Transfer-Encoding: base64"), "{eml}");
        assert!(eml.contains("filename=\"a.txt\""), "{eml}");
        assert!(eml.contains("ZGF0YQ=="), "{eml}");
    }

    #[test]
    fn bcc_never_appears_in_the_rendered_headers() {
        // The entire point of a blind copy.
        let mut message = message();
        message.envelope.bcc.push(Address::new("secret@example.com"));

        let eml = render_eml(&message);
        assert!(!eml.contains("Bcc"), "{eml}");
        assert!(!eml.contains("secret@example.com"), "{eml}");
    }

    #[test]
    fn cc_and_reply_to_do_appear() {
        let mut message = message();
        message.envelope.cc.push(Address::new("cc@example.com"));
        message.envelope.reply_to.push(Address::new("support@example.com"));

        let eml = render_eml(&message);
        assert!(eml.contains("Cc: cc@example.com"), "{eml}");
        assert!(eml.contains("Reply-To: support@example.com"), "{eml}");
    }

    #[test]
    fn newlines_are_stripped_from_rendered_headers() {
        let mut message = message();
        message.envelope.subject = "Hello\r\nBcc: attacker@evil.test".into();

        let eml = render_eml(&message);
        let subject_line =
            eml.lines().find(|line| line.starts_with("Subject:")).expect("a subject line");
        assert!(subject_line.contains("attacker"), "the text is kept…");
        assert!(!eml.contains("\nBcc:"), "…but it cannot become a header: {eml}");
    }
}
