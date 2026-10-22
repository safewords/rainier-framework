//! Getting a link to the person — [`SendsPasswordResetLinks`],
//! [`SendsVerificationLinks`], and the mail-backed defaults.
//!
//! The flows build the link; *delivering* it is a port, because "send an
//! email now" is only one answer. An application that already queues its
//! mail, sends a templated message through its provider, or texts a code
//! implements the trait and hands it over.
//!
//! # Queue it, if you can
//!
//! `POST /forgot-password` answers identically whether or not the address has
//! an account. Identical *words* — but a sender that talks to an SMTP server
//! inline takes a few hundred milliseconds that the unknown-address path does
//! not, and that difference is measurable from outside. A sender that pushes a
//! job onto a queue and returns closes it. The mail-backed defaults here send
//! inline, because they have no queue to reach for; they are right for getting
//! started and worth replacing before an attacker would care.

use std::sync::Arc;

use rainier_mail::{Content, Envelope, Mailable, Mailer};
use rainier_support::Result;

use crate::users::AccountUser;

/// Delivers a password reset link.
#[async_trait::async_trait]
pub trait SendsPasswordResetLinks<U: AccountUser>: Send + Sync + 'static {
    /// Send `url` to `user`.
    async fn send_reset_link(&self, user: &U, url: &str) -> Result<()>;
}

/// Delivers an email verification link.
#[async_trait::async_trait]
pub trait SendsVerificationLinks<U: AccountUser>: Send + Sync + 'static {
    /// Send `url` to `user`.
    async fn send_verification_link(&self, user: &U, url: &str) -> Result<()>;
}

/// The reset link, as an email.
#[derive(Debug, Clone)]
pub struct ResetPasswordMail {
    /// Who it goes to.
    pub to: String,
    /// The link.
    pub url: String,
    /// The application's name, for the subject line.
    pub app_name: String,
    /// How many minutes the link works for, to say so.
    pub expires_in_minutes: u64,
}

impl Mailable for ResetPasswordMail {
    fn envelope(&self) -> Envelope {
        Envelope::new(format!("Reset your {} password", self.app_name)).to(self.to.clone())
    }

    fn content(&self) -> Result<Content> {
        let url = escape(&self.url);
        Ok(Content::Literal {
            html: Some(format!(
                "<p>Somebody asked to reset the password for this account.</p>\
                 <p><a href=\"{url}\">Choose a new password</a></p>\
                 <p>The link works for {} minutes. If it was not you, ignore this email — \
                 your password has not changed.</p>",
                self.expires_in_minutes
            )),
            text: Some(format!(
                "Somebody asked to reset the password for this account.\n\n\
                 Choose a new password: {}\n\n\
                 The link works for {} minutes. If it was not you, ignore this email — \
                 your password has not changed.\n",
                self.url, self.expires_in_minutes
            )),
        })
    }
}

/// The verification link, as an email.
#[derive(Debug, Clone)]
pub struct VerifyEmailMail {
    /// Who it goes to.
    pub to: String,
    /// The link.
    pub url: String,
    /// The application's name, for the subject line.
    pub app_name: String,
}

impl Mailable for VerifyEmailMail {
    fn envelope(&self) -> Envelope {
        Envelope::new(format!("Verify your {} email address", self.app_name)).to(self.to.clone())
    }

    fn content(&self) -> Result<Content> {
        let url = escape(&self.url);
        Ok(Content::Literal {
            html: Some(format!(
                "<p>Confirm this is your address.</p><p><a href=\"{url}\">Verify email \
                 address</a></p><p>If you did not create an account, ignore this email.</p>"
            )),
            text: Some(format!(
                "Confirm this is your address: {}\n\nIf you did not create an account, ignore \
                 this email.\n",
                self.url
            )),
        })
    }
}

/// Sends both links as email through a [`Mailer`].
#[derive(Clone)]
pub struct MailLinks {
    mailer: Arc<Mailer>,
    app_name: String,
    reset_minutes: u64,
}

impl MailLinks {
    /// Send through `mailer`, naming the application `app_name` in subjects.
    pub fn new(mailer: Arc<Mailer>, app_name: impl Into<String>) -> Self {
        Self { mailer, app_name: app_name.into(), reset_minutes: 60 }
    }

    /// The reset link lifetime to quote in the email.
    ///
    /// [`Accounts`](crate::Accounts) sets this from its own
    /// [`Features::reset_token_ttl`](crate::Features::reset_token_ttl), so the
    /// email cannot promise longer than the link actually lasts.
    #[must_use]
    pub fn reset_link_minutes(mut self, minutes: u64) -> Self {
        self.reset_minutes = minutes;
        self
    }
}

impl std::fmt::Debug for MailLinks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MailLinks").field("app_name", &self.app_name).finish()
    }
}

#[async_trait::async_trait]
impl<U: AccountUser> SendsPasswordResetLinks<U> for MailLinks {
    async fn send_reset_link(&self, user: &U, url: &str) -> Result<()> {
        self.mailer
            .send(&ResetPasswordMail {
                to: user.email().to_string(),
                url: url.to_string(),
                app_name: self.app_name.clone(),
                expires_in_minutes: self.reset_minutes,
            })
            .await
            .map(drop)
    }
}

#[async_trait::async_trait]
impl<U: AccountUser> SendsVerificationLinks<U> for MailLinks {
    async fn send_verification_link(&self, user: &U, url: &str) -> Result<()> {
        self.mailer
            .send(&VerifyEmailMail {
                to: user.email().to_string(),
                url: url.to_string(),
                app_name: self.app_name.clone(),
            })
            .await
            .map(drop)
    }
}

/// Enough HTML escaping for a URL inside an attribute.
fn escape(value: &str) -> String {
    value.replace('&', "&amp;").replace('"', "&quot;").replace('<', "&lt;").replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reset_mail_carries_the_link_escaped_in_html_and_raw_in_text() {
        let mail = ResetPasswordMail {
            to: "ada@example.com".into(),
            url: "https://app.test/reset?token=a&email=b".into(),
            app_name: "Rainier".into(),
            expires_in_minutes: 60,
        };

        assert_eq!(mail.envelope().subject, "Reset your Rainier password");
        let Content::Literal { html, text } = mail.content().unwrap() else { unreachable!() };
        assert!(html.unwrap().contains("token=a&amp;email=b"));
        assert!(text.unwrap().contains("token=a&email=b"));
    }
}
