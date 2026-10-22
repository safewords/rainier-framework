//! Account flows end to end — registration, logout, login, a forgotten
//! password, and email verification — with nothing outside the process.
//!
//! ```sh
//! cargo run -p rainier-framework --example accounts --features accounts
//! ```
//!
//! The users live in a `Vec`, the cache and the session store are in memory,
//! and "sending an email" records the link in an outbox so the example can
//! click it. Swap each of those for the real thing and the wiring in `main`
//! does not change. See `docs/accounts.md`.

use std::sync::{Arc, Mutex};

use rainier_framework::accounts::{
    AccountUser, AccountUsers, Accounts, CreatesNewUsers, EnsureEmailIsVerified, Features, NewUser,
    SendsPasswordResetLinks, SendsVerificationLinks,
};
use rainier_framework::auth::{
    Argon2Hasher, Authenticatable, Credentials, SessionGuard, UserProvider,
};
use rainier_framework::cache::MemoryCache;
use rainier_framework::container::Container;
use rainier_framework::crypt::{Key, KeyRing, UrlSigner};
use rainier_framework::http::{Method, Request, Response};
use rainier_framework::routing::{CompiledRouter, GroupAttributes, Req, Router};
use rainier_framework::session::csrf::VerifyCsrfToken;
use rainier_framework::session::{MemorySessionStore, StartSession};
use rainier_framework::support::{Error, Result};
use rainier_framework::testing::TestResponse;
use serde_json::{json, Value};

// --- 1. The user model ----------------------------------------------------------
//
// Two traits: `Authenticatable` (how to identify one, where its password hash
// is) and `AccountUser` (the address mail goes to, and whether it is proven).

#[derive(Debug, Clone)]
struct User {
    id: u64,
    email: String,
    name: String,
    password: String,
    verified: bool,
}

impl Authenticatable for User {
    fn auth_identifier(&self) -> String {
        self.id.to_string()
    }

    fn auth_password_hash(&self) -> Option<&str> {
        Some(&self.password)
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

// --- 2. Where users live ----------------------------------------------------------
//
// In an application this is a repository over the `users` table. The flows
// only ever find a user, change their password hash, and mark them verified.

#[derive(Default)]
struct UserStore {
    rows: Mutex<Vec<User>>,
}

impl UserStore {
    fn update(&self, id: u64, change: impl FnOnce(&mut User)) {
        if let Some(row) = self.rows.lock().unwrap().iter_mut().find(|row| row.id == id) {
            change(row);
        }
    }
}

#[async_trait::async_trait]
impl AccountUsers<User> for UserStore {
    async fn find_by_id(&self, id: &str) -> Result<Option<User>> {
        Ok(self.rows.lock().unwrap().iter().find(|u| u.id.to_string() == id).cloned())
    }

    async fn find_by_email(&self, email: &str) -> Result<Option<User>> {
        Ok(self.rows.lock().unwrap().iter().find(|u| u.email == email).cloned())
    }

    async fn set_password_hash(&self, user: &User, hash: &str) -> Result<()> {
        self.update(user.id, |row| row.password = hash.to_string());
        Ok(())
    }

    async fn mark_email_verified(&self, user: &User) -> Result<()> {
        self.update(user.id, |row| row.verified = true);
        Ok(())
    }
}

// Registration hands over an address that is validated, normalised and not
// taken, and a password that is already hashed. Your own fields are yours to
// validate — an `Error::validation` here is a 422 like any other.
#[async_trait::async_trait]
impl CreatesNewUsers<User> for UserStore {
    async fn create(&self, new_user: NewUser) -> Result<User> {
        let name = new_user.input["name"].as_str().unwrap_or_default().trim().to_string();
        if name.is_empty() {
            return Err(Error::validation(json!({ "name": ["Tell us your name."] })));
        }

        let mut rows = self.rows.lock().unwrap();
        let user = User {
            id: rows.len() as u64 + 1,
            email: new_user.email,
            name,
            password: new_user.password_hash,
            verified: false,
        };
        rows.push(user.clone());
        Ok(user)
    }
}

// The session guard reads the logged-in user back by id on every request.
struct Provider(Arc<UserStore>);

#[async_trait::async_trait]
impl UserProvider<User> for Provider {
    async fn retrieve_by_id(&self, id: &str) -> Result<Option<User>> {
        self.0.find_by_id(id).await
    }

    // The account flows check passwords themselves; the guard only needs ids.
    async fn retrieve_by_credentials(&self, _: &Credentials) -> Result<Option<User>> {
        Ok(None)
    }

    async fn validate_credentials(&self, _: &User, _: &Credentials) -> Result<bool> {
        Ok(false)
    }
}

// --- 3. How links reach people ------------------------------------------------------
//
// `MailLinks::new(mailer, "Acme")` sends real email. This outbox records the
// links instead, which is also the shape of a sender that pushes a job onto a
// queue.

#[derive(Default)]
struct Outbox {
    links: Mutex<Vec<String>>,
}

impl Outbox {
    fn last(&self) -> String {
        self.links.lock().unwrap().last().cloned().expect("a link was sent")
    }
}

#[async_trait::async_trait]
impl SendsPasswordResetLinks<User> for Outbox {
    async fn send_reset_link(&self, user: &User, url: &str) -> Result<()> {
        println!("    ✉  reset link for {}: {url}", user.email);
        self.links.lock().unwrap().push(url.to_string());
        Ok(())
    }
}

#[async_trait::async_trait]
impl SendsVerificationLinks<User> for Outbox {
    async fn send_verification_link(&self, user: &User, url: &str) -> Result<()> {
        println!("    ✉  verification link for {}: {url}", user.email);
        self.links.lock().unwrap().push(url.to_string());
        Ok(())
    }
}

// --- 4. Wiring ----------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    let users = Arc::new(UserStore::default());
    let outbox = Arc::new(Outbox::default());
    let guard = Arc::new(SessionGuard::new("web", Arc::new(Provider(Arc::clone(&users)))));

    let accounts = Arc::new(
        Accounts::new(
            guard.clone(),
            Arc::clone(&users) as _,
            // `Argon2Hasher::new()` in an application; the test parameters
            // keep this example quick.
            Arc::new(Argon2Hasher::insecure_for_tests()),
            // Throttles and reset tokens. The shared (Redis) cache once there
            // is more than one replica, or a reset link only works on the
            // replica that sent it.
            Arc::new(MemoryCache::new()),
        )
        .features(Features::all())
        .creates_users_with(Arc::clone(&users) as _)
        .sends_reset_links_with(Arc::clone(&outbox) as _)
        .sends_verification_links_with(Arc::clone(&outbox) as _)
        // Reset links are normally delivered on a detached task, so the answer
        // to `/forgot-password` takes as long for an unknown address as for a
        // real one. This outbox only records — as fast as a queue push — and
        // the example wants the link before it carries on.
        .deliver_links_inline(true)
        .signs_urls_with(Arc::new(UrlSigner::new(KeyRing::new(Key::generate()))))
        .app_url("https://acme.test")
        .home("/dashboard"),
    );

    let mut router = Router::new();

    // The account routes need a session; browser-facing ones also want CSRF
    // protection, which runs inside it. A front end reads the `XSRF-TOKEN`
    // cookie and echoes it in an `X-XSRF-TOKEN` header — the client below
    // does exactly that. (An API authenticated by bearer tokens has no session
    // to forge and would sit outside both.)
    let web = GroupAttributes::new().middleware((
        StartSession::new(Arc::new(MemorySessionStore::default())),
        VerifyCsrfToken::new(),
    ));

    router.group(web, |router| {
        // A page to land on first, which hands out the CSRF cookie.
        router.get("/", |_: Req| async { Response::no_content() });

        // Every account route, under `accounts.*` names.
        accounts.routes(router).expect("every enabled flow has what it needs");

        // Something only a verified address may reach.
        router
            .get("/dashboard", |_: Req| async { Response::text("welcome in") })
            .middleware(EnsureEmailIsVerified::<User>::new(guard.clone()));
    });

    let browser = Browser::new(router.compile(&Container::new())?);

    // --- 5. Driving it --------------------------------------------------------------

    println!("\nA visitor arrives and gets a CSRF cookie");
    browser.get("/").await;

    println!("\nThey register — and are logged in, and sent a verification link");
    browser
        .send(
            Method::POST,
            "/register",
            json!({
                "name": "Ada",
                "email": "Ada@Example.com",
                "password": "correct horse",
                "password_confirmation": "correct horse",
            }),
        )
        .await;
    let verification = outbox.last();

    println!("\nThe dashboard is closed until the address is verified");
    browser.get("/dashboard").await;

    println!("\nThey log out, then back in");
    browser.send(Method::POST, "/logout", json!({})).await;
    browser
        .send(Method::POST, "/login", json!({ "email": "ada@example.com", "password": "nope" }))
        .await;
    browser
        .send(
            Method::POST,
            "/login",
            json!({ "email": "ada@example.com", "password": "correct horse" }),
        )
        .await;

    println!("\nThey forget their password");
    browser.send(Method::POST, "/forgot-password", json!({ "email": "ada@example.com" })).await;
    // An address with no account gets the very same answer.
    browser.send(Method::POST, "/forgot-password", json!({ "email": "nobody@example.com" })).await;

    // The link points at the front end's page: /reset-password/{token}?email=…
    let reset = outbox.last();
    let token = reset.split("/reset-password/").nth(1).unwrap().split('?').next().unwrap();

    println!("\nThey follow the reset link and choose a new password");
    browser
        .send(
            Method::POST,
            "/reset-password",
            json!({
                "token": token,
                "email": "ada@example.com",
                "password": "battery staple",
                "password_confirmation": "battery staple",
            }),
        )
        .await;
    println!("  …and the same link does not work twice");
    browser
        .send(
            Method::POST,
            "/reset-password",
            json!({
                "token": token,
                "email": "ada@example.com",
                "password": "something else",
                "password_confirmation": "something else",
            }),
        )
        .await;

    println!("\nThey click the verification link from their first email");
    browser.get(verification.strip_prefix("https://acme.test").unwrap()).await;
    browser.get("/dashboard").await;

    let ada = users.find_by_email("ada@example.com").await?.unwrap();
    println!("\n{} (#{}) is verified: {}", ada.name, ada.id, ada.verified);
    Ok(())
}

// --- A browser, minus the browser -----------------------------------------------------
//
// Carries cookies between requests and echoes the CSRF cookie back as a
// header — what a front end's HTTP client does.

struct Browser {
    router: CompiledRouter,
    cookies: Mutex<Vec<(String, String)>>,
}

impl Browser {
    fn new(router: CompiledRouter) -> Self {
        Self { router, cookies: Mutex::new(Vec::new()) }
    }

    async fn get(&self, uri: &str) -> TestResponse {
        self.run(Request::builder().method(Method::GET).uri(uri)).await
    }

    async fn send(&self, method: Method, uri: &str, body: Value) -> TestResponse {
        self.run(Request::builder().method(method).uri(uri).json(&body)).await
    }

    async fn run(&self, mut request: rainier_framework::http::RequestBuilder) -> TestResponse {
        {
            let cookies = self.cookies.lock().unwrap();
            if !cookies.is_empty() {
                let header: Vec<String> =
                    cookies.iter().map(|(name, value)| format!("{name}={value}")).collect();
                request = request.header("cookie", &header.join("; "));
            }
            if let Some((_, token)) = cookies.iter().find(|(name, _)| name == "XSRF-TOKEN") {
                request = request.header("x-xsrf-token", token);
            }
        }
        let request = request.header("accept", "application/json").build();
        let method = request.method().clone();
        let uri = request.path().to_string();

        let response = self.router.dispatch(request).await;
        self.remember(&response);

        let response = TestResponse::of(response).await;
        let body = response.text();
        println!(
            "  {:<6} {:<34} → {}{}",
            method.as_str(),
            uri,
            response.status(),
            if body.is_empty() { String::new() } else { format!("  {body}") }
        );
        response
    }

    fn remember(&self, response: &Response) {
        let mut cookies = self.cookies.lock().unwrap();
        for set in response.headers().get_all("set-cookie") {
            let Some((name, value)) =
                set.to_str().ok().and_then(|set| set.split(';').next()?.split_once('='))
            else {
                continue;
            };
            cookies.retain(|(existing, _)| existing != name);
            cookies.push((name.trim().to_string(), value.trim().to_string()));
        }
    }
}
