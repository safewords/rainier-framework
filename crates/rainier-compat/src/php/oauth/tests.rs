use std::sync::Arc;

use rainier_database::testing::{fake_database, MemoryConnection};
use rainier_database::{Cell, OwnedRow};
use rainier_http::{Method, Request};
use rainier_oauth_server::{
    AccessTokenSigner, AccessTokenVerifier, AuthCodePayload, AuthorizationServer, MemoryStore,
    RefreshTokenPayload, Scopes,
};
use rainier_orm::Dialect;
use serde_json::Value as Json;

use super::*;

// --- the sealer -----------------------------------------------------------------

#[test]
fn the_app_key_is_decoded_when_it_says_base64() {
    assert_eq!(app_key_bytes("base64:AAECAw==").unwrap(), vec![0, 1, 2, 3]);
    assert_eq!(app_key_bytes("plain-key").unwrap(), b"plain-key".to_vec());
    assert!(app_key_bytes("base64:!!!").is_err());
}

#[test]
fn a_sealed_refresh_token_is_a_defuse_ciphertext_holding_the_league_payload() {
    let sealer =
        DefuseSealer::from_app_key("base64:AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=").unwrap();
    let sealed = sealer.seal(br#"{"refresh_token_id":"r"}"#).unwrap();
    assert!(sealed.starts_with("def50200"));
    assert_eq!(sealer.open(&sealed).unwrap(), br#"{"refresh_token_id":"r"}"#);
    assert!(DefuseSealer::new(b"other".to_vec()).open(&sealed).is_err());
}

// --- client rows ----------------------------------------------------------------

fn flag_shaped(secret: Option<&str>, owner: Option<&str>) -> OwnedRow {
    OwnedRow::new()
        .with("id", "9d5f3c1e-0000-4000-8000-000000000001")
        .with("name", "Mobile")
        .with("secret", secret.map(str::to_string))
        .with("provider", Cell::Null)
        .with("redirect", "https://a.test/cb, https://b.test/cb")
        .with("redirect_uris", Cell::Null)
        .with("grant_types", Cell::Null)
        .with("scopes", Cell::Null)
        .with("owner_ref", owner.map(str::to_string))
        .with("personal_access_client", false)
        .with("password_client", true)
        .with("revoked", false)
}

#[test]
fn a_flag_shaped_row_derives_its_grants_as_the_php_model_does() {
    let client = client_from_row(&flag_shaped(Some("$2y$10$hash"), None));
    assert_eq!(client.redirect_uris, vec!["https://a.test/cb", "https://b.test/cb"]);
    assert_eq!(
        client.grant_types,
        vec!["authorization_code", "client_credentials", "password", "refresh_token"]
    );
    assert!(client.is_confidential());
    assert!(client.is_first_party());
}

#[test]
fn an_owned_or_public_client_gets_no_client_credentials() {
    let owned = client_from_row(&flag_shaped(Some("$2y$10$hash"), Some("7")));
    assert!(!owned.supports_grant("client_credentials"));
    assert_eq!(owned.owner_id.as_deref(), Some("7"));

    let public = client_from_row(&flag_shaped(None, None));
    assert!(!public.supports_grant("client_credentials"));
    assert!(!public.is_confidential());
}

#[test]
fn a_personal_access_flag_needs_a_secret_too() {
    let row = flag_shaped(Some("$2y$10$hash"), None).with("personal_access_client", true);
    assert!(client_from_row(&row).supports_grant("personal_access"));
    let row = flag_shaped(None, None).with("personal_access_client", true);
    assert!(!client_from_row(&row).supports_grant("personal_access"));
}

#[test]
fn a_list_shaped_row_is_taken_at_its_word() {
    let row = OwnedRow::new()
        .with("id", "c")
        .with("name", "Web")
        .with("secret", "$2y$10$hash")
        .with("redirect_uris", r#"["https://app.test/cb"]"#)
        .with("grant_types", r#"["authorization_code","refresh_token"]"#)
        .with("scopes", r#"["account"]"#)
        .with("revoked", true);
    let client = client_from_row(&row);
    assert_eq!(client.redirect_uris, vec!["https://app.test/cb"]);
    assert_eq!(client.grant_types, vec!["authorization_code", "refresh_token"]);
    assert_eq!(client.scopes, Some(vec!["account".to_string()]));
    assert!(client.revoked);
}

// --- the SQL ------------------------------------------------------------------------

fn store(connection: MemoryConnection) -> (LeagueOAuthStore, Arc<MemoryConnection>) {
    let (db, connection) = fake_database(connection);
    (LeagueOAuthStore::new(Arc::new(db)), connection)
}

#[tokio::test]
async fn a_client_lookup_asks_for_the_owner_column_once_and_reads_it_as_text() {
    let (store, connection) = store(
        MemoryConnection::new(Dialect::MySql)
            .returning([flag_shaped(Some("$2y$10$hash"), None)])
            .returning([flag_shaped(Some("$2y$10$hash"), None)]),
    );

    store.find_client("c").await.unwrap().unwrap();
    store.find_client("c").await.unwrap().unwrap();

    let statements = connection.statements();
    assert_eq!(statements.len(), 3, "one probe, then one SELECT per lookup: {statements:#?}");
    assert_eq!(statements[0], "SELECT user_id FROM oauth_clients WHERE 1 = 0");
    assert_eq!(
        statements[1],
        "SELECT *, CAST(user_id AS CHAR) AS owner_ref FROM oauth_clients WHERE id = ?"
    );
}

#[tokio::test]
async fn an_access_token_row_is_written_as_the_php_side_writes_it() {
    let (store, connection) = store(MemoryConnection::new(Dialect::MySql));
    let created = Utc.with_ymd_and_hms(2026, 10, 21, 12, 0, 0).unwrap();
    store
        .create_access_token(&AccessTokenRecord {
            id: "t".into(),
            user_id: Some("42".into()),
            client_id: "c".into(),
            name: None,
            scopes: vec!["account".into(), "mcp:use".into()],
            revoked: false,
            created_at: created,
            expires_at: created + chrono::Duration::days(1),
        })
        .await
        .unwrap();

    let recorded = &connection.recorded()[0];
    assert!(recorded.sql.starts_with("INSERT INTO oauth_access_tokens (id, user_id, client_id"));
    assert_eq!(recorded.params[1], Value::BigInt(Some(42)), "an integer column gets an integer");
    assert_eq!(recorded.params[4], Value::String(Some(r#"["account","mcp:use"]"#.into())));
    assert_eq!(recorded.params[6], Value::String(Some("2026-10-21 12:00:00".into())));
    assert_eq!(recorded.params[8], Value::String(Some("2026-10-22 12:00:00".into())));
}

#[tokio::test]
async fn datetimes_are_written_and_read_in_the_configured_timezone() {
    let (store, connection) = store(MemoryConnection::new(Dialect::Postgres));
    let store = store.timezone(FixedOffset::west_opt(7 * 3600).unwrap());
    let at = Utc.with_ymd_and_hms(2026, 10, 21, 12, 0, 0).unwrap();
    assert_eq!(store.format(at), "2026-10-21 05:00:00");
    assert_eq!(store.parse("2026-10-21 05:00:00"), Some(at));

    store
        .create_refresh_token(&RefreshTokenRecord {
            id: "r".into(),
            access_token_id: "a".into(),
            revoked: false,
            expires_at: at,
        })
        .await
        .unwrap();
    let sql = connection.statements().remove(0);
    assert!(sql.contains("CAST($4 AS TIMESTAMP)") || sql.contains("CAST(? AS TIMESTAMP)"), "{sql}");
}

#[tokio::test]
async fn revoking_is_a_claim_that_reports_whether_it_took() {
    let (store, connection) = store(MemoryConnection::new(Dialect::MySql).with_outcome(1, 0));
    assert!(store.revoke_refresh_token("r").await.unwrap());
    assert!(connection.statements()[0].ends_with("WHERE id = ? AND revoked = ?"));

    let (store, _) = store_with_outcome(0);
    assert!(
        !store.revoke_refresh_token("r").await.unwrap(),
        "already revoked: someone else spent it"
    );
}

fn store_with_outcome(rows: u64) -> (LeagueOAuthStore, Arc<MemoryConnection>) {
    store(MemoryConnection::new(Dialect::MySql).with_outcome(rows, 0))
}

#[tokio::test]
async fn a_token_row_reads_back_with_its_scopes_and_expiry() {
    let row = OwnedRow::new()
        .with("id", "t")
        .with("user_id", "42")
        .with("client_id", "c")
        .with("name", Cell::Null)
        .with("scopes", r#"["account","vehicles"]"#)
        .with("revoked", false)
        .with("created_at", "2026-10-21 12:00:00")
        .with("expires_at", "2026-10-22 12:00:00");
    let (store, _) = store(MemoryConnection::new(Dialect::MySql).returning([row]));
    let token = store.find_access_token("t").await.unwrap().unwrap();
    assert_eq!(token.user_id.as_deref(), Some("42"));
    assert_eq!(token.scopes, vec!["account", "vehicles"]);
    assert_eq!(token.expires_at, Utc.with_ymd_and_hms(2026, 10, 22, 12, 0, 0).unwrap());
}

// --- the server, end to end, with the defuse sealer -----------------------------------

#[tokio::test]
async fn a_server_with_the_defuse_sealer_issues_league_shaped_refresh_tokens() {
    use rainier_crypt::{Argon2Hasher, Hasher};
    use rsa::RsaPrivateKey;

    let hasher = Arc::new(Argon2Hasher::insecure_for_tests());
    let store = Arc::new(MemoryStore::new().with_client(rainier_oauth_server::Client {
        id: "c".into(),
        name: "c".into(),
        secret_hash: Some(hasher.hash("s").unwrap()),
        redirect_uris: vec![],
        grant_types: vec!["client_credentials".into(), "refresh_token".into(), "password".into()],
        scopes: None,
        provider: None,
        owner_id: None,
        revoked: false,
    }));
    let key = RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048).unwrap();
    let sealer = Arc::new(DefuseSealer::new(vec![7u8; 32]));

    struct Owners;
    #[async_trait]
    impl rainier_oauth_server::ResourceOwners for Owners {
        async fn verify(
            &self,
            u: &str,
            p: &str,
            _: &rainier_oauth_server::Client,
        ) -> Result<Option<String>> {
            Ok((u == "ada" && p == "pw").then(|| "42".to_string()))
        }
    }

    let server =
        AuthorizationServer::new(store, sealer.clone(), AccessTokenSigner::from_key(key), hasher)
            .scopes(Scopes::new().with("account", "Account"))
            .password_grant(Arc::new(Owners));

    let request = Request::builder()
        .method(Method::POST)
        .form(&[
            ("grant_type", "password"),
            ("client_id", "c"),
            ("client_secret", "s"),
            ("username", "ada"),
            ("password", "pw"),
            ("scope", "account"),
        ])
        .build();
    let response = server.respond_to_token_request(&request).await;
    let rainier_http::Body::Bytes(body) = response.body() else { panic!("a buffered body") };
    let body: Json = serde_json::from_slice(body).unwrap();

    let refresh = body["refresh_token"].as_str().unwrap();
    assert!(refresh.starts_with("def50200"), "{refresh}");
    let payload: RefreshTokenPayload =
        serde_json::from_slice(&sealer.open(refresh).unwrap()).unwrap();
    assert_eq!(payload.client_id, "c");
    assert_eq!(payload.user_id.as_deref(), Some("42"));
    assert_eq!(payload.scopes, vec!["account"]);
}

// --- fixtures from a real PHP server ----------------------------------------------

/// The `oauth` section of the file `RAINIER_COMPAT_FIXTURES` names, written by
/// `tools/php-oauth-fixtures.php`. `None` — and the test passes, saying so —
/// when the variable is unset or the file has no such section.
fn fixtures() -> Option<Json> {
    let path = std::env::var("RAINIER_COMPAT_FIXTURES").ok()?;
    let text = std::fs::read_to_string(&path).expect("read the fixtures file");
    let all: Json = serde_json::from_str(&text).expect("the fixtures file is JSON");
    let section = all.get("oauth").cloned();
    if section.is_none() {
        eprintln!("{path} has no `oauth` section — run tools/php-oauth-fixtures.php");
    }
    section
}

#[test]
#[ignore = "needs RAINIER_COMPAT_FIXTURES from tools/php-oauth-fixtures.php"]
fn a_php_issued_access_token_verifies() {
    let Some(f) = fixtures() else { return };
    let verifier = AccessTokenVerifier::from_pem(f["public_key"].as_str().unwrap()).unwrap();
    let at = Utc.timestamp_opt(f["generated_at"].as_i64().unwrap(), 0).unwrap();
    let token = verifier.verify_at(f["access_token"].as_str().unwrap(), at).unwrap();
    assert_eq!(token.id, f["access_token_id"].as_str().unwrap());
    assert_eq!(token.client_id, f["client_id"].as_str().unwrap());
    assert_eq!(token.subject, f["user_id"].as_str().unwrap());
    assert_eq!(token.scopes, vec!["account", "vehicles"]);
}

#[test]
#[ignore = "needs RAINIER_COMPAT_FIXTURES from tools/php-oauth-fixtures.php"]
fn a_php_sealed_refresh_token_opens() {
    let Some(f) = fixtures() else { return };
    let sealer = DefuseSealer::from_app_key(f["app_key"].as_str().unwrap()).unwrap();
    let plain = sealer.open(f["refresh_token"].as_str().unwrap()).unwrap();
    let payload: RefreshTokenPayload = serde_json::from_slice(&plain).unwrap();
    assert_eq!(payload.refresh_token_id, f["refresh_token_id"].as_str().unwrap());
    assert_eq!(payload.access_token_id, f["access_token_id"].as_str().unwrap());
    assert_eq!(payload.client_id, f["client_id"].as_str().unwrap());
    assert_eq!(payload.user_id.as_deref(), f["user_id"].as_str());
}

#[test]
#[ignore = "needs RAINIER_COMPAT_FIXTURES from tools/php-oauth-fixtures.php"]
fn a_php_sealed_authorization_code_opens() {
    let Some(f) = fixtures() else { return };
    let sealer = DefuseSealer::from_app_key(f["app_key"].as_str().unwrap()).unwrap();
    let plain = sealer.open(f["auth_code"].as_str().unwrap()).unwrap();
    let payload: AuthCodePayload = serde_json::from_slice(&plain).unwrap();
    let expected: AuthCodePayload = serde_json::from_value(f["auth_code_payload"].clone()).unwrap();
    assert_eq!(payload, expected);
}
