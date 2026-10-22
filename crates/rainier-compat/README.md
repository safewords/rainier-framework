# rainier-compat

Interoperate with the stack you are migrating from.

A Rainier application answering some routes while the old one answers the
rest shares a database, a Redis, users and an application key with it. This
crate reads — and where it is safe, writes — the formats the old stack keeps
that state in, so a migration can move one route at a time without signing
anyone out.

| Module | Feature | Reads and writes |
|---|---|---|
| `php::serialize` | `php` | `serialize()` / `unserialize()`, into data — never into objects |
| `php::defuse` | `php` | defuse/php-encryption ciphertexts |
| `php::cookie` | `php` | encrypted cookies with the name-binding prefix |
| `php::session` | `php` | sessions, migrated on first touch; remember-me cookies |
| `php::cache` | `php` | cache values and tagged keys on a shared Redis |
| `php::csrf` | `php` | the sealed `XSRF-TOKEN` cookie, for `rainier_session::VerifyCsrfToken` |
| `php::queue` | `php` | queued job payloads, for inspection |
| `php::oauth` | `php-oauth` | OAuth tokens and tables from league/oauth2-server |
| `php::webauthn` | `php-webauthn` | passkey credentials from web-auth/webauthn-lib |

Through the framework: `compat-php`, `compat-php-oauth`, `compat-php-webauthn`.

**Use it to migrate, not to stay** — read the old format, write Rainier's own,
and take the feature out when the old side is gone.

`tools/php-fixtures.php` exports real values from the running PHP application
for the ignored test suite:

```sh
RAINIER_COMPAT_FIXTURES=fixtures.json cargo test -p rainier-compat --features php -- --ignored
```

See [`docs/compat.md`](../../docs/compat.md).
