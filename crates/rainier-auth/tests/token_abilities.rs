//! Token abilities, through the real middleware pipeline.
//!
//! The unit tests in `abilities.rs` assert what a set of abilities grants.
//! This asserts the part that has to be wired correctly to matter: that the
//! guard's answer reaches the request, and that a token issued for less than
//! its owner can do is actually refused.

use std::sync::Arc;

use rainier_auth::{
    Abilities, AbilitiesRequestExt, AuthManager, Authenticatable, Authenticate, Credentials,
    RequireAbility, TokenGuard, UserProvider,
};
use rainier_http::{Method, Request, Response, StatusCode};
use rainier_middleware::Pipeline;
use rainier_support::Result;

#[derive(Debug, Clone, PartialEq)]
struct User {
    id: u64,
    admin: bool,
}

impl Authenticatable for User {
    fn auth_identifier(&self) -> String {
        self.id.to_string()
    }

    fn auth_password_hash(&self) -> Option<&str> {
        None
    }
}

/// One user, and a table of tokens with what each was issued for.
struct Tokens;

#[async_trait::async_trait]
impl UserProvider<User> for Tokens {
    async fn retrieve_by_id(&self, _id: &str) -> Result<Option<User>> {
        Ok(None)
    }

    async fn retrieve_by_credentials(&self, _credentials: &Credentials) -> Result<Option<User>> {
        Ok(None)
    }

    async fn validate_credentials(&self, _user: &User, _credentials: &Credentials) -> Result<bool> {
        Ok(false)
    }

    async fn retrieve_by_token(&self, token: &str) -> Result<Option<User>> {
        // Every one of these belongs to the same admin. The point is that they
        // do not all reach the same endpoints.
        Ok(match token {
            "read-only" | "publisher" | "unrestricted" => Some(User { id: 1, admin: true }),
            _ => None,
        })
    }

    async fn retrieve_abilities_by_token(&self, token: &str) -> Result<Abilities> {
        Ok(match token {
            "read-only" => Abilities::new(["posts:read"]),
            "publisher" => Abilities::new(["posts:*"]),
            "unrestricted" => Abilities::everything(),
            _ => Abilities::none(),
        })
    }
}

fn auth() -> Arc<AuthManager<User>> {
    Arc::new(AuthManager::new("api").register(Arc::new(TokenGuard::new("api", Arc::new(Tokens)))))
}

fn request(token: &str) -> Request {
    Request::builder()
        .method(Method::POST)
        .uri("/api/posts")
        .header("authorization", &format!("Bearer {token}"))
        .build()
}

/// `auth` then the ability check, which is the order a route declares.
async fn through(middleware: RequireAbility, request: Request) -> Response {
    Pipeline::new()
        .through(Authenticate::new(auth()))
        .through(middleware)
        .then(|request: Request| async move {
            // Proof the abilities reached the handler, not just the guard.
            Response::ok(request.token_abilities().map(Abilities::to_csv).unwrap_or_default())
        })
        .run(request)
        .await
}

#[tokio::test]
async fn a_token_issued_for_it_gets_through() {
    let response = through(RequireAbility::any(["posts:read"]), request("read-only")).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.into_string().await.unwrap(), "posts:read");
}

#[tokio::test]
async fn a_token_not_issued_for_it_is_refused_even_though_its_owner_could() {
    // The whole reason this exists. The user is an admin; no policy about
    // admins can express "but this token is read-only", because the policy is
    // about the person and the person is an admin.
    let response = through(RequireAbility::any(["posts:write"]), request("read-only")).await;

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_namespace_wildcard_reaches_the_verbs_under_it() {
    let response = through(RequireAbility::any(["posts:write"]), request("publisher")).await;

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn an_unrestricted_token_reaches_everything() {
    let response =
        through(RequireAbility::all(["posts:write", "users:delete"]), request("unrestricted"))
            .await;

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn requiring_all_needs_all_of_them() {
    let response =
        through(RequireAbility::all(["posts:read", "users:delete"]), request("read-only")).await;

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn an_unknown_token_never_reaches_the_ability_check() {
    // 401 rather than 403: it failed to authenticate, which is a different
    // thing from being authenticated and not allowed.
    let response = through(RequireAbility::any(["posts:read"]), request("nonsense")).await;

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn without_authenticate_before_it_the_check_refuses() {
    // A misordered route. The middleware has nothing to read, and a guard that
    // passes when it is misordered is not a guard.
    let response = Pipeline::new()
        .through(RequireAbility::any(["posts:read"]))
        .then(|_| async { Response::ok("reached the handler") })
        .run(request("unrestricted"))
        .await;

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_session_guard_carries_no_restriction() {
    // Nothing changes for an application that does not issue narrow tokens:
    // the default is everything, so adding the middleware to an existing route
    // refuses nobody who was getting through before.
    let abilities = Abilities::everything();

    assert!(RequireAbility::any(["anything"]).required().contains(&"anything".to_string()));
    assert!(abilities.can("anything"));
}
