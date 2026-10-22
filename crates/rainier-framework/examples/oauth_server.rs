//! An OAuth 2.0 authorization server, end to end, offline.
//!
//! ```sh
//! cargo run -p rainier-framework --example oauth_server --features oauth-server
//! ```
//!
//! Everything runs through a compiled router, the way a real application
//! would serve it — only the network is missing:
//!
//! 1. two clients: a confidential web app and a public mobile app;
//! 2. the authorization code flow with PKCE — validate, consent, complete,
//!    exchange the code at `POST /oauth/token`;
//! 3. a refresh, with rotation: the old refresh token is spent;
//! 4. a client-credentials token;
//! 5. a personal access token;
//! 6. an API route behind `OAuthGuard` and a scope, called with and without it;
//! 7. revocation, and the purge that tidies up after it.
//!
//! The memory store and a freshly generated key keep it self-contained. A real
//! deployment implements `OAuthStore` over its own tables (or uses
//! `rainier-compat`'s, when taking over from a PHP server) and loads its key
//! with `AccessTokenSigner::from_pem`.

use std::sync::Arc;

use async_trait::async_trait;
use rainier_framework::auth::{
    AuthManager, Authenticatable, Authenticate, AuthenticatedUser, Credentials, RequireAbility,
    UserProvider,
};
use rainier_framework::container::Container;
use rainier_framework::crypt::{AeadEncrypter, Argon2Hasher, Hasher, Key, KeyRing};
use rainier_framework::http::{Method, Request, Response};
use rainier_framework::oauth_server::{
    grant, pkce, AccessTokenSigner, AuthorizationServer, AuthorizationSession, Client,
    EncrypterSealer, MemoryStore, OAuthGuard, OAuthStore, Scopes, UserProviderOwners,
};
use rainier_framework::routing::{CompiledRouter, GroupAttributes, Req, Router};
use rainier_framework::session::{MemorySessionStore, SessionStore, StartSession};
use rainier_framework::support::Result;
use serde_json::Value;

// --- the application's side: a user model and where users come from -----------

#[derive(Debug, Clone)]
struct User {
    id: String,
    name: String,
}

impl Authenticatable for User {
    fn auth_identifier(&self) -> String {
        self.id.clone()
    }
    fn auth_password_hash(&self) -> Option<&str> {
        None
    }
}

/// One user, Ada, id 7, password `correct horse`.
struct Users;

#[async_trait]
impl UserProvider<User> for Users {
    async fn retrieve_by_id(&self, id: &str) -> Result<Option<User>> {
        Ok((id == "7").then(|| User { id: "7".into(), name: "Ada".into() }))
    }
    async fn retrieve_by_credentials(&self, credentials: &Credentials) -> Result<Option<User>> {
        Ok((credentials.get("email") == Some("ada@example.com"))
            .then(|| User { id: "7".into(), name: "Ada".into() }))
    }
    async fn validate_credentials(&self, _: &User, credentials: &Credentials) -> Result<bool> {
        Ok(credentials.password_value() == Some("correct horse"))
    }
}

const WEB_SECRET: &str = "web-app-secret";
const REDIRECT: &str = "https://web.example/callback";

#[tokio::main]
async fn main() -> Result<()> {
    // --- 1. the server and its clients ----------------------------------------

    let hasher: Arc<dyn Hasher> = Arc::new(Argon2Hasher::insecure_for_tests());
    let store = MemoryStore::new().with_personal_access_client("personal");

    store.insert_client(Client {
        id: "web".into(),
        name: "Web dashboard".into(),
        secret_hash: Some(hasher.hash(WEB_SECRET)?),
        redirect_uris: vec![REDIRECT.into()],
        grant_types: vec![
            grant::AUTHORIZATION_CODE.into(),
            grant::REFRESH_TOKEN.into(),
            grant::CLIENT_CREDENTIALS.into(),
        ],
        scopes: None,
        provider: None,
        owner_id: None,
        revoked: false,
    });
    store.insert_client(Client {
        id: "mobile".into(),
        name: "Mobile app".into(),
        secret_hash: None, // public: an app on a phone cannot keep a secret
        redirect_uris: vec!["http://127.0.0.1/callback".into()],
        grant_types: vec![grant::AUTHORIZATION_CODE.into(), grant::REFRESH_TOKEN.into()],
        scopes: None,
        provider: None,
        owner_id: None,
        revoked: false,
    });
    store.insert_client(Client {
        id: "personal".into(),
        name: "Personal access".into(),
        secret_hash: Some(hasher.hash("unused")?),
        redirect_uris: vec![],
        grant_types: vec![grant::PERSONAL_ACCESS.into()],
        scopes: None,
        provider: None,
        owner_id: None,
        revoked: false,
    });
    let store = Arc::new(store);
    step("1. registered a confidential client (web), a public one (mobile), and a personal access client");

    let sealer =
        Arc::new(EncrypterSealer::new(Arc::new(AeadEncrypter::new(KeyRing::new(Key::generate())))));
    let server = Arc::new(
        AuthorizationServer::new(
            store.clone() as Arc<dyn OAuthStore>,
            sealer,
            AccessTokenSigner::generate(2048)?,
            hasher.clone(),
        )
        .scopes(
            Scopes::new()
                .with("account", "Read your account")
                .with("vehicles", "See and control your vehicles"),
        )
        .password_grant(Arc::new(UserProviderOwners::new(
            Arc::new(Users) as Arc<dyn UserProvider<User>>
        ))),
    );

    let app = App::new(server.clone(), store.clone());

    // --- 2. authorization code + PKCE, as the mobile app ----------------------

    let verifier = pkce::verifier();
    let challenge = pkce::challenge(&verifier);
    let authorize = format!(
        "/oauth/authorize?response_type=code&client_id=mobile&scope=account%20vehicles\
         &redirect_uri=http%3A%2F%2F127.0.0.1%3A53111%2Fcallback&state=xyz\
         &code_challenge={challenge}&code_challenge_method=S256"
    );
    let consent = app.send(Method::GET, &authorize, &[]).await;
    step(&format!(
        "2. GET /oauth/authorize → {} — the consent screen: {}",
        consent.status, consent.body
    ));

    let auth_token = consent.json["auth_token"].as_str().unwrap_or_default().to_string();
    let approved = app.send(Method::POST, "/oauth/authorize", &[("auth_token", &auth_token)]).await;
    let location = approved.location.clone().unwrap_or_default();
    step(&format!(
        "   POST /oauth/authorize (approve) → {} to {}",
        approved.status,
        shorten(&location)
    ));

    let code = query_value(&location, "code").unwrap_or_default();
    let exchanged = app
        .send(
            Method::POST,
            "/oauth/token",
            &[
                ("grant_type", "authorization_code"),
                ("client_id", "mobile"),
                ("redirect_uri", "http://127.0.0.1:53111/callback"),
                ("code", &code),
                ("code_verifier", &verifier),
            ],
        )
        .await;
    step(&format!(
        "   POST /oauth/token (authorization_code) → {}: access token {}, refresh token {}",
        exchanged.status,
        shorten(exchanged.json["access_token"].as_str().unwrap_or("-")),
        shorten(exchanged.json["refresh_token"].as_str().unwrap_or("-")),
    ));
    let replay = app
        .send(
            Method::POST,
            "/oauth/token",
            &[
                ("grant_type", "authorization_code"),
                ("client_id", "mobile"),
                ("redirect_uri", "http://127.0.0.1:53111/callback"),
                ("code", &code),
                ("code_verifier", &verifier),
            ],
        )
        .await;
    step(&format!("   the same code again → {} {}", replay.status, replay.json["hint"]));

    // --- 3. refresh, with rotation ---------------------------------------------

    let old_refresh = exchanged.json["refresh_token"].as_str().unwrap_or_default().to_string();
    let refresh = |token: &str| {
        vec![
            ("grant_type".to_string(), "refresh_token".to_string()),
            ("client_id".into(), "mobile".into()),
            ("refresh_token".into(), token.to_string()),
        ]
    };
    let refreshed = app.send_owned(Method::POST, "/oauth/token", refresh(&old_refresh)).await;
    step(&format!("3. refresh → {}: a new pair", refreshed.status));
    let reused = app.send_owned(Method::POST, "/oauth/token", refresh(&old_refresh)).await;
    step(&format!("   the old refresh token again → {} {}", reused.status, reused.json["hint"]));

    // --- 4. client credentials ---------------------------------------------------

    let machine = app
        .send(
            Method::POST,
            "/oauth/token",
            &[
                ("grant_type", "client_credentials"),
                ("client_id", "web"),
                ("client_secret", WEB_SECRET),
                ("scope", "vehicles"),
            ],
        )
        .await;
    step(&format!(
        "4. client_credentials → {}: a token for the client itself, refresh token: {}",
        machine.status,
        machine.json.get("refresh_token").is_some()
    ));

    // --- 5. a personal access token ---------------------------------------------

    let personal =
        server.issue_personal_access_token("7", "CLI on my laptop", &["account"], None).await?;
    step(&format!(
        "5. personal access token `{}` for user 7, expires in {} days, scopes {:?}",
        personal.token.name.as_deref().unwrap_or_default(),
        (personal.expires_in as f64 / 86_400.0).round(),
        personal.token.scopes
    ));

    // --- 6. the API side ---------------------------------------------------------

    let user_token = refreshed.json["access_token"].as_str().unwrap_or_default().to_string();
    let with_scope = app.get_with_bearer("/api/vehicles", &user_token).await;
    step(&format!(
        "6. GET /api/vehicles with the account+vehicles token → {} {}",
        with_scope.status, with_scope.body
    ));
    let without_scope = app.get_with_bearer("/api/vehicles", &personal.access_token).await;
    step(&format!(
        "   GET /api/vehicles with the account-only personal token → {} {}",
        without_scope.status, without_scope.body
    ));
    let anonymous = app.get_with_bearer("/api/vehicles", "not-a-token").await;
    step(&format!("   GET /api/vehicles with junk → {}", anonymous.status));

    // --- 7. revoke and purge -------------------------------------------------------

    server.revoke(&personal.token.id).await?;
    let revoked = app.get_with_bearer("/api/me", &personal.access_token).await;
    step(&format!("7. revoked the personal token; GET /api/me with it → {}", revoked.status));
    let purged = server.purge(std::time::Duration::ZERO).await?;
    step(&format!(
        "   purge → removed {} access tokens, {} refresh tokens, {} codes",
        purged.access_tokens, purged.refresh_tokens, purged.auth_codes
    ));

    Ok(())
}

// --- the routes, as an application would declare them ------------------------------

struct App {
    router: CompiledRouter,
    cookie: std::sync::Mutex<Option<String>>,
}

impl App {
    fn new(server: Arc<AuthorizationServer>, store: Arc<MemoryStore>) -> Self {
        let users: Arc<dyn UserProvider<User>> = Arc::new(Users);
        let guard = OAuthGuard::new("api", server.verifier(), store as Arc<dyn OAuthStore>, users);
        let auth = Arc::new(AuthManager::<User>::new("api").register(Arc::new(guard)));
        let sessions: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::default());

        let mut router = Router::new();

        // The token endpoint: no session, no CSRF — the client authenticates itself.
        let token_server = server.clone();
        router.post("/oauth/token", move |request: Req| {
            let server = token_server.clone();
            async move { server.respond_to_token_request(&request).await }
        });

        // The authorization endpoint lives in the browser's session. In a real
        // application both routes also sit behind the login middleware, and
        // the user comes from it; here Ada (id 7) is always logged in.
        router.group(GroupAttributes::new().middleware(StartSession::new(sessions)), |router| {
            let show = server.clone();
            router.get("/oauth/authorize", move |request: Req| {
                let server = show.clone();
                async move {
                    let authorization = match server.validate_authorization_request(&request).await
                    {
                        Ok(authorization) => authorization,
                        Err(error) => return error.into_response(),
                    };
                    match AuthorizationSession::stash(&request, &authorization) {
                        // The consent screen: a template in a real application.
                        Ok(auth_token) => Response::json(&serde_json::json!({
                            "client": authorization.client_name,
                            "scopes": authorization.scopes,
                            "auth_token": auth_token,
                        })),
                        Err(error) => Response::from(error),
                    }
                }
            });

            let approve = server.clone();
            router.post("/oauth/authorize", move |request: Req| {
                let server = approve.clone();
                async move {
                    let token = request.input_or("auth_token", "");
                    match AuthorizationSession::take(&request, &token) {
                        Ok(authorization) => {
                            server.complete_authorization_request(&authorization, "7", true).await
                        }
                        Err(error) => Response::from(error),
                    }
                }
            });
        });

        // The API: `Authenticate` through the OAuth guard, then a scope.
        router
            .get("/api/me", |user: AuthenticatedUser<User>| async move {
                Response::json(&serde_json::json!({ "name": user.get().name }))
            })
            .middleware(Authenticate::new(auth.clone()));
        router
            .get("/api/vehicles", |_: Req| async {
                Response::json(&serde_json::json!(["Model 3"]))
            })
            .middleware(Authenticate::new(auth))
            .middleware(RequireAbility::all(["vehicles"]));

        Self {
            router: router.compile(&Container::new()).expect("the routes compile"),
            cookie: std::sync::Mutex::new(None),
        }
    }

    async fn send(&self, method: Method, uri: &str, form: &[(&str, &str)]) -> Answer {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(cookie) = self.cookie.lock().expect("cookie lock").clone() {
            builder = builder.header("cookie", &cookie);
        }
        if !form.is_empty() {
            builder = builder.form(form);
        }
        self.answer(builder.build()).await
    }

    async fn send_owned(&self, method: Method, uri: &str, form: Vec<(String, String)>) -> Answer {
        let borrowed: Vec<(&str, &str)> =
            form.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        self.send(method, uri, &borrowed).await
    }

    async fn get_with_bearer(&self, uri: &str, token: &str) -> Answer {
        let request =
            Request::builder().uri(uri).header("authorization", &format!("Bearer {token}")).build();
        self.answer(request).await
    }

    async fn answer(&self, request: Request) -> Answer {
        let mut response = self.router.dispatch(request).await;
        if let Some(set) = response.header("set-cookie") {
            let pair = set.split(';').next().unwrap_or_default().to_string();
            *self.cookie.lock().expect("cookie lock") = Some(pair);
        }
        let location = response.header("location").map(str::to_string);
        let bytes = response.take_body().collect().await.unwrap_or_default();
        let body = String::from_utf8_lossy(&bytes).into_owned();
        Answer {
            status: response.status().as_u16(),
            json: serde_json::from_str(&body).unwrap_or(Value::Null),
            body,
            location,
        }
    }
}

struct Answer {
    status: u16,
    body: String,
    json: Value,
    location: Option<String>,
}

fn step(line: &str) {
    println!("{line}");
}

/// Long tokens, shortened for the terminal.
fn shorten(value: &str) -> String {
    if value.len() <= 40 {
        value.to_string()
    } else {
        format!("{}…({} chars)", &value[..32], value.len())
    }
}

/// One parameter of a URL's query string, decoded.
fn query_value(url: &str, name: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == name).then(|| decode(value))
    })
}

fn decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                out.push(u8::from_str_radix(&value[i + 1..i + 3], 16).unwrap_or(b'?'));
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}
