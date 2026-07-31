# rainier-http-client

The outbound HTTP client for the [Rainier](https://github.com/safewords/rainier-framework)
framework — a fluent `Http::` facade, with a fake that makes an outbound call
assertable.

```rust,ignore
let response = Http::post("https://hooks.example.com/user-updated")
    .json(&payload)?
    .timeout(Duration::from_secs(10))
    .retry(3, Backoff::exponential())
    .send()
    .await?;
```

```rust,ignore
Http::fake();

notify_application(&user).await?;

Http::assert_sent(|request| request.url_contains("/user-updated"));
```

Without the fake, asserting that an outbound call happened means standing up a
real server in a test — so nobody does, and the code that signs the webhook is
the code nothing exercises.

See the [documentation](https://docs.rs/rainier-http-client).

## Licence

MIT OR Apache-2.0.
