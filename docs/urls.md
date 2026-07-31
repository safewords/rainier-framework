# URL Generation

`UrlGenerator` turns a [named route](routing.md#named-routes) back into a URL:
name the route once, derive every URL from it.

```rust
let urls = app.resolve::<UrlGenerator>()?;
// or
Url::instance()
```

```rust
urls.route("posts.show", &[("post", "hello-world")])?;   // "/posts/hello-world"
urls.absolute("posts.show", &[("post", "hello")])?;      // "https://example.com/posts/hello"
urls.to("/assets/app.css");                              // "https://example.com/assets/app.css"
```

The generator is built at boot from the compiled router's named routes, so
every name that resolves is a route that actually exists.

## Parameters

Named parameters fill the path:

```rust
// route: /posts/{post}/comments/{comment}
urls.route("comments.show", &[("post", "hello"), ("comment", "7")])?;
// "/posts/hello/comments/7"
```

Anything left over becomes a **query string**:

```rust
urls.route("posts.index", &[("page", "2"), ("sort", "new")])?;
// "/posts?page=2&sort=new"
```

That rule means you rarely have to build a query string by hand. Extra parameters are **sorted**, so the same call always
produces the same URL — which matters for caching and for asserting on a URL in
a test.

A missing required parameter is an error rather than a URL with `{post}` still
in it:

```
Error: the `posts.show` route needs a `post` parameter
```

An unknown name says so too, which is the failure you get when a route was
renamed and a template was not.

Optional parameters (`{post?}`) may be omitted; the segment is dropped. A
wildcard (`{path*}`) keeps its slashes — that is the point of it — while each
piece is still encoded.

## Absolute URLs

```rust
UrlGenerator::from_routes(compiled.named_routes()).with_base("https://example.com")
```

The base comes from `app.url` in config, which comes from `APP_URL` in `.env`.
`route()` gives you a path; `absolute()` prefixes the base. Use `absolute` in
anything that leaves the application — [mail](mail.md), webhooks, redirects to
a different host.

## Escaping

Both path segments and query values are percent-encoded, and this is not
cosmetic.

An unescaped `&` in a query **value** would let the value forge additional
parameters:

```rust
urls.route("search", &[("q", "a&admin=1")])?;
// "/search?q=a%26admin%3D1"    — one parameter, as intended
```

The query encoding set escapes `&`, `=`, `+` and `?` in addition to the usual
characters, precisely so a value cannot break out of its own slot. Path
segments are escaped for path context.

## Signed URLs

A link that proves this application produced it, so following it needs no
session and no database row.

```rust
let signed = app.resolve::<SignedUrls>()?;

// /unsubscribe/42?signature=…
let link = signed.route("unsubscribe", &[("user", "42")])?;

// …and one that stops working. `expires` is in the query, so it is signed.
let link = signed.temporary_route("verify", expires_at, &[("user", "42")])?;

// What goes in an email.
let link = signed.absolute_route("verify", &[("user", "42")])?;
```

Checking it is a middleware:

```rust
router.get("/unsubscribe/{user}", unsubscribe)
    .name("unsubscribe")
    .middleware(ValidateSignature::resolved());
```

It answers `403` for a missing, forged or expired signature — and says which,
because "this link has expired" and "this link is not valid" send the reader
to different places.

### What this removes

An unsubscribe link, an email-verification link and a one-time download all
have the same shape without it: a token table, a lookup on every visit, and a
scheduled job to sweep the rows nobody used. The URL carries its own proof
instead, so there is nothing to store and nothing to sweep.

### What is signed

The **path and the query**, with parameters sorted and `signature` removed.
Sorting is what makes the signature independent of the order a client, a proxy
or a mail client happens to send them in; removing `signature` is obvious in
hindsight and is the first thing everybody gets wrong.

The **host is not**. The key is the boundary, not the hostname: two deployments
holding the same `APP_KEY` are the same application by definition, and signing
the host would break every link the moment a request arrives through a proxy
that rewrote it. It also means an absolute link generated for an email
verifies at a server that only ever sees a path — which is the whole point of
having `absolute_route`.

If a link must work on exactly one hostname, put the hostname in the query.
Then it is signed like everything else and your handler can check it.

### Two things a signature is not

**Not single-use.** Anyone holding the link can follow it as many times as they
like until it expires, and a link in an email lives in that mailbox forever.
For anything that must happen once — accepting an invitation, changing an
address — the signature proves *this application issued it* and something
stateful still has to prove *it has not been used*. See
[challenges](authentication.md#challenges).

**Not secret.** The query is in the address bar, in the referrer header, and in
whatever logs the URL. Sign an id; do not sign anything you would mind reading
over somebody's shoulder.

### Rotation retires links

The signer uses the same [key ring](encryption.md) as everything else, so
rotating a key out invalidates every link it signed. That is correct, and worth
knowing before rotating one: every outstanding verification email stops
working. A key kept on the ring as a *previous* key still verifies, which is
how a rotation happens without that.

## Checking a name exists

```rust
urls.has("posts.show");
```

Useful in a template helper or a navigation builder where a route may be
conditionally registered.

## Building one by hand

```rust
let mut urls = UrlGenerator::new();
urls.insert("posts.show", "/posts/{post}");
```

Mostly for tests. In an application the booted one is already in the container,
and it knows every route.
