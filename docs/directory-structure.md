# Directory Structure

The sample project's layout maps one-for-one onto the conventional PHP MVC
project layout. If you know where something lives in that tradition, you know
where it lives here.

```text
my-app/
├── Cargo.toml                          composer.json
├── .env / .env.example                 .env / .env.example
├── src/
│   ├── main.rs                         the CLI binary     — the console entry point
│   ├── lib.rs                          —                  — the module tree
│   ├── bootstrap.rs                    bootstrap/app.php  — assembles and boots the app
│   ├── config.rs                       config/*.php       — configuration, read from .env
│   ├── app/
│   │   ├── models/                     app/Models
│   │   ├── http/
│   │   │   ├── kernel.rs               app/Http/Kernel
│   │   │   ├── controllers/            app/Http/Controllers
│   │   │   ├── middleware/             app/Http/Middleware
│   │   │   └── requests/               app/Http/Requests
│   │   ├── providers/                  app/Providers
│   │   ├── jobs/                       app/Jobs
│   │   ├── mail/                       app/Mail
│   │   ├── notifications/              app/Notifications
│   │   ├── repositories/               —                  — the data-access layer
│   │   ├── policies/                   app/Policies
│   │   └── console/commands/           app/Console/Commands
│   ├── database/
│   │   ├── migrations/                 database/migrations
│   │   └── seeders.rs                  database/seeders
│   └── routes/
│       ├── web.rs                      routes/web.php
│       ├── api.rs                      routes/api.php
│       ├── channels.rs                 routes/channels.php
│       ├── openapi.rs                   —                  — what the router cannot know
│       └── console.rs                  routes/console.php
├── resources/views/                    resources/views
├── storage/
│   ├── logs/                           storage/logs
│   └── mail/                           —                  — .eml files, in `file` mail mode
└── tests/feature.rs                    tests/Feature
```

## What goes where

| To add a… | Write it in | And register it in |
|---|---|---|
| [model](models.md) | `app/models/` | `app/models/mod.rs` |
| [controller](controllers.md) | `app/http/controllers/` | `app/http/controllers/mod.rs` + a route |
| [route](routing.md) | `routes/web.rs` or `routes/api.rs` | — |
| [request contract](validation.md#request-contracts) | `app/http/requests/` | `app/http/requests/mod.rs` |
| [middleware](middleware.md) | `app/http/middleware/` | `app/http/kernel.rs` |
| [job](queues.md) | `app/jobs/` | the `JobRegistry` in `app/providers/app_provider.rs` |
| [mailable](mail.md) | `app/mail/` | — (constructed where it is sent) |
| [notification](notifications.md) | `app/notifications/` | — (constructed where it is sent) |
| [broadcast channel rule](broadcasting.md) | `routes/channels.rs` | — (the list *is* the registration) |
| [repository](repositories.md) | `app/repositories/` | `app/providers/repository_provider.rs` |
| [policy](authorization.md) | `app/policies/` | — (called from a controller) |
| [command](console.md) | `app/console/commands/` | `routes/console.rs` |
| [migration](migrations.md) | `database/migrations/` | `database/migrations/mod.rs` — append to `all()` |
| [listener](events.md) | `app/providers/` | `EventServiceProvider::register_listeners` |
| service | anywhere | `app/providers/app_provider.rs` |

## The differences

### `src/lib.rs` **and** `src/main.rs`

```toml
[lib]
name = "app"
path = "src/lib.rs"

[[bin]]
name = "app"
path = "src/main.rs"
```

The binary is the console entry point; the library is
what the tests boot. Without the split, `tests/feature.rs` could not reach your
application's own types — an integration test can only see a **library**
target.

### There is no `public/`

There is no document root, because there is no web server in front of the
application. `serve` **is** the server. Static assets are served by whatever is
in front of it in production — nginx, Cloudflare, a CDN — or by a route you
write.

### There is no `config/` directory

Rainier's `config.rs` is one function per concern, all writing into the same
dotted tree:

```rust
pub fn configure(config: &Config, env: &Env) -> Result<()> {
    app(config, env)?;
    posts(config, env)?;
    Ok(())
}

fn posts(config: &Config, env: &Env) -> Result<()> {
    config.set("posts.per_page", env.int("POSTS_PER_PAGE", 15))?;
    config.set("posts.max_per_page", env.int("POSTS_MAX_PER_PAGE", 100))?;
    Ok(())
}
```

Same dotted keys, one file. Split it into a module if it grows. See
[Configuration](configuration.md).

### `database/migrations/mod.rs` lists the order

Migrations are ordered. Nothing is discovered from a scan over timestamped
filenames, so the order is a list you can read:

```rust
// src/database/migrations/mod.rs
Migrator::new()
    .add(m0001_create_users::migration())
    .add(m0002_create_posts::migration())
    .add(m0007_create_post_tag::migration())
    .merge(DatabaseChannel::migrations())
```

One module per migration — its name, its `up` and its `down` in one file — and
the order in this one. See [Migrations](migrations.md).

## Two things Rust does differently

Neither is a Rainier decision — both fall out of the language, and both are
what a developer arriving from PHP hits first.

### 1. Nothing is autoloaded

Every directory has a `mod.rs` listing its files. Adding a controller means
adding one line:

```rust
// src/app/http/controllers/mod.rs
pub mod auth_controller;
pub mod home_controller;
pub mod post_controller;      // ← the new one
```

The compiler tells you when you forget, because nothing will reference it.

### 2. Nothing is discovered by name

| Discovered by convention elsewhere | Rainier is told |
|---|---|
| a providers array in a config file | `.with_provider(…)` in `bootstrap.rs` |
| a `Policy` suffix matching a model | `PostPolicy::gate()`, called explicitly |
| a listener map on a provider | `events.listen(…)` |
| a job class name in the payload | `registry.register::<Job>()` |
| a resource name matching a controller | you pass the controller |

More typing. In exchange, there is no "why isn't my listener firing?" — the
wiring is one grep away, and a missing registration is a compile error or a
startup failure rather than silence.

## Module conventions

The sample follows two conventions worth keeping:

**A `mod.rs` re-exports what the module offers**, so callers write
`crate::app::models::Post` rather than `crate::app::models::post::Post`:

```rust
// src/app/models/mod.rs
pub mod post;
pub mod user;

pub use post::{Post, PostPublished};
pub use user::User;
```

**Tests live next to the code**, in a `#[cfg(test)] mod tests` at the bottom of
the file. `tests/feature.rs` is for end-to-end tests that boot the application.
See [Testing](testing.md).

## Storage

```text
storage/
├── logs/.gitkeep
└── mail/.gitkeep
```

Both are git-ignored except for the `.gitkeep`, because the directories must
exist for the `file` mail transport to write into. There is no `framework/`
subdirectory: Rainier compiles no views and caches no config to disk.
