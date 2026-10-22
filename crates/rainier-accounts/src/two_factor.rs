//! TOTP two-factor authentication — the challenge after a right password, and
//! the routes that manage it. Behind the `two-factor` feature.
//!
//! ```ignore
//! Accounts::new(guard, users, hasher, cache)
//!     .features(Features { two_factor: true, ..Features::default() })
//!     .two_factor_with(Arc::new(MyTwoFactorStore), Totp::new().issuer("Acme"))
//! ```
//!
//! | | | |
//! |---|---|---|
//! | `POST` | `/two-factor-challenge` | `code` or `recovery_code`, after `/login` answered `{"two_factor": true}` |
//! | `POST` | `/user/two-factor-authentication` | start enrolment: secret, `otpauth://` URI, recovery codes |
//! | `POST` | `/user/confirmed-two-factor-authentication` | finish it with the first code |
//! | `DELETE` | `/user/two-factor-authentication` | turn it off |
//! | `POST` | `/user/two-factor-recovery-codes` | replace the recovery codes |
//!
//! Enrolment, removal and new recovery codes need a password confirmed in the
//! last fifteen minutes — `POST /user/confirm-password` — so a session left
//! open on a shared machine cannot quietly add or remove a factor.
//!
//! # How a login with a second factor goes
//!
//! `/login` checks the password exactly as before. If the user has a
//! **confirmed** enrolment, it does not log them in: it writes a pending
//! login into the session (the user id and an expiry, nothing else) and
//! answers `{"two_factor": true}`. `/two-factor-challenge` then takes a code
//! or a recovery code, and only a right one turns the pending login into a
//! session. Until then the session belongs to nobody, so a stolen password
//! alone reaches nothing.
//!
//! An enrolment that was started but never confirmed does not challenge: the
//! user has not proven their app works, and locking them out on a typo during
//! setup would be the scaffold's fault.
//!
//! # Replay
//!
//! A code is refused if its step is not newer than the last one accepted
//! ([`Totp::verify`](rainier_mfa::Totp::verify)), and the step is claimed
//! atomically in the cache first, so two requests racing with one code cannot
//! both win.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rainier_http::{Request, Response, StatusCode};
use rainier_mfa::{recovery, Secret, Totp};
use rainier_middleware::RateLimitStore as _;
use rainier_session::SessionRequestExt;
use rainier_support::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::accounts::{input, require_confirmed_password, string, too_many, Accounts};
use crate::events::{
    RecoveryCodeUsed, TwoFactorChallenged, TwoFactorDisabled, TwoFactorEnabled, TwoFactorFailed,
};
use crate::users::AccountUser;

/// Where a pending login waits for its second factor.
const PENDING: &str = "_accounts.two_factor.pending";

/// A user's TOTP enrolment, as the application stores it.
///
/// Store it however suits — columns on the users table, or a table of its
/// own. **Encrypt `secret` at rest**: it is the second factor, and a database
/// dump that holds it holds every user's authenticator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TwoFactorRecord {
    /// The shared secret, base32.
    pub secret: String,
    /// Whether the user has proven their app produces codes for it. Only a
    /// confirmed enrolment is challenged at login.
    pub confirmed: bool,
    /// Unspent recovery codes, as [`recovery::hash`] hashes.
    pub recovery_codes: Vec<String>,
    /// The last TOTP step accepted — what stops a code being used twice.
    pub last_used_step: Option<u64>,
}

/// Where enrolments are kept.
#[async_trait::async_trait]
pub trait TwoFactorUsers<U: AccountUser>: Send + Sync + 'static {
    /// The user's enrolment, if they have started one.
    async fn find_two_factor(&self, user: &U) -> Result<Option<TwoFactorRecord>>;

    /// Store the enrolment — or, given `None`, remove it.
    async fn save_two_factor(&self, user: &U, record: Option<&TwoFactorRecord>) -> Result<()>;
}

/// The configured two-factor pieces.
pub(crate) struct TwoFactor<U: AccountUser> {
    pub(crate) users: Arc<dyn TwoFactorUsers<U>>,
    pub(crate) totp: Totp,
}

#[derive(Serialize, Deserialize)]
struct Pending {
    id: String,
    expires_at: u64,
}

impl<U: AccountUser> Accounts<U> {
    fn two_factor(&self) -> Result<&TwoFactor<U>> {
        self.two_factor
            .as_ref()
            .ok_or_else(|| Error::internal("two-factor authentication has no `two_factor_with`"))
    }

    /// After a right password: a challenge, if this user has a confirmed
    /// enrolment. `None` means log them straight in.
    pub(crate) async fn begin_two_factor(
        &self,
        request: &Request,
        user: &U,
    ) -> Result<Option<Response>> {
        if !self.enabled().two_factor {
            return Ok(None);
        }
        let record = self.two_factor()?.users.find_two_factor(user).await?;
        if !record.is_some_and(|record| record.confirmed) {
            return Ok(None);
        }
        // Whoever held this session before the password is not who holds it
        // after: a fixed session id must not carry into the challenge.
        if let Some(session) = request.session() {
            session.regenerate();
        }

        let session = request
            .session()
            .ok_or_else(|| Error::internal("a two-factor login needs the session middleware"))?;
        let expires_at = now() + self.enabled().two_factor_timeout.as_secs();
        session.put(PENDING, Pending { id: user.auth_identifier(), expires_at })?;

        self.announce(TwoFactorChallenged { user_id: user.auth_identifier() }).await;
        Ok(Some(self.respond(
            request,
            StatusCode::OK,
            json!({ "two_factor": true }),
            &self.two_factor_page(),
        )))
    }

    /// `POST /two-factor-challenge` — the second factor, then the session.
    pub async fn two_factor_challenge(&self, request: &Request) -> Result<Response> {
        let session = request
            .session()
            .ok_or_else(|| Error::internal("a two-factor login needs the session middleware"))?;
        let pending: Option<Pending> = session.get(PENDING);
        let Some(pending) = pending.filter(|pending| pending.expires_at >= now()) else {
            session.forget(PENDING);
            return Err(Error::unauthenticated(
                "There is no login waiting for a second factor. Please sign in again.",
            ));
        };

        let Some(user) = self.users().find_by_id(&pending.id).await? else {
            session.forget(PENDING);
            return Err(Error::unauthenticated("Please sign in again."));
        };

        let key = pending.id.clone();
        let limiter = self.limiter_for("two-factor-attempts:");
        if let Some(hit) = limiter.peek(&key).await? {
            if hit.count >= self.enabled().two_factor_attempts {
                return Ok(too_many(
                    "Too many two-factor attempts. Please try again later.",
                    hit.resets_in,
                ));
            }
        }

        let input = input(request);
        let code = string(&input, "code");
        let recovery_code = string(&input, "recovery_code");

        let tf = self.two_factor()?;
        let mut record = match tf.users.find_two_factor(&user).await? {
            Some(record) if record.confirmed => record,
            _ => {
                // Turned off between the password and the code. Nothing to
                // check against, so the login starts again.
                session.forget(PENDING);
                return Err(Error::unauthenticated("Please sign in again."));
            }
        };

        let accepted = if !code.is_empty() {
            self.accept_code(&tf.totp, &pending.id, &mut record, &code).await?
        } else if !recovery_code.is_empty() {
            let spent = recovery::consume(&mut record.recovery_codes, &recovery_code);
            if spent {
                self.announce(RecoveryCodeUsed {
                    user_id: pending.id.clone(),
                    remaining: record.recovery_codes.len(),
                })
                .await;
            }
            spent
        } else {
            false
        };

        if !accepted {
            limiter.hit(&key, self.enabled().login_decay).await?;
            self.announce(TwoFactorFailed { user: Arc::new(user) }).await;
            return Err(Error::validation(json!({
                "code": ["The two-factor code was not valid."],
            })));
        }

        tf.users.save_two_factor(&user, Some(&record)).await?;
        limiter.clear(&key).await?;
        session.forget(PENDING);

        self.complete_login(request, user).await
    }

    /// `POST /user/two-factor-authentication` — start enrolling.
    ///
    /// Answers the secret, the `otpauth://` URI to render as a QR code, and
    /// the recovery codes — the only time the codes are ever shown.
    pub async fn enable_two_factor(&self, request: &Request) -> Result<Response> {
        let user = self.current_user(request).await?;
        require_confirmed_password(request)?;

        let tf = self.two_factor()?;
        if tf.users.find_two_factor(&user).await?.is_some_and(|record| record.confirmed) {
            return Err(Error::conflict(
                "Two-factor authentication is already on. Turn it off before enrolling again.",
            ));
        }

        let secret = Secret::generate();
        let codes = recovery::generate(self.enabled().recovery_codes);
        let record = TwoFactorRecord {
            secret: secret.to_base32(),
            confirmed: false,
            recovery_codes: codes.iter().map(|code| recovery::hash(code)).collect(),
            last_used_step: None,
        };
        tf.users.save_two_factor(&user, Some(&record)).await?;

        Ok(Response::json(&json!({
            "secret": record.secret,
            "otpauth_url": tf.totp.provisioning_uri(&secret, user.email()),
            "recovery_codes": codes,
        }))
        .with_status(StatusCode::CREATED))
    }

    /// `POST /user/confirmed-two-factor-authentication` — finish enrolling
    /// with the first code the app shows.
    pub async fn confirm_two_factor(&self, request: &Request) -> Result<Response> {
        let user = self.current_user(request).await?;
        let tf = self.two_factor()?;

        let mut record = match tf.users.find_two_factor(&user).await? {
            Some(record) if !record.confirmed => record,
            Some(_) => return Err(Error::conflict("Two-factor authentication is already on.")),
            None => return Err(Error::conflict("Start enrolling first.")),
        };

        let code = string(&input(request), "code");
        if !self.accept_code(&tf.totp, &user.auth_identifier(), &mut record, &code).await? {
            return Err(Error::validation(json!({
                "code": ["The two-factor code was not valid."],
            })));
        }

        record.confirmed = true;
        tf.users.save_two_factor(&user, Some(&record)).await?;
        self.announce(TwoFactorEnabled { user: Arc::new(user) }).await;

        Ok(self.respond(
            request,
            StatusCode::OK,
            json!({ "status": "two-factor-authentication-enabled" }),
            "back",
        ))
    }

    /// `DELETE /user/two-factor-authentication` — turn it off.
    pub async fn disable_two_factor(&self, request: &Request) -> Result<Response> {
        let user = self.current_user(request).await?;
        require_confirmed_password(request)?;

        let tf = self.two_factor()?;
        if tf.users.find_two_factor(&user).await?.is_some() {
            tf.users.save_two_factor(&user, None).await?;
            self.announce(TwoFactorDisabled { user: Arc::new(user) }).await;
        }

        Ok(self.respond(
            request,
            StatusCode::OK,
            json!({ "status": "two-factor-authentication-disabled" }),
            "back",
        ))
    }

    /// `POST /user/two-factor-recovery-codes` — replace every recovery code.
    pub async fn regenerate_recovery_codes(&self, request: &Request) -> Result<Response> {
        let user = self.current_user(request).await?;
        require_confirmed_password(request)?;

        let tf = self.two_factor()?;
        let Some(mut record) = tf.users.find_two_factor(&user).await? else {
            return Err(Error::conflict("Two-factor authentication is not on."));
        };

        let codes = recovery::generate(self.enabled().recovery_codes);
        record.recovery_codes = codes.iter().map(|code| recovery::hash(code)).collect();
        tf.users.save_two_factor(&user, Some(&record)).await?;

        Ok(Response::json(&json!({ "recovery_codes": codes })))
    }

    /// Check a TOTP code, refuse a replay, and record the step on `record`.
    async fn accept_code(
        &self,
        totp: &Totp,
        user_id: &str,
        record: &mut TwoFactorRecord,
        code: &str,
    ) -> Result<bool> {
        let secret = Secret::from_base32(&record.secret)?;
        let Some(step) = totp.verify(&secret, code, record.last_used_step) else {
            return Ok(false);
        };

        // The stored step stops a replay after this request finishes; this
        // claim stops one racing it. Held a little longer than the window, by
        // when the stored step has taken over.
        let claim = format!("two-factor-step:{user_id}:{step}");
        if !self.cache().add(&claim, b"1", Some(Duration::from_secs(5 * 60))).await? {
            return Ok(false);
        }

        record.last_used_step = Some(step);
        Ok(true)
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}
