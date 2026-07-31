# Rainier

A batteries-included MVC framework for Rust.

```toml
[dependencies]
rainier = { version = "1", features = ["sea-orm-executor"] }
```

```rust
use rainier::prelude::*;

async fn index() -> &'static str {
    "Hello from Rainier"
}

#[tokio::main]
async fn main() -> Result<()> {
    let app = Rainier::new(".")
        .with_routes(|router| {
            router.get("/", index).name("home");
        })
        .boot()
        .await?;

    rainier::console("rainier").run_from_env(&app).await;
    Ok(())
}
```

This crate is [`rainier-framework`](https://crates.io/crates/rainier-framework)
re-exported — the same crate under a shorter name, versioned together. Depend
on whichever reads better.

**[Documentation](https://github.com/safewords/rainier-framework/tree/main/docs)**
· [Coming from another MVC framework](https://github.com/safewords/rainier-framework/blob/main/docs/README.md)
· [Starter app](https://github.com/safewords/rainier-sample-project)

## Licence

MIT OR Apache-2.0.
