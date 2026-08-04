//! Cloudflare Email Service — [`CloudflareEmailConnector`].
//!
//! ```ignore
//! let connector = CloudflareEmailConnector::open("<api-token>")?;
//! ```
//!
//! # Why this is a connector and not a transport
//!
//! The service speaks ordinary SMTP, so there is nothing a second mail
//! transport would do differently. What is specific to Cloudflare is the
//! *connection*: where it is, which port, and the SASL identity. That belongs
//! here with the other connectors, and building a transport over it belongs to
//! the mail crate — which already has one, tested, with `DATA` rendering and
//! blind-copy handling this has no business repeating.
//!
//! What it removes is the chance of getting those four wrong, and three of them
//! fail in ways that do not look like configuration:
//!
//! | Setting | Value | What a wrong value looks like |
//! |---|---|---|
//! | host | `smtp.mx.cloudflare.net` | connection refused |
//! | port | `465` | a hang, then a timeout — 587 is not listening |
//! | encryption | implicit TLS | a hang; the server never offers `STARTTLS` |
//! | username | the literal `api_token` | `535 5.7.8`, which reads as a bad token |
//!
//! That last one is the reason this exists. The username is not the account,
//! the email address, or the token — it is the fixed string `api_token`, and
//! the token goes in the password. Getting it wrong produces an authentication
//! failure indistinguishable from a revoked credential, so the obvious next
//! step is to reissue a token that was never the problem.
//!
//! # Limits the service enforces, which this does not
//!
//! Per session: **50 recipients**, **5 MiB** per message, 30s to authenticate
//! and 300s to send. Exceeding them is refused by the server — `552 5.3.4` for
//! size — rather than checked here, because a limit enforced in two places
//! drifts, and the copy that is wrong is the one nobody updated.
//!
//! [`explain`] maps the documented replies to something a reader can act on.
//!
//! # The sender domain has to be onboarded
//!
//! `MAIL FROM` must use a domain onboarded for Email Sending on the account the
//! token belongs to. A domain that is not gets `550 5.7.1`, which says "relay
//! denied" and means "this domain is not yours yet".

use std::time::Duration;

use rainier_support::{Error, Result};

/// The service's SMTP endpoint. Not configurable — there is one.
pub const HOST: &str = "smtp.mx.cloudflare.net";

/// Implicit TLS. Cloudflare does not listen on 587 and does not offer
/// `STARTTLS`, so this is the only port that connects.
pub const PORT: u16 = 465;

/// The SASL username, which is this exact string for every account.
///
/// The API token is the *password*. See the module docs on why this is the
/// setting most worth not hand-writing.
pub const USERNAME: &str = "api_token";

/// Largest message the service accepts, per its documentation — 5 MiB.
///
/// Here to be quoted in an error rather than enforced: the server refuses
/// oversize mail itself with `552 5.3.4`, and a limit checked in two places
/// drifts.
pub const MAX_MESSAGE_BYTES: usize = 5 * 1024 * 1024;

/// Recipients the service accepts per session.
pub const MAX_RECIPIENTS_PER_SESSION: usize = 50;

/// How long a client should wait on the whole exchange.
///
/// The service's own timeouts are 30s to authenticate and 300s to send, so
/// this sits above the first and below the second: long enough that a slow
/// handshake is not cut off, short enough that a stalled `DATA` does not hold
/// a worker for five minutes.
pub const TIMEOUT: Duration = Duration::from_secs(60);

/// A validated connection to Cloudflare Email Service.
///
/// Holds what the *service* fixes — endpoint, port, SASL username — so a
/// caller supplies only what is theirs, the API token. Turning this into a
/// mail transport is [`rainier_mail`](https://docs.rs/rainier-mail)'s job; this
/// crate knows the endpoint, not how mail is sent.
#[derive(Clone)]
pub struct CloudflareEmailConnector {
    token: String,
}

impl CloudflareEmailConnector {
    /// Validate an API token and describe the connection it opens.
    ///
    /// `api_token` needs the **Email Sending: Edit** permission. A token
    /// without it authenticates and then cannot send, which surfaces as `550`
    /// on the first message rather than here.
    pub fn open(api_token: impl Into<String>) -> Result<Self> {
        let token = api_token.into();

        // Refused here rather than at the first send: SASL PLAIN with an empty
        // password is a well-formed exchange, so the server answers
        // `535 5.7.8` and the log reads "authentication failed" — pointing at
        // the token rather than at its absence, and sending somebody to
        // reissue a credential that was never the problem.
        if token.trim().is_empty() {
            return Err(Error::internal(
                "Cloudflare Email Service needs an API token with `Email Sending: Edit`;                  the value given is empty. It is the SMTP *password* — the username is                  always the literal string `api_token`.",
            ));
        }

        Ok(Self { token })
    }

    /// The SMTP host. Not configurable — there is one.
    pub fn host(&self) -> &'static str {
        HOST
    }

    /// The port. 465, and only 465.
    pub fn port(&self) -> u16 {
        PORT
    }

    /// The SASL username, which is this string for every account.
    pub fn username(&self) -> &'static str {
        USERNAME
    }

    /// The API token, which is the SASL password.
    pub fn token(&self) -> &str {
        &self.token
    }

    /// How long to allow the whole exchange.
    pub fn timeout(&self) -> Duration {
        TIMEOUT
    }
}

/// What Cloudflare's documented SMTP replies mean, for an error a reader can
/// act on.
///
/// The codes are standard and the *meanings* are not: `550 5.7.1` is spelled
/// "relay denied", which reads as a permissions problem with the token when it
/// is nearly always a domain that has not been onboarded.
pub fn explain(code: u16, enhanced: &str) -> Option<&'static str> {
    match (code, enhanced) {
        (235, "2.7.0") => Some("authentication succeeded"),
        (535, "5.7.8") => Some(
            "authentication failed — check the token carries `Email Sending: Edit`, and that \
             the SMTP username is the literal string `api_token` rather than an address",
        ),
        (550, "5.7.1") => Some(
            "sender denied — the `MAIL FROM` domain is not onboarded for Email Sending on the \
             account this token belongs to. Usually a domain, not a token, problem",
        ),
        (552, "5.3.4") => Some("message is larger than the service's 5 MiB limit"),
        _ => None,
    }
}

impl std::fmt::Debug for CloudflareEmailConnector {
    /// Renders without the token.
    ///
    /// A connector reaches logs and panic messages by every route a struct
    /// does, and an API token in either is a credential leak that survives in
    /// whatever collects them.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CloudflareEmailConnector")
            .field("host", &HOST)
            .field("port", &PORT)
            .field("username", &USERNAME)
            .field("token", &"<redacted>")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_endpoint_is_the_documented_one() {
        // Pinned because every one of these is a value somebody would
        // otherwise copy from memory, and three of the four fail as a hang or
        // as "bad token" rather than as a wrong setting.
        assert_eq!(HOST, "smtp.mx.cloudflare.net");
        assert_eq!(PORT, 465);
        assert_eq!(USERNAME, "api_token");
    }

    #[test]
    fn an_empty_token_is_refused_at_build_time() {
        let error =
            CloudflareEmailConnector::open("   ").expect_err("an empty token cannot authenticate");

        // The message has to say the token is the password, because the
        // failure it prevents — `535` from an empty SASL password — reads as a
        // revoked credential and sends someone to reissue a working token.
        assert!(error.message().contains("api_token"), "{}", error.message());
    }

    #[test]
    fn the_documented_replies_are_explained() {
        assert!(explain(550, "5.7.1").unwrap().contains("onboarded"));
        assert!(explain(535, "5.7.8").unwrap().contains("api_token"));
        assert!(explain(552, "5.3.4").unwrap().contains("5 MiB"));
    }

    #[test]
    fn an_unknown_reply_is_not_invented() {
        // Better silent than confidently wrong: a made-up explanation for a
        // code the service may add later would send a reader the wrong way.
        assert!(explain(451, "4.3.0").is_none());
    }

    #[test]
    fn a_token_never_reaches_a_rendering_of_the_connector() {
        let connector = CloudflareEmailConnector::open("secret-token").unwrap();
        let rendered = format!("{connector:?}");

        assert!(!rendered.contains("secret-token"), "{rendered}");
        // Not vacuous — the endpoint itself does render.
        assert!(rendered.contains("smtp.mx.cloudflare.net"), "{rendered}");
    }

    #[test]
    fn the_limits_match_the_published_ones() {
        assert_eq!(MAX_MESSAGE_BYTES, 5 * 1024 * 1024);
        assert_eq!(MAX_RECIPIENTS_PER_SESSION, 50);
    }
}
