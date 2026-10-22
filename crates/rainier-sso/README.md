# rainier-sso

"Sign in with …" for [Rainier](https://github.com/safewords/rainier-framework):
OAuth 2.0 and OpenID Connect providers — GitHub, GitLab, Discord, Google,
Microsoft Entra ID, and any OIDC issuer by discovery — `redirect()`, then
`user()`, with the safe defaults on.

- PKCE (`S256`) on every provider; `state` compared in constant time and
  consumed on first use; no `stateless()`.
- ID tokens verified against the issuer's JWKS: signature, `iss`, `aud`,
  `azp`, `exp`, `nonce`, with one refetch on key rotation.
- `login_with` finds, links or creates the user and logs them in on a guard —
  and refuses to link an existing account by an email the provider did not
  verify.
- Every call over the framework's HTTP transport port, so tests run on
  `FakeTransport` with no network.

Enabled through `rainier-framework`'s `sso` feature. See
[`docs/sso.md`](../../docs/sso.md), and run the example:

```sh
cargo run -p rainier-framework --example sso_login --features sso
```
