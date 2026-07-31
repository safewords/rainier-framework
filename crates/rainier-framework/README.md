# rainier-framework

An MVC framework for Rust — the umbrella crate that assembles the
`rainier-*` components and adds the facades, the bootstrap builder and the
prelude.

```toml
[dependencies]
rainier-framework = { version = "1", features = ["sea-orm-executor"] }
```

Also published as [`rainier`](https://crates.io/crates/rainier), which is this
crate re-exported under a shorter name. They are versioned together; depend on
whichever reads better.

**[Documentation](https://github.com/safewords/rainier-framework/tree/main/docs)**
· [Starter app](https://github.com/safewords/rainier-sample-project)

## Licence

MIT OR Apache-2.0.
