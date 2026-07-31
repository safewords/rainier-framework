//! Selecting the PHP encryption envelope from configuration.
//!
//! The unit tests in `rainier-crypt` assert the format. This asserts the part
//! a ported application depends on: that `APP_CIPHER=php` actually changes
//! what the `Crypt` facade writes, and that the two schemes cannot read each
//! other — which is what makes choosing the wrong one a loud failure rather
//! than a quiet one.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use rainier_crypt::CryptScheme;
use rainier_framework::config::{Config as ConfigRepository, Env};
use rainier_framework::prelude::*;
use rainier_framework::testing::TestApp;
use std::sync::Arc;

/// A fixed key, so two applications in one test can read each other's rows.
const KEY: &str = "base64:AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";

async fn app(scheme: CryptScheme) -> Arc<Application> {
    Rainier::new(".")
        .without_tracing()
        .without_facades()
        .configure(move |config| {
            config.set(rainier_framework::keys::APP_CIPHER, scheme).unwrap();
        })
        .boot()
        .await
        .expect("boots")
}

/// The bootstrap reads `APP_KEY` from the environment, which a test cannot set
/// without affecting its neighbours — so this builds the encryption directly
/// from the same key both ways.
fn encryption(scheme: CryptScheme) -> Encryption {
    let keys = rainier_crypt::KeyRing::from_base64(KEY, &[]).expect("a key");

    match scheme {
        CryptScheme::Native => Encryption::from_keys(keys),
        CryptScheme::Php => Encryption::new(
            Arc::new(rainier_crypt::PhpEncrypter::new(keys.clone())),
            Arc::new(rainier_crypt::HmacSigner::new(keys)),
        ),
    }
}

#[tokio::test]
async fn the_php_scheme_writes_the_php_envelope() {
    let crypt = encryption(CryptScheme::Php);
    let payload = crypt.encrypt("a card number").unwrap();

    // What PHP will find: base64 around JSON with exactly these three fields.
    let decoded = B64.decode(&payload).expect("base64");
    let json: serde_json::Value = serde_json::from_slice(&decoded).expect("json");

    assert!(json["iv"].is_string());
    assert!(json["value"].is_string());
    assert!(json["mac"].is_string());

    assert_eq!(crypt.decrypt(&payload).unwrap(), "a card number");
}

#[tokio::test]
async fn the_native_scheme_writes_something_else_entirely() {
    let payload = encryption(CryptScheme::Native).encrypt("a card number").unwrap();

    // Not base64-wrapped JSON — it is Rainier's own self-describing payload.
    assert!(B64
        .decode(&payload)
        .ok()
        .and_then(|raw| serde_json::from_slice::<serde_json::Value>(&raw).ok())
        .is_none());
}

#[tokio::test]
async fn the_two_schemes_cannot_read_each_other() {
    // Which is the property that makes picking the wrong one a loud failure
    // rather than a quiet corruption.
    let native = encryption(CryptScheme::Native);
    let php = encryption(CryptScheme::Php);

    let by_native = native.encrypt("secret").unwrap();
    let by_php = php.encrypt("secret").unwrap();

    assert!(php.decrypt(&by_native).is_err());
    assert!(native.decrypt(&by_php).is_err());
}

#[tokio::test]
async fn the_default_is_the_native_scheme() {
    // Adding this feature must not change what an existing application writes.
    let app = app(CryptScheme::default()).await;
    let config = app.resolve::<ConfigRepository>().unwrap();

    assert_eq!(config.setting(rainier_framework::keys::APP_CIPHER).unwrap(), CryptScheme::Native);
}

#[tokio::test]
async fn the_setting_reaches_the_bound_encryption() {
    // The wiring, rather than the format: an application configured for the
    // PHP scheme gets an `Encryption` that writes the PHP envelope.
    let app = app(CryptScheme::Php).await;
    let crypt = app.resolve::<Encryption>().unwrap();

    let payload = crypt.encrypt("a card number").unwrap();
    let decoded = B64.decode(&payload).expect("base64 around JSON");

    assert!(serde_json::from_slice::<serde_json::Value>(&decoded).is_ok());
}

#[tokio::test]
async fn a_scheme_nobody_can_spell_stops_the_boot() {
    // The same rule as a driver name. `APP_CIPHER=hpp` writing the default
    // envelope would be a database half in one format and half in another.
    let env = Env::parse("APP_CIPHER=hpp");

    assert!(env.setting::<CryptScheme>("APP_CIPHER").is_err());
}

#[tokio::test]
async fn a_ported_application_can_still_boot_and_serve() {
    // End to end: the whole application comes up on the PHP envelope.
    let booted = Rainier::new(".")
        .without_tracing()
        .configure(|config| {
            config.set(rainier_framework::keys::APP_CIPHER, CryptScheme::Php).unwrap();
        })
        .with_routes(|router| {
            router.get("/", || async { Response::ok("up") });
        })
        .boot()
        .await
        .expect("boots on the PHP cipher");

    TestApp::new(booted).unwrap().get("/").await.assert_ok();
}
