//! A real SMTP server — [`SmtpTransport`], behind the `smtp` feature.
//!
//! ```ignore
//! let transport = SmtpTransport::builder("smtp.example.com")
//!     .credentials("postmaster", "secret")
//!     .encryption(MailEncryption::StartTls)
//!     .build()?;
//! ```
//!
//! Async over tokio and TLS over rustls, so the build stays free of a C
//! toolchain. The wire client is [`lettre`], which has spent a decade meeting
//! the servers that disagree about `AUTH`; what Rainier adds is the message
//! rendering it already owns — [`render_eml`](crate::render_eml()) is the `DATA`
//! payload — and configuration that fails at build time rather than at the
//! first send.
//!
//! # `Bcc` stays blind
//!
//! Recipients come from the SMTP envelope — `to`, `cc` **and** `bcc` — while
//! the rendered headers never contain `Bcc`. That split is the entire
//! mechanism of a blind copy, and it is decided here and in
//! [`render_eml`](crate::render_eml()), not by the server.

use std::time::Duration;

use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Tokio1Executor};
use rainier_support::{BoxFuture, Error, Result};

use crate::driver::MailEncryption;
use crate::message::Message;
use crate::transport::{render_eml, Transport};

/// Delivers over SMTP.
pub struct SmtpTransport {
    inner: AsyncSmtpTransport<Tokio1Executor>,
    host: String,
}

impl SmtpTransport {
    /// Start describing a connection to `host`.
    pub fn builder(host: impl Into<String>) -> SmtpBuilder {
        SmtpBuilder {
            host: host.into(),
            port: None,
            credentials: None,
            encryption: MailEncryption::default(),
            timeout: Duration::from_secs(30),
        }
    }
}

/// Everything an SMTP connection needs deciding, decided before the first
/// send. [`build`](SmtpBuilder::build) is where a bad host name or an
/// impossible TLS setup fails — at boot, with the configuration in hand,
/// rather than inside the request that first sends mail.
pub struct SmtpBuilder {
    host: String,
    port: Option<u16>,
    credentials: Option<(String, String)>,
    encryption: MailEncryption,
    timeout: Duration,
}

impl SmtpBuilder {
    /// Connect to this port instead of the one the encryption arrangement
    /// conventionally uses (587 for STARTTLS, 465 for TLS, 25 for neither).
    pub fn port(mut self, port: u16) -> Self {
        self.port = Some(port);
        self
    }

    /// Authenticate. Most relays require it; a capture container does not.
    pub fn credentials(mut self, username: impl Into<String>, password: impl Into<String>) -> Self {
        self.credentials = Some((username.into(), password.into()));
        self
    }

    /// How the connection is secured. The default is required STARTTLS.
    pub fn encryption(mut self, encryption: MailEncryption) -> Self {
        self.encryption = encryption;
        self
    }

    /// The wall clock on connecting and on each command. Thirty seconds by
    /// default — a mail server that takes longer is a mail server that is
    /// down, and the worker sending this has other jobs.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Build the transport.
    ///
    /// # Errors
    ///
    /// When the host is not a host — empty, or containing whitespace — or
    /// cannot anchor a TLS session. The failure a misconfigured deployment
    /// should meet at boot, not at the first send.
    pub fn build(self) -> Result<SmtpTransport> {
        if self.host.trim().is_empty() || self.host.contains(char::is_whitespace) {
            return Err(Error::internal(format!("`{}` is not an SMTP host", self.host)));
        }

        // rustls wants one process-level crypto provider. With only ring
        // compiled in it picks it alone; with the AWS SDK in the same binary,
        // aws-lc is also present and rustls refuses to guess between them —
        // by aborting, at the worst possible moment. Ring, explicitly; if
        // something already installed a provider, whoever won is fine.
        let _ = rustls::crypto::ring::default_provider().install_default();

        let mut builder = match self.encryption {
            MailEncryption::Tls => AsyncSmtpTransport::<Tokio1Executor>::relay(&self.host),
            MailEncryption::StartTls => {
                AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&self.host)
            }
            MailEncryption::None => {
                Ok(AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&self.host))
            }
        }
        .map_err(|e| Error::internal(format!("`{}` cannot be an SMTP relay: {e}", self.host)))?;

        if let Some(port) = self.port {
            builder = builder.port(port);
        }
        if let Some((username, password)) = self.credentials {
            builder = builder.credentials(Credentials::new(username, password));
        }

        Ok(SmtpTransport { inner: builder.timeout(Some(self.timeout)).build(), host: self.host })
    }
}

impl Transport for SmtpTransport {
    fn name(&self) -> &str {
        "smtp"
    }

    fn send<'a>(&'a self, message: &'a Message) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let envelope = smtp_envelope(message)?;
            let data = crlf(&render_eml(message));

            let response = self.inner.send_raw(&envelope, data.as_bytes()).await.map_err(|e| {
                // The host is in the message on purpose: "connection
                // refused" with no address is the least useful line in any
                // log.
                Error::service_unavailable(format!("{} did not accept the message: {e}", self.host))
            })?;

            if !response.is_positive() {
                return Err(Error::service_unavailable(format!(
                    "{} answered {}",
                    self.host,
                    response.code()
                )));
            }
            Ok(())
        })
    }
}

impl std::fmt::Debug for SmtpTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SmtpTransport").field("host", &self.host).finish()
    }
}

/// The SMTP envelope: the return path and every recipient — including `bcc`,
/// which is exactly where a blind copy travels.
fn smtp_envelope(message: &Message) -> Result<lettre::address::Envelope> {
    let from = match &message.envelope.from {
        Some(address) => Some(parse(&address.email)?),
        // The mailer applies the configured default before any transport runs,
        // so this is a message built outside the mailer. Refusing beats letting
        // the relay guess a return path.
        None => return Err(Error::internal("an SMTP message needs a From address")),
    };

    let recipients = message
        .envelope
        .recipients()
        .iter()
        .map(|address| parse(&address.email))
        .collect::<Result<Vec<_>>>()?;

    lettre::address::Envelope::new(from, recipients)
        .map_err(|e| Error::internal(format!("the envelope is not sendable: {e}")))
}

fn parse(email: &str) -> Result<lettre::Address> {
    email.parse().map_err(|_| Error::internal(format!("`{email}` is not a mailable address")))
}

/// SMTP `DATA` requires CRLF line endings; the renderer writes `\n`.
/// Normalised here — the one place that knows it is speaking SMTP.
fn crlf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\n', "\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{Address, Envelope};

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
    fn the_envelope_carries_every_recipient_including_bcc() {
        let envelope = smtp_envelope(&message()).unwrap();

        let recipients: Vec<String> =
            envelope.to().iter().map(std::string::ToString::to_string).collect();
        assert_eq!(recipients, ["ada@example.com", "cc@example.com", "secret@example.com"]);
        assert_eq!(envelope.from().unwrap().to_string(), "app@example.com");
    }

    #[test]
    fn a_message_without_a_sender_is_refused() {
        let mut message = message();
        message.envelope.from = None;

        let err = smtp_envelope(&message).unwrap_err();
        assert!(err.message().contains("From"), "{}", err.message());
    }

    #[test]
    fn an_unparseable_recipient_is_named_in_the_error() {
        let mut message = message();
        message.envelope.to.push(Address::new("not an address"));

        let err = smtp_envelope(&message).unwrap_err();
        assert!(err.message().contains("not an address"), "{}", err.message());
    }

    #[test]
    fn line_endings_are_normalised_to_crlf_exactly_once() {
        assert_eq!(crlf("a\nb"), "a\r\nb");
        assert_eq!(crlf("a\r\nb"), "a\r\nb", "already-correct endings are not doubled");
        assert_eq!(crlf("a\nb\r\nc\n"), "a\r\nb\r\nc\r\n");
    }

    #[test]
    fn a_bad_relay_host_fails_at_build_time() {
        // The deployment mistake should be met at boot, not at the first send.
        let err = SmtpTransport::builder("not a host name").build().unwrap_err();
        assert!(err.message().contains("not a host name"), "{}", err.message());
    }

    #[test]
    fn a_capture_container_needs_no_tls_and_no_credentials() {
        let transport = SmtpTransport::builder("localhost")
            .port(1025)
            .encryption(MailEncryption::None)
            .build()
            .unwrap();

        assert_eq!(transport.name(), "smtp");
    }
}
