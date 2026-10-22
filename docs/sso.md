# Single sign-on

"Sign in with GitHub", "Sign in with Google", "Sign in with your company
account" — OAuth 2.0 and OpenID Connect, in `rainier-sso`, behind the
framework's `sso` feature.

```toml
rainier-framework = { version = "2", features = ["sso"] }
```

The shape is two calls — `redirect()`, then `user()` — and the defaults are
the safe ones: PKCE always, `state` checked and consumed, ID tokens verified,
no account linking by an unverified email. [Safe by default](#safe-by-default)
lists them, and each one closes a hole some application has shipped.

## Quick start

A runnable version of everything below — config, both routes, a full GitHub
round trip against a fake transport, and the takeover refusal — is
[`crates/rainier-framework/examples/sso_login.rs`](../crates/rainier-framework/examples/sso_login.rs):

```sh
cargo run -p rainier-framework --example sso_login --features sso
```

**1. Turn on the feature.** It brings the outbound HTTP client and JWT
verification with it.

```toml
rainier-framework = { version = "2", features = ["sso"] }
```

**2. Put the secrets in `.env`:**

```env
GITHUB_CLIENT_ID=Iv1.…
GITHUB_CLIENT_SECRET=…
OIDC_ISSUER=https://login.example.com
OIDC_CLIENT_ID=…
OIDC_CLIENT_SECRET=…
APP_URL=https://app.example.com
```

**3. Declare the providers and build them** once, at boot. An `SsoConfig` is
plain serde, so it can come from a configuration section
(`app.config().get("services.sso")?`) or be assembled from the environment as
here. A declaration that cannot work fails at this point, not on the first click.

```rust
use rainier_framework::http_client::ReqwestTransport;
use rainier_framework::sso::{login_with, Sso, SsoConfig};

let url = env.string("APP_URL", "http://localhost:8000");
let config: SsoConfig = serde_json::from_value(json!({
    "github": {
        "client_id": env.require("GITHUB_CLIENT_ID")?,
        "client_secret": env.require("GITHUB_CLIENT_SECRET")?,
        "redirect": format!("{url}/auth/github/callback"),
    },
    "work": {
        "driver": "oidc",
        "issuer": env.require("OIDC_ISSUER")?,
        "client_id": env.require("OIDC_CLIENT_ID")?,
        "client_secret": env.require("OIDC_CLIENT_SECRET")?,
        "redirect": format!("{url}/auth/work/callback"),
    },
}))?;
let sso = Sso::from_config(&config, Arc::new(ReqwestTransport::new()))?;
```

**4. Mount the two routes** behind the session middleware:

```rust
router.get("/auth/{provider}/redirect", move |request: Req| {
    let sso = sso.clone();
    async move { sso.driver(request.route_param("provider").unwrap_or_default())?.redirect(&request).await }
});

router.get("/auth/{provider}/callback", move |request: Req| {
    let (sso, guard, accounts) = (sso.clone(), guard.clone(), accounts.clone());
    async move {
        let social = sso.driver(request.route_param("provider").unwrap_or_default())?.user(&request).await?;
        login_with(&*guard, &*accounts, &request, &social).await?;
        Ok::<_, Error>(Redirect::to("/dashboard"))
    }
});
```

**5. Implement `SocialAccounts`** over your users and a `social_accounts`
table — [Linking accounts](#linking-accounts) — and `login_with` finds, links
or creates the user and logs them in on your session guard.

## How a sign-in flows

```mermaid
sequenceDiagram
    participant B as Browser
    participant A as Your app
    participant P as Provider

    B->>A: GET /auth/github/redirect
    A->>A: state, PKCE verifier, nonce → session
    A-->>B: 302 to provider (state, S256 challenge, nonce)
    B->>P: sign in, consent
    P-->>B: 302 /auth/github/callback?code&state
    B->>A: GET /auth/github/callback
    A->>A: pull pending from session, compare state
    A->>P: POST token (code + verifier)
    P-->>A: access token (+ ID token)
    A->>A: verify ID token: JWKS, iss, aud, exp, nonce
    A->>A: login_with — find, link or create; guard.login
```

## Declaring providers

Providers are configuration. A `github` entry needs no `driver`; anything else
names one of `github`, `gitlab`, `discord`, `x` (alias `twitter`), `google`,
`microsoft` or `oidc`.

```json
{
  "github":  { "client_id": "Iv1.…", "client_secret": "…",
               "redirect": "https://app.example.com/auth/github/callback" },
  "google":  { "client_id": "….apps.googleusercontent.com", "client_secret": "…",
               "redirect": "https://app.example.com/auth/google/callback",
               "parameters": { "prompt": "select_account" } },
  "azure":   { "driver": "microsoft", "tenant": "contoso.onmicrosoft.com",
               "client_id": "…", "client_secret": "…",
               "redirect": "https://app.example.com/auth/azure/callback" },
  "work":    { "driver": "oidc", "issuer": "https://login.example.com",
               "client_id": "…", "client_secret": "…",
               "redirect": "https://app.example.com/auth/work/callback",
               "scopes": ["openid", "email", "profile", "offline_access"] }
}
```

```rust
use rainier_framework::sso::{Sso, SsoConfig};

let config: SsoConfig = app.config().get("services.sso")?;
let sso = Sso::from_config(&config, http_transport)?;
```

A declaration that cannot work — an `oidc` entry with no `issuer`, a driver
nobody has heard of — fails `from_config`, at boot, rather than on the first
person who clicks the button.

| key | meaning |
|---|---|
| `driver` | which built-in; defaults to the entry's name |
| `client_id`, `client_secret` | the registration |
| `redirect` (or `redirect_uri`) | the callback URL, exactly as registered |
| `scopes` | replaces the driver's default scopes |
| `parameters` | extra authorize-URL parameters (`prompt`, `hd`, `login_hint`, `access_type`) |
| `issuer` | `oidc`: the issuer to discover |
| `tenant` | `microsoft`: a tenant id or domain, or `common` / `organizations` / `consumers` |
| `base_url` | `github` / `gitlab`: a self-hosted instance |
| `basic_auth` | authenticate to the token endpoint with HTTP Basic |

Okta, Auth0, Keycloak, Authentik, Zitadel and safewords identity are all
`oidc` with their issuer URL.

## The two routes

```rust
use rainier_framework::sso::{login_with, LoginOutcome};

router.group(GroupAttributes::new().middleware(["web"]), |router| {
    let sso = sso.clone();
    router.get("/auth/{provider}/redirect", move |request: Req| {
        let sso = sso.clone();
        async move {
            let provider = sso.driver(request.route_param("provider").unwrap_or_default())?;
            provider.redirect(&request).await
        }
    });

    router.get("/auth/{provider}/callback", move |request: Req| {
        let (sso, guard, accounts) = (sso.clone(), guard.clone(), accounts.clone());
        async move {
            let provider = sso.driver(request.route_param("provider").unwrap_or_default())?;
            let social = provider.user(&request).await?;
            login_with(&*guard, &*accounts, &request, &social).await?;
            Ok::<_, Error>(Redirect::to("/dashboard"))
        }
    });
});
```

Both routes need the **session middleware**. Without it there is nowhere to
keep the `state`, and `redirect` refuses rather than running unprotected.

`sso.driver(name)` answers a `404` for a name nobody declared — it is a route
parameter, so an unknown one is a bad URL rather than a server fault.

## What comes back

```rust
pub struct SocialUser {
    pub provider: String,             // "github"
    pub id: String,                   // the provider's stable id — link by this
    pub email: Option<String>,
    pub email_verified: Option<bool>, // Some(true) only when the provider says so
    pub name: Option<String>,
    pub nickname: Option<String>,
    pub avatar: Option<String>,
    pub token: AccessToken,           // access, refresh, expiry, granted scopes, id_token
    pub raw: Value,                   // everything, unmapped
}
```

`token.scopes` is what was **granted**, which can be less than what was asked —
a person can untick a box on GitHub's consent screen. Check
`token.has_scope("repo")` before relying on one.

`AccessToken` and every credential type print `[redacted]` under `Debug`.

## Linking accounts

Implement `SocialAccounts` over wherever you keep the links — usually a
`social_accounts` table of `(user_id, provider, provider_id)`, unique on the
last two:

```rust
#[async_trait]
impl SocialAccounts<User> for Accounts {
    async fn find_by_provider(&self, provider: &str, id: &str) -> Result<Option<User>> { … }
    async fn find_by_email(&self, email: &str) -> Result<Option<User>> { … }
    async fn link(&self, user: &User, social: &SocialUser) -> Result<()> { … }
    async fn create_user(&self, social: &SocialUser) -> Result<User> { … }
}
```

`login_with(guard, accounts, request, social)` then:

1. logs in the user already linked to `(provider, id)` — **`Existing`**;
2. otherwise, if a user has this email **and the provider verified it**, links
   and logs them in — **`Linked`**;
3. otherwise, if a user has this email and the provider did *not* verify it,
   refuses with a **`409`** and logs nobody in;
4. otherwise creates, links and logs in — **`Created`**.

### Why step 3 refuses

Because otherwise it is an account takeover. Mallory registers at some
provider with Ada's address — many let you, some never check, and Entra ID lets
a tenant administrator set any address at all — then signs in here. An "email
matches, so link it" rule hands Mallory Ada's account.

So the person signs in the way they already can, and connects the provider
from their settings page with `connect(accounts, &user, &social)` — where they
have proven they own the account, which is what the email was standing in for.
`connect` refuses (`409`) an identity already linked to somebody else.

### Which providers verify email

| provider | `email_verified` is `Some(true)` when |
|---|---|
| GitHub | the address `/user/emails` marks primary **and** verified (needs `user:email`). The profile's public email is free text and is never verified. |
| GitLab | the account is confirmed |
| Discord | its `verified` flag |
| X | it returned `confirmed_email` at all — which needs email access granted in X's developer portal **and** the `users.email` scope added to the defaults (`tweet.read users.read offline.access`). Without both there is no address, which is the usual case. |
| Google, generic OIDC | the `email_verified` claim |
| **Microsoft** | **never** — Entra's `email` is an editable attribute ("nOAuth"). Link by `sub` and `tid`. |

## OpenID Connect

For `google`, `microsoft` and `oidc`, the person comes from the **ID token**,
verified before a claim is read:

- the signature, against the issuer's JWKS, selected by `kid`; the algorithm
  comes from the key, never the token's header;
- `exp`, with a minute of leeway;
- `aud` contains your client id, and `azp`, when present, is your client id;
- `iss` is the discovered issuer (for Microsoft's multi-tenant endpoints,
  `{tenantid}` filled from the token's `tid`);
- `nonce` is the one this session generated.

The discovery document is fetched once per provider and must name the issuer
it was fetched from. The JWKS is cached, and refetched **once** when a token
names a key the cache does not hold — which is what an issuer rotating its keys
looks like, so rotation costs one request rather than an outage.

If the ID token carries no email, the `userinfo` endpoint is asked and merged
in — but only when it describes the same `sub`.

A round trip that produced no ID token is refused: asking for `openid` and
getting plain OAuth back is not something to quietly accept.

Multi-tenant Microsoft (`common`, `organizations`) lets **any** tenant's users
in. If that is not what you meant, use your tenant id; if it is, check
`social.raw["tid"]` against the tenants you serve.

## Tokens from elsewhere

A mobile app or SPA that ran the flow itself can hand the server its access
token:

```rust
let social = sso.driver("google")?.user_from_token(AccessToken::bearer(token)).await?;
```

That asks the provider's API (or OIDC `userinfo`) who the token belongs to. Any
ID token passed in is discarded — no nonce of this server's is in it.

`provider.refresh(&refresh_token)` trades a refresh token for a new access
token, keeping the old refresh token when the provider does not rotate it.

## Safe by default

| | the common default | rainier-sso |
|---|---|---|
| PKCE | opt-in, per provider | always, `S256` only |
| `state` | compared with `!==`, left in the session | constant-time, pulled on first use |
| `stateless()` | available | absent — fix the missing session middleware instead |
| OIDC ID token | mostly not verified | signature, `iss`, `aud`, `azp`, `exp`, `nonce` |
| Linking by email | your code, usually unguarded | refused unless the provider verified it |
| Secrets in dumps | printed | `[redacted]` |

## A provider that is not built in

Implement `ProfileSource` — endpoints, and how a token becomes a `Profile` —
and wrap it in an `OAuth2Provider`:

```rust
struct Strava;

#[async_trait]
impl ProfileSource for Strava {
    async fn endpoints(&self, _: &Api) -> Result<Endpoints> {
        Ok(Endpoints {
            authorize: "https://www.strava.com/oauth/authorize".into(),
            token: "https://www.strava.com/oauth/token".into(),
        })
    }

    async fn profile(&self, api: &Api, token: &AccessToken, _: Option<&str>) -> Result<Profile> {
        let athlete = api.get_json("strava", "https://www.strava.com/api/v3/athlete", Some(&token.access_token)).await?;
        Ok(Profile { id: athlete["id"].to_string(), raw: athlete, ..Profile::default() })
    }
}

let strava = OAuth2Provider::new("strava", credentials, Strava, Api::new(http))
    .scopes(["read"])
    .scope_separator(",");
sso = sso.register(Arc::new(strava));
```

State, PKCE, the token exchange and single use come with it.

## Testing

Every call goes through the framework's HTTP transport port, so a test builds
the provider on a `FakeTransport`, queues the provider's answers, and asserts on
the exact request:

```rust
let fake = Arc::new(FakeTransport::new());
fake.responding(200, r#"{"access_token":"gho_x","scope":"read:user,user:email"}"#)
    .responding(200, r#"{"id":1,"login":"octocat"}"#)
    .responding(200, r#"[{"email":"o@example.com","primary":true,"verified":true}]"#);

let github = providers::github(credentials, Api::new(fake.clone()));
// redirect, read `_sso.github` out of the session for the state, call `user` …
fake.assert_sent(|r| r.url().ends_with("/login/oauth/access_token"));
```

## Not covered

- **SAML.** XML signatures and canonicalisation are a large, sharp surface that
  deserves its own crate. Most SAML identity providers also speak OIDC.
- **Sign in with Apple.** Its client secret is a short-lived ES256 JWT you
  mint, and its user's name arrives once, in a form POST. Possible on
  `OAuth2Provider` with a custom `ProfileSource`; not built in.
- **The routes themselves.** They are two handlers, shown above, because which
  guard, which accounts store and where to land afterwards are yours.
