//! A WebAuthn relying party — passkey registration and sign-in.
//!
//! ```ignore
//! let rp = RelyingParty::new("example.com", "Example").origin("https://example.com");
//!
//! // Registration: options to the browser, state into the session.
//! let (options, state) = rp.start_registration(&PasskeyUser::new(handle, "ada", "Ada"), &existing);
//! session.put("passkey.registration", &state)?;
//! // …navigator.credentials.create({ publicKey: PublicKeyCredential.parseCreationOptionsFromJSON(options) })
//! let credential = rp.finish_registration(&state, &RegistrationResponse::from_json(&body)?)?;
//! store(credential);
//!
//! // Sign-in: the same shape, against a stored credential.
//! let (options, state) = rp.start_authentication(&[]);          // discoverable
//! let response = AuthenticationResponse::from_json(&body)?;
//! let mut credential = find(&response.credential_id()?)?;
//! let verified = rp.finish_authentication(&state, &response, &credential)?;
//! verified.apply_to(&mut credential);                          // new sign count, backup state
//! update(credential);
//! ```
//!
//! # A ceremony is two requests, so it has state
//!
//! [`RegistrationState`] and [`AuthenticationState`] carry the challenge
//! between the request that issues it and the one that answers it. They are
//! serialisable so they can sit in the session — and **only** in the session:
//! a challenge the client could choose is no challenge. Each carries its own
//! expiry, so an answer to a challenge issued an hour ago is refused however
//! long the session lives.
//!
//! # What is checked
//!
//! Everything the WebAuthn Level 3 "verifying" procedures require of a relying
//! party that does not demand attestation:
//!
//! - the client data's `type`, `challenge` (constant-time), `origin` (exact
//!   match against an allow-list) and `crossOrigin`;
//! - the RP ID hash, the user-present flag, user verification when required,
//!   and backup flags that are consistent with each other;
//! - the credential's algorithm against the ones offered, and its key's
//!   validity;
//! - for sign-in, the signature over `authenticatorData ‖ SHA-256(clientData)`,
//!   that the credential is one the challenge allowed, that the user handle
//!   matches, and that the signature counter moved forward.
//!
//! # Attestation is accepted, not trusted
//!
//! Registration asks for `attestation: "none"`, which is what passkey
//! providers send and what every relying party that is not issuing hardware to
//! staff should ask for. A `none` statement must be empty; a `packed`
//! self-attestation is verified with the credential's own key. Anything else —
//! `packed` with a certificate chain, `tpm`, `android-key`, `fido-u2f`,
//! `apple` — is a claim about the authenticator's make that only means
//! something checked against the FIDO Metadata Service, which this crate does
//! not consult. [`AttestationPolicy::Lenient`] (the default) accepts such a
//! credential and records the format as unverified; [`AttestationPolicy::Strict`]
//! refuses it.
//!
//! # A counter that goes backwards
//!
//! A non-zero signature counter that does not exceed the stored one means two
//! authenticators hold the same private key — a cloned security key. Sign-in
//! is refused. Synced passkeys report zero throughout, which is allowed.

pub mod testing;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use base64::Engine as _;
use rainier_support::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value as Json};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::cbor::{self, Value};
use crate::cose::{CoseAlgorithm, CoseKey};

const FLAG_UP: u8 = 0x01;
const FLAG_UV: u8 = 0x04;
const FLAG_BE: u8 = 0x08;
const FLAG_BS: u8 = 0x10;
const FLAG_AT: u8 = 0x40;
const FLAG_ED: u8 = 0x80;

/// The longest credential id WebAuthn allows.
const MAX_CREDENTIAL_ID: usize = 1023;

/// Whether the authenticator must verify the user — PIN, biometric.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UserVerification {
    /// Refuse a response without it. What a passkey that *replaces* a
    /// password and a second factor needs: possession plus verification is
    /// two factors.
    #[default]
    Required,
    /// Ask for it, accept a response without it.
    Preferred,
    /// Do not ask.
    Discouraged,
}

impl UserVerification {
    fn name(self) -> &'static str {
        match self {
            Self::Required => "required",
            Self::Preferred => "preferred",
            Self::Discouraged => "discouraged",
        }
    }
}

/// What to do with an attestation this crate cannot verify — see the
/// [module documentation](self#attestation-is-accepted-not-trusted).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AttestationPolicy {
    /// Accept it, recording the format as unverified.
    #[default]
    Lenient,
    /// Accept only `none` and `packed` self-attestation.
    Strict,
}

/// The user a passkey is created for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasskeyUser {
    /// An opaque, stable identifier of up to 64 bytes. **Not** an email
    /// address or anything else personal: authenticators store it and may
    /// show it, and it outlives any address change.
    pub handle: Vec<u8>,
    /// A name the user recognises the account by — usually their address.
    pub name: String,
    /// What the authenticator shows.
    pub display_name: String,
}

impl PasskeyUser {
    /// A user with this handle.
    pub fn new(
        handle: impl Into<Vec<u8>>,
        name: impl Into<String>,
        display_name: impl Into<String>,
    ) -> Self {
        Self { handle: handle.into(), name: name.into(), display_name: display_name.into() }
    }

    /// A fresh random 32-byte handle — store it on the user and reuse it for
    /// every passkey they register.
    pub fn random_handle() -> Vec<u8> {
        crate::random_bytes(32)
    }
}

/// A registered passkey, as the application stores it.
///
/// Serialisable, with byte fields as unpadded base64url, so it can be one JSON
/// column. Built by [`RelyingParty::finish_registration`], or from the parts
/// another system stored with [`from_parts`](Self::from_parts).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PasskeyCredential {
    /// The credential id the authenticator chose.
    #[serde(with = "b64")]
    pub id: Vec<u8>,
    /// The public key, COSE-encoded.
    #[serde(with = "b64")]
    pub public_key: Vec<u8>,
    /// The last signature counter seen.
    pub sign_count: u32,
    /// The user handle it was created for.
    #[serde(with = "b64")]
    pub user_handle: Vec<u8>,
    /// How the browser reported it can be reached — `internal`, `hybrid`,
    /// `usb`, `nfc`, `ble`. A hint for sign-in, never a security property.
    #[serde(default)]
    pub transports: Vec<String>,
    /// Whether the credential may be synced to other devices.
    #[serde(default)]
    pub backup_eligible: bool,
    /// Whether it is currently backed up.
    #[serde(default)]
    pub backed_up: bool,
    /// The authenticator model's AAGUID — zero for most passkey providers.
    #[serde(default, with = "b64_array")]
    pub aaguid: [u8; 16],
    /// The attestation format registration received, and whether it was
    /// verified (`none`, `packed`, or `unverified:<fmt>`).
    #[serde(default)]
    pub attestation: String,
}

impl PasskeyCredential {
    /// A credential from its stored parts — for passkeys another system
    /// registered.
    ///
    /// # Errors
    ///
    /// An empty or over-long id, or a public key that is not a valid COSE key
    /// for ES256, EdDSA or RS256.
    pub fn from_parts(
        id: impl Into<Vec<u8>>,
        public_key: impl Into<Vec<u8>>,
        sign_count: u32,
        user_handle: impl Into<Vec<u8>>,
    ) -> Result<Self> {
        let id = id.into();
        if id.is_empty() || id.len() > MAX_CREDENTIAL_ID {
            return Err(Error::bad_request("a passkey credential id must be 1 to 1023 bytes"));
        }
        let public_key = public_key.into();
        CoseKey::from_bytes(&public_key)?;

        Ok(Self {
            id,
            public_key,
            sign_count,
            user_handle: user_handle.into(),
            transports: Vec::new(),
            backup_eligible: false,
            backed_up: false,
            aaguid: [0; 16],
            attestation: "none".into(),
        })
    }

    /// With transport hints.
    #[must_use]
    pub fn with_transports(mut self, transports: Vec<String>) -> Self {
        self.transports = transports;
        self
    }

    /// With backup flags.
    #[must_use]
    pub fn with_backup(mut self, eligible: bool, backed_up: bool) -> Self {
        self.backup_eligible = eligible;
        self.backed_up = backed_up;
        self
    }

    /// With an AAGUID.
    #[must_use]
    pub fn with_aaguid(mut self, aaguid: [u8; 16]) -> Self {
        self.aaguid = aaguid;
        self
    }

    /// With the attestation format it was registered under.
    #[must_use]
    pub fn with_attestation(mut self, attestation: impl Into<String>) -> Self {
        self.attestation = attestation.into();
        self
    }

    /// The id as unpadded base64url — what a browser calls it, and a good
    /// lookup key.
    pub fn id_base64url(&self) -> String {
        B64URL.encode(&self.id)
    }

    /// The parsed public key.
    ///
    /// # Errors
    ///
    /// If the stored key no longer parses.
    pub fn key(&self) -> Result<CoseKey> {
        CoseKey::from_bytes(&self.public_key)
    }

    fn descriptor(&self) -> Json {
        let mut descriptor = json!({ "type": "public-key", "id": self.id_base64url() });
        if !self.transports.is_empty() {
            descriptor["transports"] = json!(self.transports);
        }
        descriptor
    }
}

/// What a registration ceremony carries between its two requests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistrationState {
    #[serde(with = "b64")]
    challenge: Vec<u8>,
    #[serde(with = "b64")]
    user_handle: Vec<u8>,
    user_verification: UserVerification,
    expires_at: u64,
}

impl RegistrationState {
    /// The user handle this ceremony is creating a passkey for.
    pub fn user_handle(&self) -> &[u8] {
        &self.user_handle
    }
}

/// What a sign-in ceremony carries between its two requests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthenticationState {
    #[serde(with = "b64")]
    challenge: Vec<u8>,
    /// Credential ids the challenge was issued for; empty for a discoverable
    /// sign-in, where any of the user's passkeys may answer.
    #[serde(default)]
    allowed: Vec<String>,
    user_verification: UserVerification,
    expires_at: u64,
}

/// The browser's answer to `navigator.credentials.create()`, as
/// `PublicKeyCredential.toJSON()` writes it.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistrationResponse {
    /// The credential id, base64url.
    pub id: String,
    /// The same, again — WebAuthn sends both.
    pub raw_id: String,
    /// `public-key`.
    #[serde(rename = "type")]
    pub kind: String,
    /// The authenticator's response.
    pub response: AttestationResponse,
}

/// The `response` member of a [`RegistrationResponse`].
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttestationResponse {
    /// The client data, base64url.
    #[serde(rename = "clientDataJSON")]
    pub client_data_json: String,
    /// The attestation object, base64url CBOR.
    pub attestation_object: String,
    /// Transport hints, when the browser supplies them.
    #[serde(default)]
    pub transports: Vec<String>,
}

impl RegistrationResponse {
    /// Parse the JSON the browser sent.
    ///
    /// # Errors
    ///
    /// If it is not the shape `toJSON()` produces.
    pub fn from_json(value: &Json) -> Result<Self> {
        serde_json::from_value(value.clone())
            .map_err(|e| Error::bad_request(format!("not a passkey registration: {e}")))
    }
}

/// The browser's answer to `navigator.credentials.get()`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthenticationResponse {
    /// The credential id, base64url.
    pub id: String,
    /// The same, again.
    pub raw_id: String,
    /// `public-key`.
    #[serde(rename = "type")]
    pub kind: String,
    /// The authenticator's response.
    pub response: AssertionResponse,
}

/// The `response` member of an [`AuthenticationResponse`].
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssertionResponse {
    /// The client data, base64url.
    #[serde(rename = "clientDataJSON")]
    pub client_data_json: String,
    /// The authenticator data, base64url.
    pub authenticator_data: String,
    /// The signature, base64url.
    pub signature: String,
    /// The user handle, base64url — present for a discoverable credential.
    #[serde(default)]
    pub user_handle: Option<String>,
}

impl AuthenticationResponse {
    /// Parse the JSON the browser sent.
    ///
    /// # Errors
    ///
    /// If it is not the shape `toJSON()` produces.
    pub fn from_json(value: &Json) -> Result<Self> {
        serde_json::from_value(value.clone())
            .map_err(|e| Error::bad_request(format!("not a passkey sign-in: {e}")))
    }

    /// The credential id the response claims — what to look the stored
    /// credential up by before calling
    /// [`finish_authentication`](RelyingParty::finish_authentication).
    ///
    /// # Errors
    ///
    /// If it is not base64url.
    pub fn credential_id(&self) -> Result<Vec<u8>> {
        decode(&self.raw_id, "rawId")
    }

    /// The user handle the authenticator returned, if any.
    ///
    /// # Errors
    ///
    /// If it is not base64url.
    pub fn user_handle(&self) -> Result<Option<Vec<u8>>> {
        match self.response.user_handle.as_deref() {
            None | Some("") => Ok(None),
            Some(handle) => decode(handle, "userHandle").map(Some),
        }
    }
}

/// A sign-in that verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedAssertion {
    /// The credential that signed.
    pub credential_id: Vec<u8>,
    /// The new signature counter — store it.
    pub sign_count: u32,
    /// Whether the user was verified (PIN, biometric).
    pub user_verified: bool,
    /// Whether the credential is backup-eligible.
    pub backup_eligible: bool,
    /// Whether it is currently backed up.
    pub backed_up: bool,
}

impl VerifiedAssertion {
    /// Write what this sign-in learned onto the stored credential.
    pub fn apply_to(&self, credential: &mut PasskeyCredential) {
        credential.sign_count = self.sign_count;
        credential.backup_eligible = self.backup_eligible;
        credential.backed_up = self.backed_up;
    }
}

/// The relying party: who the passkeys are for, and the ceremonies.
#[derive(Debug, Clone)]
pub struct RelyingParty {
    id: String,
    name: String,
    origins: Vec<String>,
    user_verification: UserVerification,
    allow_cross_origin: bool,
    timeout: Duration,
    attestation: AttestationPolicy,
    algorithms: Vec<CoseAlgorithm>,
}

impl RelyingParty {
    /// A relying party for the domain `id` — `example.com`, no scheme, no
    /// port — shown to users as `name`.
    ///
    /// The allowed origin defaults to `https://{id}`; name others with
    /// [`origin`](Self::origin).
    pub fn new(id: impl Into<String>, name: impl Into<String>) -> Self {
        let id = id.into();
        Self {
            origins: vec![format!("https://{id}")],
            id,
            name: name.into(),
            user_verification: UserVerification::Required,
            allow_cross_origin: false,
            timeout: Duration::from_secs(300),
            attestation: AttestationPolicy::Lenient,
            algorithms: CoseAlgorithm::ALL.to_vec(),
        }
    }

    /// Replace the allowed origins with exactly these — every scheme, host
    /// and port a ceremony may run on, `https://app.example.com` and
    /// `http://localhost:5173` alike. Native apps' origins
    /// (`android:apk-key-hash:…`, `ios:bundle-id:…`) go here too.
    #[must_use]
    pub fn origins<I, S>(mut self, origins: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.origins = origins.into_iter().map(Into::into).collect();
        self
    }

    /// Allow one more origin.
    #[must_use]
    pub fn origin(mut self, origin: impl Into<String>) -> Self {
        let origin = origin.into();
        if !self.origins.contains(&origin) {
            self.origins.push(origin);
        }
        self
    }

    /// Whether user verification is required. Required by default.
    #[must_use]
    pub fn user_verification(mut self, requirement: UserVerification) -> Self {
        self.user_verification = requirement;
        self
    }

    /// Accept ceremonies run inside a cross-origin iframe. Off by default.
    #[must_use]
    pub fn allow_cross_origin(mut self, allow: bool) -> Self {
        self.allow_cross_origin = allow;
        self
    }

    /// How long a challenge stays answerable. Five minutes by default.
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The attestation policy.
    #[must_use]
    pub fn attestation(mut self, policy: AttestationPolicy) -> Self {
        self.attestation = policy;
        self
    }

    /// The RP ID.
    pub fn id(&self) -> &str {
        &self.id
    }

    // --- registration -------------------------------------------------------

    /// Options for `navigator.credentials.create()` — the
    /// `PublicKeyCredentialCreationOptionsJSON` form, for
    /// `PublicKeyCredential.parseCreationOptionsFromJSON` — and the state to
    /// keep in the session.
    ///
    /// `existing` are the user's current passkeys, excluded so an
    /// authenticator that already holds one says so instead of creating a
    /// duplicate.
    pub fn start_registration(
        &self,
        user: &PasskeyUser,
        existing: &[PasskeyCredential],
    ) -> (Json, RegistrationState) {
        let challenge = crate::random_bytes(32);
        let options = json!({
            "rp": { "id": self.id, "name": self.name },
            "user": {
                "id": B64URL.encode(&user.handle),
                "name": user.name,
                "displayName": user.display_name,
            },
            "challenge": B64URL.encode(&challenge),
            "pubKeyCredParams": self
                .algorithms
                .iter()
                .map(|alg| json!({ "type": "public-key", "alg": alg.id() }))
                .collect::<Vec<_>>(),
            "timeout": self.timeout.as_millis() as u64,
            "excludeCredentials": existing.iter().map(PasskeyCredential::descriptor).collect::<Vec<_>>(),
            "authenticatorSelection": {
                "residentKey": "preferred",
                "requireResidentKey": false,
                "userVerification": self.user_verification.name(),
            },
            "attestation": "none",
        });

        let state = RegistrationState {
            challenge,
            user_handle: user.handle.clone(),
            user_verification: self.user_verification,
            expires_at: now() + self.timeout.as_secs(),
        };
        (options, state)
    }

    /// Verify the browser's answer and return the credential to store.
    ///
    /// Does not check that the credential id is new — look it up and refuse a
    /// duplicate before storing, since only the application can see its own
    /// table.
    ///
    /// # Errors
    ///
    /// Any failed check — `400` for a malformed response, `401` for one that
    /// is well-formed and wrong.
    pub fn finish_registration(
        &self,
        state: &RegistrationState,
        response: &RegistrationResponse,
    ) -> Result<PasskeyCredential> {
        check_live(state.expires_at)?;
        let raw_id = same_ids(&response.kind, &response.id, &response.raw_id)?;

        let client_data = decode(&response.response.client_data_json, "clientDataJSON")?;
        self.check_client_data(&client_data, "webauthn.create", &state.challenge)?;

        let attestation =
            cbor::decode(&decode(&response.response.attestation_object, "attestationObject")?)?;
        let format = attestation
            .get_text("fmt")
            .and_then(Value::as_text)
            .ok_or_else(|| malformed("an attestation object with no format"))?;
        let statement = attestation
            .get_text("attStmt")
            .filter(|statement| statement.as_map().is_some())
            .ok_or_else(|| malformed("an attestation object with no statement"))?;
        let auth_data_bytes = attestation
            .get_text("authData")
            .and_then(Value::as_bytes)
            .ok_or_else(|| malformed("an attestation object with no authenticator data"))?;

        let auth_data = AuthenticatorData::parse(auth_data_bytes)?;
        self.check_auth_data(&auth_data, state.user_verification)?;
        let attested = auth_data
            .attested
            .as_ref()
            .ok_or_else(|| malformed("a registration with no attested credential"))?;

        if !self.algorithms.contains(&attested.key.algorithm()) {
            return Err(refused("the passkey uses an algorithm that was not offered"));
        }
        if attested.id != raw_id {
            return Err(refused("the credential id does not match the response's"));
        }

        let client_data_hash = Sha256::digest(&client_data);
        let attestation = self.check_statement(
            format,
            statement,
            auth_data_bytes,
            &client_data_hash,
            &attested.key,
        )?;

        Ok(PasskeyCredential {
            id: attested.id.clone(),
            public_key: attested.public_key.clone(),
            sign_count: auth_data.sign_count,
            user_handle: state.user_handle.clone(),
            transports: response.response.transports.clone(),
            backup_eligible: auth_data.flags & FLAG_BE != 0,
            backed_up: auth_data.flags & FLAG_BS != 0,
            aaguid: attested.aaguid,
            attestation,
        })
    }

    fn check_statement(
        &self,
        format: &str,
        statement: &Value,
        auth_data: &[u8],
        client_data_hash: &[u8],
        key: &CoseKey,
    ) -> Result<String> {
        let entries = statement.as_map().unwrap_or_default();

        match format {
            "none" => {
                if !entries.is_empty() {
                    return Err(refused("a `none` attestation with a statement"));
                }
                Ok("none".into())
            }
            "packed" if statement.get_text("x5c").is_none() => {
                // Self-attestation: signed by the credential's own key.
                let alg = statement.get_text("alg").and_then(Value::as_int);
                if alg != Some(i128::from(key.algorithm().id())) {
                    return Err(refused("a self-attestation whose algorithm is not the key's"));
                }
                let signature = statement
                    .get_text("sig")
                    .and_then(Value::as_bytes)
                    .ok_or_else(|| malformed("a packed attestation with no signature"))?;
                let mut signed = auth_data.to_vec();
                signed.extend_from_slice(client_data_hash);
                key.verify(&signed, signature)
                    .map_err(|_| refused("the self-attestation signature did not verify"))?;
                Ok("packed".into())
            }
            other => match self.attestation {
                AttestationPolicy::Lenient => Ok(format!("unverified:{other}")),
                AttestationPolicy::Strict => Err(refused(&format!(
                    "`{other}` attestation is not accepted by this relying party"
                ))),
            },
        }
    }

    // --- sign-in ------------------------------------------------------------

    /// Options for `navigator.credentials.get()`, and the state to keep.
    ///
    /// Pass the user's credentials to restrict the ceremony to them (the user
    /// typed their address first), or none for a discoverable sign-in from an
    /// empty form — the browser offers whatever passkeys it holds for this RP.
    pub fn start_authentication(&self, allow: &[PasskeyCredential]) -> (Json, AuthenticationState) {
        let challenge = crate::random_bytes(32);
        let options = json!({
            "challenge": B64URL.encode(&challenge),
            "rpId": self.id,
            "timeout": self.timeout.as_millis() as u64,
            "allowCredentials": allow.iter().map(PasskeyCredential::descriptor).collect::<Vec<_>>(),
            "userVerification": self.user_verification.name(),
        });

        let state = AuthenticationState {
            challenge,
            allowed: allow.iter().map(PasskeyCredential::id_base64url).collect(),
            user_verification: self.user_verification,
            expires_at: now() + self.timeout.as_secs(),
        };
        (options, state)
    }

    /// Verify a sign-in against the stored credential it names.
    ///
    /// # Errors
    ///
    /// Any failed check, including a counter that did not advance.
    pub fn finish_authentication(
        &self,
        state: &AuthenticationState,
        response: &AuthenticationResponse,
        credential: &PasskeyCredential,
    ) -> Result<VerifiedAssertion> {
        check_live(state.expires_at)?;
        let raw_id = same_ids(&response.kind, &response.id, &response.raw_id)?;

        if !bool::from(raw_id.ct_eq(&credential.id)) {
            return Err(refused("the response is not from this credential"));
        }
        if !state.allowed.is_empty() && !state.allowed.contains(&credential.id_base64url()) {
            return Err(refused("this credential was not asked for"));
        }

        match response.user_handle()? {
            Some(handle) => {
                if !bool::from(handle.ct_eq(&credential.user_handle)) {
                    return Err(refused("the passkey belongs to a different user"));
                }
            }
            None if state.allowed.is_empty() => {
                // A discoverable sign-in identifies the user by the handle;
                // without one there is nobody to sign in.
                return Err(refused("a discoverable sign-in returned no user handle"));
            }
            None => {}
        }

        let client_data = decode(&response.response.client_data_json, "clientDataJSON")?;
        self.check_client_data(&client_data, "webauthn.get", &state.challenge)?;

        let auth_data_bytes = decode(&response.response.authenticator_data, "authenticatorData")?;
        let auth_data = AuthenticatorData::parse(&auth_data_bytes)?;
        self.check_auth_data(&auth_data, state.user_verification)?;

        let mut signed = auth_data_bytes.clone();
        signed.extend_from_slice(&Sha256::digest(&client_data));
        let signature = decode(&response.response.signature, "signature")?;
        credential.key()?.verify(&signed, &signature)?;

        if (auth_data.sign_count != 0 || credential.sign_count != 0)
            && auth_data.sign_count <= credential.sign_count
        {
            return Err(refused(
                "the passkey's signature counter did not advance — it may have been cloned",
            ));
        }

        Ok(VerifiedAssertion {
            credential_id: credential.id.clone(),
            sign_count: auth_data.sign_count,
            user_verified: auth_data.flags & FLAG_UV != 0,
            backup_eligible: auth_data.flags & FLAG_BE != 0,
            backed_up: auth_data.flags & FLAG_BS != 0,
        })
    }

    // --- shared checks ------------------------------------------------------

    fn check_client_data(&self, bytes: &[u8], kind: &str, challenge: &[u8]) -> Result<()> {
        let data: Json =
            serde_json::from_slice(bytes).map_err(|_| malformed("client data that is not JSON"))?;

        if data["type"].as_str() != Some(kind) {
            return Err(refused(&format!("client data that is not `{kind}`")));
        }

        let echoed = data["challenge"]
            .as_str()
            .ok_or_else(|| malformed("client data with no challenge"))
            .and_then(|text| decode(text, "challenge"))?;
        if !bool::from(echoed.ct_eq(challenge)) {
            return Err(refused("the challenge does not match"));
        }

        let origin = data["origin"].as_str().unwrap_or_default();
        if !self.origins.iter().any(|allowed| allowed == origin) {
            return Err(refused(&format!("the origin `{origin}` is not allowed")));
        }

        if data["crossOrigin"].as_bool() == Some(true) && !self.allow_cross_origin {
            return Err(refused("a cross-origin ceremony"));
        }

        Ok(())
    }

    fn check_auth_data(&self, data: &AuthenticatorData, uv: UserVerification) -> Result<()> {
        let expected = Sha256::digest(self.id.as_bytes());
        if !bool::from(data.rp_id_hash.ct_eq(expected.as_slice())) {
            return Err(refused("the passkey is for a different site"));
        }
        if data.flags & FLAG_UP == 0 {
            return Err(refused("the user was not present"));
        }
        if uv == UserVerification::Required && data.flags & FLAG_UV == 0 {
            return Err(refused("the user was not verified"));
        }
        if data.flags & FLAG_BS != 0 && data.flags & FLAG_BE == 0 {
            return Err(malformed("backed up but not backup-eligible"));
        }
        Ok(())
    }
}

/// Parsed authenticator data.
struct AuthenticatorData {
    rp_id_hash: Vec<u8>,
    flags: u8,
    sign_count: u32,
    attested: Option<AttestedCredential>,
}

struct AttestedCredential {
    aaguid: [u8; 16],
    id: Vec<u8>,
    public_key: Vec<u8>,
    key: CoseKey,
}

impl AuthenticatorData {
    fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < 37 {
            return Err(malformed("authenticator data shorter than 37 bytes"));
        }
        let rp_id_hash = bytes[..32].to_vec();
        let flags = bytes[32];
        let sign_count = u32::from_be_bytes(bytes[33..37].try_into().expect("four bytes"));
        let mut rest = &bytes[37..];

        let attested = if flags & FLAG_AT != 0 {
            if rest.len() < 18 {
                return Err(malformed("attested credential data is truncated"));
            }
            let aaguid: [u8; 16] = rest[..16].try_into().expect("sixteen bytes");
            let id_len = u16::from_be_bytes([rest[16], rest[17]]) as usize;
            if id_len == 0 || id_len > MAX_CREDENTIAL_ID || rest.len() < 18 + id_len {
                return Err(malformed("a credential id of an impossible length"));
            }
            let id = rest[18..18 + id_len].to_vec();
            rest = &rest[18 + id_len..];

            let (value, used) = cbor::decode_prefix(rest)?;
            let key = CoseKey::from_value(&value)?;
            let public_key = rest[..used].to_vec();
            rest = &rest[used..];

            Some(AttestedCredential { aaguid, id, public_key, key })
        } else {
            None
        };

        if flags & FLAG_ED != 0 {
            match cbor::decode(rest)? {
                Value::Map(_) => {}
                _ => return Err(malformed("extensions that are not a map")),
            }
        } else if !rest.is_empty() {
            return Err(malformed("trailing bytes in authenticator data"));
        }

        Ok(Self { rp_id_hash, flags, sign_count, attested })
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn check_live(expires_at: u64) -> Result<()> {
    if now() > expires_at {
        return Err(refused("the passkey challenge has expired — start again"));
    }
    Ok(())
}

fn same_ids(kind: &str, id: &str, raw_id: &str) -> Result<Vec<u8>> {
    if kind != "public-key" {
        return Err(malformed("a credential that is not `public-key`"));
    }
    let raw = decode(raw_id, "rawId")?;
    if decode(id, "id")? != raw {
        return Err(malformed("`id` and `rawId` disagree"));
    }
    Ok(raw)
}

/// base64url, forgiving padding and the standard alphabet — browsers and
/// libraries disagree about both.
fn decode(text: &str, field: &str) -> Result<Vec<u8>> {
    let normalised: String = text
        .trim_end_matches('=')
        .chars()
        .map(|c| match c {
            '+' => '-',
            '/' => '_',
            other => other,
        })
        .collect();
    B64URL.decode(normalised).map_err(|_| Error::bad_request(format!("`{field}` is not base64url")))
}

/// Lenient base64url decoding, for converters reading another system's
/// stored credentials.
///
/// # Errors
///
/// If the text is not base64 in either alphabet.
pub fn decode_base64url(text: &str) -> Result<Vec<u8>> {
    decode(text, "value")
}

fn malformed(why: &str) -> Error {
    Error::bad_request(format!("invalid passkey response: {why}"))
}

fn refused(why: &str) -> Error {
    Error::unauthorized(format!("passkey refused: {why}"))
}

mod b64 {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
    use base64::Engine as _;
    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&B64URL.encode(bytes))
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(deserializer)?;
        super::decode(&text, "bytes").map_err(serde::de::Error::custom)
    }
}

mod b64_array {
    use serde::{Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(
        bytes: &[u8; 16],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        super::b64::serialize(bytes, serializer)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<[u8; 16], D::Error> {
        let bytes = super::b64::deserialize(deserializer)?;
        bytes.try_into().map_err(|_| serde::de::Error::custom("an AAGUID is 16 bytes"))
    }
}

#[cfg(test)]
mod tests;
