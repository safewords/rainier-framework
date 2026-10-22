//! Values exported from a running PHP application by
//! `tools/php-fixtures.php`, read back here.
//!
//! Ignored by default: they need the file. Run with
//!
//! ```text
//! RAINIER_COMPAT_FIXTURES=fixtures.json cargo test -p rainier-compat --features php -- --ignored
//! ```
//!
//! Every test here is a format the unit tests already pin against
//! independently computed values; these pin it against **the** values, from
//! the application being migrated, under its real key. The file holds that
//! key — keep it out of version control and delete it after.

use std::sync::Arc;

use base64::Engine as _;
use rainier_crypt::{Key, KeyRing};
use serde_json::Value as Json;

use super::cache::{tagged_key, PhpCache};
use super::cookie::EncryptedCookies;
use super::csrf::EncryptedXsrf;
use super::serialize::{serialize, unserialize};
use super::session::{RememberCookies, SessionMapping};
use rainier_session::XsrfCodec;

fn fixtures() -> Json {
    let path = std::env::var("RAINIER_COMPAT_FIXTURES")
        .expect("set RAINIER_COMPAT_FIXTURES to the file tools/php-fixtures.php wrote");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    serde_json::from_str(&text).expect("the fixtures file is JSON")
}

fn text<'a>(json: &'a Json, path: &str) -> &'a str {
    path.split('.')
        .try_fold(json, |json, segment| json.get(segment))
        .and_then(Json::as_str)
        .unwrap_or_else(|| panic!("fixtures have no string at {path}"))
}

fn bytes(json: &Json, path: &str) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD.decode(text(json, path)).expect("base64")
}

/// The ring from the configured key string — `base64:…`, or raw.
fn keys(json: &Json) -> KeyRing {
    let configured = text(json, "app_key");
    let key = match configured.strip_prefix("base64:") {
        Some(encoded) => Key::from_base64(encoded).expect("a 32-byte base64 key"),
        None => {
            let raw: [u8; 32] = configured.as_bytes().try_into().expect("a 32-byte raw key");
            Key::from_bytes(raw)
        }
    };
    KeyRing::new(key)
}

#[test]
#[ignore = "needs RAINIER_COMPAT_FIXTURES"]
fn serialized_values_read_and_write_back_identically() {
    let json = fixtures();
    for sample in json["serialize"].as_array().expect("serialize samples") {
        let php = base64::engine::general_purpose::STANDARD
            .decode(sample["php"].as_str().unwrap())
            .unwrap();
        let value = unserialize(&php).unwrap();
        if let Some(expected) = sample.get("json").filter(|j| !j.is_null()) {
            assert_eq!(&value.to_json().unwrap(), expected);
        }
        if sample["byte_identical"].as_bool().unwrap_or(true) {
            assert_eq!(serialize(&value), php, "{}", String::from_utf8_lossy(&php));
        }
    }
}

#[test]
#[ignore = "needs RAINIER_COMPAT_FIXTURES"]
fn a_real_encrypted_cookie_opens() {
    let json = fixtures();
    let cookies = EncryptedCookies::new(keys(&json));
    let opened = cookies.decrypt(text(&json, "cookie.name"), text(&json, "cookie.sealed")).unwrap();
    assert_eq!(opened, text(&json, "cookie.plain"));

    // And one sealed here opens there: the PHP side's check is the prefix and
    // the envelope, both of which the unit tests pin; round-tripping under the
    // real key is the part left.
    let sealed = cookies.encrypt(text(&json, "cookie.name"), "from-rainier").unwrap();
    assert_eq!(cookies.decrypt(text(&json, "cookie.name"), &sealed).unwrap(), "from-rainier");
}

#[test]
#[ignore = "needs RAINIER_COMPAT_FIXTURES"]
fn a_real_session_payload_maps() {
    let json = fixtures();
    let payload = bytes(&json, "session.payload");
    let mapped =
        SessionMapping::default().guard(text(&json, "session.guard")).map_bytes(&payload).unwrap();
    assert_eq!(mapped.user_id.as_deref(), Some(text(&json, "session.user_id")));
    assert_eq!(mapped.data.values["_token"], Json::String(text(&json, "session.token").into()));
}

#[tokio::test]
#[ignore = "needs RAINIER_COMPAT_FIXTURES"]
async fn a_real_cache_value_and_tagged_key_read() {
    let json = fixtures();
    let store = Arc::new(rainier_cache::MemoryCache::new());
    let cache = PhpCache::new(Arc::clone(&store) as _, text(&json, "cache.prefix"));

    let stored = bytes(&json, "cache.stored");
    rainier_cache::Cache::put(&*store, &cache.key(text(&json, "cache.key")), &stored, None)
        .await
        .unwrap();
    let value = cache.get(text(&json, "cache.key")).await.unwrap().unwrap();
    assert_eq!(value.to_json().unwrap(), json["cache"]["json"]);
    assert_eq!(super::cache::encode(&value), stored, "written back the same way");

    // A store without tag support exports no tagged key.
    if json["tagged"]["item_key"].is_null() {
        return;
    }
    let tags: Vec<&str> =
        json["tagged"]["tags"].as_array().unwrap().iter().map(|t| t.as_str().unwrap()).collect();
    let namespace: Vec<String> = tags.iter().map(|tag| format!("tag:{tag}:entries")).collect();
    assert_eq!(
        tagged_key(&namespace.join("|"), text(&json, "tagged.key")),
        text(&json, "tagged.item_key")
    );
}

#[test]
#[ignore = "needs RAINIER_COMPAT_FIXTURES"]
fn a_real_xsrf_cookie_opens_to_the_session_token() {
    let json = fixtures();
    let codec = EncryptedXsrf::new(EncryptedCookies::new(keys(&json)));
    assert_eq!(codec.open(text(&json, "xsrf.sealed")).as_deref(), Some(text(&json, "xsrf.token")));
}

#[test]
#[ignore = "needs RAINIER_COMPAT_FIXTURES"]
fn a_real_remember_cookie_verifies() {
    let json = fixtures();
    let Some(remember) = json.get("remember").filter(|r| !r.is_null()) else {
        return;
    };
    let cookies = EncryptedCookies::new(keys(&json));
    let opened = cookies.decrypt(text(remember, "name"), text(remember, "sealed")).unwrap();
    let recaller = super::session::Recaller::parse(&opened).unwrap();
    assert_eq!(recaller.id, text(remember, "user_id"));

    let reader =
        RememberCookies::new(cookies, text(&json, "session.guard"), text(&json, "app_key"));
    assert_eq!(reader.password_mac(text(remember, "password_hash")), recaller.hash);
}
