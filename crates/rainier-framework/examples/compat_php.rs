//! Rainier running beside a PHP application, offline.
//!
//! Everything a migration shares with the application it is replacing, in the
//! order it is usually adopted — each value built the way the PHP side builds
//! it, then read back by Rainier:
//!
//! 1. the application key, shared;
//! 2. a signed-in PHP session, adopted into a Rainier session — the user is
//!    signed in on both sides, and the PHP cookie is never touched;
//! 3. a state-changing request carrying the PHP-issued `XSRF-TOKEN` cookie,
//!    accepted by Rainier's own CSRF middleware — and a forged one refused;
//! 4. a plain and a tagged cache value from the shared Redis;
//! 5. a serialized object with protected and private properties, read as
//!    data;
//! 6. a refresh token sealed the way the PHP OAuth server seals them, opened
//!    by the token sealer Rainier's OAuth server uses;
//! 7. a stored passkey credential, read so the passkey keeps working.
//!
//! ```sh
//! cargo run -p rainier-framework --example compat_php \
//!     --features compat-php-oauth,compat-php-webauthn
//! ```
//!
//! In an application the memory stores become the shared Redis and database,
//! and the key comes from the PHP application's environment. See
//! `docs/compat.md` for the playbook.

use std::sync::Arc;

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine as _;
use rainier_framework::cache::{Cache, MemoryCache};
use rainier_framework::compat::php::cache::{tagged_key, PhpCache};
use rainier_framework::compat::php::cookie::EncryptedCookies;
use rainier_framework::compat::php::csrf::EncryptedXsrf;
use rainier_framework::compat::php::oauth::DefuseSealer;
use rainier_framework::compat::php::serialize::{serialize, unserialize, Key, Value, Visibility};
use rainier_framework::compat::php::session::{
    AdoptForeignSession, ForeignSessionStore, MemorySessionSource, SessionSource,
};
use rainier_framework::compat::php::webauthn;
use rainier_framework::crypt::{Key as AppKey, KeyRing};
use rainier_framework::http::{Body, Method, Request, Response};
use rainier_framework::mfa::webauthn::testing::SoftwareKey;
use rainier_framework::middleware::Pipeline;
use rainier_framework::oauth_server::{RefreshTokenPayload, TokenSealer};
use rainier_framework::session::{
    MemorySessionStore, SessionConfig, SessionRequestExt, SessionStore, StartSession,
    VerifyCsrfToken,
};
use serde_json::json;

/// The PHP application's session cookie, and Rainier's beside it.
const PHP_COOKIE: &str = "app_session";
const RAINIER_COOKIE: &str = "rainier_session";

/// The key the PHP application signs in with — `login_{guard}_` and the SHA-1
/// of its guard class. Any forty hex digits match.
const GUARD_KEY: &str = "login_web_59ba36addc2b2f9401580f014c7f58ea4e30989d";

#[tokio::main]
async fn main() -> rainier_framework::support::Result<()> {
    // --- 1. the shared application key ---------------------------------------
    //
    // Exactly the string in the PHP application's environment.
    let app_key = format!("base64:{}", STANDARD.encode([0x5au8; 32]));
    let keys = KeyRing::new(AppKey::from_base64(app_key.trim_start_matches("base64:"))?);
    let cookies = EncryptedCookies::new(keys.clone());
    step(1, "shared application key", "one KeyRing, from the PHP application's APP_KEY");

    // --- 2. a signed-in PHP session, adopted ----------------------------------
    //
    // What the PHP side has: a session payload in its store, and the session
    // id sealed into its cookie.
    let php_session_id = "Ab1Cd2Ef3Gh4Ij5Kl6Mn7Op8Qr9St0Uv1Wx2Yz3A";
    let csrf_token = "tok-issued-by-php-0123456789abcdef01234";
    let payload = serialize(&Value::Array(vec![
        (Key::str("_token"), Value::string(csrf_token)),
        (Key::str(GUARD_KEY), Value::Int(42)),
        (
            Key::str("_flash"),
            Value::Array(vec![
                (Key::str("old"), Value::Array(vec![(Key::Int(0), Value::string("status"))])),
                (Key::str("new"), Value::Array(vec![])),
            ]),
        ),
        (Key::str("status"), Value::string("Charging schedule saved.")),
    ]));
    let php_sessions = Arc::new(MemorySessionSource::new());
    php_sessions.insert(php_session_id, payload);
    let php_cookie = cookies.encrypt(PHP_COOKIE, php_session_id)?;

    // Rainier's side: its own sessions, reading PHP ones through.
    let rainier_sessions = Arc::new(MemorySessionStore::default());
    let store: Arc<dyn SessionStore> = Arc::new(ForeignSessionStore::new(
        Arc::clone(&rainier_sessions) as Arc<dyn SessionStore>,
        Arc::clone(&php_sessions) as Arc<dyn SessionSource>,
        cookies.clone(),
        PHP_COOKIE,
        &keys,
    )?);

    let web = |store: Arc<dyn SessionStore>, cookies: EncryptedCookies| {
        Pipeline::new()
            .through(AdoptForeignSession::new(PHP_COOKIE, RAINIER_COOKIE))
            .through(StartSession::with_config(
                store,
                SessionConfig::default().cookie(RAINIER_COOKIE),
            ))
            .through(VerifyCsrfToken::new().codec(Arc::new(EncryptedXsrf::new(cookies))))
            .then(|request: Request| async move {
                let session = request.session().expect("inside StartSession");
                Response::json(&json!({
                    "user": session.string("_auth_id"),
                    "flashed": session.string("status"),
                }))
            })
    };

    let response = web(Arc::clone(&store), cookies.clone())
        .run(
            Request::builder()
                .uri("/garage")
                .header("cookie", &format!("{PHP_COOKIE}={php_cookie}"))
                .build(),
        )
        .await;
    let seen = body(&response);
    step(
        2,
        "PHP session adopted",
        &format!(
            "user {} signed in, flash {:?}, Rainier cookie set: {}, PHP session kept: {}",
            seen["user"],
            seen["flashed"].as_str().unwrap_or_default(),
            set_cookie(&response, RAINIER_COOKIE).is_some(),
            php_sessions.contains(php_session_id),
        ),
    );

    // --- 3. CSRF with the PHP-issued XSRF-TOKEN cookie -------------------------
    //
    // The PHP side sealed the token into `XSRF-TOKEN`; the front end copies the
    // sealed value into `X-XSRF-TOKEN`, as it always has.
    let xsrf_cookie = cookies.encrypt("XSRF-TOKEN", csrf_token)?;
    let post = |xsrf: &str| {
        Request::builder()
            .method(Method::POST)
            .uri("/vehicles/1/schedule")
            .header("cookie", &format!("{PHP_COOKIE}={php_cookie}"))
            .header("x-xsrf-token", xsrf)
            .build()
    };
    let accepted = web(Arc::clone(&store), cookies.clone()).run(post(&xsrf_cookie)).await;
    let forged = web(Arc::clone(&store), cookies.clone())
        .run(post(&cookies.encrypt("XSRF-TOKEN", "a-token-the-attacker-chose")?))
        .await;
    step(
        3,
        "CSRF",
        &format!(
            "PHP-issued token -> {}, forged token -> {}",
            accepted.status().as_u16(),
            forged.status().as_u16(),
        ),
    );

    // --- 4. cache values on the shared Redis -----------------------------------
    //
    // What the PHP side wrote: a plain value, a raw number, and an entry
    // tagged `vehicle-telemetry`.
    let redis = Arc::new(MemoryCache::new());
    let prefix = "app_cache_";
    redis.put(&format!("{prefix}exchange-rate"), b"1.0842", None).await?;
    redis
        .put(
            &format!("{prefix}fleet:summary"),
            br#"a:2:{s:8:"vehicles";i:3;s:6:"online";b:1;}"#,
            None,
        )
        .await?;
    let vin = "5YJ3E1EA7KF000001";
    let item = tagged_key("tag:vehicle-telemetry:entries", vin);
    redis
        .put(
            &format!("{prefix}{item}"),
            br#"a:2:{s:3:"soc";i:81;s:5:"state";s:8:"charging";}"#,
            None,
        )
        .await?;

    let shared = PhpCache::new(Arc::clone(&redis) as Arc<dyn Cache>, prefix);
    let rate = shared.get("exchange-rate").await?;
    let summary = shared.get("fleet:summary").await?.and_then(|v| v.to_json());
    let reading = shared.tags(&["vehicle-telemetry"]).get(vin).await?.and_then(|v| v.to_json());
    step(
        4,
        "shared cache",
        &format!(
            "rate {rate:?}, summary {}, tagged reading {}",
            summary.unwrap_or_default(),
            reading.unwrap_or_default()
        ),
    );

    // --- 5. a serialized object, as data ----------------------------------------
    let bytes = b"O:11:\"App\\Vehicle\":3:{s:3:\"vin\";s:17:\"5YJ3E1EA7KF000001\";s:11:\"\0*\0odometer\";i:48210;s:19:\"\0App\\Vehicle\0secret\";s:4:\"shh!\";}";
    let object = unserialize(bytes)?;
    let object = object.as_object().expect("an object");
    let properties: Vec<String> = object
        .properties
        .iter()
        .map(|(property, _)| {
            let visibility = match property.visibility() {
                Visibility::Public => "public".to_string(),
                Visibility::Protected => "protected".to_string(),
                Visibility::Private(class) => format!("private to {class}"),
            };
            format!("{} ({visibility})", property.name())
        })
        .collect();
    step(
        5,
        "unserialize",
        &format!(
            "class {}, properties [{}] — nothing instantiated",
            object.class,
            properties.join(", "),
        ),
    );

    // --- 6. a refresh token, sealed as the PHP OAuth server seals it ------------
    //
    // `DefuseSealer` is a `TokenSealer`: hand it to Rainier's authorization
    // server and every refresh token the PHP side issued keeps refreshing.
    let sealer: Box<dyn TokenSealer> = Box::new(DefuseSealer::from_app_key(&app_key)?);
    let issued = RefreshTokenPayload {
        client_id: "7".into(),
        refresh_token_id: "c0ffee".repeat(13),
        access_token_id: "beef".repeat(20),
        scopes: vec!["vehicle_data".into(), "offline_access".into()],
        user_id: Some("42".into()),
        expire_time: 1_893_456_000,
    };
    let sealed = sealer.seal(&serde_json::to_vec(&issued)?)?;
    let opened: RefreshTokenPayload = serde_json::from_slice(&sealer.open(&sealed)?)?;
    step(
        6,
        "refresh token",
        &format!(
            "sealed as {}… ({} hex chars), opened for user {:?}, scopes {:?}",
            &sealed[..8],
            sealed.len(),
            opened.user_id,
            opened.scopes,
        ),
    );

    // --- 7. a stored passkey --------------------------------------------------------
    //
    // The JSON column the PHP side keeps per passkey. The public key is the
    // authenticator's own COSE bytes, so it verifies here unchanged.
    let public_key = SoftwareKey::es256().cose();
    let stored = json!({
        "publicKeyCredentialId": URL_SAFE_NO_PAD.encode(b"passkey-on-a-phone"),
        "type": "public-key",
        "transports": ["internal", "hybrid"],
        "attestationType": "none",
        "aaguid": "00000000-0000-0000-0000-000000000000",
        "credentialPublicKey": URL_SAFE_NO_PAD.encode(&public_key),
        "userHandle": URL_SAFE_NO_PAD.encode(b"42"),
        "counter": 12,
        "backupEligible": true,
        "backupStatus": true,
    });
    let credential = webauthn::read_credential(&stored.to_string())?;
    step(
        7,
        "passkey",
        &format!(
            "credential {} for user handle {:?}, counter {}, synced: {}",
            credential.id_base64url(),
            String::from_utf8_lossy(&credential.user_handle),
            credential.sign_count,
            credential.backed_up,
        ),
    );

    Ok(())
}

fn step(n: u8, title: &str, detail: &str) {
    println!("{n}. {title:<26} {detail}");
}

fn body(response: &Response) -> serde_json::Value {
    match response.body() {
        Body::Bytes(bytes) => serde_json::from_slice(bytes).unwrap_or_default(),
        _ => serde_json::Value::Null,
    }
}

fn set_cookie(response: &Response, name: &str) -> Option<String> {
    response.headers().get_all("set-cookie").iter().find_map(|value| {
        let value = value.to_str().ok()?;
        value
            .strip_prefix(&format!("{name}="))
            .map(|rest| rest.split(';').next().unwrap_or("").to_string())
    })
}
