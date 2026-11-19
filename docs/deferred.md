# Deferred Work

Some work belongs to a request but should not hold its response: touching a
`last_seen_at`, bumping a view counter, warming a cache after a write. The
client is not waiting for it, and making it wait costs a round trip to
whatever the work talks to — a database primary that is slow today is slow
for every page that touches it.

`defer` runs that work in the background of the request:

```rust
use rainier_framework::prelude::*;

async fn show(request: Request) -> Result<Response> {
    let profile = current_profile(&request).await?;

    // Returns at once. The response does not wait for this.
    defer("profiles.touch_last_seen", async move {
        profiles::touch_last_seen(profile.id).await
    });

    Ok(render_profile(&profile))
}
```

The future may return `()` or any `Result<T, E>` whose `E: Display`. An `Err`
is logged; the `Ok` value is dropped, because there is nobody left to give it
to.

## What it does that `tokio::spawn` does not

| | `tokio::spawn` | `defer` |
|---|---|---|
| the request's span and trace id | lost | the task runs in a `deferred` span whose parent is the request's |
| facades (`DB::`, `Cache::`, …) | resolve through the process-wide application | resolve through the request's application |
| an `Err` | dropped unless you log it yourself | logged at `warn` with the task's name |
| a panic | logged by tokio, unnamed | caught, logged at `error` with the task's name |
| metrics | none | a count and a duration per name and outcome |
| a pod rolling | the task is dropped mid-write | `serve` waits for it, up to a bound |
| a slow dependency | tasks pile up without limit | past a ceiling new work is shed and counted |

## Naming

The first argument is a `&'static str` naming the **kind** of work —
`profiles.touch_last_seen`, `posts.count_view`. It is a metric label and a log
field, so it must never contain anything per call: an id in the name is one
time series per id.

## Not a queue

Deferred work lives in this process's memory. If the process dies, the work is
gone, and a failure is logged rather than retried. That is the trade:

| | `defer` | [a queued job](queues.md) |
|---|---|---|
| costs the request | nothing | a round trip to the queue backend |
| survives a crash | no | yes |
| retries | no | yes |
| runs | in this process, now | on a worker, when one is free |

Use `defer` for work that is cheap to lose and wrong to make a client wait
for. Anything that **must** happen — an email, a charge, a webhook — goes on a
queue.

## Shutdown

`serve` stops accepting connections on `SIGTERM`, then waits for in-flight
deferred work before the application's terminating hooks run. `queue:work`
does the same after its workers stop, for work a job deferred. The wait is
bounded: whatever is still running at the deadline is logged and abandoned.

Keep the bound inside your orchestrator's grace period — Kubernetes sends
`SIGKILL` 30 seconds after `SIGTERM` by default.

Running the server from your own `main` instead of `serve`, call the same
drain yourself:

```rust
Server::from_arc(kernel).run_until(rx).await?;
rainier_framework::deferred::drain_on_shutdown(&app).await;
```

## Configuration

| Key | Environment | Default | |
|---|---|---|---|
| `defer.max_in_flight` | `DEFER_MAX_IN_FLIGHT` | `1024` | tasks at once before new ones are shed; `0` is no ceiling |
| `defer.timeout_secs` | `DEFER_TIMEOUT` | `0` (off) | cancel a task that runs longer |
| `defer.shutdown_timeout_secs` | `DEFER_SHUTDOWN_TIMEOUT` | `10` | how long shutdown waits |

Past the ceiling, `defer` returns `Deferral::Shed`, logs a warning and counts
it, and the work does not run. It sheds rather than queueing because the
ceiling exists for the case where the work has become slow — a database that
has stopped answering — and holding it in memory is exactly the pile-up the
ceiling prevents. A timeout is worth setting alongside it: a task stuck on a
dead connection holds its slot until something cancels it.

An application that wants other settings binds its own before boot finishes:

```rust
use std::time::Duration;
use rainier_framework::deferred::Deferrer;

app.instance(
    Deferrer::new().max_in_flight(64).timeout(Duration::from_secs(5)),
);
```

## Metrics

With a `Metrics` registry bound when the application boots:

| Metric | Type | Labels |
|---|---|---|
| `deferred_tasks_total` | counter | name, outcome |
| `deferred_task_duration_seconds` | histogram | name, outcome |
| `deferred_tasks_in_flight` | gauge | — |

`outcome` is `completed`, `failed`, `panicked`, `timed_out` or `shed`. Shed
work is counted and not timed. Anything else that wants the numbers implements
`DeferObserver` and passes it to `Deferrer::with_observer`.

## Testing

`defer` needs no application. Without one bound it uses a process-wide
`Deferrer`, so code that defers runs unchanged in a unit test. To assert on
what the deferred work did, wait for it first:

```rust
use rainier_framework::deferred;

#[tokio::test]
async fn showing_a_profile_touches_last_seen() {
    let response = app.get("/profiles/ada").await;
    deferred::settle().await;

    assert!(last_seen_of("ada").await.is_some());
}
```

`settle` waits without a bound. A `Deferrer` you build yourself has its own
`settle`, `drain(timeout)` and `in_flight()`.
