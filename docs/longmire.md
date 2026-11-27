# Longmire

Longmire is one view across many [Lookouts](lookout.md). Each service's
Lookout records its own work; Longmire, in the one application that can reach
all of them — an operations console — asks every one at once for its part of
a request, and links the parts into one tree: each service's work under the
call that reached it.

Lookouts never talk to one another. Each records its own side and serves it;
Longmire is where the reports come together.

```mermaid
flowchart LR
    subgraph services
      A[orders<br/>Lookout] ~~~ B[billing<br/>Lookout] ~~~ C[identity<br/>Lookout]
    end
    L[console<br/>Longmire] -- "GET /api/traces/{trace}" --> A
    L -- "GET /api/traces/{trace}" --> B
    L -- "GET /api/traces/{trace}" --> C
```

## What it needs from each service

Lookout's [request tracing](lookout.md#traces-across-services) on
(`LOOKOUT_REQUEST_TRACING=true`), which turns on
[request tagging](observability.md#request-tagging), and a way for that
Lookout to recognise Longmire: [the link](#the-link) (preferred), or a Lookout
[reader](lookout.md#readers-another-service) that accepts the console's
credentials. Request tracing records, on each side, the span id every call put
on the wire: the caller's batch as the call's `call.span_id`, the callee's as
its `remote_parent_span_id`. That shared id is all Longmire links on.

## Turning it on

Build with the `longmire` feature (it brings the HTTP client's real
transport), and bind it in the console: which Lookouts, and how to sign in to
each.

```rust
use rainier_framework::longmire::{Longmire, Source};

let longmire = Longmire::new([
    Source::new("orders", "http://orders.internal"),
    Source::new("billing", "http://billing.internal").path("/ops/lookout").name("Billing"),
])
// The bearer token each Lookout's reader accepts — a service token, typically.
.credentials(|source| {
    let audience = source.id.clone();
    Box::pin(async move { service_tokens().for_audience(&audience).await })
});
```

Sources can come from configuration as JSON — `Longmire::from_json(&env)` over
`[{"id", "name"?, "url", "path"?}]`. `path` is where the service mounts
Lookout, `/_lookout` by default. A source with no id or no http(s) URL, or a
repeated id, is dropped. A credentials callback that answers `None` leaves
that source out of the answer, named, rather than calling it unsigned.

## Reading

| Call | Answers |
|---|---|
| `longmire.trace(id)` | `{trace_id, started_at, services, batches, unanswered}` — every source's part, linked |
| `longmire.recent(&RecentQuery { kept, before, limit })` | `{services, traces}` — the latest traces that crossed services, one row per trace, newest first, at most 50 |

The id is a trace id as Lookout keys it, or a UUID as a frontend that started
the trace with [`REQUEST_TAGGING_HEADER`](observability.md#request-tagging)
shows it — dashes and all.

Every source is asked at once, each within eight seconds (`.timeout(…)`). In
`services`, each source is `ok` with how many parts it gave, or `unknown` with
the reason — it did not answer, it answered 404 (request tracing off, no
reader, or credentials it does not take), or it refused them.

### How the parts are linked

In `batches`, every batch carries its source (`service`, `service_name`) and
is placed under the outbound call whose `call.span_id` equals its
`remote_parent_span_id`; a batch with no such call is a root. They come in
tree order — each followed by the ones under it, both by start time — with a
`depth`, and `parent` naming the call (`{call_span_id, span_id}`). A batch's
`spans` are its own: type, summary, start, duration, failed, slow, and for an
outbound call its `call` (`span_id`, `host`, `status`). Never content: that
never leaves a Lookout.

`unanswered` lists the calls that carried the trace and that no batch here
answered: into a service that let its part go, one that is not a source, or
another company's API.

```text
orders    POST /checkout 502 ──┬─ query …
                               └─ POST billing/charges 500   call.span_id = S1
billing   POST /charges 500       remote_parent_span_id = S1 ─┘
unanswered: POST api.payments.example/v1/charges (from billing)
```

## The fleet

Besides traces, Longmire answers for the whole fleet at once, from the
rollups every Lookout keeps — which count every request, call and page view,
kept or not, so the numbers are the real ones:

| Call | Answers |
|---|---|
| `longmire.health(query)` | every source's `status` (`ok`, `degraded`, `failing`, `idle`, `unknown` with the `reason`) and its `health`, plus how many are in each status |
| `longmire.requests(query)` | requests by route, method and status class across every source, busiest first, each row naming its `service`; each source's `totals` |
| `longmire.queues(query)` | every job queue across every source — depth, concurrency against slots, throughput, waits, a status — deepest backlog first, each row naming its `service`; each source's `totals`, and the `fleet`'s added up (backlog, attempts, failures, running, slots, utilisation, statuses). See [Queues](lookout.md#queues) |
| `longmire.rum_vitals(query)` / `rum_errors(query)` | real users' page metrics and script errors across every frontend |
| `longmire.cases(query)` | exceptions across every source, most frequent first |
| `longmire.map(query)` | who calls whom — below |
| `longmire.one(source, Endpoint::…, query)` | one source's answer as its Lookout gave it, for a page about one service |

`query` is a query string; only each Lookout endpoint's own parameters are
forwarded (`since`, `until`, `period`, and the endpoint's filters and order —
see [service health](lookout.md#service-health)), each once, none longer than
200 characters, still encoded. Every source is asked at once.

### The service map

`map` is the fleet as a graph over the window (`since`, `until`):

```mermaid
flowchart LR
    orders((orders<br/>ok)) -- "1.7/min · 10% errors · p95 9 ms" --> billing((billing<br/>failing))
    billing -- "0.5/min · p95 200 ms" --> pay[api.payments.example]
```

- **Nodes**: every source, with its status, requests a minute, error rate and
  p95; and every host a source calls that is not a source, as `external`.
- **Edges**: every caller and callee, with its calls, calls a minute, `errors`
  (5xx answers and calls that got no answer at all), `client_errors` (4xx),
  `error_rate`, mean latency and a p95 (the worst status class's).

Edges come from each Lookout's `call_facet` rollup: every outbound call
counted by the host it went to and its status class. A host is a source when
it is the host of the source's URL or one of its `hosts` — the other names a
service is called by (`Source::new(…).host("billing.example.com")`, or
`"hosts": [...]` in JSON).

## The link

Longmire and Lookout have a link of their own, so that nothing but Longmire
can read a Lookout this way and nothing on the network learns what it said.
Both ends hold a **link key** — an id and a secret of at least 32 bytes,
shared by the fleet:

```env
# On every service
LOOKOUT_LONGMIRE_KEYS=2026-11:base64-secret        # comma-separate two to rotate
```

```rust
// In the console
let longmire = Longmire::from_json(&sources)?
    .link(LinkKey::parse(&env.string("LONGMIRE_KEY", ""))?);
```

- **Every request is signed**: HMAC-SHA256 over the method, the path and
  query, a timestamp and a fresh nonce, in `x-lookout-link`.
- **Lookout recognises it** only when the key is one it holds, the signature
  matches (compared in constant time), the timestamp is within a minute of
  its clock, and it has not seen the nonce — a request captured on the way
  cannot be sent again. Anything else gets the same 404 as a path that does
  not exist: nothing says the link is there.
- **The answer is sealed**: AES-256-GCM under a key derived for sealing, the
  request's nonce authenticated with it, so it can be read only by the
  holder of the key, only as the answer to that request, and not altered on
  the way. Longmire refuses an answer that is not sealed.
- The secret never crosses the wire, and the signing and sealing keys are
  derived from it separately (HKDF-SHA256).

The link reaches what a reader may read — health, requests, RUM, cases,
aggregates and, with request tracing on, traces — and nothing else. With a
link, Longmire sends no other credentials. The protocol is
`rainier-lookout-link`, which both ends use.

## Routes

To serve the same answers as JSON, mount `longmire::routes` with a check of
your own over who may read; anyone it refuses gets the router's 404.

```rust
let router = rainier_framework::longmire::routes(
    Arc::new(longmire),
    Arc::new(|request| {
        let staff = is_staff(request);
        Box::pin(async move { staff })
    }),
);
app_router.group(GroupAttributes::new().prefix("/ops/longmire"), |r| r.merge(router));
```

`GET /traces` (`kept`, `before`, `limit`), `GET /traces/{trace}`, `GET
/health`, `/requests`, `/queues`, `/rum/vitals`, `/rum/errors`, `/cases` and `/map`, each
answering `{"data": …}`, and `GET /sources/{source}/{health|requests|queues|rum/vitals|rum/errors|cases}`
answering one source's own answer (a `502` with the reason when it has none);
a malformed trace id is a `400`. An application with
its own response envelope or access rules can call `trace` and `recent` from
its own handlers instead.

## Limits

- Longmire keeps nothing: each answer is assembled from what the Lookouts
  hold now, so a trace is there as long as each service keeps its batches.
- A part a service did not keep is missing; the call into it is still in its
  caller's part, and in `unanswered`.
- The sources' clocks are their own: start times across services are only as
  close as the services' clocks.
