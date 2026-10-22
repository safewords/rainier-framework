# rainier-accounts

Authentication scaffolding for the [Rainier](https://github.com/safewords/rainier-framework)
framework — registration, login, logout, password reset, email verification,
password confirmation and password change, as routes you mount over traits you
implement. Headless and JSON-first, with a `303` for browser form posts.

```rust,ignore
let accounts = Arc::new(
    Accounts::new(guard, users, hasher, cache)
        .creates_users_with(Arc::new(CreateUser))
        .mails_links_with(MailLinks::new(mailer, "Acme"))
        .signs_urls_with(signer)
        .features(Features::all())
        .middleware(web()),
);
accounts.routes(&mut router)?;
```

Timing-uniform login, per-address-and-IP throttling with `Retry-After`,
hashed single-use reset tokens, signed verification links bound to the
address, session rotation, and hash upgrades on login.

Enable it through the framework: `features = ["accounts"]`. See
[`docs/accounts.md`](../../docs/accounts.md).
