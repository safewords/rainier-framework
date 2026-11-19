# Abandoned requests

A request is **abandoned** when its answer can no longer reach anybody:

- **the client hung up** — closed the tab, lost signal, gave up waiting;
- **a [`Timeout`](middleware.md#timeout) gave up on it** and answered `408`.

Each route decides, when it is declared, what happens to its work then:

| | |
|---|---|
| `OnAbandon::Finish` | The work runs on to the end, in a task of its own, and its response is discarded. |
| `OnAbandon::Cancel` | The work stops at its next `await`. Nothing after that point runs. |

The default is **finish when the client hangs up, cancel at a timeout**.

```rust
use rainier_framework::prelude::*;

// The default: nothing to write.
router.post("/posts", posts::store);

// A webhook: record what we were sent, however it ends.
router.post("/webhooks/stripe", stripe::handle).when_abandoned(AbandonPolicy::finish());

// A search nobody is waiting for is worth nothing.
router.get("/search", search::index).when_abandoned(AbandonPolicy::cancel());

// One half at a time; the other is inherited.
router.post("/import", import::run)
    .middleware(Timeout::seconds(60))
    .when_abandoned(AbandonPolicy::new().on_timeout(OnAbandon::Finish));
```

## Why it matters

A future in Rust is stopped by dropping it, and the drop happens at whichever
`await` the work was paused on. Code written top to bottom reads as if it always
gets to the bottom; a dropped future doesn't. For a handler that writes twice:

```rust
let chat = chats.create(chat).await?;          // committed
participants.create(owner_of(&chat)).await?;   // ← the client hangs up here
```

cancelling leaves a chat with no members, and nothing anywhere reports an error.
A query cut off mid-flight also leaves its database connection unusable; the
driver throws it away and opens another.

Finishing is the default on a hang-up because a closed tab is ordinary, the work
is nearly always short, and the cost of running it to the end is a few
milliseconds for nobody. Cancelling is the default at a timeout because a
request that slow is already in trouble. Cancelling it gives back its database
connection while the thing it was waiting on recovers. Finishing it would keep
the connection while the client retries, and under load that piles up.

## Neither is a substitute for a transaction

Finishing makes a half-done write rarer, not impossible: a process that is
killed — a deploy, an OOM, a node going away — stops everything mid-flight
regardless of policy. Writes that must land together belong in one
[transaction](database.md#transactions):

```rust
db.transaction(|tx| async move {
    let chat = EntityRepository::<Chat>::new(tx.clone()).create(chat).await?;
    EntityRepository::<ChatParticipant>::new(tx).create(owner_of(&chat)).await?;
    Ok(chat)
}).await?;
```

With that in place, cancelling is safe too, because a cancelled transaction
rolls back.

## Choosing

| The route… | Pick |
|---|---|
| reads, and the reader is gone | `cancel()`: the result is worthless |
| writes, in one statement or one transaction | the default; either is safe |
| writes in several steps that cannot be one transaction (a database write, then an upload, then another write) | `finish()` |
| is a webhook or a callback another service will not repeat | `finish()` |
| is expensive and can be repeated by the client | `cancel()` |
| waits for something to happen — a long poll, a lock, a slow upstream it only reads from | `cancel()`: finishing would keep waiting for nobody |
| may legitimately outlive its timeout, and the work matters more than the database pressure | `.on_timeout(OnAbandon::Finish)` |

Finishing at a timeout is the one to be careful with. Each timed-out request
keeps running after its `408`. If the client retries, the retry runs at the same
time as the original. Use it on a route whose work is bounded, not to rescue a
slow query.

## Where a policy comes from

The route's own choice comes first. Anything it leaves unset comes from its
[group](routing.md#groups), and then from `AbandonPolicy::DEFAULT`:

```rust
router.group(GroupAttributes::new().prefix("webhooks").when_abandoned(AbandonPolicy::finish()), |router| {
    router.post("/stripe", stripe::handle);
    router.post("/github", github::handle);
});
```

A [resource controller](controllers.md#resource-controllers) can decide per
action:

```rust
impl ResourceController for PostController {
    fn when_abandoned(&self, action: ResourceAction) -> AbandonPolicy {
        match action {
            ResourceAction::Store | ResourceAction::Destroy => AbandonPolicy::finish(),
            _ => AbandonPolicy::new(),
        }
    }
}
```

## Stopping early on your own terms

A long handler can watch for the signal and stop at a point of its choosing,
whatever its policy says. `request.abandonment()` gives the signal. Clone it
before the request is consumed:

```rust
async fn export(request: Request) -> Response {
    let abandoned = request.abandonment();
    for batch in batches() {
        if abandoned.is_abandoned() {
            return Response::text("stopped");   // nobody will read this
        }
        write(batch).await;
    }
    Response::text("done")
}
```

`abandonment.reason()` says why, and `abandonment.abandoned().await` resolves
when it happens.

## How it works

The server runs each request's work in a task of its own and only waits on it.
When the client hangs up, hyper drops the waiting, and that drop marks the
request `ClientGone`. A route that cancels on hang-up races its work against
that mark, and drops the work when the mark comes first. Its response is then
`499`, the status proxies log for a client that went away. It is not reported as
an error.

`Timeout` marks the request `TimedOut` before it drops what it was waiting on.
A route that finishes at a timeout has already moved its work into a task of
its own, so the drop lets the task go instead of stopping it. If the work is
dropped and nothing marked the request, it is always cancelled. Work only keeps
running because a policy asked for it.

The task carries the facade application, the event bus's unit of work, and the
tracing span, so a handler cannot tell where it runs. Work that belongs to a
`sticky` [database scope](database.md) is the exception: that scope is the
caller's to enter, and a task does not inherit it.

Two limits:

- **Global middleware is outside the route.** A route that cancels on hang-up
  cancels its own middleware and handler. Global middleware that runs before
  routing (sessions, CORS, the timeout itself) runs to the end regardless.
- **Shutdown is not a policy.** When the process exits, every task stops,
  finished or not. A deploy is the one abandonment no policy covers, which is
  why the writes that matter go in a transaction.

On wasm there is no runtime to spawn on: the work runs in place and is
cancelled with its caller, whatever the policy.
