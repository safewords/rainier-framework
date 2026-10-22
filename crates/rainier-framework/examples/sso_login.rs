//! "Sign in with GitHub", end to end, offline.
//!
//! What it shows:
//!
//! 1. an [`Sso`] built from an [`SsoConfig`] — GitHub and a company OIDC
//!    issuer, the way they would sit in configuration;
//! 2. a router with the two routes every sign-in needs,
//!    `/auth/{provider}/redirect` and `/auth/{provider}/callback`, behind the
//!    session middleware that keeps the `state`;
//! 3. a browser's round trip driven through them — the redirect, the `state`
//!    read back out of the `Location`, the callback with `code` and `state` —
//!    against a fake transport answering as GitHub would;
//! 4. [`login_with`] over an in-memory [`SocialAccounts`]: `Created` for a new
//!    person, `Existing` the next time, `Linked` for an existing account with a
//!    verified email — and the `409` for an existing account whose email the
//!    provider did **not** verify, which is the account takeover it prevents.
//!
//! ```sh
//! cargo run -p rainier-framework --example sso_login --features sso
//! ```
//!
//! In an application the fake becomes the real transport and the secrets come
//! from the environment:
//!
//! ```ignore
//! use rainier_framework::http_client::ReqwestTransport;
//!
//! let sso = Sso::from_config(&config.get("services.sso")?, Arc::new(ReqwestTransport::new()))?;
//! ```

use std::sync::{Arc, Mutex};

use rainier_framework::auth::{Authenticatable, Credentials, SessionGuard, UserProvider};
use rainier_framework::container::Container;
use rainier_framework::http::{Request, Response};
use rainier_framework::http_client::{FakeTransport, Transport};
use rainier_framework::middleware::{Middleware, Pipeline};
use rainier_framework::routing::{CompiledRouter, Req, Router};
use rainier_framework::session::{MemorySessionStore, SessionStore, StartSession};
use rainier_framework::sso::{
    login_with, LoginOutcome, SocialAccounts, SocialUser, Sso, SsoConfig,
};
use rainier_framework::support::{Error, Result};
use serde_json::json;

// --- the application's side -------------------------------------------------

/// The application's user. An SSO-only user has no password.
#[derive(Clone, Debug)]
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

/// Users and their linked provider identities. In an application: a `users`
/// table and a `social_accounts (user_id, provider, provider_id)` table,
/// unique on the last two.
#[derive(Default)]
struct Accounts {
    users: Mutex<Vec<User>>,
    links: Mutex<Vec<(String, String, String)>>,
}

impl Accounts {
    fn user(&self, id: &str) -> Option<User> {
        self.users.lock().unwrap().iter().find(|u| u.id == id).cloned()
    }
}

#[async_trait::async_trait]
impl SocialAccounts<User> for Accounts {
    async fn find_by_provider(&self, provider: &str, provider_id: &str) -> Result<Option<User>> {
        let links = self.links.lock().unwrap();
        let owner = links.iter().find(|(p, id, _)| p == provider && id == provider_id);
        Ok(owner.and_then(|(_, _, user_id)| self.user(user_id)))
    }

    async fn find_by_email(&self, email: &str) -> Result<Option<User>> {
        Ok(self.users.lock().unwrap().iter().find(|u| u.email == email).cloned())
    }

    async fn link(&self, user: &User, social: &SocialUser) -> Result<()> {
        self.links.lock().unwrap().push((
            social.provider.clone(),
            social.id.clone(),
            user.id.clone(),
        ));
        Ok(())
    }

    async fn create_user(&self, social: &SocialUser) -> Result<User> {
        let mut users = self.users.lock().unwrap();
        let user = User {
            id: format!("user-{}", users.len() + 1),
            email: social.email.clone().unwrap_or_default(),
        };
        users.push(user.clone());
        Ok(user)
    }
}

/// The session guard looks users up through this.
#[async_trait::async_trait]
impl UserProvider<User> for Accounts {
    async fn retrieve_by_id(&self, id: &str) -> Result<Option<User>> {
        Ok(self.user(id))
    }

    async fn retrieve_by_credentials(&self, _: &Credentials) -> Result<Option<User>> {
        Ok(None)
    }

    async fn validate_credentials(&self, _: &User, _: &Credentials) -> Result<bool> {
        Ok(false)
    }
}

// --- configuration and routes -------------------------------------------------

/// What `services.sso` would hold. `github` needs no `driver`; `work` is any
/// OpenID Connect issuer, found by discovery the first time it is used.
fn config() -> SsoConfig {
    serde_json::from_value(json!({
        "github": {
            "client_id": "Iv1.example",
            "client_secret": "github-secret",
            "redirect": "https://app.example.com/auth/github/callback"
        },
        "work": {
            "driver": "oidc",
            "issuer": "https://login.example.com",
            "client_id": "app",
            "client_secret": "oidc-secret",
            "redirect": "https://app.example.com/auth/work/callback"
        }
    }))
    .expect("the configuration is well-formed")
}

/// The two routes. `sso.driver(…)` answers a 404 for a provider nobody
/// declared, so the route parameter needs no checking of its own.
fn routes(
    sso: Sso,
    guard: Arc<SessionGuard<User>>,
    accounts: Arc<Accounts>,
) -> Result<CompiledRouter> {
    let mut router = Router::new();

    let redirect_sso = sso.clone();
    router.get("/auth/{provider}/redirect", move |request: Req| {
        let sso = redirect_sso.clone();
        async move {
            let provider = sso.driver(request.route_param("provider").unwrap_or_default())?;
            provider.redirect(&request).await
        }
    });

    router.get("/auth/{provider}/callback", move |request: Req| {
        let (sso, guard, accounts) = (sso.clone(), Arc::clone(&guard), Arc::clone(&accounts));
        async move {
            let provider = sso.driver(request.route_param("provider").unwrap_or_default())?;
            let social = provider.user(&request).await?;
            let (user, outcome) = login_with(&*guard, &*accounts, &request, &social).await?;
            Ok::<_, Error>(Response::json(&json!({
                "outcome": format!("{outcome:?}"),
                "user": user.id,
                "email": user.email,
            })))
        }
    });

    router.compile(&Container::new())
}

// --- the browser --------------------------------------------------------------

/// One request through the session middleware and the router — what the HTTP
/// kernel does for a real one.
async fn send(
    router: &Arc<CompiledRouter>,
    start: &Arc<StartSession>,
    request: Request,
) -> Response {
    let router = Arc::clone(router);
    Pipeline::new()
        .through_arc(Arc::clone(start) as Arc<dyn Middleware>)
        .then(move |request: Request| {
            let router = Arc::clone(&router);
            async move { router.dispatch(request).await }
        })
        .run(request)
        .await
}

/// The `name=value` half of a `Set-Cookie`, which is what a browser sends back.
fn cookie_of(response: &Response) -> String {
    let set = response.header("set-cookie").expect("the session middleware set a cookie");
    set.split(';').next().unwrap_or_default().to_string()
}

/// What GitHub answers: the token exchange, `/user`, then `/user/emails`.
fn github(
    id: u64,
    login: &str,
    profile_email: &str,
    verified_primary: Option<&str>,
) -> Arc<FakeTransport> {
    let fake = Arc::new(FakeTransport::new());
    fake.responding(
        200,
        r#"{"access_token":"gho_example","token_type":"bearer","scope":"read:user,user:email"}"#,
    );
    fake.responding(
        200,
        json!({ "id": id, "login": login, "name": login, "email": profile_email }).to_string(),
    );
    match verified_primary {
        Some(email) => fake.responding(
            200,
            json!([{ "email": email, "primary": true, "verified": true }]).to_string(),
        ),
        // No `user:email` grant: the profile email is all there is, and it is
        // free text the person typed.
        None => fake.responding(404, r#"{"message":"Not Found"}"#),
    };
    fake
}

/// One browser's whole round trip: redirect, provider, callback.
async fn sign_in(accounts: &Arc<Accounts>, github: Arc<FakeTransport>) -> Result<(u16, String)> {
    let http: Arc<dyn Transport> = github;
    let sso = Sso::from_config(&config(), http)?;
    let guard =
        Arc::new(SessionGuard::new("web", Arc::clone(accounts) as Arc<dyn UserProvider<User>>));
    let router = Arc::new(routes(sso, Arc::clone(&guard), Arc::clone(accounts))?);
    let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::default());
    let start = Arc::new(StartSession::new(store));

    // 1. The "Sign in with GitHub" button.
    let redirect =
        send(&router, &start, Request::builder().uri("/auth/github/redirect").build()).await;
    let location = redirect.header("location").expect("a redirect to GitHub").to_string();
    let cookie = cookie_of(&redirect);
    println!("  302 → {}…", &location[..location.find('?').unwrap_or(location.len())]);

    // 2. GitHub sends the browser back with a code and the state it was given.
    let query = location.split_once('?').map(|(_, q)| q).unwrap_or_default();
    let state = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("state="))
        .expect("the authorize URL carries a state");

    // 3. The callback, from the same browser — the same session cookie.
    let callback = Request::builder()
        .uri(format!("/auth/github/callback?code=from-github&state={state}"))
        .header("cookie", &cookie)
        .build();
    let response = send(&router, &start, callback).await;

    let status = response.status().as_u16();
    let body = rainier_framework::testing::TestResponse::of(response).await.text().to_string();
    Ok((status, body))
}

#[tokio::main]
async fn main() -> Result<()> {
    let accounts = Arc::new(Accounts::default());
    // Ada already has an account here, from before SSO existed.
    accounts.users.lock().unwrap().push(User { id: "ada".into(), email: "ada@example.com".into() });

    let declared = Sso::from_config(&config(), Arc::new(FakeTransport::new()))?;
    println!("providers declared: {}", declared.names().join(", "));

    println!("\n1. someone new signs in with GitHub");
    let (status, body) =
        sign_in(&accounts, github(1, "octocat", "", Some("octo@example.com"))).await?;
    println!("  {status} {body}");
    assert!(body.contains(&format!("{:?}", LoginOutcome::Created)));

    println!("\n2. they come back — found by GitHub id, not by email");
    let (status, body) =
        sign_in(&accounts, github(1, "octocat", "", Some("octo@example.com"))).await?;
    println!("  {status} {body}");
    assert!(body.contains(&format!("{:?}", LoginOutcome::Existing)));

    println!("\n3. someone claims Ada's email on a GitHub account that never verified it");
    let (status, body) =
        sign_in(&accounts, github(666, "mallory", "ada@example.com", None)).await?;
    println!("  {status} {body}");
    assert_eq!(status, 409, "an unverified email must not log into Ada's account");

    println!("\n4. Ada signs in with a GitHub account whose primary email is verified");
    let (status, body) =
        sign_in(&accounts, github(2, "ada-l", "", Some("ada@example.com"))).await?;
    println!("  {status} {body}");
    assert!(body.contains(&format!("{:?}", LoginOutcome::Linked)));

    let links = accounts.links.lock().unwrap();
    println!("\nlinked identities:");
    for (provider, id, user) in links.iter() {
        println!("  {provider}:{id} → {user}");
    }
    Ok(())
}
