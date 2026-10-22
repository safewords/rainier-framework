//! Passkeys end to end, against a software authenticator.

use std::sync::{Arc, Mutex};

use rainier_auth::Authenticatable;
use rainier_http::{Method, StatusCode};
use rainier_mfa::webauthn::testing::SoftwareAuthenticator;
use rainier_mfa::{PasskeyCredential, RelyingParty};
use rainier_support::Result;
use serde_json::json;

use super::{app_with, App, User};
use crate::events::{Login, PasskeyRegistered, PasskeyUsed};
use crate::{Features, PasskeyRepository, StoredPasskey};

#[derive(Default)]
struct Passkeys(Mutex<Vec<(String, String, PasskeyCredential)>>);

#[async_trait::async_trait]
impl PasskeyRepository<User> for Passkeys {
    async fn passkeys_for(&self, user: &User) -> Result<Vec<PasskeyCredential>> {
        let id = user.auth_identifier();
        Ok(self
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|(u, _, _)| *u == id)
            .map(|(_, _, c)| c.clone())
            .collect())
    }
    async fn find_passkey(&self, credential_id: &[u8]) -> Result<Option<StoredPasskey>> {
        Ok(self.0.lock().unwrap().iter().find(|(_, _, c)| c.id == credential_id).map(
            |(user_id, _, c)| StoredPasskey { user_id: user_id.clone(), credential: c.clone() },
        ))
    }
    async fn store_passkey(
        &self,
        user: &User,
        name: &str,
        credential: &PasskeyCredential,
    ) -> Result<()> {
        self.0.lock().unwrap().push((user.auth_identifier(), name.to_string(), credential.clone()));
        Ok(())
    }
    async fn update_passkey(&self, credential: &PasskeyCredential) -> Result<()> {
        for row in self.0.lock().unwrap().iter_mut().filter(|(_, _, c)| c.id == credential.id) {
            row.2 = credential.clone();
        }
        Ok(())
    }
    async fn delete_passkey(&self, user: &User, credential_id: &[u8]) -> Result<bool> {
        let mut rows = self.0.lock().unwrap();
        let before = rows.len();
        let id = user.auth_identifier();
        rows.retain(|(u, _, c)| !(*u == id && c.id == credential_id));
        Ok(rows.len() != before)
    }
}

fn passkey_app() -> (App, Arc<Passkeys>) {
    let store = Arc::new(Passkeys::default());
    let shared = Arc::clone(&store);
    let app = app_with(Features { passkeys: true, ..Features::default() }, move |accounts| {
        accounts.passkeys_with(shared as _, RelyingParty::new("app.test", "App"))
    });
    (app, store)
}

fn authenticator() -> SoftwareAuthenticator {
    SoftwareAuthenticator::es256("app.test", "https://app.test")
}

impl App {
    /// Sign in with a password, confirm it, and register `authenticator`'s
    /// passkey; leaves the session logged out.
    async fn register_passkey(&self, authenticator: &mut SoftwareAuthenticator) {
        self.seed("ada@example.com", "correct horse");
        self.login("ada@example.com", "correct horse").await;
        self.post("/user/confirm-password", json!({ "password": "correct horse" })).await;

        let options = self.post("/user/passkeys/options", json!({})).await;
        assert_eq!(options.status, StatusCode::OK, "{}", options.body);
        let credential = authenticator.register(&options.body);
        let stored = self
            .post("/user/passkeys", json!({ "credential": credential, "name": "Laptop" }))
            .await;
        assert_eq!(stored.status, StatusCode::CREATED, "{}", stored.body);

        self.post("/logout", json!({})).await;
    }

    async fn passkey_login(&self, authenticator: &mut SoftwareAuthenticator) -> super::Answer {
        let options = self.post("/passkeys/login/options", json!({})).await;
        assert_eq!(options.status, StatusCode::OK);
        let credential = authenticator.authenticate(&options.body);
        self.post("/passkeys/login", json!({ "credential": credential })).await
    }
}

#[tokio::test]
async fn a_registered_passkey_signs_in() {
    let (app, store) = passkey_app();
    let mut authenticator = authenticator();
    app.register_passkey(&mut authenticator).await;

    let rows = store.0.lock().unwrap().clone();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1, "Laptop");
    assert_eq!(rows[0].2.user_handle, b"1", "the user id is the default handle");
    app.events.assert_dispatched_times::<PasskeyRegistered<User>>(1);

    assert!(!app.is_logged_in().await);
    let login = app.passkey_login(&mut authenticator).await;
    assert_eq!(login.status, StatusCode::OK, "{}", login.body);
    assert!(app.is_logged_in().await);
    app.events.assert_dispatched_times::<PasskeyUsed>(1);
    app.events.assert_dispatched_times::<Login<User>>(2);
}

#[tokio::test]
async fn registering_needs_a_confirmed_password() {
    let (app, _) = passkey_app();
    app.seed("ada@example.com", "correct horse");
    app.login("ada@example.com", "correct horse").await;

    let refused = app.post("/user/passkeys/options", json!({})).await;
    assert_eq!(refused.status.as_u16(), 423);
}

#[tokio::test]
async fn a_sign_in_answer_cannot_be_replayed() {
    let (app, _) = passkey_app();
    let mut authenticator = authenticator();
    app.register_passkey(&mut authenticator).await;

    let options = app.post("/passkeys/login/options", json!({})).await;
    let credential = authenticator.authenticate(&options.body);
    let first = app.post("/passkeys/login", json!({ "credential": credential.clone() })).await;
    assert_eq!(first.status, StatusCode::OK);
    app.post("/logout", json!({})).await;

    let replayed = app.post("/passkeys/login", json!({ "credential": credential })).await;
    assert_eq!(
        replayed.status,
        StatusCode::BAD_REQUEST,
        "the challenge was spent: {}",
        replayed.body
    );

    // Even against a fresh challenge, the old answer signs the old one.
    app.post("/passkeys/login/options", json!({})).await;
    let stale = app
        .post(
            "/passkeys/login",
            json!({ "credential": authenticator_answer_for_old(&mut authenticator) }),
        )
        .await;
    assert!(stale.status.is_client_error());
    assert!(!app.is_logged_in().await);
}

/// An answer to a challenge the server never issued.
fn authenticator_answer_for_old(authenticator: &mut SoftwareAuthenticator) -> serde_json::Value {
    authenticator
        .authenticate(&json!({ "challenge": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" }))
}

#[tokio::test]
async fn a_passkey_from_a_phishing_origin_is_refused() {
    let (app, _) = passkey_app();
    let mut authenticator = authenticator();
    app.register_passkey(&mut authenticator).await;

    authenticator.set_origin("https://app-test.example");
    let login = app.passkey_login(&mut authenticator).await;
    assert_eq!(login.status, StatusCode::UNAUTHORIZED);
    assert!(!app.is_logged_in().await);
}

#[tokio::test]
async fn an_unknown_passkey_is_refused_without_saying_why() {
    let (app, _) = passkey_app();
    let mut stranger = authenticator();
    let login = app.passkey_login(&mut stranger).await;
    assert_eq!(login.status, StatusCode::UNAUTHORIZED);
    assert_eq!(login.body["message"], crate::passkeys::PASSKEY_REFUSED);
}

#[tokio::test]
async fn a_deleted_passkey_no_longer_signs_in() {
    let (app, store) = passkey_app();
    let mut authenticator = authenticator();
    app.register_passkey(&mut authenticator).await;
    let id = store.0.lock().unwrap()[0].2.id_base64url();

    app.passkey_login(&mut authenticator).await;
    app.post("/user/confirm-password", json!({ "password": "correct horse" })).await;
    let deleted = app.send(Method::DELETE, &format!("/user/passkeys/{id}"), None, true).await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    assert!(store.0.lock().unwrap().is_empty());
    app.post("/logout", json!({})).await;

    let login = app.passkey_login(&mut authenticator).await;
    assert_eq!(login.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn the_same_authenticator_cannot_register_twice() {
    let (app, _) = passkey_app();
    let mut authenticator = authenticator();
    app.register_passkey(&mut authenticator).await;

    app.login("ada@example.com", "correct horse").await;
    app.post("/user/confirm-password", json!({ "password": "correct horse" })).await;
    let options = app.post("/user/passkeys/options", json!({})).await;
    assert_eq!(options.body["excludeCredentials"].as_array().unwrap().len(), 1);
    let again = app
        .post("/user/passkeys", json!({ "credential": authenticator.register(&options.body) }))
        .await;
    assert_eq!(again.status, StatusCode::CONFLICT);
}
