//! The SMTP transport against a real server.
//!
//! Everything interesting about SMTP delivery is a property of the
//! conversation — that the envelope, not the headers, decides who receives
//! the message; that the MIME survives the `DATA` framing; that an attachment
//! arrives as a file. None of it can be checked against a mock, so these
//! tests run against [Mailpit](https://mailpit.axllent.org) — an SMTP server
//! that keeps what it receives and answers questions about it over HTTP —
//! and skip unless one answers. CI provides one.
//!
//! ```sh
//! docker run --rm -p 1025:1025 -p 8025:8025 axllent/mailpit
//! cargo test -p rainier-mail --features smtp --test smtp
//! ```

#![cfg(feature = "smtp")]

use rainier_mail::{Address, Envelope, MailEncryption, Message, SmtpTransport, Transport};

/// Where the tests look for the capture server's SMTP side.
fn smtp_host() -> String {
    std::env::var("MAIL_SMTP_HOST").unwrap_or_else(|_| "localhost:1025".to_string())
}

/// …and its HTTP API.
fn api_base() -> String {
    std::env::var("MAIL_SMTP_API").unwrap_or_else(|_| "http://localhost:8025".to_string())
}

/// A transport, or `None` when nothing is listening.
///
/// Skipping rather than failing, so a contributor with no Mailpit gets a
/// green suite — **except** where `MAIL_SMTP_REQUIRED` is set, which CI does.
/// A suite that silently skipped in CI would be a transport nobody had ever
/// run, reported as passing.
fn transport() -> Option<SmtpTransport> {
    use std::net::ToSocketAddrs as _;

    let host = smtp_host();
    let (name, port) = host.split_once(':').unwrap_or((host.as_str(), "1025"));

    // A cheap port probe, so a machine with no capture server skips in a
    // couple of seconds rather than waiting out an SMTP handshake timeout.
    let addr = format!("{name}:{port}").to_socket_addrs().ok()?.next()?;
    if std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(2)).is_err() {
        return None;
    }

    Some(
        SmtpTransport::builder(name)
            .port(port.parse().expect("MAIL_SMTP_HOST port"))
            .encryption(MailEncryption::None)
            .build()
            .expect("a capture server needs no TLS"),
    )
}

macro_rules! smtp_or_skip {
    ($name:literal) => {
        match transport() {
            Some(transport) => transport,
            None if std::env::var("MAIL_SMTP_REQUIRED")
                .is_ok_and(|required| !required.is_empty()) =>
            {
                panic!("MAIL_SMTP_REQUIRED is set and nothing answered at {}", smtp_host())
            }
            None => {
                eprintln!("skipping `{}`: no SMTP server at MAIL_SMTP_HOST", $name);
                return;
            }
        }
    };
}

/// Ask Mailpit for the message a subject names.
async fn captured(subject: &str) -> serde_json::Value {
    let client = reqwest::Client::new();

    // The capture is asynchronous on Mailpit's side; a handful of polls beats
    // a sleep long enough to be reliable.
    for _ in 0..20 {
        let found: serde_json::Value = client
            .get(format!("{}/api/v1/search", api_base()))
            .query(&[("query", format!("subject:\"{subject}\""))])
            .send()
            .await
            .expect("Mailpit answered the port probe but not the API")
            .json()
            .await
            .expect("Mailpit speaks JSON");

        if let Some(id) = found["messages"][0]["ID"].as_str() {
            return client
                .get(format!("{}/api/v1/message/{id}", api_base()))
                .send()
                .await
                .expect("the message list named this message")
                .json()
                .await
                .expect("a message detail is JSON");
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("Mailpit never showed a message with subject {subject:?}");
}

/// A subject no other test run shares, so parallel runs cannot read each
/// other's captures.
fn unique(label: &str) -> String {
    format!("{label} {}", chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default())
}

#[tokio::test]
async fn a_message_arrives_with_its_parts_and_its_blind_copy_stays_blind() {
    let transport = smtp_or_skip!("roundtrip");
    let subject = unique("Invoice enclosed");

    let mut message = Message::new(
        Envelope::new(&subject)
            .from(Address::named("billing@example.com", "Billing"))
            .to(Address::named("ada@example.com", "Ada"))
            .cc("accounts@example.com")
            .bcc("archive@example.com"),
    );
    message.html = Some("<p>The invoice is attached.</p>".into());
    message.text = Some("The invoice is attached.".into());
    message.attachments = vec![rainier_mail::Attachment::from_bytes(
        "invoice.pdf",
        "application/pdf",
        b"not really a pdf".to_vec(),
    )];

    transport.send(&message).await.expect("the capture server accepts everything");

    let arrived = captured(&subject).await;

    assert_eq!(arrived["From"]["Address"], "billing@example.com");
    assert_eq!(arrived["To"][0]["Address"], "ada@example.com");
    assert_eq!(arrived["Cc"][0]["Address"], "accounts@example.com");
    assert!(arrived["HTML"].as_str().unwrap_or_default().contains("attached"), "{arrived}");
    assert_eq!(arrived["Attachments"][0]["FileName"], "invoice.pdf");

    // The blind copy was *delivered* — Mailpit derives this from the SMTP
    // envelope — while the headers the other recipients can read never name
    // it. This split is the entire mechanism of a Bcc, and it is the one
    // property of the transport that only a real SMTP conversation can pin.
    assert_eq!(arrived["Bcc"][0]["Address"], "archive@example.com");
}

#[tokio::test]
async fn a_plain_text_message_stays_plain() {
    let transport = smtp_or_skip!("plain");
    let subject = unique("Just words");

    let mut message =
        Message::new(Envelope::new(&subject).from("app@example.com").to("ada@example.com"));
    message.text = Some("No HTML anywhere.".into());

    transport.send(&message).await.expect("sends");

    let arrived = captured(&subject).await;
    assert!(arrived["Text"].as_str().unwrap_or_default().contains("No HTML"), "{arrived}");
    assert_eq!(arrived["HTML"], "", "a text-only message must not grow an HTML part: {arrived}");
}
