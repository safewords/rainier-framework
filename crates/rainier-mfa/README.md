# rainier-mfa

The second factor for [Rainier](https://github.com/safewords/rainier-framework):
TOTP with recovery codes, and WebAuthn passkeys. Pure Rust — no C, no OpenSSL.

| Module | Feature | |
|---|---|---|
| `totp` | `totp` (default) | RFC 6238 codes, base32 secrets, `otpauth://` URIs, replay refusal |
| `recovery` | `totp` | single-use recovery codes, stored as SHA-256 |
| `webauthn` | `passkeys` | a relying party: registration and sign-in ceremonies, ES256 / EdDSA / RS256 |
| `webauthn::testing` | `passkeys` | a software authenticator for testing passkey flows |

The cryptography and the ceremonies only: no routes, tables or sessions. The
login flows that use it are `rainier-accounts`' `two-factor` and `passkeys`
features. See [Two-Factor Authentication and Passkeys](../../docs/two-factor.md).
