//! Where mail goes — [`MailDriver`].

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

        /// A real SMTP server.
        Smtp = "smtp",
    }
}

impl MailDriver {
    /// Whether a message actually leaves the machine.
    ///
    /// The check worth making before a seeder or a backfill: everything else
    /// here is safe to point a hundred thousand messages at.
    pub fn delivers(&self) -> bool {
        matches!(self, Self::Smtp)
    }

    /// Whether the sent messages can be read back and asserted on.
    pub fn is_inspectable(&self) -> bool {
        matches!(self, Self::Memory | Self::File)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_support::Setting;

    #[test]
    fn only_smtp_actually_sends() {
        assert!(MailDriver::Smtp.delivers());
        for driver in MailDriver::ALL.iter().filter(|d| **d != MailDriver::Smtp) {
            assert!(!driver.delivers(), "{driver} must not send real mail");
        }
    }

    #[test]
    fn the_default_sends_nothing() {
        // The property that matters: a deployment that forgot to set
        // MAIL_DRIVER cannot mail real users.
        assert!(!MailDriver::default().delivers());
    }
}
