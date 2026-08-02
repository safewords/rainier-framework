# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Every crate in the workspace shares one version, so an entry here applies to
the release as a whole and names the crate it landed in.

## [Unreleased]

### Added

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
