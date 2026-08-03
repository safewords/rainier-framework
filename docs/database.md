# Database: Getting Started

Rainier's data layer is built on **[Rainier ORM]**, a multi-dialect DBAL where
one `#[derive(Entity)]` serves SQLite, MySQL and Postgres — and Cloudflare D1,
which speaks SQLite over HTTP and therefore reports `Dialect::Sqlite`.

Rainier adds the four things a framework needs on top:

| | |
|---|---|
| `Connection` / `Database` | a `dyn`-safe port, so a backend can live in the container |
| [`Model`](models.md) | an entity the framework manages, with lifecycle hooks |
| [`Repository`](repositories.md) | a contract to depend on, implemented once for every model |
| [`Criteria`](repositories.md#criteria) / [`Paginated`](pagination.md) | composable scopes and paging |

## Connecting

```env
DATABASE_URL=sqlite::memory:
DATABASE_URL=sqlite://storage/app.sqlite
DATABASE_URL=mysql://user:pass@localhost/app
DATABASE_URL=postgres://user:pass@localhost/app
```

That is the whole of it: `Rainier::boot()` opens the connection, and binds it
both as a `Database` — which is what repositories, the `DB` facade and
`migrate` take — and as the default of a `DatabaseManager` with nothing else in
it. Leave `DATABASE_URL` unset and **no database is opened at all**, which is
what an application that has none should get; a seeded `sqlite::memory:` would
accept every statement, migrate cleanly and answer every question about the
application's own data with no rows.

To build the connection yourself — a test double, or a backend no
configuration file can describe — hand one over instead:

```rust
use rainier_orm::{PoolConfig, SeaOrmExecutor};
use rainier_framework::database::Database;

let executor = SeaOrmExecutor::connect(&url, &PoolConfig::default()).await?;

Rainier::new(".").with_database(Database::new(executor))
```

`with_database` wins over anything declared, the way `with_storage` wins over
declared disks: it sits in the builder chain a reviewer is already reading.

Nothing else in the application changes between those — that is the ORM's whole
premise.

### The in-memory SQLite trap

An in-memory SQLite database exists only as long as its connection, so a pool
of five would give you **five empty databases** and a test that fails
depending on which connection it drew:

```rust
let pool = if url.starts_with("sqlite::memory:") {
    PoolConfig::serverless()      // max = 1
} else {
    PoolConfig::default()
};
```

`Databases` below picks that pool for you — see
[what the section does not carry](#what-the-section-does-not-carry).

## More than one database

One `DATABASE_URL` is one database, which is right for nearly every
application. A read replica, a reporting warehouse or a second database another
system also writes to cannot be written down that way at all, so those are
declared as a **section**: a `default` naming one entry, and each entry naming
its own driver and settings.

```rust
use rainier_database::{Databases, ServerDatabase, SqliteDatabase};

let databases = Databases::new("primary")
    .with("primary", ServerDatabase::mysql("app").host("db.internal").credentials("app", secret))
    .with("replica", ServerDatabase::mysql("app").host("replica.internal").credentials("reader", secret))
    .with("reporting", SqliteDatabase::new("storage/reporting.sqlite"));

let manager = databases.build().await?;      // every connection opened once

manager.default_connection();                // the `primary` handle
manager.connection("replica");               // Some(&Database)
manager.connection("replicaa");              // None — never the default
```

Hand that to the builder and the framework opens every connection at boot:

```rust
Rainier::new(".").with_databases(databases)
```

which writes it to the `databases` key, so the same thing can come from the
configuration tree instead:

```json
{
  "databases": {
    "default": "primary",
    "connections": {
      "primary":   { "driver": "mysql", "host": "db.internal", "database": "app",
                     "username": "app", "password": "…" },
      "replica":   { "driver": "postgres", "url": "postgres://reader:…@replica.internal/app" },
      "reporting": { "driver": "sqlite", "database": "storage/reporting.sqlite" }
    }
  }
}
```

`DATABASE_URL` is the same section with one entry in it, and stays the way to
say so:

```rust
let manager = Databases::from_url(&url)?.build().await?;
```

### Never both

`DATABASE_URL` and a `databases` section each name the **default connection**,
so setting both is two answers to one question and fails the boot. There is no
precedence rule on purpose: whichever declaration lost would still be sitting in
the configuration being read by whoever changes it next, so repointing the
database by editing the visible one would review cleanly, deploy cleanly and
change nothing — and the query that then ran against the other database would
come back with rows rather than an error.

When the platform injects `DATABASE_URL` and you need a second connection, read
it while building rather than leaving both in force:

```rust
let builder = Rainier::new(".");
let url = builder.env().require("DATABASE_URL")?;

builder.with_databases(Databases::from_url(&url)?.with("replica", replica))
```

The driver comes from the DSN's scheme (`mysql`, `mariadb`, `postgres`,
`postgresql`, `sqlite`), and a scheme no driver speaks is a boot failure rather
than a guess.

### A name nobody declared is not the default

`manager.connection("replicaa")` is `None`, and `manager.resolve(Some(name))`
is an error listing what *is* declared. Neither falls back, because a query
against the wrong database does not raise — it **answers**. The rows come back,
the types match, the report renders, and nothing anywhere fails, because from
the database's point of view nothing went wrong.

### Two ways to write one connection, never both

A platform that injects one secret injects a **DSN**; a file written by hand
names **discrete fields**, because a host you can read is a host you can
review. Both are supported. Declaring both on one connection is refused rather
than resolved by precedence: the setting that loses is still sitting in the file
being read by whoever changes it next, so repointing a connection by editing its
visible `host` would review cleanly, deploy cleanly and change nothing.

The other refusals are on the same principle — a `host` on a `sqlite`
connection, a server connection with no `host` or no `database`, a `password`
with no `username`, a `default` naming an entry nobody declared. Each is a case
where the connection would work and read the wrong rows.

### What the section does not carry

**Pool settings.** The one case where getting it wrong is silent — an in-memory
SQLite database with more than one connection is more than one *database* — has
exactly one right answer, and `Databases` uses it. Sizing a pool is a tuning
decision with no wrong-data failure mode.

**The `d1` and `libsql` drivers.** Their executors take a caller-supplied
transport (a `fetch` binding in a Worker, an HTTP client on a server), which is
not a value a configuration tree can hold. Build one in code and register it:

```rust
manager.with_connection("edge", Database::new(D1Executor::new(transport)))
```

## Dialects

```rust
db.dialect();          // Dialect::Sqlite | MySql | Postgres
```

The dialect decides SQL rendering — quoting, `LIMIT`/`OFFSET`, upsert syntax,
DDL types. You rarely name it except in a
[migration step](migrations.md#a-step-per-dialect) that genuinely differs.

## Executors

`Connection` is the `dyn`-safe port. `Database` holds an `Arc<dyn Connection>`.

### Why the indirection exists

Rainier ORM's `Executor` uses `async fn` in trait and carries no `Send + Sync`
bound — deliberately, so the same code runs in a single-threaded Cloudflare
Worker over a `!Send` D1 binding. That makes it unusable as `dyn Executor`,
which is exactly what a container needs to store.

`Database` resolves it by holding `Arc<dyn Connection>` and **re-implementing
`Executor` on top of it**. The whole ORM surface therefore works against a
`Database` unchanged:

```rust
// Through the repository contract…
let page = posts.paginate_matching(Criteria::new().where_eq("published", true), 1, 20).await?;

// …or straight through Rainier ORM, because Database is an Executor.
let newest: Option<Post> = repo::query::<Post>().order_by_desc("id").first(&db).await?;
```

### Registering a backend

```rust
rainier_framework::bind_executor!(MyExecutor);
```

Two constraints, both worth knowing before you reach for it:

1. **The type must be concrete.** Rust cannot prove a generic `E: Executor`
   produces `Send` futures without return-type notation, which is unstable.
2. **You may only call it in the crate that defines the trait or the type.**
   The orphan rule. `Connection` belongs to `rainier-database` and
   `SeaOrmExecutor` to Rainier ORM, so **an application cannot bind
   `SeaOrmExecutor` itself** — Rainier ships that impl behind the
   `sea-orm-executor` feature.

```toml
rainier-framework = { git = "…", features = ["sea-orm-executor"] }
```

`bind_executor!` is for executors *you* wrote.

## Running SQL

Most code goes through a [repository](repositories.md). Two lower levels are
there when you need them.

### `database.query(sql)`

Raw SQL, for the query the criteria builder does not have a shape for: a
recursive CTE, a window function, a `LATERAL` join, an `EXPLAIN`, a
migration's one-off backfill.

```rust
let stale = database
    .query("DELETE FROM sessions WHERE last_seen_at < ?")
    .bind(cutoff)
    .execute()
    .await?;

let posts: Vec<Post> = database
    .query("SELECT * FROM posts WHERE author_id = ? ORDER BY published_at DESC")
    .bind(author_id)
    .fetch_all()
    .await?;

let total = database
    .query("SELECT SUM(weight) AS total FROM widgets")
    .scalar_i64("total")
    .await?;
```

| Terminal | Returns |
|---|---|
| `execute()` | `ExecOutcome` — `rows_affected`, `last_insert_id` |
| `fetch_all::<E>()` / `fetch_one::<E>()` | entities decoded by column name |
| `fetch(columns)` | `Vec<OwnedRow>`, for a shape no entity has |
| `scalar_i64(col)` / `scalar_string(col)` | one value from the first row |
| `column(col)` | one text column from every row |
| `prepared()` | the `Prepared` it would send, for a log or a test |

`scalar_i64` returns `Option`, and does not flatten `None` to `0`. `SUM` over
no rows is `NULL`, not zero, and rounding that to zero is how a total silently
becomes wrong.

#### Placeholders are always `?`

MySQL and SQLite spell a placeholder `?`; Postgres spells it `$1`, `$2`.
Writing the same statement twice for two dialects is the madness a DBAL exists
to absorb, so `?` is the spelling here and it is rewritten to `$n` for
Postgres, in order, skipping anything inside a string literal or a quoted
identifier.

The one case that bites is Postgres's JSON `?` operator (`data ? 'key'`) and
its `??`/`?|`/`?&` relatives — genuinely `?` characters that are not
placeholders. Reach for `.raw_placeholders()` there and write `$1` yourself.

#### On a shard

```rust
database
    .query("SELECT * FROM orders WHERE customer_id = ?")
    .bind(customer_id)
    .route_by(customer_id)
    .fetch_all::<Order>()
    .await?;
```

`route_by` takes the same values the ORM routes by — a shard-encoded id as-is,
a string key through the same stable hash — so the same key lands on the same
shard from any process. Without it a query is `ShardRoute::Global`, which on a
sharded deployment means it will look in the wrong place and quietly find
nothing. `on_shard_key(key)` takes an already-resolved key.

#### The one thing that is not safe

**Values** are always bound; a bound value can never be SQL, which is the whole
point of `bind`. The **statement** is whatever string you passed. Building that
string out of anything a request supplied is an injection, and no amount of
binding downstream repairs it:

```rust
// NEVER
database.query(&format!("SELECT * FROM {table} WHERE id = ?")).bind(id)
```

A table or column name that has to vary belongs in a `match` over a closed set.

### `Prepared`, by hand

```rust
db.statement("PRAGMA foreign_keys = ON").await?;      // no bindings, for DDL

let prepared = statement::select_by_column::<Post>(db.dialect(), "author_id", 7.into());
let posts: Vec<Post> = db.fetch_all(prepared).await?;
let count = db.fetch_count(prepared).await?;
let outcome = db.execute(prepared).await?;      // rows_affected, last_insert_id
```

`statement::` renders SQL **synchronously**, returning a `Prepared { sql,
params, route }`. That keeps shard routing and dialect rendering explicit at
the framework seam — see below.

## Sharding

Rainier ORM routes by shard when the backend does:

```rust
db.is_sharded();
db.shard_family();
```

A `Prepared` carries a `ShardRoute` computed from the value the entity is
sharded on. `Connection::allocate_id(shard_key)` mints a shard-encoded primary
key. Single-database backends answer `None` to all of it and nothing changes.

## The `Send` story

Building Rainier surfaced a real incompatibility, since **fixed upstream**. It
is worth understanding because the same trap catches any async Rust code
touching `sea-query`.

`sea_query`'s statement types hold `Rc<dyn Iden>`. A value merely *alive across
an `.await`* is captured by the generated future — so a statement in scope at an
await point makes the whole future `!Send`, and therefore unusable inside a
handler a multi-threaded server will `tokio::spawn`.

```mermaid
flowchart TD
    subgraph before ["Before — the future is !Send"]
        A1["build the statement"] --> A2["render it to SQL"]
        A2 --> A3["await the query"]
        A3 -.->|"statement still in scope"| A4["the future captures its Rc"]
    end

    subgraph after ["After — the future is Send"]
        B1["build and render<br/>inside an inner scope"] --> B2["the scope ends;<br/>the statement is dropped"]
        B2 --> B3["await the query"]
        B3 --> B4["the future holds only<br/>a String and a Vec of values"]
    end

    style A4 fill:#633,stroke:#a66,color:#fff
    style B4 fill:#353,stroke:#6a6,color:#fff
```

The fix has two halves:

- **`repo::`** builds and renders each statement inside a scope that ends before
  the await, so only the rendered `String` and `Vec<Value>` cross it.
- **`Query`'s terminals** are `fn … -> impl Future` rather than `async fn`.
  This one is subtle: **an `async fn`'s future captures every argument** whether
  or not the body moves it out first — and `self` is a `Query<E>`, which holds
  `Rc`. Consuming `self` outside the `async move` block leaves it out of the
  future.

`rainier-orm/tests/send_futures.rs` asserts the property at compile time for
every public async API, so a regression fails the build rather than surfacing as
a baffling error downstream. Rainier keeps its own live check in
`rainier-database`'s tests.

### Two things that follow

**A future's *output* need not be `Send` for the future to be `Send`.** The
value is moved out on the final poll rather than held across a suspension. That
is why `Connection::fetch_raw` returns `BoxFuture<'_, Result<Vec<Box<dyn Row>>>>`
even though `Box<dyn Row>` is not `Send` — and it is what lets Rainier ORM'
`repo::` API work from a request handler. The rows still cannot cross a thread,
so decode them before the next `await`, which is exactly what
`Entity::from_row` does.

**One documented exception remains.** `rainier_orm::Migrator::run` boxes each
step behind `dyn`, which erases auto traits, and the bound cannot be added:
`CreateTable` implements `Migration<X>` for *every* `X: Executor`, so it would
have to promise a `Send` future for every executor — unknowable generically
without return-type notation. Use [`rainier_database::Migrator`](migrations.md)
instead; it renders DDL synchronously and executes plain strings, so it is
`Send`.

### Why Rainier still renders synchronously

`rainier-database::statement` prepares SQL before any await. That began as a
workaround and is now a **design choice**: it keeps shard routing and dialect
rendering explicit at the framework seam, where you can see and test them.
`repo::` and the query builder work directly against a `Database` too, and a
test asserts it.

## Testing

`MemoryConnection` records statements instead of running them:

```rust
use rainier_framework::database::testing::{fake_database, MemoryConnection};

let (db, connection) = fake_database(
    MemoryConnection::new(Dialect::Sqlite)
        .returning([OwnedRow::new().with("id", 1_u64).with("email", "a@b.c")]),
);

let users = EntityRepository::<User>::new(db);
let found = users.first_by("email", "a@b.c".into()).await?;

assert!(connection.last_statement().unwrap().contains("users"));
assert_eq!(connection.statement_count(), 1);
```

See [Testing](testing.md#the-database-double) for the full API.

[Rainier ORM]: ../crates/rainier-orm
