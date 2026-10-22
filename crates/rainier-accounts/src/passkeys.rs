//! Passkey sign-in and registration. Behind the `passkeys` feature.
//!
//! ```ignore
//! Accounts::new(guard, users, hasher, cache)
//!     .features(Features { passkeys: true, ..Features::default() })
//!     .passkeys_with(
//!         Arc::new(MyPasskeyTable),
//!         RelyingParty::new("example.com", "Example").origin("https://app.example.com"),
//!     )
//! ```
//!
//! | | | |
//! |---|---|---|
//! | `POST` | `/passkeys/login/options` | sign-in options for `navigator.credentials.get()` |
//! | `POST` | `/passkeys/login` | `{credential}` — the browser's answer; logs in |
//! | `POST` | `/user/passkeys/options` | registration options for `navigator.credentials.create()` |
//! | `POST` | `/user/passkeys` | `{credential, name}` — store the new passkey |
//! | `DELETE` | `/user/passkeys/{id}` | remove one, by base64url credential id |
//!
//! Each ceremony's challenge lives in the session between its two requests
//! and is spent by the second, so an answer cannot be replayed and a challenge
//! cannot be supplied by the client.
//!
//! # A passkey is both factors
//!
//! Sign-in requires user verification by default (see
//! [`RelyingParty::user_verification`]): the device proves possession, and
//! its PIN or biometric proves the person. So a passkey login does **not**
//! go on to the TOTP challenge — it already is multi-factor. Relax user
//! verification and that stops being true; keep it required if two-factor is
//! on.
//!
//! Registering or removing a passkey needs a recently confirmed password, as
//! enrolling a second factor does.

use std::sync::Arc;

use rainier_http::{Request, Response, StatusCode};
use rainier_mfa::webauthn::{
    decode_base64url, AuthenticationResponse, AuthenticationState, PasskeyCredential, PasskeyUser,
    RegistrationResponse, RegistrationState, RelyingParty,
};
use rainier_session::SessionRequestExt;
use rainier_support::{Error, Result};
use serde_json::json;

use crate::accounts::{input, require_confirmed_password, string, Accounts};
use crate::events::{PasskeyDeleted, PasskeyRegistered, PasskeyUsed};
use crate::users::AccountUser;

const REGISTRATION: &str = "_accounts.passkeys.registration";
const AUTHENTICATION: &str = "_accounts.passkeys.authentication";

/// What every refused passkey sign-in says.
pub const PASSKEY_REFUSED: &str = "That passkey could not be used to sign in.";

/// A stored passkey, as a sign-in finds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredPasskey {
    /// The owning user's identifier — what
    /// [`auth_identifier`](rainier_auth::Authenticatable::auth_identifier)
    /// returns.
    pub user_id: String,
    /// The credential.
    pub credential: PasskeyCredential,
}

/// Where passkeys are kept.
///
/// [`PasskeyCredential`] serialises to JSON, so one JSON column beside a
/// user id and a name is enough — index the credential id (as base64url,
/// [`PasskeyCredential::id_base64url`]) for sign-in.
#[async_trait::async_trait]
pub trait PasskeyRepository<U: AccountUser>: Send + Sync + 'static {
    /// The user's passkeys — excluded at registration, so an authenticator
    /// that already holds one does not create a duplicate.
    async fn passkeys_for(&self, user: &U) -> Result<Vec<PasskeyCredential>>;

    /// The passkey with this credential id, whoever owns it.
    async fn find_passkey(&self, credential_id: &[u8]) -> Result<Option<StoredPasskey>>;

    /// Store a newly registered passkey.
    async fn store_passkey(
        &self,
        user: &U,
        name: &str,
        credential: &PasskeyCredential,
    ) -> Result<()>;

    /// Write back what a sign-in changed — the signature counter and the
    /// backup flags.
    async fn update_passkey(&self, credential: &PasskeyCredential) -> Result<()>;

    /// Remove one of the user's passkeys. `false` if they have none by that
    /// id.
    async fn delete_passkey(&self, user: &U, credential_id: &[u8]) -> Result<bool>;

    /// How the user is described to the authenticator.
    ///
    /// The handle defaults to the user's identifier. It must be stable — the
    /// authenticator stores it and hands it back at every sign-in — and should
    /// not be personal data; override this to return a random handle stored
    /// on the user if identifiers are emails.
    fn passkey_user(&self, user: &U) -> PasskeyUser {
        PasskeyUser::new(user.auth_identifier().into_bytes(), user.email(), user.email())
    }
}

/// The configured passkey pieces.
pub(crate) struct Passkeys<U: AccountUser> {
    pub(crate) store: Arc<dyn PasskeyRepository<U>>,
    pub(crate) relying_party: RelyingParty,
}

impl<U: AccountUser> Accounts<U> {
    fn passkeys(&self) -> Result<&Passkeys<U>> {
        self.passkeys.as_ref().ok_or_else(|| Error::internal("passkeys have no `passkeys_with`"))
    }

    /// `POST /passkeys/login/options` — a challenge for any passkey this site
    /// has.
    pub async fn passkey_login_options(&self, request: &Request) -> Result<Response> {
        let passkeys = self.passkeys()?;
        let (options, state) = passkeys.relying_party.start_authentication(&[]);
        session_put(request, AUTHENTICATION, &state)?;
        Ok(Response::json(&options))
    }

    /// `POST /passkeys/login` — verify the browser's answer and log in.
    pub async fn passkey_login(&self, request: &Request) -> Result<Response> {
        let passkeys = self.passkeys()?;
        // Pulled, not read: the challenge is spent by this attempt whatever
        // its outcome.
        let state: AuthenticationState = session_pull(request, AUTHENTICATION)?;

        let response = AuthenticationResponse::from_json(&input(request)["credential"])?;
        let credential_id = response.credential_id()?;

        // One answer for "no such passkey", "no such user" and "a signature
        // that did not verify", so the endpoint does not say which credential
        // ids exist. The reason goes to the log instead.
        let refused = |why: &str| {
            tracing::info!(reason = why, "a passkey sign-in was refused");
            Error::unauthenticated(PASSKEY_REFUSED)
        };
        let Some(stored) = passkeys.store.find_passkey(&credential_id).await? else {
            return Err(refused("no passkey with that credential id"));
        };
        let Some(user) = self.users().find_by_id(&stored.user_id).await? else {
            return Err(refused("the passkey's user no longer exists"));
        };

        let mut credential = stored.credential;
        let verified = passkeys
            .relying_party
            .finish_authentication(&state, &response, &credential)
            .map_err(|e| refused(e.message()))?;
        verified.apply_to(&mut credential);
        passkeys.store.update_passkey(&credential).await?;

        self.announce(PasskeyUsed {
            user_id: stored.user_id,
            credential_id: credential.id_base64url(),
        })
        .await;
        self.complete_login(request, user).await
    }

    /// `POST /user/passkeys/options` — a challenge to create a passkey for
    /// the signed-in user.
    pub async fn passkey_register_options(&self, request: &Request) -> Result<Response> {
        let user = self.current_user(request).await?;
        require_confirmed_password(request)?;

        let passkeys = self.passkeys()?;
        let existing = passkeys.store.passkeys_for(&user).await?;
        let (options, state) = passkeys
            .relying_party
            .start_registration(&passkeys.store.passkey_user(&user), &existing);
        session_put(request, REGISTRATION, &state)?;
        Ok(Response::json(&options))
    }

    /// `POST /user/passkeys` — verify the new passkey and store it.
    pub async fn passkey_register(&self, request: &Request) -> Result<Response> {
        let user = self.current_user(request).await?;
        require_confirmed_password(request)?;

        let passkeys = self.passkeys()?;
        let state: RegistrationState = session_pull(request, REGISTRATION)?;

        // The ceremony was started for a user handle; finishing it as someone
        // else (the session changed hands in between) would attach a passkey
        // the authenticator thinks is theirs to this account.
        if state.user_handle() != passkeys.store.passkey_user(&user).handle.as_slice() {
            return Err(Error::unauthorized("This passkey was started for another account."));
        }

        let body = input(request);
        let response = RegistrationResponse::from_json(&body["credential"])?;
        let credential = passkeys.relying_party.finish_registration(&state, &response)?;

        if passkeys.store.find_passkey(&credential.id).await?.is_some() {
            return Err(Error::conflict("That passkey is already registered."));
        }

        let name = match string(&body, "name").trim() {
            "" => "Passkey".to_string(),
            name => name.chars().take(100).collect(),
        };
        passkeys.store.store_passkey(&user, &name, &credential).await?;
        self.announce(PasskeyRegistered { user: Arc::new(user), name }).await;

        Ok(Response::json(&json!({ "id": credential.id_base64url() }))
            .with_status(StatusCode::CREATED))
    }

    /// `DELETE /user/passkeys/{id}` — remove one of the user's passkeys.
    pub async fn passkey_delete(&self, request: &Request) -> Result<Response> {
        let user = self.current_user(request).await?;
        require_confirmed_password(request)?;

        let id = request.route_param("id").unwrap_or_default().to_string();
        let credential_id = decode_base64url(&id)?;
        if !self.passkeys()?.store.delete_passkey(&user, &credential_id).await? {
            return Err(Error::not_found("No such passkey."));
        }
        self.announce(PasskeyDeleted { user: Arc::new(user), credential_id: id }).await;

        Ok(Response::no_content())
    }
}

fn session_put<T: serde::Serialize>(request: &Request, key: &str, value: &T) -> Result<()> {
    request
        .session()
        .ok_or_else(|| Error::internal("passkeys need the session middleware"))?
        .put(key, value)
}

fn session_pull<T: serde::de::DeserializeOwned>(request: &Request, key: &str) -> Result<T> {
    request
        .session()
        .ok_or_else(|| Error::internal("passkeys need the session middleware"))?
        .pull(key)
        .ok_or_else(|| Error::bad_request("There is no passkey request in progress. Start again."))
}
