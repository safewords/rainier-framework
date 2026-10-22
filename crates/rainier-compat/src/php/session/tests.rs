use std::sync::Arc;

use rainier_crypt::{Key as AppKey, KeyRing};
use rainier_http::{Request, Response, StatusCode};
use rainier_middleware::Pipeline;
use rainier_session::{MemorySessionStore, SessionRequestExt, SessionStore, StartSession};
use serde_json::json;

use super::*;
use crate::php::serialize::{serialize, Key, Object};

const PHP_ID: &str = "aB3dE6gH9jK2mN5pQ8sT1vW4yZ7bC0eF3hJ6kL9n";
const GUARD_KEY: &str = "login_web_59ba36addc2b2f9401580f014c7f58ea4e30989d";

fn keys() -> KeyRing {
    KeyRing::new(AppKey::from_bytes([11; 32]))
}

fn cookies() -> EncryptedCookies {
    EncryptedCookies::new(keys())
}

/// A payload as the PHP side stores it: the attributes, serialized.
fn payload() -> Vec<u8> {
    serialize(&Value::Array(vec![
        (Key::str("_token"), Value::string("csrf-from-php")),
        (Key::str(GUARD_KEY), Value::Int(42)),
        (Key::str("password_hash_web"), Value::string("abc123")),
        (
            Key::str("_previous"),
            Value::Array(vec![(Key::str("url"), Value::string("https://app.test/garage"))]),
        ),
        (
            Key::str("_flash"),
            Value::Array(vec![
                (Key::str("old"), Value::Array(vec![(Key::Int(0), Value::string("status"))])),
                (Key::str("new"), Value::Array(vec![])),
            ]),
        ),
        (Key::str("status"), Value::string("Saved.")),
        (
            Key::str("errors"),
            Value::Object(Object { class: "App\\Support\\ErrorBag".into(), properties: vec![] }),
        ),
    ]))
}

struct Setup {
    source: Arc<MemorySessionSource>,
    inner: Arc<MemorySessionStore>,
    store: Arc<ForeignSessionStore>,
}

fn setup(release: Release) -> Setup {
    let source = Arc::new(MemorySessionSource::new());
    source.insert(PHP_ID, payload());
    let inner = Arc::new(MemorySessionStore::default());
    let store = Arc::new(
        ForeignSessionStore::new(
            Arc::clone(&inner) as Arc<dyn SessionStore>,
            Arc::clone(&source) as Arc<dyn SessionSource>,
            cookies(),
            "app_session",
            &keys(),
        )
        .unwrap()
        .release(release),
    );
    Setup { source, inner, store }
}

/// The PHP session cookie as a browser holds it.
fn php_cookie() -> String {
    cookies().encrypt("app_session", PHP_ID).unwrap()
}

/// A request through `AdoptForeignSession` + `StartSession`, with a handler
/// that reports the session's user, token and flashed status.
async fn visit(setup: &Setup, cookie_header: &str, login_as: Option<&str>) -> Response {
    let login_as = login_as.map(str::to_string);
    Pipeline::new()
        .through(AdoptForeignSession::new("app_session", "rainier_session"))
        .through(StartSession::new(Arc::clone(&setup.store) as Arc<dyn SessionStore>))
        .then(move |request: Request| {
            let login_as = login_as.clone();
            async move {
                let session = request.session().unwrap();
                if let Some(user) = login_as {
                    session.regenerate();
                    session.put(RAINIER_AUTH_KEY, user).unwrap();
                }
                Response::json(&json!({
                    "user": session.string(RAINIER_AUTH_KEY),
                    "token": session.string("_token"),
                    "status": session.string("status"),
                    "previous": session.value("_previous"),
                    "errors": session.has("errors"),
                }))
            }
        })
        .run(Request::builder().uri("/garage").header("cookie", cookie_header).build())
        .await
}

fn body(response: &Response) -> serde_json::Value {
    match response.body() {
        rainier_http::Body::Bytes(bytes) => serde_json::from_slice(bytes).unwrap(),
        other => panic!("unexpected body {other:?}"),
    }
}

fn rainier_cookie(response: &Response) -> Option<String> {
    response.headers().get_all("set-cookie").iter().find_map(|value| {
        let value = value.to_str().ok()?;
        let rest = value.strip_prefix("rainier_session=")?;
        Some(rest.split(';').next()?.to_string())
    })
}

#[test]
fn the_auth_key_is_the_guards_own() {
    assert_eq!(RAINIER_AUTH_KEY, rainier_auth::AUTH_KEY);
}

#[test]
fn the_guard_key_is_matched_by_shape() {
    let mapping = SessionMapping::default();
    assert!(mapping.is_guard_key(GUARD_KEY));
    assert!(!mapping.is_guard_key("login_api_59ba36addc2b2f9401580f014c7f58ea4e30989d"));
    assert!(!mapping.is_guard_key("login_web_short"));
    assert!(!mapping.is_guard_key("login_web_59ba36addc2b2f9401580f014c7f58ea4e30989z"));
    assert!(mapping
        .clone()
        .guard("api")
        .is_guard_key("login_api_59ba36addc2b2f9401580f014c7f58ea4e30989d"));
}

#[test]
fn a_payload_maps_into_rainiers_shape() {
    let mapped = SessionMapping::default().map_bytes(&payload()).unwrap();

    assert_eq!(mapped.user_id.as_deref(), Some("42"));
    assert_eq!(mapped.data.values[RAINIER_AUTH_KEY], json!("42"));
    assert_eq!(mapped.data.values["_token"], json!("csrf-from-php"));
    assert_eq!(mapped.data.values["password_hash_web"], json!("abc123"));
    assert_eq!(mapped.data.values["_previous"], json!({ "url": "https://app.test/garage" }));
    assert_eq!(mapped.data.flash, vec!["status".to_string()]);
    assert!(!mapped.data.values.contains_key(GUARD_KEY), "renamed, not copied");
    assert!(!mapped.data.values.contains_key("_flash"));
    assert_eq!(mapped.skipped, vec!["errors".to_string()]);
}

#[test]
fn a_payload_that_is_not_an_array_is_refused() {
    assert!(SessionMapping::default().map_bytes(b"s:3:\"abc\";").is_err());
    assert!(SessionMapping::default().map_bytes(b"garbage").is_err());
}

#[tokio::test]
async fn coexisting_the_php_session_comes_across_and_is_left_alone() {
    let setup = setup(Release::Keep);

    let response = visit(&setup, &format!("app_session={}", php_cookie()), None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let seen = body(&response);
    assert_eq!(seen["user"], "42");
    assert_eq!(seen["token"], "csrf-from-php");
    assert_eq!(seen["status"], "Saved.", "flashed by the last PHP request, readable now");
    assert_eq!(seen["previous"]["url"], "https://app.test/garage");
    assert_eq!(seen["errors"], false);

    // Rainier's cookie now names the derived session; the PHP one is untouched.
    let id = rainier_cookie(&response).expect("a Rainier session cookie");
    assert_eq!(id, setup.store.derived_id(PHP_ID));
    assert!(rainier_session::is_well_formed(&id));
    assert!(setup.inner.read(&id).await.unwrap().is_some());
    assert!(setup.source.contains(PHP_ID), "Keep leaves the PHP session");
    assert!(
        response
            .headers()
            .get_all("set-cookie")
            .iter()
            .all(|c| !c.to_str().unwrap().starts_with("app_session=")),
        "the PHP cookie is never rewritten"
    );

    // The next request carries both cookies and reads Rainier's session; the
    // flash has aged out.
    let next =
        visit(&setup, &format!("app_session={}; rainier_session={id}", php_cookie()), None).await;
    let seen = body(&next);
    assert_eq!(seen["user"], "42");
    assert_eq!(seen["status"], serde_json::Value::Null);
}

#[tokio::test]
async fn concurrent_first_requests_converge_on_one_session() {
    let setup = setup(Release::Keep);
    let header = format!("app_session={}", php_cookie());

    let (a, b) = tokio::join!(visit(&setup, &header, None), visit(&setup, &header, None));
    assert_eq!(rainier_cookie(&a), rainier_cookie(&b));
    assert_eq!(setup.inner.len(), 1);
}

#[tokio::test]
async fn logging_in_regenerates_and_destroy_releases_the_php_session_when_asked() {
    let setup = setup(Release::Destroy);
    let response = visit(&setup, &format!("app_session={}", php_cookie()), Some("7")).await;

    let id = rainier_cookie(&response).unwrap();
    assert_ne!(id, setup.store.derived_id(PHP_ID), "regenerated at login");
    assert_eq!(body(&response)["user"], "7");
    assert!(!setup.source.contains(PHP_ID), "Destroy releases the PHP session");
}

#[tokio::test]
async fn keep_survives_a_login_too() {
    let setup = setup(Release::Keep);
    visit(&setup, &format!("app_session={}", php_cookie()), Some("7")).await;
    assert!(setup.source.contains(PHP_ID));
}

#[tokio::test]
async fn a_cookie_that_does_not_open_gets_a_fresh_session() {
    let setup = setup(Release::Keep);

    // Sealed for another cookie name.
    let moved = cookies().encrypt("theme", PHP_ID).unwrap();
    let seen = body(&visit(&setup, &format!("app_session={moved}"), None).await);
    assert_eq!(seen["user"], serde_json::Value::Null);

    // Sealed with another key.
    let foreign = EncryptedCookies::new(KeyRing::new(AppKey::from_bytes([1; 32])))
        .encrypt("app_session", PHP_ID)
        .unwrap();
    let seen = body(&visit(&setup, &format!("app_session={foreign}"), None).await);
    assert_eq!(seen["user"], serde_json::Value::Null);

    // A well-sealed value that is not a session id.
    let odd = cookies().encrypt("app_session", "../../etc/passwd").unwrap();
    let seen = body(&visit(&setup, &format!("app_session={odd}"), None).await);
    assert_eq!(seen["user"], serde_json::Value::Null);
}

#[tokio::test]
async fn an_unknown_php_session_is_a_fresh_one() {
    let setup = setup(Release::Keep);
    let unknown = cookies().encrypt("app_session", &"Z".repeat(40)).unwrap();
    let seen = body(&visit(&setup, &format!("app_session={unknown}"), None).await);
    assert_eq!(seen["user"], serde_json::Value::Null);
}

#[tokio::test]
async fn taking_over_reads_the_php_cookie_under_its_own_name() {
    // Rainier's cookie *is* the PHP one: no adoption middleware.
    let setup = setup(Release::Keep);
    let config = rainier_session::SessionConfig::default().cookie("app_session");
    let response = Pipeline::new()
        .through(StartSession::with_config(
            Arc::clone(&setup.store) as Arc<dyn SessionStore>,
            config,
        ))
        .then(|request: Request| async move {
            Response::text(request.session().unwrap().string(RAINIER_AUTH_KEY).unwrap_or_default())
        })
        .run(Request::builder().header("cookie", &format!("app_session={}", php_cookie())).build())
        .await;

    match response.body() {
        rainier_http::Body::Bytes(bytes) => assert_eq!(&bytes[..], b"42"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_client_side_store_underneath_is_refused() {
    let cookie_store =
        rainier_session::CookieSessionStore::new(rainier_crypt::Encryption::from_keys(keys()));
    let refused = ForeignSessionStore::new(
        Arc::new(cookie_store),
        Arc::new(MemorySessionSource::new()),
        cookies(),
        "app_session",
        &keys(),
    );
    assert!(refused.is_err());
}

#[tokio::test]
async fn the_cache_source_unwraps_the_doubly_serialized_payload() {
    let store = Arc::new(rainier_cache::MemoryCache::new());
    let cache = PhpCache::new(Arc::clone(&store) as _, "app_cache_");
    cache.put(PHP_ID, &Value::String(payload()), None).await.unwrap();

    let source = CacheSessionSource::new(cache);
    assert_eq!(source.load(PHP_ID).await.unwrap(), Some(payload()));
    assert_eq!(source.load(&"Q".repeat(40)).await.unwrap(), None);
    source.destroy(PHP_ID).await.unwrap();
    assert_eq!(source.load(PHP_ID).await.unwrap(), None);
}

#[tokio::test]
async fn the_file_source_reads_by_id_and_refuses_paths() {
    let dir = std::env::temp_dir().join(format!("rainier-compat-sessions-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(PHP_ID), payload()).unwrap();

    let source = FileSessionSource::new(&dir);
    assert_eq!(source.load(PHP_ID).await.unwrap(), Some(payload()));
    assert_eq!(source.load("../../../../etc/passwd").await.unwrap(), None);
    source.destroy(PHP_ID).await.unwrap();
    assert_eq!(source.load(PHP_ID).await.unwrap(), None);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_database_row_payload_is_base64() {
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(payload());
    assert_eq!(database_payload(&encoded).unwrap(), payload());
    assert!(database_payload("not base64!").is_err());
}

// --- remember-me ------------------------------------------------------------------

struct Users;

#[async_trait]
impl RememberedUsers for Users {
    async fn password_hash_for(&self, id: &str, token: &str) -> Result<Option<String>> {
        Ok((id == "42" && token == "remember-token").then(|| "$2y$10$hash".to_string()))
    }
}

const REMEMBER: &str = "remember_web_59ba36addc2b2f9401580f014c7f58ea4e30989d";

fn remember(value: &str) -> Request {
    let sealed = cookies().encrypt(REMEMBER, value).unwrap();
    Request::builder().header("cookie", &format!("{REMEMBER}={sealed}")).build()
}

#[tokio::test]
async fn a_remember_cookie_signs_in_with_the_password_mac() {
    let reader = RememberCookies::new(cookies(), "web", "base64:configured-key");
    let mac = reader.password_mac("$2y$10$hash");

    let request = remember(&format!("42|remember-token|{mac}"));
    assert_eq!(reader.user_id(&request, &Users).await.unwrap().as_deref(), Some("42"));
}

#[tokio::test]
async fn a_remember_cookie_with_the_raw_hash_from_before_the_mac_still_reads() {
    let reader = RememberCookies::new(cookies(), "web", "base64:configured-key");
    let request = remember("42|remember-token|$2y$10$hash");
    assert_eq!(reader.user_id(&request, &Users).await.unwrap().as_deref(), Some("42"));
}

#[tokio::test]
async fn a_remember_cookie_is_refused_after_a_password_change_or_with_a_wrong_token() {
    let reader = RememberCookies::new(cookies(), "web", "base64:configured-key");
    let stale =
        RememberCookies::new(cookies(), "web", "base64:configured-key").password_mac("$2y$10$old");

    assert_eq!(
        reader.user_id(&remember(&format!("42|remember-token|{stale}")), &Users).await.unwrap(),
        None
    );
    let mac = reader.password_mac("$2y$10$hash");
    assert_eq!(reader.user_id(&remember(&format!("42|wrong|{mac}")), &Users).await.unwrap(), None);
    assert_eq!(reader.user_id(&remember("42|remember-token"), &Users).await.unwrap(), None);
    assert_eq!(reader.user_id(&Request::builder().build(), &Users).await.unwrap(), None);
}

#[test]
fn the_password_mac_is_hmac_sha256_keyed_with_the_configured_string() {
    let reader = RememberCookies::new(cookies(), "web", "key");
    // RFC 4231 test case 2: key "Jefe", data "what do ya want for nothing?".
    let jefe = RememberCookies::new(cookies(), "web", "Jefe");
    assert_eq!(
        jefe.password_mac("what do ya want for nothing?"),
        "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
    );
    assert!(reader.is_cookie_name(REMEMBER));
    assert!(!reader.is_cookie_name("remember_web_x"));
}

#[test]
fn recallers_need_all_three_parts() {
    assert_eq!(
        Recaller::parse("1|t|h|extra"),
        Some(Recaller { id: "1".into(), token: "t".into(), hash: "h".into() })
    );
    assert_eq!(Recaller::parse("1|t"), None);
    assert_eq!(Recaller::parse("|t|h"), None);
}
