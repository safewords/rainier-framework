//! The grants end to end, over the memory store.

use std::sync::Arc;

use async_trait::async_trait;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine as _;
use rainier_auth::{Authenticatable, Credentials, Guard, GuardExt, UserProvider};
use rainier_crypt::{AeadEncrypter, Argon2Hasher, Hasher, Key, KeyRing};
use rainier_http::{Method, Request, Response};
use rainier_session::Session;
use rainier_support::Result;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::jwt::tests::key;
use crate::*;

// --- fixtures -------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
struct User {
    id: String,
    email: String,
}

impl Authenticatable for User {
    fn auth_identifier(&self) -> String {
        self.id.clone()
    }
    fn auth_password_hash(&self) -> Option<&str> {
        None
    }
}

struct Users;

#[async_trait]
impl UserProvider<User> for Users {
    async fn retrieve_by_id(&self, id: &str) -> Result<Option<User>> {
        Ok((id == "7").then(|| User { id: "7".into(), email: "ada@example.com".into() }))
    }
    async fn retrieve_by_credentials(&self, credentials: &Credentials) -> Result<Option<User>> {
        Ok((credentials.get("email") == Some("ada@example.com"))
            .then(|| User { id: "7".into(), email: "ada@example.com".into() }))
    }
    async fn validate_credentials(&self, _: &User, credentials: &Credentials) -> Result<bool> {
        Ok(credentials.password_value() == Some("correct horse"))
    }
}

const SECRET: &str = "s3cret";

fn hasher() -> Arc<dyn Hasher> {
    Arc::new(Argon2Hasher::insecure_for_tests())
}

fn client(id: &str, confidential: bool, grants: &[&str]) -> Client {
    Client {
        id: id.into(),
        name: format!("{id} app"),
        secret_hash: confidential.then(|| hasher().hash(SECRET).unwrap()),
        redirect_uris: vec!["https://app.test/callback".into()],
        grant_types: grants.iter().map(|g| g.to_string()).collect(),
        scopes: None,
        provider: None,
        owner_id: None,
        revoked: false,
    }
}

struct Fixture {
    server: AuthorizationServer,
    store: Arc<MemoryStore>,
}

fn fixture() -> Fixture {
    let all = [
        grant::AUTHORIZATION_CODE,
        grant::REFRESH_TOKEN,
        grant::PASSWORD,
        grant::CLIENT_CREDENTIALS,
    ];
    let store = Arc::new(
        MemoryStore::new()
            .with_client(client("web", true, &all))
            .with_client(client("mobile", false, &all))
            .with_client(client("code-only", true, &[grant::AUTHORIZATION_CODE]))
            .with_client(client("personal", true, &[grant::PERSONAL_ACCESS]))
            .with_personal_access_client("personal"),
    );
    let sealer =
        Arc::new(EncrypterSealer::new(Arc::new(AeadEncrypter::new(KeyRing::new(Key::generate())))));
    let server = AuthorizationServer::new(
        store.clone(),
        sealer,
        AccessTokenSigner::from_key(key()),
        hasher(),
    )
    .scopes(Scopes::new().with("account", "Your account").with("vehicles", "Your vehicles"))
    .password_grant(Arc::new(UserProviderOwners::new(
        Arc::new(Users) as Arc<dyn UserProvider<User>>
    )));
    Fixture { server, store }
}

fn token_request(pairs: &[(&str, &str)]) -> Request {
    Request::builder().method(Method::POST).uri("/oauth/token").form(pairs).build()
}

async fn post(fixture: &Fixture, pairs: &[(&str, &str)]) -> (u16, Value, Response) {
    let response = fixture.server.respond_to_token_request(&token_request(pairs)).await;
    let body = serde_json::from_slice(&body_bytes(&response)).unwrap_or(Value::Null);
    (response.status().as_u16(), body, response)
}

fn body_bytes(response: &Response) -> Vec<u8> {
    match response.body() {
        rainier_http::Body::Bytes(bytes) => bytes.to_vec(),
        _ => Vec::new(),
    }
}

fn password_pairs<'a>(client: &'a str, password: &'a str) -> Vec<(&'a str, &'a str)> {
    vec![
        ("grant_type", "password"),
        ("client_id", client),
        ("client_secret", SECRET),
        ("username", "ada@example.com"),
        ("password", password),
        ("scope", "account"),
    ]
}

// --- client authentication ------------------------------------------------------

#[tokio::test]
async fn an_unknown_grant_is_unsupported() {
    let (status, body, _) = post(&fixture(), &[("grant_type", "implicit")]).await;
    assert_eq!(status, 400);
    assert_eq!(body["error"], "unsupported_grant_type");
}

#[tokio::test]
async fn a_wrong_secret_is_invalid_client_and_basic_auth_earns_a_challenge() {
    let f = fixture();
    let (status, body, _) = post(
        &f,
        &[("grant_type", "client_credentials"), ("client_id", "web"), ("client_secret", "nope")],
    )
    .await;
    assert_eq!(status, 401);
    assert_eq!(body["error"], "invalid_client");

    let request = Request::builder()
        .method(Method::POST)
        .uri("/oauth/token")
        .header("authorization", &format!("Basic {}", STANDARD.encode("web:nope")))
        .form(&[("grant_type", "client_credentials")])
        .build();
    let response = f.server.respond_to_token_request(&request).await;
    assert_eq!(response.header("www-authenticate"), Some("Basic realm=\"OAuth\""));
}

#[tokio::test]
async fn a_missing_secret_names_the_parameter() {
    let (status, body, _) =
        post(&fixture(), &[("grant_type", "client_credentials"), ("client_id", "web")]).await;
    assert_eq!(status, 400);
    assert_eq!(body["hint"], "Check the `client_secret` parameter");
}

#[tokio::test]
async fn a_grant_the_client_is_not_allowed_is_unauthorized_client() {
    let (_, body, _) = post(
        &fixture(),
        &[
            ("grant_type", "client_credentials"),
            ("client_id", "code-only"),
            ("client_secret", SECRET),
        ],
    )
    .await;
    assert_eq!(body["error"], "unauthorized_client");
}

#[tokio::test]
async fn a_revoked_client_is_unknown() {
    let f = fixture();
    let mut revoked = client("web", true, &[grant::CLIENT_CREDENTIALS]);
    revoked.revoked = true;
    f.store.insert_client(revoked);
    let (status, _, _) = post(
        &f,
        &[("grant_type", "client_credentials"), ("client_id", "web"), ("client_secret", SECRET)],
    )
    .await;
    assert_eq!(status, 401);
}

// --- password -------------------------------------------------------------------

#[tokio::test]
async fn the_password_grant_issues_an_access_and_a_refresh_token() {
    let f = fixture();
    let (status, body, response) = post(&f, &password_pairs("web", "correct horse")).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["token_type"], "Bearer");
    assert!(body["expires_in"].as_i64().unwrap() > 3500);
    assert!(body["refresh_token"].is_string());
    assert_eq!(response.header("cache-control"), Some("no-store"));

    let verified = f.server.verifier().verify(body["access_token"].as_str().unwrap()).unwrap();
    assert_eq!(verified.subject, "7");
    assert_eq!(verified.scopes, vec!["account"]);
    assert_eq!(f.store.access_tokens().len(), 1);
    assert_eq!(f.store.refresh_tokens().len(), 1);
}

#[tokio::test]
async fn wrong_credentials_are_invalid_grant() {
    let (status, body, _) = post(&fixture(), &password_pairs("web", "wrong")).await;
    assert_eq!(status, 400);
    assert_eq!(body["error"], "invalid_grant");
    assert_eq!(body["error_description"], "The user credentials were incorrect.");
}

#[tokio::test]
async fn an_unknown_scope_is_invalid_scope() {
    let mut pairs = password_pairs("web", "correct horse");
    pairs.retain(|(k, _)| *k != "scope");
    pairs.push(("scope", "account admin"));
    let (_, body, _) = post(&fixture(), &pairs).await;
    assert_eq!(body["error"], "invalid_scope");
    assert_eq!(body["hint"], "Check the `admin` scope");
}

// --- client credentials ---------------------------------------------------------

#[tokio::test]
async fn client_credentials_issue_no_refresh_token_and_name_the_client() {
    let f = fixture();
    let (status, body, _) = post(
        &f,
        &[
            ("grant_type", "client_credentials"),
            ("client_id", "web"),
            ("client_secret", SECRET),
            ("scope", "*"),
        ],
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.get("refresh_token").is_none());
    let verified = f.server.verifier().verify(body["access_token"].as_str().unwrap()).unwrap();
    assert_eq!(verified.subject, "web");
    assert_eq!(verified.scopes, vec!["*"], "`*` survives a grant with no consent screen");
}

#[tokio::test]
async fn a_public_client_cannot_use_client_credentials() {
    let (status, body, _) =
        post(&fixture(), &[("grant_type", "client_credentials"), ("client_id", "mobile")]).await;
    assert_eq!(status, 401);
    assert_eq!(body["error"], "invalid_client");
}

// --- refresh --------------------------------------------------------------------

#[tokio::test]
async fn refreshing_rotates_and_the_old_token_cannot_be_used_twice() {
    let f = fixture();
    let (_, first, _) = post(&f, &password_pairs("web", "correct horse")).await;
    let old_refresh = first["refresh_token"].as_str().unwrap().to_string();
    let old_access = f.server.verifier().verify(first["access_token"].as_str().unwrap()).unwrap();

    let refresh = |token: &str| {
        vec![
            ("grant_type".to_string(), "refresh_token".to_string()),
            ("client_id".into(), "web".into()),
            ("client_secret".into(), SECRET.into()),
            ("refresh_token".into(), token.to_string()),
        ]
    };
    let pairs = refresh(&old_refresh);
    let pairs: Vec<(&str, &str)> = pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();

    let (status, second, _) = post(&f, &pairs).await;
    assert_eq!(status, 200, "{second}");
    assert_ne!(second["refresh_token"], first["refresh_token"]);

    let old_row = f.store.find_access_token(&old_access.id).await.unwrap().unwrap();
    assert!(old_row.revoked, "the old access token is revoked");

    let (status, again, _) = post(&f, &pairs).await;
    assert_eq!(status, 400);
    assert_eq!(again["error"], "invalid_grant");
    assert_eq!(again["hint"], "Token has been revoked");
}

#[tokio::test]
async fn a_refresh_token_from_another_client_or_tampered_is_refused() {
    let f = fixture();
    let (_, first, _) = post(&f, &password_pairs("web", "correct horse")).await;
    let token = first["refresh_token"].as_str().unwrap();

    let (_, body, _) = post(
        &f,
        &[("grant_type", "refresh_token"), ("client_id", "mobile"), ("refresh_token", token)],
    )
    .await;
    assert_eq!(body["hint"], "Token is not linked to client");

    let tampered = format!("{token}x");
    let (_, body, _) = post(
        &f,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", "web"),
            ("client_secret", SECRET),
            ("refresh_token", &tampered),
        ],
    )
    .await;
    assert_eq!(body["hint"], "Cannot decrypt the refresh token");
}

#[tokio::test]
async fn a_refresh_cannot_widen_the_scopes() {
    let f = fixture();
    let (_, first, _) = post(&f, &password_pairs("web", "correct horse")).await;
    let (_, body, _) = post(
        &f,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", "web"),
            ("client_secret", SECRET),
            ("refresh_token", first["refresh_token"].as_str().unwrap()),
            ("scope", "account vehicles"),
        ],
    )
    .await;
    assert_eq!(body["error"], "invalid_scope");
    assert_eq!(body["hint"], "Check the `vehicles` scope");
}

// --- authorization code ---------------------------------------------------------

fn verifier_and_challenge() -> (String, String) {
    let verifier = "v".repeat(64);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

fn authorize_request(query: &str) -> Request {
    Request::builder()
        .uri(format!("/oauth/authorize?{query}"))
        .build()
        .with_extension(Session::new())
}

/// Run an approved authorization and return the code from the redirect.
async fn code_for(f: &Fixture, client: &str, challenge: Option<&str>) -> String {
    let mut query = format!("response_type=code&client_id={client}&scope=account&state=xyz");
    if let Some(challenge) = challenge {
        query.push_str(&format!("&code_challenge={challenge}&code_challenge_method=S256"));
    }
    let authorization =
        f.server.validate_authorization_request(&authorize_request(&query)).await.unwrap();
    let response = f.server.complete_authorization_request(&authorization, "7", true).await;
    assert_eq!(response.status().as_u16(), 302);
    let location = response.header("location").unwrap().to_string();
    assert!(location.starts_with("https://app.test/callback?code="), "{location}");
    assert!(location.ends_with("&state=xyz"));
    let code = location.split("code=").nth(1).unwrap().split('&').next().unwrap();
    percent_decode(code)
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                out.push(u8::from_str_radix(&value[i + 1..i + 3], 16).unwrap());
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).unwrap()
}

#[tokio::test]
async fn the_code_flow_with_pkce_issues_tokens_once() {
    let f = fixture();
    let (verifier, challenge) = verifier_and_challenge();
    let code = code_for(&f, "mobile", Some(&challenge)).await;

    let pairs = [
        ("grant_type", "authorization_code"),
        ("client_id", "mobile"),
        ("code", code.as_str()),
        ("code_verifier", verifier.as_str()),
    ];
    let (status, body, _) = post(&f, &pairs).await;
    assert_eq!(status, 200, "{body}");
    let verified = f.server.verifier().verify(body["access_token"].as_str().unwrap()).unwrap();
    assert_eq!(verified.subject, "7");
    assert_eq!(verified.scopes, vec!["account"]);

    let (status, again, _) = post(&f, &pairs).await;
    assert_eq!(status, 400);
    assert_eq!(again["hint"], "Authorization code has been revoked");
}

#[tokio::test]
async fn a_wrong_verifier_or_a_missing_one_is_refused() {
    let f = fixture();
    let (_, challenge) = verifier_and_challenge();
    let code = code_for(&f, "mobile", Some(&challenge)).await;

    let wrong = "w".repeat(64);
    let (_, body, _) = post(
        &f,
        &[
            ("grant_type", "authorization_code"),
            ("client_id", "mobile"),
            ("code", &code),
            ("code_verifier", &wrong),
        ],
    )
    .await;
    assert_eq!(body["hint"], "Failed to verify `code_verifier`.");

    let (_, body, _) =
        post(&f, &[("grant_type", "authorization_code"), ("client_id", "mobile"), ("code", &code)])
            .await;
    assert_eq!(body["hint"], "Check the `code_verifier` parameter");
}

#[tokio::test]
async fn a_public_client_must_use_pkce() {
    let f = fixture();
    let err = f
        .server
        .validate_authorization_request(&authorize_request(
            "response_type=code&client_id=mobile&scope=account&state=s",
        ))
        .await
        .unwrap_err();
    assert_eq!(err.hint.as_deref(), Some("Code challenge must be provided for public clients"));
    assert!(err.redirect_uri.unwrap().starts_with("https://app.test/callback?state=s"));
}

#[tokio::test]
async fn an_unregistered_redirect_uri_is_never_redirected_to() {
    let f = fixture();
    let err = f
        .server
        .validate_authorization_request(&authorize_request(
            "response_type=code&client_id=web&redirect_uri=https%3A%2F%2Fevil.test%2F",
        ))
        .await
        .unwrap_err();
    assert_eq!(err.error, "invalid_client");
    assert!(err.redirect_uri.is_none(), "an open redirect would hand the code to evil.test");
}

#[tokio::test]
async fn a_code_issued_to_one_client_cannot_be_redeemed_by_another() {
    let f = fixture();
    let code = code_for(&f, "web", None).await;
    let (_, body, _) = post(
        &f,
        &[
            ("grant_type", "authorization_code"),
            ("client_id", "code-only"),
            ("client_secret", SECRET),
            ("code", &code),
        ],
    )
    .await;
    assert_eq!(body["hint"], "Authorization code was not issued to this client");
}

#[tokio::test]
async fn a_denied_authorization_goes_back_with_access_denied_and_the_state() {
    let f = fixture();
    let authorization = f
        .server
        .validate_authorization_request(&authorize_request(
            "response_type=code&client_id=web&scope=account&state=abc",
        ))
        .await
        .unwrap();
    let response = f.server.complete_authorization_request(&authorization, "7", false).await;
    let location = response.header("location").unwrap();
    assert!(location.starts_with("https://app.test/callback?state=abc&error=access_denied"));
}

#[tokio::test]
async fn the_consent_round_trip_needs_the_token_it_was_stashed_with() {
    let f = fixture();
    let request = authorize_request("response_type=code&client_id=web&scope=account");
    let authorization = f.server.validate_authorization_request(&request).await.unwrap();
    let token = AuthorizationSession::stash(&request, &authorization).unwrap();

    assert!(AuthorizationSession::take(&request, "forged").is_err());
    // The forged attempt spent it, too: the stash is single-use.
    assert!(AuthorizationSession::take(&request, &token).is_err());

    let token = AuthorizationSession::stash(&request, &authorization).unwrap();
    assert_eq!(AuthorizationSession::take(&request, &token).unwrap(), authorization);
}

#[tokio::test]
async fn a_returning_user_with_the_scopes_already_granted_counts_as_approved() {
    let f = fixture();
    let request = authorize_request("response_type=code&client_id=web&scope=account");
    let authorization = f.server.validate_authorization_request(&request).await.unwrap();
    assert!(!f.server.previously_approved(&authorization, "7").await.unwrap());

    post(&f, &password_pairs("web", "correct horse")).await;
    assert!(f.server.previously_approved(&authorization, "7").await.unwrap());
}

// --- personal access tokens, the guard, housekeeping ----------------------------

#[tokio::test]
async fn a_personal_access_token_is_named_long_lived_and_has_no_refresh() {
    let f = fixture();
    let issued =
        f.server.issue_personal_access_token("7", "CLI", &["account"], None).await.unwrap();
    assert_eq!(issued.token.name.as_deref(), Some("CLI"));
    assert!(issued.expires_in > 360 * 24 * 60 * 60);
    assert!(f.store.refresh_tokens().is_empty());
    assert!(f.server.verifier().verify(&issued.access_token).is_ok());
}

fn bearer(token: &str) -> Request {
    Request::builder().header("authorization", &format!("Bearer {token}")).build()
}

#[tokio::test]
async fn the_guard_resolves_the_user_and_the_scopes_and_honours_revocation() {
    let f = fixture();
    let issued =
        f.server.issue_personal_access_token("7", "CLI", &["vehicles"], None).await.unwrap();
    let guard = OAuthGuard::new(
        "api",
        f.server.verifier(),
        f.store.clone(),
        Arc::new(Users) as Arc<dyn UserProvider<User>>,
    );

    let request = bearer(&issued.access_token);
    assert_eq!(guard.user(&request).await.unwrap().unwrap().email, "ada@example.com");
    let abilities = guard.abilities(&request).await.unwrap();
    assert!(abilities.can("vehicles:read"));
    assert!(!abilities.can("account"));

    f.server.revoke(&issued.token.id).await.unwrap();
    assert!(guard.guest(&request).await.unwrap(), "a revoked token is nobody");
    assert!(guard.user(&bearer("not.a.jwt")).await.unwrap().is_none());
}

#[tokio::test]
async fn a_client_token_authenticates_a_client_and_no_user() {
    let f = fixture();
    let (_, body, _) = post(
        &f,
        &[("grant_type", "client_credentials"), ("client_id", "web"), ("client_secret", SECRET)],
    )
    .await;
    let guard = OAuthGuard::new(
        "api",
        f.server.verifier(),
        f.store.clone(),
        Arc::new(Users) as Arc<dyn UserProvider<User>>,
    );
    let request = bearer(body["access_token"].as_str().unwrap());
    let token = guard.token(&request).await.unwrap().unwrap();
    assert_eq!(token.token.client_id, "web");
    assert_eq!(token.user_id, None);
    assert!(guard.user(&request).await.unwrap().is_none());
}

#[tokio::test]
async fn revocation_answers_200_whatever_happens_and_revokes_what_it_should() {
    let f = fixture();
    let (_, body, _) = post(&f, &password_pairs("web", "correct horse")).await;
    let refresh = body["refresh_token"].as_str().unwrap();

    let request = Request::builder()
        .method(Method::POST)
        .form(&[("client_id", "web"), ("client_secret", SECRET), ("token", refresh)])
        .build();
    assert_eq!(f.server.respond_to_revocation_request(&request).await.status().as_u16(), 200);
    assert!(f.store.refresh_tokens().iter().all(|t| t.revoked));
    assert!(f.store.access_tokens().iter().all(|t| t.revoked));

    let junk = Request::builder()
        .method(Method::POST)
        .form(&[("client_id", "web"), ("client_secret", SECRET), ("token", "junk")])
        .build();
    assert_eq!(f.server.respond_to_revocation_request(&junk).await.status().as_u16(), 200);
}

#[tokio::test]
async fn purging_removes_revoked_rows() {
    let f = fixture();
    let (_, body, _) = post(&f, &password_pairs("web", "correct horse")).await;
    let id = f.server.verifier().verify(body["access_token"].as_str().unwrap()).unwrap().id;
    f.server.revoke(&id).await.unwrap();
    let purged = f.server.purge(std::time::Duration::from_secs(7 * 24 * 3600)).await.unwrap();
    assert_eq!(purged.access_tokens, 1);
    assert_eq!(purged.refresh_tokens, 1);
    assert!(f.store.access_tokens().is_empty());
}
