//! Guards — [`Guard`], [`TokenGuard`], [`SessionGuard`] and [`AuthManager`].
//!
//! A guard answers one question: *who is making this request?* How it answers
//! is the guard's business — a bearer token, a session cookie, a signed header
//! — and the rest of the application only sees the answer.
//!
//! Guards are **stateless with respect to the request**: `user(request)` reads
//! the request and returns, rather than caching "the current user" in the
//! guard. A guard is a shared singleton serving every concurrent request, so
//! a `current_user` field on it would be a race, not a convenience. The
//! resolved user is stashed in the request's own extensions instead, by the
//! [`Authenticate`](crate::Authenticate) middleware.

use std::collections::HashMap;
use std::sync::Arc;

use rainier_http::Request;
use rainier_session::SessionRequestExt;
use rainier_support::{BoxFuture, Error, Result};

use crate::abilities::Abilities;
use crate::session::AUTH_KEY;
use crate::user::{Authenticatable, Credentials, UserProvider};

/// Identifies the user behind a request.
pub trait Guard<U: Authenticatable>: Send + Sync + 'static {
    /// A label for diagnostics — `"session"`, `"api"`.
    fn name(&self) -> &str;

    /// The user this request belongs to, or `None` if it is unauthenticated.
    ///
    /// A *failure* to determine that (the database is down) is an `Err`;
    /// "nobody is logged in" is `Ok(None)`. Conflating the two would turn an
    /// outage into a silent mass logout.
    fn user<'a>(&'a self, request: &'a Request) -> BoxFuture<'a, Result<Option<U>>>;

    /// What the credential in this request was issued to do.
    ///
    /// Defaults to [`Abilities::everything`]: a session cookie carries no
    /// abilities, and neither does a token from a provider that does not
    /// implement them. Only [`TokenGuard`] answers anything narrower.
    fn abilities<'a>(&'a self, request: &'a Request) -> BoxFuture<'a, Result<Abilities>> {
        let _ = request;
        Box::pin(async { Ok(Abilities::everything()) })
    }
}

/// A guard that can also log a user in and out — a session, as opposed to a
/// stateless token.
///
/// Every method takes the request, because the session belongs to it. The
/// cookie is not this trait's business: writing it is
/// [`StartSession`](rainier_session::StartSession)'s job, and having two
/// things write one cookie is how they end up disagreeing.
pub trait StatefulGuard<U: Authenticatable>: Guard<U> {
    /// Verify credentials and, on success, log the user into this request's
    /// session. Returns the user, or `None` if the credentials were wrong.
    fn attempt<'a>(
        &'a self,
        request: &'a Request,
        credentials: &'a Credentials,
    ) -> BoxFuture<'a, Result<Option<U>>>;

    /// Log an already-verified user into this request's session.
    fn login<'a>(&'a self, request: &'a Request, user: &'a U) -> BoxFuture<'a, Result<()>>;

    /// End the session behind this request.
    fn logout<'a>(&'a self, request: &'a Request) -> BoxFuture<'a, Result<()>>;
}

/// Extension methods every guard gets. Separate from [`Guard`] so that trait
/// stays object-safe.
#[async_trait::async_trait]
pub trait GuardExt<U: Authenticatable>: Guard<U> {
    /// Whether the request is authenticated.
    async fn check(&self, request: &Request) -> Result<bool> {
        Ok(self.user(request).await?.is_some())
    }

    /// Whether the request is *not* authenticated.
    async fn guest(&self, request: &Request) -> Result<bool> {
        Ok(!self.check(request).await?)
    }

    /// The authenticated user's identifier.
    async fn id(&self, request: &Request) -> Result<Option<String>> {
        Ok(self.user(request).await?.map(|user| user.auth_identifier()))
    }

    /// The authenticated user, failing with a `401` if there is none.
    async fn authenticate(&self, request: &Request) -> Result<U> {
        self.user(request).await?.ok_or_else(|| Error::unauthenticated("Unauthenticated."))
    }
}

impl<U: Authenticatable, G: Guard<U> + ?Sized> GuardExt<U> for G {}

/// Authenticates by `Authorization: Bearer …`.
///
/// Stateless: nothing is stored, so there is nothing to log out of. Revoking a
/// token means changing it on the user row.
pub struct TokenGuard<U: Authenticatable> {
    name: String,
    provider: Arc<dyn UserProvider<U>>,
}

impl<U: Authenticatable> TokenGuard<U> {
    /// A token guard named `name`.
    pub fn new(name: impl Into<String>, provider: Arc<dyn UserProvider<U>>) -> Self {
        Self { name: name.into(), provider }
    }
}

impl<U: Authenticatable> Guard<U> for TokenGuard<U> {
    fn name(&self) -> &str {
        &self.name
    }

    fn user<'a>(&'a self, request: &'a Request) -> BoxFuture<'a, Result<Option<U>>> {
        Box::pin(async move {
            // Accept the token from `Authorization: Bearer` or an
            // `api_token` input — the two places an API client puts one.
            let token =
                request.bearer_token().map(str::to_string).or_else(|| request.input("api_token"));

            match token {
                Some(token) if !token.is_empty() => self.provider.retrieve_by_token(&token).await,
                _ => Ok(None),
            }
        })
    }

    fn abilities<'a>(&'a self, request: &'a Request) -> BoxFuture<'a, Result<Abilities>> {
        Box::pin(async move {
            let token =
                request.bearer_token().map(str::to_string).or_else(|| request.input("api_token"));

            match token {
                Some(token) if !token.is_empty() => {
                    self.provider.retrieve_abilities_by_token(&token).await
                }
                // No token, so nothing was issued and nothing is granted.
                // Reaching here means the guard already reported nobody.
                _ => Ok(Abilities::none()),
            }
        })
    }
}

/// Authenticates from the request's session.
///
/// Requires the [`StartSession`](rainier_session::StartSession) middleware to
/// have run — without it there is no session, and this guard reports nobody
/// rather than pretending.
pub struct SessionGuard<U: Authenticatable> {
    name: String,
    provider: Arc<dyn UserProvider<U>>,
}

impl<U: Authenticatable> SessionGuard<U> {
    /// A session guard named `name`.
    pub fn new(name: impl Into<String>, provider: Arc<dyn UserProvider<U>>) -> Self {
        Self { name: name.into(), provider }
    }

    /// The user provider this guard looks users up through.
    pub fn provider(&self) -> &Arc<dyn UserProvider<U>> {
        &self.provider
    }
}

impl<U: Authenticatable> Guard<U> for SessionGuard<U> {
    fn name(&self) -> &str {
        &self.name
    }

    fn user<'a>(&'a self, request: &'a Request) -> BoxFuture<'a, Result<Option<U>>> {
        Box::pin(async move {
            let Some(session) = request.session() else {
                // The route is not behind `StartSession`. Nobody is logged in
                // here, and saying so is better than a 500 — but it is almost
                // always a wiring mistake, so it is worth a line in the log.
                tracing::debug!(
                    guard = %self.name,
                    "a session guard ran on a route with no session middleware"
                );
                return Ok(None);
            };

            let Some(user_id) = session.string(AUTH_KEY) else {
                return Ok(None);
            };

            self.provider.retrieve_by_id(&user_id).await
        })
    }
}

impl<U: Authenticatable> StatefulGuard<U> for SessionGuard<U> {
    fn attempt<'a>(
        &'a self,
        request: &'a Request,
        credentials: &'a Credentials,
    ) -> BoxFuture<'a, Result<Option<U>>> {
        Box::pin(async move {
            let Some(user) = self.provider.retrieve_by_credentials(credentials).await? else {
                return Ok(None);
            };
            if !self.provider.validate_credentials(&user, credentials).await? {
                return Ok(None);
            }

            self.login(request, &user).await?;
            Ok(Some(user))
        })
    }

    fn login<'a>(&'a self, request: &'a Request, user: &'a U) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let session = request
                .session()
                .ok_or_else(|| Error::internal("logging in needs the session middleware"))?;

            // Before writing the identity, not after: an attacker who fixed
            // the pre-login session id must not end up holding a cookie for
            // the authenticated one.
            session.regenerate();
            session.put(AUTH_KEY, user.auth_identifier())?;
            Ok(())
        })
    }

    fn logout<'a>(&'a self, request: &'a Request) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if let Some(session) = request.session() {
                // Everything, not just the auth key: a cart or a half-filled
                // form left behind by a logout is the next person at that
                // browser reading it.
                session.invalidate();
            }
            Ok(())
        })
    }
}

/// Resolves named guards — `guard("api")` picks one by name.
///
/// One user type per manager. An application with genuinely different user
/// kinds (customers and staff, with different tables) wants one manager each,
/// which is also clearer than a single registry returning different shapes.
pub struct AuthManager<U: Authenticatable> {
    guards: HashMap<String, Arc<dyn Guard<U>>>,
    default: String,
}

impl<U: Authenticatable> AuthManager<U> {
    /// A manager whose default guard is `default`.
    pub fn new(default: impl Into<String>) -> Self {
        Self { guards: HashMap::new(), default: default.into() }
    }

    /// Register a guard under its own name.
    pub fn register(mut self, guard: Arc<dyn Guard<U>>) -> Self {
        self.guards.insert(guard.name().to_string(), guard);
        self
    }

    /// The guard named `name`.
    pub fn guard(&self, name: &str) -> Result<&Arc<dyn Guard<U>>> {
        self.guards.get(name).ok_or_else(|| {
            Error::internal(format!(
                "no authentication guard named `{name}` — registered guards: {:?}",
                self.guard_names()
            ))
        })
    }

    /// The default guard.
    pub fn default_guard(&self) -> Result<&Arc<dyn Guard<U>>> {
        self.guard(&self.default)
    }

    /// The default guard's name.
    pub fn default_name(&self) -> &str {
        &self.default
    }

    /// Every registered guard's name.
    pub fn guard_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.guards.keys().map(String::as_str).collect();
        names.sort();
        names
    }

    /// The user behind `request`, via the default guard.
    pub async fn user(&self, request: &Request) -> Result<Option<U>> {
        self.default_guard()?.user(request).await
    }

    /// The user behind `request`, via a named guard.
    pub async fn user_via(&self, name: &str, request: &Request) -> Result<Option<U>> {
        self.guard(name)?.user(request).await
    }

    /// What the credential in `request` was issued to do.
    pub async fn abilities(&self, request: &Request) -> Result<Abilities> {
        self.default_guard()?.abilities(request).await
    }

    /// [`abilities`](Self::abilities), via a named guard.
    pub async fn abilities_via(&self, name: &str, request: &Request) -> Result<Abilities> {
        self.guard(name)?.abilities(request).await
    }
}

impl<U: Authenticatable> std::fmt::Debug for AuthManager<U> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthManager")
            .field("default", &self.default)
            .field("guards", &self.guard_names())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Session;
    use rainier_support::Result as R;

    #[derive(Debug, Clone, PartialEq)]
    struct User {
        id: u64,
        email: String,
        password: String,
        token: Option<String>,
    }

    impl Authenticatable for User {
        fn auth_identifier(&self) -> String {
            self.id.to_string()
        }
        fn auth_password_hash(&self) -> Option<&str> {
            Some(&self.password)
        }
    }

    fn ada() -> User {
        User {
            id: 42,
            email: "ada@example.com".into(),
            password: "plain:secret".into(),
            token: Some("tok_123".into()),
        }
    }

    /// A provider over one in-memory user, verifying a `plain:` prefix rather
    /// than running a real hash — the hashing itself is tested elsewhere.
    struct FakeProvider {
        user: Option<User>,
        fail: bool,
    }

    impl FakeProvider {
        fn holding(user: User) -> Arc<dyn UserProvider<User>> {
            Arc::new(Self { user: Some(user), fail: false })
        }
        fn empty() -> Arc<dyn UserProvider<User>> {
            Arc::new(Self { user: None, fail: false })
        }
        fn broken() -> Arc<dyn UserProvider<User>> {
            Arc::new(Self { user: None, fail: true })
        }
    }

    #[async_trait::async_trait]
    impl UserProvider<User> for FakeProvider {
        async fn retrieve_by_id(&self, id: &str) -> R<Option<User>> {
            if self.fail {
                return Err(Error::internal("database is down"));
            }
            Ok(self.user.clone().filter(|u| u.auth_identifier() == id))
        }

        async fn retrieve_by_credentials(&self, credentials: &Credentials) -> R<Option<User>> {
            if self.fail {
                return Err(Error::internal("database is down"));
            }
            Ok(self.user.clone().filter(|u| credentials.get("email") == Some(&u.email)))
        }

        async fn validate_credentials(&self, user: &User, credentials: &Credentials) -> R<bool> {
            Ok(credentials.password_value().is_some_and(|p| user.password == format!("plain:{p}")))
        }

        async fn retrieve_by_token(&self, token: &str) -> R<Option<User>> {
            if self.fail {
                return Err(Error::internal("database is down"));
            }
            Ok(self.user.clone().filter(|u| u.token.as_deref() == Some(token)))
        }
    }

    fn bearer(token: &str) -> Request {
        Request::builder().header("authorization", &format!("Bearer {token}")).build()
    }

    // --- token guard -------------------------------------------------------

    #[tokio::test]
    async fn a_token_guard_resolves_a_bearer_token() {
        let guard = TokenGuard::new("api", FakeProvider::holding(ada()));

        let user = guard.user(&bearer("tok_123")).await.unwrap();
        assert_eq!(user.unwrap().id, 42);
        assert!(guard.check(&bearer("tok_123")).await.unwrap());
        assert_eq!(guard.id(&bearer("tok_123")).await.unwrap().as_deref(), Some("42"));
    }

    #[tokio::test]
    async fn a_token_guard_rejects_an_unknown_or_absent_token() {
        let guard = TokenGuard::new("api", FakeProvider::holding(ada()));

        assert!(guard.user(&bearer("wrong")).await.unwrap().is_none());
        assert!(guard.user(&Request::builder().build()).await.unwrap().is_none());
        assert!(guard.guest(&Request::builder().build()).await.unwrap());
    }

    #[tokio::test]
    async fn a_token_guard_also_accepts_an_api_token_input() {
        let guard = TokenGuard::new("api", FakeProvider::holding(ada()));
        let request = Request::builder().uri("/x?api_token=tok_123").build();

        assert!(guard.check(&request).await.unwrap());
    }

    #[tokio::test]
    async fn authenticate_fails_with_a_401_for_a_guest() {
        let guard = TokenGuard::new("api", FakeProvider::empty());
        let err = guard.authenticate(&Request::builder().build()).await.unwrap_err();
        assert_eq!(err.status(), 401);
    }

    #[tokio::test]
    async fn a_backend_failure_is_an_error_not_a_silent_logout() {
        // The distinction that matters: an outage must not read as "everyone
        // is a guest", which would quietly drop authorisation everywhere.
        let guard = TokenGuard::new("api", FakeProvider::broken());
        assert!(guard.user(&bearer("tok_123")).await.is_err());
        assert!(guard.check(&bearer("tok_123")).await.is_err());
    }

    // --- session guard -----------------------------------------------------

    fn session_guard() -> SessionGuard<User> {
        SessionGuard::new("web", FakeProvider::holding(ada()))
    }

    /// A request carrying a session, as `StartSession` would have left it.
    fn with_session() -> Request {
        Request::builder().build().with_extension(Session::new())
    }

    #[tokio::test]
    async fn a_session_guard_logs_in_and_resolves_the_user() {
        let guard = session_guard();
        let request = with_session();

        let user = guard
            .attempt(&request, &Credentials::password("ada@example.com", "secret"))
            .await
            .unwrap()
            .expect("correct credentials should log in");

        assert_eq!(user.id, 42);
        assert_eq!(guard.user(&request).await.unwrap().unwrap().id, 42);
    }

    #[tokio::test]
    async fn logging_in_rotates_the_session_id() {
        // Session fixation: an attacker who set the pre-login cookie must not
        // be holding the authenticated session's id afterwards.
        let guard = session_guard();
        let request = with_session();
        let before = request.session().unwrap().id();

        guard.login(&request, &ada()).await.unwrap();

        assert_ne!(request.session().unwrap().id(), before);
    }

    #[tokio::test]
    async fn a_session_guard_rejects_a_wrong_password() {
        let guard = session_guard();
        let request = with_session();

        assert!(guard
            .attempt(&request, &Credentials::password("ada@example.com", "wrong"))
            .await
            .unwrap()
            .is_none());
        assert!(guard.guest(&request).await.unwrap(), "a failed attempt must not log anyone in");
    }

    #[tokio::test]
    async fn a_session_guard_rejects_an_unknown_user() {
        let guard = session_guard();
        let request = with_session();

        assert!(guard
            .attempt(&request, &Credentials::password("nobody@example.com", "secret"))
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn logging_out_empties_the_session_and_rotates_it() {
        let guard = session_guard();
        let request = with_session();
        guard.login(&request, &ada()).await.unwrap();

        let before = request.session().unwrap().id();
        request.session().unwrap().put("cart", vec![1u64]).unwrap();

        assert!(guard.check(&request).await.unwrap());
        guard.logout(&request).await.unwrap();

        assert!(guard.guest(&request).await.unwrap());
        assert!(
            !request.session().unwrap().has("cart"),
            "a logout must not leave the next person at this browser the cart"
        );
        assert_ne!(request.session().unwrap().id(), before);
    }

    #[tokio::test]
    async fn a_request_with_no_session_is_a_guest_rather_than_an_error() {
        // The route is not behind `StartSession`. Reporting nobody is the
        // right answer; a 500 would take down a page that merely forgot it.
        let guard = session_guard();
        assert!(guard.guest(&Request::builder().build()).await.unwrap());
    }

    #[tokio::test]
    async fn an_empty_session_is_a_guest() {
        let guard = session_guard();
        assert!(guard.guest(&with_session()).await.unwrap());
    }

    #[tokio::test]
    async fn logging_out_without_a_session_is_not_an_error() {
        let guard = session_guard();
        assert!(guard.logout(&Request::builder().build()).await.is_ok());
    }

    #[tokio::test]
    async fn logging_in_without_a_session_says_what_is_missing() {
        let guard = session_guard();
        let err = guard.login(&Request::builder().build(), &ada()).await.unwrap_err();

        assert!(err.message().contains("session middleware"), "{}", err.message());
    }

    // --- manager -----------------------------------------------------------

    #[tokio::test]
    async fn the_manager_resolves_guards_by_name() {
        let manager = AuthManager::<User>::new("web")
            .register(Arc::new(session_guard()))
            .register(Arc::new(TokenGuard::new("api", FakeProvider::holding(ada()))));

        assert_eq!(manager.guard_names(), vec!["api", "web"]);
        assert_eq!(manager.default_name(), "web");
        assert_eq!(manager.default_guard().unwrap().name(), "web");
        assert_eq!(manager.guard("api").unwrap().name(), "api");
    }

    #[tokio::test]
    async fn an_unknown_guard_name_lists_the_known_ones() {
        let manager = AuthManager::<User>::new("web").register(Arc::new(session_guard()));

        let err = manager.guard("nope").err().expect("should fail");
        assert!(err.message().contains("`nope`"), "{}", err.message());
        assert!(err.message().contains("web"), "{}", err.message());
    }

    #[tokio::test]
    async fn the_manager_authenticates_through_the_chosen_guard() {
        let manager = AuthManager::<User>::new("api")
            .register(Arc::new(TokenGuard::new("api", FakeProvider::holding(ada()))));

        let request = bearer("tok_123");
        assert_eq!(manager.user(&request).await.unwrap().unwrap().id, 42);
        assert_eq!(manager.user_via("api", &request).await.unwrap().unwrap().id, 42);
    }

    #[tokio::test]
    async fn a_missing_default_guard_is_reported_when_used() {
        let manager = AuthManager::<User>::new("web");
        assert!(manager.user(&Request::builder().build()).await.is_err());
    }
}
