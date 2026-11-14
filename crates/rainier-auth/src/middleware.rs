//! The [`Authenticate`] middleware — `->middleware("auth")`.
//!
//! Resolves the user once, puts them in the request's extensions, and rejects
//! the request if there is nobody there. Doing it in middleware rather than in
//! each controller means the guard runs exactly once per request, and a handler
//! that is behind `auth` can read [`AuthenticatedUser`] without re-checking.

use std::sync::Arc;

use rainier_events::report::{report, AuthFailed};
use rainier_events::Exception;
use rainier_http::OriginalError;
use rainier_http::{FromRequest, IntoResponse, Request, Response};
use rainier_middleware::{Middleware, MiddlewareStack, Next};
use rainier_support::{BoxedFuture, Error, Result};

use crate::abilities::Abilities;
use crate::guard::AuthManager;
use crate::user::Authenticatable;

/// The user resolved for this request, stored in its extensions.
///
/// A newtype rather than a bare `U` so it cannot be confused with any other
/// value of the same type an application might attach.
#[derive(Debug)]
pub struct AuthenticatedUser<U>(pub Arc<U>);

/// Cloning shares the user rather than copying it, so this holds whatever `U`
/// is — a derived `Clone` would have demanded `U: Clone` for no reason.
impl<U> Clone for AuthenticatedUser<U> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<U> AuthenticatedUser<U> {
    /// The user.
    pub fn get(&self) -> &U {
        &self.0
    }
}

impl<U> std::ops::Deref for AuthenticatedUser<U> {
    type Target = U;

    fn deref(&self) -> &U {
        &self.0
    }
}

/// The authenticated user, as a **controller parameter**.
///
/// The framework supplies it, which is why an action can read
///
/// ```ignore
/// pub async fn me(user: AuthenticatedUser<User>) -> Result<Response> {
///     Ok(Response::json(user.get()))
/// }
/// ```
///
/// rather than taking the whole request and digging the user out of it.
///
/// Fails with `401` when there is none — which can only happen on a route with
/// no [`Authenticate`] middleware in front, so the message says so rather than
/// blaming the caller for a wiring mistake.
impl<U: Send + Sync + 'static> FromRequest for AuthenticatedUser<U> {
    fn from_request(request: Arc<Request>) -> BoxedFuture<Result<Self>> {
        Box::pin(async move {
            request
                .extension::<AuthenticatedUser<U>>()
                .cloned()
                .ok_or_else(|| Error::unauthenticated("Unauthenticated."))
        })
    }
}

/// Rejects unauthenticated requests, and attaches the user to authenticated
/// ones.
pub struct Authenticate<U: Authenticatable> {
    auth: Arc<AuthManager<U>>,
    /// The guard to use, or `None` for the manager's default — this is the
    /// `api` in `->middleware("auth:api")`.
    guard: Option<String>,
}

impl<U: Authenticatable> Authenticate<U> {
    /// Authenticate with the manager's default guard.
    pub fn new(auth: Arc<AuthManager<U>>) -> Self {
        Self { auth, guard: None }
    }

    /// Authenticate with a named guard.
    pub fn with_guard(auth: Arc<AuthManager<U>>, guard: impl Into<String>) -> Self {
        Self { auth, guard: Some(guard.into()) }
    }

    /// A stack that builds this middleware from the container.
    ///
    /// Routes are declared before the container is populated, so the
    /// `AuthManager` cannot be handed over at declaration time. This defers the
    /// resolve to when the router compiles:
    ///
    /// ```ignore
    /// router.get("/me", me).middleware(Authenticate::<User>::resolved());
    /// ```
    ///
    /// The `auth` middleware, attached by value. The user type is
    /// named explicitly, which is the thing a string alias could never say.
    pub fn resolved() -> MiddlewareStack {
        MiddlewareStack::new().resolved(|auth: Arc<AuthManager<U>>| Self::new(auth))
    }

    /// The guard this middleware asks, by name, for a report.
    fn guard_name(&self) -> String {
        self.guard.clone().unwrap_or_else(|| self.auth.default_name().to_string())
    }

    /// [`resolved`](Self::resolved) with a named guard — `->middleware('auth:api')`.
    pub fn resolved_with_guard(guard: impl Into<String>) -> MiddlewareStack {
        let guard = guard.into();
        MiddlewareStack::new()
            .resolved(move |auth: Arc<AuthManager<U>>| Self::with_guard(auth, guard.clone()))
    }
}

/// Who [`Authenticate`] let through, left on the **response**.
///
/// For code outside the route that wants to know who a request was for after
/// it was handled — a diagnostics recorder, an access log — without asking the
/// guard again. The request, where [`AuthenticatedUser`] lives, has been
/// consumed by then.
///
/// Deliberately not a span field: spans are exported, and who made a request
/// is not something to hand an OpenTelemetry backend by default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Authenticated {
    /// The guard that authenticated the request.
    pub guard: String,
    /// The user's [`auth_identifier`](Authenticatable::auth_identifier).
    pub user_id: String,
}

#[async_trait::async_trait]
impl<U: Authenticatable> Middleware for Authenticate<U> {
    async fn handle(&self, mut request: Request, next: Next) -> Response {
        let resolved = match &self.guard {
            Some(name) => self.auth.user_via(name, &request).await,
            None => self.auth.user(&request).await,
        };

        match resolved {
            Ok(Some(user)) => {
                // The abilities as well as the identity: a route behind
                // `auth` may still need to know the *token* was issued for
                // less than its owner can do. Resolved here rather than in
                // `RequireAbility`, so the guard is asked once.
                let abilities = match &self.guard {
                    Some(name) => self.auth.abilities_via(name, &request).await,
                    None => self.auth.abilities(&request).await,
                };

                match abilities {
                    Ok(abilities) => {
                        request.extensions_mut().insert(TokenAbilities(abilities));
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "could not read the token's abilities");
                        return report_and_render(e, self.guard_name(), "abilities").await;
                    }
                }

                let identified =
                    Authenticated { guard: self.guard_name(), user_id: user.auth_identifier() };
                request.extensions_mut().insert(AuthenticatedUser(Arc::new(user)));
                let mut response = next.run(request).await;
                // The request is gone by the time anything outside the route
                // sees the outcome; the response is what survives.
                response.extensions_mut().insert(identified);
                response
            }
            Ok(None) => Error::unauthenticated("Unauthenticated.").into_response(),
            // A guard failure is a 500, not a 401: the user may well be
            // authenticated, and telling them they are not would be a lie that
            // sends them to a login page which will also fail.
            Err(e) => {
                tracing::error!(error = %e, "the authentication guard failed");
                report_and_render(e, self.guard_name(), "user").await
            }
        }
    }

    fn name(&self) -> &'static str {
        "Authenticate"
    }
}

/// What the token behind this request was issued to do.
///
/// Put on the request by [`Authenticate`], so a handler or a later middleware
/// can read it without asking the guard again.
///
/// **Absent means unauthenticated**, not unrestricted. A route that has not
/// been through `auth` has no abilities on it at all, and
/// [`RequireAbility`] refuses rather than assuming.
#[derive(Debug, Clone)]
pub struct TokenAbilities(pub Abilities);

impl std::ops::Deref for TokenAbilities {
    type Target = Abilities;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[async_trait::async_trait]
impl FromRequest for TokenAbilities {
    fn from_request(request: Arc<Request>) -> BoxedFuture<Result<Self>> {
        Box::pin(async move {
            request.extension::<TokenAbilities>().cloned().ok_or_else(|| {
                Error::internal(
                    "no token abilities on the request — is this route behind the `auth` \
                     middleware?",
                )
            })
        })
    }
}

/// Reading the abilities off a request.
pub trait AbilitiesRequestExt {
    /// What this request's token may do, if it went through [`Authenticate`].
    fn token_abilities(&self) -> Option<&Abilities>;

    /// Whether this request's token may `ability`.
    ///
    /// `false` for a request that never authenticated — see
    /// [`TokenAbilities`].
    fn token_can(&self, ability: &str) -> bool;
}

impl AbilitiesRequestExt for Request {
    fn token_abilities(&self) -> Option<&Abilities> {
        self.extension::<TokenAbilities>().map(|abilities| &abilities.0)
    }

    fn token_can(&self, ability: &str) -> bool {
        self.token_abilities().is_some_and(|abilities| abilities.can(ability))
    }
}

/// Refuses a request whose token was not issued for what it is asking.
///
/// ```ignore
/// router.post("/api/posts", store)
///     .middleware((Authenticate::<User>::resolved(), RequireAbility::any(["posts:write"])));
/// ```
///
/// Order matters: it must run **after** [`Authenticate`], which is what puts
/// the abilities on the request. Before it, there is nothing to read and this
/// refuses everything — loudly rather than silently, because a guard that
/// passes when it is misordered is not a guard.
#[derive(Debug, Clone)]
pub struct RequireAbility {
    required: Vec<String>,
    /// Whether every ability is needed, or any one of them.
    all: bool,
}

impl RequireAbility {
    /// Require **any one** of these.
    ///
    /// The usual shape: several ability names that each permit this endpoint.
    pub fn any(abilities: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self { required: abilities.into_iter().map(Into::into).collect(), all: false }
    }

    /// Require **all** of these.
    pub fn all(abilities: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self { required: abilities.into_iter().map(Into::into).collect(), all: true }
    }

    /// What it requires.
    pub fn required(&self) -> &[String] {
        &self.required
    }

    fn satisfied_by(&self, abilities: &Abilities) -> bool {
        let required = self.required.iter().map(String::as_str);

        if self.all {
            abilities.can_all(required)
        } else {
            abilities.can_any(required)
        }
    }
}

#[async_trait::async_trait]
impl Middleware for RequireAbility {
    async fn handle(&self, request: Request, next: Next) -> Response {
        let Some(abilities) = request.token_abilities() else {
            // Misordered, or the route is not behind `auth` at all. Either
            // way this cannot make a decision, and the safe answer is no.
            tracing::error!(
                required = ?self.required,
                "`RequireAbility` ran without `Authenticate` before it, so the request has no \
                 abilities to check — refusing"
            );
            report(AuthFailed::new(
                Exception::message(
                    rainier_support::ErrorKind::Internal,
                    format!(
                        "`RequireAbility` ran without `Authenticate` before it (required: {})",
                        self.required.join(", ")
                    ),
                ),
                "abilities",
                "require_ability",
            ))
            .await;
            return Error::unauthorized("This action is unauthorized.").into_response();
        };

        if !self.satisfied_by(abilities) {
            return Error::unauthorized(format!(
                "This token is not allowed to {}.",
                self.required.join(" or ")
            ))
            .into_response();
        }

        next.run(request).await
    }

    fn name(&self) -> &'static str {
        "RequireAbility"
    }
}

/// Report a guard's error as an [`AuthFailed`], and render it as the
/// response.
///
/// The error becomes the response — the kernel reports that as a failed
/// request too — but only this report says it was the *guard* that failed,
/// and which one. Rendered first, so the report can share the whole error the
/// response keeps in its extensions rather than a copy of its message.
async fn report_and_render(error: Error, guard: String, operation: &str) -> Response {
    let fallback = Exception::message(error.kind(), error.message());
    let response = error.into_response();
    let exception = match response.extensions().get::<OriginalError>() {
        Some(OriginalError(original)) => Exception::from_shared(Arc::clone(original)),
        None => fallback,
    };
    report(AuthFailed::new(exception, guard, operation)).await;
    response
}

/// Rejects *authenticated* requests — the `guest` middleware, for a login page
/// an already-logged-in user should not see.
pub struct RedirectIfAuthenticated<U: Authenticatable> {
    auth: Arc<AuthManager<U>>,
    to: String,
}

impl<U: Authenticatable> RedirectIfAuthenticated<U> {
    /// Send authenticated users to `to`.
    pub fn new(auth: Arc<AuthManager<U>>, to: impl Into<String>) -> Self {
        Self { auth, to: to.into() }
    }
}

#[async_trait::async_trait]
impl<U: Authenticatable> Middleware for RedirectIfAuthenticated<U> {
    async fn handle(&self, request: Request, next: Next) -> Response {
        match self.auth.user(&request).await {
            Ok(Some(_)) => rainier_http::Redirect::to(self.to.clone()).into_response(),
            // A guard failure here is not a reason to block a login attempt —
            // failing open to the login page is the safe direction.
            Ok(None) => next.run(request).await,
            Err(e) => {
                tracing::warn!(error = %e, "the authentication guard failed; treating as a guest");
                let guard = self.auth.default_name().to_string();
                report(AuthFailed::new(e, guard, "redirect_if_authenticated")).await;
                next.run(request).await
            }
        }
    }

    fn name(&self) -> &'static str {
        "RedirectIfAuthenticated"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guard::TokenGuard;
    use crate::user::{Credentials, UserProvider};
    use rainier_http::StatusCode;
    use rainier_middleware::Pipeline;
    use rainier_support::Result;

    #[derive(Debug, Clone, PartialEq)]
    struct User {
        id: u64,
    }

    impl Authenticatable for User {
        fn auth_identifier(&self) -> String {
            self.id.to_string()
        }
        fn auth_password_hash(&self) -> Option<&str> {
            None
        }
    }

    struct Provider {
        fail: bool,
    }

    #[async_trait::async_trait]
    impl UserProvider<User> for Provider {
        async fn retrieve_by_id(&self, _: &str) -> Result<Option<User>> {
            Ok(None)
        }
        async fn retrieve_by_credentials(&self, _: &Credentials) -> Result<Option<User>> {
            Ok(None)
        }
        async fn validate_credentials(&self, _: &User, _: &Credentials) -> Result<bool> {
            Ok(false)
        }
        async fn retrieve_by_token(&self, token: &str) -> Result<Option<User>> {
            if self.fail {
                return Err(Error::internal("database is down"));
            }
            Ok((token == "good").then_some(User { id: 7 }))
        }
    }

    fn manager(fail: bool) -> Arc<AuthManager<User>> {
        Arc::new(
            AuthManager::new("api")
                .register(Arc::new(TokenGuard::new("api", Arc::new(Provider { fail })))),
        )
    }

    async fn reveal_user(request: Request) -> Response {
        match request.extension::<AuthenticatedUser<User>>() {
            Some(user) => Response::text(user.id.to_string()),
            None => Response::text("anonymous"),
        }
    }

    async fn body_of(response: Response) -> String {
        String::from_utf8(response.into_http().into_body().collect().await.unwrap().to_vec())
            .unwrap()
    }

    fn bearer(token: &str) -> Request {
        Request::builder().header("authorization", &format!("Bearer {token}")).build()
    }

    #[tokio::test]
    async fn an_authenticated_request_reaches_the_handler_with_its_user() {
        let response = Pipeline::new()
            .through(Authenticate::new(manager(false)))
            .then(reveal_user)
            .run(bearer("good"))
            .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_of(response).await, "7");
    }

    #[tokio::test]
    async fn the_response_says_who_was_authenticated_and_by_which_guard() {
        let response = Pipeline::new()
            .through(Authenticate::new(manager(false)))
            .then(reveal_user)
            .run(bearer("good"))
            .await;
        assert_eq!(
            response.extensions().get::<Authenticated>(),
            Some(&Authenticated { guard: "api".into(), user_id: "7".into() })
        );

        let rejected = Pipeline::new()
            .through(Authenticate::new(manager(false)))
            .then(reveal_user)
            .run(bearer("bad"))
            .await;
        assert!(rejected.extensions().get::<Authenticated>().is_none());
    }

    #[tokio::test]
    async fn an_unauthenticated_request_is_rejected_before_the_handler() {
        let response = Pipeline::new()
            .through(Authenticate::new(manager(false)))
            .then(|_: Request| async { panic!("the handler must not run") })
            .run(bearer("bad"))
            .await;

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_guard_failure_is_a_500_not_a_401() {
        // Telling an authenticated user they are not sends them to a login
        // page that will also fail. Report the outage instead.
        let response = Pipeline::new()
            .through(Authenticate::new(manager(true)))
            .then(reveal_user)
            .run(bearer("good"))
            .await;

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn a_named_guard_can_be_selected() {
        let middleware = Authenticate::with_guard(manager(false), "api");
        let response =
            Pipeline::new().through(middleware).then(reveal_user).run(bearer("good")).await;

        assert_eq!(body_of(response).await, "7");
    }

    #[tokio::test]
    async fn an_unknown_named_guard_fails_loudly() {
        let middleware = Authenticate::with_guard(manager(false), "nope");
        let response =
            Pipeline::new().through(middleware).then(reveal_user).run(bearer("good")).await;

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn the_guest_middleware_redirects_an_authenticated_user() {
        let response = Pipeline::new()
            .through(RedirectIfAuthenticated::new(manager(false), "/dashboard"))
            .then(|_: Request| async { Response::text("login form") })
            .run(bearer("good"))
            .await;

        assert_eq!(response.status(), StatusCode::FOUND);
        assert_eq!(response.header("location"), Some("/dashboard"));
    }

    #[tokio::test]
    async fn the_guest_middleware_lets_a_guest_through() {
        let response = Pipeline::new()
            .through(RedirectIfAuthenticated::new(manager(false), "/dashboard"))
            .then(|_: Request| async { Response::text("login form") })
            .run(Request::builder().build())
            .await;

        assert_eq!(body_of(response).await, "login form");
    }

    #[tokio::test]
    async fn the_guest_middleware_fails_open_to_the_login_page() {
        // If the guard is broken, blocking the login page helps nobody.
        let response = Pipeline::new()
            .through(RedirectIfAuthenticated::new(manager(true), "/dashboard"))
            .then(|_: Request| async { Response::text("login form") })
            .run(bearer("good"))
            .await;

        assert_eq!(body_of(response).await, "login form");
    }
}
