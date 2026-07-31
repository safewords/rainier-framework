<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/rainier_framework_logo_primary_lockup_dark.svg">
    <img src="assets/rainier_framework_logo_primary_lockup.svg" alt="Rainier — an MVC framework for Rust" width="520">
  </picture>
</p>

<p align="center">
  <a href="https://github.com/safewords/rainier-framework/actions/workflows/ci.yml"><img src="https://github.com/safewords/rainier-framework/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="https://github.com/safewords/rainier-framework/actions/workflows/ci.yml"><img src="https://img.shields.io/endpoint?url=https%3A%2F%2Fraw.githubusercontent.com%2Fsafewords%2Frainier-framework%2Fmain%2F.github%2Fbadges%2Ftests.json%3Fv%3D1&labelColor=1B2024" alt="Tests"></a>
  <a href="https://crates.io/crates/rainier-framework"><img src="https://img.shields.io/crates/d/rainier-framework?label=downloads&color=C75232&labelColor=1B2024" alt="Downloads"></a>
  <a href="https://github.com/safewords/rainier-framework/actions/workflows/ci.yml"><img src="https://img.shields.io/badge/rust-1.88%2B-C75232?logo=rust&logoColor=white&labelColor=1B2024" alt="Rust 1.88+"></a>
  <a href="#licence"><img src="https://img.shields.io/badge/licence-MIT%20OR%20Apache--2.0-C75232?labelColor=1B2024" alt="MIT OR Apache-2.0"></a>

</p>

The structures an MVC application is made of — container, providers, router,
form requests, guards, jobs, mailables, facades, events — each given a native
Rust shape. Designed to provide a smooth transition for developers familiar
with Laravel.

**📚 [Documentation](docs/)** · [Concept map](docs/README.md#the-concept-map) · [Starter app](https://github.com/safewords/rainier-sample-project)

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

## Quick start

```sh
git clone https://github.com/safewords/rainier-sample-project.git my-app
cd my-app
cargo run -- app:seed
cargo run -- serve
```

[rainier-sample-project] is a complete application against a real SQLite
database — models, repositories, groups, request contracts, a token guard, a
policy, an event, a queued job, mailables, views, console. Laid out exactly as
[Directory Structure](docs/directory-structure.md) describes.

## The components

Every component is its own crate and depends only on what it needs, so an
application that wants the queue but not HTTP pays for neither the router nor
hyper.

| Crate | Owns | Depends on |
|---|---|---|
| `rainier-support` | `Error`, `Result`, `BoxFuture`, type-maps, inflection | — |
| `rainier-container` | IoC container, service providers, lifecycle hooks, facades | support |
| `rainier-config` | config repository, `.env` | support |
| `rainier-events` | the event dispatcher — the **hook** bus | support |
| `rainier-http` | requests, responses, cookies, uploads, extractors | support |
| `rainier-middleware` | `handle(request, next)` pipeline, registry, built-ins | http |
| `rainier-http-client` | the **outbound** client and its fake | support |
| `rainier-routing` | route declaration, groups, resources, URL generation | http, middleware |
| `rainier-validation` | rules, the validator, **request contracts** | http |
| `rainier-view` | the template engine, escaped by default | support |
| `rainier-database` | models, **repositories**, **relationships**, the ORM seam | support, events, orm |
| `rainier-auth` | **guards**, user providers, hashing, gates | http, middleware, database |
| `rainier-queue` | **jobs**, queue drivers, the worker | container, events, database |
| `rainier-mail` | **mailables**, the mailer, transports | view, events |
| `rainier-notify` | **notifications**, notifiables, channels | mail, support |
| `rainier-broadcast` | **broadcast** channels, drivers, subscription auth | support |
| `rainier-websocket` | **socket** handlers, rooms, the socket handle | http |
| `rainier-metrics` | the **Prometheus** registry and request timing | middleware |
| `rainier-openapi` | the **OpenAPI** document, and rules-to-schema | routing, validation |
| `rainier-telemetry` | **trace context**, and OTLP behind a feature | middleware |
| `rainier-server` | the HTTP kernel and the hyper server | routing, middleware |
| `rainier-console` | the console kernel and its commands | container |
| `rainier-crypt` | **ciphers**, signing, key rotation | support |
| `rainier-drivers` | Redis / Redis Cluster / Memcached / Kafka / AWS **transports** | support |
| `rainier-cache` | the **cache** port, its drivers, and **atomic locks** | support, drivers |
| `rainier-filesystem` | the **storage** port — local, memory, S3/R2 | support, drivers |
| `rainier-scheduler` | **cron** tasks, `without_overlapping`, `on_one_server` | container, cache |
| `rainier-session` | session bag, stores, middleware | http, middleware, database, cache, crypt |
| `rainier-orm` | one `#[derive(Entity)]` across SQLite, MySQL, Postgres and D1 | — |
| `rainier-framework` | facades, bootstrap, built-in commands, the prelude | everything |

A DAG with no cycles, and no component reaches sideways: `routing` does not know
`auth` exists, `mail` does not know about HTTP, `database` does not know a
request is involved.

## The pieces

### Routing

```rust
router.group(GroupAttributes::new().prefix("api").name("api.").middleware(kernel::api()), |router| {
    router.get("/posts", index).name("posts.index");
    router.get("/posts/{post}", show).name("posts.show").where_slug("post");

    router.group(GroupAttributes::new().middleware(kernel::auth("api")), |router| {
        router.post("/posts", store).name("posts.store");
    });
});

router.resource("comments", Arc::new(CommentController));  // the seven RESTful routes
```

Named routes generate URLs (`urls.route("api.posts.show", &[("post", "hello")])`),
groups nest, parameters take constraints, `route:list` prints the table.

### Controllers and extractors

An action is a plain `async fn`; its parameters say what it needs.

```rust
async fn store(
    request: Req,
    Validated(input): Validated<StorePost>,
) -> Result<Response> { … }
```

### Request contracts

A request contract authorises, validates, and hands the action a typed payload
containing **only** the fields the rules named — mass-assignment protection out
of the rules you already wrote.

```rust
#[derive(Deserialize)]
struct StorePost { title: String, body: String }

#[async_trait]
impl FormRequest for StorePost {
    fn rules() -> RuleSet {
        vec![
            field("title", [Rule::Required, Rule::String, Rule::Between(3.0, 120.0)]),
            field("body",  [Rule::Required, Rule::Min(10.0)]),
        ]
    }

    async fn authorize(request: &Request) -> bool { … }
}
```

### Middleware

```rust
#[async_trait]
impl Middleware for RequireApiKey {
    async fn handle(&self, request: Request, next: Next) -> Response {
        match request.header("x-api-key") {
            Some("secret") => next.run(request).await,
            _ => Response::new(StatusCode::UNAUTHORIZED),
        }
    }
}
```

Middleware sits *around* the rest of the chain — inspect the response, wrap the
call, or decline to call `next`. Routes attach it **by value**, never by name: a
group is a function returning a `MiddlewareStack`, so a typo is a compile error
rather than a route that quietly runs unguarded.

The built-ins cover CORS, throttling, trusted proxies, input trimming, sessions
and authentication, plus three that are configuration rather than code:
`Timeout` answers `408` when a handler overruns, `Compress` gzips a text
response the client asked for, and `MethodOverride` lets an HTML form spell
`PUT` and `DELETE`.

### Models, repositories, hooks

```rust
#[derive(Entity, Clone)]
#[orm(table = "posts")]
struct Post { #[orm(pk, auto_increment)] id: u64, slug: String, title: String }

impl Model for Post {
    fn route_key_name() -> &'static str { "slug" }   // bind /posts/{post} by slug
}

let page = posts
    .paginate_matching(Criteria::new().where_eq("published", true), 1, 20)
    .await?;
```

`EntityRepository<M>` implements `Repository<M>` for **any** model, so declaring
a repository is no code. Writes dispatch `Creating` / `Created` / `Updating` / …
through the event bus; a `-ing` listener returning `Err` **vetoes** the write.

### Guards and gates

```rust
let auth = AuthManager::<User>::new("api")
    .register(Arc::new(TokenGuard::new("api", provider)));

router.get("/me", me).middleware(["auth:api"]);
```

Authentication answers *who*, a `Gate` answers *whether they may*. An undefined
ability is denied, so a typo fails closed.

### Jobs and the queue

```rust
#[async_trait]
impl Job for NotifyAuthor {
    const NAME: &'static str = "blog.notify-author";   // stable on the wire
    const QUEUE: &'static str = "mail";
    const TRIES: u32 = 5;

    async fn handle(&self, context: &JobContext) -> Result<()> { … }
}

Queue::instance().dispatch(NotifyAuthor { post_id }).await?;
```

Drivers: `SyncQueue` (inline), `MemoryQueue` (tests), `DatabaseQueue` (on the
database you already have, with an optimistic claim so two workers cannot take
the same job).

### Mailables

```rust
impl Mailable for WelcomeEmail {
    fn envelope(&self) -> Envelope { Envelope::new("Welcome!").to(self.email.clone()) }
    fn content(&self) -> Result<Content> {
        Content::view("mail.welcome", json!({ "name": self.name }))
    }
}
```

`Mailable::build` does no I/O, so the interesting part is testable without a
mail server. `Mailer::always_to` redirects everything to one address.

### Migrations

```rust
Step::create("0007_create_post_tag", "post_tag", |table| {
    table.foreign_id("post_id").constrained_on("posts").cascade_on_delete();
    table.foreign_id("tag_id").constrained_on("tags").cascade_on_delete();
    table.primary(["post_id", "tag_id"]);
})
```

A schema builder with **no SQL in it** — the three
engines disagree about nearly every line of a `CREATE TABLE`
(`AUTOINCREMENT`/`AUTO_INCREMENT`/`bigserial`, `blob`/`bytea`,
`CREATE INDEX IF NOT EXISTS` which MySQL rejects), and reconciling that is what
a DBAL is for.

`Step::table` alters an existing table and **derives its own `down`** from what
changed, so the half of a migration that goes stale first cannot. Where a change
genuinely cannot be undone it says which one and refuses before it starts.

### Relationships

```rust
impl Post {
    pub fn author() -> BelongsTo<Post, User> { BelongsTo::new().foreign_key("author_id") }
    pub fn tags() -> BelongsToMany<Post, Tag> { BelongsToMany::new("post_tag") }
}

let authors = Post::author().load(&posts, &*users).await?;   // ONE query
let name = &authors.one(&post).unwrap().name;
```

`has_one`, `has_many`, `belongs_to`, `belongs_to_many`, and counting without
loading. There is
no lazy `post.author`, because Rust has no `__get` to hang one on — and the
consequence is the point: **N+1 is unrepresentable**, since `load` takes a
slice and there is no per-model load to put in a loop. It is a `WHERE key IN
(…)` against the other side's own repository, never a `JOIN`, so the two sides
stay free to live in different backends.

### Broadcasting

```rust
impl Broadcastable for OrderShipped {
    fn broadcast_on(&self) -> Vec<Channel> {
        vec![Channel::private(format!("orders.{}", self.order_id))]
    }
}

Broadcast::instance().event(&OrderShipped { .. }).await?;
```

Public, private and presence channels; a channel table that **fails closed**,
so an undeclared channel is denied rather than readable; `/broadcasting/auth`
with the Pusher protocol's HMAC. Publishing goes over Redis pub/sub, which is
what soketi and other Pusher-protocol servers read — Rainier is not the
WebSocket server; publishing and authorising subscriptions are the two halves
an application owns.

### Observability

```rust
// config/metrics.rs, config/openapi.rs, config/telemetry.rs
config.set(METRICS_ENABLED, env.bool("METRICS_ENABLED", false))?;
```

Prometheus metrics, an OpenAPI document and OpenTelemetry — three optional
crates, each with its own config section and all off by default, because each
one **exposes** something worth deciding about.

Logs are the exception, since an application logs whether or not anyone asked:
`LOG_FORMAT` defaults to `auto`, which is JSON in production and staging and
readable everywhere else, with the fields flattened to the top level where an
aggregator's parser finds them.

The metrics label routes by **pattern**, so `/posts/1` and `/posts/2` are one
series rather than a way to fill a monitoring system. The OpenAPI request
schemas come from **the validator's own rules**, so the document cannot
describe a body the endpoint would reject. Trace context propagates with no
exporter at all — the trace id joins, lands on every log line and comes back on
the response — and OTLP export is behind a feature.

### Notifications

```rust
impl Notification<User> for PostLive {
    fn notification_name(&self) -> &'static str { "post.live" }
    fn via(&self, _: &User) -> Channels {
        Channels::new().with::<DatabaseChannel>().with::<MailChannel>()
    }
    fn to_mail(&self, to: &User) -> Option<Box<dyn Mailable>> { … }
    fn to_data(&self, _: &User) -> Option<Value> { … }
}

Notify::instance().send(&author, &PostLive { post }).await?;
```

An **event** is a fact with no recipient; a **notification** is a message to a
named one, and `via()` decides its channels per recipient. Channels are
selected by type, and the notification declares three renderings — mail, text,
data — rather than one method per channel, so the set of channels stays open.

### Views

Directive-based templates: `{{ escaped }}`, `{!! raw !!}`,
`@if`/`@elseif`/`@else`, `@foreach`, `@include`,
`@extends`/`@section`/`@yield`. **Escaped by default.**

### Facades

```rust
Config::instance().string("app.name");
Event::instance().dispatch(PostPublished { post }).await?;
Queue::instance().dispatch(job).await?;
```

Every call resolves through the container, so rebinding the accessor swaps what
every call site sees — which is how a test installs a fake.

### Encryption, sessions, storage

```rust
let sealed = Crypt::instance().encrypt("a card number")?;   // five AEAD ciphers
let signed = Crypt::instance().sign("unsubscribe-42")?;     // readable, not editable

router.get("/dashboard", dashboard).middleware(["session"]);
```

Every payload records which key wrote it, so key rotation is a deploy. Sessions
have four drivers — memory, database, cache (Redis, sharded cluster, Memcached),
cookie — behind one port; storage has local, memory and S3/R2.

### Console

```sh
cargo run -- list
cargo run -- route:list [--json]
cargo run -- migrate [--pretend]
cargo run -- queue:work [--queue=mail] [--once]
cargo run -- serve [--port=3000]
```

## Design positions

Places where Rainier departs from the conventional MVC answer, deliberately.
The reasoning lives next to the code.

- **Middleware is attached by value, never by name.** Reflection by string is a
  route that silently runs unguarded when you misspell it; a `MiddlewareStack`
  is checked by the compiler.
- **A notification declares three renderings, not one method per channel.**
  Rust has no `__call`, so closing the *representations* is what keeps the set
  of channels open.
- **A relationship is loaded for a slice, never for one model.** No `__get`
  means no lazy loading; making the batch the only shape means N+1 cannot be
  written.
- **The auth manager is generic over the user model**, not `dyn Authenticatable`.
  Erasing it would only force a downcast at every call site.
- **Model hooks veto, they do not mutate.** Otherwise the outcome depends on
  listener registration order.
- **Request bodies buffer; response bodies stream.** Buffering is what lets
  `request.input("title")` be synchronous.
- **Validation distinguishes absent, null and empty.** Only presence rules look at
  the first two, so `[Rule::Email]` means "if supplied, must be an email".
- **`5xx` messages never reach the client** unless debug is on — they contain
  connection strings and queries. `4xx` always does; it describes what the client did.

## Building an application

1. **Depend on `rainier-orm` directly.** `#[derive(Entity)]` expands to
   `::rainier_orm::…`, so the framework's re-export is not enough.
2. **Do not call `bind_executor!` for an executor you did not write.** The
   `Connection`/`SeaOrmExecutor` impl ships behind the `sea-orm-executor` feature;
   the orphan rule puts it out of an application's reach. `bind_executor!` is for
   *your* executors.
3. **Run migrations outside a spawned task.** `Migrator::run` boxes each step
   behind `dyn`, which erases `Send`. Everything else is `Send` and
   `rainier-orm/tests/send_futures.rs` asserts it at compile time.

## Testing

```sh
cargo test --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

A feature test is three lines:

```rust
let app = TestApp::new(boot(Mode::Testing).await?)?;

app.get("/health").await.assert_ok().assert_json_path("database.status", "ok");
```

It drives the real kernel — real routes, real middleware, real database, real
migrations — and scopes the facades to the thread it runs on, so tests that
each boot their own application no longer resolve out of each other's
containers.

Every component also ships a test double, and each refuses to let an assertion
pass vacuously — `Dispatcher::fake()`, `QueueManager::fake()`, `Mailer::fake()`
and `MemoryConnection` all panic if you assert against a real instance instead
of a recording one.

See [Testing](docs/testing.md).

## Changelog

[CHANGELOG.md](CHANGELOG.md), in
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) format. Every crate in
the workspace shares one version, so an entry there applies to the release as a
whole.

## Trademarks

Rainier is an independent project. It is not affiliated with, sponsored by, or
endorsed by the Laravel project or its trademark holders, and it claims no
rights in the Laravel name or marks. The name appears in this repository only
to identify the framework whose developers Rainier aims to make at home.

## Licence

MIT OR Apache-2.0.

[rainier-sample-project]: https://github.com/safewords/rainier-sample-project
