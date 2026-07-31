# Hashing

Password hashing is a **port**, `Hasher`, with Argon2id as the implementation
that ships.

```rust
use rainier_framework::auth::{Argon2Hasher, Hasher};

let hasher = Argon2Hasher::new();

let stored = hasher.hash("correct-horse-battery-staple")?;
let ok = hasher.verify("correct-horse-battery-staple", &stored);   // bool
```

Bind it once, in a [provider](providers.md):

```rust
app.instance(Argon2Hasher::new());
```

## The trait

```rust
pub trait Hasher: Send + Sync + 'static {
    fn hash(&self, plain: &str) -> Result<String>;
    fn verify(&self, plain: &str, hashed: &str) -> bool;

    fn needs_rehash(&self, hashed: &str) -> bool { false }
    fn dummy_verify(&self, plain: &str) { let _ = self.hash(plain); }
    fn unusable(&self) -> String { … }
    fn is_unusable(&self, hashed: &str) -> bool { … }
}
```

It is a port rather than a concrete choice because **the right algorithm
changes over time**, and an application that has to migrate should be able to
run two hashers side by side while it does — verify with the old one, re-hash
with the new one on the next successful login.

### `verify` returns `bool`, not `Result`

A malformed stored hash returns `false` rather than an error.

To a caller deciding whether to let someone in, "this stored hash is corrupt"
and "the password is wrong" must lead to the **same outcome**. Making it a
`Result` invites a `?` that turns a corrupt row into a `500` — which tells an
attacker they found an interesting account.

## The login that leaks which addresses are registered

The default login handler has two branches, and they cost wildly different
amounts:

```rust
// WRONG — and it looks fine.
let Some(user) = users.by_email(&email).await? else {
    return Err(Error::unauthenticated("Invalid credentials."));   // ~1ms
};

if !hasher.verify(&password, &user.password) {                    // ~50ms
    return Err(Error::unauthenticated("Invalid credentials."));
}
```

Both answer "invalid credentials", so it reads as though nothing is revealed.
But one answers in a millisecond and the other in fifty, and that difference is
a working **account-enumeration oracle**: a script walks a list of addresses
and learns which are registered, without ever guessing a password. It survives
review precisely because the *messages* are identical.

`dummy_verify` spends the work anyway:

```rust
let Some(user) = users.by_email(&email).await? else {
    hasher.dummy_verify(&password);
    return Err(Error::unauthenticated("Invalid credentials."));
};
```

The default implementation does one hash at this hasher's own cost, which is
the same KDF work a verify does. It has to be called explicitly — nothing can
detect the no-such-user branch on your behalf.

## An account with no password

SSO, a magic link, an API-key-only service account, a suspension:

```rust
user.password = hasher.unusable();
```

The two obvious alternatives are both worse. An empty string is a hash some
algorithm might match an empty password against; `NULL` makes every read site
decide what a missing hash means, and one of them will decide wrong.

`verify` always returns `false` for it — and takes the same time doing so as a
real check, because the account **exists** and how it authenticates is not
something a login form should leak. `is_unusable(&stored)` answers the question
directly, and treats an empty string as unusable too, since a column defaulted
to `''` is the shape this arrives in.

## Argon2id

```rust
Argon2Hasher::new()                              // OWASP baseline
Argon2Hasher::with_params(19 * 1024, 2, 1)       // memory KiB, iterations, lanes
```

The default is OWASP's baseline: **19 MiB, 2 iterations, 1 lane**. Comfortably
above the point where GPU cracking stops being cheap, and still fast enough to
run on every login.

The encoded hash carries its own parameters and salt, so raising the cost later
does not invalidate existing hashes — old ones still verify, and
`needs_rehash` tells you which to upgrade:

```rust
if hasher.verify(&input.password, &user.password) {
    if hasher.needs_rehash(&user.password) {
        user.password = hasher.hash(&input.password)?;
        users.update(&user).await?;
    }
    // … log them in
}
```

That block is the whole migration story: raise the parameters, deploy, and the
population re-hashes itself as people log in.

## Reading hashes this application did not write

Every port from PHP, Rails or Django lands on this the first day: the users
table already exists, it is full of `$2y$` or `pbkdf2_sha256$` hashes, and
nobody knows anybody's password — so the rows cannot be re-hashed until their
owners log in.

```rust
app.instance(Argon2Hasher::new().with_legacy(BcryptVerifier));
```

`verify` dispatches on the stored hash's **own prefix**, and `needs_rehash`
answers `true` for anything a legacy scheme recognises. So the block that was
already there does the migration:

```rust
if hasher.verify(&input.password, &user.password) {
    if hasher.needs_rehash(&user.password) {
        user.password = hasher.hash(&input.password)?;
        users.update(&user).await?;
    }
    // … log them in
}
```

Deploy it, and the population converts itself as people arrive. What is left
after a year is the accounts nobody uses, which is its own useful signal.

### A legacy scheme cannot write

[`LegacyVerifier`] has `recognises` and `verify` and deliberately **no**
`hash`. "Support bcrypt" cannot quietly become "keep producing bcrypt", and
the type says so.

```rust
pub trait LegacyVerifier: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    fn recognises(&self, hashed: &str) -> bool;
    fn verify(&self, plain: &str, hashed: &str) -> bool;
}
```

`recognises` runs on **every** login against strings written by schemes it has
never heard of, so it must be cheap and total — almost always a prefix test.

`BcryptVerifier` is behind the `bcrypt` cargo feature and reads `$2a$`, `$2b$`
and `$2y$`; PHP writes the last one. An application arriving from Django or
Rails writes its own three lines rather than waiting for a variant.

Note bcrypt silently truncates at **72 bytes**. That is a property of the
algorithm and one of the reasons to migrate off it rather than keep writing
it.

[`LegacyVerifier`]: https://docs.rs/rainier-auth/latest/rainier_auth/legacy/trait.LegacyVerifier.html

## Tests must not use the real one

```rust
Argon2Hasher::insecure_for_tests()
```

19 MiB and two iterations **per hash** is the point of Argon2, and it is also
what turns a suite that creates fifty users into a suite that takes a minute.
The test hasher uses minimal parameters.

The name is deliberately unpleasant. It should be obvious in a diff that a
production path is using it.

```rust
let hasher = match mode {
    Mode::Running => Argon2Hasher::new(),
    Mode::Testing => Argon2Hasher::insecure_for_tests(),
};
app.instance(hasher);
```

## Hashing is not encryption

`Hasher` is for **passwords** — one-way, salted, deliberately slow, and never
reversed.

Rainier ships no encryption facility. For data you need to read back — a stored
API credential, a token you must present upstream — use a dedicated crate
(`ring`, `aes-gcm`, `age`) and keep the key outside the repository.

Reaching for a password hasher to "encrypt" something is a mistake the type
signature makes hard: `hash` returns a `String` you cannot get the input back
out of, which is the point.

## What is not here

**No `Hash::make` facade.** Resolve the hasher from the container, or take it
as a constructor argument. A hasher is exactly the kind of dependency worth
seeing in a signature — the [facades](facades.md#the-cost) argument applies
with force, because a test wants the cheap one.

**No second scheme to write with.** `Argon2Hasher` is the only thing that
produces a hash. Reading another is [a legacy
verifier](#reading-hashes-this-application-did-not-write); writing one is not
offered, because the reason to reach for it never survives contact with the
question "why would you write a bcrypt hash in 2026".
