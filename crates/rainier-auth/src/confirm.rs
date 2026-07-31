//! Re-asking for the password before something serious — [`ConfirmPassword`].
//!
//! ```ignore
//! // The gate.
//! router.post("/account/password", change_password)
//!     .middleware(ConfirmPassword::within(Duration::from_secs(900)));
//!
//! // And the endpoint that satisfies it.
//! router.post("/account/confirm-password", confirm_password);
//! ```
//!
//! **Not** the [`Confirmed`] validation rule,
//! which asserts `password == password_confirmation` inside one submitted form
//! — a different feature with a confusingly similar name, and the two get
//! mixed up often enough to be worth saying twice.
//!
//! # Why holding a session is not enough
//!
//! A session says somebody logged in as this account at some point. It does
//! not say the person at the keyboard *now* is them. Between those two facts
//! sit an unlocked laptop, a borrowed phone, a shared desk, and a session
//! token lifted from a machine somebody else has.
//!
//! So anything that would let an attacker keep the account — changing the
//! password, changing the address it recovers to, enrolling or removing a
//! second factor, issuing an API token — asks again. It is the difference
//! between "somebody used this laptop" and "somebody knows the password".
//!
//! An identity provider has more of these than most applications, which is why
//! writing it inline twice is how it usually goes.
//!
//! [`Confirmed`]: https://docs.rs/rainier-validation

use std::sync::Arc;
use std::time::Duration;

use rainier_http::{IntoResponse, Request, Response};
use rainier_middleware::{Middleware, MiddlewareStack, Next};
use rainier_session::SessionRequestExt;
use rainier_support::Error;

use crate::guard::AuthManager;
use crate::hashing::Hasher;
use crate::middleware::AuthenticatedUser;
use crate::user::Authenticatable;

/// Where the session records that the password was confirmed.
pub const CONFIRMED_AT: &str = "auth.password_confirmed_at";

/// Refuses until the password has been confirmed recently.
///
/// Answers `423 Locked` rather than `403`, so a client can tell "prove it is
/// you again" apart from "you may never do this". A browser application
/// redirects to its confirmation page; an API client asks for the password and
/// posts it to the confirm endpoint.
#[derive(Debug, Clone, Copy)]
pub struct ConfirmPassword {
    window: Duration,
}

impl ConfirmPassword {
    /// Accept a confirmation made within `window`.
    ///
    /// Fifteen minutes is the conventional default and a reasonable one: long enough
    /// that changing three settings does not mean typing the password three
    /// times, short enough that a walked-away-from laptop is not an open door.
    pub fn within(window: Duration) -> Self {
        Self { window }
    }

    /// Fifteen minutes.
    pub fn recently() -> Self {
        Self::within(Duration::from_secs(900))
    }

    /// The window.
    pub fn window(&self) -> Duration {
        self.window
    }

    /// Record a confirmation on this request's session.
    ///
    /// What the confirm endpoint calls once it has checked the password.
    /// Separate from the middleware so an application can confirm by some
    /// other proof — a passkey, a second factor — and reuse the same gate.
    pub fn mark_confirmed(request: &Request) -> Result<(), Error> {
        let session = request.session().ok_or_else(|| {
            Error::internal("confirming a password needs the `session` middleware")
        })?;

        session.put(CONFIRMED_AT, now())?;
        Ok(())
    }

    /// Forget any confirmation, so the next guarded action asks again.
    ///
    /// Worth calling when the stakes change — after a failed second factor, or
    /// when a session is adopted from somewhere new.
    pub fn forget(request: &Request) {
        if let Some(session) = request.session() {
            session.forget(CONFIRMED_AT);
        }
    }

    /// How long ago the password was confirmed on this request's session.
    pub fn confirmed_ago(request: &Request) -> Option<Duration> {
        let confirmed_at: i64 = request.session()?.get(CONFIRMED_AT)?;
        Some(Duration::from_secs(now().saturating_sub(confirmed_at).max(0) as u64))
    }

    /// Whether this request's session has a confirmation inside `window`.
    pub fn is_confirmed(&self, request: &Request) -> bool {
        Self::confirmed_ago(request).is_some_and(|ago| ago <= self.window)
    }
}

#[async_trait::async_trait]
impl Middleware for ConfirmPassword {
    async fn handle(&self, request: Request, next: Next) -> Response {
        if request.session().is_none() {
            // Nothing to read a confirmation from. Refusing is the only safe
            // answer: a guard that passes when it is misconfigured is not a
            // guard, and this one stands in front of the account-takeover
            // actions.
            tracing::error!(
                "`ConfirmPassword` ran without a session, so it cannot tell whether the password \
                 was confirmed — refusing. Is this route behind the `web` group?"
            );
            return Error::new(
                rainier_support::ErrorKind::Status(423),
                "Password confirmation is required.",
            )
            .into_response();
        }

        if self.is_confirmed(&request) {
            return next.run(request).await;
        }

        Error::new(rainier_support::ErrorKind::Status(423), "Please confirm your password.")
            .into_response()
    }

    fn name(&self) -> &'static str {
        "ConfirmPassword"
    }
}

/// Check a submitted password against the authenticated user, and record it.
///
/// The other half of [`ConfirmPassword`] — the endpoint its `423` is asking
/// the client to visit.
///
/// ```ignore
/// router.post("/account/confirm-password", |request: Req, Json(body): Json<Confirm>| async move {
///     confirm_password::<User>(&request, &body.password)?;
///     Ok(Response::no_content())
/// });
/// ```
///
/// # Errors
///
/// `401` when nobody is authenticated, `422` when the password is wrong. The
/// wrong password is deliberately **not** a `403`: it is a failed check of
/// submitted input, and rendering it as an authorization failure sends a
/// browser to a login page it does not need.
pub fn confirm_password<U: Authenticatable>(
    request: &Request,
    password: &str,
    hasher: &Arc<dyn Hasher>,
) -> Result<(), Error> {
    let user = request
        .extension::<AuthenticatedUser<U>>()
        .ok_or_else(|| Error::unauthenticated("Unauthenticated."))?;

    let Some(stored) = user.auth_password_hash() else {
        // An account that authenticates some other way has no password to
        // confirm, and pretending otherwise would lock it out of every
        // guarded action with a message about a password it does not have.
        return Err(Error::bad_request("This account does not sign in with a password."));
    };

    if !hasher.verify(password, stored) {
        return Err(Error::validation(serde_json::json!({
            "password": ["That password is not correct."],
        })));
    }

    ConfirmPassword::mark_confirmed(request)
}

/// [`ConfirmPassword`] as a stack, for symmetry with the rest of the auth
/// middleware.
pub fn confirm_password_within<U: Authenticatable>(window: Duration) -> MiddlewareStack {
    // The manager is not needed to *check* a confirmation — only to make one —
    // but resolving it here keeps a misconfigured application failing at boot
    // rather than at the first guarded request.
    MiddlewareStack::new().resolved(move |_: Arc<AuthManager<U>>| ConfirmPassword::within(window))
}

/// Seconds since the epoch.
fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_http::{Method, StatusCode};
    use rainier_middleware::Pipeline;
    use rainier_session::{MemorySessionStore, StartSession};

    fn with_session() -> StartSession {
        StartSession::new(Arc::new(MemorySessionStore::default()))
    }

    /// Runs `request` through a session and the guard, reporting the status.
    async fn through(guard: ConfirmPassword, confirm_first: bool) -> StatusCode {
        Pipeline::new()
            .through(with_session())
            .through(ConfirmFirst(confirm_first))
            .through(guard)
            .then(|_| async { Response::ok("through") })
            .run(Request::builder().method(Method::POST).uri("/account/password").build())
            .await
            .status()
    }

    /// Stands in for the confirm endpoint: marks the session confirmed.
    struct ConfirmFirst(bool);

    #[async_trait::async_trait]
    impl Middleware for ConfirmFirst {
        async fn handle(&self, request: Request, next: Next) -> Response {
            if self.0 {
                ConfirmPassword::mark_confirmed(&request).expect("a session");
            }
            next.run(request).await
        }

        fn name(&self) -> &'static str {
            "ConfirmFirst"
        }
    }

    #[tokio::test]
    async fn a_recent_confirmation_gets_through() {
        assert_eq!(through(ConfirmPassword::recently(), true).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn no_confirmation_is_refused_with_423() {
        // 423 rather than 403, so a client can tell "prove it is you again"
        // apart from "you may never do this".
        assert_eq!(through(ConfirmPassword::recently(), false).await, StatusCode::LOCKED);
    }

    #[tokio::test]
    async fn a_confirmation_older_than_the_window_is_refused() {
        // A zero-length window: anything already recorded is too old.
        assert_eq!(
            through(ConfirmPassword::within(Duration::ZERO), true).await,
            StatusCode::OK,
            "a confirmation made this instant is inside a zero window"
        );

        // And one made in the past is not.
        let guard = ConfirmPassword::within(Duration::from_secs(60));
        let request = Request::builder().method(Method::POST).uri("/x").build();

        let status = Pipeline::new()
            .through(with_session())
            .through(ConfirmedLongAgo)
            .through(guard)
            .then(|_| async { Response::ok("through") })
            .run(request)
            .await
            .status();

        assert_eq!(status, StatusCode::LOCKED);
    }

    /// Records a confirmation from an hour ago.
    struct ConfirmedLongAgo;

    #[async_trait::async_trait]
    impl Middleware for ConfirmedLongAgo {
        async fn handle(&self, request: Request, next: Next) -> Response {
            request.session().unwrap().put(CONFIRMED_AT, now() - 3600).unwrap();
            next.run(request).await
        }

        fn name(&self) -> &'static str {
            "ConfirmedLongAgo"
        }
    }

    #[tokio::test]
    async fn without_a_session_it_refuses_rather_than_passing() {
        // A misordered route. This guard stands in front of the actions that
        // let an attacker keep the account, so failing open is not an option.
        let status = Pipeline::new()
            .through(ConfirmPassword::recently())
            .then(|_| async { Response::ok("through") })
            .run(Request::builder().method(Method::POST).uri("/x").build())
            .await
            .status();

        assert_eq!(status, StatusCode::LOCKED);
    }

    #[tokio::test]
    async fn forgetting_a_confirmation_closes_the_window_early() {
        let status = Pipeline::new()
            .through(with_session())
            .through(ConfirmFirst(true))
            .through(ForgetIt)
            .through(ConfirmPassword::recently())
            .then(|_| async { Response::ok("through") })
            .run(Request::builder().method(Method::POST).uri("/x").build())
            .await
            .status();

        assert_eq!(status, StatusCode::LOCKED);
    }

    struct ForgetIt;

    #[async_trait::async_trait]
    impl Middleware for ForgetIt {
        async fn handle(&self, request: Request, next: Next) -> Response {
            ConfirmPassword::forget(&request);
            next.run(request).await
        }

        fn name(&self) -> &'static str {
            "ForgetIt"
        }
    }

    #[test]
    fn the_default_window_is_fifteen_minutes() {
        assert_eq!(ConfirmPassword::recently().window(), Duration::from_secs(900));
    }
}
