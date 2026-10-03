# Kubernetes jobs

Dispatch a `Job` as a one-shot `batch/v1 Job` pod on Kubernetes instead of
as a queue message.

Behind the `kubernetes` feature on `rainier-framework`.

```toml
rainier-framework = { ..., features = ["kubernetes"] }
```

## When to use this

The ordinary queue path is the right default. A fixed pool of workers
drains thousands of small deferred actions cheaply, and spinning a pod per
message would add 5–15 s cold-start to every one and overwhelm the
Kubernetes API server. **Don't opt in by default.**

Opt in when a job's resource profile would be a problem on a shared worker:

- **It needs materially more memory than ordinary jobs.** The example this
  mechanism was built for: an uploader that reads a multi-gigabyte file
  into memory to post it to an external API. On a general worker pod
  sized for ordinary traffic, one of those pushes past the memory limit
  and OOM-kills the pod, taking out every unrelated job it was draining.
  A one-shot pod with memory matched to the file is bounded, exits
  cleanly, and never shared RAM with anything else.
- **It handles sensitive content that must stay isolated.** CSAM review,
  payment-card tokens, PII cross-border transfer. Pair the one-shot pod
  with a narrowly-scoped `ServiceAccount` (IRSA), a tainted node pool
  (`nodeSelector`), and `emptyDir { medium: Memory }` for tempfiles — none
  of which the shared pool offers.
- **Its CPU shape is spiky and would starve neighbors.** A one-off model
  run, a video thumbnail that briefly saturates the box, a compression
  pass that would make every other job in the pool miss its SLO.
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
| `.with_image(..)` | Container image. Default: the dispatcher's own image (via `RAINIER_K8S_DEFAULT_IMAGE`). |
| `.with_service_account(..)` | `spec.template.spec.serviceAccountName`. **Always set this for sensitive-content jobs.** |
| `.with_namespace(..)` | Namespace to create the Job in. Default: the dispatcher's own namespace. |
| `.with_node_selector(k, v)` | Chainable; pins the pod to tainted nodes. |
| `.with_ttl_seconds_after_finished(n)` | How long the finished Job + its logs linger before GC. Default `60`; CSAM pods should set `0`. |
| `.with_tmpfs_scratch()` | Mounts `/scratch` as `emptyDir { medium: Memory }`. For pods whose tempfiles must never touch disk. |

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

## The pod wire protocol

When the dispatcher creates a Job for your opt-in job, it:

1. Serializes the `Job` struct to JSON.
2. Creates a `batch/v1 Job` resource whose pod container runs the
   current image, with:
   - `args: ["--rainier-run-single-job"]` (the sentinel from
     `rainier_queue::SINGLE_JOB_SUBCOMMAND`)
   - `env.RAINIER_JOB_NAME = J::NAME`
   - `env.RAINIER_JOB_PAYLOAD = <serialized JSON>`
   - resources, nodeSelector, serviceAccount, tmpfs from the spec

The payload rides in an env var, which has a per-pod 1 MiB cap on
Kubernetes. The dispatcher rejects payloads over ~900 KiB. The intended
use is passing identifiers (match ids, upload ids — tens of bytes); for
larger payloads the dispatcher would need a ConfigMap-based variant,
which is not implemented yet.

## Application plumbing — two pieces

The dispatch call sites don't change. `Queue::instance().dispatch(job)`
transparently routes a Kubernetes-eligible job (one whose `kubernetes()`
returns `Some`) to the dispatcher when one is bound, and to the queue
otherwise. A reader at the call site doesn't need to know which path
this job takes — the job's own `impl Job` block is the single source of
truth.

### 1. Bind the dispatcher at boot

```rust
use rainier_framework::queue::KubernetesDispatcher;

match KubernetesDispatcher::try_in_cluster().await {
    Ok(Some(dispatcher)) => {
        tracing::info!(
            namespace = dispatcher.default_namespace(),
            "KubernetesDispatcher bound"
        );

        // Optional but recommended: a boot-time RBAC check.
        // SelfSubjectAccessReview asks the API server what the current
        // pod's SA is allowed to do with `batch/v1 Jobs`; it does not
        // actually create anything. Cheap, and catches a misconfigured
        // cluster at startup rather than at the first dispatch.
        match dispatcher.verify_rbac().await {
            Ok(report) if report.is_dispatch_ready() => {
                tracing::info!(summary = %report.summary(), "k8s RBAC check ok");
            }
            Ok(report) => {
                // Choose your strictness: warn and continue (the
                // dispatcher will fail at the first dispatch), or
                // return Err here to hard-fail boot.
                tracing::warn!(
                    summary = %report.summary(),
                    "k8s dispatcher bound but current SA cannot `create jobs.batch` — \
                     opt-in Kubernetes jobs will error on dispatch; the ordinary queue \
                     path is unaffected"
                );
            }
            Err(e) => tracing::warn!(
                error = %e.message(),
                "k8s RBAC check failed to reach the API; opt-in Kubernetes dispatch \
                 may error at use"
            ),
        }

        builder = builder.with_instance_arc(Arc::new(dispatcher));
    }
    Ok(None) => tracing::debug!("not in a cluster; k8s jobs fall back to the queue"),
    Err(e) => tracing::warn!(error = %e.message(), "k8s dispatcher unavailable"),
}
```

`try_in_cluster` returns `Ok(None)` outside Kubernetes (`KUBERNETES_SERVICE_HOST`
unset), so local dev, CI, and non-k8s deployments work unchanged — the
dispatcher is just not bound and every job takes the queue path.

#### What `verify_rbac` checks (and doesn't)

The check runs `SelfSubjectAccessReview` for three verbs against
`batch/v1 Jobs` in the dispatcher's default namespace:

| Verb | Required for | Returned on |
|---|---|---|
| `create` | Any Kubernetes dispatch at all | `RbacReport.can_create` |
| `get` | Reading a single Job's status | `RbacReport.can_get` |
| `list` | Enumerating outstanding Jobs | `RbacReport.can_list` |

`is_dispatch_ready()` is sugar for `can_create` — the only hard
requirement. The others are optional and only matter if the
application grows a Jobs admin surface.

**What it can't check** is the per-job `ServiceAccount` named on each
`KubernetesJobSpec`. That's a different identity from the dispatcher's,
and SSAR can't impersonate without the dispatcher itself having
`impersonate` on `serviceaccounts` (which it almost certainly doesn't
and shouldn't). A dispatch to a job that references a missing SA will
fail at pod creation with a sharper error than SSAR could offer; the
warn-log at that dispatch is the right surface.

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
        run_single_job, JobContext, JobRegistry, JOB_NAME_ENV, PAYLOAD_ENV,
    };

    let name = std::env::var(JOB_NAME_ENV).expect("RAINIER_JOB_NAME set by dispatcher");
    let payload: serde_json::Value = serde_json::from_str(
        &std::env::var(PAYLOAD_ENV).expect("RAINIER_JOB_PAYLOAD set by dispatcher"),
    ).expect("valid JSON");

    let registry = app.container().resolve::<JobRegistry>().unwrap();
    let context = Arc::new(JobContext::new(
        app.container().clone(),
        "k8s-pod".into(),
        "kubernetes".into(),
        1, 1,
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
error, which Kubernetes retries up to `backoffLimit` — default 1 since
the queue's retry is the primary mechanism).

### 3. Dispatch sites don't change

```rust
Queue::instance().dispatch(job).await?;              // or
Queue::instance().dispatch_on("notifications", job).await?;
```

That's the whole dispatch surface. `QueueManager::dispatch` and
`dispatch_on` both route internally: if the feature is enabled, the
job declared a spec, and a dispatcher is bound in the facade
container, the job runs as a one-shot pod. Otherwise it rides the
queue. A reader doesn't branch here; a reviewer sees the dispatch
once at the trait impl.

`dispatch_after(delay, job)` is the exception: `batch/v1 Jobs` have
no delayed-start primitive, so a k8s-eligible job dispatched this way
logs a warn and takes the queue path (where the delay is actually
honored). Wrap it in a scheduled job if you need both.

For code that holds a `KubernetesDispatcher` directly (test harnesses,
custom runners that bypass the facade), two lower-level primitives
remain available:

- `KubernetesDispatcher::dispatch(&job)` — unconditional k8s dispatch
  of a job that declared a spec.
- `KubernetesDispatcher::dispatch_or_queue(Some(&k8s), &queue, job)` —
  the raw router. The `QueueLike` trait is a one-method shim your
  queue type implements. `QueueManager::dispatch` is this primitive
  internally.

## Infrastructure prerequisites

The dispatcher doesn't configure your cluster — only you can decide what
`ServiceAccount`, `Role`, `RoleBinding`, and node pool are right for the
work. For a job to actually run on Kubernetes you need:

- **RBAC for the dispatcher pod itself.** The pod that creates `batch/v1 Jobs`
  needs a `Role` with `create` on `jobs.batch` in the target namespace.
- **A `ServiceAccount` per sensitive job.** CSAM upload, payment handling,
  etc. — a narrow IRSA role scoped to just the S3 buckets / external
  endpoints the job touches. The spec names it; the SA has to exist.
- **Nodes with the taint the spec selects.** `nodeSelector: role=sensitive-content`
  pins the pod to nodes with the matching label (plus a matching
  toleration if those nodes are tainted — add via a `tolerations` field
  on the spec if your deployment needs one; the current prototype only
  carries `nodeSelector`).
- **Image availability.** The dispatcher runs your current image by
  default (via `RAINIER_K8S_DEFAULT_IMAGE`, which the deployment should
  set from `fieldRef: metadata.annotations['kubernetes.io/image']` or
  equivalent). The target namespace needs pull access to that image.
- **DownwardAPI on the dispatcher pod.** `KUBERNETES_NAMESPACE` needs to
  be set via DownwardAPI so the dispatcher knows what namespace to
  create Jobs in. Without it the dispatcher logs a warning and falls
  back to `"default"` — which is almost certainly wrong.

## Known limitations

- **Env-var payload cap, ~900 KiB.** Larger payloads need a
  ConfigMap-based variant (not implemented). The intended use case is
  identifiers; shrink the payload rather than growing it.
- **Pod cold-start latency.** 5–15 s between dispatch and the job
  starting to run, depending on image pull cache and node scheduling.
  Jobs with sub-second time budgets belong on the queue.
- **Kubernetes API rate limits.** The dispatcher creates one Job per
  dispatch. A sudden flood (10k matches arriving at once) would get
  throttled. The ordinary queue buffers naturally; the k8s path does
  not. For high-volume opt-in jobs, pair with a KEDA-style ScaledJob
  operator rather than this direct-dispatch mode.
- **No feedback loop.** `dispatcher.dispatch(..)` returns `Ok(())` as
  soon as the API accepts the Job resource — not when the pod starts,
  not when the job's `handle` returns. Observability has to come from
  Kubernetes (`Job.status`, pod logs) and your own tracing, not from
  the dispatcher's return value.
- **No retry mapping.** `Job::TRIES` is honored by the pod-side runner
  (via the `JobRegistry::run` path), but the pod itself can only be
  retried by Kubernetes according to `backoffLimit`. The default spec's
  `backoffLimit = 1` makes the queue path's own retry (if the job also
  rides the queue path) the primary mechanism; raise it only if you
  understand which retry story applies in each failure mode.

## When *not* to opt in

Flip of the "when to use this" list:

- Jobs that complete in under a second — the pod cold-start dominates.
- Jobs that are dispatched at rates over ~1/s — API rate-limits bite.
- Jobs where every retry would hurt (unique external side effects,
  idempotency not well-established) — Kubernetes may retry the pod
  underneath you.
- Jobs where the simpler answer is a bigger shared worker — if your
  queue has a single hot job that's eating memory, upsize the pool
  before reaching for one-shot pods.

The ordinary queue is the right answer most of the time. Kubernetes
dispatch is for the specific shape where its three affordances —
per-job sizing, narrow IAM, strong isolation — are worth the three
costs — cold start, API load, retry-story complexity. A few jobs per
application, not all of them.
