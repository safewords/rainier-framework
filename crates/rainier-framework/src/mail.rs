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
//! [`transport`] is the exhaustive match over [`MailDriver`]. The safe
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
    match config.setting(keys::MAIL_DRIVER)? {
        MailDriver::Log => Ok(Arc::new(LogTransport)),
        MailDriver::Memory => Ok(Arc::new(MemoryTransport::new())),
        MailDriver::File => {
            let directory = config
                .get(keys::MAIL_FILE_PATH)
                .filter(|path| !path.trim().is_empty())
                .unwrap_or_else(|| "storage/mail".into());
            Ok(Arc::new(FileTransport::new(directory)?))
        }
        MailDriver::Smtp => smtp(config),
        MailDriver::Ses => ses(),
        MailDriver::Postmark => postmark(config),
        MailDriver::Mailgun => mailgun(config),
        MailDriver::Sendgrid => sendgrid(config),
        MailDriver::Resend => resend(config),
    }
}

/// A [`Mailer`] over [`transport`], with the `mail.from` default applied and
/// `MAIL_ALWAYS_TO` honoured when set.
///
/// Chain [`Mailer::with_events`] yourself — whether sends should announce
/// themselves on the event bus is the application's call, not configuration.
pub fn mailer(config: &Config, views: Arc<dyn ViewEngine>) -> Result<Mailer> {
    let over = transport(config)?;
    Ok(mailer_over(config, views, over))
}

/// The same `mail.from` and `MAIL_ALWAYS_TO` treatment [`mailer`] applies,
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

/// A setting the selected driver cannot work without.
#[cfg(any(
    feature = "mail-smtp",
    feature = "mail-postmark",
    feature = "mail-mailgun",
    feature = "mail-sendgrid",
    feature = "mail-resend"
))]
fn require(config: &Config, key: rainier_config::Key<String>, name: &str) -> Result<String> {
    config.get(key).filter(|value| !value.trim().is_empty()).ok_or_else(|| {
        Error::internal(format!(
            "`MAIL_DRIVER` selects a driver that needs `{name}`, which is not set."
        ))
    })
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
fn smtp(config: &Config) -> Result<Arc<dyn Transport>> {
    let host = require(config, keys::MAIL_HOST, "MAIL_HOST")?;

    let seconds = config.get(keys::MAIL_TIMEOUT).unwrap_or(30);
    let seconds = u64::try_from(seconds).map_err(|_| {
        Error::internal(format!("`MAIL_TIMEOUT={seconds}` is not a number of seconds"))
    })?;

    let mut builder = SmtpTransport::builder(host)
        .encryption(config.setting(keys::MAIL_ENCRYPTION)?)
        .timeout(std::time::Duration::from_secs(seconds));

    let port = config.get(keys::MAIL_PORT).unwrap_or(0);
    if port > 0 {
        builder = builder.port(
            u16::try_from(port)
                .map_err(|_| Error::internal(format!("`MAIL_PORT={port}` is not a port")))?,
        );
    }

    let username = config.get(keys::MAIL_USERNAME).unwrap_or_default();
    if !username.is_empty() {
        builder =
            builder.credentials(username, config.get(keys::MAIL_PASSWORD).unwrap_or_default());
    }

    Ok(Arc::new(builder.build()?))
}

#[cfg(not(feature = "mail-smtp"))]
fn smtp(_: &Config) -> Result<Arc<dyn Transport>> {
    Err(feature_missing("smtp", "mail-smtp"))
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
fn postmark(config: &Config) -> Result<Arc<dyn Transport>> {
    let token = require(config, keys::MAIL_POSTMARK_TOKEN, "MAIL_POSTMARK_TOKEN")?;
    Ok(Arc::new(PostmarkTransport::new(http(), token)))
}

#[cfg(not(feature = "mail-postmark"))]
fn postmark(_: &Config) -> Result<Arc<dyn Transport>> {
    Err(feature_missing("postmark", "mail-postmark"))
}

#[cfg(feature = "mail-mailgun")]
fn mailgun(config: &Config) -> Result<Arc<dyn Transport>> {
    let domain = require(config, keys::MAIL_MAILGUN_DOMAIN, "MAIL_MAILGUN_DOMAIN")?;
    let secret = require(config, keys::MAIL_MAILGUN_SECRET, "MAIL_MAILGUN_SECRET")?;

    let mut transport = MailgunTransport::new(http(), domain, secret);
    if let Some(endpoint) =
        config.get(keys::MAIL_MAILGUN_ENDPOINT).filter(|url| !url.trim().is_empty())
    {
        transport = transport.with_base_url(endpoint);
    }
    Ok(Arc::new(transport))
}

#[cfg(not(feature = "mail-mailgun"))]
fn mailgun(_: &Config) -> Result<Arc<dyn Transport>> {
    Err(feature_missing("mailgun", "mail-mailgun"))
}

#[cfg(feature = "mail-sendgrid")]
fn sendgrid(config: &Config) -> Result<Arc<dyn Transport>> {
    let key = require(config, keys::MAIL_SENDGRID_KEY, "MAIL_SENDGRID_KEY")?;
    Ok(Arc::new(SendGridTransport::new(http(), key)))
}

#[cfg(not(feature = "mail-sendgrid"))]
fn sendgrid(_: &Config) -> Result<Arc<dyn Transport>> {
    Err(feature_missing("sendgrid", "mail-sendgrid"))
}

#[cfg(feature = "mail-resend")]
fn resend(config: &Config) -> Result<Arc<dyn Transport>> {
    let key = require(config, keys::MAIL_RESEND_KEY, "MAIL_RESEND_KEY")?;
    Ok(Arc::new(ResendTransport::new(http(), key)))
}

#[cfg(not(feature = "mail-resend"))]
fn resend(_: &Config) -> Result<Arc<dyn Transport>> {
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
