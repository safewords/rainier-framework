# Observability

Three optional things, each with its own section in `config/` — Prometheus
metrics, an OpenAPI document, and OpenTelemetry tracing — plus [logs](#logs)
and [health checks](#health-checks), which are not optional and only need
deciding about.

```env
METRICS_ENABLED=true
OPENAPI_ENABLED=true
TELEMETRY_ENABLED=true
OTEL_EXPORTER_OTLP_ENDPOINT=http://collector:4317
LOG_FORMAT=auto
```

---

## All three are off by default

Not out of caution about performance, though each does cost a request
something. Because **each exposes something**:

- a scrape endpoint tells whoever reads it your traffic shape, your error rate
  and every route you serve;
- an OpenAPI document is a complete map of your API, including every field and
  every constraint;
- a trace id on a response is a small thing to hand a client, and it is still
  something you should decide to hand them.

So each is turned on in `config/`, on a path you have thought about, rather
than being there because the framework thought it would be helpful.

[Logging](#logs) is the exception, and it is not really a fourth of these: an
application logs whether or not anyone configured it to, so the only question
is what shape the lines are.

---

## Metrics

```rust
// config/metrics.rs
config.set(METRICS_ENABLED, env.bool("METRICS_ENABLED", false))?;
config.set(METRICS_PATH, env.string("METRICS_PATH", "/metrics"))?;
```

`RecordMetrics` gives you three:

| Metric | Type | Labels |
|---|---|---|
| `http_requests_total` | counter | method, route, status |
| `http_request_duration_seconds` | histogram | method, route |
| `http_requests_in_flight` | gauge | — |

Record your own alongside them:

```rust
let metrics = resolve::<Metrics>()?;
metrics.counter("posts_published_total", "Posts published");
metrics.increment("posts_published_total", labels([("author", "…")]));
```

### Cardinality is the thing to be careful about

Every distinct label set is a series Prometheus stores forever. A label holding
a user id, a request id or a raw URL multiplies your storage by however many of
those exist — the classic way to take down a monitoring system with a one-line
change.

Two decisions here follow from that:

**The route label is the pattern.** `/posts/{post}`, never `/posts/1`. A
thousand posts are one series.

**An unmatched request is labelled `<unmatched>`.** The path of a 404 is chosen
by whoever sent it, so labelling by it would let anyone mint unbounded series
by making up URLs.

### Where to put the middleware

**In a group's stack, not the global one.** The router attaches the matched
route just before a route's own pipeline runs, so a group-level middleware can
read the pattern and a global one cannot — it runs before anything is matched.

```rust
pub fn api(metrics: Option<Arc<Metrics>>) -> MiddlewareStack {
    let stack = MiddlewareStack::new();
    let stack = match metrics {
        Some(metrics) => stack.with(RecordMetrics::new(metrics)),
        None => stack,
    };
    stack.with(ThrottleRequests::per_minute(60))
}
```

Put it **first** in that stack. The rate limiter can answer `429` without
calling `next`, and timing it from outside is the only way that request is
counted at all.

Placed globally it still works; every series is simply labelled `<unmatched>`.

### Guard the endpoint

The sample serves `/api/metrics` unguarded because it is a sample. A real
deployment puts it behind whatever its admin routes are behind, or binds it to
an interface only the scraper can reach. `METRICS_PATH` exists so you can also
move it somewhere unguessable when neither is available.

### No client library

The text exposition format is a dozen rules — a `# HELP`, a `# TYPE`, one line
per series, cumulative buckets and a required `+Inf`. Writing it is cheaper
than a dependency tree and keeps the crate compilable for wasm. What that costs:
no exemplars, no native histograms, no protobuf. A scrape needs none of them.

---

## OpenAPI

```rust
// config/openapi.rs
config.set(OPENAPI_ENABLED, env.bool("OPENAPI_ENABLED", false))?;
config.set(OPENAPI_TITLE, "Rainier Sample API".to_string())?;
config.set(OPENAPI_VERSION, "1.0.0".to_string())?;
```

### Half generated, half declared

**Generated** from the compiled router: every path, every method, the path
parameters, and a `401` plus a security scheme on anything behind
authentication. That half cannot drift — it is read from the routes being
served.

**Declared** in `routes/openapi.rs`: the summary, the tags, the request body
and the responses.

```rust
.describe(
    "api.posts.store",
    Endpoint::new()
        .summary("Create a draft")
        .tag("Posts")
        .accepts(StorePostRequest::rules())
        .returns(201, "The created post"),
)
```

Rust erases a handler's parameter types by the time the router holds one, so
there is nothing to introspect. Guessing would produce a document that is
confidently wrong, which is worse than one that is plainly incomplete.

### The request body comes from the validator

This is the part worth having. `accepts(StorePostRequest::rules())` hands the
document **the same `RuleSet` the validator runs**, so the schema cannot
describe a body the endpoint would reject:

| Rule | Schema |
|---|---|
| `Required` | listed in `required` |
| `String` + `Between(3, 120)` | `minLength: 3`, `maxLength: 120` |
| `Integer` + `Between(18, 120)` | `minimum: 18`, `maximum: 120` |
| `Array` + `Max(5)` | `maxItems: 5` |
| `Email` / `Url` / `Uuid` / `Date` | the matching `format` |
| `In([...])` | `enum` |
| `StartsWith` / `Slug` / `Alpha` | an anchored `pattern` |

Note rows two and three: the same rule means length for text and value for a
number, and getting that backwards produces a document clients act on and are
wrong about.

A rule no schema can express — `Confirmed`, `Same`, a custom predicate — is
skipped rather than approximated.

### Renaming a route orphans its description

Descriptions point at a route by **name**, and that is the one way this rots.
`dangling()` reports the orphans, and asserting on it is a one-line test:

```rust
#[test]
fn every_description_points_at_a_route_that_exists() {
    assert_eq!(document().dangling(&router()), Vec::<String>::new());
}
```

### No UI

Swagger UI and Redoc are large JavaScript bundles; serving one means vendoring a
megabyte or loading it from a CDN — a third party on your documentation page.
Point either at `/openapi.json`; both take a URL.

---

## OpenTelemetry

```rust
// config/telemetry.rs
config.set(TELEMETRY_ENABLED, env.bool("TELEMETRY_ENABLED", false))?;
config.set(TELEMETRY_SERVICE_NAME, env.string("OTEL_SERVICE_NAME", "rainier-sample"))?;
config.set(TELEMETRY_SAMPLE_RATIO, env.float("OTEL_TRACES_SAMPLER_ARG", 1.0))?;
```

The standard `OTEL_*` variables, so a deployment that already sets them needs
no Rainier-specific ones.

### Propagation is most of the value, and it is free

With no collector at all, `Trace` reads the W3C `traceparent` header, joins the
trace it names, puts the id on every log line emitted while handling the
request, and echoes it back on the response.

That is enough to follow one request across four services with `grep`, and it
costs no dependency and no infrastructure. Turn it on well before you have
somewhere to send spans.

```text
traceparent: 00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01
             ^  ^                                ^                ^
             │  trace id                         parent span      flags
             version
```

A missing or malformed header starts a fresh trace rather than failing the
request — an upstream with a misconfigured proxy must not stop this service
tracing.

### Put it outermost

```rust
pub fn register(registry: &MiddlewareRegistry, trace: Option<Trace>) {
    if let Some(trace) = trace {
        registry.global(trace);
    }
    registry.global(RequestIdMiddleware::new());
}
```

Everything logged inside it inherits the trace id, including from middleware
that rejects the request — which are the lines you most want to find later.

### Sampling belongs upstream

A trace that arrives with a decision keeps it, whatever this service is
configured to do. `TELEMETRY_SAMPLE_RATIO` applies only to traces this service
**starts**.

A trace sampled in half its services is a trace with holes, and a hole is
indistinguishable from a call that never happened — which is worse than no
trace at all.

### Exporting

```toml
rainier-telemetry = { version = "0.1", features = ["otlp"] }
```

```rust
tracing_subscriber::registry()
    .with(tracing_subscriber::fmt::layer())
    .with(Otlp::new(endpoint, "rainier-sample").sample_ratio(0.1).layer()?)
    .init();
```

Behind a feature because it is a large dependency tree. Spans are **batched** —
a span per network round trip would add the collector's latency to every
request, which is how tracing gets switched off in production. Call
`telemetry::shutdown()` before exiting, or the last few seconds never leave.

An endpoint configured without the feature built in logs a warning at boot
rather than silently exporting nothing.

With `rainier-framework`'s `telemetry` feature none of this is needed — see
[Passive instrumentation](#passive-instrumentation).

---

## Passive instrumentation

### Quick start

**1. Build with the feature.**

```toml
rainier-framework = { version = "2", features = ["telemetry"] }
```

**2. Set two variables.** No code: at boot the framework installs the OTLP
exporter beside its log subscriber and puts `Trace` outermost in the global
middleware.

```env
TELEMETRY_ENABLED=true
OTEL_EXPORTER_OTLP_ENDPOINT=http://otel-collector:4317

# Optional — the OpenTelemetry SDK's own names.
OTEL_SERVICE_NAME=orders-api
OTEL_TRACES_SAMPLER_ARG=0.25      # of traces this service starts
```

**3. Let workers be scraped.** A `queue:work` process has no HTTP server, so
give it one route for its metrics — the registry it serves is the
application's `Metrics`, bound as for the web process.

```env
WORKER_METRICS_ADDRESS=0.0.0.0:9100
```

**4. Run a collector.** It receives OTLP on 4317 and forwards to whatever
stores traces — Tempo and Jaeger both accept OTLP directly:

```yaml
# otel-collector.yaml
receivers:
  otlp:
    protocols:
      grpc:
        endpoint: 0.0.0.0:4317

processors:
  batch: {}

exporters:
  otlp/tempo:
    endpoint: tempo:4317
    tls: { insecure: true }
  otlp/jaeger:
    endpoint: jaeger:4317
    tls: { insecure: true }

service:
  pipelines:
    traces:
      receivers: [otlp]
      processors: [batch]
      exporters: [otlp/tempo, otlp/jaeger]   # either, or both
```

and point Prometheus at the web process's metrics path and each worker's
`WORKER_METRICS_ADDRESS`:

```yaml
scrape_configs:
  - job_name: orders-api
    metrics_path: /metrics
    static_configs: [{ targets: ["orders-api:8080"] }]
  - job_name: orders-worker
    static_configs: [{ targets: ["orders-worker:9100"] }]
```

**5. Ask the questions.**

```promql
# How long jobs wait before a worker picks them up — p95, per queue.
histogram_quantile(0.95,
  sum by (queue, le) (rate(queue_job_wait_seconds_bucket[5m])))

# What fraction of jobs fail or time out, per job type.
sum by (job) (rate(queue_jobs_total{outcome=~"failed|timed_out"}[5m]))
  /
sum by (job) (rate(queue_jobs_total[5m]))

# The backlog, and whether it is growing.
max by (queue) (queue_depth)
deriv(max by (queue) (queue_depth)[15m:1m]) > 0

# Scheduled tasks turned away because the last run was still going.
sum by (task) (increase(scheduler_task_runs_total{outcome="skipped_still_running"}[1h]))
```

**See it without any of that.** The `telemetry` example boots an
application, serves one request that reads the cache, runs a query, calls
another service and queues a job, processes the job, and prints every span
and the queue metrics — no collector, no network:

```sh
cargo run -p rainier-framework --example telemetry --features telemetry
```

```text
    cache.get  cache.hit=false cache.operation=get cache.store=memory …
    db.query  db.operation.name=SELECT db.collection.name=orders db.query.text=SELECT id FROM orders WHERE id = ? …
    http.client.request  server.address=carrier.example.com url.full=https://carrier.example.com/v1/rates …
    queue.publish  messaging.destination.name=default rainier.job.name=orders.ship …
  http.server.request  http.route=/orders/{order} http.response.status_code=200 trace_id=7098af80… …
  [carrier] traceparent: 00-7098af80ed1c524fa32c207113cd192b-391aa6aff6b114f0-01
  [queue] same trace as the request: true
    queue.deserialize …
    queue.handle …
    queue.ack …
  queue.process  messaging.queue.wait_ms=253 rainier.job.outcome=processed …
  queue_jobs_total{job="orders.ship",outcome="processed",queue="default"} 1
```

### How it fits together

With the `telemetry` feature every component opens
its own spans at its real seams, and at boot the framework installs the OTLP
exporter beside the log subscriber and puts `Trace` outermost in the global
stack — no `config/telemetry.rs`, no layer to compose, no middleware to
register. An application that already does any of those keeps what it did:
a `telemetry.*` value set in configuration wins over the environment, a
subscriber installed by the application is left alone (exporter and all), and
a `Trace` it registered is not joined by a second.

The variables are the OpenTelemetry SDK's own — `OTEL_EXPORTER_OTLP_ENDPOINT`,
`OTEL_SERVICE_NAME`, `OTEL_TRACES_SAMPLER_ARG` — so a deployment configured for
any other OpenTelemetry SDK is configured for this one.
`cargo rainier features` turns the feature on for an environment that sets
`TELEMETRY_ENABLED=true`.

### Off is free

Each crate has a `telemetry` feature of its own, and `rainier-framework`'s
turns them all on. With it off, **no span is compiled** — not skipped at run
time, absent from the binary. With it on, a span costs what a `tracing` span
costs, and the levels are chosen so that an idle process makes none: the two
operations that run on a timer whether or not there is work — a worker's
empty poll, a socket's individual message — are at `DEBUG`, below the default
`info` filter.

### The catalogue

Every span declares `otel.status_code` and `error.type`. A failure sets the
first to `ERROR` and the second to a stable, low-cardinality kind
(`internal`, `not_found`, `db_error`, a status code). **Never the error's
message** — a driver's message is the one place a bound value leaks back out
("Duplicate entry 'ada@example.com'…"), and a span goes to a system with other
readers and other retention than the log line the message is still on.

| Component | Span | Attributes | Deliberately not recorded |
|---|---|---|---|
| server | `http.server.request` | `http.request.method`, `url.path`, `http.route`, `rainier.route.name`, `http.response.status_code`, `user_agent.original`, `trace_id`; named `GET /posts/{post}` | the query string; a 4xx is not an error |
| routing | records onto the server span | the matched **pattern**, never the path | — |
| outbound HTTP | `http.client.request` (one per attempt) | `http.request.method`, `server.address`, `server.port`, `url.scheme`, `url.full`, `http.response.status_code` | the query string and any `user:pass@`; bodies; headers |
| database | `db.query` | `db.system.name`, `db.operation.name`, `db.collection.name`, `db.query.text`, `db.response.returned_rows`; named `SELECT users` | bound values; literals in the text, which is sanitised to `?` |
| cache | `cache.get` / `put` / `forget` / `has` / `add` / `increment` / `forget_if` / `flush` | `cache.store`, `cache.operation`, `cache.hit` | the key; the value |
| storage | `storage.get` / `put` / `read` / `delete` / `list` / `copy` / `rename` / `sign` / … | `storage.driver`, `storage.operation`, `storage.bytes` | the path; the contents |
| queue, dispatch | `queue.publish` | `messaging.system`, `messaging.destination.name`, `messaging.message.id`, `rainier.job.name` | the payload |
| queue, worker | see [below](#queues) | | the payload |
| mail | `mail.send` | `mail.transport`, `mail.recipients` (a count), `mail.attachments` (a count) | addresses; the subject; the body |
| notifications | `notification.send` | `rainier.notification.name`, `rainier.notification.channels` | the recipient; the content |
| events | `event.dispatch` — only when something listens | `rainier.event.name`, `rainier.event.listeners` | the event |
| broadcasting | `broadcast.publish` | `rainier.broadcast.driver`, `rainier.broadcast.event`, `rainier.broadcast.channels` (a count) | channel names (`private-user.42`); the payload |
| websockets | `websocket.connect`; `websocket.message` at `DEBUG` | `rainier.websocket.kind`, `rainier.websocket.bytes` | the socket's path; message contents |
| scheduler | `schedule.task` | `rainier.task.name`, `rainier.task.outcome`, `rainier.task.skipped` (why) | — |
| console | `console.command` | `rainier.command.name`, `process.exit.code` | the arguments, which is where a console takes a password |
| sessions | `session.load`, `session.save` | `rainier.session.store`, `rainier.session.found` | the session id, which is a bearer credential |
| authentication | `auth.attempt` | `rainier.auth.guard`, `rainier.auth.outcome` | the credentials; who — a failed attempt's username is as often somebody else's |
| views | `view.render` | `rainier.view.name` | the data the template was given |

### The trace crosses every boundary

- **Into the process.** The server span adopts an incoming `traceparent` as
  its parent, so this service's spans hang off the caller's.
- **Out over HTTP.** Every outbound request — through the `Http` facade, a mail
  API, a payment API, anything that reaches a transport — carries a
  `traceparent` naming its own client span. A caller that set one keeps it.
- **Through the queue.** A dispatch writes the `traceparent` in force onto the
  job's envelope, and the worker's `queue.process` span adopts it: a job run
  ten minutes later in another process is part of the request that queued it.
  Anything the job calls joins the same trace.

Without the exporter there are no span ids to cite, and what travels is the
trace id the request arrived with plus an id this service mints for itself —
one level shallower, the same trace, which is what keeps `grep <trace id>`
working across services and queues with no collector at all.

### Queues

A slow job has four possible causes and a span per job cannot tell them apart.
So a job is a tree:

```text
queue.process          the whole job; wait, reserve time, outcome, attempt
├── queue.deserialize  decoding the payload into its type
├── queue.handle       the handler (or queue.kubernetes, for a pod)
└── queue.ack          or queue.release / queue.fail — the driver settling it
```

`queue.process` carries `messaging.system`, `messaging.destination.name`,
`messaging.message.id`, `rainier.job.name`, `rainier.job.attempt`,
`rainier.job.max_attempts`, `rainier.job.runner` (`in_process` or
`kubernetes`), `rainier.queue.reserve_ms` (the driver round-trip that handed
this job over), `messaging.queue.wait_ms`, `rainier.job.outcome` and, for a
release, `rainier.job.backoff_ms`. The reserve itself is a `queue.reserve` span
at `DEBUG`: an idle worker polls several times a second, and a span per empty
poll would bury the jobs.

**Wait time is measured from `available_at`, not from the push.** A job delayed
by ten minutes was not waiting for ten minutes, and a released job was not
waiting during its backoff. Every driver already writes the moment a job
becomes runnable onto its envelope, so the wait is `reserved − available_at` —
and it is on every envelope ever written, including the ones sitting in a
queue across the deploy that turned this on. One exception: SQS releases a
message by changing its visibility without rewriting it, so a retried SQS
job's wait includes the backoff and the attempt before it. The database driver
stores jobs as columns and has none for `traceparent`, so its jobs start
traces of their own; their timings are unaffected.

**Outcomes** are `processed`, `released`, `failed`, `timed_out` (a timeout,
whether or not attempts remain — the fix is a different limit, not a code
change) and `error` (the driver could not settle the job; it will be
redelivered).

With a `Metrics` registry bound, `queue:work` records:

| Metric | Type | Labels |
|---|---|---|
| `queue_job_duration_seconds` | histogram | queue, job, outcome |
| `queue_job_wait_seconds` | histogram, to two hours | queue, job |
| `queue_reserve_duration_seconds` | histogram | queue |
| `queue_jobs_total` | counter | queue, job, outcome |
| `queue_jobs_in_flight` | gauge | queue |
| `queue_depth` | gauge, sampled | queue |
| `queue_pod_start_seconds` | histogram — creating the Kubernetes Job to its container starting | queue, job |
| `queue_pod_run_seconds` | histogram — the container's run | queue, job |

`job` is the job's **type name**, never its id, so the series count is fixed
by the code rather than by traffic.

**Depth is sampled, never per job.** Each worker reads `Queue::size` for each
of its queues on its own task every fifteen seconds
(`Worker::sample_depth_every`). What that costs depends on the driver:

| Driver | A sample is |
|---|---|
| Redis | `XLEN` + `ZCARD` — the ready stream and the delayed set |
| SQS | one `ApproximateNumberOfMessages` read |
| database | one `COUNT` of unreserved rows |
| Kafka | the end offset and the consumer's cursor per partition — its lag |
| memory | a length |

Every replica samples, so a backlog is read `replicas × queues` times per
interval; lengthen the interval, or pass `None`, if that matters for yours.

**Pod timings** come from one extra API call per Kubernetes-dispatched job — a
pod list once the Job is terminal and before it is deleted — made only with
the feature on. A job that timed out is deleted on the spot and reports none.

**A worker has no HTTP server**, so nothing would scrape what it records. Set
`WORKER_METRICS_ADDRESS=0.0.0.0:9100` and `queue:work` serves the registry at
`GET /metrics` on that address, and nothing else.

### The scheduler

With a registry bound, `schedule:run` and `schedule:work` record
`scheduler_task_runs_total` and `scheduler_task_duration_seconds`, labelled by
task and outcome — `ran`, `failed`, or `skipped_still_running`,
`skipped_another_server`, `skipped_condition_false`. The first of those skips
is the overlap guard turning a run away: the number to alert on when a task
has started taking longer than its interval.

### What it does not instrument

**Individual middleware.** One span per stage on every request is a dozen spans
that each take microseconds; the server span already covers them, and a slow
middleware shows up as the gap before the route's work starts.

**Below the `Database` handle.** A query is traced where every query passes —
connection pooling and the drivers' own internals are not.

**Socket paths and channel names.** Both carry identifiers.

---

## Logs

The one piece of observability that is **not** off by default, because an
application logs whether or not anyone configured it to.

```env
LOG_FORMAT=auto        # auto | pretty | compact | json
RUST_LOG=info
```

| | |
|---|---|
| `auto` | JSON in production and staging, `pretty` everywhere else |
| `pretty` | coloured, one event per line — `tracing_subscriber`'s default |
| `compact` | denser and still readable, for `docker logs` and CI |
| `json` | one object per line, for anything that indexes fields |

The choice is really one question: *is a human reading this, or a log
aggregator?* A human wants colour and alignment. Datadog, Loki and CloudWatch
want one JSON object per line and nothing else, and they will happily index a
pretty-printed line as an unparseable blob with the fields no longer fields.

Getting it wrong is quiet in exactly the way that costs an incident: the logs
are *there*, they simply cannot be searched by the field you need at the moment
you need it. Which is why `auto` is the default and why it is right far more
often than either fixed answer.

Staging is included in "production-shaped" deliberately. It exists to rehearse
production, and one whose logs are searched differently cannot rehearse an
incident.

```json
{"timestamp":"2026-07-25T09:14:00Z","level":"INFO","message":"request","status":200,"route":"/api/posts"}
```

Fields are **flattened to the top level** rather than nested under `fields`,
because that is where every aggregator's default parser looks for them; nested,
they are one query deeper forever.

`LOG_FORMAT` outside its four values is refused at boot, like a driver name —
`LOG_FORMAT=jsn` would otherwise log prose into something that wanted objects
and say nothing about it.

Installing it yourself, if the bootstrap is not doing it for you:

```rust
let settings = TelemetrySettings::from_config(&config);
settings.install_logging(&app.environment(), &env.string("RUST_LOG", "info"));
```

It returns whether it installed one. `false` means a subscriber was already set
— by the application, or by an earlier boot in the same test binary — and that
one is left alone, because replacing somebody else's logging configuration is
worse than not installing yours.

### With tracing on, every line carries the trace id

Which is the combination worth having: structured lines, and an id that ties
the ones from four services into a single request.

## Health checks

```rust
// In a provider.
app.instance(
    Health::new()
        .register("database", |app| async move {
            app.resolve::<Database>()?.query("SELECT 1").execute().await?;
            Ok(())
        })
        .register("cache", |app| async move {
            app.resolve::<CacheManager>()?.store().has("health").await?;
            Ok(())
        })
        .describing_build(build_info!()),
);

// And a route.
router.get("/health/ready", health::endpoint).name("health.ready");
```

```json
{
  "status": "ok",
  "checks": {
    "cache":    { "status": "ok", "duration_ms": 1 },
    "database": { "status": "ok", "duration_ms": 3 }
  },
  "build": { "name": "identity", "version": "2.4.1", "commit": "9f3c2ab", "profile": "release" }
}
```

Roughly the same three hundred lines in every service anybody deploys, and they
diverge in the ways that cost an outage: one returns `200` while its database
is unreachable, another has no timeout and hangs the probe, a third reports a
version somebody hardcoded.

A check receives the application, so it can resolve what it needs to prove.
That is the point — a check that does not touch the dependency is a check that
reports the dependency is fine when it is not.

### Liveness and readiness are different questions

| | Asks | Answering it wrong |
|---|---|---|
| **liveness** | is this process running? | a restart loop |
| **readiness** | can it serve a request? | traffic to a replica that cannot |

A liveness probe that checks the database restarts **every replica** when the
database blips — turning a degradation into an outage, and removing the
processes that would have recovered.

So `health::endpoint` is readiness, and a liveness route should be two lines
that do no I/O:

```rust
router.get("/health", || async { Response::json(&json!({ "status": "ok" })) });
```

### `503`, not `500`

A dependency being down means "try another replica, try again shortly", which
is what a load balancer and a retrying client both need to hear. A `500` says
this service is broken, which invites a different and slower response from
whoever is paged.

### Every check has a deadline

A probe that hangs is **worse** than one that fails: the orchestrator waits,
decides nothing, and the replica sits in exactly the unknown state the probe
existed to resolve. Each check runs under `timeout` — five seconds by default —
and one that overruns is a failed check with a reason.

Checks run **concurrently**, so six that take a second each take a second. A
probe has a deadline of its own, and a slow report is a failed one.

Each runs in its own task, so a **panicking check becomes one failed check**
rather than a probe that returns nothing. A check is somebody else's closure
and one of them will panic eventually.

### It says when nothing is registered

An endpoint with an empty registry answers `503` and says so, rather than
reporting healthy — which is what "every check passed" would otherwise mean
when there are no checks.

### The build belongs in the same document

The first two questions of an incident are "is it up" and "which one is it".
`describing_build(build_info!())` puts the answer to the second next to the
first. See [`build_info!()`](helpers.md#build_info).

## What is not here


**No log rotation or file sinks.** `tracing_subscriber` writes to stdout, and
the thing that collects stdout is the platform's — Docker, systemd, a sidecar.
An application that writes its own log files is an application that also has to
rotate them.

**No metrics push gateway.** `Metrics::render()` is a `String`; posting it on a
timer is a [scheduled task](scheduling.md).

**No profiling or continuous profiling.** Different tool, different problem.
