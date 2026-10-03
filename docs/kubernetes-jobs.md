# Kubernetes jobs

Run a reserved queue job as a one-shot `batch/v1 Job` pod on Kubernetes
instead of in the worker's own process.

Behind the `kubernetes` feature on `rainier-framework`.

```toml
rainier-framework = { ..., features = ["kubernetes"] }
```

## The queue is still the authority

**Dispatch is unchanged.** `Queue::instance().dispatch(job)` writes to
the ordinary queue backend regardless of whether this job will eventually
run on Kubernetes. That means every job that opts in still has:

- a `jobs` row (or Redis/SQS message) the moment it is dispatched;
- the queue's own retries via [`Job::TRIES`] and [`WorkerOptions::tries`];
- the queue's per-queue timeouts and per-queue concurrency ceilings;
- `failed_jobs` on terminal failure, with the same shape every other job's
  failure takes;
- `QueueManager::fake()` recording the dispatch for tests, with no
  Kubernetes machinery involved.

What changes is **who executes `handle()`**. The queue worker reserves
the job as usual, then — if the job's `kubernetes()` returns `Some` AND
the worker has a `KubernetesDispatcher` configured — launches a one-shot
`batch/v1 Job` pod, watches it to a terminal condition, and translates
the result back into the worker's own success/failure outcome. The
worker's concurrency slot is held for the pod's lifetime, so per-queue
caps still apply.

A `Complete` Job returns `Ok(())`, the queue row is acknowledged. A
`Failed` Job returns `Err` carrying the pod's log tail, which the
worker's existing retry logic treats like any other failure — released
for retry, then moved to `failed_jobs` on the final attempt.

## When to use this

The ordinary queue path is the right default. A fixed pool of workers
drains thousands of small deferred actions cheaply, and spinning a pod
per message would add 5–15 s cold-start to every one and overwhelm the
Kubernetes API server. **Don't opt in by default.**

Opt in when a job's resource profile would be a problem on a shared
worker:

- **It needs materially more memory than ordinary jobs.** The example
  this mechanism was built for: an uploader that reads a multi-gigabyte
  file into memory to post it to an external API. On a general worker
  pod sized for ordinary traffic, one of those pushes past the memory
  limit and OOM-kills the pod, taking out every unrelated job it was
  draining. A one-shot pod sized to the file is bounded, exits cleanly,
  and never shares RAM with anything else.
- **It handles sensitive content that must stay isolated.** CSAM review,
  payment-card tokens, PII cross-border transfer. Pair the one-shot pod
  with a narrowly-scoped `ServiceAccount` (IRSA), a tainted node pool
  (`nodeSelector`), and `emptyDir { medium: Memory }` for tempfiles —
  none of which the shared pool offers.
- **Its CPU shape is spiky and would starve neighbors.** A one-off
  model run, a video thumbnail that briefly saturates the box, a
  compression pass that would make every other job in the pool miss
  its SLO.
- **It can tolerate 5–15 s of pod cold-start.** This is the hard trade:
  what you buy in isolation and sizing you pay in startup time. A job
  that should begin within milliseconds of being queued belongs on the
  queue.

Rule of thumb: **a few jobs per application, not all of them.**

## How a job opts in

Implement `Job::kubernetes`:

```rust
use rainier_framework::prelude::*;
use rainier_framework::queue::KubernetesJobSpec;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadConfirmedCsamToNcmec {
    pub report_case_id: u64,
    pub post_id: u64,
}

#[async_trait]
impl Job for UploadConfirmedCsamToNcmec {
    const NAME: &'static str = "ncmec.upload-confirmed-csam";
    const QUEUE: &'static str = "notifications";
    const TRIES: u32 = 5;

    fn kubernetes(&self) -> Option<KubernetesJobSpec> {
        Some(
            KubernetesJobSpec::new("4Gi", "6Gi", "250m", "1")
                .with_service_account("ncmec-uploader-sa")
                .with_node_selector("role", "sensitive-content")
                .with_tmpfs_scratch()
                .with_ttl_seconds_after_finished(0),
        )
    }

    async fn handle(&self, context: &JobContext) -> Result<()> {
        /* reads the asset into memory and uploads it */
        Ok(())
    }
}
```

The default `Job::kubernetes` returns `None`. Every existing `impl Job`
compiles unchanged; opting in is one method.

### What the spec covers

`KubernetesJobSpec::new(memory_request, memory_limit, cpu_request, cpu_limit)`
takes the four quantities in Kubernetes format (`"4Gi"`, `"250m"`, etc.)
and leaves everything else at a safe default. The builder methods layer
specifics on top:

| Method | What it sets on the pod |
|---|---|
| `.with_image(..)` | Container image. Default: the worker's own image (via `RAINIER_K8S_DEFAULT_IMAGE`). |
| `.with_service_account(..)` | `spec.template.spec.serviceAccountName`. **Always set this for sensitive-content jobs.** |
| `.with_namespace(..)` | Namespace to create the Job in. Default: the worker's own namespace. |
| `.with_node_selector(k, v)` | Chainable; pins the pod to tainted nodes. |
| `.with_ttl_seconds_after_finished(n)` | How long the finished Job + its logs linger before GC. Default `60`; CSAM pods should set `0`. |
| `.with_tmpfs_scratch()` | Mounts `/scratch` as `emptyDir { medium: Memory }`. For pods whose tempfiles must never touch disk. |
| `.with_backoff_limit(n)` | Kubernetes-side retries. **Default `0`.** Retries live in the queue worker — raising this gives Kubernetes a second attempt at the pod before `Job::TRIES` is consulted, which is almost never what you want. |

### Resource sizing

The pod will be scheduled to a node with enough `requests` to fit, and
killed if it exceeds `limits`. Both should reflect the real max of the
job across a representative payload distribution; a tight limit means
one bad input OOM-kills the pod, a loose one wastes cluster capacity.

For a job that holds the whole file in memory, size memory to the 99th
percentile file + a few hundred megabytes of request/response + runtime
overhead. For a job that streams, memory stays small regardless of input
size and can be a flat request.

CPU is a request, not a hard limit for most workloads — the pod gets at
least `requests.cpu` and can burst to `limits.cpu`. Set requests to the
steady-state CPU need (typical: 100m–500m for I/O-bound jobs, higher for
compute-bound).

## How the worker drives it

1. Worker reserves a job from the queue (ordinary path — `queue.reserve`,
   nothing k8s-aware).
2. Worker asks the registry: "does this job's current payload return
   `Some(spec)` from `kubernetes()`?" The registry stores a per-type
   extractor set up when `register::<J>()` runs, so this is one hashmap
   lookup + one `serde_json::from_value` per reserved job.
3. If `None` or the dispatcher is absent → the handler runs in-process,
   same as before.
4. If `Some(spec)` and a `KubernetesDispatcher` is wired to the worker
   → the worker calls `KubernetesDispatcher::run_and_watch(..)`, which:
   - computes a deterministic Job name from `(J::NAME, queue_row_id,
     attempt)` — DNS-1123 safe, under 63 chars;
   - POSTs a `batch/v1 Job` to the API with that name, resources,
     service account, node selector, tmpfs, and env (job name, payload,
     queue row id, attempt);
   - polls the Job's status until it reaches `Complete` or `Failed`,
     subject to the worker's effective timeout for this job (the usual
     four-level resolution: operator override → `Job::TIMEOUT` →
     queue timeout → worker default);
   - on `Complete` → returns `Ok(())`;
   - on `Failed` → fetches the pod's log tail and returns `Err`
     carrying it;
   - on timeout → deletes the Job and returns an `Err`;
   - on 409 Conflict at create (reattach — see next section) → skips
     straight to the watch loop.
5. The worker then runs its normal success/failure/retry/`failed_jobs`
   logic against that `Result<()>`. From the queue's point of view, a
   failed Kubernetes Job is indistinguishable from a failing in-process
   handler.

### Reattach on worker restart

The Kubernetes Job's `metadata.name` is derived deterministically from
`(J::NAME, queue_row_id, attempt)`. If a worker crashes while watching
a Job and another worker reserves the same queue row (after the
backend's `retry_after`), the second worker computes the same name,
tries to create the Job, gets a `409 AlreadyExists`, and jumps straight
to the watch loop. No duplicate pod; the uniqueness is enforced by the
API server's own primary key.

This matters for anything with external side effects — an NCMEC upload
you do not want to make twice.

### Labels on every Job

For observability + ad-hoc queries, the dispatcher stamps:

| Label | Value |
|---|---|
| `app.kubernetes.io/managed-by` | `rainier-queue` |
| `rainier.job/name` | slug of `J::NAME` (`ncmec-upload-confirmed-csam`) |
| `rainier.job/queued-id` | the queue row's id (slugified) |
| `rainier.job/instance` | the derived `metadata.name` |

`kubectl get jobs -l app.kubernetes.io/managed-by=rainier-queue` enumerates
every outstanding one.

## The pod wire protocol

When the worker launches a Job, the pod container runs the application's
own image with:

- `args: ["--rainier-run-single-job"]` (`rainier_queue::SINGLE_JOB_SUBCOMMAND`)
- `env.RAINIER_JOB_NAME = J::NAME`
- `env.RAINIER_JOB_PAYLOAD = <serialized JSON>`
- `env.RAINIER_QUEUED_JOB_ID = <queue row id>`
- `env.RAINIER_JOB_ATTEMPT = <attempt number>`
- resources, nodeSelector, serviceAccount, tmpfs, labels from the spec

The payload rides in an env var, which has a per-pod 1 MiB cap on
Kubernetes. The dispatcher rejects payloads over ~900 KiB. The intended
use is passing identifiers (match ids, upload ids — tens of bytes); for
larger payloads a ConfigMap-based variant would be needed (not
implemented).

## Application plumbing — two pieces

### 1. Build and attach the dispatcher on the worker

The dispatcher is a property of the pod that runs `queue:work`. The
HTTP tier (octane/axum/whatever) does not need it — HTTP never launches
Kubernetes Jobs under this design; it dispatches to the queue as usual
and the worker is the one that executes.

```rust
use std::sync::Arc;
use rainier_framework::queue::{KubernetesDispatcher, Worker};

async fn build_worker(/* ... */) -> Result<Worker> {
    let mut worker = Worker::new(queue, registry, container)
        .with_options(worker_options);

    #[cfg(feature = "kubernetes")]
    match KubernetesDispatcher::try_in_cluster().await {
        Ok(Some(dispatcher)) => {
            tracing::info!(
                namespace = dispatcher.default_namespace(),
                "KubernetesDispatcher bound to worker"
            );

            // Optional but recommended: boot-time RBAC check.
            match dispatcher.verify_rbac().await {
                Ok(report) if report.is_dispatch_ready() => {
                    tracing::info!(summary = %report.summary(), "k8s RBAC check ok");
                }
                Ok(report) => tracing::warn!(
                    summary = %report.summary(),
                    "k8s dispatcher bound but current SA cannot create jobs — \
                     opt-in jobs will fail on reserve and release for retry; the ordinary \
                     queue path is unaffected"
                ),
                Err(e) => tracing::warn!(
                    error = %e.message(),
                    "k8s RBAC check failed to reach the API"
                ),
            }

            worker = worker.with_kubernetes(Arc::new(dispatcher));
        }
        Ok(None) => tracing::debug!("not in a cluster; k8s-opted jobs run in-process"),
        Err(e) => tracing::warn!(error = %e.message(), "k8s dispatcher unavailable"),
    }

    Ok(worker)
}
```

`try_in_cluster` returns `Ok(None)` outside Kubernetes
(`KUBERNETES_SERVICE_HOST` unset), so local dev, CI, and non-k8s
deployments work unchanged — the dispatcher is just not attached and
every job runs in the worker process.

#### What `verify_rbac` checks

The check runs `SelfSubjectAccessReview` against the worker's SA for
four verbs:

| Verb | On | Required for |
|---|---|---|
| `create` | `batch/jobs` | launching any Kubernetes Job |
| `get` | `batch/jobs` | polling the Job's status |
| `watch` | `batch/jobs` | a future informer-based watch |
| `get` | `pods/log` | capturing the pod's log tail on failure |

`is_dispatch_ready()` requires `create` + `get`. The log read is
optional — missing it means failed Jobs come back without the pod's
log tail, which is a diagnosis-quality bit, not a correctness one.

### 2. The CLI pre-check in `main`

The pod runs your binary with the sentinel arg. `main` has to catch it
**before** the application's own CLI parser runs:

```rust
#[tokio::main]
async fn main() -> Result<()> {
    let application = boot(Mode::Running).await?;
    let raw: Vec<String> = std::env::args().skip(1).collect();

    if raw.first().map(String::as_str)
        == Some(rainier_framework::queue::SINGLE_JOB_SUBCOMMAND)
    {
        let code = run_kubernetes_single_job(&application).await;
        application.terminate();
        std::process::exit(code);
    }

    // ... normal CLI dispatch ...
}

async fn run_kubernetes_single_job(app: &Application) -> i32 {
    use rainier_framework::queue::{
        run_single_job, JobContext, JobRegistry,
        JOB_NAME_ENV, PAYLOAD_ENV, QUEUED_ID_ENV, ATTEMPT_ENV,
    };

    let name = std::env::var(JOB_NAME_ENV).expect("RAINIER_JOB_NAME set by dispatcher");
    let payload: serde_json::Value = serde_json::from_str(
        &std::env::var(PAYLOAD_ENV).expect("RAINIER_JOB_PAYLOAD set by dispatcher"),
    ).expect("valid JSON");
    let queued_id = std::env::var(QUEUED_ID_ENV)
        .unwrap_or_else(|_| "k8s-pod".into());
    let attempt: u32 = std::env::var(ATTEMPT_ENV)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);

    let registry = app.container().resolve::<JobRegistry>().unwrap();
    let context = Arc::new(JobContext::new(
        app.container().clone(),
        queued_id,
        "kubernetes".into(),
        attempt,
        attempt, // this pod is one attempt; worker owns the retry count
    ));

    match run_single_job(&registry, &name, payload, context).await {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("job `{name}` failed: {}", e.message());
            1
        }
    }
}
```

The pod runs the single job to completion and exits 0 (or non-zero on
error). The worker watching on the orchestrator side translates that
exit code into `Ok(())` / `Err` and the queue handles the rest.

### 3. Dispatch sites don't change

```rust
Queue::instance().dispatch(job).await?;              // or
Queue::instance().dispatch_on("notifications", job).await?;
Queue::instance().dispatch_after(TEN_MIN, job).await?;
```

That's the whole dispatch surface. All three go to the queue backend;
whether the job eventually runs in-process or on a Kubernetes pod is
decided on reserve by the worker. A reviewer sees the dispatch once at
the trait impl; the call sites are the same they have always been.

## Infrastructure prerequisites

The dispatcher doesn't configure your cluster — only you can decide what
`ServiceAccount`, `Role`, `RoleBinding`, and node pool are right for the
work. For a job to actually run on Kubernetes you need:

- **RBAC on the WORKER pod's ServiceAccount.** The pod that runs
  `queue:work` needs a `Role` with `create get list watch` on
  `jobs.batch` plus `get` on `pods/log` in the target namespace. The
  HTTP pod does not — it only dispatches to the queue.
- **A `ServiceAccount` per sensitive job.** CSAM upload, payment
  handling, etc. — a narrow IRSA role scoped to just the S3 buckets /
  external endpoints the job touches. The spec names it; the SA has
  to exist.
- **Nodes with the taint the spec selects.** `nodeSelector:
  role=sensitive-content` pins the pod to nodes with the matching
  label (plus a matching toleration if those nodes are tainted — add
  via a `tolerations` field on the spec if your deployment needs one;
  the current prototype only carries `nodeSelector`).
- **Image availability.** The dispatcher runs the worker's current
  image by default (via `RAINIER_K8S_DEFAULT_IMAGE`, which the
  deployment should set — typically from a static env tracking the
  Deployment's `.spec.template.spec.containers[].image`). The target
  namespace needs pull access to that image.
- **DownwardAPI on the worker pod.** `KUBERNETES_NAMESPACE` from
  `fieldRef: metadata.namespace` so the dispatcher knows what namespace
  to create Jobs in. Without it the dispatcher logs a warning and
  falls back to `"default"` — which is almost certainly wrong.

## Known limitations

- **Env-var payload cap, ~900 KiB.** Larger payloads need a
  ConfigMap-based variant (not implemented). The intended use case is
  identifiers; shrink the payload rather than growing it.
- **Pod cold-start latency.** 5–15 s between reserve and the pod
  starting to run, depending on image pull cache and node scheduling.
  Jobs with sub-second time budgets belong in-process.
- **Kubernetes API rate limits.** The worker creates one Job per
  reserved opt-in row. A sudden flood (10k matches arriving at once)
  would be gated first by per-queue concurrency (`WorkerOptions::queue_limit`)
  and then by the API's rate limits. For high-volume opt-in jobs,
  pair with a KEDA-style ScaledJob operator rather than this
  worker-driven path.
- **The worker's slot is held for the pod's lifetime.** A long-running
  pod ties up one of the worker's concurrency slots. That is the
  mechanism's whole point — it makes per-queue limits apply equally
  to k8s jobs — but it means a pod that takes an hour costs an
  hour of a slot. Size the worker's concurrency accordingly.

## When *not* to opt in

Flip of the "when to use this" list:

- Jobs that complete in under a second — the pod cold-start dominates.
- Jobs that are dispatched at rates over ~1/s per worker replica — API
  rate-limits and slot pressure bite.
- Jobs where every retry would hurt (unique external side effects,
  idempotency not well-established) — reattach-on-worker-restart
  guards against duplicate launches within one attempt, but
  `Job::TRIES` means the worker will launch a fresh Job for each
  retry.
- Jobs where the simpler answer is a bigger shared worker — if your
  queue has a single hot job that's eating memory, upsize the pool
  before reaching for one-shot pods.

The ordinary queue is the right answer most of the time. Kubernetes
dispatch is for the specific shape where its three affordances —
per-job sizing, narrow IAM, strong isolation — are worth the three
costs: cold start, API load, slot occupancy. A few jobs per
application, not all of them.
