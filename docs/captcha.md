# Human Checks

Behind the `captcha` feature: a form's widget token verified server-side,
for Cloudflare Turnstile, hCaptcha or reCAPTCHA.

> **Run it:** `cargo run -p rainier-framework --example captcha --features captcha`
> — a form route behind `RequireCaptcha`, one refused and one accepted
> submission through a compiled router, offline
> ([`examples/captcha.rs`](../crates/rainier-framework/examples/captcha.rs)).

## Quick start

**1. Turn the feature on.**

```toml
rainier-framework = { version = "2", features = ["captcha"] }
```

`captcha` brings `http-client`, the real outbound transport.

**2. Put the keys in the environment.** The provider issues two: a public
*site key* the widget renders with, and a *secret* only the server holds.

```env
CAPTCHA_SITE_KEY=0x4AAAAAAA…      # rendered into the page — public
CAPTCHA_SECRET=0x4AAAAAAA…        # server only — never in a template
CAPTCHA_HOSTNAME=app.example.com
```

**3. Build one verifier at boot and put the middleware on the routes.**

```rust
use std::sync::Arc;
use rainier_framework::http_client::{ReqwestTransport, Transport};
use rainier_framework::{Captcha, RequireCaptcha};

// bootstrap.rs
let http: Arc<dyn Transport> = Arc::new(ReqwestTransport::new());
let captcha = Arc::new(
    Captcha::turnstile(http, env.string("CAPTCHA_SECRET", "")?)
        .expect_hostname(env.string("CAPTCHA_HOSTNAME", "")?)
        .expect_action("register"),
);

// routes/web.rs
router.post("/register", register).middleware(RequireCaptcha::new(Arc::clone(&captcha)));
router.post("/forgot-password", forgot).middleware(RequireCaptcha::new(captcha));
```

**4. Render the widget inside the form.** It writes its token into a hidden
field named for the provider — `cf-turnstile-response` for Turnstile — which
is the field `RequireCaptcha` reads. Nothing else to wire:

```html
<script src="https://challenges.cloudflare.com/turnstile/v0/api.js" async defer></script>

<form method="POST" action="/register">
  <input name="email" type="email">
  <div class="cf-turnstile" data-sitekey="{{ site_key }}" data-action="register"></div>
  <button>Create account</button>
</form>
```

A single-page app posting JSON reads the token from the widget's callback and
sends it under the same key (`{"email": …, "cf-turnstile-response": token}`),
or under its own name with `RequireCaptcha::new(captcha).field("captcha")`.

**5. In tests**, either hand the verifier a `FakeTransport` answering
`{"success": true}` — to test the check — or use `Captcha::always_pass()` to
test the form behind it.

## How it works

The three providers speak one protocol — POST the token and a secret to a
`siteverify` URL, read `success` — with three URLs and three form fields, so
one verifier covers them: `Captcha::new(CaptchaProvider::HCaptcha, http, secret)`.

| Provider | Field the widget writes |
|---|---|
| Turnstile | `cf-turnstile-response` |
| hCaptcha | `h-captcha-response` |
| reCAPTCHA | `g-recaptcha-response` (`min_score` for v3) |

`RequireCaptcha` refuses with a `422` carrying the error on that field, exactly
as a validation failure would, so a form shows it where it shows the others.
Call `captcha.verify(token, ip)` directly for anything that is not a form.

## What is checked

- **`success`** — the token is real, unexpired and unspent. Tokens are single
  use, so a replay fails here.
- **The hostname**, when you name one. The site key is public; without this, a
  token solved on any page carrying it is accepted.
- **The action**, when you name one — a token from the newsletter form should
  not open the login form.
- **The client IP** goes along as `remoteip` when the request knows it (put
  [`TrustProxies`](middleware.md#trustproxies) before this behind a proxy).

## It fails closed

An unreachable provider refuses the request. A check that waves everybody
through when the provider is slow is one an attacker disables by being
patient. `.fail_open()` exists for a form whose availability matters more
than bots, and logs a warning every time it lets one through.

## Testing

The verifier talks through the framework's HTTP transport port: hand it a
`FakeTransport` answering `{"success": true}`, or use `Captcha::always_pass()`
in a feature test that is about something else. The secret never appears in
`Debug` output.
