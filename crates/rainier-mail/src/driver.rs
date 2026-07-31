//! Where mail goes — [`MailDriver`] — and how an SMTP connection is secured —
//! [`MailEncryption`].

use rainier_support::setting_enum;

setting_enum! {
    /// Which [`Transport`](crate::Transport) to build.
    ///
    /// ```
    /// use rainier_mail::MailDriver;
    /// use rainier_support::Setting;
    ///
    /// assert!(!MailDriver::Log.delivers(), "nothing leaves the machine");
    /// assert!(MailDriver::parse("smtp").unwrap().delivers());
    /// ```
    pub enum MailDriver: "mail driver" {
        /// Write the message to the log and deliver nothing.
        ///
        /// The default, and the only safe one to have by accident: a
        /// misconfigured production deployment fails to send mail rather than
        /// sending it to real people from a staging database.
        #[default]
        Log = "log",

        /// Write each message to a `.eml` file under `storage/mail`.
        ///
        /// Openable in a mail client, so you can check what a template actually
        /// renders to. Delivers nothing.
        File = "file",

        /// Keep messages in memory so a test can assert on them.
        ///
        /// Never right outside a test — the messages go nowhere and the vector
        /// grows until the process ends.
        Memory = "memory",

        /// A real SMTP server — [`SmtpTransport`](crate::SmtpTransport),
        /// behind the `smtp` feature.
        Smtp = "smtp",

        /// Amazon SES — [`SesTransport`](crate::SesTransport), behind the
        /// `ses` feature.
        Ses = "ses",

        /// The Postmark API — [`PostmarkTransport`](crate::PostmarkTransport),
        /// behind the `postmark` feature.
        Postmark = "postmark",

        /// The Mailgun API — [`MailgunTransport`](crate::MailgunTransport),
        /// behind the `mailgun` feature.
        Mailgun = "mailgun",

        /// The SendGrid API — [`SendGridTransport`](crate::SendGridTransport),
        /// behind the `sendgrid` feature.
        Sendgrid = "sendgrid",

        /// The Resend API — [`ResendTransport`](crate::ResendTransport),
        /// behind the `resend` feature.
        Resend = "resend",
    }
}

impl MailDriver {
    /// Whether a message actually leaves the machine.
    ///
    /// The check worth making before a seeder or a backfill: everything else
    /// here is safe to point a hundred thousand messages at.
    ///
    /// Spelled as the list of senders on purpose — a new variant fails to
    /// compile nothing, but it does fail [the test](self) that pins which side
    /// of this line every variant sits on, so adding one means deciding.
    pub fn delivers(&self) -> bool {
        matches!(
            self,
            Self::Smtp | Self::Ses | Self::Postmark | Self::Mailgun | Self::Sendgrid | Self::Resend
        )
    }

    /// Whether the sent messages can be read back and asserted on.
    pub fn is_inspectable(&self) -> bool {
        matches!(self, Self::Memory | Self::File)
    }
}

setting_enum! {
    /// How an SMTP connection is secured. Selected by `MAIL_ENCRYPTION`.
    ///
    /// A closed set, so `MAIL_ENCRYPTION=ssl` is a boot failure listing the
    /// valid values rather than a guess.
    pub enum MailEncryption: "mail encryption" {
        /// Connect in the clear, then upgrade with `STARTTLS` — the port 587
        /// arrangement, and the default.
        ///
        /// The upgrade is **required**, not opportunistic: a server that will
        /// not upgrade is an error, never a plaintext session that looks like
        /// a secure one.
        #[default]
        StartTls = "starttls",

        /// TLS from the first byte — the port 465 arrangement.
        Tls = "tls",

        /// No TLS at all.
        ///
        /// For a capture container on localhost — Mailpit, MailHog — and
        /// nothing else. Credentials over an unencrypted connection are
        /// credentials shared with the network.
        None = "none",
    }
}

impl MailEncryption {
    /// The port this arrangement conventionally uses, when `MAIL_PORT` does
    /// not say otherwise.
    pub fn default_port(&self) -> u16 {
        match self {
            Self::StartTls => 587,
            Self::Tls => 465,
            Self::None => 25,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_support::Setting;

    #[test]
    fn every_variant_has_decided_whether_it_sends() {
        // The property that keeps a new driver honest: it appears in exactly
        // one of these two lists.
        let senders = [
            MailDriver::Smtp,
            MailDriver::Ses,
            MailDriver::Postmark,
            MailDriver::Mailgun,
            MailDriver::Sendgrid,
            MailDriver::Resend,
        ];
        let safe = [MailDriver::Log, MailDriver::File, MailDriver::Memory];

        assert_eq!(senders.len() + safe.len(), MailDriver::ALL.len());
        for driver in senders {
            assert!(driver.delivers(), "{driver} sends real mail");
        }
        for driver in safe {
            assert!(!driver.delivers(), "{driver} must not send real mail");
        }
    }

    #[test]
    fn the_default_sends_nothing() {
        // The property that matters: a deployment that forgot to set
        // MAIL_DRIVER cannot mail real users.
        assert!(!MailDriver::default().delivers());
    }

    #[test]
    fn encryption_defaults_to_starttls() {
        assert_eq!(MailEncryption::default(), MailEncryption::StartTls);
    }

    #[test]
    fn each_arrangement_knows_its_port() {
        assert_eq!(MailEncryption::StartTls.default_port(), 587);
        assert_eq!(MailEncryption::Tls.default_port(), 465);
        assert_eq!(MailEncryption::None.default_port(), 25);
    }

    #[test]
    fn a_misspelled_encryption_is_an_error_listing_the_choices() {
        let err = MailEncryption::parse("ssl").unwrap_err();
        assert!(err.message().contains("starttls"), "{}", err.message());
    }
}
