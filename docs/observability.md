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
