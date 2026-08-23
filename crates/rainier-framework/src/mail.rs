//! Everything [`rainier_mail`] exports, plus the step between `MAIL_*` in an
//! environment file and a running [`Mailer`] — so an application's provider
//! is two lines rather than a hand-assembled transport:
//!
//! ```ignore
//! // app/providers/app_provider.rs
//! let mailer = mail::mailer(&config, Arc::clone(views.engine()))?
//!     .with_events(container.resolve::<Dispatcher>()?);
//! ```
//!
//! [`transport()`] is the exhaustive match over [`MailDriver`]. The safe
//! drivers — `log`, `file`, `memory` — always build. The senders are behind
//! cargo features, and selecting one the build does not carry **fails the
//! boot naming the feature**, because the silent version of that mistake is a
//! production deployment logging its mail and nobody noticing until a
//! password reset does not arrive:
//!
//! | `MAIL_DRIVER` | Feature | Needs |
//! |---|---|---|
//! | `smtp` | `mail-smtp` | `MAIL_HOST`, and see `MAIL_ENCRYPTION` |
//! | `ses` | `mail-ses` | the AWS default chain |
//! | `postmark` | `mail-postmark` | `MAIL_POSTMARK_TOKEN` |
//! | `mailgun` | `mail-mailgun` | `MAIL_MAILGUN_DOMAIN`, `MAIL_MAILGUN_SECRET` |
//! | `sendgrid` | `mail-sendgrid` | `MAIL_SENDGRID_KEY` |
//! | `resend` | `mail-resend` | `MAIL_RESEND_KEY` |

use std::sync::Arc;

pub use crate::mailers::{
    CloudflareMailer, FileMailer, MailerConfig, MailgunMailer, SmtpMailer, TokenMailer,
};

use rainier_config::Config;
use rainier_support::{Error, Result};
use rainier_view::ViewEngine;

pub use rainier_mail::*;

use crate::keys;

/// The transport `MAIL_DRIVER` names, built from its `MAIL_*` settings.
///
/// # Errors
///
/// When the driver's feature is not compiled in, or a setting it requires is
/// empty — each error names the feature or the variable, because "mail is
/// not working" should take one read of the boot log to diagnose.
pub fn transport(config: &Config) -> Result<Arc<dyn Transport>> {
    build_declared(&declared(config)?)
}

/// The declaration this configuration describes.
///
/// An explicit `mail.mailer` section wins; otherwise the loose `MAIL_*` values
/// are folded into the same shape. Folding rather than branching is what keeps
/// there being **one** construction path: a declaration and a set of variables
/// cannot produce different transports, because the variables become a
/// declaration before anything is built.
pub fn declared(config: &Config) -> Result<MailerConfig> {
    if let Some(declared) = config.get(keys::MAILER) {
        return Ok(declared);
    }

    let text = |key| config.get(key).filter(|value: &String| !value.trim().is_empty());

    let driver = config.setting(keys::MAIL_DRIVER)?;

    // Refused rather than dropped. `u16::try_from(...).ok()` would leave a
    // deployment that wrote `MAIL_PORT=70000` connecting on the encryption's
    // default port instead — the setting read, understood by whoever wrote it,
    // and then ignored.
    let port = match config.get(keys::MAIL_PORT).unwrap_or(0) {
        0 => None,
        port => Some(
            u16::try_from(port)
                .map_err(|_| Error::internal(format!("`MAIL_PORT={port}` is not a port")))?,
        ),
    };

    let raw = serde_json::json!({
        "driver": driver,
        "path": text(keys::MAIL_FILE_PATH),
        "host": text(keys::MAIL_HOST),
        "port": port,
        "username": text(keys::MAIL_USERNAME),
        "password": text(keys::MAIL_PASSWORD),
        "encryption": config.setting(keys::MAIL_ENCRYPTION)?,
        "timeout_secs": u64::try_from(config.get(keys::MAIL_TIMEOUT).unwrap_or(30)).ok(),
        // One `token` field for four providers that each name it differently.
        // Read in driver order rather than merged, so a deployment carrying a
        // stale key for a provider it no longer uses cannot supply the
        // credential for the one it does.
        "token": match driver {
            MailDriver::Cloudflare => text(keys::MAIL_CLOUDFLARE_TOKEN),
            MailDriver::Postmark => text(keys::MAIL_POSTMARK_TOKEN),
            MailDriver::Sendgrid => text(keys::MAIL_SENDGRID_KEY),
            MailDriver::Resend => text(keys::MAIL_RESEND_KEY),
            _ => None,
        },
        "domain": text(keys::MAIL_MAILGUN_DOMAIN),
        "secret": text(keys::MAIL_MAILGUN_SECRET),
        "endpoint": text(keys::MAIL_MAILGUN_ENDPOINT),
    });

    // `null` is how the fields above say "unset", and `deny_unknown_fields`
    // has no quarrel with a key whose value is absent — but a `None` port on a
    // `u16` field would still fail to deserialise, so they are stripped.
    let raw = match raw {
        serde_json::Value::Object(fields) => serde_json::Value::Object(
            fields.into_iter().filter(|(_, value)| !value.is_null()).collect(),
        ),
        other => other,
    };

    serde_json::from_value(raw).map_err(|e| {
        Error::internal(format!("the mail settings do not describe a usable mailer: {e}"))
    })
}

/// Build the transport a declaration names.
///
/// The one place a mail transport is constructed. `transport` reaches it by
/// way of `declared`, so the `MAIL_*` variables and a `mail.mailer` section
/// are two spellings of one thing rather than two implementations of it.
pub(crate) fn build_declared(declared: &MailerConfig) -> Result<Arc<dyn Transport>> {
    match declared {
        MailerConfig::Log => Ok(Arc::new(LogTransport)),
        MailerConfig::Memory => Ok(Arc::new(MemoryTransport::new())),
        MailerConfig::File(file) => Ok(Arc::new(FileTransport::new(&file.path)?)),
        MailerConfig::Smtp(declared) => smtp(declared),
        MailerConfig::Cloudflare(declared) => cloudflare(declared),
        MailerConfig::Ses => ses(),
        MailerConfig::Postmark(declared) => postmark(declared),
        MailerConfig::Mailgun(declared) => mailgun(declared),
        MailerConfig::SendGrid(declared) => sendgrid(declared),
        MailerConfig::Resend(declared) => resend(declared),
    }
}

/// A [`Mailer`] over [`transport()`], with the `mail.from` default applied and
/// `MAIL_ALWAYS_TO` honoured when set.
///
/// Chain [`Mailer::with_events`] yourself — whether sends should announce
/// themselves on the event bus is the application's call, not configuration.
pub fn mailer(config: &Config, views: Arc<dyn ViewEngine>) -> Result<Mailer> {
    let over = transport(config)?;
    Ok(mailer_over(config, views, over))
}

/// The same `mail.from` and `MAIL_ALWAYS_TO` treatment [`mailer()`] applies,
/// over a transport you chose — for the provider that swaps transports per
/// mode, because a test wants the memory one and the same everything else.
pub fn mailer_over(
    config: &Config,
    views: Arc<dyn ViewEngine>,
    transport: Arc<dyn Transport>,
) -> Mailer {
    let mut mailer = Mailer::new(views, transport);

    if let Some(address) = config.get(keys::MAIL_FROM_ADDRESS).filter(|a| !a.trim().is_empty()) {
        mailer = match config.get(keys::MAIL_FROM_NAME).filter(|n| !n.trim().is_empty()) {
            Some(name) => mailer.with_default_from(Address::named(address, name)),
            None => mailer.with_default_from(Address::new(address)),
        };
    }

    if let Some(address) = config.get(keys::MAIL_ALWAYS_TO).filter(|a| !a.trim().is_empty()) {
        mailer = mailer.always_to(Address::new(address));
    }

    mailer
}

/// The refusal a sender compiled out answers with — at boot, naming the
/// feature, rather than a mailer that quietly logs instead of sending.
#[allow(dead_code, reason = "unused only when every mail feature is enabled")]
fn feature_missing(driver: &str, feature: &str) -> Error {
    Error::internal(format!(
        "`MAIL_DRIVER={driver}` needs the `{feature}` cargo feature, which this build does not \
         carry. Enable it on `rainier-framework`, or pick a driver this build has."
    ))
}

#[cfg(feature = "mail-smtp")]
fn smtp(declared: &SmtpMailer) -> Result<Arc<dyn Transport>> {
    let mut builder = SmtpTransport::builder(&declared.host)
        .encryption(declared.encryption)
        .timeout(std::time::Duration::from_secs(declared.timeout_secs));

    if let Some(port) = declared.port {
        builder = builder.port(port);
    }

    // A username with no password is still credentials — some relays accept
    // an empty one — so this follows the username rather than requiring both.
    if let Some(username) = &declared.username {
        builder = builder.credentials(username, declared.password.clone().unwrap_or_default());
    }

    Ok(Arc::new(builder.build()?))
}

#[cfg(not(feature = "mail-smtp"))]
fn smtp(_: &SmtpMailer) -> Result<Arc<dyn Transport>> {
    Err(feature_missing("smtp", "mail-smtp"))
}

#[cfg(feature = "mail-smtp")]
fn cloudflare(declared: &CloudflareMailer) -> Result<Arc<dyn Transport>> {
    // The one setting an application supplies. Host, port, implicit TLS and
    // the `api_token` username are the service's, not a deployment's, so the
    // driver holds them rather than asking four times for the same answer.
    Ok(Arc::new(rainier_mail::cloudflare::cloudflare_smtp(&declared.token)?))
}

#[cfg(not(feature = "mail-smtp"))]
fn cloudflare(_: &CloudflareMailer) -> Result<Arc<dyn Transport>> {
    // The same feature as `smtp`, because it *is* the SMTP transport with
    // settings applied — naming `mail-smtp` here rather than inventing a
    // `mail-cloudflare` keeps the fix one flag rather than a guess.
    Err(feature_missing("cloudflare", "mail-smtp"))
}

#[cfg(feature = "mail-ses")]
fn ses() -> Result<Arc<dyn Transport>> {
    // Lazy on purpose: the AWS chain is walked on the first send, so building
    // the transport stays synchronous — a service provider is a synchronous
    // place — and cannot fail for a reason the first request would not repeat.
    Ok(Arc::new(SesTransport::from_env()))
}

#[cfg(not(feature = "mail-ses"))]
fn ses() -> Result<Arc<dyn Transport>> {
    Err(feature_missing("ses", "mail-ses"))
}

#[cfg(feature = "mail-postmark")]
fn postmark(declared: &TokenMailer) -> Result<Arc<dyn Transport>> {
    Ok(Arc::new(PostmarkTransport::new(http(), &declared.token)))
}

#[cfg(not(feature = "mail-postmark"))]
fn postmark(_: &TokenMailer) -> Result<Arc<dyn Transport>> {
    Err(feature_missing("postmark", "mail-postmark"))
}

#[cfg(feature = "mail-mailgun")]
fn mailgun(declared: &MailgunMailer) -> Result<Arc<dyn Transport>> {
    let mut transport = MailgunTransport::new(http(), &declared.domain, &declared.secret);
    if let Some(endpoint) = &declared.endpoint {
        transport = transport.with_base_url(endpoint);
    }
    Ok(Arc::new(transport))
}

#[cfg(not(feature = "mail-mailgun"))]
fn mailgun(_: &MailgunMailer) -> Result<Arc<dyn Transport>> {
    Err(feature_missing("mailgun", "mail-mailgun"))
}

#[cfg(feature = "mail-sendgrid")]
fn sendgrid(declared: &TokenMailer) -> Result<Arc<dyn Transport>> {
    Ok(Arc::new(SendGridTransport::new(http(), &declared.token)))
}

#[cfg(not(feature = "mail-sendgrid"))]
fn sendgrid(_: &TokenMailer) -> Result<Arc<dyn Transport>> {
    Err(feature_missing("sendgrid", "mail-sendgrid"))
}

#[cfg(feature = "mail-resend")]
fn resend(declared: &TokenMailer) -> Result<Arc<dyn Transport>> {
    Ok(Arc::new(ResendTransport::new(http(), &declared.token)))
}

#[cfg(not(feature = "mail-resend"))]
fn resend(_: &TokenMailer) -> Result<Arc<dyn Transport>> {
    Err(feature_missing("resend", "mail-resend"))
}

/// The socket the HTTP providers share. Their features imply the framework's
/// `http-client` feature, so the real transport is always here to construct.
#[cfg(any(
    feature = "mail-postmark",
    feature = "mail-mailgun",
    feature = "mail-sendgrid",
    feature = "mail-resend"
))]
fn http() -> Arc<dyn rainier_http_client::Transport> {
    Arc::new(rainier_http_client::ReqwestTransport::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_config::Env;

    fn config(env: &str) -> Config {
        let config = Config::new();
        let env = Env::parse(env).isolated();

        config.set(keys::MAIL_DRIVER, env.setting("MAIL_DRIVER").unwrap()).unwrap();
        config.set(keys::MAIL_FROM_ADDRESS, env.string("MAIL_FROM", "hello@example.com")).unwrap();
        config.set(keys::MAIL_FROM_NAME, env.string("MAIL_FROM_NAME", "Rainier")).unwrap();
        config.set(keys::MAIL_ALWAYS_TO, env.string("MAIL_ALWAYS_TO", "")).unwrap();
        config.set(keys::MAIL_FILE_PATH, env.string("MAIL_FILE_PATH", "")).unwrap();
        config.set(keys::MAIL_HOST, env.string("MAIL_HOST", "")).unwrap();
        config.set(keys::MAIL_PORT, env.int("MAIL_PORT", 0)).unwrap();
        config.set(keys::MAIL_USERNAME, env.string("MAIL_USERNAME", "")).unwrap();
        config.set(keys::MAIL_PASSWORD, env.string("MAIL_PASSWORD", "")).unwrap();
        config.set(keys::MAIL_ENCRYPTION, env.setting("MAIL_ENCRYPTION").unwrap()).unwrap();
        config.set(keys::MAIL_TIMEOUT, env.int("MAIL_TIMEOUT", 30)).unwrap();
        config.set(keys::MAIL_POSTMARK_TOKEN, env.string("MAIL_POSTMARK_TOKEN", "")).unwrap();
        config.set(keys::MAIL_MAILGUN_DOMAIN, env.string("MAIL_MAILGUN_DOMAIN", "")).unwrap();
        config.set(keys::MAIL_MAILGUN_SECRET, env.string("MAIL_MAILGUN_SECRET", "")).unwrap();
        config.set(keys::MAIL_MAILGUN_ENDPOINT, env.string("MAIL_MAILGUN_ENDPOINT", "")).unwrap();
        config.set(keys::MAIL_SENDGRID_KEY, env.string("MAIL_SENDGRID_KEY", "")).unwrap();
        config.set(keys::MAIL_RESEND_KEY, env.string("MAIL_RESEND_KEY", "")).unwrap();
        config
    }

    #[test]
    fn the_default_is_the_log_and_nothing_escapes() {
        let transport = transport(&config("APP_ENV=local")).unwrap();
        assert_eq!(transport.name(), "log");
    }

    #[test]
    fn the_file_driver_honours_its_path() {
        let directory = std::env::temp_dir().join("rainier-mail-config-file");
        let _ = std::fs::remove_dir_all(&directory);

        let transport = transport(&config(&format!(
            "MAIL_DRIVER=file\nMAIL_FILE_PATH={}",
            directory.display()
        )))
        .unwrap();

        assert_eq!(transport.name(), "file");
        assert!(directory.is_dir(), "the directory is created at build time");
    }

    #[test]
    fn a_misspelled_driver_stops_the_boot_listing_the_choices() {
        // The refusal happens where the value is read from the environment —
        // before any transport is looked at — which is what makes it a boot
        // failure rather than a first-send one.
        let err = Env::parse("MAIL_DRIVER=smpt")
            .isolated()
            .setting::<MailDriver>("MAIL_DRIVER")
            .err()
            .expect("a misspelled driver must be refused");
        assert!(err.message().contains("smtp"), "{}", err.message());
    }

    #[cfg(not(feature = "mail-smtp"))]
    #[test]
    fn a_sender_the_build_does_not_carry_names_its_feature() {
        let err = transport(&config("MAIL_DRIVER=smtp\nMAIL_HOST=smtp.example.com"))
            .err()
            .expect("a sender without its feature must be refused");

        assert!(err.message().contains("mail-smtp"), "{}", err.message());
    }

    #[cfg(feature = "mail-smtp")]
    #[test]
    fn smtp_builds_from_its_settings() {
        let transport = transport(&config(
            "MAIL_DRIVER=smtp\nMAIL_HOST=localhost\nMAIL_PORT=1025\nMAIL_ENCRYPTION=none",
        ))
        .unwrap();

        assert_eq!(transport.name(), "smtp");
    }

    #[cfg(feature = "mail-smtp")]
    #[test]
    fn smtp_without_a_host_names_the_variable() {
        let err = transport(&config("MAIL_DRIVER=smtp"))
            .err()
            .expect("smtp with no host must be refused");
        assert!(err.message().contains("MAIL_HOST"), "{}", err.message());
    }

    #[cfg(feature = "mail-smtp")]
    #[test]
    fn a_port_that_is_not_a_port_is_refused() {
        let err =
            transport(&config("MAIL_DRIVER=smtp\nMAIL_HOST=smtp.example.com\nMAIL_PORT=70000"))
                .err()
                .expect("an impossible port must be refused");

        assert!(err.message().contains("70000"), "{}", err.message());
    }

    #[cfg(feature = "mail-postmark")]
    #[test]
    fn postmark_without_its_token_names_the_variable() {
        let err = transport(&config("MAIL_DRIVER=postmark"))
            .err()
            .expect("postmark with no token must be refused");
        assert!(err.message().contains("MAIL_POSTMARK_TOKEN"), "{}", err.message());
    }

    #[cfg(feature = "mail-mailgun")]
    #[test]
    fn mailgun_needs_both_halves_of_its_credential() {
        let err = transport(&config("MAIL_DRIVER=mailgun\nMAIL_MAILGUN_DOMAIN=mg.example.com"))
            .err()
            .expect("mailgun with half a credential must be refused");

        assert!(err.message().contains("MAIL_MAILGUN_SECRET"), "{}", err.message());
    }

    struct Plain;

    impl Mailable for Plain {
        fn envelope(&self) -> Envelope {
            Envelope::new("Hello").to("ada@example.com")
        }
        fn content(&self) -> Result<Content> {
            Ok(Content::text("Hi"))
        }
    }

    #[test]
    fn the_mailer_applies_the_configured_sender() {
        let views = Arc::new(rainier_view::MemoryEngine::new());
        let mailer =
            mailer(&config("MAIL_FROM=team@example.com\nMAIL_FROM_NAME=The Team"), views).unwrap();

        let message = mailer.prepare(&Plain).unwrap();
        assert_eq!(message.envelope.from.as_ref().unwrap().email, "team@example.com");
        assert_eq!(message.envelope.from.as_ref().unwrap().name.as_deref(), Some("The Team"));
    }

    #[test]
    fn always_to_redirects_when_set_and_only_then() {
        let views = Arc::new(rainier_view::MemoryEngine::new());

        let redirected = mailer(&config("MAIL_ALWAYS_TO=dev@example.com"), Arc::clone(&views) as _)
            .unwrap()
            .prepare(&Plain)
            .unwrap();
        assert_eq!(redirected.envelope.to[0].email, "dev@example.com");

        let direct = mailer(&config("APP_ENV=local"), views).unwrap().prepare(&Plain).unwrap();
        assert_eq!(direct.envelope.to[0].email, "ada@example.com");
    }
}
