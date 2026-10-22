//! The flows themselves, and the routes they are mounted on — [`Accounts`].

use std::sync::Arc;
use std::time::Duration;

use rainier_auth::{ConfirmPassword, Hasher, StatefulGuard};
use rainier_cache::{Cache, CacheRateLimiter};
use rainier_crypt::UrlSigner;
use rainier_events::Dispatcher;
use rainier_http::{IntoResponse, Redirect, Request, Response, StatusCode};
use rainier_middleware::{IntoMiddlewareStack, MiddlewareStack, RateLimitStore};
use rainier_routing::{GroupAttributes, Req, Router};
use rainier_session::SessionRequestExt;
use rainier_support::{Error, Result};
use rainier_validation::{Rule, Validator};
use serde_json::{json, Value};

use crate::events::{
    Failed, Lockout, Login, Logout, PasswordReset, PasswordUpdated, Registered, Verified,
};
use crate::features::Features;
use crate::links::{MailLinks, SendsPasswordResetLinks, SendsVerificationLinks};
use crate::passwords::PasswordBroker;
use crate::users::{AccountUser, AccountUsers, CreatesNewUsers, NewUser};
use crate::verification::{normalise_email, EmailVerifier};

/// What `POST /forgot-password` says, whatever happened.
pub const RESET_LINK_SENT: &str =
    "If that address has an account, a password reset link is on its way.";

/// The account flows, configured — mount them with [`routes`](Self::routes).
///
/// See the [crate documentation](crate) for the whole picture.
pub struct Accounts<U: AccountUser> {
    guard: Arc<dyn StatefulGuard<U>>,
    users: Arc<dyn AccountUsers<U>>,
    hasher: Arc<dyn Hasher>,
    cache: Arc<dyn Cache>,
    creator: Option<Arc<dyn CreatesNewUsers<U>>>,
    events: Option<Arc<Dispatcher>>,
    features: Features,
    signer: Option<Arc<UrlSigner>>,
    reset_links: Option<Arc<dyn SendsPasswordResetLinks<U>>>,
    links_in_background: bool,
    verification_links: Option<Arc<dyn SendsVerificationLinks<U>>>,
    prefix: String,
    mounted_under: String,
    names: String,
    app_url: String,
    reset_url: String,
    home: String,
    login_page: String,
    middleware: Option<MiddlewareStack>,
    #[cfg(feature = "two-factor")]
    pub(crate) two_factor: Option<crate::two_factor::TwoFactor<U>>,
    #[cfg(feature = "passkeys")]
    pub(crate) passkeys: Option<crate::passkeys::Passkeys<U>>,
}

impl<U: AccountUser> Accounts<U> {
    /// Flows that log users into `guard`, find and change them through
    /// `users`, hash with `hasher`, and keep throttles and reset tokens in
    /// `cache`.
    ///
    /// `cache` should be the shared one in any deployment with more than one
    /// replica — see [`PasswordBroker::is_shared`].
    pub fn new(
        guard: Arc<dyn StatefulGuard<U>>,
        users: Arc<dyn AccountUsers<U>>,
        hasher: Arc<dyn Hasher>,
        cache: Arc<dyn Cache>,
    ) -> Self {
        Self {
            guard,
            users,
            hasher,
            cache,
            creator: None,
            events: None,
            features: Features::default(),
            signer: None,
            reset_links: None,
            links_in_background: true,
            verification_links: None,
            prefix: String::new(),
            mounted_under: String::new(),
            names: "accounts.".into(),
            app_url: String::new(),
            reset_url: "{app_url}/reset-password/{token}?email={email}".into(),
            home: "/".into(),
            login_page: "/login".into(),
            middleware: None,
            #[cfg(feature = "two-factor")]
            two_factor: None,
            #[cfg(feature = "passkeys")]
            passkeys: None,
        }
    }

    // --- configuration ----------------------------------------------------

    /// Which flows are mounted, and their limits.
    #[must_use]
    pub fn features(mut self, features: Features) -> Self {
        self.features = features;
        self
    }

    /// How registration creates a user. Required when registration is on.
    #[must_use]
    pub fn creates_users_with(mut self, creator: Arc<dyn CreatesNewUsers<U>>) -> Self {
        self.creator = Some(creator);
        self
    }

    /// Announce what happens on this bus — see [`events`](crate::events).
    #[must_use]
    pub fn events(mut self, events: Arc<Dispatcher>) -> Self {
        self.events = Some(events);
        self
    }

    /// Sign verification links with this. Required when email verification is
    /// on.
    #[must_use]
    pub fn signs_urls_with(mut self, signer: Arc<UrlSigner>) -> Self {
        self.signer = Some(signer);
        self
    }

    /// How reset links are delivered. Required when password resets are on.
    #[must_use]
    pub fn sends_reset_links_with(mut self, sender: Arc<dyn SendsPasswordResetLinks<U>>) -> Self {
        self.reset_links = Some(sender);
        self
    }

    /// Deliver reset links inside the `/forgot-password` request instead of
    /// on a detached task.
    ///
    /// Detached is the default, and the reason is timing: a delivery takes a
    /// mail provider's round trip, and an answer that takes 300 ms longer for
    /// addresses with an account says which addresses have one — however
    /// carefully the body is kept identical. Inline is for tests that assert
    /// on the delivery, and for a sender that only queues a job (which is
    /// already fast enough to hide in the noise).
    #[must_use]
    pub fn deliver_links_inline(mut self, inline: bool) -> Self {
        self.links_in_background = !inline;
        self
    }

    /// How verification links are delivered. Required when email
    /// verification is on.
    #[must_use]
    pub fn sends_verification_links_with(
        mut self,
        sender: Arc<dyn SendsVerificationLinks<U>>,
    ) -> Self {
        self.verification_links = Some(sender);
        self
    }

    /// Deliver both kinds of link as email — the quick start.
    #[must_use]
    pub fn mails_links_with(self, links: MailLinks) -> Self {
        let minutes = self.features.reset_token_ttl.as_secs() / 60;
        let links = Arc::new(links.reset_link_minutes(minutes));
        self.sends_reset_links_with(Arc::clone(&links) as _)
            .sends_verification_links_with(links as _)
    }

    /// Mount every route under this path prefix — `"auth"` gives
    /// `/auth/login`.
    #[must_use]
    pub fn prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = clean(prefix.into());
        self
    }

    /// The prefix of a group the routes are declared **inside**.
    ///
    /// Only needed for building verification links: the routes cannot see the
    /// groups around them, and a link that omits the outer prefix points at a
    /// path that does not exist.
    #[must_use]
    pub fn mounted_under(mut self, outer_prefix: impl Into<String>) -> Self {
        self.mounted_under = clean(outer_prefix.into());
        self
    }

    /// The route name prefix — `"accounts."` by default, giving
    /// `accounts.login`.
    #[must_use]
    pub fn names(mut self, prefix: impl Into<String>) -> Self {
        self.names = prefix.into();
        self
    }

    /// The application's public base URL — `https://example.com` — which
    /// links in email are absolute against.
    #[must_use]
    pub fn app_url(mut self, url: impl Into<String>) -> Self {
        self.app_url = url.into().trim_end_matches('/').to_string();
        self
    }

    /// The reset link, as a template over `{app_url}`, `{token}` and
    /// `{email}`.
    ///
    /// Points at the page that shows the new-password form — usually the
    /// front end's, not this API's. Defaults to
    /// `{app_url}/reset-password/{token}?email={email}`.
    #[must_use]
    pub fn reset_url(mut self, template: impl Into<String>) -> Self {
        self.reset_url = template.into();
        self
    }

    /// Where a browser goes after logging in, registering or verifying.
    #[must_use]
    pub fn home(mut self, path: impl Into<String>) -> Self {
        self.home = path.into();
        self
    }

    /// Where a browser goes after resetting its password.
    #[must_use]
    pub fn login_page(mut self, path: impl Into<String>) -> Self {
        self.login_page = path.into();
        self
    }

    /// Middleware around every account route — at the least, whatever starts
    /// the session, unless the routes are declared inside a group that
    /// already does.
    #[must_use]
    pub fn middleware(mut self, middleware: impl IntoMiddlewareStack) -> Self {
        self.middleware = Some(middleware.into_middleware_stack());
        self
    }

    /// Two-factor authentication: where each user's TOTP enrolment is kept,
    /// and the code settings (issuer, digits, window). Required when
    /// [`Features::two_factor`] is on.
    #[cfg(feature = "two-factor")]
    #[must_use]
    pub fn two_factor_with(
        mut self,
        users: Arc<dyn crate::two_factor::TwoFactorUsers<U>>,
        totp: rainier_mfa::Totp,
    ) -> Self {
        self.two_factor = Some(crate::two_factor::TwoFactor { users, totp });
        self
    }

    /// Passkeys: where they are kept, and the relying party they are for.
    /// Required when [`Features::passkeys`] is on.
    #[cfg(feature = "passkeys")]
    #[must_use]
    pub fn passkeys_with(
        mut self,
        store: Arc<dyn crate::passkeys::PasskeyRepository<U>>,
        relying_party: rainier_mfa::RelyingParty,
    ) -> Self {
        self.passkeys = Some(crate::passkeys::Passkeys { store, relying_party });
        self
    }

    /// The features in force.
    pub fn enabled(&self) -> &Features {
        &self.features
    }

    #[cfg(any(feature = "two-factor", feature = "passkeys"))]
    pub(crate) fn users(&self) -> &Arc<dyn AccountUsers<U>> {
        &self.users
    }

    #[cfg(feature = "two-factor")]
    pub(crate) fn cache(&self) -> &Arc<dyn Cache> {
        &self.cache
    }

    // --- mounting ---------------------------------------------------------

    /// Declare the enabled routes on `router`.
    ///
    /// | | | |
    /// |---|---|---|
    /// | `POST` | `/register` | `register` |
    /// | `POST` | `/login` | `login` |
    /// | `POST` | `/logout` | `logout` |
    /// | `POST` | `/forgot-password` | `password.email` |
    /// | `POST` | `/reset-password` | `password.update` |
    /// | `GET` | `/email/verify/{id}/{hash}` | `verification.verify` |
    /// | `POST` | `/email/verification-notification` | `verification.send` |
    /// | `POST` | `/user/confirm-password` | `password.confirm` |
    /// | `PUT` | `/user/password` | `user-password.update` |
    ///
    /// Names carry the [`names`](Self::names) prefix.
    ///
    /// # Errors
    ///
    /// When an enabled flow is missing what it needs — registration with no
    /// [`creates_users_with`](Self::creates_users_with), say. At boot, rather
    /// than at the first request that reaches it.
    pub fn routes(self: &Arc<Self>, router: &mut Router) -> Result<()> {
        self.check()?;

        let mut group = GroupAttributes::new().prefix(self.prefix.clone()).name(self.names.clone());
        if let Some(middleware) = &self.middleware {
            group = group.middleware(middleware.clone());
        }

        let features = self.features.clone();
        let accounts = Arc::clone(self);

        router.group(group, move |router| {
            macro_rules! route {
                ($method:ident, $path:expr, $name:expr, $flow:ident) => {{
                    let accounts = Arc::clone(&accounts);
                    router
                        .$method($path, move |request: Req| {
                            let accounts = Arc::clone(&accounts);
                            async move { accounts.$flow(&request).await }
                        })
                        .name($name);
                }};
            }

            if features.registration {
                route!(post, "/register", "register", register);
            }
            route!(post, "/login", "login", login);
            route!(post, "/logout", "logout", logout);
            if features.reset_passwords {
                route!(post, "/forgot-password", "password.email", forgot_password);
                route!(post, "/reset-password", "password.update", reset_password);
            }
            if features.email_verification {
                route!(get, "/email/verify/{id}/{hash}", "verification.verify", verify_email);
                route!(
                    post,
                    "/email/verification-notification",
                    "verification.send",
                    resend_verification
                );
            }
            if features.password_confirmation {
                route!(post, "/user/confirm-password", "password.confirm", confirm_password);
            }
            if features.update_passwords {
                route!(put, "/user/password", "user-password.update", update_password);
            }
            #[cfg(feature = "two-factor")]
            if features.two_factor {
                route!(post, "/two-factor-challenge", "two-factor.login", two_factor_challenge);
                route!(
                    post,
                    "/user/two-factor-authentication",
                    "two-factor.enable",
                    enable_two_factor
                );
                route!(
                    post,
                    "/user/confirmed-two-factor-authentication",
                    "two-factor.confirm",
                    confirm_two_factor
                );
                route!(
                    delete,
                    "/user/two-factor-authentication",
                    "two-factor.disable",
                    disable_two_factor
                );
                route!(
                    post,
                    "/user/two-factor-recovery-codes",
                    "two-factor.recovery-codes",
                    regenerate_recovery_codes
                );
            }
            #[cfg(feature = "passkeys")]
            if features.passkeys {
                route!(
                    post,
                    "/passkeys/login/options",
                    "passkeys.login-options",
                    passkey_login_options
                );
                route!(post, "/passkeys/login", "passkeys.login", passkey_login);
                route!(
                    post,
                    "/user/passkeys/options",
                    "passkeys.register-options",
                    passkey_register_options
                );
                route!(post, "/user/passkeys", "passkeys.store", passkey_register);
                route!(delete, "/user/passkeys/{id}", "passkeys.destroy", passkey_delete);
            }
        });

        Ok(())
    }

    fn check(&self) -> Result<()> {
        // A flow switched on in a build that cannot run it. Said at boot, so
        // the switch is never a silent no-op.
        #[cfg(not(all(feature = "two-factor", feature = "passkeys")))]
        let not_built = |what: &str, feature: &str| {
            Err(Error::internal(format!(
                "rainier-accounts: {what} is enabled, but the crate was built without its \
                 `{feature}` feature"
            )))
        };
        let missing = |what: &str, method: &str| {
            Err(Error::internal(format!(
                "rainier-accounts: {what} is enabled but has no `{method}` — configure it or turn \
                 the feature off"
            )))
        };
        if self.features.registration && self.creator.is_none() {
            return missing("registration", "creates_users_with");
        }
        if self.features.reset_passwords && self.reset_links.is_none() {
            return missing("password reset", "sends_reset_links_with");
        }
        if self.features.email_verification && self.signer.is_none() {
            return missing("email verification", "signs_urls_with");
        }
        if self.features.email_verification && self.verification_links.is_none() {
            return missing("email verification", "sends_verification_links_with");
        }

        #[cfg(feature = "two-factor")]
        if self.features.two_factor && self.two_factor.is_none() {
            return missing("two-factor authentication", "two_factor_with");
        }
        #[cfg(not(feature = "two-factor"))]
        if self.features.two_factor {
            return not_built("two-factor authentication", "two-factor");
        }

        #[cfg(feature = "passkeys")]
        if self.features.passkeys && self.passkeys.is_none() {
            return missing("passkeys", "passkeys_with");
        }
        #[cfg(not(feature = "passkeys"))]
        if self.features.passkeys {
            return not_built("passkeys", "passkeys");
        }

        Ok(())
    }

    // --- the flows ----------------------------------------------------------
    //
    // Public so an application can mount one at a path of its own choosing, or
    // wrap one in a controller that does something first.

    /// `POST /register` — validate, create, log in.
    pub async fn register(&self, request: &Request) -> Result<Response> {
        let creator = self
            .creator
            .as_ref()
            .ok_or_else(|| Error::internal("registration has no `creates_users_with`"))?;

        let input = input(request);
        validate(
            Validator::new()
                .rules("email", vec![Rule::Required, Rule::String, Rule::Email, Rule::Max(255.0)])
                .rules("password", self.password_rules()),
            &input,
        )?;

        let email = normalise_email(&string(&input, "email"));
        if self.users.find_by_email(&email).await?.is_some() {
            // An enumeration oracle, unavoidably: a registration form that
            // accepted a taken address silently would leave its owner unable
            // to tell why they never got in. Throttle the route if it matters.
            return Err(Error::validation(json!({
                "email": ["That email address is already registered."],
            })));
        }

        let password_hash = self.hasher.hash(&string(&input, "password"))?;
        let mut rest = input.clone();
        if let Value::Object(map) = &mut rest {
            map.remove("password");
            map.remove("password_confirmation");
            map.remove("email");
        }

        let user = Arc::new(creator.create(NewUser { email, password_hash, input: rest }).await?);
        self.guard.login(request, &user).await?;
        self.announce(Registered { user: Arc::clone(&user) }).await;

        if self.features.email_verification && !user.has_verified_email() {
            // Registration has happened; a mail server having a bad minute
            // does not undo it. The resend endpoint is the recovery.
            if let Err(e) = self.send_verification(&user).await {
                tracing::error!(error = %e, "could not send the verification link at registration");
            }
        }

        Ok(self.respond(
            request,
            StatusCode::CREATED,
            json!({ "id": user.auth_identifier() }),
            &self.home,
        ))
    }

    /// `POST /login` — throttled, timing-uniform, session-rotating.
    pub async fn login(&self, request: &Request) -> Result<Response> {
        let input = input(request);
        validate(
            Validator::new()
                .rules("email", vec![Rule::Required, Rule::String])
                .rules("password", vec![Rule::Required, Rule::String]),
            &input,
        )?;

        let email = normalise_email(&string(&input, "email"));
        let password = string(&input, "password");
        let ip = request.ip();
        let key = format!("{email}|{}", ip.map(|ip| ip.to_string()).unwrap_or_default());
        let limiter = self.limiter();

        if let Some(hit) = limiter.peek(&key).await? {
            if hit.count >= self.features.max_login_attempts {
                self.announce(Lockout { email, ip, retry_after: hit.resets_in }).await;
                return Ok(too_many(
                    "Too many login attempts. Please try again later.",
                    hit.resets_in,
                ));
            }
        }

        let user = self.users.find_by_email(&email).await?;
        let verified = match user.as_ref().and_then(|user| user.auth_password_hash()) {
            Some(hash) => self.hasher.verify(&password, hash),
            None => {
                // No such account, or one with no password: spend the time a
                // real verification would, so the answer's latency does not
                // say which addresses have accounts.
                self.hasher.dummy_verify(&password);
                false
            }
        };

        let Some(user) = user.filter(|_| verified) else {
            limiter.hit(&key, self.features.login_decay).await?;
            self.announce(Failed { email, ip }).await;
            return Err(Error::validation(json!({
                "email": ["These credentials do not match our records."],
            })));
        };

        limiter.clear(&key).await?;

        // The hash was written under weaker parameters than the hasher now
        // uses — upgrade it while the plaintext is in hand, which is the only
        // time it ever is.
        if let Some(hash) = user.auth_password_hash() {
            if self.hasher.needs_rehash(hash) {
                let upgraded = self.hasher.hash(&password)?;
                if let Err(e) = self.users.set_password_hash(&user, &upgraded).await {
                    tracing::warn!(error = %e, "could not upgrade a password hash at login");
                }
            }
        }

        // A second factor stands between a right password and a session: the
        // password is proven and the throttle cleared, but the session is not
        // yet anybody's.
        #[cfg(feature = "two-factor")]
        if let Some(challenge) = self.begin_two_factor(request, &user).await? {
            return Ok(challenge);
        }

        self.complete_login(request, user).await
    }

    /// Log a fully-authenticated user in and say so — the last step of every
    /// way in.
    pub(crate) async fn complete_login(&self, request: &Request, user: U) -> Result<Response> {
        let user = Arc::new(user);
        self.guard.login(request, &user).await?;
        self.announce(Login { user, guard: self.guard.name().to_string() }).await;

        Ok(self.respond(request, StatusCode::OK, json!({ "authenticated": true }), &self.home))
    }

    /// `POST /logout` — ends the session, whoever it belonged to.
    pub async fn logout(&self, request: &Request) -> Result<Response> {
        let user = self.guard.user(request).await?;
        self.guard.logout(request).await?;
        if let Some(user) = user {
            self.announce(Logout { user: Arc::new(user) }).await;
        }

        Ok(if request.expects_json() {
            Response::no_content()
        } else {
            Redirect::see_other("/").into_response()
        })
    }

    /// `POST /forgot-password` — the same answer whether or not the address
    /// has an account.
    pub async fn forgot_password(&self, request: &Request) -> Result<Response> {
        let input = input(request);
        validate(
            Validator::new().rules("email", vec![Rule::Required, Rule::String, Rule::Email]),
            &input,
        )?;
        let email = normalise_email(&string(&input, "email"));

        // Every failure past validation is swallowed into the log, because
        // each would otherwise answer differently for an address with an
        // account than for one without.
        if let Err(e) = self.send_reset_link(&email).await {
            tracing::error!(error = %e, "could not send a password reset link");
        }

        Ok(self.respond(request, StatusCode::OK, json!({ "status": RESET_LINK_SENT }), "back"))
    }

    async fn send_reset_link(&self, email: &str) -> Result<()> {
        let Some(user) = self.users.find_by_email(email).await? else {
            return Ok(());
        };
        let Some(token) = self.broker().issue(&user.auth_identifier()).await? else {
            // Throttled. Silently, for the same reason as above.
            return Ok(());
        };

        let url = self
            .reset_url
            .replace("{app_url}", &self.app_url)
            .replace("{token}", &token)
            .replace("{email}", &encode_component(user.email()));

        let sender =
            Arc::clone(self.reset_links.as_ref().ok_or_else(|| {
                Error::internal("password reset has no `sends_reset_links_with`")
            })?);

        // Off the request, where there is a runtime to put it on — see
        // `deliver_links_inline`. The lookup and the token write above stay
        // inline: both are one round trip to stores every request touches
        // anyway, where the delivery is a third party's.
        if self.links_in_background {
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    if let Err(e) = sender.send_reset_link(&user, &url).await {
                        tracing::error!(error = %e, "could not send a password reset link");
                    }
                });
                return Ok(());
            }
        }
        sender.send_reset_link(&user, &url).await
    }

    /// `POST /reset-password` — spend a token, set the password.
    pub async fn reset_password(&self, request: &Request) -> Result<Response> {
        let input = input(request);
        validate(
            Validator::new()
                .rules("token", vec![Rule::Required, Rule::String])
                .rules("email", vec![Rule::Required, Rule::String, Rule::Email])
                .rules("password", self.password_rules()),
            &input,
        )?;

        let email = normalise_email(&string(&input, "email"));
        let Some(user) = self.users.find_by_email(&email).await? else {
            // The broker's own wording, so an unknown address and a bad token
            // are one answer.
            return Err(Error::validation(json!({
                "email": ["This password reset link is invalid or has expired."],
            })));
        };

        self.broker().consume(&user.auth_identifier(), &string(&input, "token")).await?;

        let hash = self.hasher.hash(&string(&input, "password"))?;
        self.users.set_password_hash(&user, &hash).await?;
        self.announce(PasswordReset { user: Arc::new(user) }).await;

        Ok(self.respond(
            request,
            StatusCode::OK,
            json!({ "status": "Your password has been reset." }),
            &self.login_page,
        ))
    }

    /// `GET /email/verify/{id}/{hash}` — the link in the email.
    ///
    /// Does not need the clicker to be signed in, so a link opened on a phone
    /// works for an account created on a laptop. The signature and the
    /// address hash are what prove it; if somebody **is** signed in, it must
    /// be the same account.
    pub async fn verify_email(&self, request: &Request) -> Result<Response> {
        let id = request.route_param("id").unwrap_or_default().to_string();
        let Some(user) = self.users.find_by_id(&id).await? else {
            return Err(Error::unauthorized("This verification link is not valid."));
        };

        self.verifier()?.check(request, &user)?;

        if let Some(current) = self.guard.user(request).await? {
            if current.auth_identifier() != user.auth_identifier() {
                return Err(Error::unauthorized("This verification link is for another account."));
            }
        }

        if !user.has_verified_email() {
            self.users.mark_email_verified(&user).await?;
            self.announce(Verified { user: Arc::new(user) }).await;
        }

        Ok(self.respond(
            request,
            StatusCode::OK,
            json!({ "verified": true }),
            &format!("{}?verified=1", self.home),
        ))
    }

    /// `POST /email/verification-notification` — send the link again.
    pub async fn resend_verification(&self, request: &Request) -> Result<Response> {
        let user = self.current_user(request).await?;

        if user.has_verified_email() {
            return Ok(self.respond(
                request,
                StatusCode::OK,
                json!({ "status": "already-verified" }),
                &self.home,
            ));
        }

        // `add` is atomic, so two clicks racing each other send one email.
        let throttle = format!("email-verification:{}", user.auth_identifier());
        let fresh =
            self.cache.add(&throttle, b"1", Some(self.features.verification_throttle)).await?;
        if !fresh {
            return Ok(too_many(
                "A verification link was sent recently. Please check your inbox.",
                self.features.verification_throttle,
            ));
        }

        self.send_verification(&user).await?;

        Ok(self.respond(
            request,
            StatusCode::ACCEPTED,
            json!({ "status": "verification-link-sent" }),
            "back",
        ))
    }

    /// Build and send a verification link for `user`.
    pub async fn send_verification(&self, user: &U) -> Result<()> {
        let path = self.verifier()?.link_for(user)?;
        let sender = self.verification_links.as_ref().ok_or_else(|| {
            Error::internal("email verification has no `sends_verification_links_with`")
        })?;
        sender.send_verification_link(user, &format!("{}{path}", self.app_url)).await
    }

    /// `POST /user/confirm-password` — what
    /// [`ConfirmPassword`]'s `423` asks for.
    pub async fn confirm_password(&self, request: &Request) -> Result<Response> {
        let user = self.current_user(request).await?;
        let password = request.input("password").unwrap_or_default();

        let Some(hash) = user.auth_password_hash() else {
            return Err(Error::bad_request("This account does not sign in with a password."));
        };
        if !self.hasher.verify(&password, hash) {
            return Err(Error::validation(json!({
                "password": ["That password is not correct."],
            })));
        }

        ConfirmPassword::mark_confirmed(request)?;
        Ok(if request.expects_json() {
            Response::no_content()
        } else {
            Redirect::see_other(back(request)).into_response()
        })
    }

    /// `PUT /user/password` — change it, knowing the current one.
    pub async fn update_password(&self, request: &Request) -> Result<Response> {
        let user = self.current_user(request).await?;
        let input = input(request);
        validate(
            Validator::new()
                .rules("current_password", vec![Rule::Required, Rule::String])
                .rules("password", self.password_rules()),
            &input,
        )?;

        let current_ok = user
            .auth_password_hash()
            .is_some_and(|hash| self.hasher.verify(&string(&input, "current_password"), hash));
        if !current_ok {
            return Err(Error::validation(json!({
                "current_password": ["That password is not correct."],
            })));
        }

        let hash = self.hasher.hash(&string(&input, "password"))?;
        self.users.set_password_hash(&user, &hash).await?;

        // A reset link in an old email must not be able to undo this, and a
        // session id that was observed before the change should not survive
        // it.
        self.broker().revoke(&user.auth_identifier()).await?;
        if let Some(session) = request.session() {
            session.regenerate();
        }
        self.announce(PasswordUpdated { user: Arc::new(user) }).await;

        Ok(self.respond(request, StatusCode::OK, json!({ "status": "password-updated" }), "back"))
    }

    // --- helpers ------------------------------------------------------------

    /// The password reset broker these flows use.
    pub fn broker(&self) -> PasswordBroker {
        PasswordBroker::new(Arc::clone(&self.cache))
            .lasting(self.features.reset_token_ttl)
            .throttled(self.features.reset_throttle)
    }

    /// The verification link builder these flows use.
    pub fn verifier(&self) -> Result<EmailVerifier> {
        let signer = self
            .signer
            .as_ref()
            .ok_or_else(|| Error::internal("email verification has no `signs_urls_with`"))?;
        let path = format!("{}{}/email/verify", self.mounted_under, self.prefix);
        Ok(EmailVerifier::new(
            Arc::clone(signer),
            path,
            self.features.verification_ttl.as_secs() as i64,
        ))
    }

    pub(crate) fn limiter(&self) -> CacheRateLimiter {
        self.limiter_for("login-attempts:")
    }

    pub(crate) fn limiter_for(&self, prefix: &str) -> CacheRateLimiter {
        CacheRateLimiter::new(Arc::clone(&self.cache)).with_prefix(prefix)
    }

    /// Where a browser goes to type its second factor: the challenge page
    /// the front end serves at the same path the API answers on.
    #[cfg(feature = "two-factor")]
    pub(crate) fn two_factor_page(&self) -> String {
        format!("{}{}/two-factor-challenge", self.mounted_under, self.prefix)
    }

    fn password_rules(&self) -> Vec<Rule> {
        vec![
            Rule::Required,
            Rule::String,
            Rule::Min(self.features.password_min_length as f64),
            Rule::Confirmed,
        ]
    }

    pub(crate) async fn current_user(&self, request: &Request) -> Result<U> {
        self.guard.user(request).await?.ok_or_else(|| Error::unauthenticated("Unauthenticated."))
    }

    pub(crate) async fn announce<E: Send + Sync + 'static>(&self, event: E) {
        if let Some(events) = &self.events {
            events.dispatch_quietly(event).await;
        }
    }

    /// JSON for an API client; a `303` for a browser form post.
    pub(crate) fn respond(
        &self,
        request: &Request,
        status: StatusCode,
        body: Value,
        to: &str,
    ) -> Response {
        if request.expects_json() {
            Response::json(&body).with_status(status)
        } else if to == "back" {
            Redirect::see_other(back(request)).into_response()
        } else {
            Redirect::see_other(to).into_response()
        }
    }
}

impl<U: AccountUser> std::fmt::Debug for Accounts<U> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Accounts")
            .field("guard", &self.guard.name())
            .field("prefix", &self.prefix)
            .field("features", &self.features)
            .finish()
    }
}

/// Where a form came from, falling back to the home page — only ever a path
/// on this site, so a forged `Referer` cannot turn a form post into an open
/// redirect.
fn back(request: &Request) -> String {
    request
        .header("referer")
        .and_then(|referer| {
            let path = match referer.find("://") {
                Some(scheme) => {
                    let rest = &referer[scheme + 3..];
                    &rest[rest.find('/').unwrap_or(rest.len())..]
                }
                None => referer,
            };
            (path.starts_with('/') && !path.starts_with("//")).then(|| path.to_string())
        })
        .unwrap_or_else(|| "/".to_string())
}

/// The submitted fields, with `email` already trimmed and lowercased — before
/// validation, so `" Ada@Example.com "` pasted with its whitespace is accepted
/// as the address it obviously is.
pub(crate) fn input(request: &Request) -> Value {
    let mut input = request.all();
    if let Some(Value::String(email)) = input.get_mut("email") {
        *email = normalise_email(email);
    }
    input
}

pub(crate) fn validate(validator: Validator, input: &Value) -> Result<()> {
    validator.validate(input).map_err(|errors| Error::validation(errors.to_json()))
}

pub(crate) fn string(input: &Value, key: &str) -> String {
    match input.get(key) {
        Some(Value::String(value)) => value.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

pub(crate) fn too_many(message: &str, retry_after: Duration) -> Response {
    // Rounded up: "retry after 0" for a window with 400ms left invites an
    // immediate retry that is refused again.
    let seconds = retry_after.as_millis().div_ceil(1000).max(1);
    Error::too_many_requests(message)
        .into_response()
        .with_header("retry-after", &seconds.to_string())
}

/// Refuse with `423` unless the password was confirmed recently — the answer
/// [`ConfirmPassword`]'s middleware gives, so a client handles both alike.
#[cfg(any(feature = "two-factor", feature = "passkeys"))]
pub(crate) fn require_confirmed_password(request: &Request) -> Result<()> {
    if ConfirmPassword::recently().is_confirmed(request) {
        Ok(())
    } else {
        Err(Error::new(rainier_support::ErrorKind::Status(423), "Please confirm your password."))
    }
}

/// `""`, or `/x` — never a trailing slash.
fn clean(prefix: String) -> String {
    let trimmed = prefix.trim_matches('/');
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("/{trimmed}")
    }
}

/// Percent-encoding for a query value.
fn encode_component(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}
