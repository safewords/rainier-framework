# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Every crate in the workspace shares one version, so an entry here applies to
the release as a whole and names the crate it landed in.

## [Unreleased]

### Added

- **`QueryCache`** (`rainier-framework`): a process-local cache of computed
  results — a lookup table, an aggregate, a small set of ids — invalidated by
  writes to the tables they were read from, on every replica. Entries are
  stamped with each table's generation before the load reads; a builder or
  `repo` write advances the generation before it returns, here and through a
  shared counter the other processes read back within a second. `watching`
  names the tables at boot, so a process that never serves the read still
  announces its writes. Bounded, single-flight per key, never fails a read. See
  [cache](docs/cache.md#computed-results-querycache).
- **Idle backoff for workers** (`rainier-queue`, `rainier-framework`):
  `WorkerOptions::idle_backoff(max)` and `queue:work --max-sleep=N` double
  the sleep after each empty poll in a row, up to `max`, and reset on the
  first job. Against a database-backed queue every poll is a query; a quiet
  worker no longer runs one a second forever. Off by default, because it lets
  a job that arrives after a quiet spell wait up to `max` to start.
- **Transactions** (`rainier-database`, `rainier-drivers`):
  `Database::transaction(|tx| …)` commits on `Ok` and rolls back on `Err` or a
  panic; `tx` is an ordinary handle, so repositories and `repo::` run inside.
  Change notifications are held until the commit and dropped on rollback, so
  `ModelCache` and `QueryCache` never act on a write that was undone. Nested
  calls join the outer transaction. `transaction_retrying(n, …)` re-runs on a
  deadlock, serialization failure or Galera certification conflict. Backends
  opt in through `Connection::begin` (`bind_executor!(T, begin = f)`); the
  sea-orm executor does, through `SeaOrmExecutor::begin` and
  `SeaOrmTransaction`. See [database](docs/database.md#transactions).
- **`vec_distance_euclidean`** (`rainier-database`): Euclidean distance —
  `VEC_DISTANCE_EUCLIDEAN` on MariaDB, `<->` on pgvector, `vec_distance_l2` on
  sqlite-vec — the distance a MariaDB vector index uses unless declared
  otherwise. A cosine query against such an index scans every row without
  saying so; the docs now say which to use.
- **`words(columns, terms)`** (`rainier-database`): a word search a `FULLTEXT`
  index answers — `MATCH … AGAINST (… IN BOOLEAN MODE)` on MySQL and MariaDB,
  each term required and prefix-matched, operators stripped from what was typed;
  a `contains` per term elsewhere. See
  [repositories](docs/repositories.md#expressions).
- **`Cache::touch`** (`rainier-cache`): move a key's expiry without rewriting it.
  One `PEXPIRE` on Redis, in place in memory, forwarded by the prefixed and
  traced wrappers; other stores read and rewrite.
- **`test_after_idle`** on `PoolConfig` and in a connection's `pool` declaration
  (`rainier-orm`, `rainier-database`, `rainier-drivers`): the pre-acquire ping
  runs only for a connection idle at least this long. Default one second;
  `0` pings every acquire, as `PoolConfig::serverless()` does.

### Changed

- **A query no longer pays a ping** (`rainier-drivers`): with the new
  `test_after_idle` default, a connection used within the last second is handed
  out without one, halving the round trips of a plain read. A plain `SELECT`
  that draws a connection the server has closed is retried on another, up to the
  pool's size, so a database restart is ridden through even when every pooled
  connection died together. Writes are never retried.
- **An unchanged session is touched once an interval, not every request**
  (`rainier-session`): `StartSession` skips the touch for a session it extended
  within a hundredth of the lifetime (at most a minute); the cookie is still
  renewed every response. `touch_every_request()` restores the old behaviour.
  The cache session store now touches with `Cache::touch` — one `PEXPIRE` instead
  of a read and a rewrite, and a logged-out session can no longer be put back by
  it.
- **Routing is indexed** (`rainier-routing`): routes are found through a trie of
  their leading literal segments instead of trying every route in turn, and a
  path is split once per request rather than once per route. Declaration order
  still decides the match. Against a 238-route table, dispatch went from 10–20 µs
  to about 1 µs.
- **Accepted connections set `TCP_NODELAY`** (`rainier-server`), so a response
  written in more than one piece is not held back by Nagle's algorithm.

- **Dependencies are current** (2026-11-18): `jsonwebtoken` 12, `rsa` 0.10 on
  the same `digest` 0.11 generation as `sha2` and `hmac` (so `rainier-crypt`,
  `rainier-mfa` and `rainier-oauth-server` hash with the workspace `sha2`
  rather than rsa's own), and `kube` 5.1 with `k8s-openapi` 0.29 under
  `rainier-queue`'s `kubernetes` feature. Every other dependency is at its
  newest compatible release.
- **A Lookout session is the browser's** (`rainier-lookout`,
  `rainier-framework`), breaking: one tab, renewed after 30 minutes without
  the visitor doing anything — measured from their activity, not only from a
  reload, so a tab left open is not one session for days. An application no
  longer defines what a session is: `Rainier::with_lookout_sessions` and
  `Lookout::sessions_with` are gone, and so is the `lookout-session` metric.
  It labels the session instead, with **`Lookout::describe_session(batch,
  name, value)`** or **`rainier_framework::lookout::describe_session(name,
  value)`** — `("profile", "ada")` — from where the request already knows:
  the request is tagged `profile:ada`, and the label rides back to the
  browser signed (`lookout-context`), so the page views, API calls and
  replays the session reports are tagged too. A browser cannot label itself.

### Fixed

- **Session replay** (`rainier-lookout`): each recorded piece starts at the
  page's size, not one event later — the recorder split a snapshot's two
  events into two pieces and dropped the first, so most pieces had no size.
  The player plays a session's pieces back to back, a second apart, instead
  of across the hours between them, and draws a white, outlined pointer that
  shows on a dark page; rrweb's own is black.

### Added

- **Request percentiles on Lookout's dashboard** (`rainier-lookout`): the
  main Requests chart draws p90, p95 and p99 per bucket beside the average
  request time.
- **`repo::insert_many`** (`rainier-orm`): many rows of one entity in one
  `INSERT … VALUES (…), (…)`, chunked under the placeholder cap and reported
  as a write. An empty slice writes nothing. An auto-increment key stays the
  database's, and a shard key is not minted — a row that needs one still goes
  through `repo::insert`.
- **`SharedRedis::query_pipeline`** (`rainier-drivers`): run several commands
  on one connection and decode the replies together. On a cluster they have
  to share a hash slot; a second slot is an error rather than a split.

- **Lookout percentiles over the whole range** (`rainier-lookout`): every
  aggregate bucket keeps a duration histogram, four slots per doubling, so
  `/api/aggregates` returns `p50_us`, `p90_us`, `p95_us` and `p99_us` per key
  over every occurrence in the range, to within about 9%. The database store
  keeps the slots in a new `lookout_aggregate_histograms` table (migration
  `rainier_lookout_0007_aggregate_histograms`), incremented atomically like
  the counts; the memory and cache stores keep them in the aggregate.
- **Lookout's Slow page filters by type.** The list mixes queries, outgoing
  calls and the rest of what ran slow; a Type menu shows one kind on its own.
- **Lookout Requests and Jobs pages**: one page each, grouped by route and by
  job class, with totals and p50/p90/p95/p99 over everything at the top, the
  same figures per route or job class in the table, and the latest batches
  below. They replace the separate "Jobs (timing)" page and the Requests and
  Jobs batch tabs in the navigation; the filtered batch list is a link away.
- **Lookout streams "Live"** (`rainier-lookout`): with the switch on, new
  batches arrive as server-sent events from `/api/stream` about a second after
  they are stored — on any replica, since one task per process tails the
  store for every viewer and runs only while someone watches — and pages update
  on them instead of re-asking every few seconds. The switch is remembered per
  browser, so a viewer who left it on is subscribed again on their next visit.
  `LOOKOUT_STREAM=false` falls back to polling, as does a browser whose stream
  cannot connect.

  The stream also sends a **`pulse`**: the newest request, exception and job
  count buckets, at every rolled-up period, whenever they move (at most every
  two seconds). The first version announced only kept batches, so on a
  healthy service — where nearly every request is counted and not kept — the
  dashboard's totals stood still while the stream was connected. The
  dashboard now folds each pulse into its totals and charts in place, and
  every page that shows data (cases, issues, timing, status) refreshes on the
  news it shows, plus once a minute. The writer stores its counts every 2
  seconds instead of 10, and only when something was counted.
- **The dashboard names what it shows** (`rainier-lookout`): a request's
  page says Request and lives at `/requests/<id>` — likewise jobs, commands,
  tasks, page views and API calls — instead of calling everything a batch,
  which is the storage word. `/batches/<id>` still opens any of them.
- **Pairing, sessions and whole-page replay** (`rainier-lookout`,
  `rainier-framework`):
  - A request from a listed frontend that would be dropped is held for
    `LOOKOUT_RUM_HOLD_SECONDS` (120) in the store; a browser report about it
    keeps it, tagged `rum:paired` and uncounted the second time, so an API
    call's page links to its server request and the two times compare. New
    `Store::hold`, `release` and `store_uncounted`, implemented by the memory
    and cache stores.
  - Sessions: every page view, API call, paired request and replay carries
    `session:<id>`, and **Browser → Sessions** lays a session out as one
    timeline with a stitched replay. (Since changed: a session is always the
    browser's; see above.) The user is attributed by a signed `lookout-user`
    id.
  - `LOOKOUT_RUM_REPLAY_MODE=always` records whole page views, uploading each
    stretch as it closes; page views and sessions show their replay.
- **Page views, apart from API calls** (`rainier-lookout`): with RUM on,
  the browser script also reports what the page itself did — its load,
  vitals, route changes inside a single-page app, script errors and long
  tasks — as a new `page` batch kind (tag `rum:page`), listed under
  **Browser → Page views**, separate from the API calls it made (`browser`,
  tag `rum:api`, summaries now `api …`) under **Browser → API calls**. Both
  carry `pageview:<id>`, so a page view lists its calls and a call links to
  its page view. A frontend on another origin proves its page talked to the
  API with a token from one of its responses.
- **Session replay** (`rainier-lookout`), off by default:
  `LOOKOUT_RUM_REPLAY=true` lets a page record itself with rrweb, masked in
  the browser — text as asterisks, no inputs, media as blank boxes,
  `data-lookout-block` left out — keeping the last ~30 seconds in memory and
  uploading only when an API request fails or is slow or the page throws.
  Consent is required by default (`LookoutRum.allowReplay()`), pages can be
  excluded (`LOOKOUT_RUM_REPLAY_EXCEPT`), and a recording is gzipped and
  capped (`LOOKOUT_RUM_REPLAY_MAX_BYTES`). The dashboard plays it back on the
  request's page; its CSP now allows inline styles, which the replayed page
  needs, while still loading nothing from other hosts.
- **Browser reports know the server's side** (`rainier-lookout`): responses'
  `Server-Timing` carries the server's own time (`dur`) and the route
  (`lookout-route`), so a report splits the wait and links to its route even
  when the request's own batch was not kept.
- **Who and what made a request, and how its browser saw it** (`rainier-lookout`,
  `rainier-framework`), each off by default:
  - `LOOKOUT_CAPTURE_USER`: the application describes the user with
    `lookout::describe_user(LookoutUser::new(id).name(..).email(..).with(..))`
    where it authenticates; the request's page shows a **User** card.
  - `LOOKOUT_CAPTURE_CLIENT`: the client's address (its network, unless
    `LOOKOUT_CAPTURE_CLIENT_IP=full`), browser, system and device from the
    user agent and client hints, origin, page and language — from headers,
    parsed on the writer, with no frontend changes.
  - `LOOKOUT_RUM`: real user monitoring for a frontend on another origin.
    Responses carry a signed `Server-Timing` token and `Timing-Allow-Origin`
    for `LOOKOUT_RUM_ORIGINS`; the page loads `{path}/rum.js` from the API and
    beacons each request's phases, device, connection and page vitals to
    `{path}/rum`, which stores them as a new `browser` batch kind on the
    request's trace. Reports are accepted only from listed origins, bounded,
    rate-limited, and only for requests this application signed.
- **Lookout's duration chart is a distribution curve** (`rainier-lookout`): an
  issue page's bar histogram is now a smooth density over a log time axis
  with the percentiles marked, drawn from every occurrence in the range —
  `/api/series` returns the range's merged `histogram` — instead of from the
  stored occurrences, which in production are mostly the slow ones.
- **`docs/_sidebar.json`**: the website's table of contents, kept with the
  docs it orders, and a test (`docs_sidebar.rs`) that every page is in it
  once and every entry is a page. rainierframework.io builds its docs from
  `docs/` on `main`; `AGENTS.md` says what that means for a change.
- **Lookout keeps request and response bodies, redacted** (`rainier-lookout`):
  `LOOKOUT_CAPTURE_REQUEST_BODY` and `LOOKOUT_CAPTURE_RESPONSE_BODY` keep
  every kept request's bodies (up to `LOOKOUT_CAPTURE_MAX_BODY_BYTES`, 64
  KiB), shown under **Payload** on the batch page with the request headers.
  `LOOKOUT_CAPTURE_BODIES_EXCEPT` names routes (globs over the route name or
  pattern) whose bodies are never kept — private messages, identity
  documents — whatever else is on.
  The request holds a slice of its own buffer; parsing and redaction happen on
  the writer, for kept batches only.
- **Redaction hides more, and can be told what to hide** (`rainier-lookout`,
  `rainier-orm`, `rainier-validation`, `rainier-support`): a field is hidden
  when its name contains `password`, `secret`, `token`, `signature` and the
  like (`redaction.field_patterns`), and when the application marks it —
  `#[orm(sensitive)]` on a model field, `FormRequest::sensitive()`, or
  `rainier_support::sensitive::mark`. Any other value that looks like a key or
  token (long, one word, a key's alphabet, high entropy) is starred out the
  same way every time, keeping its first four characters and length
  (`LOOKOUT_MASK_SECRETS`, on by default).
- **Independent queries at once** (`rainier-database`, in the prelude):
  `concurrently!(a, b, …)` runs a handful of queries at the same time, each on
  its own pooled connection, and returns their values in order or the first
  error; `all_concurrently(iter)` does a list, at most four at a time, in order
  (`all_concurrently_limited(n, iter)` to choose). Against MariaDB, four
  `SLEEP(0.2)` statements take 221 ms this way rather than 820 ms. See
  `docs/database.md`, "Independent queries at once".
- **Percentiles for each route and job class on its own page**
  (`rainier-lookout`): p50, p90, p95 and p99 over every run in the range, and
  p90, p95 and p99 over time beside the average. `/api/series/{type}` carries
  `percentiles` over the whole range. The page used to work out p50, p95
  and p99 from the stored runs, which in production are mostly the slow ones.
- **Percentiles over time** (`rainier-lookout`): each point of
  `/api/series/{type}` carries `p50_us`, `p90_us`, `p95_us` and `p99_us` from
  its bucket's histogram (across every key in it), and the Requests and Jobs
  time charts draw p90, p95 and p99 beside the average.
- **Short, stable ids for every family** (`rainier-lookout`): `family_id`,
  sixteen hex digits, on aggregate rows and in the dashboard's URLs
  (`/issues/query/107acb4d18704a6e` rather than the SQL), and
  `GET /api/families/{type}/{id}` to turn one back into its key. Links of the
  earlier `?key=` form still open.
- **`Store::store_counting`** (`rainier-lookout`): stores the kept batches
  and adds the aggregates of the ones only counted. The built-in stores
  implement it; the default, for a store of your own, drops the counted ones.
- **`with_lookout_viewers_where_async`** (`rainier-framework`): a Lookout
  viewer predicate that can await — for an application whose administrators
  are a group in a table rather than a claim on the request.

- `Upsert::maximum` and `UpsertAction::Maximum` (`rainier-orm`): on a
  conflict, keep the larger of the stored and the inserted value.
- **Lookout drills in, forecasts and keeps cases** (`rainier-lookout`).
  - Every dashboard row opens its own page — one route, query, job, exception
    or outgoing call — with its series over time, a forecast, p50/p95/p99, a
    duration histogram and every occurrence linking to its trace; before, a
    click went to the whole category.
  - The dashboard shows throughput, request, error and job metrics with
    graphs; a Jobs page compares every job's runs, averages, maxima and total
    time; every table sorts, filters and fits a phone; a search box looks
    through batches, entries and cases.
  - **Insights**: what is new, rising, falling, slower or faster than the
    previous window, and where each type is heading, with thresholds from
    `LOOKOUT_INSIGHTS_*` that a viewer can change.
  - **Cases**: each exception fingerprint is a case with a severity that
    escalates with occurrences, affected users and regressions, the routes,
    jobs and users it touched, and links to every trace. A case can be
    resolved, reopened or ignored; one that recurs after being resolved
    reopens as a regression. The database store adds migration
    `rainier_lookout_0005_cases`.
  - **Code traces**: an exception's application frames, and for a slow query,
    cache call or outgoing request the code it was called from
    (`LOOKOUT_CAPTURE_CODE_TRACES`, on).
  - API: `GET /api/series/{type}`, `GET /api/insights`, `GET`/`POST
    /api/cases…`, and `key`/`q` on the entries and `q` on the batches lists.
- `Rainier::with_lookout_viewers_where`: let a predicate over the
  authenticated user **and the request** decide who sees Lookout, for an
  application whose roles live in the session or a header rather than on the
  user value. The guard still authenticates first.
- **Lookout, the diagnostics suite** (`rainier-lookout`, `rainier-framework`
  feature `lookout`, mirrored by `rainier`). Every request, job, console
  command and scheduled task is recorded as a batch of the queries, cache
  calls, outbound requests, jobs, mail, events, logs and failures it caused,
  with slow-entry thresholds, N+1 detection, per-route/guard/user scope and
  per-watcher sampling, and browsed at `/_lookout`. See `docs/lookout.md`.
  - **Wired in at boot** from `LOOKOUT_*` (an invalid value fails the boot),
    adjusted with `Rainier::with_lookout(|config| …)`: the recorder's
    `tracing` layer joins the framework's subscriber, its middleware sits just
    inside `Trace`, it listens on the event bus, reads the guard and user from
    the `Authenticated` marker on the response, and its background writer
    starts after boot; `Lookout` and `Arc<dyn Store>` are bound in the
    container. Disabled, nothing is bound or mounted.
  - **Storage**: in memory, Lookout's own tables on any connection
    (`rainier_framework::lookout::migrations()`, opt-in like the queue's), or a
    ring buffer in any cache store.
  - **The dashboard** is mounted outside every middleware group, open in
    `local` and elsewhere behind the dashboard guard plus a `viewLookout`
    ability on the application's `Gate` (`Rainier::with_lookout_viewers::<U>()`);
    anyone else gets the router's own 404.
  - **`lookout:prune`, `lookout:clear`, `lookout:status`** in
    `rainier_framework::console`; `lookout::flush` and `lookout::shutdown` for
    the process's last batches.
  - **The feature** turns on every component's spans without the OTLP
    exporter; `cargo rainier features` maps `LOOKOUT_ENABLED=true` to it.
  - **`RUST_LOG` is now a per-layer filter** on the framework's log lines and
    exporter (`TelemetrySettings::subscriber`, `install_logging_with`,
    `install_with`), so a layer beside them sees spans the log filter drops.
    The log output is unchanged.
  - Example: `cargo run -p rainier-framework --example lookout --features lookout`.
- **Every failure on the event bus** (`rainier-events`, `rainier-http`,
  `rainier-server`, `rainier-queue`, `rainier-console`, `rainier-scheduler`,
  `rainier-framework`, and the crates that swallow failures). The framework no
  longer decides which failures matter: everything it would otherwise swallow
  — beyond a `tracing` line — is published as a typed event, and the
  application subscribes to what it cares about. See
  `docs/errors.md#reporting`.
  - **The catalogue** (`rainier-events`, `rainier_events::report`). One event
    struct per kind of failure — `HttpRequestFailed`, `WebSocketFailed`,
    `ServerFailed`, `MiddlewareFailed`, `AuthFailed`, `SessionFailed`,
    `OAuthFailed`, `AccountFailed`, `JobAttemptFailed`,
    `QueueMessageDiscarded`, `QueueOperationFailed`, `CommandFailed`,
    `ScheduledTaskFailed`, `ListenerFailed`, `BackgroundTaskFailed`,
    `BroadcastFailed`, `NotificationFailed`, `MailFailed`, `PublishFailed`,
    `WebhookFailed`, `CacheFailed`, `DatabaseFailed`, `FilesystemFailed`,
    `IntegrationFailed`, `TelemetryFailed`, `ConfigurationFailed`,
    `ApplicationFailed` — each with
    fixed typed fields, an `exception: Exception` and a `meta: ReportMeta`,
    built with `X::new(exception, fields…)` and implementing `FailureEvent`
    (`with_extra`). `Failure` is the catalogue as an enum and `FailureKind`
    without data. `Exception` is a `Clone` snapshot of an `Error`, a
    `std::error::Error` or a panic: kind, status, the error's own message,
    details, cause chain, backtrace, and the original `Arc<Error>`
    (`Exception::from_ref` copies all but that).
    `ReportMeta` carries a per-report `id`, `reported_at_ms`, `pid`, `host`
    and free-form `extra`. Everything but the original error is
    `Serialize + Deserialize`, for forwarding to another process.
  - **`Reporter`** (`rainier-events`). `report(failure)` dispatches the typed
    event and then the catch-all `ExceptionReported { failure }`, both with
    `dispatch_quietly`, so a failing listener never changes the outcome of
    what failed. **Everything is reported by default, 4xx included**;
    `dont_report(ErrorKind)` (never a panic) and `ignore(FailureKind)` filter.
    `with_context(|kind, meta| …)` adds context; a failure's own `extra`
    wins. `report_error(e)` reports an `ApplicationFailed`.
  - **The process-wide reporter** (`rainier-events`). `report::install`,
    `uninstall`, `installed`, `report`, `report_error` and `report_detached`
    (spawned, for code that is not `async`) — how a cache, a queue driver or a
    session store with no handle to the application reports. `capture()`
    collects them in a test. A listener that fails under `dispatch_quietly`
    is reported as a `ListenerFailed`, except a listener of a failure event.
  - **Who reports what.** The HTTP kernel, every error response and caught
    panic (`HttpRequestFailed`; `IntoResponse` for `Error` attaches
    `OriginalError(Arc<Error>)` so the report keeps the kind and cause chain);
    the server and WebSocket upgrade (`ServerFailed`, `WebSocketFailed`); the
    queue worker, every failed attempt, released or failed for good, a panic
    as a panic (`JobAttemptFailed`; `Worker::with_connection` names the
    connection, and a worker without `with_reporter` falls back to the
    process-wide reporter; `JobFailed` and `JobReleased` gain `exception`); queue drivers (`QueueMessageDiscarded`,
    `QueueOperationFailed`); `Console::run`, a command's `Err` or panic —
    resumed afterwards (`CommandFailed`); `schedule:run` / `schedule:work`
    (`ScheduledTaskFailed`; `RunSummary` gains `errors`). In
    `rainier-framework`: `kafka::publish_events` (`PublishFailed`), the Kafka
    relay (`BroadcastFailed`), `ModelCache` (`CacheFailed`, store
    `model_cache`), captcha verification (`IntegrationFailed`, service
    `captcha:<provider>`), session delivery keys (`SessionFailed`), a
    panicking health check and `queue:work`'s signal handler
    (`BackgroundTaskFailed`), the worker's metrics endpoint
    (`TelemetryFailed`), and configuration boot carried on past — a missing or
    unusable `APP_KEY`, CORS no preflight reaches, per-process rate limits and
    scheduler locks, a symlink out of `public/`, a bare `--option`
    (`ConfigurationFailed`). The other crates report their own; the table in
    `docs/errors.md` lists every emitter.
  - **A failing scheduled command is reported twice, linked.** The console's
    `CommandFailed` carries the command's own error; the scheduler's
    `ScheduledTaskFailed` carries `meta.extra["command"]`, equal to the
    `CommandFailed`'s `command`.
  - **Wired at boot** (`rainier-framework`). `Rainier::boot` builds one
    `Reporter` over the application's dispatcher, binds it, hands it to the
    kernel (`queue:work` to the worker), and — unless built
    `without_facades()` — installs it process-wide; the last application
    booted wins. Every report carries `app` and `environment` in
    `meta.extra`. `Rainier::with_reporting(|reporter| …)` filters and adds
    context. `rainier_framework::report(&app, e)` (`ApplicationFailed`),
    `report_failure(&app, failure)` and the `Report` facade report from
    application code. The catalogue is re-exported as
    `rainier_framework::report`, with `Exception`, `ExceptionReported`,
    `Failure`, `FailureEvent`, `FailureKind`, `ReportMeta` and `Reporter` at
    the root. `TestApp::reported()`, `TestApp::reported_of::<F>()` and
    `TestApp::assert_nothing_reported()` assert on what a test caused.
  - `cors::warn_if_cors_cannot_answer_a_preflight`,
    `limits::warn_if_rate_limits_are_not_shared` and
    `scheduling::warn_if_locks_are_not_shared` now return the warning as an
    `Option<ConfigurationFailed>`, which boot reports.
- **Row-change notification** (`rainier-orm`, `rainier-database`). Every write a
  statement builder or `rainier_orm::repo` / `Query` makes now reports a
  `RowChange` — table, kind, and the primary keys it touched when it names
  them — and `Database::on_change` subscribes to them, awaited before the write
  returns. Covers bulk updates and counter increments, which lifecycle hooks
  never did. `Prepared` gains a `change` field (`Prepared::changing`),
  `Executor` a `rows_changed` method with a no-op default, and
  `statement::pinned_keys` reads the keys a criteria pins. See
  `docs/repositories.md`.
- **`ModelCache`** (`rainier-framework`). A cache of a model's hot rows only —
  counted per process over a rolling window — kept in the application's cache
  and invalidated by `Database::on_change` the moment the row is written:
  tombstones for named rows, a generation bump for unnamed ones, atomic
  set-if-absent so a racing read cannot restore a stale row, and a checked
  index for lookups by another column. See `docs/cache.md`.
- **`SharedRedis`, a self-healing handle to the application's Redis**
  (`rainier-drivers`, reached through `rainier-cache`'s
  `StoreConfig::shared_redis()`, `RedisStore::shared()` and
  `RedisClusterStore::shared()`). Opens lazily; a connection failure drops the
  connection so the next call opens a fresh one, and a failed open backs off
  instead of dialling on every call. For commands the cache does not wrap,
  against the server the cache already declares. See `docs/cache.md`.
- **`RedisScript` and `hash_slot`** (`rainier-drivers`). A Lua script that
  computes each declared key's cluster hash slot on the client and refuses a
  call spanning more than one — on any server, so a test against a single
  node catches the `CROSSSLOT` a cluster would raise — and runs by `EVALSHA`
  with a fallback to the source. `RedisConnection::eval` and
  `SharedRedis::eval`. Enables the `redis` crate's `script` feature.
- **Batched upserts** (`rainier-orm`, `rainier-database`).
  `repo::upsert_many_with` folds a slice of rows into a table with one
  `INSERT … VALUES (…), (…) ON CONFLICT` per chunk, chunked under every
  dialect's placeholder cap, with every `Upsert` action including
  `increment`; `statement::upsert_many_with` is the one-statement form. See
  `docs/repositories.md`.
- **Add-ins: billing, single sign-on and account scaffolding**, three new
  optional crates, each behind one `rainier-framework` feature so an
  application that asks for none compiles none of them. `cargo rainier
  features` builds the ones a deployment names in `RAINIER_ADDINS`.
  - **`rainier-billing`** (`billing-stripe`). A Stripe client over the
    framework's outbound HTTP port — customers, Checkout (payment,
    subscription and setup modes), the billing portal, subscriptions (swap,
    quantity, cancel now or at period end, resume, pause), payment and setup
    intents, refunds, invoices, prices and products, and auto-pagination.
    Every POST carries an `Idempotency-Key`, every call a pinned
    `Stripe-Version`; a refusal surfaces as a typed `StripeError`, with 429 and
    5xx reported as retryable. Webhooks are verified (`Stripe-Signature`,
    constant-time, tolerance checked after the signature, secret rotation),
    deduplicated by event id through the cache, and dispatched onto the event
    bus as `StripeEventReceived` plus eleven typed events; a failing listener
    answers 500 and releases the claim, so Stripe's retry is processed.
    `SubscriptionSnapshot` gives the subscription predicates (`valid`, `on_trial`,
    `on_grace_period`, …) and `Billable` the customer helpers. Tested against
    the HTTP fake; see `docs/billing.md`.
  - **`rainier-sso`** (`sso`). OAuth 2.0 and OpenID Connect sign-in — GitHub
    (and Enterprise), GitLab (self-hosted), Discord, Google, Microsoft Entra
    ID, and any OIDC issuer by discovery. PKCE always, state and nonce held in
    the session and spent on the first callback, ID tokens verified against
    the issuer's JWKS (`iss`, `aud`, `azp`, `exp`, `nonce`, one refetch on an
    unknown `kid`). `login_with` finds, links or creates the user and logs them
    in, and refuses to link an existing account by email unless the provider
    says the address is verified. Providers are declared in configuration and
    resolved with `Sso::driver("github")`. See `docs/sso.md`.
  - **`rainier-accounts`** (`accounts`). Registration, login, logout,
    password reset, email verification, password confirmation and password
    change, mounted with `accounts.routes(router)` and switched per flow by
    `Features`. Login is throttled per address and IP and costs the same for
    an unknown address as a wrong password; `/forgot-password` answers
    identically for every address and delivers the link off the request so
    its latency cannot be timed; reset tokens are stored hashed, single-use and
    expire by cache TTL; verification links are signed and bound to the
    address they were sent to. `EnsureEmailIsVerified` guards routes. Every
    flow raises an event. See `docs/accounts.md`.
- **More add-ins**, on the same terms — each behind its own feature, each with
  a runnable example in `crates/rainier-framework/examples/`, and an overview
  of all of them in `docs/add-ins.md`.
  - **`rainier-oauth-server`** (`oauth-server`). An OAuth 2.0 authorization
    server: authorization code with PKCE (required for public clients),
    refresh with rotation, client credentials, password (opt-in), personal
    access tokens, scopes, RFC 7009 revocation and purging. RS256 access
    tokens; refresh tokens and codes sealed through a `TokenSealer` port
    (Rainier's own encrypter by default). A code or refresh token is spent
    before tokens are issued, so a raced request yields one set. `OAuthGuard`
    accepts the tokens on API routes, with scopes arriving as `Abilities`.
    See `docs/oauth-server.md`.
  - **`rainier-mfa`** (`two-factor`, `passkeys`). RFC 6238 TOTP (SHA-1/256/512,
    replay-protected, pinned by the RFC vectors), single-use recovery codes,
    and a WebAuthn relying party in pure Rust — ES256, EdDSA and RS256, `none`
    and `packed` attestation, sign-count regression refused — verified against
    published browser vectors. With `accounts` on, the login gains the
    two-factor challenge (throttled; each code step claimed atomically) and
    enrolment, recovery and passkey routes. See `docs/two-factor.md`.
  - **`rainier-compat`** (`compat-php`, `compat-php-oauth`,
    `compat-php-webauthn`). Interoperating with the stack an application is
    migrating from, grouped by language: PHP `serialize`/`unserialize` (never
    instantiating anything), defuse ciphertexts, encrypted cookies, sessions
    adopted on first touch or shared in coexist mode, cache values and tagged
    keys, CSRF tokens, queue payloads, OAuth tokens and tables written by
    league/oauth2-server, and passkey credentials written by
    web-auth/webauthn-lib. Fixture tools export real values from the other
    application, and an ignored test suite checks every reader against them.
    See `docs/compat.md`.
  - **Passive instrumentation** (`telemetry`). Every component gains a
    `telemetry` feature; the framework's turns them all on with the OTLP
    exporter, and `TELEMETRY_ENABLED=true` plus `OTEL_EXPORTER_OTLP_ENDPOINT`
    is the whole configuration. Spans follow OpenTelemetry semantic
    conventions for requests, outbound HTTP (injecting `traceparent`),
    queries (statement text, never bound values), cache, storage, jobs, mail,
    notifications, events, broadcasts, sockets, scheduled tasks, commands,
    sessions, logins and views; the trace continues from a dispatch into the
    worker that runs the job. Queue performance is first-class: wait time from
    when a job became runnable, per-phase timings, outcomes, and
    `queue_job_*` metrics with depth sampled off the hot path;
    `WORKER_METRICS_ADDRESS` serves a worker's `/metrics`. With the feature
    off, no span code is compiled. See `docs/observability.md`.
  - **CSRF protection** (`rainier-session`). `VerifyCsrfToken`: the session
    token against `_token`, `X-CSRF-TOKEN` or `X-XSRF-TOKEN`, `Sec-Fetch-Site`
    honoured, an except-list, `419` on mismatch, and failing closed without a
    session. See `docs/sessions.md#the-csrf-token`.
  - **Impersonation** (`rainier-auth`). `Impersonation` with a mandatory
    policy, refusing nested and self-impersonation, rotating the session on
    both transitions, and `ForbidImpersonation` for routes it must not reach.
  - **Captcha verification** (`captcha`). Turnstile, hCaptcha and reCAPTCHA
    tokens verified server-side, failing closed, with `RequireCaptcha`
    middleware. See `docs/captcha.md`.
  - **A Discord notification channel** (`notify-discord`), by webhook or bot,
    with embeds, mentions off by default, and routes validated before use.
  - **X sign-in** (`sso`), as the `x` driver.

- **General SQL expressions** (`rainier-database`). `expression::Expression`
  and `Predicate`: columns, bound values, portable functions (`LOWER`,
  `COALESCE`, `NULLIF`, `GREATEST`, `CONCAT`, …), arithmetic, `CAST`, `CASE`,
  aggregates with `DISTINCT`, date parts, `ROW_NUMBER`/`RANK`/`DENSE_RANK`
  windows, scalar sub-selects; any-to-any comparisons, escaped `LIKE`, `IN`
  lists and sub-selects, `BETWEEN`, `EXISTS`, nested `AND`/`OR`/`NOT`. Each
  renders per dialect. `Criteria` takes them in `where_expr`, `having`,
  `select_expr`, `group_by_expr`, `order_by_expr` and `join_as`/`left_join_as`
  (self-joins, anti-joins), and `Assignment::Expression` writes `SET col =
  <expr>`. See `docs/repositories.md#expressions`.
- **`INSERT … SELECT`** (`rainier-database`). `SubSelect` is now a full query:
  joins (including derived tables), `HAVING`, `ORDER BY`, `OFFSET`,
  `from_select` and `union_all`. `statement::insert_select` writes its rows
  into an entity's table, `statement::upsert_select` adds a per-dialect conflict
  clause whose assignments read the incoming row through `incoming(col)`, and
  `statement::select` reads one as rows. Columns and select width are checked
  before rendering; sharded entities are refused.
- **`statement::insert_many`** (`rainier-database`): many rows of one entity
  in one `INSERT … VALUES (…), (…)`, rendered per row exactly as `insert`
  renders one. Refuses an empty batch and one that spans shards.
- **`statement::insert_with_key`** (`rainier-database`): an `INSERT` that
  writes the entity's primary key even when it is auto-increment — for
  fixtures other rows refer to by id, imports and restores. `insert` still
  leaves such a key to the database.
- **Vector distance in expressions** (`rainier-database`):
  `vec_distance_cosine(a, b)` and `vec_from_text(text)` render as MariaDB's
  `VEC_DISTANCE_COSINE`/`VEC_FromText`, pgvector's `<=>`/`CAST(… AS vector)`,
  and sqlite-vec's `vec_distance_cosine`/`vec_f32`. `selected(name)` orders by
  a select-list alias unqualified, which is the form MariaDB's vector index
  answers.

- **Disks declared in configuration** (`rainier-filesystem`). A `filesystems`
  section — a `default` naming one of a `disks` map, each entry naming **its
  own** driver and settings — deserialises into `Disks` and builds a `Storage`
  in one call: `Disks::new("uploads").with("uploads", DiskConfig::local(…))
  .with("archive", S3Disk::new(…).endpoint(…))`, or `Rainier::with_disks(…)`,
  or the section itself. `keys::FILESYSTEMS` reads it; `default_config` seeds
  one `local` disk under `<base>/storage/app`, so the default boot is what it
  was.

  Each disk is built from its own declaration, which is the point. The previous
  arrangement had a consuming application loop over a hardcoded list building
  every named disk from **one** connector, and that cannot express two disks on
  two services: the second gets the right bucket name pointed at the wrong host.
  It does not raise — it reads an empty prefix, which is indistinguishable from
  a bucket that is genuinely empty. `Disks::build` has no shared connector to
  inherit from, and the test that pins it asserts on the region each built
  client will sign for, with the by-hand shared-connector build beside it as a
  negative control.

  An s3 disk's credentials default to the **ambient chain** — the SDK's
  discovered, refreshed provider chain — and take an explicit `key`/`secret`
  pair for a service that has no chain (R2, MinIO, B2). That default is the safe
  direction: a disk that should have named a pair fails to authenticate, where
  the reverse authenticates successfully against somebody else's bucket. Half a
  pair is refused rather than silently falling back to the chain, an explicit
  pair without a `region` is refused rather than signing for a guessed one, and
  no `Debug` anywhere renders a credential.

  A declaration is checked when it is read, not when a write lands somewhere
  unexpected: `driver` is required, a setting the chosen driver does not use
  (`bucket` on a `local` disk) is an error rather than a dropped key, an unknown
  setting is a typo rather than a comment, and a `default` naming an undeclared
  disk stops the boot instead of falling back. `Storage::disk(name)` still
  answers `None` for an unregistered disk and still never falls back.

  `Filesystem::as_any` is new and required — one line, `self`, in each driver —
  with `Storage::as_driver::<T>()` over it. Once disks come from configuration
  nobody holds the concrete value, so there was no route back to
  `S3Filesystem::client()` for the operations the port does not carry: a
  presigned URL, a multipart upload, object tagging.

- **Composite primary keys** (`rainier-orm`). `#[derive(Entity)]` takes more
  than one `#[orm(pk)]` field, so a join table or a per-bucket aggregate keyed
  `(parent_id, slot)` can be modelled instead of dropped to raw SQL. The key is
  every marked field **in declaration order** — which is the column order of the
  emitted `PRIMARY KEY (a, b)`, and so decides which prefix lookups the index
  serves. `Entity::primary_key_columns()` / `pk_values()` are the general form;
  `primary_key()` / `pk_value()` still answer for the *first* key column, which
  is what shard routing and route-model binding want, and are unchanged for the
  entities that already had one.

  Every key predicate is built in one place (`rainier_orm::key`), because the
  failure mode is silent: a `WHERE` missing one part of a composite key still
  parses, still runs, and still reports a plausible row count — it just matches
  everything sharing the part that survived, so an `UPDATE` overwrites siblings
  and a `DELETE` removes them. `repo::update` and `Tracked::save` take the whole
  entity, so they cannot be handed a partial key at all; `repo::find_by_keys` /
  `delete_by_keys` (and `statement::select_by_keys` / `delete_by_keys`) take a
  positional list and refuse one of the wrong length rather than narrowing or
  widening the match.

  The APIs that take a *single* key value — `find_by_pk`, `delete_by_pk`,
  `cursor`, `Tracked::load`, `Query::first_or_create` — are bounded on the new
  `SingleKey` marker, which the derive emits only for a one-column key. Pointing
  one at a composite entity is a compile error rather than a partial-key `WHERE`.
  `Model` requires `SingleKey` for the same reason: `Repository::find`/`delete`
  take one `Value` and route binding names one column, so a composite entity is
  refused where the error names the model. Such tables stay fully usable through
  Rainier ORM itself and through `Criteria`.

- **`rainier-features` and the `cargo rainier` subcommand.** Cargo cannot
  enable features from code — they resolve before anything compiles, they
  are additive, and a build script cannot add one — and dead-code
  elimination cannot size the binary either, because a well-built
  application's driver matches are deliberately exhaustive, so every
  compiled driver is referenced. So the feature set is *computed*: the
  library reads a deployment's environment file (every runtime driver
  selection) and its source tree (the compile-time choices: `Jwt`, the
  `Http` facade) and answers with the minimal feature list, with reasons.
  The mapping lives in the framework because it is knowledge *about* the
  framework: tests pin it against every driver enum, so a new driver breaks
  this crate until the table learns it, in the same commit. An environment
  file is **required** — explicit `--env`, or `.env` — with no fallback to
  `.env.example`: sizing from the example's defaults would shape the binary
  like the documentation rather than the deployment, silently. `--list`
  emits the bare comma-separated set for scripts and Dockerfiles, where an
  unforwarded selection is always fatal. `cargo install cargo-rainier` for
  the standalone door (`cargo rainier features [--check|--list]`, `cargo
  rainier build`), and what the sample project's Dockerfile installs —
  pinned to the framework revision its lockfile pins. A workspace that
  prefers no global tools can put a thin xtask over the library instead.

- **The PHP envelope's GCM variant** (`rainier-crypt`).
  `PhpEncrypter::new(keys).writing(PhpCipher::Aes256Gcm)` writes the
  `{iv, value, mac: "", tag}` shape a GCM-configured PHP application
  produces, byte for byte — including the empty `mac` its payload check
  insists on. Reading takes **both** variants whatever is selected, and
  tolerates the missing `mac` key at least one earlier reimplementation
  omitted; a cross-implementation vector from exactly such a writer is
  pinned in the tests.

### Changed

- Lookout's time range lives in the bar under the header — 1h, 6h, 24h, 7d,
  or a custom pair of dates — and follows the viewer from page to page.
  Dashboard, Requests, Jobs and an issue's charts read it; newest-first pages
  leave it in the URL and do not filter by it. A custom range sends `until`
  on `/api/series` and `/api/aggregates`, which otherwise still run up to now.
- Lookout's dashboard draws request throughput and average request time on
  one chart: requests per minute on the left axis, mean duration on the
  right, each with its forecast.
- **Lookout's cache store no longer rewrites every bucket and case on every
  write** (`rainier-lookout`). Each aggregate bucket used to be listed in one
  `lookout:buckets` key and every case kept in one `lookout:cases` key, both
  read and rewritten whole under the writers' lock on every flush — a day of
  per-minute buckets was already 0.8 MB, a month about 25 MB, and 1000 cases
  1.4 MB more, on every flush of every pod. Buckets are now listed per UTC
  day (`buckets:{period}:{type}:{day}`, rewritten only when a bucket first
  appears, and `buckets:days`), and each case is its own `case:{id}` key under
  a small `cases:index`. Measured with the new `cache_store_cost` test (a full
  ring of 1000 batches, a day of buckets, 1000 cases, 1 ms per command): a
  flush with an exception went from 2.9 MB read and 2.9 MB written in 115 ms to
  0.8 MB and 0.9 MB in 17 ms. The first write after the upgrade folds the old
  keys in and deletes them; readers merge them until then.
- **Lookout's cache store sends its commands concurrently** and reads less:
  listing entries of a type read every batch in the ring one after another
  (1001 commands, 16.8 MB, 2.6 s in that test) and now reads the newest few
  at a time and stops once nothing older can hold a newer entry (17 commands,
  0.7 MB, 8 ms). A case listing reads only the cases on the page (1.4 MB →
  0.2 MB); insights read 8.4 MB → 0.25 MB, and the insights endpoint reads its
  ten types at once.
- **Lookout's code traces leave the request faster** (`rainier-lookout`).
  The stack is still walked as a slow span closes, but symbolised by the
  background writer on a blocking thread, and a batch walks at most
  `MAX_CODE_TRACES_PER_BATCH` (16). A request of twenty slow queries spent
  627 µs recording, 203 µs now (`hot_path_cost` test, release, macOS). A batch
  the production keep rule drops is dropped before the N+1 search, and span
  fields move into entries instead of being copied: an ordinary request went
  from 22.5 µs to 19.1 µs, a kept one from 26.7 µs to 19.2 µs.
- **Lookout's database store writes aggregates in one upsert**
  (`rainier-lookout`, `rainier-orm`). The maximum used to be raised by a
  guarded `UPDATE` per aggregate row after the upsert — dozens of statements a
  flush; `Upsert::maximum` keeps the larger value in the upsert itself
  (`GREATEST`, SQLite's `MAX`). A family's entries are one query on the new
  index `(family_hash, type, occurred_at_us)` (migration
  `rainier_lookout_0006_entries_family_index`); the second query, for entries
  without a family hash, could not use an index and scanned every entry of the
  type. Entries without a family hash are now hashed by their summary; ones
  written before the upgrade are not found by their family and age out with
  their batches.
- **A client error no longer captures a backtrace in a release build**
  (`rainier-support`). With `RUST_BACKTRACE=1` set in production — as the
  images often set it — every 404, 401 and failed validation walked the stack
  and then symbolised it two or three times (the rendered response, the
  failure report, an auth failure's own report): about 160 µs per error on
  Apple silicon, 220 µs on emulated x86-64, for a stack nobody reads. A debug
  build still captures one for the debug page, and `Error::with_kind` captures
  one when it turns an error into a server error. New
  `Error::backtrace_text()` symbolises once however often it is asked; the
  framework's reporters and `IntoResponse` use it, which takes a 500's three
  renders from 160 µs to 107 µs. A 4xx's failure report, and so a Lookout
  case for one, no longer carries a stack; such a case's fingerprint loses its
  first application frame, so a 4xx case recorded before the upgrade is
  opened again under a new id.

- **Lookout no longer counts a request's client error as a failure**
  (`rainier-lookout`). A `404` or `422` answered to a request is recorded
  with its status, but adds no exception, does not flag the batch and opens no
  case; `LOOKOUT_CAPTURE_CLIENT_ERRORS=true` restores the old behaviour. A
  client error inside a job or command is still recorded.
- `rainier_lookout::Store` gains `aggregate_rows` (required; `aggregates` is
  now provided on top of it), `family_entries`, and `cases`, `case` and
  `save_case` (provided, keeping no cases). `BatchQuery` gains `search`.
- **The minimum supported Rust version is 1.100**, up from 1.99: the
  workspace `rust-version`, every toolchain pin in CI, and what
  `docs/installation.md` and the README badge say (both still said 1.89).
  Supersedes the 1.89 entry below. No public signature changed; internally
  the code uses `str::trim_prefix` / `trim_suffix` and `mem::DropGuard` where
  they replace a hand-written equivalent.
- **Dependencies nothing used are gone**, found by Cargo 1.100's
  `unused_dependencies` lint: `async-trait` from `rainier-container`,
  `rainier-http-client` and `rainier-mail`, `tracing` from `rainier-drivers`
  and `rainier-websocket`, and `http-body-util` from `rainier-http`'s build
  (its one use is a test, so it is a dev-dependency now). The unused
  `thiserror` workspace entry and `authors` workspace field go with them.
- **An unused dependency now fails the build**: `unused_dependencies = "deny"`
  under `[workspace.lints.cargo]`, which every crate inherits. To get there,
  each dependency that was only used behind a feature is now `optional` and
  compiled by that feature alone:
  - `rainier-cache`: `rainier-drivers`, by `redis-driver`, `memcached` and
    `dynamodb`.
  - `rainier-filesystem`: `rainier-drivers` by `s3`, `tracing` by `telemetry`.
  - `rainier-mail`: `rainier-drivers` by `smtp`, `tokio` by `ses`.
  - `rainier-mfa`: `hmac` and `sha1` by `totp`; `base64`, `serde` and
    `serde_json` by `passkeys`; `rand`, `sha2` and `subtle` by either.
  - `rainier-compat`: everything, by `php` — the crate is empty without it.
  - `rainier-drivers`: `anyhow` by `redis-driver`, and the SQL executors'
    dependencies by the executor that uses them (`sea-orm-executor`,
    `d1-http`, `libsql-http`) rather than by the shared `sql` feature, which
    now only gates the module. Its unused optional `serde` is gone.

  No feature was added, removed or renamed, and every `rainier-framework`
  feature compiles the same packages with the same features as before; a
  crate used directly, without the facade, compiles the same or less. The
  lint is silent about a dependency another crate in the graph already pulls
  in, so it catches less than its name promises — see the comment beside it
  in `Cargo.toml`.
- **Unused dependencies the lint cannot see are gone too** — each is pulled
  in by another crate in the graph, which is what hides it: `base64` and
  `chrono` from `rainier-accounts`, `chrono` from `rainier-auth`,
  `rainier-support` and `tracing` from `rainier-metrics`, `rainier-support`
  and `serde` from `rainier-openapi`, and `rainier-view`'s optional `tracing`
  (its `telemetry` spans go through `rainier-support`). `async-trait` in
  `rainier-server` and `serde_json` in `rainier-database` are used only by
  tests and doc examples, and are dev-dependencies now.
- **`QueuedJob` gains `traceparent: Option<String>`** (`rainier-queue`), the
  trace context a dispatch stamps for the worker to continue. Serialised only
  when present and defaulted when absent, so envelopes written before it read
  unchanged. Code that builds a `QueuedJob` with a struct literal — usually a
  test helper — adds `traceparent: None`.
- **The `Trace` middleware no longer holds its span across an `.await`**,
  which could attribute one request's log lines to another; it instruments
  the request future instead.
- **Dependencies refreshed** (lockfile only): hyper 1.12, tokio 1.54,
  rustls 0.23.46, rustls-webpki 0.103.16, opentelemetry 0.33.1, the
  aws-smithy stack, and smaller ecosystem bumps. No manifest ranges changed.
- **The authentication docs no longer list password reset, email
  verification and OAuth as missing** — they are the `accounts` and `sso`
  add-ins.

- **The minimum supported Rust version is 1.89**, up from 1.88. `aes` 0.9,
  which the RustCrypto generation `rainier-crypt` now builds on, declares 1.89,
  and it is in the default build.

- **The PHP compat layer is layered like one** (`rainier-crypt`). "Php" in
  `PhpEncrypter` names a wire format, not a cipher, and the type no longer
  fuses the two: `php::envelope` is the codec (JSON/base64/hex and which
  bytes the MAC covers — no cryptography), `php::primitive` is the raw-key
  AES-256-CBC / AES-256-GCM / HMAC (no encoding), and `PhpEncrypter` is the
  thin composition holding the key ring, the write selection and the
  one-error policy. The public API and the wire format are unchanged; the
  pinned interop vectors prove it.

- **Mail transports that actually send** (`rainier-mail`). The page that said
  "implement `Transport` yourself" now ships them: `SmtpTransport` (lettre
  over rustls, feature `smtp`), `SesTransport` (the AWS default chain,
  feature `ses`), and `PostmarkTransport` / `MailgunTransport` /
  `SendGridTransport` / `ResendTransport` (features `postmark`, `mailgun`,
  `sendgrid`, `resend`) — one cargo feature per destination, because "we send
  through Postmark" is one fact about a deployment.

  Every sender carries the same message: `render_eml`'s MIME document travels
  whole over SMTP, SES and Mailgun, and the JSON APIs are fed from the same
  fields it renders. `Bcc` stays blind by construction on every path — the
  headers never carry it; the envelope, destination list or API field does.

  The HTTP providers ride the framework's own HTTP transport port, so their
  tests hand them the `FakeTransport` and pin the exact request each provider
  documents. The SMTP transport is tested against a real server —
  [Mailpit](https://mailpit.axllent.org) in CI, the same container the docs
  suggest for development — including the property only a live SMTP
  conversation can check: a blind copy is delivered by the envelope while the
  headers other recipients read never name it.

- **`rainier_framework::mail::transport` and `mail::mailer`** — the step
  between `MAIL_*` in an environment file and a running `Mailer`, in the
  image of the `kafka` module. The exhaustive match over `MailDriver`:
  selecting a sender the build did not enable fails the boot naming the
  cargo feature, and a driver missing a setting fails naming the variable.

- **`MAIL_*` configuration** — host, port, credentials, `MAIL_ENCRYPTION`
  (`starttls` — required, not opportunistic — `tls`, or `none`),
  `MAIL_TIMEOUT`, `MAIL_ALWAYS_TO`, `MAIL_FILE_PATH`, and one credential
  setting per API provider. `MailDriver` gained the `ses`, `postmark`,
  `mailgun`, `sendgrid` and `resend` variants; `MailDriver::delivers()`
  still answers `false` for everything a forgotten `MAIL_DRIVER` can select.

- **`HashManager` — the algorithm behind a selection** (`rainier-crypt`).
  Argon2 and bcrypt are different algorithms, not an algorithm and its poor
  relation: each is a full `Hasher` driver, `HASH_DRIVER=argon2id|bcrypt` (or
  an explicit constructor argument) names the one `hash` writes with, and
  **verification never consults the selection** — a stored hash names its own
  algorithm in its own prefix, the `password_verify` contract. So changing
  algorithm is a deploy, and `needs_rehash` converts rows on the next
  successful login, in either direction.
- **`BcryptHasher`** (feature `bcrypt`) — bcrypt as a writable peer driver at
  cost 12, for an application sharing its users table with a PHP application
  that still writes rows. `BcryptVerifier` remains for standalone hashers,
  but inside the manager the driver reads all three prefixes itself.
- **The `Hash` facade**, and `keys::HASH_DRIVER`.
- **`Hasher::recognises`** — a driver claims its own format, which is how the
  manager dispatches. Defaulted, so nothing implementing the port breaks.
- **A format nothing recognises now fails at full cost.** The manager pads a
  corrupt or hand-filled password column with a `dummy_verify` at the
  selected driver's cost, the way the unusable sentinel already was — a rare
  row that answered quickly could be singled out of a timing profile.

### Changed

- **Password hashing moved from `rainier-auth` to `rainier-crypt::hash`** —
  hashing is cryptography, and the guards *consume* the `Hasher` port rather
  than owning it. Not breaking: `rainier-auth` re-exports the whole surface
  at its old paths, and its `bcrypt` feature forwards.

### Fixed

- Lookout's "every query like this" page no longer asks for the family by
  putting its SQL in the query string. Entries and series take `family`, the
  same 16 hex id the page URL already uses, and resolve it on the server.
  A filter in front of the app was answering 403 to the SQL.
- Lookout's dashboard tiles show the whole headline number. A long count
  shrinks to fit the tile instead of being cut off with an ellipsis, and the
  sparkline sits under the number so it does not take the number's width.
- **A WebSocket refusing its client is not a Lookout case** (`rainier-lookout`):
  a handler that refuses a client with a client error — a `401` for a revoked
  token — no longer opens a case per refusal. It follows the same rule as an
  HTTP client error: recorded only with `LOOKOUT_CAPTURE_CLIENT_ERRORS=true`.
  A socket that fails on the server's side is still recorded.
- **A session that was only read is no longer written back** (`rainier-session`):
  `StartSession` wrote every non-empty session back at the end of every
  request, so a background request that read the session before a logout and
  finished after it restored the session, and the visitor was signed in
  again. Found by an end-to-end test of signing out while the page polled.
  Unchanged sessions are now touched instead — the new
  `SessionStore::touch` extends a stored session's expiry and never creates
  one. The database and memory stores do it in place; the cache store and
  any custom store read and rewrite only a session that is still there.

- **A client hanging up an HTTP connection is no longer reported**
  (`rainier-server`). The accept loop reported every `serve_connection`
  error, and the ordinary ones — a request that ended before its body did,
  a reset, a broken pipe, an unexpected end of file — are a closed browser,
  a cancelled request, or a load balancer dropping an idle keep-alive. Those
  stay a debug line. Anything else is still reported.
- **A refused private-channel subscription is a warning, not an exception**
  (`rainier-broadcast`). The signature is over the socket id this connection
  was given, and a reconnect between authorising and subscribing presents
  the previous one. The client already receives `pusher:error` 4009. The
  warning says whether a signature was present, so a real mismatch is still
  in the log.

- **Lookout's aggregates count every request and job, not only the kept
  ones** (`rainier-lookout`). They were rolled up from stored batches, and in
  production the keep rule stores only slow, failed and monitored ones — so
  a route's count was a fraction of its traffic and its p50 to p99 read as if
  every request were slow. Every batch in scope is now counted; only the
  kept ones are stored.
- **Lookout does nothing on a request's or job's behalf after it ends**
  (`rainier-lookout`). The batch was finalized — the keep rule, slow and N+1
  detection, sorting, summaries — as its root span closed, which for a span
  the Lookout middleware opened is before the response is written. Now the
  raw batch is handed to the writer and all of that happens there: about
  17 µs a request on its own thread, from 20 µs, and a request with slow
  queries no longer pays for the slow detection.
- **One code trace per query shape** (`rainier-lookout`): twenty slow runs
  of the same query walk the stack once, not up to sixteen times — 73 µs
  for twenty slow queries of five shapes, from 209 µs.
- **The waterfall shows a slow span once** (`rainier-lookout`), with a
  `slow` pill on its own row (and `N+1` or `failed` where it applies),
  instead of a second row for the derived slow entry.

- **A WebSocket client that left without closing is now noticed, and leaving
  is no longer reported as a failure** (`rainier-server`, `rainier-websocket`).
  Nothing ever timed out a client that stopped answering — a laptop shut, a
  phone between networks — so the server went on queueing sends to it until
  the kernel gave up on the TCP connection, many minutes later. The transport
  now pings a connection that has been quiet for 25 s and closes one silent
  for 70 s, and a write that cannot finish in that time ends it too. A send
  to a socket that has gone away returns a 499 rather than a 500
  (`is_disconnect` recognises it), and a handler passing that up, a reset,
  a connection dropped without the closing handshake, or an upgrade the
  client abandoned closes at `DEBUG` instead of being reported.

- **An unreadable session is no longer replaced with an empty one**
  (`rainier-session`). `StartSession` treated a store *read failure* the same as
  a session that was not there: it started a fresh one. That session has a new
  id, so `save` wrote it to the store and set it as the cookie, and the real
  session — intact, merely unreadable for that one request — was orphaned. The
  effect is that every signed-in visitor is silently signed out by a blip, and
  the cost is paid twice: once by them, and once by a store now holding an
  abandoned session for every request the outage touched. Measured in
  production at roughly four thousand of them, on a platform with fifteen
  accounts, after a Redis Cluster shard lost its master for three hours.

  A failed read now yields a **detached** session: the id the cookie already
  carries, no data, and neither written back nor allowed to move the cookie.
  The request still renders, which is what the previous behaviour was reaching
  for and is preserved — anything written to that session is lost, which is the
  honest consequence of a store that cannot be reached. When the store returns,
  the visitor is still signed in.

  `Ok(None)` is deliberately untouched. A session that is genuinely absent —
  expired, or never issued — has nothing on the other end of its id to protect,
  and still starts and saves a fresh one exactly as before. The fix rests
  entirely on that distinction, and a test pins it so the two cannot collapse
  back into one.

- **A broker outage no longer ends a worker run** (`rainier-queue`). `Worker::run`
  propagated any error out of `reserve`, which ended the process. Under an
  orchestrator that is a restart into the same outage, then another, with the
  backoff growing each time — so a queue whose consumers are all in
  `CrashLoopBackOff` is not being drained at all, and stays that way for minutes
  after the broker itself is healthy. That is the failure mode rather than a
  theoretical one: a Redis Cluster shard lost its master, and both
  replicas of an application's interactive worker spent three hours restarting instead
  of consuming the four queues they serve, one of which carries feed generation.

  A reserve failure is now logged, waited out and retried. The wait doubles from
  `WorkerOptions::sleep` and is capped by `max_error_backoff` (30s), because a
  worker that retried a dead shard in a tight loop would answer an outage with
  load, and every replica would do it at once. Only `max_consecutive_errors`
  (20) back-to-back failures end the run — around eight minutes at the defaults,
  long enough to sit through a cluster failover and short enough that a worker
  pointed at a broker that is never coming back still exits non-zero and says
  so. Consecutive, so a long-lived worker does not slowly spend a budget on
  unrelated blips; one success resets it.

  This is the judgement already made elsewhere in the same loop — a panicking
  job fails the job rather than the worker, a uniqueness lock that will not
  release is logged rather than propagated — applied to the one call that had
  been left able to take the process down.

  `WorkerStats::errors` counts the refusals. It is deliberately not part of
  `total()`, which drives `--max-jobs`: a broker outage must not spend a
  worker's job budget without a job ever having run. `queue:work` prints the
  count when it is non-zero, since none of the other three numbers move while
  the queue cannot be reached and the run would otherwise read as a quiet one.

- **Redis Cluster failures say what happened** (`rainier-drivers`). Every cluster
  error kind fell to an arm that echoed the server's raw `detail()`. For a
  redirect that detail is the slot and the address, so a downed shard reached
  the log as `Redis: 11221 :0` — a number, a colon and a zero — in every worker,
  session read and scheduled task, for three hours, without once naming Redis
  Cluster or a shard that had lost its master.

  `MOVED`/`ASK` are now read as a redirect and reported as one. An address of
  `:0` gets its own wording: it is not an ordinary redirect but a node the
  cluster still credits with slots while having lost its address (`noaddr` in
  `CLUSTER NODES`), which is what a failed master with no promoted replica
  leaves behind, and it is unfollowable — there is nowhere to follow it to. The
  two are a world apart operationally, one resolving in milliseconds and the
  other never, so they no longer read alike. `CLUSTERDOWN`, `MASTERDOWN`,
  `TRYAGAIN`, `CROSSSLOT` and a missing node connection are likewise said rather
  than echoed. All of them stay 503s: the shard is unavailable, the request is
  not wrong.

## [2.0.0] - 2026-07-31

This release removes third-party branding from the project: the
documentation, the crate metadata, and the two public APIs that carried it.
Previous releases are yanked on crates.io, and the repository history
restarts at this release.

### Added

- **Kafka**, as a driver in `rainier-drivers` and wired into the three ports
  that can use a log.

  **Broadcasting.** `KafkaBroadcaster` publishes to one topic keyed by channel
  — a topic per channel would mean provisioning `private-orders.7`, and keying
  gets the ordering guarantee anyway. It writes the same `{event, data, socket}`
  body the Redis broadcaster does. Choose it when the event that moves a browser
  is also one the audit consumer and next quarter's service should be able to
  read; Redis pub/sub forgets it the instant it is delivered.

  **Sockets across replicas.** `KafkaRelay` reads the topic back and
  `relay::SocketFanOut` pushes it into this process's `Rooms`, which lifts the
  ceiling `rainier-websocket` documents about itself — a room registry is one
  process's memory, so two replicas had two sets of rooms. Every replica
  publishes and every replica reads, inside the web process, with no second
  deployment.

  `to_others` needed a real fix to survive that. Socket ids are per-process
  counters, so "everyone except 7" would have silenced an unrelated browser on
  every other replica. `Socket::identity()` pairs the counter with a per-process
  id, and `socket_from_identity` returns `None` for another replica's — meaning
  "not here, tell everyone".

  **Jobs.** `KafkaQueue` implements the `Queue` port and documents every place
  the port and the log disagree, because they are the kind that cost money:
  concurrency is the partition count, a delayed job blocks its partition, a
  retry goes to the end of the topic, and `clear` skips rather than deletes.
  Partition ownership is a lock-manager lease and the cursor is a cache entry —
  the shared store `on_one_server` already needs — and the constructor refuses
  a store that is not shared, because the quiet version of that mistake is every
  job running on every machine.

  **Events.** `kafka::publish_events::<E>` puts every event of a type on a
  topic, keyed by whatever identifies the subject.

  `rskafka` rather than `rdkafka`: the usual choice wraps librdkafka and wants
  a C compiler and CMake on every machine that compiles the workspace,
  including the ones that will never speak to Kafka. TLS is behind `kafka-tls`.

  Eighteen integration tests run against a real broker, in CI and locally, and
  they found two things unit tests could not — see **Fixed**.

- **`KAFKA_*` configuration** — brokers, group, topic prefix, broadcast topic,
  TLS and SASL. A misspelled `KAFKA_SASL_MECHANISM` stops the boot rather than
  falling back to `PLAIN`, which would send the password in the clear.

- **`QueueDriver::Kafka`.** Exhaustive matches on `QueueDriver` need a new arm.
  Prefer naming it explicitly over a catch-all, so the next variant is a compile
  error rather than a silent default.

### Changed

- **The PHP-compat cipher surface is renamed** (`rainier-crypt`). The encrypter
  type, its `CryptScheme` variant and the `APP_CIPHER` value that selects it
  previously carried a third-party framework's trademark; they are now
  `PhpEncrypter`, `CryptScheme::Php` and `APP_CIPHER=php`. **Breaking:** a boot
  with the old cipher value now fails, listing the valid values, and code
  naming the old type stops compiling. The wire format is unchanged — every
  existing row stays readable.
- **The template engine and its default extension are renamed**
  (`rainier-view`). The engine type is now `TemplateEngine`, and templates
  resolve as `*.view.html`. **Breaking:** rename your template files, or keep
  the old extension with `.with_extension(..)`. The syntax itself is
  untouched.
- **The documentation no longer describes Rainier by comparison to another
  framework.** Rainier is an independent project, unaffiliated with the
  frameworks its developers may be arriving from; the docs now say what each
  piece is, rather than whose it resembles.

### Fixed

- **A Kafka operation could hang for minutes.** The wire client's retry deadline
  counts accumulated *sleep* time rather than elapsed time, so a connection
  refused in two seconds is retried a dozen times before ten seconds of sleep
  add up — measured at 145 seconds to give up on a "10 second" timeout. Every
  operation now runs under a wall-clock `timeout`, so the number in
  `with_timeout` is the number.


## 1.1.0 - 2026-07-27

The second round from the same identity-provider port. Thirteen of the
fourteen requests; the fourteenth is a decision rather than a feature, and is
recorded below.

Everything is additive except one enum variant — see **Changed**.

### Added

- **A cache-backed, keyable throttle.** `ThrottleRequests` counted in its own
  process, so five replicas each enforced "five a minute" separately and the
  real limit was twenty-five. The counter is now a port: `MemoryRateLimitStore`
  is the default and says it is not shared, and `rainier-cache` implements the
  same port over any `Cache`. `keyed_by(|r| r.input("email"))` counts against
  what was submitted — the default token-or-address key is the wrong one for a
  login form — and `named("login")` stops two limiters on one route spending
  each other's allowance. The bootstrap warns when a throttled route is
  counting per-process, naming the routes.
- **Signed URLs.** `SignedUrls::route` / `temporary_route` over HMAC of the
  path and sorted query, and a `ValidateSignature` middleware. Removes the
  token table, the lookup and the sweep job behind every unsubscribe and
  verification link. `HmacSigner` gained `detached_tag`/`verify_detached`,
  which also verify against the key the tag names — so a link signed before a
  rotation keeps working.
- **JWTs and a JWKS document** (`rainier-crypt`, feature `jwt`). RS256 and
  ES256, a ring keyed by `kid`, rotation as an overlap, and a JWKS listing
  every key that can still verify. The algorithm comes from the key the `kid`
  names and never from the token's header.
- **An outbound HTTP client** — the new `rainier-http-client` crate and the
  `Http` facade, with the real transport behind the framework's `http-client`
  feature. `Http::post(url).json(..).timeout(..).retry(n, backoff)`, and
  a fake that records instead of sending. A `4xx` other than `408`/`425`/`429`
  is deliberately not retried.
- **`spawn_with_facades` and `with_facade_application`** (`rainier-container`),
  plus `Server::for_application`. A spawned task inherited no facade scope and
  resolved through the process-wide application, silently — which is where a
  served request actually runs.
- **Legacy password hashes.** `Argon2Hasher::with_legacy(BcryptVerifier)`
  dispatches on the stored hash's prefix, and `needs_rehash` converts the row
  on the next successful login. `bcrypt` is behind a feature.
- **A challenge primitive** (`rainier-auth`). `Challenges::issue`/`consume` —
  short-lived, single-use, attempt-limited, bound to a purpose, compared in
  constant time. Cache-backed, so nothing needs sweeping.
- **Password confirmation.** `ConfirmPassword::within(window)` and
  `confirm_password`, answering `423`. A session says somebody logged in at
  some point; it does not say the person at the keyboard now is them.
- **Token abilities.** `Abilities`, `RequireAbility::any/all`, and
  `UserProvider::retrieve_abilities_by_token`. `*`, an exact name, and
  `posts:*` for a namespace.
- **Model factories.** `User::factory().count(3).state(..).create(&repo)`, and
  `#[derive(Factory)]`.
- **Health checks.** `Health::register(name, check)` and an endpoint rendering
  per-check status and timing with `build_info!()`. Checks run concurrently,
  each under a deadline and in its own task.
- **A PHP-compatible cipher.** Reads and writes the `{iv, value, mac}` JSON
  envelope PHP MVC frameworks produce, byte for byte, for a database PHP
  already filled. Selected by `APP_CIPHER` (see [2.0.0] for the surface's
  current names).
- **Cloudflare Workers KV** as a cache driver (`cloudflare-kv`), compiling for
  `wasm32`.
- **`Cache::supports_atomic_add`**, and `LockManager::is_shared` now requires
  it. Shared and lock-capable are different questions: KV is visible to every
  replica and has no compare-and-set.
- `Encryption::keys()`, `KeyRing::all()`, `Router::describe()` on an
  uncompiled router, and `Health`/`RateLimits`/`SignedUrls` bound at boot.

### Changed

- **`Gate` is generic over any actor.** It required `U: Authenticatable`, so an
  API client on the client-credentials path — which has no password and no
  session — could not be authorized without pretending to be a person. The
  bound bought the gate nothing; no check ever called an `Authenticatable`
  method. Every existing `Gate<User>` keeps working.
- **`CacheDriver` gained a `Kv` variant.** This is the one breaking change and
  the reason this is a minor release: an application matching `CacheDriver`
  exhaustively stops compiling. Add a `Kv` arm rather than a `_` one — the
  sample project hit exactly this, and its own comment had already explained
  why: a catch-all would have swallowed the new driver silently instead of the
  compiler pointing at the line that needs a decision.
- `rainier-cache` and `rainier-middleware` take tokio's timer rather than the
  workspace's, which carries `net` and pulls in mio — so both now compile for
  `wasm32`.
- **The minimum supported Rust version is 1.88**, up from 1.85. It was already
  the real floor for anything using the SQL executor; the declared one now
  matches.

### Fixed

- A flaky doc test in `rainier-session` asserted an encrypted cookie does not
  contain `"42"`; base64 of random bytes contains a given two-character run
  often enough to fail a run every few hundred.

### Not done

- **A wasm32 runtime** (`rainier-server`). Asked for as item 14b and
  deliberately not attempted: the server is hyper on tokio, a Worker has no
  thread pool and its entry point is a `fetch` handler, and every future in it
  is `!Send`. That is a framework-sized project and deserves deciding on its
  own merits.
## 1.0.1 - 2026-07-27

The first round of changes driven by a real application — a 31,000-line
identity provider ported onto Rainier. Everything here is additive: nothing
that compiled against 1.0.0 needs to change.

### Added

- **A test harness.** `rainier_framework::testing` — `TestApp` and
  `TestResponse`, with `assert_ok`, `assert_status`, `assert_json_path`,
  `assert_json_missing`, `assert_header` and the rest. A feature test was forty
  lines of building a kernel and picking the response apart by hand; it is now
  three.
- **A scoped facade container.** `rainier_container::scope_facade_application`
  returns a `FacadeScope` that overrides the process-global application for the
  current thread. `TestApp` holds one, so a suite that boots an application per
  test no longer races itself.
- **`Env::isolated()` and `Env::from_map()`** (`rainier-config`), so a test
  states its own environment and is believed. The production rule — a real
  variable beats the `.env` file — is the wrong rule for a test.
- **`Response::into_bytes`, `into_string`, `into_json`** (`rainier-http`).
  `into_json` quotes the start of the body in its error, because a parse
  failure in a test is nearly always an error response nobody expected.
- **Structured logging.** `LOG_FORMAT` is `auto | pretty | compact | json`, and
  `auto` means JSON in production and staging, pretty everywhere else. Fields
  are flattened to the top level, where every aggregator's default parser looks
  for them.
- **`rainier_console::io`** — `table`, `ask`, `ask_with_default`, `secret`,
  `confirm`, `confirm_by_typing`, `is_interactive`. Column widths are counted in
  characters, prompts fail at end-of-input rather than spinning, and `secret`
  turns terminal echo off rather than falling back to a visible read.
- **`Timeout` middleware** (`rainier-middleware`), answering `408`. Turn it on
  globally with `server.request_timeout_secs`; it is off by default.
- **`Compress` middleware** — gzip and deflate for text responses over
  `min_size`, never for a stream, with `Vary: accept-encoding` on anything
  compressible. Turn it on with `server.compression`.
- **`MethodOverride` middleware**, off by default, so an HTML form can spell
  `PUT`, `PATCH` and `DELETE` through a hidden `_method` field. Only ever
  upgrades a `POST`.
- **Raw SQL with bindings.** `database.query(sql).bind(value)` with `execute`,
  `fetch_all::<E>`, `fetch_one::<E>`, `scalar_i64`, `scalar_string`, `column`
  and `prepared`. Placeholders are `?` on every dialect and are rewritten to
  `$1`, `$2` for Postgres. `route_by()` sends a query to the shard that owns a
  key, by the same rule the ORM uses.
- **`Cache::remember` and `remember_forever`** (`rainier-cache`). A failure is
  never cached: caching an error for five minutes turns one bad second into
  five bad minutes.
- **`Cache::is_shared`**, so a store answers for itself instead of being
  guessed at by name, and **`LockManager::declared_shared()`** for a store
  implemented outside this workspace.
- **`Hasher::dummy_verify`**, for the branch where there is no user — a login
  that returns in a millisecond when the email is unknown and fifty when it is
  known is a working account-enumeration oracle. **`Hasher::unusable`** and
  `is_unusable` give an account that authenticates some other way a stored
  value nothing can match.
- **`build_info!()`** (`rainier-support`), producing a `BuildInfo` with the
  crate's name and version, the commit from `GIT_SHA`/`GITHUB_SHA` if the build
  was told, and the profile.
- **`events.listen_queued::<E, J>()`** (`rainier-framework`), with the
  `FromEvent` trait — a listener that puts a job on the queue instead of doing
  the work inside the request.
- **`Router::describe()`** on an uncompiled router, so `route:list` can answer
  "what routes are there?" without building middleware that may need services
  nothing has bound yet.
- **`OpenApi::describes`, `endpoint`, `described` and `undocumented`**, for
  asserting in a test that every named route carries a description.
- **`Error::request_timeout`, `too_many_requests` and `service_unavailable`**
  (`rainier-support`).
- **`Request::set_method` and `Response::take_body`** (`rainier-http`), the two
  seams the new middleware needed.
- **`ScheduledTask::needs_shared_locks` and
  `Schedule::tasks_needing_shared_locks`** (`rainier-scheduler`), and
  `scheduling::assert_locks_are_shared` / `warn_if_locks_are_not_shared`
  (`rainier-framework`) for asserting it yourself.

### Changed

- **A schedule whose locks are decoration now says so, and `schedule:run`
  refuses in production.** If any task declares `without_overlapping` or
  `on_one_server` while the lock manager is not shared, every process logs it
  at boot — at `error` in production — and `schedule:run` and `schedule:work`
  return a failure there rather than running the task on every machine at
  once. The refusal is in the scheduler rather than in `boot` on purpose: a
  web container refusing to serve HTTP over a scheduling concern would be a
  larger outage than the one being prevented. Previously this was silent, and
  it shipped a real bug in the port.
- **Booting with `CACHE_DRIVER` set to a shared store and no cache built now
  warns.** The same gap between what a deployment believes and what it has, one
  layer down.
- `route:list` and `schedule:list` render through `io::table`, so their columns
  no longer bend on a value containing an accent.
- `LOG_FORMAT` outside its closed set is refused at boot, like a driver name.
- `rainier-middleware` depends on tokio's timer only, rather than the
  workspace's tokio — the `net` feature pulled in mio, which does not build for
  a wasm target.

### Fixed

- `Compress` would have dropped the body of a streaming response: it took the
  body out to inspect it and returned early without putting it back. Caught by
  the test written alongside it, before the middleware shipped.

### Security

- `Hasher::dummy_verify` closes the timing side channel described above. It is
  opt-in — an authentication flow has to call it on the no-such-user branch —
  and `Hasher::verify` now uses it internally when the stored hash is unusable,
  so an account with no password cannot be told apart from a wrong one by how
  fast the answer arrives.

## 1.0.0 - 2026-07-25

First release. Thirty-one crates, published together.

### Added

- **The container and the application.** Bindings, singletons, transients,
  service providers, scoped resolution and the facades.
- **HTTP.** Requests, responses, extractors, uploads, cookies, streaming and
  server-sent events, on hyper.
- **Routing.** Named routes, groups, resource controllers, route-model binding,
  a URL generator, and per-route middleware.
- **Middleware.** A pipeline with global, group and route stages; CORS, throttling,
  trusted proxies, input trimming.
- **The ORM and DBAL.** Entities, repositories, criteria, relationships with
  eager loading that makes N+1 unrepresentable, pagination, a schema builder
  that renders per dialect, and migrations.
- **Drivers.** MySQL, Postgres and SQLite through sea-orm; Cloudflare D1 and
  libSQL over HTTP.
- **Queues.** In-memory, database, Redis streams and Amazon SQS, with retries,
  backoff, unique jobs, batches and a failed-job table.
- **Cache.** In-memory, Redis, Redis Cluster, Memcached and DynamoDB, with locks.
- **Sessions, authentication, hashing, encryption and signed URLs.**
- **Mail**, with a log driver, SMTP and a fake for tests.
- **Notifications** over mail, database, broadcast and the log.
- **Broadcasting**, with channel authorisation and a Redis fan-out.
- **WebSockets**, sharing the HTTP listener.
- **Validation**, with form requests that plug into controller methods.
- **Views**, a directive-based template engine, escaped by default.
- **The console kernel** and the commands an application expects — `serve`,
  `route:list`, `migrate`, `queue:work`, `schedule:run`.
- **A scheduler**, with cron expressions, overlap and single-server guards.
- **Filesystem** — local, S3 and anything S3-shaped.
- **Observability** — Prometheus metrics, an OpenAPI document, and OpenTelemetry
  tracing with W3C context propagation. All three optional and off by default.

[Unreleased]: https://github.com/safewords/rainier-framework/compare/v2.0.0...HEAD
[2.0.0]: https://github.com/safewords/rainier-framework/releases/tag/v2.0.0
