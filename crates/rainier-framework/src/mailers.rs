//! Mail as a declaration — [`MailerConfig`] and one struct per driver.
//!
//! The last section that was a driver name plus loose keys. The queue declares
//! `ConnectionConfig`, the cache `StoreConfig`, the filesystem `DiskConfig`,
//! broadcasting `BroadcasterConfig`; mail read `MAIL_DRIVER` and then eleven
//! `MAIL_*` values whose relevance depended on it, with nothing in the type
//! system to say which belonged to which.
//!
//! ```
//! # use rainier_framework::mail::{MailerConfig, SmtpMailer};
//! # use rainier_mail::MailEncryption;
//! let declared = MailerConfig::Smtp(SmtpMailer {
//!     host: "smtp.example.com".into(),
//!     port: Some(587),
//!     username: Some("postmaster".into()),
//!     password: Some("hunter2".into()),
//!     encryption: MailEncryption::StartTls,
//!     timeout_secs: 30,
//! });
//!
//! assert_eq!(declared.driver_name(), "smtp");
//! ```
//!
//! A struct literal rather than a builder: public fields and [`Default`], so a
//! declaration names what it sets. The field names and the `MAIL_*` variables
//! are the same words, which is what makes one readable from the other.
//!
//! # It did not grow a second way to build a transport
//!
//! [`transport`](super::mail::transport) still reads the loose keys, and now
//! does it by folding them into a [`MailerConfig`] and building *that*. There
//! is one construction path, so a declaration and a set of variables cannot
//! drift into producing different transports — which is the failure a parallel
//! implementation would have introduced while looking like tidying.
//!
//! # What a declaration refuses
//!
//! A driver whose required setting is missing fails at the declaration rather
//! than at the first send. An SMTP mailer with no host, a Postmark mailer with
//! no token: both would otherwise build, boot, serve, and fail on the first
//! password reset somebody actually needed.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use rainier_mail::{MailDriver, MailEncryption, Transport};
use rainier_support::Result;

/// A declared mail transport.
///
/// One variant per driver, each carrying its own settings and built from those
/// alone.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "RawMailer", into = "RawMailer")]
pub enum MailerConfig {
    /// Write the message to the log and send nothing.
    ///
    /// The default, and deliberately: a default that can reach a real person
    /// is a default that reaches one from staging.
    Log,

    /// Keep messages in this process, for tests.
    Memory,

    /// Write each message to a directory as `.eml`.
    File(FileMailer),

    /// A plain SMTP server.
    Smtp(SmtpMailer),

    /// Cloudflare Email Service, which is SMTP with the service's own host,
    /// port, TLS and SASL identity already applied.
    Cloudflare(CloudflareMailer),

    /// Amazon SES, over the ambient AWS credential chain.
    Ses,

    /// Postmark's HTTP API.
    Postmark(TokenMailer),

    /// Mailgun's HTTP API.
    Mailgun(MailgunMailer),

    /// SendGrid's HTTP API.
    SendGrid(TokenMailer),

    /// Resend's HTTP API.
    Resend(TokenMailer),
}

impl Default for MailerConfig {
    /// [`Log`](Self::Log). Sends nothing, reaches nobody.
    fn default() -> Self {
        Self::Log
    }
}

/// Messages written to a directory as `.eml` files.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FileMailer {
    /// Where the files go.
    pub path: String,
}

impl Default for FileMailer {
    fn default() -> Self {
        Self { path: "storage/mail".into() }
    }
}

/// A plain SMTP server.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct SmtpMailer {
    /// The server to connect to. Required — an assumed one is localhost, which
    /// in a container is nothing.
    pub host: String,

    /// `None` uses the port the encryption arrangement implies, so the two
    /// cannot disagree.
    pub port: Option<u16>,

    /// Set both credentials or neither.
    pub username: Option<String>,

    /// Never printed by [`Debug`].
    pub password: Option<String>,

    /// Implicit TLS, STARTTLS, or none.
    pub encryption: MailEncryption,

    /// How long a connection may take before it fails.
    pub timeout_secs: u64,
}

impl Default for SmtpMailer {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: None,
            username: None,
            password: None,
            encryption: MailEncryption::default(),
            timeout_secs: 30,
        }
    }
}

/// Cloudflare Email Service.
///
/// One field, because the rest belongs to the service rather than to a
/// deployment: host, port, implicit TLS and the literal `api_token` SASL
/// username live in the driver instead of being asked for four times.
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CloudflareMailer {
    /// The API token, used as the SMTP password. Never printed by [`Debug`].
    pub token: String,
}

/// A provider whose whole configuration is one credential — Postmark's server
/// token, SendGrid's and Resend's API keys.
///
/// One type for three drivers because they are the same declaration; the
/// variant says which service it is.
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TokenMailer {
    /// Never printed by [`Debug`].
    pub token: String,
}

/// Mailgun's HTTP API.
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MailgunMailer {
    /// The sending domain.
    pub domain: String,

    /// The API key. Never printed by [`Debug`].
    pub secret: String,

    /// `https://api.eu.mailgun.net` for an EU account. `None` is the US
    /// default, held by the driver rather than copied here.
    pub endpoint: Option<String>,
}

/// Names the server and never the password.
///
/// Hand-written rather than derived, the same rule the queue's
/// `RedisConnection` and the broadcaster's `RedisBroadcast` carry: a derived
/// `Debug` puts the credential into a configuration dump, which for a boot log
/// means every process that started.
impl std::fmt::Debug for SmtpMailer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SmtpMailer")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("encryption", &self.encryption)
            .field("timeout_secs", &self.timeout_secs)
            .finish()
    }
}

impl std::fmt::Debug for CloudflareMailer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CloudflareMailer").field("token", &"<redacted>").finish()
    }
}

impl std::fmt::Debug for TokenMailer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenMailer").field("token", &"<redacted>").finish()
    }
}

impl std::fmt::Debug for MailgunMailer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MailgunMailer")
            .field("domain", &self.domain)
            .field("secret", &"<redacted>")
            .field("endpoint", &self.endpoint)
            .finish()
    }
}

impl MailerConfig {
    /// The driver this declares, by the name the configuration uses.
    pub fn driver_name(&self) -> &'static str {
        match self {
            Self::Log => "log",
            Self::Memory => "memory",
            Self::File(_) => "file",
            Self::Smtp(_) => "smtp",
            Self::Cloudflare(_) => "cloudflare",
            Self::Ses => "ses",
            Self::Postmark(_) => "postmark",
            Self::Mailgun(_) => "mailgun",
            Self::SendGrid(_) => "sendgrid",
            Self::Resend(_) => "resend",
        }
    }

    /// The driver as the enum the rest of the framework matches on.
    pub fn driver(&self) -> MailDriver {
        match self {
            Self::Log => MailDriver::Log,
            Self::Memory => MailDriver::Memory,
            Self::File(_) => MailDriver::File,
            Self::Smtp(_) => MailDriver::Smtp,
            Self::Cloudflare(_) => MailDriver::Cloudflare,
            Self::Ses => MailDriver::Ses,
            Self::Postmark(_) => MailDriver::Postmark,
            Self::Mailgun(_) => MailDriver::Mailgun,
            Self::SendGrid(_) => MailDriver::Sendgrid,
            Self::Resend(_) => MailDriver::Resend,
        }
    }

    /// Whether this declaration can actually deliver to a person.
    ///
    /// `false` for the three that deliberately cannot — log, memory and file.
    /// Surfaced because "mail is configured" and "mail leaves the building"
    /// are different questions, and a deployment that answers the first and
    /// not the second looks healthy until somebody needs a password reset.
    pub fn delivers(&self) -> bool {
        !matches!(self, Self::Log | Self::Memory | Self::File(_))
    }

    /// Refuse a declaration the driver cannot work with.
    ///
    /// # Why this is not only in the deserialiser
    ///
    /// `TryFrom<RawMailer>` catches a bad *section*, but these are public
    /// structs with public fields and an application can write
    /// `SmtpMailer { host: String::new(), .. }` in Rust without going near
    /// serde. Without this that builds a transport pointed at nothing, boots,
    /// serves, and fails on the first message somebody needed — which is the
    /// exact failure the section-level check exists to prevent, reached by the
    /// other door.
    ///
    /// Same messages either way, so a reader cannot tell which door they came
    /// through and does not need to.
    pub fn validate(&self) -> Result<()> {
        let refuse =
            |field: &str| Err(rainier_support::Error::internal(missing(self.driver(), field)));

        match self {
            Self::Smtp(smtp) if smtp.host.trim().is_empty() => refuse("host"),
            Self::Cloudflare(c) if c.token.trim().is_empty() => refuse("token"),
            Self::Postmark(t) | Self::SendGrid(t) | Self::Resend(t)
                if t.token.trim().is_empty() =>
            {
                refuse("token")
            }
            Self::Mailgun(m) if m.domain.trim().is_empty() => refuse("domain"),
            Self::Mailgun(m) if m.secret.trim().is_empty() => refuse("secret"),
            _ => Ok(()),
        }
    }

    /// Build the transport this declares.
    ///
    /// # Errors
    ///
    /// When the driver's feature is off, or the declaration is missing
    /// something the driver cannot work without.
    pub fn build(&self) -> Result<Arc<dyn Transport>> {
        self.validate()?;
        super::mail::build_declared(self)
    }
}

/// The flat shape a `mail` section has on the wire.
///
/// Every field optional and `deny_unknown_fields`, so a misspelt setting is a
/// boot failure naming the key rather than a value read by nothing.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMailer {
    driver: MailDriver,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    password: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    encryption: Option<MailEncryption>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    timeout_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    domain: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    endpoint: Option<String>,
}

/// The environment variable a declaration field is usually set from.
///
/// Named in the error beside the field, because those are two different
/// audiences for the same message: somebody writing a `MailerConfig` knows the
/// field, and somebody who set eleven `MAIL_*` variables in a chart knows the
/// variable. A message carrying only one of them sends the other reader
/// looking for a name that does not appear in what they wrote.
fn variable_for(driver: MailDriver, field: &str) -> &'static str {
    match (driver, field) {
        (_, "host") => "MAIL_HOST",
        (_, "path") => "MAIL_FILE_PATH",
        (_, "domain") => "MAIL_MAILGUN_DOMAIN",
        (_, "secret") => "MAIL_MAILGUN_SECRET",
        (MailDriver::Cloudflare, "token") => "MAIL_CLOUDFLARE_TOKEN",
        (MailDriver::Postmark, "token") => "MAIL_POSTMARK_TOKEN",
        (MailDriver::Sendgrid, "token") => "MAIL_SENDGRID_KEY",
        (MailDriver::Resend, "token") => "MAIL_RESEND_KEY",
        _ => "the matching MAIL_* variable",
    }
}

/// What a driver says when something it cannot work without is absent.
///
/// One function, so the deserialiser and [`MailerConfig::validate`] cannot
/// come to word the same refusal differently.
fn missing(driver: MailDriver, field: &str) -> String {
    let variable = variable_for(driver, field);
    format!(
        "the `{driver}` mailer needs `{field}` (`{variable}`); without it the transport \
         builds, boots and fails on the first message somebody actually needed."
    )
}

/// A setting the declared driver cannot work without.
fn required(
    value: Option<String>,
    name: &str,
    driver: MailDriver,
) -> std::result::Result<String, String> {
    value.filter(|v| !v.trim().is_empty()).ok_or_else(|| missing(driver, name))
}

impl TryFrom<RawMailer> for MailerConfig {
    type Error = String;

    fn try_from(raw: RawMailer) -> std::result::Result<Self, Self::Error> {
        let driver = raw.driver;

        Ok(match driver {
            MailDriver::Log => Self::Log,
            MailDriver::Memory => Self::Memory,
            MailDriver::File => Self::File(FileMailer {
                path: raw
                    .path
                    .filter(|p| !p.trim().is_empty())
                    .unwrap_or_else(|| FileMailer::default().path),
            }),
            MailDriver::Smtp => Self::Smtp(SmtpMailer {
                host: required(raw.host, "host", driver)?,
                port: raw.port,
                username: raw.username.filter(|u| !u.trim().is_empty()),
                password: raw.password.filter(|p| !p.trim().is_empty()),
                encryption: raw.encryption.unwrap_or_default(),
                timeout_secs: raw.timeout_secs.unwrap_or(30),
            }),
            MailDriver::Cloudflare => {
                Self::Cloudflare(CloudflareMailer { token: required(raw.token, "token", driver)? })
            }
            MailDriver::Ses => Self::Ses,
            MailDriver::Postmark => {
                Self::Postmark(TokenMailer { token: required(raw.token, "token", driver)? })
            }
            MailDriver::Mailgun => Self::Mailgun(MailgunMailer {
                domain: required(raw.domain, "domain", driver)?,
                secret: required(raw.secret, "secret", driver)?,
                endpoint: raw.endpoint.filter(|e| !e.trim().is_empty()),
            }),
            MailDriver::Sendgrid => {
                Self::SendGrid(TokenMailer { token: required(raw.token, "token", driver)? })
            }
            MailDriver::Resend => {
                Self::Resend(TokenMailer { token: required(raw.token, "token", driver)? })
            }
        })
    }
}

impl From<MailerConfig> for RawMailer {
    fn from(config: MailerConfig) -> Self {
        let bare = |driver| RawMailer {
            driver,
            path: None,
            host: None,
            port: None,
            username: None,
            password: None,
            encryption: None,
            timeout_secs: None,
            token: None,
            domain: None,
            secret: None,
            endpoint: None,
        };

        let driver = config.driver();
        match config {
            MailerConfig::Log | MailerConfig::Memory | MailerConfig::Ses => bare(driver),
            MailerConfig::File(file) => RawMailer { path: Some(file.path), ..bare(driver) },
            MailerConfig::Smtp(smtp) => RawMailer {
                host: Some(smtp.host),
                port: smtp.port,
                username: smtp.username,
                password: smtp.password,
                encryption: Some(smtp.encryption),
                timeout_secs: Some(smtp.timeout_secs),
                ..bare(driver)
            },
            MailerConfig::Cloudflare(cloudflare) => {
                RawMailer { token: Some(cloudflare.token), ..bare(driver) }
            }
            MailerConfig::Postmark(t) | MailerConfig::SendGrid(t) | MailerConfig::Resend(t) => {
                RawMailer { token: Some(t.token), ..bare(driver) }
            }
            MailerConfig::Mailgun(mailgun) => RawMailer {
                domain: Some(mailgun.domain),
                secret: Some(mailgun.secret),
                endpoint: mailgun.endpoint,
                ..bare(driver)
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(value: serde_json::Value) -> std::result::Result<MailerConfig, String> {
        serde_json::from_value(value).map_err(|e| e.to_string())
    }

    #[test]
    fn a_declaration_is_a_struct_literal() {
        let declared = MailerConfig::Smtp(SmtpMailer {
            host: "smtp.example.com".into(),
            ..Default::default()
        });

        assert_eq!(declared.driver_name(), "smtp");
        assert!(declared.delivers());
    }

    #[test]
    fn it_round_trips_through_the_configuration_tree() {
        for declared in [
            MailerConfig::Log,
            MailerConfig::Ses,
            MailerConfig::File(FileMailer { path: "storage/mail".into() }),
            MailerConfig::Smtp(SmtpMailer {
                host: "smtp.example.com".into(),
                port: Some(587),
                username: Some("postmaster".into()),
                password: Some("hunter2".into()),
                encryption: MailEncryption::StartTls,
                timeout_secs: 15,
            }),
            MailerConfig::Mailgun(MailgunMailer {
                domain: "mg.example.com".into(),
                secret: "key".into(),
                endpoint: Some("https://api.eu.mailgun.net".into()),
            }),
            MailerConfig::Resend(TokenMailer { token: "re_x".into() }),
        ] {
            let json = serde_json::to_value(declared.clone()).expect("serialises");
            let back: MailerConfig = serde_json::from_value(json).expect("deserialises");

            assert_eq!(back, declared);
        }
    }

    #[test]
    fn debug_never_prints_a_credential() {
        // The rule every other declaration in this framework carries.
        let smtp = SmtpMailer {
            host: "smtp.example.com".into(),
            password: Some("hunter2".into()),
            ..Default::default()
        };
        let shown = format!("{smtp:?}");
        assert!(!shown.contains("hunter2"), "{shown}");
        assert!(shown.contains("smtp.example.com"), "{shown}");

        for shown in [
            format!("{:?}", CloudflareMailer { token: "hunter2".into() }),
            format!("{:?}", TokenMailer { token: "hunter2".into() }),
            format!(
                "{:?}",
                MailgunMailer {
                    domain: "mg.example.com".into(),
                    secret: "hunter2".into(),
                    endpoint: None,
                }
            ),
        ] {
            assert!(!shown.contains("hunter2"), "{shown}");
            assert!(shown.contains("<redacted>"), "{shown}");
        }
    }

    #[test]
    fn a_driver_that_cannot_work_without_a_setting_refuses_it() {
        // Each of these would otherwise build, boot, serve, and fail on the
        // first message somebody actually needed.
        for (value, missing) in [
            (json!({"driver": "smtp"}), "host"),
            (json!({"driver": "postmark"}), "token"),
            (json!({"driver": "sendgrid"}), "token"),
            (json!({"driver": "resend"}), "token"),
            (json!({"driver": "cloudflare"}), "token"),
            (json!({"driver": "mailgun", "secret": "k"}), "domain"),
            (json!({"driver": "mailgun", "domain": "d"}), "secret"),
        ] {
            let err = parse(value.clone()).expect_err("refused");
            assert!(err.contains(missing), "{value}: {err}");
        }
    }

    #[test]
    fn an_empty_setting_is_the_same_as_a_missing_one() {
        // A chart that renders `MAIL_HOST: ""` for an unset value is the
        // common case, and treating it as present is how a transport ends up
        // pointed at nothing.
        let err = parse(json!({"driver": "smtp", "host": "  "})).expect_err("refused");

        assert!(err.contains("host"), "{err}");
    }

    #[test]
    fn the_drivers_that_deliver_say_so() {
        assert!(!MailerConfig::Log.delivers());
        assert!(!MailerConfig::Memory.delivers());
        assert!(!MailerConfig::File(FileMailer::default()).delivers());
        assert!(MailerConfig::Ses.delivers());
        assert!(MailerConfig::Resend(TokenMailer { token: "k".into() }).delivers());
    }

    #[test]
    fn the_file_mailer_has_a_default_directory() {
        let declared = parse(json!({"driver": "file"})).expect("parses");

        assert_eq!(declared, MailerConfig::File(FileMailer { path: "storage/mail".into() }));
    }

    #[test]
    fn a_struct_literal_is_validated_too() {
        // The other door: these are public structs, so an application can
        // write one in Rust without going near serde. Building it must refuse
        // for the same reason and in the same words.
        let declared = MailerConfig::Smtp(SmtpMailer { host: "   ".into(), ..Default::default() });

        let err = declared.validate().expect_err("refused");

        assert!(err.message().contains("MAIL_HOST"), "{}", err.message());
        assert!(err.message().contains("host"), "{}", err.message());
    }

    #[test]
    fn a_complete_declaration_validates() {
        for declared in [
            MailerConfig::Log,
            MailerConfig::Ses,
            MailerConfig::File(FileMailer::default()),
            MailerConfig::Smtp(SmtpMailer {
                host: "smtp.example.com".into(),
                ..Default::default()
            }),
            MailerConfig::Resend(TokenMailer { token: "re_x".into() }),
            MailerConfig::Mailgun(MailgunMailer {
                domain: "mg.example.com".into(),
                secret: "k".into(),
                endpoint: None,
            }),
        ] {
            declared.validate().unwrap_or_else(|e| panic!("{declared:?}: {}", e.message()));
        }
    }

    #[test]
    fn an_unknown_field_is_refused_naming_itself() {
        let err = parse(json!({"driver": "smtp", "host": "h", "hots": "x"})).expect_err("refused");

        assert!(err.contains("hots"), "{err}");
    }
}
