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

`Databases` below picks that pool for you when you declare no `pool`, and
[refuses one](#sizing-the-pool) that would open a second connection to a
database that only exists inside the first.

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

### What a connection carries besides an address

Four of these decide what the connection *reaches*; two decide what happens to a
value once it gets there, and those two fail by **storing something other than
what was sent**.

```rust
ServerDatabase::mysql("app")
    .host("db.internal")
    .credentials("app", secret)
    .charset("utf8mb4")
    .collation("utf8mb4_unicode_ci")
    .strict(true)
    .tls_ca("/etc/ssl/rds-combined-ca-bundle.pem")
```

| Setting | What it is for |
|---|---|
| `charset` / `collation` | the character set the connection negotiates, and how it orders |
| `strict` | whether MySQL errors on an out-of-range value or truncates it |
| `unix_socket` | a socket on this machine instead of a host and port |
| `tls_ca` | a CA certificate to verify the server against; one setting both engines read |
| `option(k, v)` | a driver parameter this list has no field for — an allow-list, see [below](#what-the-section-does-not-carry) |

**`charset` is a data setting.** MySQL's `utf8` is three bytes wide. A
connection negotiating it does not reject an emoji, or a good deal of CJK text
— it truncates the value at the first four-byte character and stores the row.
The write succeeds and the text is short. `utf8mb4` is the one that holds all of
Unicode. Rainier still declares no default, because an assumed character set is
an assumption about every existing row in a database this connection did not
create; undeclared leaves the driver and the server to settle it exactly as they
did before.

**`strict` is the other one.** Non-strict MySQL truncates a value too long or
out of range for its column instead of erroring — the `INSERT` returns success
and the stored value is not the one that was sent. Left undeclared the server's
own `sql_mode` decides, which is not a safe default so much as an unknown one: a
managed database's parameter group is a place strict mode routinely gets turned
off. Declaring it settles it for every connection in the pool rather than for
whichever one happened to be checked out.

A setting the driver cannot honour is **refused when the connection is
declared** rather than accepted and dropped — `charset` on a driver with no such
setting, `collation` with no `charset` to order, `unix_socket` beside a `host`
nothing will dial.

### Splitting reads from writes

A connection may name a `read` role and a `write` role, each with its own hosts
and, if it needs them, its own credentials. Everything a role does not name it
takes from the connection around it, so the common case is short:

```json
{
  "primary": {
    "driver": "mysql", "host": "writer.internal", "database": "app",
    "username": "app", "password": "…",
    "read": { "host": ["replica-a.internal", "replica-b.internal"] },
    "sticky": true
  }
}
```

A connection naming neither role is one connection and behaves exactly as it did
before any of this existed — same endpoint, same pool, same connection string.

**Which endpoint a statement reaches is decided by the method that ran it**, not
by reading the SQL: a fetch reads and an execute writes. Every host of a role is
opened at boot and they are used in turn, round-robin.

#### `sticky`, and what it does outside a scope

Splitting reads onto a replica introduces exactly one failure, and it is the
quiet kind: a read issued straight after a write can land on a replica that has
not caught up and **answer**. Not an error — the row is simply not there yet.
The record that was just created 404s; the balance that was just debited reads
its old total. Nothing raises, so nothing is logged, and it arrives as "it saved
but it did not save" from somebody who could not reproduce it, because by the
second attempt the replica had caught up.

`sticky` closes it, and what it needs is a **scope**: a unit of work inside
which *this scope has already written* is worth remembering. Inside one, a write
pins the connection and every read after it goes to the endpoint that put the
row there.

```rust
use rainier_framework::database::with_sticky_scope;

with_sticky_scope(async move {
    let post = posts.create(post).await?;
    posts.find_or_fail(post.id).await          // the writer, because this scope wrote
}).await?;
```

Two things to know before declaring it, because both surprise people:

- **Outside a scope, a sticky connection reads from the writer.** There are only
  two things it could do, and sending the read to a replica is precisely the
  staleness `sticky` was declared to rule out. A read split that is not being
  used shows up as load on the primary and idle replicas — visible, measurable,
  fixable. The other answer shows up as a row that is not there. The first such
  read logs a warning naming this, once per connection per process.
- **Nothing enters a scope on your behalf yet.** `with_sticky_scope` is called
  by the caller; the framework does not wrap a request or a job in one. A
  `sticky` connection today therefore reads from its writer everywhere until you
  wrap your own units of work.

A connection is free to declare the split **without** `sticky`, which is a
deliberate statement that its reads tolerate lag. What is not on offer is the
promise plus the staleness.

### Sizing the pool

A connection may declare a `pool`, and so may either of its roles. Every field
is optional and an absent one keeps the value the connection would have had, so
a declaration says only what it is changing:

```json
{
  "primary": {
    "driver": "postgres", "host": "writer.internal", "database": "app",
    "pool": { "max_connections": 8, "acquire_timeout": 5 },
    "read": { "host": "replica.internal", "pool": { "max_connections": 20 } }
  }
}
```

Durations are whole seconds. `0` means *never* for the two that can be disabled
— `idle_timeout` and `max_lifetime` — and is refused for the two where it would
mean "give up instantly".

**The roles are sized separately because they are sized differently.** A primary
takes writes from every process and its connection budget is the scarce one;
replicas take the read traffic and there are usually several. A role that
declares no `pool` takes the connection's.

Three fail in a way that does not look like a pool problem:

- **`max_connections` is a share of a budget, not a limit on this process.** The
  database accepts some total number of connections and every app process opens
  up to its own maximum, so the number to write down is the database's budget
  divided by the process count — and on a split connection, divided again by the
  hosts in the role, because each host is its own pool. Too high does not show
  up as slowness: the processes that started first keep working and the next one
  to start is refused outright, which reads as a partial outage rather than as a
  setting.
- **`acquire_timeout` chooses which failure saturation produces.** Too short and
  requests fail while the database is healthy and merely busy. Too long and they
  queue past the point the caller gave up, so the pool spends its capacity on
  work nobody is waiting for — which keeps the queue full, and is how a brief
  spike becomes a sustained one.
- **`max_lifetime` is the guard against a connection that is not there.** A load
  balancer or a database that drops long-lived connections leaves the pool
  holding sockets that look open and fail on first use, so the failures land on
  whichever query happened to draw a dead one. Recycling on an age is what stops
  that presenting as intermittent errors nobody can reproduce.

There is deliberately **no preset to name**. `PoolConfig::serverless()` is
expressible field by field, and a preset *name* in a configuration file is a
value whose meaning moves when the library changes underneath it — where six
numbers are six things a review can check against the database in front of it.

The [in-memory SQLite](#the-in-memory-sqlite-trap) case is the one a pool
declaration cannot get wrong: the database *is* the connection, so a pool that
is not exactly one connection kept forever is refused. A second connection is a
second, empty database, and reaping the first drops the schema.

### What the section does not carry

**A table prefix.** Refused rather than accepted, because it cannot be applied
*everywhere* a table name is rendered. `Entity::table()` is a `&'static str`
with no connection in scope, a foreign key names its parent as a string, and a
migration step takes SQL already written. A prefix reaching the first of those
and not the rest is the worst outcome available: some statements hit prefixed
tables and some hit unprefixed ones, and a query against a table that exists but
is not the one holding the rows comes back **empty** rather than failing. If it
is wanted it belongs in the ORM, where every table name is rendered — a
`Database` *is* an `Executor`, so `repo::query::<E>()` renders `E::table()`
inside a crate that has never heard of this section.

**`engine`.** MySQL's table engine is a `CREATE TABLE` clause, and nothing
between a declaration and the schema builder carries it. Accepting it would put
a value in the file the database never hears, which is strictly worse than
refusing it.

**Anything `options` names that the driver would not read.** `options` is an
allow-list, not a passthrough, because of what a driver does with a parameter it
does not recognise: sqlx's MySQL URL parser ignores it outright and its
PostgreSQL one logs and moves on. Neither fails. A passthrough would let a file
say `sslmode=verify-full` under a spelling the driver does not read, and the
connection would be established unverified — with the setting sitting in the
file, reviewed, and doing nothing.

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
