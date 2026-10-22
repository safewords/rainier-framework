# Accounts

[Authentication](authentication.md) gives you guards: a way to say **who** a
request belongs to. Something still has to *make* that true — a registration
form, a login endpoint, the forgotten-password email, the link that proves an
address. `rainier-accounts` is those flows, as routes you mount.

```toml
rainier-framework = { version = "2", features = ["accounts"] }
```

It is headless and JSON-first, with a
`303` for a browser form post, and every application-specific decision behind
a small trait.

## Quick start

A complete, runnable version of everything below is
[`examples/accounts.rs`](../crates/rainier-framework/examples/accounts.rs). It
needs no database, mail server or network. It registers, logs out, logs back in,
resets a forgotten password and verifies the address, printing each answer:

```sh
cargo run -p rainier-framework --example accounts --features accounts
```

**1. Turn the feature on.**

```toml
rainier-framework = { version = "2", features = ["accounts"] }
```

**2. Implement the ports.** Three of them are required and one is optional. The
crate owns the flows and your code owns the rows:

```rust
use rainier_framework::accounts::*;

// Your user model: the address mail goes to, and whether it is proven.
impl AccountUser for User {
    fn email(&self) -> &str { &self.email }
    fn has_verified_email(&self) -> bool { self.email_verified_at.is_some() }
}

// Where users are found and changed.
#[async_trait]
impl AccountUsers<User> for UserStore {
    async fn find_by_id(&self, id: &str) -> Result<Option<User>> { … }
    async fn find_by_email(&self, email: &str) -> Result<Option<User>> { … }
    async fn set_password_hash(&self, user: &User, hash: &str) -> Result<()> { … }
    async fn mark_email_verified(&self, user: &User) -> Result<()> { … }
}

// Registration: the email is checked and the password already hashed.
#[async_trait]
impl CreatesNewUsers<User> for UserStore {
    async fn create(&self, new_user: NewUser) -> Result<User> { … }
}

// Optional: how links reach people. `MailLinks` sends email for you.
```

**3. Wire it and mount the routes.** Wire it inside the session, with CSRF
protection for browser clients:

```rust
let accounts = Arc::new(
    Accounts::new(guard.clone(), users.clone(), hasher, cache)
        .features(Features::all())                          // verification on too
        .creates_users_with(users)
        .mails_links_with(MailLinks::new(mailer, "Acme"))   // reset + verification
        .signs_urls_with(Arc::new(UrlSigner::new(keys)))    // verification links
        .app_url("https://acme.test")
        .home("/dashboard"),
);

router.group(
    GroupAttributes::new().middleware((StartSession::from_manager(&sessions), VerifyCsrfToken::new())),
    |router| {
        accounts.routes(router).expect("configured");       // errors at boot, not at first click
    },
);
```

**4. Gate what needs a proven address:**

```rust
router.get("/billing", billing)
    .middleware(EnsureEmailIsVerified::<User>::new(guard).redirect_to("/verify-email"));
```

That is everything required. The rest of this page explains what each piece guarantees.
For TOTP and passkeys on top of the same flows, see
[Two-factor and passkeys](#two-factor-and-passkeys). Its runnable companion is
`examples/two_factor.rs` (`--features accounts,two-factor,passkeys`).

## Why routes, not generated code

A generator copies controllers into your application once, and from then on
they are yours — including their bugs. A fix to the reset flow reaches nobody
who already ran it.

The parts of these flows that go wrong go wrong the same way in every
application: a failed login that answers faster for an unknown address than a
known one, a reset token stored in plaintext, a throttle message that confirms
an address has an account, a "back" redirect that follows a forged `Referer`
off-site. Those live in the crate and are fixed once. What genuinely differs —
your user table, your extra registration fields, how your mail goes out — is a
trait you implement.

## Wiring it up

```rust
use rainier_framework::accounts::{Accounts, Features, MailLinks};

let accounts = Arc::new(
    Accounts::new(guard, users, hasher, cache)
        .creates_users_with(Arc::new(CreateUser))
        .mails_links_with(MailLinks::new(mailer, "Acme"))
        .signs_urls_with(Arc::new(UrlSigner::new(keys)))
        .features(Features::all())
        .app_url("https://acme.test")
        .home("/dashboard")
        .events(events)
        .middleware(web()), // whatever starts the session
);

accounts.routes(&mut router)?;
```

| Argument | Is |
|---|---|
| `guard` | the `StatefulGuard` users are logged into — usually your `SessionGuard` |
| `users` | your `AccountUsers<User>` |
| `hasher` | the application's `Hasher` |
| `cache` | where login throttles, reset tokens and the resend throttle live — **the shared one** in a multi-replica deployment |

`routes` returns an error at boot if an enabled flow is missing what it needs —
registration without `creates_users_with`, verification without a signer — so
the omission surfaces when the process starts, not at the first click.

### It needs a session

Login *is* a session. Put `StartSession` in front — through `.middleware(…)`,
or by declaring the routes inside a group that already has it. CSRF protection
for browser form posts is your group middleware, exactly as for any other form.

If the routes are declared inside a prefixed group of your own, tell the crate
with `.mounted_under("/api")`: routes cannot see the groups around them, and
the verification link has to point at the full path.

## The routes

| Method | Path | Name | Feature |
|---|---|---|---|
| `POST` | `/register` | `accounts.register` | `registration` |
| `POST` | `/login` | `accounts.login` | always |
| `POST` | `/logout` | `accounts.logout` | always |
| `POST` | `/forgot-password` | `accounts.password.email` | `reset_passwords` |
| `POST` | `/reset-password` | `accounts.password.update` | `reset_passwords` |
| `GET` | `/email/verify/{id}/{hash}` | `accounts.verification.verify` | `email_verification` |
| `POST` | `/email/verification-notification` | `accounts.verification.send` | `email_verification` |
| `POST` | `/user/confirm-password` | `accounts.password.confirm` | `password_confirmation` |
| `PUT` | `/user/password` | `accounts.user-password.update` | `update_passwords` |

`.prefix("auth")` moves them all under `/auth`; `.names("auth.")` renames
them. A disabled feature is **not mounted** — a `404`, not a `403` — because a
route that does not exist cannot be attacked.

Each flow is also a public method on `Accounts` (`login`, `register`,
`reset_password`, …), so one that does not fit the defaults can be mounted at a
path of your own or wrapped in a controller that does something first.

## What you implement

```rust
impl AccountUser for User {
    fn email(&self) -> &str { &self.email }
    fn has_verified_email(&self) -> bool { self.email_verified_at.is_some() }
}

#[async_trait]
impl AccountUsers<User> for UserStore {
    async fn find_by_id(&self, id: &str) -> Result<Option<User>> { … }
    async fn find_by_email(&self, email: &str) -> Result<Option<User>> { … }
    async fn set_password_hash(&self, user: &User, hash: &str) -> Result<()> { … }
    async fn mark_email_verified(&self, user: &User) -> Result<()> { … } // only for verification
}

#[async_trait]
impl CreatesNewUsers<User> for CreateUser {
    async fn create(&self, new_user: NewUser) -> Result<User> {
        // new_user.email: validated, trimmed, lowercased, not already taken
        // new_user.password_hash: validated, confirmed, hashed
        // new_user.input: everything else — validate your own fields here
    }
}
```

`NewUser` never carries the plaintext password, so it cannot end up in a row,
a log line or an event by accident. Return `Error::validation(json!({ "name":
["…"] }))` from `create` and the caller gets a `422` shaped exactly like the
crate's own.

## What it holds the line on

### Login

- **Timing.** An unknown address, or an account with no password, still costs
  a hash verification (`Hasher::dummy_verify`), so latency does not say which
  addresses have accounts. The wording is identical too.
- **Throttling.** Failures are counted per address **and** client IP in the
  cache; after `max_login_attempts` within `login_decay` the address is locked
  out with `429` and an accurate `Retry-After` — even for the right password,
  which is the point. A success clears the count. Put `TrustProxies` in front
  or every client shares the load balancer's address.
- **Fixation.** Logging in rotates the session id (the guard does this).
- **Hash upgrades.** A hash written under weaker parameters is rehashed while
  the plaintext is in hand — the only moment it ever is.

### Password reset

`PasswordBroker` issues 256-bit tokens, stores **SHA-256 of them** (never the
token), keeps one live per account, consumes them on use, expires them by the
cache TTL (no table, no sweep job) and refuses a second link inside
`reset_throttle`.

`POST /forgot-password` answers identically for every address, and swallows
its own downstream failures into the log — a mail server error for a real
account must not answer differently from silence for a fake one.

> **The delivery is off the request.** Identical *words* are not identical
> *timing*: a sender that talks to SMTP inline spends a few hundred
> milliseconds the unknown-address path does not. So by default the reset
> link is handed to a detached task and the answer goes back without waiting
> for it. The lookup and the token write stay inline — one round trip each, to
> stores every request touches anyway. A detached delivery does not survive a
> crash mid-send; when that matters, give `sends_reset_links_with` a sender
> that dispatches a queued job, which is both fast and durable.
> `deliver_links_inline(true)` restores inline delivery, which is what a test
> asserting on the outbox wants.

Changing the password through `PUT /user/password` revokes any outstanding
reset link, so an old email cannot undo the change, and rotates the session id.

### Email verification

Links are `/email/verify/{id}/{hash}?expires=…&signature=…`, signed by your
`UrlSigner`. `hash` is SHA-256 of the address the link was sent **to**, so a
link sent before an address change does not verify the new one. Stateless:
nothing is stored, nothing to sweep — and rotating the application key retires
every outstanding link, which is worth knowing before you rotate one.

The link works without being signed in, so it can be opened on another device;
if somebody *is* signed in, it must be the same account.

Gate routes on it with `EnsureEmailIsVerified`:

```rust
router.get("/billing", billing)
    .middleware(EnsureEmailIsVerified::<User>::new(guard).redirect_to("/verify-email"));
```

A JSON client gets `403`; a browser is sent to the notice page.

### Password confirmation

`POST /user/confirm-password` is the endpoint `ConfirmPassword`'s `423` points
at. Guard the account-takeover actions — changing the address, adding a second
factor, issuing a token — with `ConfirmPassword::recently()`.

## Events

Dispatched with `dispatch_quietly`, so a failing welcome mail does not turn a
written registration into a `500`.

| Event | When |
|---|---|
| `Registered<U>` | a user registered |
| `Login<U>` | a password login succeeded |
| `Failed` | a login was refused (carries the address, not a user) |
| `Lockout` | the throttle locked an address out |
| `Logout<U>` | a signed-in user logged out |
| `PasswordReset<U>` | a reset link was used — send the "your password changed" mail here |
| `PasswordUpdated<U>` | a signed-in user changed their password |
| `Verified<U>` | an address was verified |

## Responses

A request that expects JSON gets JSON and a status (`201` on registration,
`202` on a resend, `204` on logout and confirmation). A browser form post gets
a `303` to `home`, to `login_page` after a reset, or back to the form — only
ever to a path on this site. Failures are ordinary framework errors (`422`,
`401`, `403`, `429`) and render the way every other error in the application
does.

## Two-factor and passkeys

Two optional features of the crate add a second factor to these flows —
`two-factor` (TOTP with recovery codes) and `passkeys` (WebAuthn). Each is a
switch in `Features`, a port you implement, and five more routes; with
two-factor on, `/login` answers `{"two_factor": true}` instead of a session
for an enrolled user. See [Two-Factor Authentication and Passkeys](two-factor.md).

## Not here

- **Signing out other sessions** after a password reset. The session store
  does not index sessions by user, so there is nothing to enumerate; a
  `PasswordReset` listener is the place to do it if your store can.
- **Views.** The crate is headless. Your front end renders the forms; the
  reset link template (`.reset_url(…)`) points at its page.
- **Social login** — see [single sign-on](sso.md).
