//! Acting as another user — [`Impersonation`].
//!
//! ```ignore
//! let impersonation = Arc::new(
//!     Impersonation::new(|staff: &User, target: &User| staff.is_support() && !target.is_staff())
//!         .on_taken(|staff, target| tracing::warn!(staff, target, "impersonation started"))
//!         .on_left(|staff, target| tracing::warn!(staff, target, "impersonation ended")),
//! );
//!
//! // POST /support/users/{user}/impersonate
//! impersonation.take(&request, &staff, &target)?;
//!
//! // POST /support/impersonation/leave
//! impersonation.leave(&request)?;
//!
//! // Routes that must never run as somebody else.
//! router.put("/user/password", update).middleware(ForbidImpersonation);
//! ```
//!
//! Support staff reproducing what a customer sees is the reason this exists,
//! and it is a loaded tool: for its duration, every request in that session
//! **is** the customer as far as every guard and policy can tell. So the
//! defaults are the conservative ones.
//!
//! # What it refuses
//!
//! - **Nobody, unless you say who.** [`Impersonation::new`] takes the policy;
//!   there is no constructor that allows everyone.
//! - **Impersonating yourself**, which would leave a session whose "real" user
//!   is the one it is already logged in as.
//! - **Nesting.** Taking a second identity while holding one would make
//!   [`leave`](Impersonation::leave) restore the wrong person — the first
//!   *target* rather than the member of staff — and that person would then
//!   be logged in as themselves, with staff's session.
//! - **Taking it from a session that is not the impersonator's.** The
//!   impersonator passed in must be the user this session is logged in as;
//!   otherwise a handler that loaded the wrong row would hand one customer's
//!   session to another.
//!
//! # The session id rotates both ways
//!
//! On taking and on leaving — the same reason a login rotates it. A session id
//! that was a customer's for ten minutes should not become staff's again, and
//! vice versa.
//!
//! # What it does not do
//!
//! It does not *audit*. [`on_taken`](Impersonation::on_taken) and
//! [`on_left`](Impersonation::on_left) are where an application records who
//! looked at whose account and for how long — a log line at minimum, a table
//! if anybody will ever ask. And it does not decide what an impersonated
//! session may do: [`ForbidImpersonation`] marks the routes (password and
//! email changes, payment methods, deleting the account, issuing API tokens)
//! that act with the customer's authority rather than merely showing what
//! they see.

use std::sync::Arc;

use rainier_http::{IntoResponse, Request, Response};
use rainier_middleware::{Middleware, Next};
use rainier_session::{Session, SessionRequestExt};
use rainier_support::{Error, Result};

use crate::session::AUTH_KEY;
use crate::user::Authenticatable;

/// The session key holding the real user's identifier while impersonating.
pub const IMPERSONATOR_KEY: &str = "_auth_impersonator";

type Policy<U> = dyn Fn(&U, &U) -> bool + Send + Sync;
type Hook = dyn Fn(&str, &str) + Send + Sync;

/// Starts and ends impersonation on a session-authenticated request.
///
/// Works on the same session key [`SessionGuard`](crate::SessionGuard) reads,
/// so every guard and policy downstream sees the target.
pub struct Impersonation<U: Authenticatable> {
    may: Arc<Policy<U>>,
    taken: Option<Arc<Hook>>,
    left: Option<Arc<Hook>>,
}

impl<U: Authenticatable> Impersonation<U> {
    /// Impersonation allowed when `may(impersonator, target)` says so.
    ///
    /// Both halves of the usual policy live here — whether this member of
    /// staff may impersonate at all, and whether this target may be
    /// impersonated (other staff usually may not).
    pub fn new(may: impl Fn(&U, &U) -> bool + Send + Sync + 'static) -> Self {
        Self { may: Arc::new(may), taken: None, left: None }
    }

    /// Called with `(impersonator_id, target_id)` once impersonation starts.
    pub fn on_taken(mut self, hook: impl Fn(&str, &str) + Send + Sync + 'static) -> Self {
        self.taken = Some(Arc::new(hook));
        self
    }

    /// Called with `(impersonator_id, target_id)` once it ends.
    pub fn on_left(mut self, hook: impl Fn(&str, &str) + Send + Sync + 'static) -> Self {
        self.left = Some(Arc::new(hook));
        self
    }

    /// Whether the policy allows `impersonator` to become `target` — for
    /// showing or hiding the button. [`take`](Self::take) checks it again.
    pub fn can(&self, impersonator: &U, target: &U) -> bool {
        impersonator.auth_identifier() != target.auth_identifier()
            && (self.may)(impersonator, target)
    }

    /// Log this request's session in as `target`, remembering `impersonator`.
    ///
    /// # Errors
    ///
    /// `403` when the policy refuses or the impersonator is the target, `409`
    /// when the session is already impersonating, `401` when the session is
    /// not logged in as `impersonator`, `500` without the session middleware.
    pub fn take(&self, request: &Request, impersonator: &U, target: &U) -> Result<()> {
        let session = session_of(request)?;
        let impersonator_id = impersonator.auth_identifier();
        let target_id = target.auth_identifier();

        if session.has(IMPERSONATOR_KEY) {
            return Err(Error::conflict(
                "already impersonating; leave the current impersonation first",
            ));
        }
        if session.string(AUTH_KEY).as_deref() != Some(impersonator_id.as_str()) {
            return Err(Error::unauthenticated(
                "the impersonator must be the user this session is logged in as",
            ));
        }
        if impersonator_id == target_id {
            return Err(Error::unauthorized("you cannot impersonate yourself"));
        }
        if !(self.may)(impersonator, target) {
            return Err(Error::unauthorized("you may not impersonate this user"));
        }

        session.regenerate();
        session.put(IMPERSONATOR_KEY, &impersonator_id)?;
        session.put(AUTH_KEY, &target_id)?;

        if let Some(hook) = &self.taken {
            hook(&impersonator_id, &target_id);
        }
        Ok(())
    }

    /// End the impersonation and log the session back in as the real user,
    /// whose identifier is returned.
    ///
    /// # Errors
    ///
    /// `409` when the session is not impersonating anybody.
    pub fn leave(&self, request: &Request) -> Result<String> {
        let session = session_of(request)?;
        let Some(impersonator_id) = session.string(IMPERSONATOR_KEY) else {
            return Err(Error::conflict("this session is not impersonating anybody"));
        };
        let target_id = session.string(AUTH_KEY).unwrap_or_default();

        session.regenerate();
        session.forget(IMPERSONATOR_KEY);
        session.put(AUTH_KEY, &impersonator_id)?;

        if let Some(hook) = &self.left {
            hook(&impersonator_id, &target_id);
        }
        Ok(impersonator_id)
    }
}

impl<U: Authenticatable> std::fmt::Debug for Impersonation<U> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Impersonation").finish_non_exhaustive()
    }
}

/// Whether this request's session is impersonating somebody.
pub fn is_impersonating(request: &Request) -> bool {
    impersonator_id(request).is_some()
}

/// The real user behind an impersonated session.
pub fn impersonator_id(request: &Request) -> Option<String> {
    request.session().and_then(|session| session.string(IMPERSONATOR_KEY))
}

fn session_of(request: &Request) -> Result<Session> {
    request
        .session()
        .cloned()
        .ok_or_else(|| Error::internal("impersonation needs the session middleware"))
}

/// Refuses a route while the session is impersonating — `403`.
///
/// For whatever acts *with* the target's authority rather than showing what
/// they see: changing their password or email, their payment methods, issuing
/// tokens in their name, deleting their account.
#[derive(Debug, Default, Clone, Copy)]
pub struct ForbidImpersonation;

#[async_trait::async_trait]
impl Middleware for ForbidImpersonation {
    async fn handle(&self, request: Request, next: Next) -> Response {
        if is_impersonating(&request) {
            return Error::unauthorized("this is not available while impersonating another user")
                .into_response();
        }
        next.run(request).await
    }

    fn name(&self) -> &'static str {
        "ForbidImpersonation"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_http::StatusCode;
    use rainier_middleware::Pipeline;
    use rainier_support::ErrorKind;
    use std::sync::Mutex;

    #[derive(Debug, Clone)]
    struct User {
        id: u64,
        staff: bool,
    }

    impl Authenticatable for User {
        fn auth_identifier(&self) -> String {
            self.id.to_string()
        }
        fn auth_password_hash(&self) -> Option<&str> {
            None
        }
    }

    const STAFF: User = User { id: 1, staff: true };
    const OTHER_STAFF: User = User { id: 2, staff: true };
    const CUSTOMER: User = User { id: 7, staff: false };

    fn logged_in_as(user: &User) -> (Request, Session) {
        let session = Session::new();
        session.put(AUTH_KEY, user.auth_identifier()).unwrap();
        (Request::builder().build().with_extension(session.clone()), session)
    }

    fn policy() -> Impersonation<User> {
        Impersonation::new(|staff: &User, target: &User| staff.staff && !target.staff)
    }

    #[test]
    fn taking_and_leaving_swaps_the_identity_and_rotates_the_session_both_times() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let (taken, left) = (Arc::clone(&log), Arc::clone(&log));
        let impersonation = policy()
            .on_taken(move |a, b| taken.lock().unwrap().push(format!("take {a}->{b}")))
            .on_left(move |a, b| left.lock().unwrap().push(format!("leave {a}<-{b}")));
        let (request, session) = logged_in_as(&STAFF);

        let before = session.id();
        impersonation.take(&request, &STAFF, &CUSTOMER).unwrap();
        assert_eq!(session.string(AUTH_KEY).as_deref(), Some("7"));
        assert_eq!(impersonator_id(&request).as_deref(), Some("1"));
        assert!(is_impersonating(&request));
        let during = session.id();
        assert_ne!(before, during);

        assert_eq!(impersonation.leave(&request).unwrap(), "1");
        assert_eq!(session.string(AUTH_KEY).as_deref(), Some("1"));
        assert!(!is_impersonating(&request));
        assert_ne!(session.id(), during);

        assert_eq!(*log.lock().unwrap(), vec!["take 1->7", "leave 1<-7"]);
    }

    #[test]
    fn the_policy_decides_and_there_is_no_default_that_allows_everyone() {
        let (request, _) = logged_in_as(&STAFF);
        let err = policy().take(&request, &STAFF, &OTHER_STAFF).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Unauthorized);

        let (request, _) = logged_in_as(&CUSTOMER);
        let err = policy().take(&request, &CUSTOMER, &STAFF).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Unauthorized);
        assert!(!policy().can(&CUSTOMER, &CUSTOMER));
        assert!(policy().can(&STAFF, &CUSTOMER));
    }

    #[test]
    fn yourself_and_nesting_are_refused() {
        let everyone = Impersonation::new(|_: &User, _: &User| true);
        let (request, _) = logged_in_as(&STAFF);
        assert_eq!(
            everyone.take(&request, &STAFF, &STAFF).unwrap_err().kind(),
            ErrorKind::Unauthorized
        );

        everyone.take(&request, &STAFF, &CUSTOMER).unwrap();
        // The session is now the customer's; even the "right" impersonator
        // cannot stack a second identity on it.
        let err = everyone.take(&request, &CUSTOMER, &OTHER_STAFF).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Conflict);
    }

    #[test]
    fn the_impersonator_must_be_who_the_session_is_logged_in_as() {
        let (request, session) = logged_in_as(&CUSTOMER);
        let err = policy().take(&request, &STAFF, &User { id: 8, staff: false }).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Unauthenticated);
        assert_eq!(session.string(AUTH_KEY).as_deref(), Some("7"), "nothing changed");
    }

    #[test]
    fn leaving_when_not_impersonating_and_missing_sessions_are_errors() {
        let (request, _) = logged_in_as(&STAFF);
        assert_eq!(policy().leave(&request).unwrap_err().kind(), ErrorKind::Conflict);

        let bare = Request::builder().build();
        assert!(policy().take(&bare, &STAFF, &CUSTOMER).is_err());
        assert!(!is_impersonating(&bare));
    }

    #[tokio::test]
    async fn forbid_impersonation_blocks_only_impersonated_sessions() {
        let run = |request: Request| async move {
            Pipeline::new()
                .through(ForbidImpersonation)
                .then(|_: Request| async { Response::text("changed") })
                .run(request)
                .await
        };

        let (request, _) = logged_in_as(&CUSTOMER);
        assert_eq!(run(request).await.status(), StatusCode::OK);

        let (request, session) = logged_in_as(&STAFF);
        policy().take(&request, &STAFF, &CUSTOMER).unwrap();
        let impersonated = Request::builder().build().with_extension(session);
        assert_eq!(run(impersonated).await.status(), StatusCode::FORBIDDEN);
    }
}
