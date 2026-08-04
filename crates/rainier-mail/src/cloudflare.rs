//! Cloudflare Email Service as a mail transport — [`cloudflare_smtp`], behind
//! the `smtp` feature.
//!
//! ```ignore
//! let transport = cloudflare_smtp("<api-token>")?;
//! ```
//!
//! The endpoint, port, SASL identity and timeout come from
//! [`CloudflareEmailConnector`] in the drivers crate. This is the eight lines
//! that turn one into the SMTP transport this crate already has — reusing its
//! client, its `DATA` rendering and its blind-copy handling rather than
//! growing a second path that would have to be kept level with the first.

use rainier_drivers::CloudflareEmailConnector;
use rainier_support::Result;

use crate::driver::MailEncryption;
use crate::smtp::SmtpTransport;

/// Build a transport for Cloudflare Email Service.
///
/// `api_token` needs the **Email Sending: Edit** permission. It is the SMTP
/// *password*; the username is the literal string `api_token`, which the
/// connector supplies so nobody has to know it.
pub fn cloudflare_smtp(api_token: impl Into<String>) -> Result<SmtpTransport> {
    let connector = CloudflareEmailConnector::open(api_token)?;

    SmtpTransport::builder(connector.host())
        .port(connector.port())
        // Implicit TLS, not STARTTLS. The service does not listen on 587 and
        // never offers the upgrade, so anything else is a hang rather than a
        // refusal.
        .encryption(MailEncryption::Tls)
        .credentials(connector.username(), connector.token())
        .timeout(connector.timeout())
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_token_is_refused_before_a_transport_exists() {
        // The connector owns this check; asserting it here pins that the
        // preset does not build around it.
        assert!(cloudflare_smtp("  ").is_err());
    }
}
