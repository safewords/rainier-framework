# Interoperating With the Stack You Are Leaving

```toml
rainier-framework = { version = "2", features = ["compat-php"] }
```

A rewrite that switches over in one night is the kind that does not happen.
What happens is a Rainier application answering some routes while the old one
answers the rest — the same database, the same Redis, the same users, the same
application key — for weeks or months. Everywhere the two share state, Rainier
has to read what the old stack wrote, byte for byte, and sometimes write it
back the same way.

`rainier-compat` is those formats, grouped by **language** and then by format,
each behind a feature so an application pays only for the stack it is leaving.

| Module | Feature | Reads and writes |
|---|---|---|
| `php::serialize` | `compat-php` | `serialize()` / `unserialize()` — into data, never into objects |
| `php::defuse` | `compat-php` | defuse/php-encryption ciphertexts |
| `php::cookie` | `compat-php` | encrypted cookies, with the name-binding prefix |
| `php::session` | `compat-php` | sessions, migrated into Rainier's on first touch; remember-me cookies |
| `php::cache` | `compat-php` | cache values and tagged-cache keys on a shared Redis |
| `php::csrf` | `compat-php` | the sealed `XSRF-TOKEN` cookie, for Rainier's CSRF middleware |
| `php::queue` | `compat-php` | queued job payloads, for inspection |
| `php::oauth` | `compat-php-oauth` | OAuth tokens and tables written by league/oauth2-server |
| `php::webauthn` | `compat-php-webauthn` | passkey credentials written by web-auth/webauthn-lib |
| `ruby::rails`, `python::django` | — | not yet; the same shape when they come |

**See it run:** `crates/rainier-framework/examples/compat_php.rs` walks every
row of that table offline — a PHP session adopted, a PHP-issued anti-forgery
cookie accepted, a cache read, an object unserialized, a refresh token opened,
a passkey read:

```sh
cargo run -p rainier-framework --example compat_php \
    --features compat-php-oauth,compat-php-webauthn
```

---

## Quick start: the migration playbook

The order matters more than any one piece. Each step is reversible on its own,
and each is proved before the next one starts.

### 1. Share the keys — before any traffic moves

Give the Rainier application the PHP application's secrets, unchanged:

| Secret | Rainier takes it as | Used by |
|---|---|---|
| `APP_KEY` (and any previous keys) | a `KeyRing`, current first | cookies, sessions, CSRF, encrypted columns, queued jobs |
| `APP_KEY`, the string as configured | `DefuseSealer::from_app_key` | refresh tokens and authorization codes |
| the OAuth RSA key pair | the access-token signer and verifier | access tokens — see [OAuth Server](oauth-server.md) |
| the database, the Redis | ordinary connections | everything else |

Nothing is re-issued, so nobody is signed out, and rolling back is deleting
the Rainier deployment.

### 2. Prove the formats against the real application

Unit tests pin every format against independently computed values. Pin them
against **the** values too, before anything depends on them. Two scripts
export real values into one JSON file:

| Script | Runs | Exports |
|---|---|---|
| `crates/rainier-compat/tools/php-fixtures.php` | inside the PHP application — its interactive console, or any script that has booted it; computes everything in memory with the application's own encrypter, cache store and guard, and writes nothing | a sealed cookie, a session payload, a cache value and tagged key, an `XSRF-TOKEN` cookie, a remember-me cookie, `serialize()` samples |
| `crates/rainier-compat/tools/php-oauth-fixtures.php` | with plain `php`, from any project that has the OAuth library installed; throwaway keys by default, yours when passed | a freshly issued access token, refresh token and authorization code, merged into the same file |

Inside the PHP application's console:

```php
$out = '/tmp/rainier-compat-fixtures.json';
require 'path/to/rainier-compat/tools/php-fixtures.php';
```

Then, to prove your real keys rather than throwaway ones:

```sh
APP_KEY=base64:… php path/to/php-oauth-fixtures.php     /tmp/rainier-compat-fixtures.json storage/oauth-private.key

# from the Rainier checkout
RAINIER_COMPAT_FIXTURES=/tmp/rainier-compat-fixtures.json     cargo test -p rainier-compat --features php,php-oauth,php-webauthn -- --ignored
```

A file made from the real application holds its key. Never commit it; delete
it when the tests pass. Run them again after any upgrade of the PHP side.

### 3. Adopt read-only

Turn on `compat-php` and **read** what the PHP side writes, writing nothing
back into its formats yet:

- background workers first — they have no sessions, and a queue consumer
  reading shared cache values with [`PhpCache`](#cache-values--phpcache) is
  the easiest place to start and the place a Rust port pays off soonest;
- then API routes authenticated by bearer token, with the OAuth store reading
  the PHP side's token tables (`compat-php-oauth`) — an access token issued by
  PHP authenticates against Rainier's guard;
- inspect the live queue with [`QueuedJob`](#queued-jobs--phpqueue) to size
  the workers you are about to replace.

### 4. Coexist

Move routes one at a time behind the proxy. On the Rainier side:

```rust
web.push(AdoptForeignSession::new("app_session", "rainier_session"))
   .push(StartSession::with_config(foreign_store, config.cookie("rainier_session")))
   .push(VerifyCsrfToken::new().codec(Arc::new(EncryptedXsrf::new(cookies))));
```

A user crossing between a PHP route and a Rainier one stays signed in on both,
a form the PHP side rendered still posts to Rainier, and a passkey registered
on either side works on both. Writes go to Rainier's own formats; where the PHP
side must keep reading something Rainier writes — a refresh token Rainier
rotates, a passkey counter — the compat writer produces the PHP format
(`DefuseSealer`, `webauthn::update_credential`).

### 5. Cut over

When the PHP application answers nothing:

1. switch the session store to plain Rainier (`Release::Destroy` beforehand
   drains PHP sessions as people log in; whatever remains simply expires);
2. drop the CSRF codec — Rainier's plain `XSRF-TOKEN` from then on;
3. keep `DefuseSealer` for as long as refresh tokens it sealed are alive (a
   year, typically), then move to Rainier's own sealer;
4. remove the `compat-*` features. Nothing else changes.

### Feature flags

| Framework feature | Pulls in |
|---|---|
| `compat-php` | `php::{serialize, defuse, cookie, session, cache, csrf, queue}` |
| `compat-php-oauth` | `compat-php` + the OAuth server + `php::oauth` |
| `compat-php-webauthn` | `compat-php` + passkeys + `php::webauthn` |

---

## Use it to migrate, not to stay

Every module here exists so two applications can share state *during* a move.
The pattern that works is the one [legacy password
hashes](hashing.md#a-scheme-with-no-driver) and [PHP-encrypted
columns](encryption.md#reading-what-a-php-application-encrypted) already use:
**read** the old format, **write** Rainier's own, and let data convert as it is
touched. When the PHP side is switched off, the compat feature comes out and
nothing else changes.

---

## PHP

### The application key

Most of what follows is keyed by the same `APP_KEY` the PHP application runs
with. Hand it to Rainier as a `KeyRing` — current key first, previous keys
after, exactly as they rotate on the PHP side:

```rust
let keys = KeyRing::from_base64(current, &previous)?;
```

Two places want the key **as configured** rather than decoded, and say so: the
remember-me cookie's password MAC is keyed with the configuration string,
`base64:` prefix and all.

### `serialize()` — `php::serialize`

```rust
let value = unserialize(bytes)?;
let user_id = value.get("user_id").and_then(Value::as_int);
let json = value.to_json();          // Some(..) only when nothing is lost
let back = serialize(&value);        // byte for byte
```

Everything reads into a `Value` tree — integers, floats (including `INF` and
`NAN`), byte strings, ordered arrays with mixed integer and string keys,
objects with their class name and visibility-mangled property names,
custom-serialized classes, enum cases and references. Nothing is ever
instantiated, which is the whole of PHP's own `unserialize` vulnerability
class: a hostile payload here can at worst be refused.

Lengths and counts are checked against the bytes actually present before
anything is allocated, and nesting is capped at 64 levels (`Limits`), so a
payload claiming a four-gigabyte string or ten thousand nested arrays is an
error rather than an allocation or a stack overflow.

### Encrypted cookies — `php::cookie`

```rust
let cookies = EncryptedCookies::new(keys.clone());
let session_id = cookies.decrypt("app_session", request.cookie("app_session")?)?;
```

The PHP side seals every cookie with its encrypter's envelope (the format
`rainier_crypt::PhpEncrypter` reads) around `hex(hmac_sha1(name + "v2", key)) |
value`. That prefix binds the ciphertext to its cookie's **name**: without it,
every encrypted cookie is interchangeable, and a client could present a cookie
whose value it chose as the session cookie. Rainier verifies the prefix rather
than cutting 41 bytes off — so a cookie moved between names is refused.

### Sessions — `php::session`

Two ways to run beside a PHP application:

| | Rainier's cookie | The PHP application afterwards |
|---|---|---|
| **Coexist** | its own name, with `AdoptForeignSession` in front | still signed in — its cookie is never touched |
| **Take over** | the PHP application's name | signed out on its next request |

```rust
let store = Arc::new(ForeignSessionStore::new(
    Arc::new(rainier_sessions),                                     // where Rainier's live
    Arc::new(CacheSessionSource::new(PhpCache::new(redis, "app_cache_"))),
    cookies.clone(),
    "app_session",                                                  // the PHP cookie
    &keys,
)?);

web.push(AdoptForeignSession::new("app_session", "rainier_session"))
   .push(StartSession::with_config(store, config.cookie("rainier_session")));
```

On a request with the PHP cookie and no Rainier one, the PHP session is
loaded, unserialized, and mapped into Rainier's shape:

| PHP session key | Becomes |
|---|---|
| `_token` | `_token` — the anti-forgery token, so forms the PHP side rendered still post |
| the session guard key a PHP application writes (`login_{guard}_` + 40 hex digits) | Rainier's `_auth_id` — the user stays signed in |
| `_flash` | Rainier's flash list — a message flashed by the last PHP request shows on this one |
| any value with a lossless JSON form | itself |
| an object, such as a validation error bag | skipped, named in a debug log line |

It is written to Rainier's store under an id **derived** from the PHP one with
the application key, and Rainier's cookie is set to it. The derivation is what
makes a page that fires four requests at once safe: all four migrate into the
same Rainier session, and a request that still carries only the PHP cookie
afterwards finds the migrated copy instead of the stale original.

`Release::Keep` (the default) leaves the PHP session in place;
`Release::Destroy` deletes it when Rainier's copy is regenerated or destroyed —
at login and logout — so the two cannot drift apart afterwards.

Payloads come from a `SessionSource`: `CacheSessionSource` for a Redis or
Memcached session driver, `FileSessionSource` for files, `MemorySessionSource`
for tests. For a database table, implement the trait with one query and pass
the `payload` column through `database_payload`, which undoes its base64.

**Remember-me cookies.** `RememberCookies` reads `remember_{guard}_…`,
checks the token through your `RememberedUsers` lookup, and checks the
password MAC so a cookie issued before a password change is refused. It
answers a user id; logging them in is your guard's `login`.

### Cache values — `php::cache`

```rust
let shared = PhpCache::new(redis_store, "app_cache_");
let reading = shared.tags(&["vehicle-telemetry"]).get(&vin).await?;
shared.put("feature:new-dashboard", &Value::Bool(true), Some(ttl)).await?;
```

Integers and finite floats are stored as their decimal text, unserialized — so
`INCRBY` works on them on either side — and everything else `serialize()`d,
under `{connection prefix}{cache prefix}{key}`. The Rainier store underneath
must apply no prefix of its own.

**Tagged entries are read-only here.** A tagged entry lives at
`sha1("tag:a:entries|tag:b:entries") + ":" + key`, and on Redis writing one also
adds it to each tag's sorted set, which is how a flush finds it. A plain
key-value port cannot express that, and an entry written without it would
silently survive the next flush of its tag. Read and forget tagged entries from
Rainier; write them from the PHP side until it is gone.

**Prefer a key of your own.** For state both sides will read for a long time,
a key both write as plain JSON is simpler than either speaking the other's
format.

### Anti-forgery tokens — `php::csrf`

Rainier has its own CSRF middleware, `rainier_session::VerifyCsrfToken`, which
works with or without a PHP application beside it. Beside one, two details
differ, and both are covered by plugging in a codec rather than a second
middleware:

```rust
VerifyCsrfToken::new().codec(Arc::new(EncryptedXsrf::new(cookies.clone())))
```

- **The token** is the session's `_token` in both, and a migrated session
  carries it — nothing to do.
- **The `XSRF-TOKEN` cookie** is sealed like every other cookie on the PHP
  side, and front ends copy the sealed value into `X-XSRF-TOKEN` verbatim.
  `EncryptedXsrf` seals the cookie the same way and opens the header.

### Queued jobs — `php::queue`

```rust
let job = QueuedJob::parse(&payload)?;
println!("{} attempt {:?}", job.display_name, job.attempts);
let args = job.command()?;          // the job object, as data
```

For looking, not running: a queued PHP job is an envelope around its job
object, `serialize()`d, and reading one answers which jobs are waiting, how
many of each, with what arguments. Running one is the job's PHP code, which is
not in the payload. Port the job, dispatch it from both sides under a new
name, and drain the old queue. An encrypted job opens with `command_with` and
the PHP encrypter.

### Passkeys — `php::webauthn`

```rust
let credential = webauthn::read_credential(&row.data)?;            // a PasskeyCredential
let verified = relying_party.finish_authentication(&state, &response, &credential)?;
let mut updated = credential.clone();
verified.apply_to(&mut updated);
row.data = webauthn::update_credential(&row.data, &updated)?;       // the PHP side keeps reading it
```

Credentials stored by web-auth/webauthn-lib — its `PublicKeyCredentialSource`
normalised to JSON — read into `rainier-mfa`'s `PasskeyCredential`. The COSE
public key is the authenticator's own bytes, so a passkey registered on the
PHP side signs in on the Rainier side with nothing re-registered.
`update_credential` writes the new signature counter back **into the existing
JSON**, leaving `trustPath`, `otherUI` and the rest as they were, so a PHP
reader sharing the row is not surprised. Look credentials up by the id inside
the JSON, not by a `credential_id` column beside it: some packages fill that
column by re-encoding the raw id bytes as text, which is lossy.

### TOTP secrets

Nothing to convert. A PHP application's TOTP secret — google2fa and its
wrappers among them — is base32 text, which `rainier_mfa::Secret::from_base32`
reads as the same bytes, and its codes are RFC 6238 SHA-1, six digits, thirty
seconds, which is `Totp::new()`. An authenticator app enrolled on the PHP side
keeps working.

If the column holds **a PHP application's encrypted TOTP secret** rather than
plain base32 — an encrypted model cast, or `encrypt()` by hand — open it with
the PHP encrypter first (`APP_CIPHER=php`, see
[Encryption](encryption.md#reading-what-a-php-application-encrypted)); the
plaintext is the base32 secret, wrapped in `serialize()` when it was written
with `encrypt()` rather than `encryptString()`, which `php::serialize` unwraps.

Replay protection carries across too, provided both sides record the last
accepted step: google2fa's `verifyKeyNewer` returns a timestamp in **steps**,
the same unit as `TwoFactorRecord::last_used_step`.

### Proving it against the real application

Every format is pinned by unit tests against independently computed values.
For a migration that cannot afford to be wrong, pin it against **the** values
too: `crates/rainier-compat/tools/php-fixtures.php` runs inside the PHP
application (its interactive console, or any script that has booted it),
computes a sealed cookie, a session payload, a cache value, a tagged key, an
`XSRF-TOKEN` cookie and a remember-me cookie **in memory** with the
application's own encrypter, cache store and guard, and writes them to a JSON
file. Then:

```sh
RAINIER_COMPAT_FIXTURES=/tmp/rainier-compat-fixtures.json \
    cargo test -p rainier-compat --features php -- --ignored
```

The file holds the application key. Do not commit it; delete it when the
tests pass.

---

## Migrating from a PHP OAuth server

```toml
rainier-framework = { version = "2", features = ["oauth-server", "compat-php-oauth"] }
```

A PHP application that issues OAuth tokens almost always does it with
league/oauth2-server, and its clients — mobile apps, integrations, other
people's servers — hold access tokens for a day and refresh tokens for a year.
Moving the [OAuth server](oauth-server.md) to Rainier must not touch either, so
`php::oauth` makes a Rainier `AuthorizationServer` answer for the PHP one with
its tokens and tables left where they are:

| Piece | What it does |
|---|---|
| `DefuseSealer::from_app_key(app_key)` | opens and writes refresh tokens and authorization codes exactly as league seals them |
| `LeagueOAuthStore::new(db)` | the `oauth_clients`, `oauth_access_tokens`, `oauth_refresh_tokens` and `oauth_auth_codes` tables, read and written as the PHP side does |
| `AccessTokenSigner::from_pem(private_key)` | the same RSA key pair; the access-token claims are already the ones league writes |

```rust
let store = Arc::new(
    LeagueOAuthStore::new(db.clone())
        .timezone(FixedOffset::east_opt(0).unwrap())   // the PHP app's timezone
        .default_provider("users"),
);
let server = AuthorizationServer::new(
    store.clone(),
    Arc::new(DefuseSealer::from_app_key(&env.string("APP_KEY", ""))?),
    AccessTokenSigner::from_pem(&std::fs::read_to_string("storage/oauth-private.key")?)?,
    Arc::new(BcryptHasher::default()),   // client secrets were hashed by PHP
);
```

### What has to match, and how it does

- **Refresh tokens and codes** are defuse v2 ciphertexts in *password* mode,
  with the application's decoded 32-byte key as the password. `DefuseSealer`
  takes the key as configured — `base64:` and all — and does the same.
- **Access tokens** are RS256 JWTs with no `kid`, `aud` = client, `jti` = row,
  `sub` = user, a `scopes` array, and timestamps the PHP side writes **with
  microseconds**. The verifier accepts all of that; tokens Rainier issues use
  integer timestamps, which the PHP verifier accepts in turn.
- **Client rows** come in two shapes. The older one says what a client may do
  with flags (`redirect`, `password_client`, `personal_access_client`,
  `user_id`), the newer with JSON lists (`redirect_uris`, `grant_types`,
  `owner_id`). The store asks the table once which it has and derives the
  grant list the PHP model would — so a client that may use the password
  grant on PHP may use it here, and one that may not, may not.
- **Client secrets** were hashed by PHP with bcrypt (`$2y$`). Hand the server a
  hasher that verifies those — the `bcrypt` feature.
- **Datetimes** are written as `Y-m-d H:i:s` in the PHP application's
  timezone, which the columns do not record. Set `.timezone(…)` to it, or every
  expiry shifts by the offset.
- **Revocation** writes the same `revoked` flags, so a token revoked on either
  side is dead on both. Spending a code or a refresh token is an `UPDATE …
  WHERE revoked = 0` whose affected-row count decides who won — strictly safer
  than the PHP server's revoke-after-issue, and compatible with it.

Run the two side by side against one database while traffic moves: either can
issue, either can refresh what the other issued, and turning the PHP one off
logs nobody out.

### Proving it

`crates/rainier-compat/tools/php-oauth-fixtures.php` mints an access token, a
refresh token and an authorization code with league/oauth2-server itself — the
library, not an application — and adds them under `"oauth"` to the same
fixtures file as above:

```sh
php crates/rainier-compat/tools/php-oauth-fixtures.php /tmp/rainier-compat-fixtures.json
RAINIER_COMPAT_FIXTURES=/tmp/rainier-compat-fixtures.json     cargo test -p rainier-compat --features php-oauth -- --ignored
```

Run it from the root of any project with league/oauth2-server installed. By
default it uses a **throwaway** application key and key pair, which proves the
formats and holds nothing secret. To prove *your* keys too, give it
`APP_KEY=… ` and the private key path — and then treat the file as a secret.

---

## CSRF without a PHP application

The CSRF middleware the PHP section plugs into is Rainier's own and needs no
compat feature:

```rust
MiddlewareStack::new()
    .push(StartSession::from_manager(&sessions))
    .push(VerifyCsrfToken::new().except(["webhooks/*"]));
```

A request other than `GET`, `HEAD` or `OPTIONS` passes if its path is
excepted, if the browser marks it `Sec-Fetch-Site: same-origin`, or if it
carries the session's token as a `_token` field, an `X-CSRF-TOKEN` header or an
`X-XSRF-TOKEN` header. Otherwise it is answered `419`. Every response that
passes sets a script-readable `XSRF-TOKEN` cookie. It must run inside
`StartSession`; without a session it refuses rather than waving requests
through.
