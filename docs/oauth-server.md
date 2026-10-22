# OAuth Server

An OAuth 2.0 authorization server in `rainier-oauth-server`, behind the
framework's `oauth-server` feature: the token endpoint, the two halves of the
authorization endpoint, personal access tokens, and the guard that accepts what
it issues.

```toml
rainier-framework = { version = "2", features = ["oauth-server"] }
```

This is the **server** side — your application issuing tokens to other apps.
Letting users sign in *with* somebody else's accounts is [Single Sign-On](sso.md).

| | |
|---|---|
| `AuthorizationServer` | the grants, the authorization endpoint, personal access tokens, revocation, purging |
| `OAuthGuard<U>` | a `rainier-auth` guard for bearer access tokens; scopes become abilities |
| `OAuthStore` / `MemoryStore` | where clients, tokens and codes are kept |
| `TokenSealer` / `EncrypterSealer` | how refresh tokens and codes are sealed |
| `AccessTokenSigner` / `AccessTokenVerifier` | RS256 access tokens |
| `Scopes` | which scopes exist, and namespace inheritance |
| `AuthorizationSession` | the pending authorization, across the consent screen |
| `pkce` | the client's half of PKCE — `verifier()` and `challenge()` |

---

## Quick start

The whole flow — two clients, authorization code with PKCE, refresh rotation,
client credentials, a personal access token, a scoped API route, revocation —
runs offline in one example:

```sh
cargo run -p rainier-framework --example oauth_server --features oauth-server
```

[`examples/oauth_server.rs`](../crates/rainier-framework/examples/oauth_server.rs)
is the reference for everything below.

**1. Turn it on.**

```toml
rainier-framework = { version = "2", features = ["oauth-server"] }
```

**2. Keys.** Access tokens are RS256, so the server needs an RSA private key;
API servers that only accept tokens need the public half. Generate a pair once
and keep it in your secret store:

```sh
openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:4096 -out oauth-private.key
openssl pkey -in oauth-private.key -pubout -out oauth-public.key
```

```rust
let signer = AccessTokenSigner::from_pem(&env.string("OAUTH_PRIVATE_KEY", ""))?;
```

`AccessTokenSigner::generate(2048)` makes a throwaway key for tests and local
development — never for a deployment, where a new key per boot logs every
client out.

Refresh tokens and codes are sealed with the application's own encrypter, so
they rotate with `APP_KEY`:

```rust
let sealer = Arc::new(EncrypterSealer::new(encrypter.clone()));
```

**3. A store.** `MemoryStore` for tests; for a deployment, implement
`OAuthStore` over your tables (four of them: clients, access tokens, refresh
tokens, codes — the record types in `model` are the columns). Taking over an
existing PHP server's tables instead is
[`rainier-compat`](compat.md#migrating-from-a-php-oauth-server).

**4. The server, and its routes.**

```rust
let server = Arc::new(
    AuthorizationServer::new(store.clone(), sealer, signer, hasher.clone())
        .scopes(Scopes::new().with("account", "Read your account").with("vehicles", "Your vehicles")),
);

// No session or CSRF on the token endpoint: the client authenticates itself.
let token = server.clone();
router.post("/oauth/token", move |request: Req| {
    let server = token.clone();
    async move { server.respond_to_token_request(&request).await }
});

// The authorization endpoint lives in the user's browser session, behind login.
router.group(GroupAttributes::new().middleware(web_with_login()), |router| {
    router.get("/oauth/authorize", show_consent);     // validate → stash → your consent page
    router.post("/oauth/authorize", approve);         // take → complete(.., true)
    router.delete("/oauth/authorize", deny);          // take → complete(.., false)
});
```

The two consent handlers are written out in full
[below](#the-authorization-endpoint) and in the example.

**5. Protect the API.**

```rust
let guard = OAuthGuard::new("api", server.verifier(), store, users.clone());
let auth = Arc::new(AuthManager::<User>::new("api").register(Arc::new(guard)));

router
    .get("/api/vehicles", list_vehicles)
    .middleware(Authenticate::new(auth.clone()))
    .middleware(RequireAbility::all(["vehicles"]));    // the scope check
```

A token without `vehicles` is answered `403`; no token, a junk one, or a revoked
one, `401`. A separate API service that only accepts tokens builds its guard
from `AccessTokenVerifier::from_pem(public_key)` and a store pointing at the
same tables.

---

## Wiring it

```rust
use rainier_framework::oauth_server::*;

let store: Arc<dyn OAuthStore> = Arc::new(MyStore::new(db.clone()));
let sealer = Arc::new(EncrypterSealer::new(encrypter.clone()));
let signer = AccessTokenSigner::from_pem(&env.string("OAUTH_PRIVATE_KEY", ""))?;

let server = Arc::new(
    AuthorizationServer::new(store.clone(), sealer, signer, hasher.clone())
        .scopes(
            Scopes::new()
                .with("account", "Manage your account")
                .with("vehicles", "See and control your vehicles"),
        )
        .lifetimes(Lifetimes {
            access_token: Duration::from_secs(24 * 3600),
            refresh_token: Duration::from_secs(365 * 24 * 3600),
            ..Lifetimes::default()
        }),
);

// The API side.
let guard = OAuthGuard::new("api", server.verifier(), store, users.clone());
let auth = AuthManager::<User>::new("web").register(Arc::new(guard));
```

| Endpoint | Method |
|---|---|
| `POST /oauth/token` | `server.respond_to_token_request(&request)` |
| `GET /oauth/authorize` | `server.validate_authorization_request(&request)`, then your consent screen |
| `POST /oauth/authorize` | `AuthorizationSession::take`, then `server.complete_authorization_request(…, true)` |
| `DELETE /oauth/authorize` | the same with `false` |
| `POST /oauth/revoke` | `server.respond_to_revocation_request(&request)` (RFC 7009) |

Each is a one-line handler; the server owns the protocol and the application
owns the routes, the login page and the consent screen.

## The grants

| Grant | Refresh token | Notes |
|---|---|---|
| `authorization_code` | yes | PKCE `S256` (and `plain`, unless turned off); **required** for public clients |
| `refresh_token` | yes | rotated: the old one is spent, the old access token revoked |
| `client_credentials` | no | confidential clients only; the token's `sub` is the client |
| `password` | yes | only once `.password_grant(owners)` is given — see below |
| personal access | no | not a grant at the endpoint: `issue_personal_access_token(user, name, scopes, provider)` |

A client may use a grant only if its `grant_types` lists it. A client with a
`secret_hash` is confidential and must authenticate (Basic auth or body
parameters); one without is public.

### The password grant is opt-in

RFC 9700 says not to use it: it teaches users to type their password into an
app that is not yours. It exists here because first-party mobile apps were
built on it and have to keep working while they move to the code flow.
`UserProviderOwners` checks credentials through the application's existing
`rainier-auth` user provider.

## Spending a code is claiming it

A code and a refresh token are each good for one exchange. The server revokes
the row **first** and issues only if that revocation is the one that took, so
two requests racing with one code get one token between them. An `OAuthStore`
over SQL does this with `UPDATE … SET revoked = 1 WHERE id = ? AND revoked =
0` and the affected-row count — the store contract says so.

## The authorization endpoint

```rust
async fn show(request: Req, user: AuthenticatedUser<User>) -> Result<Response> {
    let authorization = match server.validate_authorization_request(&request).await {
        Ok(authorization) => authorization,
        Err(error) => return Ok(error.into_response()),
    };
    if server.previously_approved(&authorization, &user.id()).await? {
        return Ok(server.complete_authorization_request(&authorization, &user.id(), true).await);
    }
    let auth_token = AuthorizationSession::stash(&request, &authorization)?;
    views.render("oauth.consent", context! { authorization, auth_token })
}

async fn approve(request: Req, user: AuthenticatedUser<User>) -> Result<Response> {
    let authorization = AuthorizationSession::take(&request, &request.input_or("auth_token", ""))?;
    Ok(server.complete_authorization_request(&authorization, &user.id(), true).await)
}
```

An error from `validate_authorization_request` carries a redirect only once the
redirect URI is known to be the client's. An unknown client or an unregistered
redirect URI is answered to the user, never redirected to — redirecting to an
unverified URI would hand the code to whoever chose it.

Redirect URIs match exactly, except an `http` loopback URI (`127.0.0.1`,
`[::1]`) matches on any port, as RFC 8252 asks for native apps.

The `auth_token` is single-use and compared in constant time; without it, a page
on another site could post "approve" from inside the user's logged-in browser.

## Scopes

`Scopes::new().with(name, description)` declares them. A request naming an
unknown one is refused by name. Scopes are namespaces: holding `vehicles`
grants `vehicles:read`, never the reverse; `*` grants everything, and survives
only grants with no consent screen (password, client credentials, personal
access) — a third party cannot talk a user into "everything".

The guard hands a token's scopes to `rainier-auth` as `Abilities`, each as
itself and as `scope:*`, so `RequireAbility` *is* the scope middleware and
agrees with the inheritance above.

## Access tokens

RS256 JWTs:

| Claim | |
|---|---|
| `aud` | the client id |
| `jti` | the token id — the row revocation is checked against |
| `iat`, `nbf`, `exp` | |
| `sub` | the user, or the client for a client-credentials token |
| `scopes` | an array |

`AccessTokenSigner::with_kid` adds a `kid`, `issued_by` an `iss`,
`with_claim` a static claim. Verification fixes the algorithm to RS256 from
the key, never from the header, accepts integer or fractional timestamps and
a string or array `aud`, and `also_accepting` a previous key during rotation.

The guard checks more than the signature: the token's row must exist and not be
revoked, and its client must exist and not be revoked. That is a read per
request, and it is what makes a token revocable before it expires.

## Housekeeping

`server.revoke(access_token_id)` revokes an access token and its refresh
tokens. `server.purge(retention)` deletes revoked rows and rows that expired
more than `retention` ago — schedule it daily.

## What is not here

The implicit grant (RFC 9700 removed it), the device authorization grant, token
introspection (RFC 7662), dynamic client registration, and OpenID Connect — an
`id_token` is a different promise to a different party.

## Taking over from a PHP OAuth server

`rainier-compat` reads and writes the tables and token formats a
league/oauth2-server deployment already has, so the cutover logs nobody out —
see [Migrating from a PHP OAuth server](compat.md#migrating-from-a-php-oauth-server).
The [`compat_php` example](../crates/rainier-framework/examples/compat_php.rs)
runs that side offline:

```sh
cargo run -p rainier-framework --example compat_php --features compat-php-oauth,compat-php-webauthn
```
