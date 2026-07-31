# Testing

```sh
cargo test
cargo test --workspace
```

Two kinds:

- **Unit tests** live next to the code, in `#[cfg(test)] mod tests`
- **Feature tests** live in `tests/feature.rs` and boot the real application

## The one rule every double follows

> **Every fake refuses to let an assertion pass vacuously.**

`Dispatcher::fake()`, `QueueManager::fake()`, `Mailer::fake()` and
`MemoryConnection` all **panic** if you assert against a real instance instead
of a recording one.

```rust
// Against a real mailer, this panics rather than passing.
Mail::instance().assert_nothing_sent();
```

Without that, the most dangerous test in a suite is the one that asserts
something did *not* happen: forget to install the fake and it passes forever,
for entirely the wrong reason, while the thing it was guarding quietly breaks.

## Feature tests

```rust
//! tests/feature.rs — the whole application, exercised from outside.

use app::{boot, Mode};
use rainier_framework::prelude::*;
use rainier_framework::testing::TestApp;

#[tokio::test]
async fn the_api_lists_published_posts() {
    let app = TestApp::new(boot(Mode::Testing).await?)?;

    app.get("/api/posts").await.assert_ok().assert_json_path("data.0.title", "Hello");
}
```

Three lines: make a request, assert the status, assert something about the
body. Anything longer is the harness leaking.

These drive the **real kernel** — real routes, real middleware, real database,
real migrations. Only the mail transport is a double, and only so a test can
assert on what was sent. A test that stubs the router is testing your stub.

### `TestApp`

[`TestApp::new`] takes an application **your** `boot` produced, rather than
booting one itself: what a test wants exercised is your bootstrap — its
providers, its configuration, its routes — not a generic one the framework
could assemble.

```rust
let app = TestApp::new(boot(Mode::Testing).await?)?;

app.get("/api/posts").await;
app.post("/api/posts", &json!({ "title": "Hello" })).await;
app.put("/api/posts/1", &body).await;
app.patch("/api/posts/1", &body).await;
app.delete("/api/posts/1").await;
app.post_empty("/api/logout").await;
```

Headers that every request should carry are set once:

```rust
let app = TestApp::new(boot(Mode::Testing).await?)?
    .with_token(token)                       // authorization: Bearer …
    .with_header("accept-language", "fr");
```

And for the request the helpers do not cover — a form encoding, an odd header,
a deliberately malformed body — build it by hand and it still carries them:

```rust
let response = app.send(
    app.request(Method::POST, "/api/posts")
        .header("content-type", "application/x-www-form-urlencoded")
        .body("title=Hello")
        .build(),
).await;
```

`app.resolve::<PostRepository>()?` reaches the container, for the half of a
feature test that asserts on state rather than on a response.

### `TestResponse`

The body is read on construction, so every assertion takes `&self` and chains,
and the response is still there afterwards to read:

```rust
let response = app.get("/api/posts/1").await;

response
    .assert_ok()
    .assert_json_path("title", "Hello")
    .assert_json_missing("author.password")
    .assert_header("content-type", "application/json; charset=utf-8");

let post: PostView = response.json_as();
```

| Assertion | Passes when |
|---|---|
| `assert_ok()` | the status is `2xx` |
| `assert_status(code)` | the status is exactly `code` |
| `assert_created()` / `assert_no_content()` | `201` / `204` |
| `assert_not_found()` | `404` |
| `assert_unauthorized()` / `assert_forbidden()` | `401` / `403` |
| `assert_invalid()` | `422` — a failed request contract |
| `assert_json_path(path, value)` | the JSON at `path` equals `value` |
| `assert_json_missing(path)` | there is nothing at `path` |
| `assert_contains(text)` | the body contains `text` |
| `assert_header(name, value)` / `assert_header_missing(name)` | |

`path` is dotted, and an index is a segment: `data.0.title`. Every failure
prints the body, because the body is where the reason is.

Reading rather than asserting: `status()`, `text()`, `header(name)`, `json()`
for a `serde_json::Value`, and `json_as::<T>()` for your own type.

`assert_json_missing` is about *absence*, and a `null` the API deliberately
sends is present. That distinction is what makes it the right assertion for
"the password hash was never serialised".

## Facades are scoped to the test

The container facades resolve through is **one process-global slot**, so a
suite that booted an application per test used to race: two tests boot, the
second wins, and the first starts resolving out of a container that has been
replaced. The symptom is `nothing is bound for …` on a random subset of tests
on a random subset of runs.

[`TestApp`] holds a [`FacadeScope`], which overrides the global one **for the
thread the test runs on** — so each test gets its own application and its own
configuration, and they do not interfere.

```rust
use rainier_framework::container::scope_facade_application;

let _scope = scope_facade_application(Arc::clone(&app));
// Facades on this thread resolve through `app` until `_scope` is dropped.
```

`#[tokio::test]` uses a current-thread runtime, so a test and everything it
awaits stay on one thread. Under `#[tokio::test(flavor = "multi_thread")]` a
future can move, and after the move a facade resolves through whatever was
installed globally — so a multi-threaded test wanting its own container should
keep the work on one task.

### The boot itself still needs a lock

Scoping fixes resolution, not registration. The bootstrap installs its
application globally **before** the providers run, because a provider's `boot`
and a middleware alias resolved while the router compiles both legitimately
reach for a facade during that call. Two boots at the same instant can still
cross.

So serialise the boot, and only the boot:

```rust
static BOOTING: std::sync::Mutex<()> = std::sync::Mutex::new(());

async fn boot_for_test() -> TestApp {
    let app = {
        let _booting = BOOTING.lock().unwrap_or_else(|e| e.into_inner());
        boot(Mode::Testing).await.expect("the application should boot")
    };

    TestApp::new(app).expect("a kernel")
}
```

Two details worth copying:

- **`unwrap_or_else(|e| e.into_inner())`** — one failing test poisons the
  mutex, and a poisoned mutex would fail every subsequent test with an
  unrelated message. Recovering keeps the first failure the only failure.
- **`#![allow(clippy::await_holding_lock)]`** — holding it across the boot's
  awaits is the entire point, and it is safe because `#[tokio::test]` runs on a
  current-thread runtime, so the guard never crosses a thread.

The alternative is `.without_facades()`, which skips the global entirely —
right for a test that resolves everything from the container by hand.

## `APP_ENV` is production unless you say otherwise

The one that catches everybody. [`AppEnv`] defaults to **production** when
`APP_ENV` is unset, which is the right default for a deployment and the wrong
one for a test: several boot checks refuse in production where they would
otherwise warn.

Say it in the bootstrap rather than leaving it to the environment:

```rust
.configure(|config| {
    if mode == Mode::Testing {
        let _ = config.set(keys::APP_ENV, AppEnv::Testing);
    }
})
```

Setting it in the config tree rather than in `Env` is deliberate: `Env::get`
consults the **process** environment first, so a machine that happens to export
`APP_ENV` would otherwise win.

## An environment a test can state

That same rule — a real variable beats the `.env` file — is right in production
and untestable: a test that sets `MAIL_DRIVER=log` in a map still sees whatever
the machine exports.

```rust
// Nothing but what it was given.
let env = Env::from_map([("MAIL_DRIVER", "log"), ("APP_ENV", "testing")]);

// Or a parsed `.env` body, sealed off the same way.
let env = Env::parse("MAIL_DRIVER=log").isolated();

assert_eq!(env.get("PATH"), None);
```

[`from_map`] implies isolation, because a map is a complete statement of intent
by definition. `is_isolated()` answers which mode an `Env` is in.

[`TestApp`]: https://docs.rs/rainier-framework/latest/rainier_framework/testing/struct.TestApp.html
[`TestApp::new`]: https://docs.rs/rainier-framework/latest/rainier_framework/testing/struct.TestApp.html#method.new
[`FacadeScope`]: https://docs.rs/rainier-container/latest/rainier_container/struct.FacadeScope.html
[`AppEnv`]: https://docs.rs/rainier-config/latest/rainier_config/enum.AppEnv.html
[`from_map`]: https://docs.rs/rainier-config/latest/rainier_config/struct.Env.html#method.from_map

## Unit tests

Most of the framework is testable without booting anything.

### An action is a function

```rust
#[tokio::test]
async fn health_is_ok() {
    assert_eq!(health().await, "ok");
}
```

### Middleware needs only a pipeline

```rust
async fn run(generator: Arc<RequestIdMiddleware>, request: Request) -> Response {
    Pipeline::new()
        .through_arc(generator as Arc<dyn Middleware>)
        .then(|request: Request| async move {
            Response::text(request.extension::<RequestId>().map(|id| id.0.clone()).unwrap_or_default())
        })
        .run(request)
        .await
}

#[tokio::test]
async fn ids_are_distinct_across_requests() {
    // One generator serving both, as the kernel does.
    let generator = Arc::new(RequestIdMiddleware::new());
    let a = run(Arc::clone(&generator), Request::builder().build()).await;
    let b = run(Arc::clone(&generator), Request::builder().build()).await;

    assert_ne!(a.header("x-request-id"), b.header("x-request-id"));
}
```

Note `Arc::clone` rather than a fresh middleware per request. Building a new one
restarts its counter, and the test passes for the wrong reason — the kind of
mistake that only shows up as a duplicate id in production.

### A gate is a value

```rust
#[test]
fn an_undefined_ability_is_denied() {
    assert!(PostPolicy::gate().denies::<Post>("posts.teleport", &user(1), None));
}
```

No request, no container, no database. See
[Authorization](authorization.md#testing-a-policy).

### A mailable does no I/O

```rust
#[test]
fn the_welcome_email_greets_by_name() {
    let engine = MemoryEngine::new().with("mail.welcome", "Hi {{ name }}");
    let message = WelcomeEmail { name: "Ada".into(), email: "a@b.c".into() }
        .build(&engine)
        .unwrap();

    assert!(message.html.unwrap().contains("Hi Ada"));
}
```

## The database double

`MemoryConnection` records statements instead of running them:

```rust
use rainier_framework::database::testing::{fake_database, MemoryConnection};

let (db, connection) = fake_database(
    MemoryConnection::new(Dialect::Sqlite)
        .returning([OwnedRow::new().with("id", 1_u64).with("email", "a@b.c")])
        .with_outcome(1, 42)          // rows_affected, last_insert_id
        .sharded("users")
        .allocating(9001)
        .failing("connection refused"),
);
```

```rust
connection.recorded();          // Vec<RecordedStatement>
connection.statements();        // Vec<String>
connection.last_statement();    // Option<String>
connection.last_route();        // Option<ShardRoute>
connection.bindings();          // Vec<Vec<Value>>
connection.statement_count();
```

Which lets you assert on the SQL a repository generates:

```rust
let posts = EntityRepository::<Post>::new(db);
posts.first_by("slug", "hello".into()).await?;

let sql = connection.last_statement().unwrap();
assert!(sql.contains("posts"));
assert!(sql.contains("slug"));
```

`.failing(…)` is for the error path, which is the one nobody covers until it
happens in production.

### Assert on the clause, not the statement

A trap worth naming. This looks right and is wrong:

```rust
// WRONG — "password" is a legitimate SELECT column.
assert!(!sql.contains("password"));
```

Inspect the part you actually mean:

```rust
let where_clause = sql.split("WHERE").nth(1).unwrap_or("");
assert!(!where_clause.contains("password"));
```

### Bind counts include paging

`sea-query` parameterises `LIMIT` and `OFFSET`, so a paged query has **more**
bindings than it has filters. Count what is there rather than what you expect
from the `where` clauses alone.

## The other doubles

| Double | Records | Asserts with |
|---|---|---|
| [`Dispatcher::fake()`](events.md#testing) | events | `assert_dispatched`, `dispatched::<E>()` |
| [`QueueManager::fake()`](queues.md#testing) | dispatches | `assert_pushed`, `pushed::<J>()` |
| [`Mailer::fake(views)`](mail.md#testing) | messages | `assert_sent_to`, `sent()` |
| `MemoryQueue` | real queueing, in memory | `pending`, `failed_jobs` |
| `MemoryTransport` | messages | `sent`, `count` |
| [`MemoryChannel`](notifications.md#testing) | notifications | `sent_to`, `sent()` |
| [`MemoryBroadcaster`](broadcasting.md#testing) | broadcasts | `assert_broadcast`, `sent()` |
| [`Http::fake()`](http-client.md#the-fake-is-the-point) | outbound calls | `assert_sent`, `recorded()` |
| a [`Socket`](websockets.md#the-socket-handle) over a channel | socket sends | `rx.try_recv()` |
| `MemoryEngine` | — templates from strings | |
| `MemorySessionStore` | real sessions, in memory | |

Install one by rebinding — [every facade call re-resolves](facades.md#how-it-works),
so it takes effect immediately:

```rust
app.instance(QueueManager::fake());
Queue::instance().assert_pushed::<NotifyAuthor>();
```

## Factories

`TestApp` solved booting an application. Factories are the other half: getting
rows into it.

```rust
#[derive(Entity, Clone, Default, Factory)]
struct User { id: u64, email: String, verified_at: Option<DateTime<Utc>> }

let users = User::factory()
    .count(3)
    .sequence(|user, i| user.email = format!("user{i}@example.com"))
    .state(|user| user.verified_at = Some(Utc::now()))
    .create(&*users)
    .await?;
```

A factory builds a row that is **valid and uninteresting**. Every field the
test does not care about gets a value that will not trip a constraint, and the
one or two it *is* about are set with `state`.

That is the whole point. A test spelling out fifteen fields to assert on one
has buried its subject: a reader cannot tell which value matters, and neither
can the next person to change the schema.

| | |
|---|---|
| `count(n)` | how many |
| `state(f)` | adjust every one — the field the test is about |
| `sequence(f)` | adjust each, knowing which it is |
| `make()` / `make_one()` | build without a database |
| `create(&repo)` / `create_one(&repo)` | build and insert |

`create` returns what the repository returned, so a database-assigned key is on
the model the test goes on to use rather than the zero it was built with.

### Unique columns need a sequence

`#[derive(Factory)]` builds from `Default`, and three defaults are three
identical rows — which a `UNIQUE` index refuses on the second.

```rust
.sequence(|user, i| user.email = format!("user{i}@example.com"))
```

Deliberately not automatic. A factory that invented unique values would have to
guess which columns are unique and what shape they take, and a wrong guess
produces a row that fails to insert for a reason nobody can see from the test
that wrote it.

For a model with unique columns, writing the factory by hand is usually better
— it puts the sequence in one place instead of in every test:

```rust
impl HasFactory for User {
    fn factory() -> Factory<Self> {
        Factory::new(|i| User { id: 0, email: format!("user{i}@example.com"), verified_at: None })
    }
}
```

### One base, several narrower ones

```rust
let base = User::factory().count(2);

let admins = base.clone().state(|user| user.admin = true).make();
let ordinary = base.make();
```

States apply in the order added, so a later one overrides an earlier one.

`create` inserts **sequentially**. Concurrent inserts against one connection
interleave in whatever order the pool feels like, which makes a test asserting
on ordering fail once a fortnight.

## Cheap hashing in tests

```rust
Argon2Hasher::insecure_for_tests()
```

19 MiB and two iterations per hash is the point of Argon2, and it is also what
turns a suite that creates fifty users into a suite that takes a minute. See
[Hashing](hashing.md#tests-must-not-use-the-real-one).

## Building requests

```rust
Request::builder()
    .method(Method::POST)
    .uri("/api/posts")                       // ← do not forget this
    .header("authorization", &format!("Bearer {token}"))
    .json(&json!({ "title": "Hello" }))
    .build()
```

**`.uri()` defaults to `/`.** Forgetting it produces a `404` that looks
mysterious, and it is the single most common cause of a confusing failure in a
Rainier test suite — which is most of why `app.get("/api/posts")` exists.

Reading a `Response` outside the harness, where there is no `TestResponse`:

```rust
let body = response.into_string().await?;      // or into_bytes / into_json::<T>
```

`into_json` quotes the start of the body in its error, because a parse failure
in a test is nearly always an error response nobody expected — and `expected
value at line 1 column 1` says nothing about which one.

## Testing an outbound call

```rust
Http::fake().responding(200, r#"{"ok":true}"#);

publish_post(&post).await?;

Http::assert_sent(|request| request.url_contains("/hooks/post-published"));
```

The double most suites do not have, because without one the only way to assert
an outbound call is to stand up a server — so nobody does, and the code that
signs the webhook is the code nothing exercises.

It scopes to the calling thread, so these run in parallel like any other test.
See [HTTP Client](http-client.md#the-fake-is-the-point).

## Testing a console command

```rust
let console = Console::new("app").register(SeedCommand);
let code = console.run_argv(&app, ["app:seed", "--fresh"]).await;

assert_eq!(code, 0);
```

Assert on the **exit code and the effect**, not on stdout. See
[Console](console.md#testing-a-command).

## Testing a job

```rust
let context = Arc::new(JobContext::new(container, "id".into(), "mail".into(), 1, 3));
NotifyAuthor { post_id: 1 }.handle(&context).await?;
```

No queue and no worker — a job's `handle` is a method. Pass the attempt number
you want to exercise, which is how you test the `is_last_attempt` branch.

## What the framework tests itself

```sh
cargo test --workspace     # 2052 tests
cargo clippy --workspace --all-targets
```

Two of those are worth knowing about because they guard properties rather than
behaviour:

- `rainier-database` asserts at **compile time** that Rainier ORM's own futures
  are `Send`, so a regression upstream fails the build here rather than
  surfacing as a baffling error in an application. See
  [the `Send` story](database.md#the-send-story).
- `rainier-container` asserts that a dependency cycle **errors rather than
  deadlocking**, and that a panicking factory does not poison later
  resolutions. Both are failure modes with no diagnostic if you get them wrong.
