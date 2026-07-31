# HTTP Client

Calling somebody else's API, and — more importantly — asserting that you did.

```rust
let response = Http::post("https://hooks.example.com/user-updated")
    .json(&payload)?
    .timeout(Duration::from_secs(10))
    .retry(3, Backoff::exponential())
    .send()
    .await?;

response.error_for_status()?;
```

The outbound half of HTTP. Before 1.1.0 the framework was server-side only, so every
application that called out — a webhook, an OAuth token exchange, a
geolocation lookup — built its own client, with its own timeout and its own
idea of what to retry.

Enable the transport with the framework's `http-client` feature. The client
API and its fake are always available; only the socket costs a TLS stack.

## The fake is the point

```rust
Http::fake().responding(200, r#"{"ok":true}"#);

notify_application(&user).await?;

Http::assert_sent(|request| {
    request.url_contains("/hooks/user-updated") && request.header("x-signature").is_some()
});
```

Without one, asserting that an outbound call happened means standing up a real
server in a test — so nobody does, and the code that signs the webhook is the
code nothing exercises. That is not hypothetical; it is how a webhook signing
bug survives a rewrite.

`Http::fake()` also **refuses to reach the network**. A suite that accidentally
calls a real endpoint is a suite that fails when somebody runs it on a train.

### It follows the one rule every double follows

```rust
Http::assert_sent(|_| true);   // panics if nothing is faking
```

An `assert_sent` that silently passed because somebody forgot `Http::fake()`
would be the most dangerous test in a suite: it guards something and reports
success forever. See [Testing](testing.md#the-one-rule-every-double-follows).

### Queued answers describe a sequence

```rust
let fake = Http::fake();
fake.responding(503, "later").responding(503, "later").responding(200, "yes");
```

Consumed in order; the last one repeats. Which is how a retry test says "fail,
fail, then work".

| | |
|---|---|
| `recorded()` | every request, in order |
| `count()` | how many |
| `assert_sent(f)` / `assert_not_sent(f)` | |
| `assert_sent_count(n)` / `assert_nothing_sent()` | |

A failed assertion lists what *was* sent. "No matching request" with no list is
the least useful assertion failure there is.

### It scopes to the calling thread

`Http::fake()` installs on the current thread, so tests using it run in
parallel like any other — the first version of this did not, and three tests in
six failed on alternate runs because they were overwriting one process-global
transport.

A **spawned** task falls back to the process-wide transport, which is the same
limit [the facade scope](facades.md#scopes) has. `Http::fake_globally()` is for
code under test that spawns; a suite using it has to run those tests one at a
time.

## Requests

```rust
Http::get(url)      Http::post(url)     Http::put(url)
Http::patch(url)    Http::delete(url)   Http::request("HEAD", url)
```

```rust
Http::post(url)
    .bearer(token)                              // Authorization: Bearer …
    .header("x-signature", &signature)
    .accept_json()
    .json(&payload)?                            // or .form(&[..]) or .body(bytes)
    .timeout(Duration::from_secs(10))
    .send()
    .await?
```

`json` returns a `Result` rather than deferring the failure to `send`, so a
value that will not serialise is reported next to the value that caused it.

**There is a thirty-second timeout by default.** A request with none can hold a
worker forever when the other end stops answering without closing, which is
the failure mode that takes a queue down. `without_timeout()` exists and is
almost never right.

## Responses

A response that **arrived** is `Ok`, whatever its status. `Err` means nothing
came back at all.

```rust
let response = Http::get(url).send().await?;

response.status();            // u16
response.is_success();        // 2xx
response.header("etag");
response.text();
let body: Payload = response.json()?;

response.error_for_status()?; // 4xx and 5xx become errors
```

`error_for_status` is not automatic, deliberately: plenty of callers want to
look at a `404` rather than propagate it, and a client that decided for them
would have them parsing the error back apart.

It distinguishes whose fault it was — a `4xx` becomes a `400`-class error and a
`5xx` becomes a `503`-class one — because one is this caller's problem to fix
and the other is worth retrying.

## Retries

```rust
.retry(3, Backoff::exponential())    // 100ms, 200ms, 400ms, capped at 10s
.retry(3, Backoff::fixed(Duration::from_millis(250)))
.retry(3, Backoff::None)             // for a test
```

| | Retried | Why |
|---|---|---|
| a transport failure | yes | a refused connection or a timeout is usually transient |
| `408`, `425`, `429` | yes | the other end said "later" in as many words |
| `5xx` | yes | the other end is having a bad time |
| **`4xx` otherwise** | **no** | the request is wrong, and sending it again keeps it wrong |

The last row is the one that matters. Retrying a `422` four times does not fix
the payload; retrying a `402` charges the card again if the other end is not
idempotent.

Exponential is the default because retrying at full speed is what turns a blip
into an outage — every caller does it at once.

## The transport

```rust
pub trait Transport: Send + Sync + 'static {
    fn send<'a>(&'a self, request: OutboundRequest) -> BoxFuture<'a, Result<RawResponse>>;
    fn name(&self) -> &str;
}
```

The same split the [D1 executor](database.md) and the [KV
cache](cache.md#cloudflare-workers-kv) use: this crate owns the ergonomics, the
retries and the recording, and a transport owns the socket. Keeping them apart
is what lets the fake exist without pretending to be a web server.

```rust
Http::install(Arc::new(ReqwestTransport::with_client(my_client)));

// Or for one call, without changing what the rest of the process does.
Http::get(url).through(Arc::clone(&other)).send().await?;
```

A transport reports **transport** failures. A `500` from the other end is a
successful send of a request that got an unhappy answer.

## What is not here

**No connection pooling knobs, no proxy configuration, no cookie jar, no
streaming upload.** `ReqwestTransport::with_client` takes a client you
configured for any of it — the point of this layer is the ergonomics and the
fake, not hiding a good library.

**No response caching, no circuit breaker.** Both are real needs and both are
policy: what to cache and when to open a circuit are decisions about a
particular dependency, not about HTTP.
