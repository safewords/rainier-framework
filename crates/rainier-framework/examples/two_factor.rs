//! Two-factor authentication and passkeys, end to end, offline.
//!
//! ```text
//! cargo run -p rainier-framework --example two_factor --features accounts,two-factor,passkeys
//! ```
//!
//! Part one uses `rainier::mfa` on its own: a TOTP secret, the `otpauth://`
//! URI an app enrols from, a code verified once and refused the second time,
//! and recovery codes.
//!
//! Part two mounts the accounts scaffolding on a real router behind a real
//! session middleware, and plays the client: enrol in two-factor, log out, log
//! in (the password alone answers `{"two_factor": true}`), answer the
//! challenge, and then register a passkey and sign in with it — the passkey
//! side played by `SoftwareAuthenticator`, which answers exactly as a browser
//! and an authenticator would.
//!
//! Everything an application supplies — the user table, where enrolments and
//! passkeys are stored — is an in-memory stand-in at the bottom of the file.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rainier_framework::accounts::{
    AccountUser, AccountUsers, Accounts, CreatesNewUsers, Features, NewUser, PasskeyRepository,
    StoredPasskey, TwoFactorRecord, TwoFactorUsers,
};
use rainier_framework::auth::{
    Argon2Hasher, Authenticatable, Credentials, Hasher, SessionGuard, UserProvider,
};
use rainier_framework::cache::MemoryCache;
use rainier_framework::container::Container;
use rainier_framework::http::{Method, Request, StatusCode};
use rainier_framework::mfa::webauthn::testing::SoftwareAuthenticator;
use rainier_framework::mfa::{recovery, PasskeyCredential, RelyingParty, Secret, Totp};
use rainier_framework::routing::{CompiledRouter, GroupAttributes, Router};
use rainier_framework::session::{MemorySessionStore, StartSession};
use rainier_framework::support::Result;
use serde_json::{json, Value};

#[tokio::main]
async fn main() -> Result<()> {
    standalone_totp();
    accounts_flow().await
}

// --- part one: the primitives ----------------------------------------------------

fn standalone_totp() {
    println!("== TOTP, on its own ==");

    let totp = Totp::new().issuer("Acme");
    let secret = Secret::generate();
    println!("secret (store encrypted):  {}", secret.to_base32());
    println!("QR code contents:          {}", totp.provisioning_uri(&secret, "ada@example.com"));

    // What the authenticator app shows right now.
    let now = unix_now();
    let code = totp.code_at(&secret, now);
    println!("the app shows:             {code}");

    // The first use is accepted and returns the step to remember...
    let step = totp.verify_at(&secret, &code, now, None).expect("a fresh code verifies");
    println!("accepted at step {step} — persist it as `last_used_step`");

    // ...and the same code is refused once that step is remembered.
    let replayed = totp.verify_at(&secret, &code, now, Some(step));
    println!(
        "the same code again:       {}",
        if replayed.is_none() { "refused" } else { "accepted?!" }
    );

    // Recovery codes: shown once, stored hashed, each spent once.
    let codes = recovery::generate(8);
    let mut stored: Vec<String> = codes.iter().map(|code| recovery::hash(code)).collect();
    println!("recovery codes (show once): {}, … ({} total)", codes[0], codes.len());
    println!("spend {}:   {}", codes[0], recovery::consume(&mut stored, &codes[0]));
    println!("spend it again:            {}", recovery::consume(&mut stored, &codes[0]));
    println!("{} recovery codes left\n", stored.len());
}

// --- part two: the routes ---------------------------------------------------------

async fn accounts_flow() -> Result<()> {
    println!("== the accounts scaffolding ==");

    let users = Arc::new(Users::default());
    // `insecure_for_tests` keeps a debug-build example quick. An application
    // uses `Argon2Hasher::new()`.
    let hasher: Arc<dyn Hasher> = Arc::new(Argon2Hasher::insecure_for_tests());
    let guard = Arc::new(SessionGuard::new("web", Arc::new(Provider(Arc::clone(&users)))));

    let accounts = Arc::new(
        Accounts::new(guard, users.clone(), Arc::clone(&hasher), Arc::new(MemoryCache::new()))
            .features(Features {
                two_factor: true,
                passkeys: true,
                // Nothing in this example sends mail.
                reset_passwords: false,
                ..Features::default()
            })
            .creates_users_with(Arc::new(CreateUser(Arc::clone(&users))))
            .two_factor_with(Arc::new(Enrolments::default()), Totp::new().issuer("Acme"))
            .passkeys_with(
                Arc::new(Passkeys::default()),
                RelyingParty::new("app.test", "Acme").origins(["https://app.test"]),
            ),
    );

    // The routes need a session; here it is the group's middleware.
    let mut router = Router::new();
    let store = Arc::new(MemorySessionStore::default());
    router.group(GroupAttributes::new().middleware(StartSession::new(store)), |router| {
        accounts.routes(router).expect("every enabled flow is configured");
    });
    let client = Client::new(router.compile(&Container::new())?);

    // Register — which also logs in.
    let registered = client
        .post(
            "/register",
            json!({
                "email": "ada@example.com",
                "password": "correct horse battery",
                "password_confirmation": "correct horse battery",
            }),
        )
        .await;
    step("POST /register", &registered);

    // Enrolling needs a recently confirmed password.
    let refused = client.post("/user/two-factor-authentication", json!({})).await;
    step("POST /user/two-factor-authentication (unconfirmed)", &refused);
    let confirmed =
        client.post("/user/confirm-password", json!({ "password": "correct horse battery" })).await;
    step("POST /user/confirm-password", &confirmed);

    let enabled = client.post("/user/two-factor-authentication", json!({})).await;
    step("POST /user/two-factor-authentication", &enabled);
    let secret = enabled.body["secret"].as_str().expect("a secret").to_string();
    println!("    the client renders {} as a QR code", enabled.body["otpauth_url"]);

    // The user types the first code their app shows.
    let first = code_for(&secret, 0);
    let confirmed =
        client.post("/user/confirmed-two-factor-authentication", json!({ "code": first })).await;
    step("POST /user/confirmed-two-factor-authentication", &confirmed);

    step("POST /logout", &client.post("/logout", json!({})).await);

    // A right password now opens nothing on its own.
    let login = client
        .post("/login", json!({ "email": "ada@example.com", "password": "correct horse battery" }))
        .await;
    step("POST /login", &login);
    assert_eq!(login.body, json!({ "two_factor": true }));

    // The code the app shows next (this step's was spent confirming).
    let next = code_for(&secret, 30);
    let challenge = client.post("/two-factor-challenge", json!({ "code": next })).await;
    step("POST /two-factor-challenge", &challenge);
    assert_eq!(challenge.body["authenticated"], true);

    // --- passkeys -------------------------------------------------------------
    println!("\n== passkeys ==");
    let mut authenticator = SoftwareAuthenticator::es256("app.test", "https://app.test");

    client.post("/user/confirm-password", json!({ "password": "correct horse battery" })).await;
    let options = client.post("/user/passkeys/options", json!({})).await;
    step("POST /user/passkeys/options", &options);
    println!("    the browser calls navigator.credentials.create() with these options");

    let credential = authenticator.register(&options.body);
    let stored = client
        .post("/user/passkeys", json!({ "credential": credential, "name": "This laptop" }))
        .await;
    step("POST /user/passkeys", &stored);

    step("POST /logout", &client.post("/logout", json!({})).await);

    let options = client.post("/passkeys/login/options", json!({})).await;
    step("POST /passkeys/login/options", &options);
    let assertion = authenticator.authenticate(&options.body);
    let signed_in = client.post("/passkeys/login", json!({ "credential": assertion })).await;
    step("POST /passkeys/login", &signed_in);
    assert_eq!(signed_in.body["authenticated"], true);
    println!("    signed in with a passkey — no password, and no TOTP challenge either");

    Ok(())
}

fn step(label: &str, answer: &Answer) {
    println!("{label:<52} → {} {}", answer.status.as_u16(), answer.body);
}

fn code_for(secret: &str, offset_seconds: u64) -> String {
    let secret = Secret::from_base32(secret).expect("base32");
    Totp::new().code_at(&secret, unix_now() + offset_seconds)
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).expect("after 1970").as_secs()
}

// --- a client with a cookie jar -------------------------------------------------

struct Client {
    router: CompiledRouter,
    cookie: Mutex<Option<String>>,
}

struct Answer {
    status: StatusCode,
    body: Value,
}

impl Client {
    fn new(router: CompiledRouter) -> Self {
        Self { router, cookie: Mutex::new(None) }
    }

    async fn post(&self, uri: &str, body: Value) -> Answer {
        let mut builder = Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header("accept", "application/json")
            .json(&body);
        if let Some(cookie) = self.cookie.lock().expect("cookie jar").clone() {
            builder = builder.header("cookie", &cookie);
        }

        let mut response = self.router.dispatch(builder.build()).await;
        if let Some(set) = response.header("set-cookie") {
            let pair = set.split(';').next().unwrap_or_default().to_string();
            *self.cookie.lock().expect("cookie jar") = Some(pair);
        }

        let bytes = response.take_body().collect().await.unwrap_or_default();
        Answer {
            status: response.status(),
            body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        }
    }
}

// --- what an application supplies -------------------------------------------------

#[derive(Debug, Clone)]
struct User {
    id: u64,
    email: String,
    password: String,
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
        true
    }
}

#[derive(Default)]
struct Users(Mutex<Vec<User>>);

#[async_trait::async_trait]
impl AccountUsers<User> for Users {
    async fn find_by_id(&self, id: &str) -> Result<Option<User>> {
        Ok(self.0.lock().expect("users").iter().find(|u| u.id.to_string() == id).cloned())
    }
    async fn find_by_email(&self, email: &str) -> Result<Option<User>> {
        Ok(self.0.lock().expect("users").iter().find(|u| u.email == email).cloned())
    }
    async fn set_password_hash(&self, user: &User, hash: &str) -> Result<()> {
        for row in self.0.lock().expect("users").iter_mut().filter(|u| u.id == user.id) {
            row.password = hash.to_string();
        }
        Ok(())
    }
}

struct CreateUser(Arc<Users>);

#[async_trait::async_trait]
impl CreatesNewUsers<User> for CreateUser {
    async fn create(&self, new_user: NewUser) -> Result<User> {
        let mut rows = self.0 .0.lock().expect("users");
        let user = User {
            id: rows.len() as u64 + 1,
            email: new_user.email,
            password: new_user.password_hash,
        };
        rows.push(user.clone());
        Ok(user)
    }
}

/// How the session guard reads the logged-in user back.
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

/// TOTP enrolments — in an application, columns or a table, with the secret
/// encrypted at rest.
#[derive(Default)]
struct Enrolments(Mutex<HashMap<u64, TwoFactorRecord>>);

#[async_trait::async_trait]
impl TwoFactorUsers<User> for Enrolments {
    async fn find_two_factor(&self, user: &User) -> Result<Option<TwoFactorRecord>> {
        Ok(self.0.lock().expect("enrolments").get(&user.id).cloned())
    }
    async fn save_two_factor(&self, user: &User, record: Option<&TwoFactorRecord>) -> Result<()> {
        let mut rows = self.0.lock().expect("enrolments");
        match record {
            Some(record) => rows.insert(user.id, record.clone()),
            None => rows.remove(&user.id),
        };
        Ok(())
    }
}

/// Passkeys — in an application, a table of (user id, name, JSON credential)
/// indexed by `credential.id_base64url()`.
#[derive(Default)]
struct Passkeys(Mutex<Vec<(String, String, PasskeyCredential)>>);

#[async_trait::async_trait]
impl PasskeyRepository<User> for Passkeys {
    async fn passkeys_for(&self, user: &User) -> Result<Vec<PasskeyCredential>> {
        let id = user.auth_identifier();
        let rows = self.0.lock().expect("passkeys");
        Ok(rows.iter().filter(|(owner, _, _)| *owner == id).map(|(_, _, c)| c.clone()).collect())
    }
    async fn find_passkey(&self, credential_id: &[u8]) -> Result<Option<StoredPasskey>> {
        let rows = self.0.lock().expect("passkeys");
        Ok(rows
            .iter()
            .find(|(_, _, c)| c.id == credential_id)
            .map(|(owner, _, c)| StoredPasskey { user_id: owner.clone(), credential: c.clone() }))
    }
    async fn store_passkey(
        &self,
        user: &User,
        name: &str,
        credential: &PasskeyCredential,
    ) -> Result<()> {
        self.0.lock().expect("passkeys").push((
            user.auth_identifier(),
            name.into(),
            credential.clone(),
        ));
        Ok(())
    }
    async fn update_passkey(&self, credential: &PasskeyCredential) -> Result<()> {
        for row in
            self.0.lock().expect("passkeys").iter_mut().filter(|(_, _, c)| c.id == credential.id)
        {
            row.2 = credential.clone();
        }
        Ok(())
    }
    async fn delete_passkey(&self, user: &User, credential_id: &[u8]) -> Result<bool> {
        let mut rows = self.0.lock().expect("passkeys");
        let before = rows.len();
        let id = user.auth_identifier();
        rows.retain(|(owner, _, c)| !(*owner == id && c.id == credential_id));
        Ok(rows.len() != before)
    }
}
