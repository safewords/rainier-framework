# Choosing Add-ins

The framework's core — container, router, guards, queue, mail, cache — is
always there. Everything on this page is **opt-in**: a cargo feature you name,
or nothing of it is compiled. That is a sizing decision as much as a security
one: an application that does not bill does not link a Stripe client, and one
that does not issue tokens has no `/oauth/token` to misconfigure.

---

## The features

| Feature | Brings in | Page | Example |
|---|---|---|---|
| `billing-stripe` | `rainier-billing`, `http-client` | [Billing](billing.md) | `billing_stripe` |
| `sso` | `rainier-sso`, `http-client`, `jwt` | [Single Sign-On](sso.md) | `sso_login` |
| `accounts` | `rainier-accounts` | [Accounts](accounts.md) | `accounts` |
| `two-factor` | `rainier-mfa` (TOTP); the challenge routes in `accounts` when it is on | [Two-Factor and Passkeys](two-factor.md) | `two_factor` |
| `passkeys` | `rainier-mfa` (WebAuthn); the passkey routes in `accounts` when it is on | [Two-Factor and Passkeys](two-factor.md) | `two_factor` |
| `oauth-server` | `rainier-oauth-server` | [OAuth Server](oauth-server.md) | `oauth_server` |
| `compat-php` | `rainier-compat` (`php::*`) | [Compatibility](compat.md) | `compat_php` |
| `compat-php-oauth` | `compat-php`, `oauth-server`, and `php::oauth` | [Compatibility](compat.md) | `compat_php` |
| `compat-php-webauthn` | `compat-php`, `passkeys`, and `php::webauthn` | [Compatibility](compat.md) | `compat_php` |
| `telemetry` | a `telemetry` feature in every component, and the OTLP exporter | [Observability](observability.md#passive-instrumentation) | `telemetry` |
| `captcha` | `http-client` | [Human Checks](captcha.md) | `captcha` |
| `notify-discord` | `rainier-notify/discord`, `http-client` | [Notifications](notifications.md#discord) | `notify_discord` |

Impersonation needs no feature; it is part of `rainier-auth` — see
[Authentication](authentication.md#impersonation) and the `impersonation`
example.

### Turning one on

```toml
[dependencies]
rainier-framework = { version = "2", features = ["accounts", "two-factor", "sso", "telemetry"] }
```

The `rainier` facade crate mirrors every one of these names, so `rainier = {
version = "2", features = [...] }` is the same request.

Each add-in is then reached through the framework's re-exports —
`rainier_framework::billing`, `::sso`, `::accounts`, `::mfa`,
`::oauth_server`, `::compat` — so an application has one dependency to keep
in step, not ten.

### Sizing a build

`cargo rainier features` reads the deployment's environment file and prints
the feature set to build with. Add-ins are **named**, not inferred, in
`RAINIER_ADDINS`:

```env
# .env.build — read by the build, not by the running container
RAINIER_ADDINS=accounts,two-factor,sso,billing-stripe
```

```sh
cargo build --release --no-default-features --features "$(cargo rainier features --env .env.build --list)"
```

Why named: an application often keeps the code for an add-in behind its own
`#[cfg(feature = ...)]`, so finding `billing::Stripe` in the source says
nothing about whether this deployment bills. Inferring from text would build
everything the repository *could* do — and an add-in that fails closed at boot
without its keys, like the OAuth server without a signing key, would fail the
deploy. An unknown name is reported rather than ignored. `TELEMETRY_ENABLED=true`
still asks for `telemetry` on its own. See
[`rainier-features`](../crates/rainier-features).

---

## Running the examples

Every add-in has one, under `crates/rainier-framework/examples/`, and every
one runs **offline**: in-memory stores, the HTTP fake standing in for Stripe,
GitHub or Discord, a generated key where a real one would be configured.

```sh
cargo run -p rainier-framework --example accounts --features accounts
cargo run -p rainier-framework --example two_factor --features accounts,two-factor,passkeys
cargo run -p rainier-framework --example billing_stripe --features billing-stripe
cargo run -p rainier-framework --example oauth_server --features oauth-server
cargo run -p rainier-framework --example compat_php --features compat-php-oauth,compat-php-webauthn
cargo run -p rainier-framework --example telemetry --features telemetry
```

They are documentation that compiles: CI's `clippy --all-targets
--all-features` builds every one, so an example cannot quietly fall behind the
API it shows. Read the file top to bottom — each prints what it is doing as it
goes.

---

## How they compose

The add-ins are separate crates that meet at the framework's own seams — the
guard, the session, the event bus, the cache — rather than at each other.
Some combinations are worth knowing:

**A product with sign-up, social login and a second factor.** `accounts` +
`sso` + `two-factor` (+ `passkeys`). The accounts routes log users into the
same `SessionGuard` that `sso::login_with` logs into, so a user created by
"Sign in with GitHub" and one who registered with a password are the same kind
of row. With `two-factor` on, a password login for an enrolled user stops at
the challenge; a passkey sign-in has already proven possession and does not.

**A paid product.** `billing-stripe` + `accounts`. Implement `Billable` on the
user, send Checkout and portal sessions from your controllers, and listen for
the typed webhook events — `CustomerSubscriptionUpdated` carries a
`SubscriptionSnapshot` to persist, and that snapshot's `valid()` is the gate.
The webhook route is outside the session and CSRF middleware; its signature
is its authentication.

**A platform others build on.** `oauth-server` + `accounts`. The server
issues tokens; `OAuthGuard` accepts them on your API routes, with each
token's scopes arriving as the [abilities](authentication.md#token-abilities)
that `RequireAbility` already checks.

**Migrating an application to Rainier.** `compat-php` (+ `compat-php-oauth`,
`compat-php-webauthn`), usually with `telemetry` on from the first day. The
two applications share the database, the cache and the keys; Rainier adopts
the other side's sessions, CSRF tokens, OAuth tokens and passkeys as they
arrive, so routes can move across one at a time and users never notice which
side answered. [Compatibility](compat.md) has the order to do it in.

**Anything in production.** `telemetry`. Set `TELEMETRY_ENABLED=true` and
`OTEL_EXPORTER_OTLP_ENDPOINT`, and every request, query, cache call, job,
mail and event is a span, with queue wait and throughput as metrics — no code.

---

## What they share

Every add-in follows the framework's rules, so none of these needs relearning:

- **Outbound calls go through the HTTP transport port**, so a test hands the
  add-in a [`FakeTransport`](http-client.md) and asserts on the exact request
  — no network, no mock server.
- **Secrets stay out of `Debug`** and out of logs.
- **Refusals fail closed** — an unverifiable webhook, an unreachable captcha
  provider, a passkey with a regressed counter, a token whose row is gone.
- **Misconfiguration fails at boot**, not on the first request: a feature
  switched on without what it needs is an error from `routes()` or `boot()`.
- **Events, not callbacks**, wherever the framework's event bus is in reach —
  `Registered`, `TwoFactorFailed`, `CheckoutSessionCompleted` — so auditing,
  mail and analytics hang off them without the add-in knowing.
