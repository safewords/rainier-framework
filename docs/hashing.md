# Hashing

Password hashing lives in `rainier-crypt` — it is cryptography, even though
nothing here can be reversed — and the front door is the **`HashManager`**:
the algorithms as peer drivers, with the one that *writes* named by
configuration or by explicit declaration, the way an MVC framework's hash
facade works.

```rust
use rainier_framework::crypt::hash::{HashDriver, HashManager, Hasher};

// In a provider — from configuration…
app.instance(HashManager::new(config.setting(keys::HASH_DRIVER)?)?);

// …or by explicit declaration.
app.instance(HashManager::new(HashDriver::Argon2id)?);
```

```env
HASH_DRIVER=argon2id   # the default; or `bcrypt`
```

```rust
let stored = manager.hash("correct-horse-battery-staple")?;   // the selected driver
let ok = manager.verify("correct-horse-battery-staple", &stored);   // any driver
```

Or through the facade, once the manager is bound:

```rust
use rainier_framework::prelude::*;

let stored = Hash::instance().hash(&input.password)?;
```

The facade is a convenience with the usual [cost](facades.md#the-cost): it
hides a dependency a constructor argument would have made visible, and a test
wants the cheap manager. Reach for it in route closures; take the manager as
an argument in anything you intend to unit-test.

## Selection governs writing, never verification

The rule the whole design hangs on — the `password_verify` contract:

- **`hash` writes with the selected driver.** One algorithm, chosen in one
  place.
- **`verify` dispatches on the stored hash's own prefix** — `$argon2id$` goes
  to the Argon2 driver, `$2y$` to bcrypt — whatever is selected. Every
  registered algorithm's rows keep verifying forever.
- **`needs_rehash` answers `true` for any row the selected driver did not
  write**, or one it wrote at weaker parameters.

Which makes changing algorithm a deploy rather than a migration:

```rust
if manager.verify(&input.password, &user.password) {
    if manager.needs_rehash(&user.password) {
        user.password = manager.hash(&input.password)?;
        users.update(&user).await?;
    }
    // … log them in
}
```

That block is the whole story. Flip `HASH_DRIVER`, deploy, and the population
converts itself as people log in — in either direction. What is left after a
year is the accounts nobody uses, which is its own useful signal.

## The drivers

| Driver | Writes | Reads | |
|---|---|---|---|
| `Argon2Hasher` | `$argon2id$` | `$argon2*` | **the default** — OWASP baseline: 19 MiB, 2 iterations, 1 lane |
| `BcryptHasher` | `$2b$` | `$2a$` / `$2b$` / `$2y$` | feature `bcrypt` — cost 12 |

Both are full [`Hasher`]s. Select bcrypt when a PHP application you share the
users table with is still writing rows — its `password_verify` reads `$2b$`
happily. Otherwise prefer Argon2id: bcrypt silently truncates at **72 bytes**
and its cost scales worse against modern hardware.

Parameters are explicit on the driver, and `with_driver` replaces one inside
the manager:

```rust
HashManager::new(HashDriver::Argon2id)?
    .with_driver(HashDriver::Argon2id, Arc::new(Argon2Hasher::with_params(64 * 1024, 3, 2)))
```

A named driver is also reachable directly — the escape hatch for one call
site that must write a specific algorithm:

```rust
let bcrypt = manager.driver(HashDriver::Bcrypt).expect("compiled in");
```

If that is every call site, change the selection instead.

Selecting a driver the build does not carry — `HASH_DRIVER=bcrypt` without
the `bcrypt` feature — fails **at boot**, naming the feature, rather than
hashing with something the configuration did not say.

## The port

```rust
pub trait Hasher: Send + Sync + 'static {
    fn hash(&self, plain: &str) -> Result<String>;
    fn verify(&self, plain: &str, hashed: &str) -> bool;

    fn needs_rehash(&self, hashed: &str) -> bool { false }
    fn dummy_verify(&self, plain: &str) { let _ = self.hash(plain); }
    fn unusable(&self) -> String { … }
    fn is_unusable(&self, hashed: &str) -> bool { … }
    fn recognises(&self, hashed: &str) -> bool { false }
}
```

`HashManager` is itself a `Hasher`, so anything that takes the port — a
`RepositoryUserProvider`, your own service — takes the manager.

`recognises` is how the manager dispatches: a driver claims its own format,
almost always by prefix. It must be cheap and total, because it runs on every
login against strings written by schemes it has never heard of. A driver of
your own implements it or stays unreachable behind the default `false`.

### `verify` returns `bool`, not `Result`

A malformed stored hash returns `false` rather than an error. To a caller
deciding whether to let someone in, "this stored hash is corrupt" and "the
password is wrong" must lead to the **same outcome**. Making it a `Result`
invites a `?` that turns a corrupt row into a `500` — which tells an attacker
they found an interesting account.

## Every failure costs the same

The login that leaks which addresses are registered:

```rust
// WRONG — and it looks fine.
let Some(user) = users.by_email(&email).await? else {
    return Err(Error::unauthenticated("Invalid credentials."));   // ~1ms
};

if !manager.verify(&password, &user.password) {                   // ~50ms
    return Err(Error::unauthenticated("Invalid credentials."));
}
```

Both branches answer "invalid credentials", but one answers in a millisecond
and the other in fifty, and that difference is a working
**account-enumeration oracle**. `dummy_verify` spends the work anyway:

```rust
let Some(user) = users.by_email(&email).await? else {
    manager.dummy_verify(&password);
    return Err(Error::unauthenticated("Invalid credentials."));
};
```

It has to be called explicitly on the no-such-user branch — nothing can
detect that branch on your behalf. The branches the manager *can* see, it
pads itself, at the selected driver's cost:

- **The unusable sentinel** — an account that authenticates some other way.
- **A format nothing recognises** — a corrupt row, a column filled in by
  hand. Rare, which is exactly why answering quickly would single those
  accounts out of a timing profile.

## An account with no password

SSO, a magic link, an API-key-only service account, a suspension:

```rust
user.password = manager.unusable();
```

The two obvious alternatives are both worse: an empty string is a hash some
algorithm might match an empty password against, and `NULL` makes every read
site decide what a missing hash means. `verify` always returns `false` for
the sentinel — at full cost, see above — and `is_unusable(&stored)` makes
"this account has no password" a question the code can ask instead of infer.

## A scheme with no driver

An inherited Django or Rails table — `pbkdf2_sha256$`, `sha1$` — is a
[`LegacyVerifier`]: `recognises` and `verify` and deliberately **no** `hash`,
so "support pbkdf2" cannot quietly become "keep producing pbkdf2".

```rust
HashManager::new(HashDriver::Argon2id)?.with_legacy(MyPbkdf2Verifier)
```

The manager consults legacy schemes after the drivers, and `needs_rehash`
answers `true` for their rows, so the same login block above converts them.

bcrypt does not need this: it is a full driver. `BcryptVerifier` remains for
a hasher used standalone, but inside the manager the driver's `recognises`
covers all three prefixes.

## Tests must not use the real parameters

```rust
HashManager::insecure_for_tests(HashDriver::Argon2id)?
```

19 MiB and two iterations **per hash** is the point of Argon2, and it is also
what turns a suite that creates fifty users into a suite that takes a minute.
Every driver in the test manager uses its weakest parameters. The name is
deliberately unpleasant so it is obvious in a diff when a production path
uses it.

```rust
let manager = match mode {
    Mode::Running => HashManager::new(config.setting(keys::HASH_DRIVER)?)?,
    Mode::Testing => HashManager::insecure_for_tests(HashDriver::Argon2id)?,
};
app.instance(manager);
```

## Hashing is not encryption

`Hasher` is for **passwords** — one-way, salted, deliberately slow, never
reversed. For data you must read back — a stored API credential, a token you
present upstream — use [`Encryption`](encryption.md), which lives beside this
module for a reason. Reaching for a password hasher to "encrypt" something is
a mistake the type signature makes hard: `hash` returns a `String` you cannot
get the input back out of, which is the point.

[`Hasher`]: https://docs.rs/rainier-crypt/latest/rainier_crypt/hash/trait.Hasher.html
[`LegacyVerifier`]: https://docs.rs/rainier-crypt/latest/rainier_crypt/hash/trait.LegacyVerifier.html
