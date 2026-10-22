//! Two-factor end to end: enrol, confirm, challenge, replay, recover, lock out.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rainier_http::{Method, StatusCode};
use rainier_mfa::{Secret, Totp};
use rainier_support::Result;
use serde_json::json;

use super::{app_with, App, User};
use crate::events::{RecoveryCodeUsed, TwoFactorChallenged, TwoFactorEnabled, TwoFactorFailed};
use crate::{Features, TwoFactorRecord, TwoFactorUsers};

#[derive(Default)]
struct Enrolments(Mutex<HashMap<u64, TwoFactorRecord>>);

#[async_trait::async_trait]
impl TwoFactorUsers<User> for Enrolments {
    async fn find_two_factor(&self, user: &User) -> Result<Option<TwoFactorRecord>> {
        Ok(self.0.lock().unwrap().get(&user.id).cloned())
    }
    async fn save_two_factor(&self, user: &User, record: Option<&TwoFactorRecord>) -> Result<()> {
        let mut rows = self.0.lock().unwrap();
        match record {
            Some(record) => rows.insert(user.id, record.clone()),
            None => rows.remove(&user.id),
        };
        Ok(())
    }
}

fn features() -> Features {
    Features { two_factor: true, ..Features::default() }
}

fn two_factor_app(features: Features) -> (App, Arc<Enrolments>) {
    let enrolments = Arc::new(Enrolments::default());
    let store = Arc::clone(&enrolments);
    let app = app_with(features, move |accounts| {
        accounts.two_factor_with(store as _, Totp::new().issuer("Acme"))
    });
    (app, enrolments)
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

fn code(secret: &str, offset: i64) -> String {
    Totp::new().code_at(&Secret::from_base32(secret).unwrap(), (now() as i64 + offset) as u64)
}

impl App {
    async fn confirm_password(&self) {
        let answer =
            self.post("/user/confirm-password", json!({ "password": "correct horse" })).await;
        assert_eq!(answer.status, StatusCode::NO_CONTENT, "{}", answer.body);
    }

    /// Log in, enrol and confirm; returns the secret and the recovery codes,
    /// and leaves the session logged out.
    async fn enrol(&self) -> (String, Vec<String>) {
        self.seed("ada@example.com", "correct horse");
        assert_eq!(self.login("ada@example.com", "correct horse").await.status, StatusCode::OK);
        self.confirm_password().await;

        let enabled = self.post("/user/two-factor-authentication", json!({})).await;
        assert_eq!(enabled.status, StatusCode::CREATED, "{}", enabled.body);
        let secret = enabled.body["secret"].as_str().unwrap().to_string();
        let codes: Vec<String> = enabled.body["recovery_codes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c.as_str().unwrap().to_string())
            .collect();

        let confirmed = self
            .post("/user/confirmed-two-factor-authentication", json!({ "code": code(&secret, 0) }))
            .await;
        assert_eq!(confirmed.status, StatusCode::OK, "{}", confirmed.body);

        self.post("/logout", json!({})).await;
        (secret, codes)
    }
}

#[tokio::test]
async fn enrolling_answers_a_secret_a_uri_and_recovery_codes_and_needs_a_confirmed_password() {
    let (app, enrolments) = two_factor_app(features());
    app.seed("ada@example.com", "correct horse");
    app.login("ada@example.com", "correct horse").await;

    let refused = app.post("/user/two-factor-authentication", json!({})).await;
    assert_eq!(refused.status.as_u16(), 423, "password not confirmed yet");

    app.confirm_password().await;
    let enabled = app.post("/user/two-factor-authentication", json!({})).await;
    assert_eq!(enabled.status, StatusCode::CREATED);
    let secret = enabled.body["secret"].as_str().unwrap();
    assert_eq!(secret.len(), 32);
    assert!(enabled.body["otpauth_url"]
        .as_str()
        .unwrap()
        .starts_with("otpauth://totp/Acme:ada%40example.com?secret="));
    assert_eq!(enabled.body["recovery_codes"].as_array().unwrap().len(), 8);

    let stored = enrolments.0.lock().unwrap().get(&1).cloned().unwrap();
    assert!(!stored.confirmed, "not until the first code");
    assert_eq!(stored.recovery_codes.len(), 8);
    assert!(
        !stored
            .recovery_codes
            .contains(&enabled.body["recovery_codes"][0].as_str().unwrap().to_string()),
        "codes are stored hashed"
    );
}

#[tokio::test]
async fn an_unconfirmed_enrolment_does_not_challenge() {
    let (app, _) = two_factor_app(features());
    app.seed("ada@example.com", "correct horse");
    app.login("ada@example.com", "correct horse").await;
    app.confirm_password().await;
    app.post("/user/two-factor-authentication", json!({})).await;
    app.post("/logout", json!({})).await;

    let login = app.login("ada@example.com", "correct horse").await;
    assert_eq!(login.body["authenticated"], true);
    assert!(app.is_logged_in().await);
}

#[tokio::test]
async fn a_wrong_first_code_does_not_confirm() {
    let (app, enrolments) = two_factor_app(features());
    app.seed("ada@example.com", "correct horse");
    app.login("ada@example.com", "correct horse").await;
    app.confirm_password().await;
    app.post("/user/two-factor-authentication", json!({})).await;

    let wrong =
        app.post("/user/confirmed-two-factor-authentication", json!({ "code": "000000" })).await;
    assert_eq!(wrong.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(!enrolments.0.lock().unwrap()[&1].confirmed);
}

#[tokio::test]
async fn a_right_password_waits_for_the_second_factor() {
    let (app, _) = two_factor_app(features());
    let (secret, _) = app.enrol().await;
    app.events.assert_dispatched_times::<TwoFactorEnabled<User>>(1);

    let login = app.login("ada@example.com", "correct horse").await;
    assert_eq!(login.status, StatusCode::OK);
    assert_eq!(login.body, json!({ "two_factor": true }));
    assert!(!app.is_logged_in().await, "a password alone reaches nothing");
    app.events.assert_dispatched_times::<TwoFactorChallenged>(1);

    let wrong = app.post("/two-factor-challenge", json!({ "code": "000000" })).await;
    assert_eq!(wrong.status, StatusCode::UNPROCESSABLE_ENTITY);
    app.events.assert_dispatched_times::<TwoFactorFailed<User>>(1);
    assert!(!app.is_logged_in().await);

    // The enrolment's own code was spent at confirmation, so the next step's.
    let right = app.post("/two-factor-challenge", json!({ "code": code(&secret, 30) })).await;
    assert_eq!(right.status, StatusCode::OK, "{}", right.body);
    assert_eq!(right.body["authenticated"], true);
    assert!(app.is_logged_in().await);
}

#[tokio::test]
async fn a_code_cannot_be_used_twice() {
    let (app, _) = two_factor_app(features());
    let (secret, _) = app.enrol().await;
    let next = code(&secret, 30);

    app.login("ada@example.com", "correct horse").await;
    assert_eq!(
        app.post("/two-factor-challenge", json!({ "code": next })).await.status,
        StatusCode::OK
    );
    app.post("/logout", json!({})).await;

    app.login("ada@example.com", "correct horse").await;
    let replayed = app.post("/two-factor-challenge", json!({ "code": next })).await;
    assert_eq!(replayed.status, StatusCode::UNPROCESSABLE_ENTITY);

    let confirmation_code = code(&secret, 0);
    let older = app.post("/two-factor-challenge", json!({ "code": confirmation_code })).await;
    assert_eq!(older.status, StatusCode::UNPROCESSABLE_ENTITY, "older than the last accepted");
}

#[tokio::test]
async fn a_recovery_code_works_once() {
    let (app, enrolments) = two_factor_app(features());
    let (_, codes) = app.enrol().await;

    app.login("ada@example.com", "correct horse").await;
    let used = app.post("/two-factor-challenge", json!({ "recovery_code": codes[3] })).await;
    assert_eq!(used.status, StatusCode::OK, "{}", used.body);
    assert_eq!(enrolments.0.lock().unwrap()[&1].recovery_codes.len(), 7);
    let event = app.events.dispatched::<RecoveryCodeUsed>();
    assert_eq!(event[0].remaining, 7);
    app.post("/logout", json!({})).await;

    app.login("ada@example.com", "correct horse").await;
    let again = app.post("/two-factor-challenge", json!({ "recovery_code": codes[3] })).await;
    assert_eq!(again.status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn too_many_wrong_codes_lock_the_pending_login_out() {
    let (app, _) = two_factor_app(Features { two_factor_attempts: 3, ..features() });
    let (secret, _) = app.enrol().await;
    app.login("ada@example.com", "correct horse").await;

    for _ in 0..3 {
        let wrong = app.post("/two-factor-challenge", json!({ "code": "000000" })).await;
        assert_eq!(wrong.status, StatusCode::UNPROCESSABLE_ENTITY);
    }
    let locked = app.post("/two-factor-challenge", json!({ "code": code(&secret, 30) })).await;
    assert_eq!(locked.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(locked.header("retry-after").is_some());
    assert!(!app.is_logged_in().await);
}

#[tokio::test]
async fn a_challenge_with_no_pending_login_is_refused() {
    let (app, _) = two_factor_app(features());
    let (secret, _) = app.enrol().await;

    let answer = app.post("/two-factor-challenge", json!({ "code": code(&secret, 30) })).await;
    assert_eq!(answer.status, StatusCode::UNAUTHORIZED);
    assert!(!app.is_logged_in().await);
}

#[tokio::test]
async fn turning_it_off_needs_a_confirmed_password_and_ends_the_challenge() {
    let (app, enrolments) = two_factor_app(features());
    let (secret, _) = app.enrol().await;
    app.login("ada@example.com", "correct horse").await;
    app.post("/two-factor-challenge", json!({ "code": code(&secret, 30) })).await;

    // A fresh session after logging in carries no confirmation.
    let refused = app.send(Method::DELETE, "/user/two-factor-authentication", None, true).await;
    assert_eq!(refused.status.as_u16(), 423);

    app.confirm_password().await;
    let off = app.send(Method::DELETE, "/user/two-factor-authentication", None, true).await;
    assert_eq!(off.status, StatusCode::OK);
    assert!(enrolments.0.lock().unwrap().is_empty());

    app.post("/logout", json!({})).await;
    let login = app.login("ada@example.com", "correct horse").await;
    assert_eq!(login.body["authenticated"], true);
}

#[tokio::test]
async fn new_recovery_codes_replace_the_old_ones() {
    let (app, _) = two_factor_app(features());
    let (secret, old) = app.enrol().await;
    app.login("ada@example.com", "correct horse").await;
    app.post("/two-factor-challenge", json!({ "code": code(&secret, 30) })).await;
    app.confirm_password().await;

    let fresh = app.post("/user/two-factor-recovery-codes", json!({})).await;
    assert_eq!(fresh.status, StatusCode::OK);
    let new: Vec<String> = fresh.body["recovery_codes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap().to_string())
        .collect();
    assert_eq!(new.len(), 8);
    app.post("/logout", json!({})).await;

    app.login("ada@example.com", "correct horse").await;
    let stale = app.post("/two-factor-challenge", json!({ "recovery_code": old[0] })).await;
    assert_eq!(stale.status, StatusCode::UNPROCESSABLE_ENTITY);
    let current = app.post("/two-factor-challenge", json!({ "recovery_code": new[0] })).await;
    assert_eq!(current.status, StatusCode::OK);
}

#[tokio::test]
async fn enabling_without_a_store_fails_at_boot() {
    let users = Arc::new(super::Users::default());
    let guard = Arc::new(rainier_auth::SessionGuard::new(
        "web",
        Arc::new(super::Provider(Arc::clone(&users))),
    ));
    let accounts = Arc::new(
        crate::Accounts::new(
            guard as _,
            users as _,
            Arc::new(rainier_auth::Argon2Hasher::insecure_for_tests()),
            Arc::new(rainier_cache::MemoryCache::new()),
        )
        .features(Features { two_factor: true, ..Features::login_only() }),
    );
    let mut router = rainier_routing::Router::new();
    let err = accounts.routes(&mut router).unwrap_err();
    assert!(err.message().contains("two_factor_with"), "{}", err.message());
}
