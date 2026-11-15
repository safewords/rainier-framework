# Lookout

Lookout is Rainier's opt-in diagnostics suite. It records what each unit of
work did — a request, a job, a console command, a scheduled task — as a
**batch** of entries: the queries it ran, the cache it touched, the calls it
made out, the jobs it queued, the mail it sent, the failures it reported, and
how long each took. You open `/_lookout` and look.

It is built from what the framework already emits. With the `lookout` feature
every component opens the same spans the [`telemetry`](observability.md#passive-instrumentation)
feature gives it; Lookout reads them in-process through a `tracing` layer, and
listens on the event bus for what spans do not carry — failures, model changes,
job outcomes. No application code opens anything for it.

Run the example to see it end to end, offline:

```sh
cargo run -p rainier-framework --example lookout --features lookout
```

---

## Turning it on

```toml
[dependencies]
rainier-framework = { version = "2", features = ["lookout"] }
```

```env
LOOKOUT_ENABLED=true
```

That is all a deployment needs. `cargo rainier features` turns the feature on
for an environment that sets `LOOKOUT_ENABLED=true`, as it does `telemetry`
for `TELEMETRY_ENABLED=true`.

The feature compiles in the spans **without** the OTLP exporter. Lookout and
`telemetry` are independent: turn both on to keep Lookout and export the same
spans to a collector.

### The environment

Read at boot through the application's `Env` (so `.env` counts), with
`APP_ENV` taken from the configuration:

| Variable | Sets | Accepts | Default |
|---|---|---|---|
| `LOOKOUT_ENABLED` | the master switch | a boolean | `false` |
| `LOOKOUT_STORE` | where batches go | `memory`, `database`, `cache` | `memory` |
| `LOOKOUT_CONNECTION` | the database store's connection | a connection name | the default connection |
| `LOOKOUT_CACHE_STORE` | the cache store's store | a cache store name | the default store |
| `LOOKOUT_CAPACITY` | how many batches the cache store keeps | a positive integer | `1000` |
| `LOOKOUT_PATH` | the dashboard's path | a path starting `/` | `/_lookout` |
| `LOOKOUT_GUARD` | the guard that authenticates viewers | a guard name | the default guard |
| `LOOKOUT_REQUEST_SAMPLE_RATE` | the fraction of requests recorded | `0.0..=1.0` | `1.0` |
| `LOOKOUT_JOB_SAMPLE_RATE` | the fraction of jobs recorded | `0.0..=1.0` | `1.0` |
| `LOOKOUT_COMMAND_SAMPLE_RATE` | the fraction of commands and tasks recorded | `0.0..=1.0` | `1.0` |
| `LOOKOUT_SLOW_QUERY_MS` | a slow query | milliseconds | `100` |
| `LOOKOUT_SLOW_REQUEST_MS` | a slow request | milliseconds | `1000` |
| `LOOKOUT_LOG_LEVEL` | the lowest log level recorded | `off`, `error`, `warn`, `info`, `debug`, `trace` | `error` |
| `LOOKOUT_RETENTION_HOURS` | how long raw batches are kept | a positive integer | `24` |
| `LOOKOUT_AGGREGATE_RETENTION_DAYS` | how long aggregates are kept | a positive integer | `30` |
| `LOOKOUT_CAPTURE_REQUEST_HEADERS` | [opt-in capture](#privacy) | a boolean | `false` |
| `LOOKOUT_CAPTURE_REQUEST_BODY_ON_EXCEPTION` | | a boolean | `false` |
| `LOOKOUT_CAPTURE_CACHE_KEYS` | | a boolean | `false` |
| `LOOKOUT_CAPTURE_MAIL` | | a boolean | `false` |
| `LOOKOUT_CAPTURE_USER_ID` | | a boolean | `false` |
| `LOOKOUT_CAPTURE_CLIENT_ERRORS` | record a request's 4xx as an exception ([below](#client-errors-are-not-failures)) | a boolean | `false` |
| `LOOKOUT_CAPTURE_CODE_TRACES` | where a slow query or call came from ([below](#code-traces)) | a boolean | `true` |
| `LOOKOUT_INSIGHTS_ENABLED` | the dashboard's [insights and forecasts](#insights-and-forecasts) | a boolean | `true` |
| `LOOKOUT_INSIGHTS_WINDOW_MINUTES` | the window compared with the one before | `1..=10080` | `60` |
| `LOOKOUT_INSIGHTS_MIN_COUNT` | how many a key needs before a change in it is reported | a positive integer | `5` |
| `LOOKOUT_INSIGHTS_CHANGE_PERCENT` | the smallest change reported | a positive integer | `50` |
| `LOOKOUT_INSIGHTS_FORECAST_BUCKETS` | how many buckets a forecast reaches; `0` draws none | `0..=100` | `6` |
| `APP_ENV` | `local` when it is `local` | — | — |

Booleans accept `true`/`false`, `1`/`0`, `yes`/`no`, `on`/`off`; an empty
variable counts as unset. **Anything else fails the boot**, with the variable,
its value and what it accepts in the message — the same as a misspelled
`QUEUE_DRIVER`. A typo in a deployment should stop it, not record nothing.

### In code

The environment sets the scalars. Scope, per-watcher rules and the keep rule
are about the application's own routes, so they are code:

```rust
use rainier_framework::lookout::config::{Patterns, Scope, WatcherConfig};
use rainier_framework::lookout::EntryType;

Rainier::new(".")
    .with_lookout(|lookout| {
        lookout
            .scope(Scope {
                routes: Patterns { include: vec![], exclude: vec!["health".into()] },
                jobs: true,
                commands: true,
                scheduled_tasks: true,
                ..Scope::default()
            })
            .watch(EntryType::Cache, |w| WatcherConfig { sample_rate: 0.1, ..w })
            .disable(EntryType::View)
            .keep_batch_if(|batch| batch.has_exception || batch.duration_us > 250_000)
    })
```

`with_lookout` runs **after** the environment is read, so what it sets wins;
called more than once, the adjustments apply in order. It does not turn
Lookout on by itself — `LOOKOUT_ENABLED` does, or `.enabled(true)` in the
closure. The result is validated again, and a sample rate outside `0.0..=1.0`
fails the boot like a bad variable.

---

## What it records

### Scope and sampling

A batch is decided in two stages. When the work starts, the **route** (its
name, else its pattern, else the raw path) and whether jobs, commands and
tasks are watched at all decide whether to buffer anything — the cheap
rejection, made before the handler runs, so an excluded route's queries are
never buffered. When the work ends, the **guard** and **user** — known only
once authentication has run — decide whether to keep it.

Sampling is per kind of work (`request_sample_rate`, `job_sample_rate`,
`command_sample_rate`) and per watcher (`WatcherConfig::sample_rate`).
**Failures ignore both**: a failure with no batch around it gets a batch of
its own, so nothing reported is lost to scoping or sampling. A batch carrying
one of `scope.monitored_tags` is kept whatever the sampling — tag a batch from
code with `Lookout::tag(None, "watch:user-42")`.

The dashboard's own path is never recorded.

### What is kept

With no `keep_batch_if`, a batch is kept when `APP_ENV=local`, or — anywhere
else — when it has an exception, something slow, a failed job or a monitored
tag. Production keeps the interesting batches and lets the rest go.

**Kept or not, every batch in scope is counted.** The ones the rule lets go
are not stored, but their entries are rolled into the aggregates like any
other, so a route's count, throughput and p50 to p99 describe every request
to it — not only the slow and failed ones that were kept. (Before this, the
aggregates were rolled from kept batches only, which made every percentile
read high in production.) A batch out of scope is counted nowhere.

### Thresholds

Each watcher has a `slow_ms`, and per-pattern `slow_overrides` keyed by a glob
over the entry's family key (route name, job name, …). A slow entry is tagged
`slow` and gets a derived `slow` entry. Queries default to 100 ms; requests,
jobs and outbound HTTP calls to 1000 ms.

### N+1 detection

A run of at least `n_plus_one.min_repeats` (5) queries with the same sanitised
SQL in one batch, taking at least `n_plus_one.min_total_ms` (50 ms) together,
adds an `n_plus_one` entry naming the query.

### Client errors are not failures

The framework's reporter puts every error on the event bus, a `404` or a
`422` included, because some listener may want them. Lookout does not: a
request answered with a client error is recorded with its status, but it adds
no exception entry, does not flag the batch as failed, and does not open a
[case](#cases). `LOOKOUT_CAPTURE_CLIENT_ERRORS=true` records them. A client
error inside a job or a command — a `NotFound` for a row that should exist —
is always recorded: there it is the application's fault.

### Code traces

An exception carries its backtrace when the error captured one
(`RUST_BACKTRACE=1`; in a release build, server errors only — see
[errors](errors.md)), and its content gains `frames`: the application's own
frames — framework, runtime and standard-library frames left out — as
`{"function", "file", "line"}`, innermost first.

A slow query, cache call, outgoing request, mail, notification or view gets
`code.trace` in the same shape: the stack **as its span closes**. `on_close`
runs as the instrumented future is dropped, inside the poll of the code that
awaited it, so those frames are where the call came from. Only spans over
their slow threshold are traced, because a capture walks the stack; fast ones
cost nothing. `LOOKOUT_CAPTURE_CODE_TRACES=false` turns it off.

The request pays only for the walk, once per family — a request running the
same slow query five hundred times gets one trace, not five hundred walks —
and for at most `MAX_CODE_TRACES_PER_BATCH` (16) families per batch. Turning the addresses into function names, which costs several times
the walk, is the background writer's job, on a blocking thread, just before
the batch is stored.

Both need symbols. A stripped binary renders every frame as `<unknown>`, which
gives no frames; function names need the symbol table, and files and lines
need line tables (`[profile.release] debug = "line-tables-only"`).

### Entry types

`request`, `query`, `cache`, `http_client`, `job`, `mail`, `notification`,
`event`, `scheduled_task`, `command`, `exception`, `log`, `model`, `view`,
`broadcast`, `session`, `auth_attempt`, `n_plus_one`, `slow`, `custom`. The
span each comes from is catalogued in `rainier_lookout::layer`.

---

## Storage

| `LOOKOUT_STORE` | Where | For |
|---|---|---|
| `memory` | this process, the last 1000 batches | development, tests — per pod on Kubernetes |
| `database` | Lookout's own four tables, on the named connection | anything shared and durable |
| `cache` | a ring buffer of the last `LOOKOUT_CAPACITY` batches in the named cache store | Redis in production, with no migrations |

The framework builds the store at boot from what it has already bound — the
database manager, the cache manager — so `LOOKOUT_CONNECTION` and
`LOOKOUT_CACHE_STORE` name connections and stores the application declared.
Naming one that does not exist fails the boot.

### Migrations for the database store

Like `DatabaseQueue::migrations()` and `DatabaseSessionStore::migrations()`,
they are opt-in: merge them into the application's migrator.

```rust
app.instance(
    Migrator::new()
        .merge(app_migrations())
        .merge(rainier_framework::lookout::migrations()),
);
```

They create `lookout_batches`, `lookout_entries`, `lookout_batch_tags`,
`lookout_aggregates` and `lookout_cases`, named `rainier_lookout_0001…0005`,
and `rainier_lookout_0006_entries_family_index` adds an index on
`lookout_entries (family_hash, type, occurred_at_us)` for a family's
occurrences. Times are `BIGINT` microseconds, so the same tables work on
SQLite, D1, MySQL/MariaDB and Postgres.

A write is a handful of statements whatever it holds: a delete and a
multi-row insert per table, and one upsert per few hundred aggregate rows
that adds the counts and keeps the larger maximum in the same statement.

### The cache ring buffer

Every key is prefixed `lookout:`:

| Key | Holds |
|---|---|
| `index` | the kept batches' summaries, newest first, at most `capacity` |
| `batch:{id}` | one batch |
| `agg:{period}:{type}:{start}` | one aggregate bucket |
| `buckets:{period}:{type}:{day}` | which buckets one UTC day has |
| `buckets:days` | which of those day lists exist |
| `case:{id}` | one case |
| `cases:index` | every case's id, status, severity and last sighting |

Writers serialise on a cache lock, so pods flushing at once do not lose each
other's updates; readers take none. A write sends its commands concurrently —
the connection multiplexes — and under the lock rewrites the index, the
buckets its batches roll into, and the cases its exceptions touch with the
small case index. A day's bucket list changes only when a bucket first
appears. Every write still rewrites the index, so keep the capacity in the
low thousands. Listing entries of a type reads the batches holding it newest
first, a few at a time, and stops once no batch left could hold a newer one.

A store without an atomic `add` (Workers KV) cannot hold the lock — use one
writer, or another store.

**From an earlier layout.** Before this release every bucket was listed in one
`buckets` key and every case kept in one `cases` key, each read and written
whole on every write — with a month of per-minute buckets, tens of megabytes a
flush. The first write after an upgrade folds both into the keys above and
deletes them; until then the dashboard reads them as well. Nothing needs to
be run.

---

## Privacy

The defaults record what the framework's telemetry spans carry, and those
deliberately leave values out: a query's text with its bindings as `?`, a
cache operation without its key, a mail's transport without its recipients.
Nothing that identifies a person is stored unless asked for:

| `Capture` | What it adds |
|---|---|
| `request_headers` | request headers, with `authorization`, `cookie`, `set-cookie`, `proxy-authorization`, `x-xsrf-token`, `x-csrf-token` and `x-api-key` redacted |
| `request_body_on_exception` | the request body, only on a batch that has an exception, with `password`, `token`, `secret`, `api_key`, `card_number`, `cvc` and their kin redacted |
| `cache_keys` | cache keys |
| `mail` | subjects and recipients |
| `user_id` | the authenticated user's id — needed for user scoping and "top users" |

Add names to `redaction.headers` and `redaction.fields`; values longer than
`redaction.max_value_len` (2048 bytes) are truncated.

**Who a request was for** comes from the `Authenticated` marker the
`Authenticate` middleware leaves on the response: the guard always, the user
id only with `capture.user_id`.

---

## The dashboard

Mounted at `LOOKOUT_PATH` (`/_lookout`) **outside every middleware group** —
no session middleware, no CSRF check. The one route that changes anything,
clearing, is a `DELETE` that needs an `X-Lookout: 1` header besides the gate
(changing a [case](#cases) needs the same header):
a cross-site form cannot set one, and a cross-site `fetch` that does is
preflighted. Mounting it inside a group would also put that group's cookies
on a refused request, which is the next point.

### What it shows

- **Dashboard** — requests, throughput, error and job counts for the chosen
  range, graphs over time with a forecast, the insights, and the slowest and
  most frequent of each type.
- **Every row drills in.** A route, query, job, exception or outgoing call
  opens its own page — `/issues/{type}/{id}`, where the id is the family's
  [`family_id`](#the-json-api), sixteen hex digits rather than its SQL — with
  its series over time and the forecast, p50, p90, p95 and p99 over every
  occurrence in the range (from the aggregates' histograms; the occurrences
  stored, mostly the slow ones, only when the range predates histograms) and
  p90, p95 and p99 per bucket on the time chart, a duration histogram, every occurrence linking to its batch's
  waterfall, and the code trace or stack. A link of the earlier form,
  `/issues/{type}?key=…`, still opens.
- **The waterfall** draws each span once. A slow one carries a `slow` pill
  (and `N+1` or `failed` where it is that) on its own row, instead of a
  second row for the derived slow entry.
- **Requests** and **Jobs** — one page each, grouped by route and by job
  class. Totals at the top (count, throughput, average, maximum, time spent
  and p50/p90/p95/p99 over everything), then a row per route or job class
  with the same figures — count, per minute, average, p50, p90, p95, p99,
  maximum, total time, share and trend — and the latest batches below. The
  percentiles cover every request or run in the chosen range, not a sample:
  each aggregate bucket keeps a duration histogram (four slots per doubling,
  so a percentile is within about 9% of the true one, and never above the
  exact maximum), and histograms add across buckets and pods. Rows written
  before an upgrade have no histogram and show no percentiles until they age
  out. The time chart draws the average with p90, p95 and p99 per bucket, so
  a tail that grows while the average holds still shows up over time.
- **Cases** — see [below](#cases).
- **Search** — the header's search box looks through batches, entries and
  cases.

### Cases

Every exception's fingerprint — failure kind, exception kind, message with
digits as `N`, first frame of the application's own code — is a **case**. The
first occurrence opens it; every later one adds to its count, users, sources
(routes, jobs, commands) and recent batches, and the case outlives the raw
batches for the aggregate retention after it was last seen.

**Severity** starts from what failed: a panic is critical, a server error or a
job's final attempt high, an attempt that will be retried medium, a client
error low. It goes up a step at 100 occurrences or 10 affected users, and
again each time the case **regresses**. A severity set by hand stays.

**Status** is open, resolved or ignored. Resolving records when, with an
optional note; an occurrence after that reopens the case as a regression. An
ignored case keeps counting and stays ignored. Changing a case is a `POST`
that needs `X-Lookout: 1`, like clearing.

The cache store keeps at most 1000 cases (open ones and the most recently
seen first), one key each, with a small index a listing filters and orders
before reading only the cases on the page. The database store keeps them in `lookout_cases`, migration
`rainier_lookout_0005_cases`; two pods folding the same case in the same
instant can lose one occurrence's count, never the case.

### Insights and forecasts

The dashboard compares the last window (`LOOKOUT_INSIGHTS_WINDOW_MINUTES`)
with the one before it, per type and per key, and reports what is **new**,
what is **rising** or **falling** in count, what is **slower** or **faster** on
average — each when the change is at least `LOOKOUT_INSIGHTS_CHANGE_PERCENT`
and the key had at least `LOOKOUT_INSIGHTS_MIN_COUNT` — and where each type is
**projected** to go over the next window. More exceptions and slower work are
bad; fewer exceptions and faster work are good.

A forecast is a least-squares line through the series' complete buckets (the
still-filling one is left out, so a half-counted minute does not read as a
fall), never below zero. It is a trend, not a model. The thresholds are the
deployment's defaults; a viewer can change them for their own view, and the
API takes them as `window`, `min`, `change` and `forecast`.

### Who may see it

| Where | Who |
|---|---|
| `APP_ENV=local` (with `dashboard.open_locally`, the default) | everyone |
| anywhere else | a viewer the dashboard's guard authenticates **and** the `Gate` lets through `dashboard.ability` — `viewLookout` by default |

Anyone else gets **the router's own 404**, on every path and method under the
dashboard — the same bytes as a path that never existed — so a probe cannot
tell Lookout is there.

The guard and the gate are generic over the application's user type, which
the framework cannot name, so the application says it once and defines the
ability on its gate:

```rust
Rainier::new(".")
    .with_lookout_viewers::<User>()
    .with_instance_arc(auth)          // AuthManager<User>
    .with_instance(
        Gate::<User>::new().define_simple("viewLookout", |user: &User| user.is_admin),
    )
```

The check resolves `AuthManager<User>` and `Gate<User>` from the container on
each request, authenticates with `LOOKOUT_GUARD` (or the manager's default),
and asks the gate. It reads the session itself, read-only, so a session guard
works on a dashboard mounted outside the `web` group. **It denies** without
`with_lookout_viewers`, without either binding, with an unknown guard, and
without the ability defined — an undefined ability is denied by the gate
itself.

When "may see it" is not a property of the user value — roles carried as
claims in the session, or a header from a proxy in front — name a predicate
instead of a gate ability:

```rust
Rainier::new(".")
    .with_lookout_viewers_where(|_user: &User, request: &Request| is_admin(request))
    .with_instance_arc(auth)          // AuthManager<User>
```

The guard still authenticates first, so an anonymous request never reaches
the predicate; the predicate sees the request with its session loaded
(read-only). No `Gate<User>` is needed, and the dashboard's `ability` is not
consulted.

When the answer takes a query — membership of an administrators group kept
in a table — use the async form. It gets the same two references and returns
a `'static` future, so copy what the query needs out first:

```rust
Rainier::new(".")
    .with_lookout_viewers_where_async(|user: &User, _request: &Request| {
        let id = user.id;
        Box::pin(async move { groups().is_administrator(id).await.unwrap_or(false) })
    })
```

Answer `false` on an error: a check that cannot be made refuses the viewer.

`LookoutConfig::local` decides "local", not the raw `APP_ENV`: an application
can turn it off in a local environment with `.local(false)`.

### The JSON API

Under `{path}/api`, behind the same gate. Lists answer
`{"data": [...], "next": {"before": "<RFC 3339>"} | null}`; single objects
`{"data": {...}}`. Bad parameters are a `422` in the framework's validation
shape.

| Method | URI | What |
|---|---|---|
| `GET` | `/api/batches` | batch summaries, newest first — `kind`, `type`, `route`, `user`, `tag`, `trace`, `exception`, `slow`, `q` (text in the summary, route, user or ids), `before`, `limit` (≤ 200, default 50) |
| `DELETE` | `/api/batches` | delete everything — needs `X-Lookout: 1` |
| `GET` | `/api/batches/{id}` | one batch, with its entries |
| `GET` | `/api/entries/{type}` | entries of one type across batches — `key` (one family), `q` (text in the summary or key; reads at most five pages), `before`, `limit` |
| `GET` | `/api/families/{type}/{id}` | the key a family id names — `{"id", "key"}`, or `404`. The id is `family_id(type, key)`: FNV-1a over the type, a newline and the key, sixteen hex digits, so the dashboard can make one without asking |
| `GET` | `/api/aggregates/{type}` | rolled-up counts and durations, with `average_us`, `p50_us`, `p90_us`, `p95_us`, `p99_us`, the `histogram` (`{slot: count}`) and the row's `family_id` — `period` (60, 600, 3600), `since`, `order` (`count`, `average`, `max`) |
| `GET` | `/api/series/{type}` | one zero-filled point per bucket from `since` to now, each with `count`, `average_us`, `max_us` and, where the bucket kept a histogram, `p50_us`, `p90_us`, `p95_us` and `p99_us`; `percentiles` holds the same four over the whole range — `period` (default: a dozen points or more), `key`, `keys` (that many busiest keys' own series), `forecast` (buckets) |
| `GET` | `/api/insights` | what changed since the previous window — `window`, `min`, `change`, `forecast`, `limit` |
| `GET` | `/api/cases` | cases — `status` (`open`, `resolved`, `ignored`, `all`), `severity` (minimum), `q`, `limit` |
| `GET` | `/api/cases/{id}` | one case |
| `POST` | `/api/cases/{id}` | `{"action": "resolve" \| "reopen" \| "ignore" \| "severity", "severity"?, "note"?}` — needs `X-Lookout: 1` |
| `GET` | `/api/status` | the store, watchers, sample rates, retention and insight thresholds |

The page itself is a prebuilt Vue app served from the binary, with a strict
Content-Security-Policy, `X-Frame-Options: DENY` and `Cache-Control:
no-store`.

---

## Commands and pruning

`rainier_framework::console(...)` registers three commands when the feature is
on. Each resolves the booted Lookout when it runs, and fails with a message
when Lookout is not enabled.

| Command | What |
|---|---|
| `lookout:prune [--hours=N] [--days=N]` | delete batches and aggregates past retention |
| `lookout:clear [--force]` | delete everything; outside `local` it asks, or needs `--force` |
| `lookout:status` | the store, watchers, sampling, retention and how much is stored |

Nothing prunes on its own — the framework does not schedule tasks for you.
Schedule it:

```rust
schedule.command(console.clone(), "lookout:prune").daily();
```

It is idempotent and deletes in bounded chunks, so a run that overlaps the
next is harmless. The cache store also expires its keys at retention; the
database store needs the prune.

---

## Overhead

- **Off, it costs nothing at run time**: a disabled Lookout binds nothing,
  mounts nothing and adds no layer or middleware. Without the feature none of
  it is compiled.
- **On, nothing waits on it.** The request path appends entries to an
  in-memory buffer. When the request or job ends, its batch is handed, as it
  is, to a **bounded channel**
  (`ingest.channel_capacity`, 10 000) with `try_send`; when the channel is
  full the batch is **dropped and counted** — `Lookout::dropped()` — and the
  request is never held up.
- **A background writer** drains the channel every `ingest.flush_interval_ms`
  (1 s) or as soon as `ingest.flush_batch_size` (500) batches are waiting.
  Everything after the hand-off is its work, after the response has gone:
  the keep rule, slow and N+1 detection, sorting, summaries, rolling every
  batch into the aggregates, symbolising code traces, and the store write.
  Every finished request goes through the channel now, kept or not, because
  every one is counted; one that would overflow it is dropped from the
  counts too, and counted in `Lookout::dropped()`. The counts of batches not
  kept are added up in the writer and written every 10 seconds (or with a
  kept batch, or on flush and shutdown), so a busy service rewrites its
  aggregates a few times a minute rather than every second. Its
  own store traffic runs inside a span marked `rainier.lookout.internal`, so
  the database or cache store's queries are not recorded. A failing store is
  logged at most once per interval, never reported on the bus.
- A batch holds at most 2000 entries; the rest are counted in a
  `truncated:<n>` tag.
- **What a request pays** on its own thread, measured by the
  `hot_path_cost` test in `crates/rainier-lookout/tests` (release build, Apple
  M-series, a request of twenty queries and three cache calls): about 17 µs
  with Lookout on, kept or not — the span bookkeeping, under a microsecond a
  span — and 1.4 µs with the feature built but `LOOKOUT_ENABLED=false`. A slow
  query adds a stack walk for its code trace (above), about 10 µs, once per
  query shape: twenty slow queries of five shapes cost about 73 µs.
  `cache_store_cost` in the same directory measures what the cache store
  costs the cache per write and per dashboard read.
- **Spans below `RUST_LOG`.** Lookout needs `info` spans whatever the log
  filter says, so with Lookout on the framework's `RUST_LOG` filter is a
  per-layer filter on the log lines (and the exporter) rather than a global
  one — see [below](#the-subscriber). Spans nobody else wants are then
  created for Lookout alone. That is the cost of recording them.

### Process lifetime

Boot starts the writer and keeps it in the container. `serve`, `queue:work`
and `schedule:run` (and `schedule:work --once`) flush as they finish. A
command's own batch closes after the command returns, so a `main` that wants
it written calls `shutdown` last:

```rust
let code = rainier_framework::console("app").run_from_env(&app).await;
rainier_framework::lookout::shutdown(&app).await;
std::process::exit(code);
```

`lookout::flush(&app)` writes what has closed so far without stopping
anything.

---

## The subscriber

At boot the framework builds Lookout **before** installing its tracing
subscriber, and installs:

```text
registry
  ├─ the log lines   .with_filter(EnvFilter(RUST_LOG))
  ├─ the exporter    .with_filter(EnvFilter(RUST_LOG))     (telemetry, with an endpoint)
  └─ Lookout's layer                                       (unfiltered)
```

A global `EnvFilter` decides for every layer at once: with `RUST_LOG=warn` the
`info`-level request and `db.query` spans would never exist, and Lookout would
record nothing. Per layer, the log lines and the exporter see exactly what
they did before. `TelemetrySettings::subscriber` builds this without
installing it.

### Lookout and OpenTelemetry

They read the same spans and do not depend on each other. `telemetry` exports
them to a collector for cross-service traces, sampling at the edge and
long-term storage; Lookout keeps them in the application for a per-request
view with queries, N+1s and failures side by side, with no collector to run.
A batch carries the request's trace id (`trace_id`, and the API's `?trace=`
filter), so a batch found in Lookout can be looked up in the tracing backend,
and the other way round.

### A subscriber of your own

The framework never replaces a subscriber the application installed. If one
was already set, or the application boots `without_tracing()`, boot logs a
warning that Lookout's layer is not attached, and the application adds it:

```rust
use tracing_subscriber::prelude::*;

let app = Rainier::new(".").without_tracing().boot().await?;
let lookout = app.resolve::<rainier_framework::lookout::Lookout>()?;
tracing_subscriber::registry()
    .with(tracing_subscriber::fmt::layer().with_filter(EnvFilter::from_default_env()))
    .with(lookout.layer())
    .init();
```

Keep the filter on your own layers, not on the registry, for the reason
above. No request, job or command runs before `boot` returns, so installing
the subscriber after it misses nothing worth recording.

In a test, set it for the thread: `tracing::subscriber::set_default(
tracing_subscriber::registry().with(lookout.layer()))`.

---

## Limitations

- **Spans outside a unit of work are not recorded** — a query at boot, a
  cache read in a background task. Only failures get a batch of their own.
- **Middleware groups are not part of the scope.** Nothing on a request or
  response says which group its route was registered in.
- **The memory store is per process.** On several replicas each pod has its
  own; use `database` or `cache` to see them together.
- **The cache store's readers take no lock**, and stores without an atomic
  `add` cannot serialise writers; see [above](#the-cache-ring-buffer).
- **The database store has no transactions** (the connection port offers
  none): a write that fails half-way can leave a batch missing entries until
  it is written again.
- **A command's own batch** is only written if the process calls
  `lookout::shutdown` (or `flush`) after the console returns.
- **`schedule:work` runs until killed**; its batches are written by the
  writer's one-second flush, and the last second's can be lost on `SIGKILL`.
