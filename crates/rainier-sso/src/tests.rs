//! The round trip, end to end, against the framework's fake transport and a
//! real session bag.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use rainier_auth::{Authenticatable, Credentials, Guard, SessionGuard, UserProvider};
use rainier_crypt::{Jwt, JwtKey, JwtKeyRing};
use rainier_http::Request;
use rainier_http_client::transport::{OutboundRequest, RawResponse};
use rainier_http_client::{FakeTransport, Transport};
use rainier_session::Session;
use rainier_support::{BoxFuture, ErrorKind, Result};
use serde_json::{json, Value};

use crate::secret::code_challenge;
use crate::*;

// --- helpers -----------------------------------------------------------------

fn credentials() -> ClientCredentials {
    ClientCredentials::new("client-1", "shh-secret", "https://app.test/auth/callback")
}

fn query_of(url: &str) -> BTreeMap<String, String> {
    let query = url.split_once('?').map(|(_, q)| q).unwrap_or("");
    form_urlencoded::parse(query.as_bytes()).into_owned().collect()
}

fn form_of(request: &rainier_http_client::RecordedRequest) -> BTreeMap<String, String> {
    form_urlencoded::parse(request.body().as_bytes()).into_owned().collect()
}

/// The pending values `redirect` left in the session.
fn pending(session: &Session, provider: &str) -> Value {
    session.value(&format!("_sso.{provider}")).expect("a sign-in is pending")
}

fn callback(session: &Session, query: &str) -> Request {
    Request::builder()
        .uri(format!("/auth/callback?{query}"))
        .build()
        .with_extension(session.clone())
}

/// Start a sign-in and return the session and the authorize URL.
async fn start(provider: &OAuth2Provider) -> (Session, String) {
    let session = Session::new();
    let request = Request::builder().build().with_extension(session.clone());
    let url = provider.authorization_url(&request).await.unwrap();
    (session, url)
}

fn state_of(session: &Session, provider: &str) -> String {
    pending(session, provider)["state"].as_str().unwrap().to_string()
}

/// A transport answering by URL, so a test can change what one endpoint says
/// between calls — which a strict queue cannot do once the round trip's
/// nonce, generated mid-test, has to appear in a later answer.
#[derive(Default)]
struct Routes {
    answers: Mutex<BTreeMap<String, (u16, String)>>,
    recorded: Mutex<Vec<OutboundRequest>>,
}

impl Routes {
    fn answer(&self, url: &str, status: u16, body: impl Into<String>) {
        self.answers.lock().unwrap().insert(url.to_string(), (status, body.into()));
    }

    fn calls_to(&self, url: &str) -> usize {
        self.recorded.lock().unwrap().iter().filter(|r| r.url == url).count()
    }
}

impl Transport for Routes {
    fn send<'a>(&'a self, request: OutboundRequest) -> BoxFuture<'a, Result<RawResponse>> {
        Box::pin(async move {
            let url = request.url.split('?').next().unwrap_or_default().to_string();
            self.recorded.lock().unwrap().push(request);
            let (status, body) =
                self.answers.lock().unwrap().get(&url).cloned().unwrap_or((404, "no route".into()));
            Ok(RawResponse { status, headers: BTreeMap::new(), body: body.into_bytes() })
        })
    }

    fn name(&self) -> &str {
        "routes"
    }
}

// --- the authorize redirect --------------------------------------------------

#[tokio::test]
async fn the_authorize_url_carries_pkce_state_and_scopes() {
    let fake = Arc::new(FakeTransport::new());
    let github = providers::github(credentials(), Api::new(fake.clone()))
        .with_parameter("allow_signup", "false");

    let (session, url) = start(&github).await;
    let query = query_of(&url);
    let pending = pending(&session, "github");

    assert!(url.starts_with("https://github.com/login/oauth/authorize?"), "{url}");
    assert_eq!(query["client_id"], "client-1");
    assert_eq!(query["redirect_uri"], "https://app.test/auth/callback");
    assert_eq!(query["response_type"], "code");
    assert_eq!(query["scope"], "read:user user:email");
    assert_eq!(query["allow_signup"], "false");
    assert_eq!(query["state"], pending["state"].as_str().unwrap());
    assert_eq!(query["code_challenge_method"], "S256");
    assert_eq!(query["code_challenge"], code_challenge(pending["verifier"].as_str().unwrap()));
    assert!(!query.contains_key("nonce"), "plain OAuth carries no nonce");
    assert!(!url.contains("shh-secret"), "the secret never goes to the browser");
    fake.assert_nothing_sent();
}

#[tokio::test]
async fn redirect_answers_a_302_to_the_provider() {
    let github = providers::github(credentials(), Api::new(Arc::new(FakeTransport::new())));
    let request = Request::builder().build().with_extension(Session::new());

    let response = github.redirect(&request).await.unwrap();
    assert_eq!(response.status().as_u16(), 302);
    assert!(response.header("location").unwrap().starts_with("https://github.com/"));
}

#[tokio::test]
async fn every_redirect_has_fresh_secrets() {
    let github = providers::github(credentials(), Api::new(Arc::new(FakeTransport::new())));
    let (_, first) = start(&github).await;
    let (_, second) = start(&github).await;

    assert_ne!(query_of(&first)["state"], query_of(&second)["state"]);
    assert_ne!(query_of(&first)["code_challenge"], query_of(&second)["code_challenge"]);
}

#[tokio::test]
async fn a_route_without_a_session_is_refused_rather_than_run_unprotected() {
    let github = providers::github(credentials(), Api::new(Arc::new(FakeTransport::new())));
    let err = github.redirect(&Request::builder().build()).await.unwrap_err();
    assert!(err.message().contains("session middleware"), "{}", err.message());
}

// --- the callback ------------------------------------------------------------

fn github_answers(fake: &FakeTransport) {
    fake.responding(
        200,
        r#"{"access_token":"gho_token","token_type":"bearer","scope":"read:user,user:email"}"#,
    )
    .responding(
        200,
        r#"{"id":583231,"login":"octocat","name":"The Octocat","email":"public@example.com","avatar_url":"https://avatars/u/583231"}"#,
    )
    .responding(
        200,
        r#"[{"email":"old@example.com","primary":false,"verified":true},
            {"email":"octo@example.com","primary":true,"verified":true}]"#,
    );
}

#[tokio::test]
async fn a_github_sign_in_redeems_the_code_with_the_verifier() {
    let fake = Arc::new(FakeTransport::new());
    github_answers(&fake);
    let github = providers::github(credentials(), Api::new(fake.clone()));

    let (session, _) = start(&github).await;
    let verifier = pending(&session, "github")["verifier"].as_str().unwrap().to_string();
    let state = state_of(&session, "github");

    let user =
        github.user(&callback(&session, &format!("code=the-code&state={state}"))).await.unwrap();

    assert_eq!(user.provider, "github");
    assert_eq!(user.id, "583231");
    assert_eq!(user.nickname.as_deref(), Some("octocat"));
    assert_eq!(user.name.as_deref(), Some("The Octocat"));
    assert_eq!(
        user.email.as_deref(),
        Some("octo@example.com"),
        "the verified primary, not the profile's"
    );
    assert_eq!(user.email_verified, Some(true));
    assert_eq!(user.token.access_token, "gho_token");
    assert_eq!(user.token.scopes, ["read:user", "user:email"]);

    let sent = fake.recorded();
    assert_eq!(sent.len(), 3);
    let exchange = &sent[0];
    assert_eq!(exchange.method(), "POST");
    assert_eq!(exchange.url(), "https://github.com/login/oauth/access_token");
    assert_eq!(exchange.header("content-type"), Some("application/x-www-form-urlencoded"));
    assert_eq!(exchange.header("accept"), Some("application/json"));
    let form = form_of(exchange);
    assert_eq!(form["grant_type"], "authorization_code");
    assert_eq!(form["code"], "the-code");
    assert_eq!(form["code_verifier"], verifier);
    assert_eq!(form["redirect_uri"], "https://app.test/auth/callback");
    assert_eq!(form["client_id"], "client-1");
    assert_eq!(form["client_secret"], "shh-secret");

    assert_eq!(sent[1].url(), "https://api.github.com/user");
    assert_eq!(sent[1].header("authorization"), Some("Bearer gho_token"));
    assert!(sent[1].header("user-agent").is_some(), "GitHub refuses requests without one");
    assert_eq!(sent[2].url(), "https://api.github.com/user/emails");
}

#[tokio::test]
async fn a_callback_works_once() {
    let fake = Arc::new(FakeTransport::new());
    github_answers(&fake);
    let github = providers::github(credentials(), Api::new(fake.clone()));

    let (session, _) = start(&github).await;
    let query = format!("code=c&state={}", state_of(&session, "github"));

    github.user(&callback(&session, &query)).await.unwrap();
    let sent = fake.count();

    let replay = github.user(&callback(&session, &query)).await.unwrap_err();
    assert_eq!(replay.kind(), ErrorKind::Unauthenticated);
    assert!(replay.message().contains("no github sign-in is in progress"), "{}", replay.message());
    assert_eq!(fake.count(), sent, "a replay reaches no endpoint");
}

#[tokio::test]
async fn a_mismatched_state_is_refused_and_consumes_the_attempt() {
    let fake = Arc::new(FakeTransport::new());
    github_answers(&fake);
    let github = providers::github(credentials(), Api::new(fake.clone()));

    let (session, _) = start(&github).await;
    let real = state_of(&session, "github");

    let err = github.user(&callback(&session, "code=c&state=forged")).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Unauthenticated);
    assert!(err.message().contains("state does not match"), "{}", err.message());

    // The real state no longer works either: a guess burns the attempt.
    let after =
        github.user(&callback(&session, &format!("code=c&state={real}"))).await.unwrap_err();
    assert!(after.message().contains("no github sign-in"), "{}", after.message());
    fake.assert_nothing_sent();
}

#[tokio::test]
async fn a_missing_state_is_refused() {
    let github = providers::github(credentials(), Api::new(Arc::new(FakeTransport::new())));
    let (session, _) = start(&github).await;

    let err = github.user(&callback(&session, "code=c")).await.unwrap_err();
    assert!(err.message().contains("state does not match"), "{}", err.message());
}

#[tokio::test]
async fn a_provider_error_is_reported_and_consumes_the_attempt() {
    let fake = Arc::new(FakeTransport::new());
    let github = providers::github(credentials(), Api::new(fake.clone()));
    let (session, _) = start(&github).await;
    let state = state_of(&session, "github");

    let err = github
        .user(&callback(
            &session,
            &format!("error=access_denied&error_description=The+user+said+no&state={state}"),
        ))
        .await
        .unwrap_err();

    assert_eq!(err.kind(), ErrorKind::Unauthenticated);
    assert!(err.message().contains("access_denied The user said no"), "{}", err.message());
    assert!(session.value("_sso.github").is_none());
    fake.assert_nothing_sent();
}

#[tokio::test]
async fn a_callback_without_a_code_is_a_bad_request() {
    let github = providers::github(credentials(), Api::new(Arc::new(FakeTransport::new())));
    let (session, _) = start(&github).await;

    let err = github
        .user(&callback(&session, &format!("state={}", state_of(&session, "github"))))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::BadRequest);
}

#[tokio::test]
async fn a_token_refusal_inside_a_200_is_still_a_refusal() {
    let fake = Arc::new(FakeTransport::new());
    fake.responding(
        200,
        r#"{"error":"bad_verification_code","error_description":"The code passed is incorrect or expired."}"#,
    );
    let github = providers::github(credentials(), Api::new(fake.clone()));
    let (session, _) = start(&github).await;

    let err = github
        .user(&callback(&session, &format!("code=c&state={}", state_of(&session, "github"))))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Unauthenticated);
    assert!(err.message().contains("bad_verification_code"), "{}", err.message());
    fake.assert_sent_count(1);
}

#[tokio::test]
async fn without_the_email_scope_github_falls_back_to_the_unverified_profile_email() {
    let fake = Arc::new(FakeTransport::new());
    fake.responding(200, r#"{"access_token":"t","scope":"read:user"}"#)
        .responding(200, r#"{"id":1,"login":"octo","email":"typed@example.com"}"#)
        .responding(404, r#"{"message":"Not Found"}"#);
    let github = providers::github(credentials(), Api::new(fake));
    let (session, _) = start(&github).await;

    let user = github
        .user(&callback(&session, &format!("code=c&state={}", state_of(&session, "github"))))
        .await
        .unwrap();
    assert_eq!(user.email.as_deref(), Some("typed@example.com"));
    assert_eq!(user.email_verified, None, "a profile email is free text");
    assert_eq!(user.verified_email(), None);
}

#[tokio::test]
async fn basic_client_auth_keeps_the_secret_out_of_the_body() {
    let fake = Arc::new(FakeTransport::new());
    fake.responding(200, r#"{"access_token":"t","refresh_token":"r2","expires_in":3600}"#);
    let gitlab =
        providers::gitlab(credentials(), Api::new(fake.clone())).client_auth(ClientAuth::Basic);

    let token = gitlab.refresh("r1").await.unwrap();
    assert_eq!(token.refresh_token.as_deref(), Some("r2"));
    assert!(token.expires_at.is_some());

    let sent = &fake.recorded()[0];
    assert_eq!(sent.url(), "https://gitlab.com/oauth/token");
    // base64("client-1:shh-secret")
    assert_eq!(sent.header("authorization"), Some("Basic Y2xpZW50LTE6c2hoLXNlY3JldA=="));
    let form = form_of(sent);
    assert_eq!(form["grant_type"], "refresh_token");
    assert_eq!(form["refresh_token"], "r1");
    assert!(!form.contains_key("client_secret"));
}

#[tokio::test]
async fn a_refresh_that_does_not_rotate_keeps_the_old_refresh_token() {
    let fake = Arc::new(FakeTransport::new());
    fake.responding(200, r#"{"access_token":"new"}"#);
    let discord = providers::discord(credentials(), Api::new(fake));

    let token = discord.refresh("keep-me").await.unwrap();
    assert_eq!(token.access_token, "new");
    assert_eq!(token.refresh_token.as_deref(), Some("keep-me"));
}

#[tokio::test]
async fn discord_and_gitlab_map_their_profiles() {
    let fake = Arc::new(FakeTransport::new());
    fake.responding(
        200,
        r#"{"id":"80351110224678912","username":"nelly","global_name":"Nelly","email":"nelly@example.com","verified":true,"avatar":"8342729096ea3675442027381ff50dfe"}"#,
    );
    let discord = providers::discord(credentials(), Api::new(fake));
    let user = discord.user_from_token(AccessToken::bearer("t")).await.unwrap();
    assert_eq!(user.id, "80351110224678912");
    assert_eq!(user.name.as_deref(), Some("Nelly"));
    assert_eq!(user.email_verified, Some(true));
    assert_eq!(
        user.avatar.as_deref(),
        Some("https://cdn.discordapp.com/avatars/80351110224678912/8342729096ea3675442027381ff50dfe.png")
    );

    let fake = Arc::new(FakeTransport::new());
    fake.responding(
        200,
        r#"{"id":7,"username":"jo","email":"jo@example.com","confirmed_at":null}"#,
    );
    let gitlab = OAuth2Provider::new(
        "gitlab",
        credentials(),
        providers::GitLab::at("https://git.example.com/"),
        Api::new(fake.clone()),
    );
    let user = gitlab.user_from_token(AccessToken::bearer("t")).await.unwrap();
    assert_eq!(user.email_verified, None, "an unconfirmed account's email says nothing");
    assert_eq!(fake.recorded()[0].url(), "https://git.example.com/api/v4/user");
}

#[tokio::test]
async fn x_maps_its_profile_and_asks_for_email_only_with_the_scope() {
    let fake = Arc::new(FakeTransport::new());
    fake.responding(
        200,
        r#"{"data":{"id":"2244994945","name":"X Dev","username":"XDevelopers","profile_image_url":"https://pbs.twimg.com/profile_images/1/a_normal.jpg"}}"#,
    );
    let x = providers::x(credentials(), Api::new(fake.clone()));

    let user = x.user_from_token(AccessToken::bearer("t")).await.unwrap();
    assert_eq!(user.id, "2244994945");
    assert_eq!(user.nickname.as_deref(), Some("XDevelopers"));
    assert_eq!(user.avatar.as_deref(), Some("https://pbs.twimg.com/profile_images/1/a.jpg"));
    assert_eq!(user.email, None);
    assert_eq!(user.email_verified, None, "no address is not an unverified address");

    let sent = &fake.recorded()[0];
    assert!(sent.url().starts_with("https://api.x.com/2/users/me?user.fields="));
    assert!(!sent.url().contains("confirmed_email"), "{}", sent.url());
    assert_eq!(sent.header("authorization"), Some("Bearer t"));

    let fake = Arc::new(FakeTransport::new());
    fake.responding(200, r#"{"data":{"id":"1","username":"a","confirmed_email":"a@example.com"}}"#);
    let x = providers::x(credentials(), Api::new(fake.clone()));
    let mut token = AccessToken::bearer("t");
    token.scopes = vec!["users.read".into(), "users.email".into()];

    let user = x.user_from_token(token).await.unwrap();
    assert!(fake.recorded()[0].url().contains("confirmed_email"));
    assert_eq!(user.email.as_deref(), Some("a@example.com"));
    assert_eq!(user.email_verified, Some(true));
}

#[tokio::test]
async fn x_authenticates_to_the_token_endpoint_with_basic_auth() {
    let fake = Arc::new(FakeTransport::new());
    fake.responding(200, r#"{"access_token":"new","token_type":"bearer"}"#);
    let x = providers::x(credentials(), Api::new(fake.clone()));

    x.refresh("r1").await.unwrap();
    let sent = &fake.recorded()[0];
    assert_eq!(sent.url(), "https://api.x.com/2/oauth2/token");
    assert_eq!(sent.header("authorization"), Some("Basic Y2xpZW50LTE6c2hoLXNlY3JldA=="));
}

#[tokio::test]
async fn x_authorize_url_carries_pkce_and_its_scopes() {
    let x = providers::x(credentials(), Api::new(Arc::new(FakeTransport::new())));
    let (_session, url) = start(&x).await;
    assert!(url.starts_with("https://x.com/i/oauth2/authorize?"), "{url}");
    let query = query_of(&url);
    assert_eq!(query["scope"], "tweet.read users.read offline.access");
    assert_eq!(query["code_challenge_method"], "S256");
}

#[test]
fn x_is_a_configurable_driver_and_twitter_is_its_alias() {
    let config: SsoConfig = serde_json::from_value(json!({
        "twitter": { "client_id": "c", "client_secret": "s", "redirect": "https://app.test/cb" },
    }))
    .unwrap();
    let sso = Sso::from_config(&config, Arc::new(FakeTransport::new())).unwrap();
    assert!(sso.has("twitter"));
}

// --- OpenID Connect ----------------------------------------------------------

const ISSUER: &str = "https://login.example.com";

struct Issuer {
    routes: Arc<Routes>,
    signer: Jwt,
}

impl Issuer {
    fn new(kid: &str) -> Self {
        Self::with_issuer(kid, ISSUER, ISSUER)
    }

    fn with_issuer(kid: &str, base: &str, claimed: &str) -> Self {
        let routes = Arc::new(Routes::default());
        routes.answer(
            &format!("{base}/.well-known/openid-configuration"),
            200,
            json!({
                "issuer": claimed,
                "authorization_endpoint": format!("{base}/authorize"),
                "token_endpoint": format!("{base}/token"),
                "userinfo_endpoint": format!("{base}/userinfo"),
                "jwks_uri": format!("{base}/jwks"),
            })
            .to_string(),
        );
        let signer = Jwt::new(JwtKeyRing::new(JwtKey::generate_es256(kid).unwrap()));
        let issuer = Self { routes, signer };
        issuer.publish(&[&issuer.signer]);
        issuer
    }

    fn base(&self) -> String {
        // Every route this issuer answers shares one base.
        let answers = self.routes.answers.lock().unwrap();
        let discovery = answers.keys().find(|k| k.ends_with("openid-configuration")).unwrap();
        discovery.trim_end_matches("/.well-known/openid-configuration").to_string()
    }

    fn publish(&self, signers: &[&Jwt]) {
        let keys: Vec<Value> =
            signers.iter().flat_map(|s| s.jwks()["keys"].as_array().unwrap().clone()).collect();
        // Drop `alg`, the way Microsoft publishes its keys.
        let keys: Vec<Value> = keys
            .into_iter()
            .map(|mut k| {
                k.as_object_mut().unwrap().remove("alg");
                k
            })
            .collect();
        self.routes.answer(
            &format!("{}/jwks", self.base()),
            200,
            json!({ "keys": keys }).to_string(),
        );
    }

    fn issue(&self, signer: &Jwt, claims: Value) {
        let id_token = signer.sign(&claims).unwrap();
        self.routes.answer(
            &format!("{}/token", self.base()),
            200,
            json!({ "access_token": "at", "token_type": "Bearer", "id_token": id_token, "expires_in": 3600 })
                .to_string(),
        );
    }

    fn claims(&self, nonce: &str) -> Value {
        json!({
            "iss": ISSUER,
            "aud": "client-1",
            "sub": "user-42",
            "exp": chrono::Utc::now().timestamp() + 300,
            "iat": chrono::Utc::now().timestamp(),
            "nonce": nonce,
            "email": "ada@example.com",
            "email_verified": true,
            "name": "Ada Lovelace",
            "preferred_username": "ada",
            "picture": "https://pics/ada.png",
        })
    }
}

fn nonce_of(session: &Session, provider: &str) -> String {
    pending(session, provider)["nonce"].as_str().unwrap().to_string()
}

async fn oidc_sign_in(
    provider: &OAuth2Provider,
    issuer: &Issuer,
    edit: impl FnOnce(&mut Value),
) -> Result<SocialUser> {
    let (session, _) = start(provider).await;
    let mut claims = issuer.claims(&nonce_of(&session, provider.name()));
    edit(&mut claims);
    issuer.issue(&issuer.signer, claims);
    provider
        .user(&callback(&session, &format!("code=c&state={}", state_of(&session, provider.name()))))
        .await
}

#[tokio::test]
async fn an_oidc_sign_in_is_discovered_and_its_id_token_verified() {
    let issuer = Issuer::new("k1");
    let work = providers::oidc("work", ISSUER, credentials(), Api::new(issuer.routes.clone()));

    let (session, url) = start(&work).await;
    let query = query_of(&url);
    assert!(url.starts_with("https://login.example.com/authorize?"), "{url}");
    assert_eq!(query["scope"], "openid email profile");
    assert_eq!(query["nonce"], nonce_of(&session, "work"));

    issuer.issue(&issuer.signer, issuer.claims(&nonce_of(&session, "work")));
    let user = work
        .user(&callback(&session, &format!("code=c&state={}", state_of(&session, "work"))))
        .await
        .unwrap();

    assert_eq!(user.provider, "work");
    assert_eq!(user.id, "user-42");
    assert_eq!(user.email.as_deref(), Some("ada@example.com"));
    assert_eq!(user.email_verified, Some(true));
    assert_eq!(user.nickname.as_deref(), Some("ada"));
    assert_eq!(user.avatar.as_deref(), Some("https://pics/ada.png"));
    assert!(user.token.id_token.is_some());

    assert_eq!(
        issuer.routes.calls_to(&format!("{ISSUER}/.well-known/openid-configuration")),
        1,
        "discovered once"
    );
    assert_eq!(
        issuer.routes.calls_to(&format!("{ISSUER}/userinfo")),
        0,
        "the ID token had the email"
    );
}

#[tokio::test]
async fn an_id_token_minted_for_another_sign_in_is_refused() {
    let issuer = Issuer::new("k1");
    let work = providers::oidc("work", ISSUER, credentials(), Api::new(issuer.routes.clone()));

    let err =
        oidc_sign_in(&work, &issuer, |c| c["nonce"] = json!("somebody-elses")).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Unauthenticated);
    assert!(err.message().contains("nonce"), "{}", err.message());
}

#[tokio::test]
async fn an_id_token_for_another_client_is_refused() {
    let issuer = Issuer::new("k1");
    let work = providers::oidc("work", ISSUER, credentials(), Api::new(issuer.routes.clone()));

    let err = oidc_sign_in(&work, &issuer, |c| c["aud"] = json!("client-2")).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Unauthenticated);

    let err = oidc_sign_in(&work, &issuer, |c| {
        c["aud"] = json!(["client-1", "client-2"]);
        c["azp"] = json!("client-2");
    })
    .await
    .unwrap_err();
    assert!(err.message().contains("another client"), "{}", err.message());
}

#[tokio::test]
async fn an_id_token_from_another_issuer_is_refused() {
    let issuer = Issuer::new("k1");
    let work = providers::oidc("work", ISSUER, credentials(), Api::new(issuer.routes.clone()));

    let err = oidc_sign_in(&work, &issuer, |c| c["iss"] = json!("https://evil.example.com"))
        .await
        .unwrap_err();
    assert!(err.message().contains("evil.example.com"), "{}", err.message());
}

#[tokio::test]
async fn an_expired_id_token_is_refused() {
    let issuer = Issuer::new("k1");
    let work = providers::oidc("work", ISSUER, credentials(), Api::new(issuer.routes.clone()));

    let err =
        oidc_sign_in(&work, &issuer, |c| c["exp"] = json!(chrono::Utc::now().timestamp() - 3600))
            .await
            .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Unauthenticated);
}

#[tokio::test]
async fn an_id_token_signed_by_a_key_the_issuer_does_not_publish_is_refused() {
    let issuer = Issuer::new("k1");
    let work = providers::oidc("work", ISSUER, credentials(), Api::new(issuer.routes.clone()));
    let forger = Jwt::new(JwtKeyRing::new(JwtKey::generate_es256("k1").unwrap()));

    let (session, _) = start(&work).await;
    issuer.issue(&forger, issuer.claims(&nonce_of(&session, "work")));
    let err = work
        .user(&callback(&session, &format!("code=c&state={}", state_of(&session, "work"))))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Unauthenticated, "same kid, wrong key: the signature fails");
}

#[tokio::test]
async fn a_rotated_key_is_picked_up_with_one_refetch() {
    let issuer = Issuer::new("k1");
    let work = providers::oidc("work", ISSUER, credentials(), Api::new(issuer.routes.clone()));
    oidc_sign_in(&work, &issuer, |_| {}).await.unwrap();
    let jwks = format!("{ISSUER}/jwks");
    assert_eq!(issuer.routes.calls_to(&jwks), 1);

    // A second sign-in with the cached key costs no fetch.
    oidc_sign_in(&work, &issuer, |_| {}).await.unwrap();
    assert_eq!(issuer.routes.calls_to(&jwks), 1);

    // The issuer rotates to k2.
    let next = Jwt::new(JwtKeyRing::new(JwtKey::generate_es256("k2").unwrap()));
    issuer.publish(&[&issuer.signer, &next]);
    let (session, _) = start(&work).await;
    issuer.issue(&next, issuer.claims(&nonce_of(&session, "work")));
    work.user(&callback(&session, &format!("code=c&state={}", state_of(&session, "work"))))
        .await
        .unwrap();
    assert_eq!(issuer.routes.calls_to(&jwks), 2);
}

#[tokio::test]
async fn a_round_trip_without_an_id_token_is_refused() {
    let issuer = Issuer::new("k1");
    let work = providers::oidc("work", ISSUER, credentials(), Api::new(issuer.routes.clone()));
    let (session, _) = start(&work).await;
    issuer.routes.answer(&format!("{ISSUER}/token"), 200, r#"{"access_token":"at"}"#);

    let err = work
        .user(&callback(&session, &format!("code=c&state={}", state_of(&session, "work"))))
        .await
        .unwrap_err();
    assert!(err.message().contains("no ID token"), "{}", err.message());
}

#[tokio::test]
async fn a_discovery_document_claiming_another_issuer_is_refused() {
    let issuer = Issuer::with_issuer("k1", ISSUER, "https://someone-else.example.com");
    let work = providers::oidc("work", ISSUER, credentials(), Api::new(issuer.routes.clone()));

    let request = Request::builder().build().with_extension(Session::new());
    let err = work.redirect(&request).await.unwrap_err();
    assert!(err.message().contains("someone-else"), "{}", err.message());
}

#[tokio::test]
async fn an_email_only_behind_userinfo_is_merged_for_the_same_subject() {
    let issuer = Issuer::new("k1");
    issuer.routes.answer(
        &format!("{ISSUER}/userinfo"),
        200,
        r#"{"sub":"user-42","email":"ada@example.com","email_verified":"true"}"#,
    );
    let work = providers::oidc("work", ISSUER, credentials(), Api::new(issuer.routes.clone()));

    let user = oidc_sign_in(&work, &issuer, |c| {
        let c = c.as_object_mut().unwrap();
        c.remove("email");
        c.remove("email_verified");
    })
    .await
    .unwrap();
    assert_eq!(user.email.as_deref(), Some("ada@example.com"));
    assert_eq!(user.email_verified, Some(true));

    // A userinfo answer about somebody else is not merged.
    issuer.routes.answer(
        &format!("{ISSUER}/userinfo"),
        200,
        r#"{"sub":"user-99","email":"x@example.com"}"#,
    );
    let user = oidc_sign_in(&work, &issuer, |c| {
        c.as_object_mut().unwrap().remove("email");
    })
    .await
    .unwrap();
    assert_eq!(user.email, None);
}

#[tokio::test]
async fn a_token_from_elsewhere_is_identified_by_userinfo() {
    let issuer = Issuer::new("k1");
    issuer.routes.answer(&format!("{ISSUER}/userinfo"), 200, r#"{"sub":"user-42","name":"Ada"}"#);
    let work = providers::oidc("work", ISSUER, credentials(), Api::new(issuer.routes.clone()));

    let user = work
        .user_from_token(AccessToken {
            id_token: Some("not-ours".into()),
            ..AccessToken::bearer("at")
        })
        .await
        .unwrap();
    assert_eq!(user.id, "user-42");
    assert_eq!(user.token.id_token, None, "an ID token nobody here asked for is set aside");
}

#[tokio::test]
async fn microsoft_multi_tenant_fills_the_issuer_from_tid_and_never_verifies_email() {
    let base = "https://login.microsoftonline.com/common/v2.0";
    let issuer =
        Issuer::with_issuer("k1", base, "https://login.microsoftonline.com/{tenantid}/v2.0");
    let microsoft = providers::microsoft("common", credentials(), Api::new(issuer.routes.clone()));

    let tid = "9188040d-6c67-4c5b-b112-36a304b66dad";
    let user = oidc_sign_in(&microsoft, &issuer, |c| {
        c["iss"] = json!(format!("https://login.microsoftonline.com/{tid}/v2.0"));
        c["tid"] = json!(tid);
    })
    .await
    .unwrap();
    assert_eq!(user.raw["tid"], tid);
    assert_eq!(user.email_verified, None, "Entra's email is an attribute, not a verified address");

    // A token from a tenant other than the one it names is refused.
    let err = oidc_sign_in(&microsoft, &issuer, |c| {
        c["iss"] = json!("https://login.microsoftonline.com/another-tenant/v2.0");
        c["tid"] = json!(tid);
    })
    .await
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Unauthenticated);
}

// --- the manager -------------------------------------------------------------

fn config() -> SsoConfig {
    serde_json::from_value(json!({
        "github": { "client_id": "gh", "client_secret": "s1", "redirect": "https://app/gh" },
        "work": { "driver": "oidc", "issuer": ISSUER, "client_id": "w", "client_secret": "s2",
                  "redirect_uri": "https://app/work", "scopes": ["openid", "email"],
                  "parameters": { "prompt": "login" } },
        "azure": { "driver": "microsoft", "tenant": "contoso.example", "client_id": "a",
                   "client_secret": "s3", "redirect": "https://app/azure" },
    }))
    .unwrap()
}

#[tokio::test]
async fn providers_are_declared_in_configuration() {
    let issuer = Issuer::new("k1");
    let sso = Sso::from_config(&config(), issuer.routes.clone()).unwrap();

    assert_eq!(sso.names(), ["azure", "github", "work"]);
    assert_eq!(sso.driver("work").unwrap().name(), "work");

    let request = Request::builder().build().with_extension(Session::new());
    let response = sso.driver("work").unwrap().redirect(&request).await.unwrap();
    let query = query_of(response.header("location").unwrap());
    assert_eq!(query["scope"], "openid email");
    assert_eq!(query["prompt"], "login");
    assert_eq!(query["client_id"], "w");
    assert!(
        request.extension::<Session>().unwrap().value("_sso.work").is_some(),
        "namespaced by its own name"
    );
}

#[test]
fn an_unknown_provider_is_a_404_naming_the_known_ones() {
    let sso = Sso::from_config(&config(), Arc::new(FakeTransport::new())).unwrap();
    let err = sso.driver("myspace").err().expect("unknown");
    assert_eq!(err.kind(), ErrorKind::NotFound);
    assert!(err.message().contains("azure, github, work"), "{}", err.message());
}

#[test]
fn a_bad_declaration_fails_at_boot() {
    let missing_issuer: SsoConfig = serde_json::from_value(json!({
        "corp": { "driver": "oidc", "client_id": "c", "client_secret": "s", "redirect": "r" }
    }))
    .unwrap();
    let err = Sso::from_config(&missing_issuer, Arc::new(FakeTransport::new())).err().unwrap();
    assert!(err.message().contains("issuer"), "{}", err.message());

    let unknown: SsoConfig = serde_json::from_value(json!({
        "x": { "driver": "friendster", "client_id": "c", "client_secret": "s", "redirect": "r" }
    }))
    .unwrap();
    let err = Sso::from_config(&unknown, Arc::new(FakeTransport::new())).err().unwrap();
    assert!(err.message().contains("friendster"), "{}", err.message());
}

#[test]
fn no_secret_reaches_debug_output() {
    let config = config();
    let sso = Sso::from_config(&config, Arc::new(FakeTransport::new())).unwrap();
    let github = providers::github(credentials(), Api::new(Arc::new(FakeTransport::new())));

    for printed in [
        format!("{config:?}"),
        format!("{sso:?}"),
        format!("{github:?}"),
        format!("{:?}", credentials()),
    ] {
        for secret in ["s1", "s2", "s3", "shh-secret"] {
            assert!(!printed.contains(&format!("\"{secret}\"")), "{secret} in {printed}");
        }
    }
}

// --- linking and logging in ----------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
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

#[derive(Default)]
struct Store {
    users: Mutex<Vec<User>>,
    links: Mutex<Vec<(String, String, String)>>,
}

#[async_trait::async_trait]
impl SocialAccounts<User> for Store {
    async fn find_by_provider(&self, provider: &str, id: &str) -> Result<Option<User>> {
        let links = self.links.lock().unwrap().clone();
        let Some((_, _, user_id)) = links.iter().find(|(p, i, _)| p == provider && i == id) else {
            return Ok(None);
        };
        Ok(self.users.lock().unwrap().iter().find(|u| &u.id == user_id).cloned())
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
            id: format!("u{}", users.len() + 1),
            email: social.email.clone().unwrap_or_default(),
        };
        users.push(user.clone());
        Ok(user)
    }
}

#[async_trait::async_trait]
impl UserProvider<User> for Store {
    async fn retrieve_by_id(&self, id: &str) -> Result<Option<User>> {
        Ok(self.users.lock().unwrap().iter().find(|u| u.id == id).cloned())
    }
    async fn retrieve_by_credentials(&self, _: &Credentials) -> Result<Option<User>> {
        Ok(None)
    }
    async fn validate_credentials(&self, _: &User, _: &Credentials) -> Result<bool> {
        Ok(false)
    }
}

fn social(provider: &str, id: &str, email: &str, verified: Option<bool>) -> SocialUser {
    Profile {
        id: id.into(),
        email: Some(email.into()),
        email_verified: verified,
        ..Profile::default()
    }
    .into_user(provider, AccessToken::bearer("t"))
}

fn setup() -> (Arc<Store>, SessionGuard<User>) {
    let store = Arc::new(Store::default());
    store.users.lock().unwrap().push(User { id: "ada".into(), email: "ada@example.com".into() });
    let guard = SessionGuard::new("web", store.clone() as Arc<dyn UserProvider<User>>);
    (store, guard)
}

#[tokio::test]
async fn a_new_person_is_created_linked_and_logged_in() {
    let (store, guard) = setup();
    let request = Request::builder().build().with_extension(Session::new());

    let (user, outcome) =
        login_with(&guard, &*store, &request, &social("github", "1", "new@example.com", None))
            .await
            .unwrap();
    assert_eq!(outcome, LoginOutcome::Created);
    assert_eq!(guard.user(&request).await.unwrap(), Some(user.clone()));

    // And the next time, it is the same user, by provider id.
    let request = Request::builder().build().with_extension(Session::new());
    let (again, outcome) =
        login_with(&guard, &*store, &request, &social("github", "1", "changed@example.com", None))
            .await
            .unwrap();
    assert_eq!(outcome, LoginOutcome::Existing);
    assert_eq!(again, user);
}

#[tokio::test]
async fn an_existing_account_is_linked_by_a_verified_email() {
    let (store, guard) = setup();
    let request = Request::builder().build().with_extension(Session::new());

    let (user, outcome) = login_with(
        &guard,
        &*store,
        &request,
        &social("google", "g1", "ada@example.com", Some(true)),
    )
    .await
    .unwrap();
    assert_eq!(outcome, LoginOutcome::Linked);
    assert_eq!(user.id, "ada");
}

#[tokio::test]
async fn an_existing_account_is_never_taken_over_by_an_unverified_email() {
    let (store, guard) = setup();

    for verified in [None, Some(false)] {
        let request = Request::builder().build().with_extension(Session::new());
        let err = login_with(
            &guard,
            &*store,
            &request,
            &social("evil", "m1", "ada@example.com", verified),
        )
        .await
        .unwrap_err();

        assert_eq!(err.kind(), ErrorKind::Conflict);
        assert_eq!(guard.user(&request).await.unwrap(), None, "nobody was logged in");
        assert!(store.links.lock().unwrap().is_empty(), "nothing was linked");
        assert_eq!(store.users.lock().unwrap().len(), 1, "no duplicate account was made");
    }
}

#[tokio::test]
async fn connecting_from_settings_refuses_an_identity_owned_by_someone_else() {
    let (store, _) = setup();
    let ada = User { id: "ada".into(), email: "ada@example.com".into() };
    let other = store.create_user(&social("x", "x", "bob@example.com", None)).await.unwrap();

    let github = social("github", "9", "whatever@example.com", None);
    connect(&*store, &ada, &github).await.unwrap();
    connect(&*store, &ada, &github).await.unwrap(); // idempotent for the owner

    let err = connect(&*store, &other, &github).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Conflict);
}
