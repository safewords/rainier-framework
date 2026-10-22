# Two-Factor Authentication and Passkeys

A second factor, in two shapes:

- **TOTP** — the six-digit code from an authenticator app, with recovery codes
  for when the phone is gone.
- **Passkeys** — WebAuthn credentials held by the device or a password
  manager, which are possession *and* verification in one gesture.

Both live in `rainier-mfa`: the cryptography and the ceremonies, with no
routes, tables or sessions. `rainier-accounts` puts them behind the login with
two optional features. Use the scaffolding, or call `rainier-mfa` from flows of
your own.

Neither needs C or OpenSSL: the signature algorithms are RustCrypto, and the
CBOR an authenticator speaks is read by a small strict decoder in the crate.

## Quick start

> **Run it first:** `cargo run -p rainier-framework --example two_factor
> --features accounts,two-factor,passkeys` plays the whole thing offline —
> TOTP on its own, then enrol → log out → log in → challenge → signed in, then
> register a passkey and sign in with it — printing every request and answer.
> The source is
> [`crates/rainier-framework/examples/two_factor.rs`](../crates/rainier-framework/examples/two_factor.rs).

**1. Turn the features on.**

```toml
rainier-framework = { version = "2", features = ["accounts", "two-factor", "passkeys"] }
```

| Feature | Turns on |
|---|---|
| `two-factor` | `rainier_framework::mfa::{totp, recovery}`; with `accounts` also on, the TOTP challenge and routes |
| `passkeys` | `rainier_framework::mfa::webauthn`; with `accounts` also on, the passkey routes |

Either works without `accounts`, for an application with login flows of its
own — see [TOTP](#totp) and [Passkeys](#passkeys) below.

**2. Implement two ports** — where enrolments and passkeys live:

```rust
#[async_trait]
impl TwoFactorUsers<User> for Enrolments {
    async fn find_two_factor(&self, user: &User) -> Result<Option<TwoFactorRecord>> { … }
    async fn save_two_factor(&self, user: &User, record: Option<&TwoFactorRecord>) -> Result<()> { … }
}

#[async_trait]
impl PasskeyRepository<User> for PasskeyTable {
    async fn passkeys_for(&self, user: &User) -> Result<Vec<PasskeyCredential>> { … }
    async fn find_passkey(&self, credential_id: &[u8]) -> Result<Option<StoredPasskey>> { … }
    async fn store_passkey(&self, user: &User, name: &str, c: &PasskeyCredential) -> Result<()> { … }
    async fn update_passkey(&self, c: &PasskeyCredential) -> Result<()> { … }
    async fn delete_passkey(&self, user: &User, credential_id: &[u8]) -> Result<bool> { … }
}
```

A `TwoFactorRecord` is four fields (secret, confirmed, hashed recovery codes,
last used step) and a `PasskeyCredential` serialises to one JSON column —
[what you implement](#what-you-implement) has the details.

**3. Wire them into the accounts scaffolding.**

```rust
use rainier_framework::accounts::{Accounts, Features};
use rainier_framework::mfa::{RelyingParty, Totp};

let accounts = Arc::new(
    Accounts::new(guard, users, hasher, cache)
        .features(Features { two_factor: true, passkeys: true, ..Features::default() })
        .two_factor_with(Arc::new(Enrolments::new(db.clone())), Totp::new().issuer("Acme"))
        .passkeys_with(
            Arc::new(PasskeyTable::new(db)),
            RelyingParty::new("acme.com", "Acme").origins(["https://app.acme.com"]),
        )
        .middleware(web()),            // whatever starts the session
);
accounts.routes(&mut router)?;         // fails here, at boot, if a piece is missing
```

**4. What the client does.**

*Enrolling* (signed in, password confirmed via `POST /user/confirm-password`):

1. `POST /user/two-factor-authentication` → `{secret, otpauth_url, recovery_codes}`.
   Render `otpauth_url` as a QR code; show the recovery codes once.
2. `POST /user/confirmed-two-factor-authentication` `{code}` with the first
   code the app shows. Until this succeeds, login does not ask for a code.

*Logging in*:

1. `POST /login` `{email, password}`. For an enrolled user the answer is
   `{"two_factor": true}` — show the code form; the session is not logged in.
2. `POST /two-factor-challenge` `{code}` or `{recovery_code}` →
   `{"authenticated": true}`.

*Passkeys* — the options and answers are JSON the browser parses directly:

```js
// register (signed in, password confirmed)
const options = await post("/user/passkeys/options");
const created = await navigator.credentials.create({
  publicKey: PublicKeyCredential.parseCreationOptionsFromJSON(options),
});
await post("/user/passkeys", { credential: created.toJSON(), name: "This laptop" });

// sign in (from an empty form — the browser offers the passkeys it holds)
const request = await post("/passkeys/login/options");
const assertion = await navigator.credentials.get({
  publicKey: PublicKeyCredential.parseRequestOptionsFromJSON(request),
});
await post("/passkeys/login", { credential: assertion.toJSON() });   // → {"authenticated": true}
```

The full route table is under [the routes](#the-routes).

---

## TOTP

```rust
use rainier_mfa::{Secret, Totp};

let totp = Totp::new().issuer("Acme");       // SHA-1, 6 digits, 30 s, ±1 step
let secret = Secret::generate();             // 160 bits
let uri = totp.provisioning_uri(&secret, "ada@example.com");   // render as a QR code

// At login — `last_used_step` is what the previous success returned:
match totp.verify(&secret, &submitted, record.last_used_step) {
    Some(step) => record.last_used_step = Some(step),   // persist it
    None => /* wrong, malformed, or already used */,
}
```

### A code works once

A TOTP code is valid for its whole step — ninety seconds, with the usual
one-step window — so a code read over a shoulder or lifted by a phishing page
replays for that long unless the verifier refuses it. RFC 6238 says it must.
`verify` takes the last accepted step and refuses anything at or before it;
**persist the step it returns**, or replay protection is not there.

The stored step stops replays that come after a login finishes. The accounts
scaffolding also claims each step atomically in the cache (`add`), so two
requests racing with one code cannot both win.

### The defaults are the apps' defaults

SHA-1, six digits and thirty seconds are what every authenticator app assumes
when the URI says nothing else. `Totp` can do SHA-256/512, eight digits and
other periods, because RFC 6238 defines them, but several popular apps
silently ignore those parameters and produce codes that never match. Leave
them alone unless your users carry hardware tokens that need them.

Every step in the window is computed and compared in constant time, so a
guess's response time says nothing about how close it was.

### Recovery codes

```rust
use rainier_mfa::recovery;

let codes = recovery::generate(8);                    // show once
let mut stored: Vec<String> = codes.iter().map(|c| recovery::hash(c)).collect();
let ok = recovery::consume(&mut stored, &submitted);  // removes it on a match
```

Two groups of ten alphanumerics, about 119 bits — stored as SHA-256, since a
table of plaintext recovery codes is a table of second-factor bypasses for
whoever reads a backup. Entropy that high needs no slow hash.
`consume_plain` accepts codes an earlier system kept readable, while you
re-store them hashed.

---

## Passkeys

```rust
use rainier_mfa::webauthn::*;

let rp = RelyingParty::new("example.com", "Example")
    .origin("https://app.example.com");      // every origin a ceremony may run on

// Registration
let (options, state) = rp.start_registration(&PasskeyUser::new(handle, "ada", "Ada"), &existing);
session.put("passkey", &state)?;             // the challenge lives in the session
// browser: navigator.credentials.create({ publicKey: PublicKeyCredential.parseCreationOptionsFromJSON(options) })
let credential = rp.finish_registration(&state, &RegistrationResponse::from_json(&body)?)?;

// Sign-in
let (options, state) = rp.start_authentication(&[]);   // discoverable: any passkey for this site
let response = AuthenticationResponse::from_json(&body)?;
let mut credential = find(&response.credential_id()?)?;
rp.finish_authentication(&state, &response, &credential)?.apply_to(&mut credential);
```

Options are the `…OptionsJSON` shape the browser's `parse…FromJSON` methods
take, and responses are what `PublicKeyCredential.toJSON()` produces — so the
front end passes JSON through without touching a byte.

### What is verified

Everything the WebAuthn Level 3 procedures require of a relying party that
does not demand attestation: the client data's type, challenge (constant
time), origin (exact match against your list) and `crossOrigin`; the RP ID
hash; user presence; user verification when required; backup flags that agree
with each other; the algorithm against those offered; and, at sign-in, the
signature, that the credential was one the challenge allowed, that the user
handle matches, and that the signature counter moved forward. A counter that
goes backwards means a cloned authenticator, and sign-in is refused; synced
passkeys report zero throughout, which is allowed.

ES256, EdDSA and RS256 cover every passkey provider and security key in use.
The test suite verifies real browser responses from py_webauthn's published
vectors — a `none` registration, an ES256 sign-in and a Windows Hello RS256
sign-in — alongside a software authenticator's.

### Attestation is accepted, not trusted

Registration asks for `attestation: "none"`, which is what passkey providers
send. A `none` statement must be empty, and `packed` self-attestation is
verified with the credential's own key. Other formats are claims about the
authenticator's make that only mean something checked against the FIDO
Metadata Service, which this crate does not consult:
`AttestationPolicy::Lenient` (the default) accepts them and records the format
as `unverified:<fmt>`; `Strict` refuses them. If you issue hardware keys to
staff and need to know they are the ones you bought, you need the metadata
service, and this is not that.

### User verification is required by default

A passkey with user verification is two factors: the device proves
possession, and its PIN or biometric proves the person. That is why the
accounts scaffolding does not put a passkey sign-in through the TOTP
challenge. Relax `user_verification` and that stops being true.

### Testing your flows

`webauthn::testing::SoftwareAuthenticator` answers the options a relying party
issues the way a real authenticator would — authenticator data, CBOR,
signature, `toJSON()` output — as a `serde_json::Value` you can bend:

```rust
let mut authenticator = SoftwareAuthenticator::es256("example.com", "https://example.com");
let created = authenticator.register(&options);
authenticator.set_origin("https://examp1e.com");     // and assert the phishing page fails
```

---

## In the accounts scaffolding

```rust
Accounts::new(guard, users, hasher, cache)
    .features(Features { two_factor: true, passkeys: true, ..Features::default() })
    .two_factor_with(Arc::new(Enrolments), Totp::new().issuer("Acme"))
    .passkeys_with(Arc::new(PasskeyTable), RelyingParty::new("example.com", "Example"))
```

### The routes

| | | | |
|---|---|---|---|
| `POST` | `/two-factor-challenge` | `two-factor.login` | `code` or `recovery_code` |
| `POST` | `/user/two-factor-authentication` | `two-factor.enable` | † answers `secret`, `otpauth_url`, `recovery_codes` |
| `POST` | `/user/confirmed-two-factor-authentication` | `two-factor.confirm` | `code` |
| `DELETE` | `/user/two-factor-authentication` | `two-factor.disable` | † |
| `POST` | `/user/two-factor-recovery-codes` | `two-factor.recovery-codes` | † answers new `recovery_codes` |
| `POST` | `/passkeys/login/options` | `passkeys.login-options` | |
| `POST` | `/passkeys/login` | `passkeys.login` | `credential` |
| `POST` | `/user/passkeys/options` | `passkeys.register-options` | † |
| `POST` | `/user/passkeys` | `passkeys.store` | † `credential`, `name` |
| `DELETE` | `/user/passkeys/{id}` | `passkeys.destroy` | † base64url credential id |

† needs a password confirmed in the last fifteen minutes (`POST
/user/confirm-password`), and answers `423` otherwise — the same answer
`ConfirmPassword` gives, so a client handles both alike. A session left open
on a shared machine cannot quietly add or remove a factor.

### A login with a second factor

`/login` checks the password exactly as before. For a user with a
**confirmed** enrolment it then rotates the session id, records a pending
login (the user id and an expiry — `Features::two_factor_timeout`, five
minutes) and answers `{"two_factor": true}`, or redirects a browser to
`/two-factor-challenge`. Until a right code or recovery code arrives, the
session belongs to nobody: a stolen password alone reaches nothing.

Wrong codes are throttled per pending login (`two_factor_attempts`, then a
`429` with `Retry-After` for `login_decay`). An enrolment that was started but
never confirmed does not challenge — locking a user out over a typo during
setup would be the scaffold's fault.

### What you implement

```rust
#[async_trait]
impl TwoFactorUsers<User> for Enrolments {
    async fn find_two_factor(&self, user: &User) -> Result<Option<TwoFactorRecord>>;
    async fn save_two_factor(&self, user: &User, record: Option<&TwoFactorRecord>) -> Result<()>;
}
```

`TwoFactorRecord` is the base32 secret, whether it is confirmed, the hashed
recovery codes and the last used step. **Encrypt the secret at rest.**

```rust
#[async_trait]
impl PasskeyRepository<User> for PasskeyTable {
    async fn passkeys_for(&self, user: &User) -> Result<Vec<PasskeyCredential>>;
    async fn find_passkey(&self, credential_id: &[u8]) -> Result<Option<StoredPasskey>>;
    async fn store_passkey(&self, user: &User, name: &str, credential: &PasskeyCredential) -> Result<()>;
    async fn update_passkey(&self, credential: &PasskeyCredential) -> Result<()>;
    async fn delete_passkey(&self, user: &User, credential_id: &[u8]) -> Result<bool>;
    fn passkey_user(&self, user: &User) -> PasskeyUser { /* defaults to the user id as handle */ }
}
```

`PasskeyCredential` serialises to JSON, so one column beside a user id and a
name holds it; index `id_base64url()` for sign-in. Override `passkey_user` to
return a random handle stored on the user if your identifiers are personal
data — authenticators store the handle and may display it.

A refused passkey sign-in answers one `401` whatever the reason — unknown
credential, deleted user, bad signature — so the endpoint does not reveal
which credential ids exist. The reason goes to the log.

### Events

`TwoFactorChallenged`, `TwoFactorFailed`, `RecoveryCodeUsed` (with how many
remain — worth a security email), `TwoFactorEnabled`, `TwoFactorDisabled`,
`PasskeyRegistered`, `PasskeyUsed`, `PasskeyDeleted`, alongside the existing
`Login`.

### A flag without its feature fails at boot

`Features { two_factor: true, .. }` in a build without the `two-factor`
feature, or without `two_factor_with`, makes `accounts.routes(…)` return an
error naming what is missing. A switch is never a silent no-op.

---

## Migrating from PHP

Existing enrolments and passkeys carry over without anyone re-enrolling — see
[TOTP secrets](compat.md#totp-secrets) and
[Passkeys](compat.md#passkeys--phpwebauthn) in the compatibility guide.

## Not here

- **Emailed or texted codes.** `rainier_auth::Challenges` is the primitive;
  SMS is a weaker factor than either of these and is left to the application.
- **Attestation trust** — the FIDO Metadata Service. See above.
- **Signing out other sessions** when a factor is added or removed; the session
  store does not index sessions by user. A `TwoFactorEnabled` listener is the
  place, if yours can.
