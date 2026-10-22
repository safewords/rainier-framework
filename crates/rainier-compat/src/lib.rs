//! # rainier-compat
//!
//! Interoperate with the stack you are migrating from.
//!
//! A rewrite that switches over in one night is the kind that does not
//! happen. What happens is a Rainier application answering some routes while
//! the old one answers the rest — the same database, the same Redis, the same
//! users, the same application key — for weeks or months. Every place the two
//! share state, Rainier has to read what the old stack wrote, byte for byte,
//! and sometimes write it back the same way. This crate is those formats.
//!
//! Grouped by **language**, then by format, each behind a feature so an
//! application pays only for the stack it is leaving:
//!
//! | Module | Feature | Reads and writes |
//! |---|---|---|
//! | `php::serialize` | `php` | `serialize()` / `unserialize()` — into data, never into objects |
//! | `php::defuse` | `php` | defuse/php-encryption ciphertexts |
//! | `php::cookie` | `php` | encrypted cookies with the name-binding prefix |
//! | `php::session` | `php` | sessions, migrated into Rainier's on first touch; remember-me cookies |
//! | `php::cache` | `php` | cache values and tagged-cache keys on a shared Redis |
//! | `php::csrf` | `php` | the sealed `XSRF-TOKEN` cookie, for Rainier's own CSRF middleware |
//! | `php::queue` | `php` | queued job payloads, for inspection |
//! | `php::oauth` | `php-oauth` | OAuth tokens and tables from league/oauth2-server |
//! | `php::webauthn` | `php-webauthn` | passkey credentials from web-auth/webauthn-lib |
//! | `ruby::rails`, `python::django` | — | not yet; the same shape when they come |
//!
//! # Use it to migrate, not to stay
//!
//! Everything here exists so two applications can share state *during* a
//! move. Each module's documentation says what to do instead once the old
//! side is gone — usually: read with this, write Rainier's own format, and
//! let the rows convert as they are touched.
//!
//! # Verified against the real thing
//!
//! Each format is pinned by tests against independently computed values. For
//! a migration that cannot afford to be wrong, `tools/php-fixtures.php`
//! exports real values from the running PHP application, and the ignored
//! test suite reads them back:
//!
//! ```text
//! RAINIER_COMPAT_FIXTURES=fixtures.json cargo test -p rainier-compat --features php -- --ignored
//! ```

#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

#[cfg(feature = "php")]
pub mod php;
