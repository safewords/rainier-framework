# Installation

## Requirements

- **Rust 1.88** or later. The workspace declares `rust-version = "1.88"`, which
  is the floor for Rainier's own code and for the wasm-safe default build; some
  drivers pull in third-party crates with higher ones.

  | Features | Needs |
  |---|---|
  | default (no driver) | 1.88 |
  | `sea-orm-executor`, `d1-http`, `libsql-http` | 1.88 |
  | `http-client` | 1.88 |
  | `kafka`, `kafka-tls` | 1.88 |
  | `aws-s3`, `aws-sqs`, `aws-dynamodb` | 1.94 |

- A database, or nothing at all — SQLite in memory is the default and needs no
  setup

## Start from the sample project

Do not start from an empty crate. [rainier-sample-project] is a complete,
working application with the layout the rest of these pages assume, and it is
what the framework is designed around.

```sh
git clone https://github.com/safewords/rainier-sample-project.git my-app
cd my-app
cp .env.example .env

cargo run -- app:seed      # a demo user and a few posts
cargo run -- serve         # http://127.0.0.1:8000
```

That works on a fresh clone with nothing installed, because it runs against
SQLite in memory. Point `DATABASE_URL` at a file, MySQL or Postgres when you
want the data to survive — nothing else in the app changes.

Then rename it:

```toml
# Cargo.toml
[package]
name = "my-app"
```

```toml
[lib]
name = "my_app"
path = "src/lib.rs"

[[bin]]
name = "my-app"
path = "src/main.rs"
```

## Adding Rainier to an existing crate

```toml
[dependencies]
rainier-framework = { git = "https://github.com/safewords/rainier-framework.git", features = [
    "sea-orm-executor",
] }

# The ORM lives in the same repository — it is a workspace member, not a
# separate crate to version against.
rainier-orm = { git = "https://github.com/safewords/rainier-framework.git" }

async-trait = "0.1"
chrono = { version = "0.4", features = ["clock", "serde"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tokio = { version = "1", features = ["rt-multi-thread", "macros", "signal"] }
tracing = "0.1"
```

```rust
use rainier_framework::prelude::*;

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

    rainier_framework::console("rainier").run_from_env(&app).await;
    Ok(())
}
```

## The two rules that bite everyone once

Both of these produce confusing errors, and both have a one-line fix.

### 1. Depend on `rainier-orm` directly

`#[derive(Entity)]` expands to `::rainier_orm::…` — an **absolute path**. The
framework re-exporting Rainier ORM is not enough to satisfy it.

```
error[E0433]: failed to resolve: use of undeclared crate or module `rainier-orm`
```

Add it to `[dependencies]` as shown above. Both must resolve to the **same
version**, which is why the sample project pins both to git and why the
lockfile records both.

### 2. Do not call `bind_executor!` for an executor you did not write

```rust
// Wrong, in an application:
rainier_framework::bind_executor!(rainier_drivers::sql::SeaOrmExecutor);
```

```
error[E0117]: only traits defined in the current crate can be implemented
              for types defined outside of the crate
```

The orphan rule. `Connection` belongs to `rainier-database` and `SeaOrmExecutor`
to Rainier ORM, so that impl is out of an application's reach. Rainier ships it
behind the `sea-orm-executor` feature:

```toml
rainier-framework = { git = "…", features = ["sea-orm-executor"] }
```

`bind_executor!` is for executors **you** wrote. See
[Database](database.md#registering-a-backend).

## Features

| Feature | Gives you |
|---|---|
| `sea-orm-executor` | the native MySQL / Postgres / SQLite driver |
| `d1-http` | Cloudflare D1 over HTTP |
| `libsql-http` | libSQL / Turso over HTTP |

Pick one. All are off by default, which is what keeps the crates usable from a
wasm target — an application targeting a Worker drops `sea-orm-executor` and
adds `d1-http`, and nothing else changes.

## Verify it works

```sh
cargo run -- list          # the command list
cargo run -- route:list    # the route table
cargo test
```

`route:list` printing your routes means the router compiled, the container
built every middleware that asked it for a service, and the kernel is bound. If
that works, the application is wired.

## Editor setup

`rust-analyzer` will want to build everything once — the first check is slow
and every one after it is not.

The workspace is clippy-clean, so it is worth keeping it that way:

```sh
cargo clippy --workspace --all-targets
```

## Next

- [Directory Structure](directory-structure.md) — where everything goes
- [The Request Lifecycle](lifecycle.md) — how a request becomes a response
- [Routing](routing.md) — the first thing you will write

[rainier-sample-project]: https://github.com/safewords/rainier-sample-project
