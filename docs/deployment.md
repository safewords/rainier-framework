# Deployment

The sample project's defaults are chosen so a fresh clone runs with nothing
installed. Several of them are wrong for production, deliberately and
obviously. This is the list.

## The checklist

| Setting | Development default | Production |
|---|---|---|
| `APP_DEBUG` | `true` in `.env.example` | **`false`** |
| `APP_ENV` | `local` | `production` |
| `DATABASE_URL` | `sqlite::memory:` | a real database |
| `QUEUE_DRIVER` | `sync` | `database` (or your own) |
| `MAIL_DRIVER` | `log` | a real transport |
| `APP_URL` | `http://localhost:8000` | your actual origin |
| `server.trust_proxy` | `false` | `true` **only** behind a proxy you control |
| `LOG_FORMAT` | `auto` → pretty | `auto` → JSON, or set it |
| `SERVER_REQUEST_TIMEOUT` | `0` (off) | a ceiling your slowest route clears |
| `CACHE_DRIVER` | `memory` | shared, if anything schedules `on_one_server` |

The framework's own defaults are already the safe ones — `app.env` is
`production` and `app.debug` is `false` when nothing sets them, so a *missing*
`.env` fails closed. It is the sample's `.env.example` that is tuned for
development.

## `APP_DEBUG=false`

With debug on, [5xx error messages reach the client](errors.md#debug-mode), and
those routinely contain a connection string, a file path, or a query. Panic
details are disclosed too.

This is the single most important line in the file.

## The database

```env
DATABASE_URL=postgres://user:pass@host/app
```

SQLite in memory is **wiped on exit**, which is what makes it a good default
and a catastrophic production setting.

Run migrations as a deploy step rather than relying on boot:

```sh
cargo run --release -- migrate --pretend    # check
cargo run --release -- migrate              # apply
```

That run is one **batch**, and `migrate:rollback` takes it back off as a unit.
`migrate` prints any step in the batch that cannot be undone, which is the one
moment that fact is worth knowing:

```
Not reversible, so `migrate:rollback` will refuse this batch:
  0005_drop_legacy_column
```

Prefer [rolling forward](migrations.md#roll-forward-when-you-can) in
production; a `down` written months ago has been tested by nothing.

The sample runs them in a provider's `boot`, which is right for development and
for a single instance. With more than one instance starting at once you want
one migration step, not a race — see [Migrations](migrations.md).

## The queue

```env
QUEUE_DRIVER=database
```

**`sync` runs jobs inline, so a failed job fails the request that dispatched
it.** That is exactly the coupling the queue exists to remove: a slow mail
server becomes a slow checkout, and a broken one becomes a failed order.

Switching needs three things:

1. `QUEUE_DRIVER=database`
2. the queue's tables — [merge them into your migrator](migrations.md#migrations-for-the-queue-and-sessions)
3. a worker process

```sh
cargo run --release -- queue:work --queue=high,default --max-jobs=1000
```

`--max-jobs` makes the worker exit after N jobs so a supervisor restarts it.
That is a cheap defence against a slow leak in a long-running process, and it
costs a second of startup.

Run `reclaim_expired()` periodically so jobs held by a worker that died become
available again.

`queue:work` exits non-zero if any job failed, which your supervisor should
notice.

## Mail

```env
MAIL_DRIVER=smtp    # or ses | postmark | mailgun | sendgrid | resend
```

The default is `log`, which sends nothing — switch before you deploy, and
enable the sender's cargo feature (`mail-smtp`, `mail-ses`, …), or the boot
fails naming it. Each sender's settings are listed under
[Mail — configuration](mail.md#configuration).

**In staging, set [`MAIL_ALWAYS_TO`](mail.md#always_to).** It redirects every
message to one address, and it is the difference between testing a flow against
a copy of production data and emailing all of those customers.

## Behind a proxy

```rust
ServerOptions::default().trust_forwarded_for(true)
```

or `server.trust_proxy = true` in config, which [`serve`](console.md#serve)
reads.

Turn this on **only** when something you control terminates every connection.
Otherwise any client can set `X-Forwarded-For` and forge its own IP — which
matters because that is what
[throttling](middleware.md#throttlerequests) keys on, so forging it defeats
rate limiting entirely.

## Body limits

```env
SERVER_MAX_BODY=2097152
```

2 MiB by default. [Request bodies are buffered](requests.md#why-bodies-are-buffered),
so this is the only thing standing between the server and a client that streams
gigabytes at it. Raise it deliberately, per what your endpoints actually
accept.

## Building

```sh
cargo build --release
```

The binary is self-contained apart from `resources/views/` and anything you
read from `storage/`. Ship those alongside it, or point the builder at where
they are:

```rust
Rainier::new("/srv/my-app")
```

## Logging

```env
LOG_FORMAT=auto        # JSON in production and staging, pretty elsewhere
RUST_LOG=info
RUST_LOG=warn,my_app=debug,rainier_server=debug
```

Standard `tracing` filters, and the line shape is
[configured](observability.md#logs) rather than hardcoded. `auto` is what you
want: production gets one JSON object per line with the fields at the top
level, where an aggregator's default parser finds them; a developer gets
colour.

Set it explicitly to read production logs by eye for an afternoon —
`LOG_FORMAT=pretty` on one box is a debugging tool, and an explicit value is
never second-guessed.

The builder installs a subscriber unless you call `without_tracing()`; call it
and install your own if you need a sink this does not cover:

```rust
Rainier::new(".").without_tracing()
```

Nothing here writes log files or rotates them. `tracing_subscriber` writes to
stdout, and collecting stdout is the platform's job — Docker, systemd, a
sidecar.

## Health checks

```rust
router.get("/health", liveness);                          // no I/O
router.get("/health/ready", health::endpoint);            // checks dependencies
```

Point the orchestrator's **liveness** probe at the first and its **readiness**
probe at the second. Pointing liveness at the dependency check restarts every
replica when the database blips, which turns a degradation into an outage. See
[Observability](observability.md#health-checks).

## Which build is running

```rust
router.get("/health/version", || async { Response::json(&build_info!()) });
```

Pass the commit into the build and the answer stops being "read the pipeline
backwards":

```dockerfile
ARG GIT_SHA
ENV GIT_SHA=$GIT_SHA
RUN cargo build --release --locked
```

```sh
docker build --build-arg GIT_SHA="$(git rev-parse HEAD)" .
```

`GITHUB_SHA` is already set inside a GitHub Actions build, so a CI image gets
it with nothing added. See [`build_info!()`](helpers.md#build_info).

## Timeouts and compression

```env
SERVER_REQUEST_TIMEOUT=30      # 0 is off, and is the default
SERVER_COMPRESSION=false       # true when Rainier is what clients talk to
```

A handler that never returns holds its connection and its task for as long as
the process lives, and enough of those is a service that has stopped answering
everything with nothing in the log about the endpoint that hung. Thirty seconds
is a reasonable first answer for an API; a route that legitimately takes longer
should carry its own [`Timeout`](middleware.md#timeout) rather than this being
raised for everything.

Leave compression off if nginx or a CDN is already doing it — compressing twice
is CPU spent to produce the same bytes.

## Graceful shutdown

```rust
let (tx, rx) = tokio::sync::watch::channel(false);
tokio::spawn(async move {
    tokio::signal::ctrl_c().await.ok();
    tx.send(true).ok();
});

Server::from_arc(kernel).run_until(rx).await?;
```

For the worker, `worker.stop()` asks it to finish the current job and exit —
which is what makes a deploy not lose work in flight.

## A reverse proxy

There is no `public/` and no document root: `serve` **is** the server. Put
nginx, Caddy or a CDN in front of it for TLS, static assets, and connection
handling, and forward everything else.

```nginx
location / {
    proxy_pass http://127.0.0.1:8000;
    proxy_set_header Host $host;
    proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
    proxy_set_header X-Forwarded-Proto $scheme;
}
```

With that in place, and only then, set `trust_proxy`.

## A container

The sample project ships a working [`Dockerfile`][dockerfile]; this is what is
in it and why.

```dockerfile
FROM rust:1.88-bookworm AS builder
WORKDIR /build

# Dependencies in their own layer: manifests, a stub main, then the real source.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src \
 && echo 'fn main() {}' > src/main.rs \
 && echo '' > src/lib.rs \
 && cargo build --release --locked \
 && rm -rf src

COPY src ./src
COPY resources ./resources
RUN touch src/main.rs src/lib.rs \
 && cargo build --release --locked \
 && strip target/release/app

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
 && apt-get install --no-install-recommends -y ca-certificates curl \
 && rm -rf /var/lib/apt/lists/*
RUN useradd --system --create-home --uid 10001 rainier
WORKDIR /app

COPY --from=builder /build/target/release/app /usr/local/bin/app
COPY --from=builder /build/resources ./resources
RUN mkdir -p storage/logs storage/mail storage/app /data \
 && chown -R rainier:rainier /app /data
USER rainier

ENV SERVER_HOST=0.0.0.0 SERVER_PORT=8000 APP_ENV=production RUST_LOG=info
EXPOSE 8000

HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD curl --fail --silent "http://127.0.0.1:${SERVER_PORT}/health" || exit 1

ENTRYPOINT ["/usr/local/bin/app"]
CMD ["serve"]
```

Five decisions worth the words:

**`SERVER_HOST=0.0.0.0`.** The default is `127.0.0.1`, which is unreachable
from outside the container. This is the single most common way a containerised
Rainier app appears to start and then answers nothing.

**The stub build.** Copying the manifests and compiling a `fn main() {}` first
puts every dependency in a layer that a change to `src/` does not invalidate —
the difference between a twenty-second rebuild and a five-minute one. The
`touch` afterwards is load-bearing: the stub left cargo's fingerprint newer
than the sources that replaced it, and cargo would otherwise ship the stub.

**Debian slim, not distroless or Alpine.** Slim keeps the glibc the build
linked against and the CA bundle an outbound HTTPS call needs. Alpine means
musl and a second toolchain; distroless means no shell, which is a real cost
the first time something is wrong in production.

**Not root.** A container escape is one bug away from being a host compromise,
and nothing here needs a privileged port.

**The healthcheck curls `/health`.** Running `app route:list` instead — which
looks tempting, since it needs no extra package — boots a *second copy of the
application* every thirty seconds, opening a second database pool, and still
says nothing about whether the socket is accepting.

`/health` is a plain 200 with no database behind it, which is what a **liveness**
probe should ask: "is this process serving?", not "is every dependency up?". A
readiness probe that checks the database belongs in the orchestrator, where it
can take an instance out of rotation without restarting it.

### `.dockerignore`

```
/target
/.cargo
.env
*.sqlite
```

`/target` because it is tens of gigabytes of artifacts for the wrong platform.
`/.cargo` because that is where a local `config.toml` patches the framework to
a sibling checkout that does not exist inside the container — git ignores it,
Docker has to be told separately, and a build with it present cannot resolve
the dependency.

Do not bake `.env` into the image. Inject the real environment —
[it wins over the file](configuration.md#env).

[dockerfile]: https://github.com/safewords/rainier-sample-project/blob/main/Dockerfile

## Continuous integration

The sample ships [a pipeline][ci]. The parts worth copying are the ones that
assert **behaviour** rather than compilation:

```yaml
- name: migrate, refuse a rollback, migrate again
  env:
    DATABASE_URL: "sqlite://ci.sqlite?mode=rwc"
  run: |
    set -euo pipefail
    cargo run --quiet -- migrate | tee first.txt
    grep -q "0001_create_users" first.txt

    if cargo run --quiet -- migrate:rollback; then
      echo "::error::rollback should have refused the irreversible step"
      exit 1
    fi

    cargo run --quiet -- migrate | tee second.txt
    grep -q "Nothing to migrate" second.txt
```

A real file rather than `sqlite::memory:`, because the point is the ledger
surviving across three processes and an in-memory database dies with its
connection.

```yaml
- run: cargo run --quiet -- route:list
```

The cheapest smoke test there is: `route:list` compiles the router, which
[builds every middleware](middleware.md#middleware-that-needs-the-container) —
so it fails if a group resolves a service no provider binds.

Two more worth having:

- **Every feature combination.** A `#[cfg]`-gated driver arm is not compiled by
  a default build, so `--features redis` can stay broken indefinitely without
  anything noticing. Matrix the features you ship.
- **A scheduled run.** If the framework is a git dependency, `main` moving
  changes your build with no commit of yours to trigger CI. Weekly is enough to
  find out before a deploy does.

[ci]: https://github.com/safewords/rainier-sample-project/blob/main/.github/workflows/ci.yml

## What to run

Three process types, from one binary:

```sh
app serve                  # the web server
app queue:work             # one or more workers
app schedule:work          # the scheduler, if you have one and no cron
app migrate                # once per deploy
```

`schedule:work` (and `schedule:run`) **refuse to start in production** when a
task declares `without_overlapping` or `on_one_server` and the cache is
per-process — every machine would take its own lock, get it, and run. `serve`
and `queue:work` are unaffected: they log the same complaint and carry on,
because refusing to serve HTTP over a scheduling concern would be the larger
outage. See [Scheduling](scheduling.md#the-framework-checks-this-for-you).

## Before you go live

- [ ] `APP_DEBUG=false`
- [ ] a real `DATABASE_URL`, migrations applied
- [ ] `QUEUE_DRIVER=database` and a worker running
- [ ] a real mail transport, `always_to` **unset** in production and **set** in staging
- [ ] `APP_URL` correct, or [generated absolute URLs](urls.md#absolute-urls) point at localhost
- [ ] `trust_proxy` only if you are behind one
- [ ] `SERVER_MAX_BODY` sized for your endpoints
- [ ] `SERVER_REQUEST_TIMEOUT` set, and larger than your slowest legitimate route
- [ ] `CACHE_DRIVER` shared if anything schedules `on_one_server` — `schedule:run` refuses otherwise
- [ ] `LOG_FORMAT` producing what your aggregator parses
- [ ] a commit in the build (`GIT_SHA`), so `/health/version` can answer
- [ ] liveness and readiness probes pointed at **different** endpoints
- [ ] `CACHE_DRIVER` shared if any route rate-limits credentials — otherwise the limit is `n × replicas`
- [ ] `APP_CIPHER` matching what wrote the rows already in the database
- [ ] secrets injected by the platform, not read from a committed file
- [ ] `cargo test` and `cargo clippy --workspace --all-targets` clean
