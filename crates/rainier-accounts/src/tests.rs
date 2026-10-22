//! The flows end to end: a real router, a real session middleware, an
//! in-memory user table, and a cookie carried between requests the way a
//! browser would.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use rainier_auth::{
    Argon2Hasher, Authenticatable, Credentials, Hasher, SessionGuard, UserProvider,
};
use rainier_cache::MemoryCache;
use rainier_container::Container;
use rainier_crypt::{Key, KeyRing, UrlSigner};
use rainier_events::Dispatcher;
use rainier_http::{Method, Request, Response, StatusCode};
use rainier_mail::Mailer;
use rainier_routing::{CompiledRouter, GroupAttributes, Router};
use rainier_session::{MemorySessionStore, StartSession};
use rainier_support::{Error, Result};
use rainier_view::MemoryEngine;
use serde_json::{json, Value};

use crate::events::{Failed, Lockout, Login, Logout, PasswordReset, Registered, Verified};
use crate::*;

// --- the application side ---------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
struct User {
    id: u64,
    email: String,
    name: String,
    password: Option<String>,
    verified: bool,
}

impl Authenticatable for User {
    fn auth_identifier(&self) -> String {
        self.id.to_string()
    }
    fn auth_password_hash(&self) -> Option<&str> {
        self.password.as_deref()
    }
}

impl AccountUser for User {
    fn email(&self) -> &str {
        &self.email
    }
    fn has_verified_email(&self) -> bool {
        self.verified
    }
}

#[derive(Default)]
struct Users {
    rows: Mutex<Vec<User>>,
}

impl Users {
    fn get(&self, email: &str) -> Option<User> {
        self.rows.lock().unwrap().iter().find(|u| u.email == email).cloned()
    }

    fn insert(&self, email: &str, name: &str, password_hash: Option<String>) -> User {
        let mut rows = self.rows.lock().unwrap();
        let user = User {
            id: rows.len() as u64 + 1,
            email: email.into(),
            name: name.into(),
            password: password_hash,
            verified: false,
        };
        rows.push(user.clone());
        user
    }
}

#[async_trait::async_trait]
impl AccountUsers<User> for Users {
    async fn find_by_id(&self, id: &str) -> Result<Option<User>> {
        Ok(self.rows.lock().unwrap().iter().find(|u| u.id.to_string() == id).cloned())
    }
    async fn find_by_email(&self, email: &str) -> Result<Option<User>> {
        Ok(self.get(email))
    }
    async fn set_password_hash(&self, user: &User, hash: &str) -> Result<()> {
        for row in self.rows.lock().unwrap().iter_mut().filter(|u| u.id == user.id) {
            row.password = Some(hash.to_string());
        }
        Ok(())
    }
    async fn mark_email_verified(&self, user: &User) -> Result<()> {
        for row in self.rows.lock().unwrap().iter_mut().filter(|u| u.id == user.id) {
            row.verified = true;
        }
        Ok(())
    }
}

/// What the session guard reads the logged-in user back through.
struct Provider(Arc<Users>);

#[async_trait::async_trait]
impl UserProvider<User> for Provider {
    async fn retrieve_by_id(&self, id: &str) -> Result<Option<User>> {
        self.0.find_by_id(id).await
    }
    async fn retrieve_by_credentials(&self, _: &Credentials) -> Result<Option<User>> {
        Ok(None)
    }
    async fn validate_credentials(&self, _: &User, _: &Credentials) -> Result<bool> {
        Ok(false)
    }
}

struct CreateUser(Arc<Users>);

#[async_trait::async_trait]
impl CreatesNewUsers<User> for CreateUser {
    async fn create(&self, new_user: NewUser) -> Result<User> {
        let name = new_user.input["name"].as_str().unwrap_or_default().to_string();
        if name.is_empty() {
            return Err(Error::validation(json!({ "name": ["The name field is required."] })));
        }
        Ok(self.0.insert(&new_user.email, &name, Some(new_user.password_hash)))
    }
}

/// Records links instead of sending them, so a test can click one.
#[derive(Default)]
struct Outbox {
    resets: Mutex<Vec<(String, String)>>,
    verifications: Mutex<Vec<(String, String)>>,
}

#[async_trait::async_trait]
impl SendsPasswordResetLinks<User> for Outbox {
    async fn send_reset_link(&self, user: &User, url: &str) -> Result<()> {
        self.resets.lock().unwrap().push((user.email.clone(), url.to_string()));
        Ok(())
    }
}

#[async_trait::async_trait]
impl SendsVerificationLinks<User> for Outbox {
    async fn send_verification_link(&self, user: &User, url: &str) -> Result<()> {
        self.verifications.lock().unwrap().push((user.email.clone(), url.to_string()));
        Ok(())
    }
}

// --- the harness --------------------------------------------------------------

struct App {
    router: CompiledRouter,
    users: Arc<Users>,
    events: Arc<Dispatcher>,
    outbox: Arc<Outbox>,
    hasher: Arc<dyn Hasher>,
    cookie: Mutex<Option<String>>,
}

struct Answer {
    status: StatusCode,
    response: Response,
    body: Value,
}

impl Answer {
    fn header(&self, name: &str) -> Option<&str> {
        self.response.header(name)
    }
}

fn app(features: Features) -> App {
    app_with(features, |accounts| accounts)
}

fn app_with(features: Features, configure: impl FnOnce(Accounts<User>) -> Accounts<User>) -> App {
    let users = Arc::new(Users::default());
    let hasher: Arc<dyn Hasher> = Arc::new(Argon2Hasher::insecure_for_tests());
    let events = Arc::new(Dispatcher::fake());
    let outbox = Arc::new(Outbox::default());
    let guard = Arc::new(SessionGuard::new("web", Arc::new(Provider(Arc::clone(&users)))));
    let signer = Arc::new(UrlSigner::new(KeyRing::new(Key::generate())));

    let accounts = Accounts::new(
        guard.clone(),
        Arc::clone(&users) as _,
        Arc::clone(&hasher),
        Arc::new(MemoryCache::new()),
    )
    .features(features)
    .creates_users_with(Arc::new(CreateUser(Arc::clone(&users))))
    .sends_reset_links_with(Arc::clone(&outbox) as _)
    .sends_verification_links_with(Arc::clone(&outbox) as _)
    .signs_urls_with(signer)
    .app_url("https://app.test")
    .home("/dashboard")
    .deliver_links_inline(true)
    .events(Arc::clone(&events));
    let accounts = Arc::new(configure(accounts));

    let store = Arc::new(MemorySessionStore::default());
    let mut router = Router::new();
    router.group(GroupAttributes::new().middleware(StartSession::new(store)), |router| {
        accounts.routes(router).unwrap();

        router
            .get("/verified-only", |_: rainier_routing::Req| async { Response::text("in") })
            .middleware(EnsureEmailIsVerified::<User>::new(guard.clone()).redirect_to("/notice"));
    });

    App {
        router: router.compile(&Container::new()).unwrap(),
        users,
        events,
        outbox,
        hasher,
        cookie: Mutex::new(None),
    }
}

impl App {
    async fn send(&self, method: Method, uri: &str, body: Option<Value>, json: bool) -> Answer {
        let mut builder = Request::builder().method(method).uri(uri);
        if json {
            builder = builder.header("accept", "application/json");
        }
        if let Some(cookie) = self.cookie.lock().unwrap().clone() {
            builder = builder.header("cookie", &cookie);
        }
        if let Some(body) = body {
            builder = if json {
                builder.json(&body)
            } else {
                let pairs: Vec<(String, String)> = body
                    .as_object()
                    .unwrap()
                    .iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
                    .collect();
                let borrowed: Vec<(&str, &str)> =
                    pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
                builder.form(&borrowed)
            };
        }

        let mut response = self.router.dispatch(builder.build()).await;

        if let Some(set) = response.header("set-cookie") {
            let pair = set.split(';').next().unwrap().to_string();
            *self.cookie.lock().unwrap() = Some(pair);
        }

        let bytes = response.take_body().collect().await.unwrap();
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        Answer { status: response.status(), response, body }
    }

    async fn post(&self, uri: &str, body: Value) -> Answer {
        self.send(Method::POST, uri, Some(body), true).await
    }

    async fn register(&self, email: &str) -> Answer {
        self.post(
            "/register",
            json!({
                "name": "Ada",
                "email": email,
                "password": "correct horse",
                "password_confirmation": "correct horse",
            }),
        )
        .await
    }

    /// A user that exists without having registered through this session.
    fn seed(&self, email: &str, password: &str) -> User {
        self.users.insert(email, "Ada", Some(self.hasher.hash(password).unwrap()))
    }

    async fn login(&self, email: &str, password: &str) -> Answer {
        self.post("/login", json!({ "email": email, "password": password })).await
    }

    async fn is_logged_in(&self) -> bool {
        // `/user/confirm-password` answers 401 to a guest and 422 to a
        // signed-in user with the wrong password — a probe that changes
        // nothing.
        self.post("/user/confirm-password", json!({ "password": "x" })).await.status
            != StatusCode::UNAUTHORIZED
    }
}

/// The path and query of an absolute link.
fn path_of(url: &str) -> String {
    url.strip_prefix("https://app.test").unwrap().to_string()
}

// --- registration -------------------------------------------------------------

#[tokio::test]
async fn registering_creates_the_user_and_logs_them_in() {
    let app = app(Features::default());

    let answer = app.register(" Ada@Example.com ").await;
    assert_eq!(answer.status, StatusCode::CREATED, "{}", answer.body);
    assert_eq!(answer.body["id"], "1");

    let user = app.users.get("ada@example.com").expect("normalised and stored");
    assert_eq!(user.name, "Ada", "the application's own field reached it");
    assert!(app.hasher.verify("correct horse", user.password.as_deref().unwrap()));
    assert!(app.is_logged_in().await);
    app.events.assert_dispatched_times::<Registered<User>>(1);
}

#[tokio::test]
async fn registration_validates_the_scaffold_rules_and_the_applications() {
    let app = app(Features::default());

    let mismatched = app
        .post(
            "/register",
            json!({ "name": "Ada", "email": "ada@example.com", "password": "correct horse",
                    "password_confirmation": "different horse" }),
        )
        .await;
    assert_eq!(mismatched.status, StatusCode::UNPROCESSABLE_ENTITY);

    let short = app
        .post(
            "/register",
            json!({ "name": "Ada", "email": "ada@example.com", "password": "short",
                    "password_confirmation": "short" }),
        )
        .await;
    assert_eq!(short.status, StatusCode::UNPROCESSABLE_ENTITY);

    let nameless = app
        .post(
            "/register",
            json!({ "email": "ada@example.com", "password": "correct horse",
                    "password_confirmation": "correct horse" }),
        )
        .await;
    assert_eq!(nameless.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(nameless.body.to_string().contains("name"), "{}", nameless.body);

    assert!(app.users.get("ada@example.com").is_none());
}

#[tokio::test]
async fn a_taken_address_cannot_register_twice() {
    let app = app(Features::default());
    app.seed("ada@example.com", "whatever1");

    assert_eq!(app.register("ADA@example.com").await.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(app.users.rows.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_browser_form_post_is_redirected_rather_than_answered_in_json() {
    let app = app(Features::default());

    let answer = app
        .send(
            Method::POST,
            "/register",
            Some(json!({ "name": "Ada", "email": "ada@example.com",
                         "password": "correct horse", "password_confirmation": "correct horse" })),
            false,
        )
        .await;

    assert_eq!(answer.status, StatusCode::SEE_OTHER);
    assert_eq!(answer.header("location"), Some("/dashboard"));
}

// --- login and logout ---------------------------------------------------------

#[tokio::test]
async fn logging_in_with_the_right_password_starts_a_session() {
    let app = app(Features::default());
    app.seed("ada@example.com", "correct horse");

    let answer = app.login("ADA@example.com", "correct horse").await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    assert!(app.is_logged_in().await);
    app.events.assert_dispatched_times::<Login<User>>(1);
}

#[tokio::test]
async fn a_wrong_password_and_an_unknown_address_answer_the_same() {
    let app = app(Features::default());
    app.seed("ada@example.com", "correct horse");

    let wrong = app.login("ada@example.com", "wrong horse").await;
    let unknown = app.login("nobody@example.com", "wrong horse").await;

    assert_eq!(wrong.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(wrong.status, unknown.status);
    assert_eq!(wrong.body, unknown.body, "the words do not say which it was");
    assert!(!app.is_logged_in().await);
    app.events.assert_dispatched_times::<Failed>(2);
}

#[tokio::test]
async fn too_many_failures_lock_the_address_out_with_a_retry_after() {
    let app = app(Features { max_login_attempts: 3, ..Features::default() });
    app.seed("ada@example.com", "correct horse");

    for _ in 0..3 {
        assert_eq!(
            app.login("ada@example.com", "wrong").await.status,
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }

    // Locked — even with the right password, which is the point.
    let locked = app.login("ada@example.com", "correct horse").await;
    assert_eq!(locked.status, StatusCode::TOO_MANY_REQUESTS);
    let retry: u64 = locked.header("retry-after").unwrap().parse().unwrap();
    assert!((1..=60).contains(&retry), "retry-after {retry}");
    app.events.assert_dispatched_times::<Lockout>(1);

    // Another address is unaffected.
    app.seed("grace@example.com", "correct horse");
    assert_eq!(app.login("grace@example.com", "correct horse").await.status, StatusCode::OK);
}

#[tokio::test]
async fn a_successful_login_clears_the_failure_count() {
    let app = app(Features { max_login_attempts: 2, ..Features::default() });
    app.seed("ada@example.com", "correct horse");

    app.login("ada@example.com", "wrong").await;
    assert_eq!(app.login("ada@example.com", "correct horse").await.status, StatusCode::OK);
    app.login("ada@example.com", "wrong").await;
    assert_eq!(
        app.login("ada@example.com", "correct horse").await.status,
        StatusCode::OK,
        "one failure since the last success is under the limit"
    );
}

#[tokio::test]
async fn logging_out_ends_the_session() {
    let app = app(Features::default());
    app.seed("ada@example.com", "correct horse");
    app.login("ada@example.com", "correct horse").await;

    let answer = app.post("/logout", json!({})).await;
    assert_eq!(answer.status, StatusCode::NO_CONTENT);
    assert!(!app.is_logged_in().await);
    app.events.assert_dispatched_times::<Logout<User>>(1);
}

// --- password reset -------------------------------------------------------------

#[tokio::test]
async fn forgot_password_answers_the_same_for_known_and_unknown_addresses() {
    let app = app(Features::default());
    app.seed("ada@example.com", "correct horse");

    let known = app.post("/forgot-password", json!({ "email": "ada@example.com" })).await;
    let unknown = app.post("/forgot-password", json!({ "email": "nobody@example.com" })).await;

    assert_eq!(known.status, StatusCode::OK);
    assert_eq!(known.status, unknown.status);
    assert_eq!(known.body, unknown.body);
    assert_eq!(known.body["status"], RESET_LINK_SENT);

    let sent = app.outbox.resets.lock().unwrap().clone();
    assert_eq!(sent.len(), 1, "only the real account was sent a link");
    assert_eq!(sent[0].0, "ada@example.com");
    assert!(sent[0].1.starts_with("https://app.test/reset-password/"), "{}", sent[0].1);
    assert!(sent[0].1.ends_with("?email=ada%40example.com"), "{}", sent[0].1);
}

#[tokio::test]
async fn a_second_request_inside_the_throttle_sends_nothing_but_says_the_same() {
    let app = app(Features::default());
    app.seed("ada@example.com", "correct horse");

    let first = app.post("/forgot-password", json!({ "email": "ada@example.com" })).await;
    let second = app.post("/forgot-password", json!({ "email": "ada@example.com" })).await;

    assert_eq!(first.body, second.body);
    assert_eq!(app.outbox.resets.lock().unwrap().len(), 1);
}

/// A sender that takes as long as a slow mail provider.
struct SlowOutbox(Arc<Outbox>);

#[async_trait::async_trait]
impl SendsPasswordResetLinks<User> for SlowOutbox {
    async fn send_reset_link(&self, user: &User, url: &str) -> Result<()> {
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        self.0.send_reset_link(user, url).await
    }
}

#[tokio::test]
async fn by_default_delivery_is_off_the_request_so_its_latency_cannot_be_timed() {
    let slow = Arc::new(Outbox::default());
    let sender = Arc::new(SlowOutbox(Arc::clone(&slow)));
    let app = app_with(Features::default(), move |accounts| {
        accounts.sends_reset_links_with(sender as _).deliver_links_inline(false)
    });
    app.seed("ada@example.com", "correct horse");

    let started = std::time::Instant::now();
    let answer = app.post("/forgot-password", json!({ "email": "ada@example.com" })).await;
    assert_eq!(answer.status, StatusCode::OK);
    assert!(
        started.elapsed() < std::time::Duration::from_millis(400),
        "the answer waited for the provider: {:?}",
        started.elapsed()
    );
    assert!(slow.resets.lock().unwrap().is_empty(), "not delivered yet");

    // ...and it is still delivered.
    for _ in 0..50 {
        if !slow.resets.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(slow.resets.lock().unwrap().len(), 1);
}

/// The token from the last reset link sent.
fn last_token(app: &App) -> String {
    let url = app.outbox.resets.lock().unwrap().last().unwrap().1.clone();
    let after = url.split("/reset-password/").nth(1).unwrap();
    after.split('?').next().unwrap().to_string()
}

async fn reset(app: &App, token: &str, password: &str) -> Answer {
    app.post(
        "/reset-password",
        json!({ "token": token, "email": "ada@example.com", "password": password,
                "password_confirmation": password }),
    )
    .await
}

#[tokio::test]
async fn a_reset_link_sets_a_new_password_once() {
    let app = app(Features::default());
    app.seed("ada@example.com", "old password");
    app.post("/forgot-password", json!({ "email": "ada@example.com" })).await;
    let token = last_token(&app);

    let answer = reset(&app, &token, "new password").await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    app.events.assert_dispatched_times::<PasswordReset<User>>(1);

    assert_eq!(app.login("ada@example.com", "old password").await.status.as_u16(), 422);
    assert_eq!(app.login("ada@example.com", "new password").await.status, StatusCode::OK);

    let reused = reset(&app, &token, "third password").await;
    assert_eq!(reused.status, StatusCode::UNPROCESSABLE_ENTITY, "single use");
}

#[tokio::test]
async fn a_wrong_token_or_an_unknown_address_is_refused_alike() {
    let app = app(Features::default());
    app.seed("ada@example.com", "old password");
    app.post("/forgot-password", json!({ "email": "ada@example.com" })).await;

    let wrong = reset(&app, "not-the-token", "new password").await;
    let unknown = app
        .post(
            "/reset-password",
            json!({ "token": last_token(&app), "email": "nobody@example.com",
                    "password": "new password", "password_confirmation": "new password" }),
        )
        .await;

    assert_eq!(wrong.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(wrong.body, unknown.body);

    // And the real token still works afterwards.
    assert_eq!(reset(&app, &last_token(&app), "new password").await.status, StatusCode::OK);
}

#[tokio::test]
async fn an_expired_reset_link_is_refused() {
    let app = app(Features { reset_token_ttl: Duration::from_millis(1), ..Features::default() });
    app.seed("ada@example.com", "old password");
    app.post("/forgot-password", json!({ "email": "ada@example.com" })).await;
    let token = last_token(&app);

    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(reset(&app, &token, "new password").await.status, StatusCode::UNPROCESSABLE_ENTITY);
}

// --- email verification ---------------------------------------------------------

fn last_verification(app: &App) -> String {
    path_of(&app.outbox.verifications.lock().unwrap().last().unwrap().1)
}

#[tokio::test]
async fn registering_sends_a_verification_link_that_verifies() {
    let app = app(Features::all());
    app.register("ada@example.com").await;

    let link = last_verification(&app);
    assert!(link.starts_with("/email/verify/1/"), "{link}");

    // Gated until verified — a browser is sent to the notice.
    let gated = app.send(Method::GET, "/verified-only", None, false).await;
    assert_eq!(gated.status, StatusCode::FOUND);
    assert_eq!(gated.header("location"), Some("/notice"));

    let answer = app.send(Method::GET, &link, None, true).await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    assert!(app.users.get("ada@example.com").unwrap().verified);
    app.events.assert_dispatched_times::<Verified<User>>(1);

    assert_eq!(app.send(Method::GET, "/verified-only", None, true).await.status, StatusCode::OK);
}

#[tokio::test]
async fn a_tampered_verification_link_is_refused() {
    let app = app(Features::all());
    app.register("ada@example.com").await;
    app.seed("grace@example.com", "whatever1");
    let link = last_verification(&app);

    // Somebody else's id on the same signature.
    let other = link.replacen("/email/verify/1/", "/email/verify/2/", 1);
    assert_eq!(app.send(Method::GET, &other, None, true).await.status, StatusCode::FORBIDDEN);

    // The expiry moved.
    let later = link.replace("expires=", "expires=9");
    assert_eq!(app.send(Method::GET, &later, None, true).await.status, StatusCode::FORBIDDEN);

    // No signature at all.
    let bare = link.split('?').next().unwrap().to_string();
    assert_eq!(app.send(Method::GET, &bare, None, true).await.status, StatusCode::FORBIDDEN);

    assert!(!app.users.get("ada@example.com").unwrap().verified);
}

#[tokio::test]
async fn a_link_sent_to_an_old_address_does_not_verify_the_new_one() {
    let app = app(Features::all());
    app.register("ada@example.com").await;
    let link = last_verification(&app);

    app.users.rows.lock().unwrap()[0].email = "ada@elsewhere.com".into();
    assert_eq!(app.send(Method::GET, &link, None, true).await.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn an_expired_verification_link_is_refused() {
    let app = app(Features { verification_ttl: Duration::ZERO, ..Features::all() });
    app.register("ada@example.com").await;
    let link = last_verification(&app);

    // The signer's clock has one-second resolution.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let answer = app.send(Method::GET, &link, None, true).await;
    assert_eq!(answer.status, StatusCode::FORBIDDEN);
    assert!(answer.body.to_string().contains("expired"), "{}", answer.body);
}

#[tokio::test]
async fn the_verification_link_can_be_resent_but_not_in_a_burst() {
    let app = app(Features::all());
    app.register("ada@example.com").await;

    // Registration's own send counts for the outbox, not the throttle.
    let first = app.post("/email/verification-notification", json!({})).await;
    assert_eq!(first.status, StatusCode::ACCEPTED, "{}", first.body);
    let second = app.post("/email/verification-notification", json!({})).await;
    assert_eq!(second.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(second.header("retry-after").is_some());

    assert_eq!(app.outbox.verifications.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn resending_needs_a_signed_in_user() {
    let app = app(Features::all());
    let answer = app.post("/email/verification-notification", json!({})).await;
    assert_eq!(answer.status, StatusCode::UNAUTHORIZED);
}

// --- confirmation and password change ---------------------------------------------

#[tokio::test]
async fn confirming_the_password_checks_it() {
    let app = app(Features::default());
    app.seed("ada@example.com", "correct horse");
    app.login("ada@example.com", "correct horse").await;

    let wrong = app.post("/user/confirm-password", json!({ "password": "nope" })).await;
    assert_eq!(wrong.status, StatusCode::UNPROCESSABLE_ENTITY);

    let right = app.post("/user/confirm-password", json!({ "password": "correct horse" })).await;
    assert_eq!(right.status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn changing_the_password_needs_the_current_one_and_retires_reset_links() {
    let app = app(Features::default());
    app.seed("ada@example.com", "correct horse");
    app.post("/forgot-password", json!({ "email": "ada@example.com" })).await;
    let token = last_token(&app);
    app.login("ada@example.com", "correct horse").await;

    let change = |current: &'static str| {
        app.send(
            Method::PUT,
            "/user/password",
            Some(json!({ "current_password": current, "password": "battery staple",
                         "password_confirmation": "battery staple" })),
            true,
        )
    };

    assert_eq!(change("wrong").await.status, StatusCode::UNPROCESSABLE_ENTITY);
    let changed = change("correct horse").await;
    assert_eq!(changed.status, StatusCode::OK, "{}", changed.body);

    assert!(app.is_logged_in().await, "the session survives, under a new id");
    assert_eq!(
        reset(&app, &token, "attacker's choice").await.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a reset link from before the change no longer works"
    );
}

// --- features and wiring ------------------------------------------------------------

#[tokio::test]
async fn a_disabled_feature_is_not_mounted_at_all() {
    let app = app(Features::login_only());

    for (method, uri) in [
        (Method::POST, "/register"),
        (Method::POST, "/forgot-password"),
        (Method::POST, "/reset-password"),
        (Method::POST, "/email/verification-notification"),
        (Method::POST, "/user/confirm-password"),
        (Method::PUT, "/user/password"),
    ] {
        let answer = app.send(method.clone(), uri, Some(json!({})), true).await;
        assert_eq!(answer.status, StatusCode::NOT_FOUND, "{method} {uri}");
    }

    app.seed("ada@example.com", "correct horse");
    assert_eq!(app.login("ada@example.com", "correct horse").await.status, StatusCode::OK);
}

#[tokio::test]
async fn routes_are_named_and_prefixed() {
    let app = app_with(Features::all(), |accounts| accounts.prefix("auth").names("auth."));
    app.seed("ada@example.com", "correct horse");

    assert_eq!(app.router.uri_for("auth.login"), Some("/auth/login"));
    assert_eq!(
        app.router.uri_for("auth.verification.verify"),
        Some("/auth/email/verify/{id}/{hash}")
    );
    assert_eq!(app.login("ada@example.com", "correct horse").await.status.as_u16(), 404);
    assert_eq!(
        app.post("/auth/login", json!({ "email": "ada@example.com", "password": "correct horse" }))
            .await
            .status,
        StatusCode::OK
    );

    // The verification link knows the prefix.
    app.post("/auth/email/verification-notification", json!({})).await;
    assert!(last_verification(&app).starts_with("/auth/email/verify/"));
}

#[test]
fn an_enabled_feature_missing_its_port_fails_at_boot() {
    let users = Arc::new(Users::default());
    let accounts = Arc::new(Accounts::<User>::new(
        Arc::new(SessionGuard::new("web", Arc::new(Provider(Arc::clone(&users))))),
        users as _,
        Arc::new(Argon2Hasher::insecure_for_tests()),
        Arc::new(MemoryCache::new()),
    ));

    let err = accounts.routes(&mut Router::new()).unwrap_err();
    assert!(err.message().contains("creates_users_with"), "{}", err.message());
}

#[tokio::test]
async fn the_mail_links_send_through_the_mailer() {
    let mailer = Arc::new(
        Mailer::fake(Arc::new(MemoryEngine::new())).with_default_from("hello@acme.test".into()),
    );
    let app = app_with(Features::default(), |accounts| {
        accounts.mails_links_with(MailLinks::new(Arc::clone(&mailer), "Acme"))
    });
    app.seed("ada@example.com", "correct horse");

    app.post("/forgot-password", json!({ "email": "ada@example.com" })).await;

    mailer.assert_sent_to("ada@example.com");
    let sent = mailer.sent();
    assert_eq!(sent[0].envelope.subject, "Reset your Acme password");
    assert!(sent[0].text.as_deref().unwrap().contains("https://app.test/reset-password/"));
    assert!(sent[0].text.as_deref().unwrap().contains("60 minutes"));
}

// --- the second factor ----------------------------------------------------------

#[cfg(feature = "passkeys")]
mod passkeys;
#[cfg(feature = "two-factor")]
mod two_factor;

#[cfg(not(feature = "two-factor"))]
#[test]
fn a_factor_switched_on_in_a_build_without_it_fails_at_boot() {
    let users = Arc::new(Users::default());
    let guard = Arc::new(SessionGuard::new("web", Arc::new(Provider(Arc::clone(&users)))));
    let accounts = Arc::new(
        Accounts::new(
            guard as _,
            users as _,
            Arc::new(Argon2Hasher::insecure_for_tests()),
            Arc::new(MemoryCache::new()),
        )
        .features(Features { two_factor: true, ..Features::login_only() }),
    );

    let err = accounts.routes(&mut Router::new()).unwrap_err();
    assert!(err.message().contains("`two-factor` feature"), "{}", err.message());
}
