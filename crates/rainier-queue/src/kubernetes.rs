//! Run a reserved job in a Kubernetes `batch/v1 Job` pod instead of in
//! the worker process.
//!
//! The regular [`Queue`](crate::Queue) path is the right default: cheap,
//! batched, and perfect for the thousands of small deferred actions a web
//! application makes. A small number of jobs don't fit it, though — the
//! ones that need a dedicated pod sized for them. The example that
//! triggered this module: an NCMEC CyberTipline uploader that reads a
//! multi-gigabyte confirmed-CSAM video into memory and streams it to
//! NCMEC. On a general worker pod sized for the ordinary queue, one of
//! those blows past the memory limit and OOM-kills the worker, taking
//! out every unrelated job it was draining. On a one-shot pod with a
//! memory request matched to the file, it finishes and the pod dies.
//!
//! # The queue is still the authority
//!
//! Dispatching is **unchanged**: `QueueManager::dispatch` writes to the
//! ordinary queue backend regardless of whether the job will eventually
//! run on Kubernetes. `jobs` and `failed_jobs` records are produced the
//! same way, retries are owned by [`WorkerOptions::tries`](
//! crate::WorkerOptions::tries), and `QueueManager::fake` captures the
//! dispatch for tests without any Kubernetes machinery.
//!
//! What changes is who executes the handler. When the [`Worker`](
//! crate::Worker) reserves a job whose
//! [`crate::Job::kubernetes`] returns `Some(spec)`
//! **and** the worker has a [`KubernetesDispatcher`] configured, the
//! worker launches a one-shot `batch/v1 Job` from the spec, watches it,
//! and translates its terminal state back into its own
//! [`Outcome`](crate::Outcome):
//!
//! * `Complete` → `Ok(())` and the queue row is acknowledged.
//! * `Failed`   → `Err(…)` carrying the pod's last log line, which the
//!   worker's existing retry logic treats like any other failure —
//!   released for retry, then moved to `failed_jobs` on the final try.
//!
//! The worker's slot is held for the pod's lifetime, so the queue's own
//! per-queue concurrency ceilings still cap how many Kubernetes Jobs can
//! be in flight at once. The job's `backoff_limit` on the Kubernetes
//! side is deliberately `0` by default: retries are the queue's job.
//!
//! # Reattach on worker restart
//!
//! The one-shot Job's name is derived deterministically from the queue
//! job id and the attempt number. If a worker crashes while watching a
//! Job, another worker that reserves the same queue row (after
//! `retry_after`) computes the same name and reattaches to the existing
//! Job's watch rather than launching a duplicate pod. The underlying
//! guarantee is Kubernetes': `metadata.name` on a `batch/v1 Job` is a
//! unique key in the namespace, so a `create` with the same name returns
//! `AlreadyExists`.
//!
//! # Feature-gated
//!
//! The [`KubernetesJobSpec`] type is always available (every `Job` has a
//! [`kubernetes`](crate::Job::kubernetes) method, feature or no feature).
//! The [`KubernetesDispatcher`] and [`run_single_job`] helper live
//! behind the `kubernetes` feature so a consumer that doesn't need them
//! doesn't pull `kube-rs` + `k8s-openapi`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// What a job tells the dispatcher it needs when it runs on Kubernetes.
///
/// Resource strings follow the Kubernetes quantity format
/// (`"500m"`, `"2Gi"`, …). An application constructs this with
/// [`KubernetesJobSpec::new`] and fills whichever fields matter.
///
/// Image, service account, and namespace are left optional on the spec
/// so the dispatcher can fill them from its own configuration (image
/// typically from the current pod's `DownwardAPI`, namespace from
/// `KUBERNETES_NAMESPACE`). An explicit value on the spec always wins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KubernetesJobSpec {
    /// `resources.requests.memory` on the pod. Example: `"2Gi"`.
    pub memory_request: String,
    /// `resources.limits.memory` on the pod. Example: `"4Gi"`.
    pub memory_limit: String,
    /// `resources.requests.cpu` on the pod. Example: `"500m"`.
    pub cpu_request: String,
    /// `resources.limits.cpu` on the pod. Example: `"1"` (one vCPU).
    pub cpu_limit: String,

    /// Container image the pod runs. `None` → the dispatcher fills it
    /// from its own default (typically the current pod's image).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,

    /// `spec.template.spec.serviceAccountName`. `None` → inherit the
    /// dispatcher's default service account (ordinarily `default`).
    /// A CSAM-handling job wants its own narrowly-scoped SA.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_account: Option<String>,

    /// Namespace to create the Job in. `None` → the dispatcher's own
    /// namespace (from `KUBERNETES_NAMESPACE`, else `"default"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,

    /// `spec.template.spec.nodeSelector`. Lets a job pin itself to
    /// sensitive-content nodes with the appropriate taints and
    /// isolation. Empty by default.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub node_selector: BTreeMap<String, String>,

    /// `spec.ttlSecondsAfterFinished`. How long the completed `Job`
    /// resource (and its logs) linger before Kubernetes garbage-collects
    /// it. `0` reaps immediately; `60` keeps it for a minute in case an
    /// operator wants to look. Default `60`.
    #[serde(default = "default_ttl_seconds_after_finished")]
    pub ttl_seconds_after_finished: i32,

    /// `spec.backoffLimit` — how many times Kubernetes retries a failed
    /// pod before marking the Job failed.
    ///
    /// **Default `0`.** Retries are the queue worker's responsibility
    /// ([`crate::Job::TRIES`], released + reserved again), so a pod failure
    /// should fail the Job once and come straight back to the worker.
    /// Raising this to `1` or above gives Kubernetes a second attempt
    /// at the pod before the worker ever sees the failure, which is
    /// usually not what the application wants.
    #[serde(default = "default_backoff_limit")]
    pub backoff_limit: i32,

    /// Enables an `emptyDir { medium: Memory }` volume mounted at
    /// `/scratch` for CSAM-handling and other temporary-content work —
    /// anything the pod writes there lives in tmpfs and dies with the
    /// pod. Default `false` because most Kubernetes-dispatched jobs
    /// don't need it; a CSAM uploader does.
    #[serde(default)]
    pub tmpfs_scratch: bool,
}

const fn default_ttl_seconds_after_finished() -> i32 {
    60
}
const fn default_backoff_limit() -> i32 {
    0
}

impl KubernetesJobSpec {
    /// A spec with the four resource strings filled and everything
    /// else at its default.
    pub fn new(
        memory_request: impl Into<String>,
        memory_limit: impl Into<String>,
        cpu_request: impl Into<String>,
        cpu_limit: impl Into<String>,
    ) -> Self {
        Self {
            memory_request: memory_request.into(),
            memory_limit: memory_limit.into(),
            cpu_request: cpu_request.into(),
            cpu_limit: cpu_limit.into(),
            image: None,
            service_account: None,
            namespace: None,
            node_selector: BTreeMap::new(),
            ttl_seconds_after_finished: default_ttl_seconds_after_finished(),
            backoff_limit: default_backoff_limit(),
            tmpfs_scratch: false,
        }
    }

    /// Builder: pin the pod to a specific image rather than inheriting.
    pub fn with_image(mut self, image: impl Into<String>) -> Self {
        self.image = Some(image.into());
        self
    }

    /// Builder: run under a specific `ServiceAccount` rather than the
    /// dispatcher's default. CSAM-handling pods should always set this.
    pub fn with_service_account(mut self, name: impl Into<String>) -> Self {
        self.service_account = Some(name.into());
        self
    }

    /// Builder: create the `Job` in a specific namespace.
    pub fn with_namespace(mut self, namespace: impl Into<String>) -> Self {
        self.namespace = Some(namespace.into());
        self
    }

    /// Builder: add a nodeSelector key/value. Chainable for several keys.
    pub fn with_node_selector(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.node_selector.insert(key.into(), value.into());
        self
    }

    /// Builder: how long to keep the finished `Job` and its logs. `0`
    /// reaps immediately.
    pub fn with_ttl_seconds_after_finished(mut self, seconds: i32) -> Self {
        self.ttl_seconds_after_finished = seconds;
        self
    }

    /// Builder: mount `/scratch` as `emptyDir { medium: Memory }`. For
    /// pods handling sensitive content that must not touch disk.
    pub fn with_tmpfs_scratch(mut self) -> Self {
        self.tmpfs_scratch = true;
        self
    }

    /// Builder: Kubernetes-side retry count. Prefer leaving this at `0`
    /// (the default) — the queue worker owns retries.
    pub fn with_backoff_limit(mut self, limit: i32) -> Self {
        self.backoff_limit = limit;
        self
    }
}

/// The CLI argument the dispatcher sets on the pod's container so the
/// binary knows it is executing a single serialised job rather than
/// starting a long-lived worker. The application's own CLI matches on
/// this and calls [`run_single_job`].
///
/// Namespaced with a dash so it can't collide with a short-flag.
pub const SINGLE_JOB_SUBCOMMAND: &str = "--rainier-run-single-job";

/// Env var the dispatcher sets on the pod carrying the serialised job
/// payload. Env vars have a per-pod 1 MiB cap in Kubernetes; a payload
/// larger than that would need a ConfigMap, not supported in this
/// prototype — the use case (match ids, upload ids) is tens of bytes.
pub const PAYLOAD_ENV: &str = "RAINIER_JOB_PAYLOAD";

/// Env var the dispatcher sets carrying the job's wire name
/// ([`Job::NAME`](crate::Job::NAME)) so the pod can look it up in the
/// [`JobRegistry`](crate::JobRegistry).
pub const JOB_NAME_ENV: &str = "RAINIER_JOB_NAME";

/// Env var the dispatcher sets carrying the queue row's id, so the pod's
/// `JobContext` reports the same id the worker reserved and the queue's
/// `jobs` row references. Lets a single confirmed-CSAM artifact be
/// correlated across the queue row, the Kubernetes Job, and the pod's
/// own log lines.
pub const QUEUED_ID_ENV: &str = "RAINIER_QUEUED_JOB_ID";

/// Env var the dispatcher sets carrying the attempt number this worker
/// invocation is on. Starts at `1` and increments each time the queue
/// worker releases and re-reserves this row.
pub const ATTEMPT_ENV: &str = "RAINIER_JOB_ATTEMPT";

#[cfg(feature = "kubernetes")]
mod dispatcher {
    //! The `kube-rs` + `k8s-openapi` half of the module. Only compiled
    //! when the `kubernetes` feature is enabled.

    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::time::Duration;

    use k8s_openapi::api::authorization::v1::{
        ResourceAttributes, SelfSubjectAccessReview, SelfSubjectAccessReviewSpec,
    };
    use k8s_openapi::api::batch::v1::{Job as K8sJob, JobSpec};
    use k8s_openapi::api::core::v1::{
        Container, EmptyDirVolumeSource, EnvVar, Pod, PodSpec, PodTemplateSpec,
        ResourceRequirements, Volume, VolumeMount,
    };
    use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
    use kube::api::{Api, DeleteParams, ListParams, LogParams, PostParams};
    use kube::runtime::wait::{await_condition, Condition};
    use kube::Client;
    use rainier_support::{Error, Result};

    use super::{
        KubernetesJobSpec, ATTEMPT_ENV, JOB_NAME_ENV, PAYLOAD_ENV, QUEUED_ID_ENV,
        SINGLE_JOB_SUBCOMMAND,
    };

    /// Label the dispatcher stamps on every Job it creates. The
    /// orchestrator-side watch loop filters by this when looking up a
    /// Job's pod for log capture.
    const LABEL_JOB_NAME: &str = "rainier.job/name";
    /// Label carrying the queue row's id, so a Kubernetes Job can be
    /// traced back to the `jobs` row that produced it.
    const LABEL_QUEUED_ID: &str = "rainier.job/queued-id";
    /// How many lines of the pod's log to include in the failure error.
    /// A whole log can be hundreds of MiB; the tail is what diagnoses
    /// the failure and `kubectl logs` is still there for the rest.
    const FAILURE_LOG_TAIL_LINES: i64 = 100;

    /// Runs a reserved queue job as a Kubernetes `batch/v1 Job` pod and
    /// watches it to completion.
    ///
    /// Not a dispatcher of queue messages — the ordinary queue is still
    /// the dispatch surface. This is a *runner* the [`Worker`](
    /// crate::Worker) consults for jobs whose
    /// [`crate::Job::kubernetes`] returns `Some`. The
    /// type name is kept for backward compatibility; the shape it
    /// implements is a runner.
    #[derive(Clone)]
    pub struct KubernetesDispatcher {
        inner: Arc<Inner>,
    }

    struct Inner {
        client: Client,
        /// Default namespace to create Jobs in. Set from
        /// `KUBERNETES_NAMESPACE` at construction time; a spec may
        /// override per-job.
        default_namespace: String,
        /// Default container image. Typically the current pod's image,
        /// obtained via `DownwardAPI`.
        default_image: Option<String>,
        /// Default service account.
        default_service_account: Option<String>,
    }

    impl KubernetesDispatcher {
        /// Build a dispatcher from an existing `kube::Client`. Prefer
        /// [`try_in_cluster`](Self::try_in_cluster) when running inside
        /// a pod — it fills the namespace, image, and service account
        /// from the pod's own environment.
        pub fn new(client: Client, default_namespace: impl Into<String>) -> Self {
            Self {
                inner: Arc::new(Inner {
                    client,
                    default_namespace: default_namespace.into(),
                    default_image: None,
                    default_service_account: None,
                }),
            }
        }

        /// Build a dispatcher using the current pod's service-account
        /// token (`/var/run/secrets/kubernetes.io/serviceaccount/*`).
        /// Returns `None` when `KUBERNETES_SERVICE_HOST` is unset —
        /// i.e. the application isn't running in a cluster.
        ///
        /// The namespace comes from `KUBERNETES_NAMESPACE` (set by the
        /// pod's `DownwardAPI`); the image + default service account
        /// are read from `RAINIER_K8S_DEFAULT_IMAGE` + `RAINIER_K8S_DEFAULT_SA`
        /// if the deployment sets them (also via `DownwardAPI` or
        /// static env), otherwise left `None`.
        pub async fn try_in_cluster() -> Result<Option<Self>> {
            if std::env::var("KUBERNETES_SERVICE_HOST").is_err() {
                return Ok(None);
            }
            // rustls 0.23 requires a process-level CryptoProvider before any
            // TLS handshake, and kube-rs does not install one itself —
            // `Client::try_default()` would panic on first use otherwise.
            // `install_default` returns Err if a provider is already
            // installed (an application that uses rainier-http-client's
            // `install_tls_provider` will have beaten us to it, and both
            // call `ring::default_provider()` so the second call is a no-op
            // on the same provider).
            let _ = rustls::crypto::ring::default_provider().install_default();
            let client = Client::try_default()
                .await
                .map_err(|e| Error::internal(format!("building in-cluster kube client: {e}")))?;
            let namespace = std::env::var("KUBERNETES_NAMESPACE").unwrap_or_else(|_| {
                tracing::warn!(
                    "KUBERNETES_NAMESPACE not set via DownwardAPI; falling back to `default`"
                );
                "default".into()
            });
            let default_image = std::env::var("RAINIER_K8S_DEFAULT_IMAGE").ok();
            let default_sa = std::env::var("RAINIER_K8S_DEFAULT_SA").ok();
            Ok(Some(Self {
                inner: Arc::new(Inner {
                    client,
                    default_namespace: namespace,
                    default_image,
                    default_service_account: default_sa,
                }),
            }))
        }

        /// Namespace this dispatcher writes to by default.
        pub fn default_namespace(&self) -> &str {
            &self.inner.default_namespace
        }

        /// Ask the API what the current pod's ServiceAccount is actually
        /// allowed to do with `batch/v1 Jobs` and `pods/log` in the
        /// dispatcher's default namespace, via `SelfSubjectAccessReview`.
        /// The returned [`RbacReport`] names each verb and whether it was
        /// granted so the application can decide how strict to be — a
        /// cheap boot-time check beats a surprise 403 at the first
        /// dispatch.
        ///
        /// SSAR is a dry run: it asks the API server's authorizer, it
        /// does not actually create or list anything. Safe to call on
        /// every boot even without the `create` permission.
        pub async fn verify_rbac(&self) -> Result<RbacReport> {
            let api: Api<SelfSubjectAccessReview> = Api::all(self.inner.client.clone());
            let namespace = self.inner.default_namespace.clone();

            let can_create = check_verb(&api, &namespace, "batch", "jobs", "create").await?;
            let can_get = check_verb(&api, &namespace, "batch", "jobs", "get").await?;
            let can_watch = check_verb(&api, &namespace, "batch", "jobs", "watch").await?;
            let can_read_pod_logs = check_verb(&api, &namespace, "", "pods/log", "get").await?;

            Ok(RbacReport { namespace, can_create, can_get, can_watch, can_read_pod_logs })
        }

        /// Launch a Kubernetes Job for a reserved queue row and watch it
        /// to a terminal state. Called by the worker in place of running
        /// the job's `handle()` in-process.
        ///
        /// `job_name` is [`Job::NAME`](crate::Job::NAME) — the wire name
        /// the pod uses to look the job up in its own `JobRegistry`.
        /// `queued_id` is the queue row's id, used as the stable half of
        /// the Kubernetes Job's name so a worker that restarts
        /// mid-watch reattaches to the running Job rather than
        /// launching a duplicate. `attempt` participates in the name
        /// too, because the worker's retry logic produces a fresh
        /// attempt and a fresh Job per failure.
        ///
        /// Returns:
        /// * `Ok(())` when the Kubernetes Job reaches `Complete`.
        /// * `Err` with the pod's log tail when it reaches `Failed`.
        /// * `Err("timed out …")` when `timeout` elapses — the Job is
        ///   deleted so the kubelet terminates the pod.
        pub async fn run_and_watch(
            &self,
            job_name: &str,
            payload: &serde_json::Value,
            spec: &KubernetesJobSpec,
            queued_id: &str,
            attempt: u32,
            timeout: Option<Duration>,
        ) -> Result<()> {
            let namespace =
                spec.namespace.clone().unwrap_or_else(|| self.inner.default_namespace.clone());
            let image =
                spec.image.clone().or_else(|| self.inner.default_image.clone()).ok_or_else(
                    || {
                        Error::internal(
                            "no image to run the Kubernetes job in — set it on the spec or \
                             configure RAINIER_K8S_DEFAULT_IMAGE on the dispatcher's pod",
                        )
                    },
                )?;

            let payload_str = serde_json::to_string(payload).map_err(|e| {
                Error::internal(format!(
                    "serialising job `{job_name}` payload for Kubernetes dispatch: {e}"
                ))
            })?;
            if payload_str.len() > 900_000 {
                return Err(Error::internal(format!(
                    "job `{job_name}` payload is {} bytes; the Kubernetes dispatcher caps env-var \
                     payloads at ~900 KiB. Switch this job to a ConfigMap payload (not yet \
                     implemented) or shrink the payload.",
                    payload_str.len()
                )));
            }

            let k8s_name = kubernetes_name_for(job_name, queued_id, attempt);
            let k8s_job = render_job(
                job_name,
                &k8s_name,
                queued_id,
                attempt,
                spec,
                &image,
                &payload_str,
                self.inner.default_service_account.as_deref(),
            );

            let jobs_api: Api<K8sJob> = Api::namespaced(self.inner.client.clone(), &namespace);
            match jobs_api.create(&PostParams::default(), &k8s_job).await {
                Ok(_) => tracing::info!(
                    job_name,
                    queued_id,
                    attempt,
                    namespace = %namespace,
                    k8s_name = %k8s_name,
                    "created kubernetes job"
                ),
                Err(kube::Error::Api(err)) if err.code == 409 => {
                    tracing::info!(
                        job_name,
                        queued_id,
                        attempt,
                        namespace = %namespace,
                        k8s_name = %k8s_name,
                        "kubernetes job already exists — reattaching to its watch"
                    );
                }
                Err(e) => {
                    return Err(Error::internal(format!(
                        "creating batch/v1 Job for `{job_name}` in namespace \
                         `{namespace}`: {e}"
                    )));
                }
            }

            let outcome = match timeout {
                Some(limit) => match tokio::time::timeout(
                    limit,
                    watch_to_terminal(&jobs_api, &self.inner.client, &namespace, &k8s_name),
                )
                .await
                {
                    Ok(result) => result,
                    Err(_) => {
                        tracing::warn!(
                            job_name,
                            queued_id,
                            attempt,
                            namespace = %namespace,
                            k8s_name = %k8s_name,
                            ?limit,
                            "kubernetes job exceeded its timeout; deleting"
                        );
                        delete_job(&jobs_api, &k8s_name).await;
                        Err(Error::internal(format!(
                            "kubernetes job `{k8s_name}` exceeded its {limit:?} timeout"
                        )))
                    }
                },
                None => {
                    watch_to_terminal(&jobs_api, &self.inner.client, &namespace, &k8s_name).await
                }
            };

            if outcome.is_ok() && spec.ttl_seconds_after_finished == 0 {
                // Succeeded with ttl=0 — delete the Job so the pod and
                // its logs vanish immediately. Not an error if the TTL
                // controller already reaped it.
                delete_job(&jobs_api, &k8s_name).await;
            }

            outcome
        }
    }

    /// A `kube::runtime::wait::Condition` matching a Kubernetes Job that
    /// has reached `Complete` **or** `Failed` with `status: "True"`.
    ///
    /// Both conditions are terminal, so `await_condition` returns on
    /// either. The caller then inspects the delivered Job to tell
    /// Complete from Failed.
    fn job_terminal() -> impl Condition<K8sJob> {
        |obj: Option<&K8sJob>| -> bool {
            obj.and_then(|j| j.status.as_ref())
                .and_then(|s| s.conditions.as_ref())
                .map(|cs| {
                    cs.iter().any(|c| {
                        c.status == "True" && (c.type_ == "Complete" || c.type_ == "Failed")
                    })
                })
                .unwrap_or(false)
        }
    }

    /// Watch `k8s_name` until it reaches a terminal condition. Returns
    /// `Ok(())` on `Complete`, `Err(…)` with the pod's log tail on
    /// `Failed`.
    ///
    /// Uses `kube::runtime::wait::await_condition`, which holds a
    /// long-lived `watch` connection and reacts to the Job's state
    /// change as the API server emits it — no polling tick, so a Job
    /// that completes in 400 ms doesn't sit for the rest of the poll
    /// interval before the worker notices. On the API end this is one
    /// resourceVersion-tracked watch per in-flight Job rather than
    /// one `get` every three seconds, which also keeps the API server
    /// load flat as more pods come online.
    async fn watch_to_terminal(
        jobs_api: &Api<K8sJob>,
        client: &Client,
        namespace: &str,
        k8s_name: &str,
    ) -> Result<()> {
        // `await_condition` returns Ok(Some(obj)) when the condition
        // becomes true, Ok(None) only when the object is deleted
        // before terminating (treat as a failure — somebody removed
        // the Job out from under us), or Err on an unrecoverable
        // watch failure.
        let final_state = await_condition(jobs_api.clone(), k8s_name, job_terminal())
            .await
            .map_err(|e| Error::internal(format!("watching kubernetes job `{k8s_name}`: {e}")))?;

        let Some(job) = final_state else {
            return Err(Error::internal(format!(
                "kubernetes job `{k8s_name}` was deleted before it reached a terminal state"
            )));
        };

        // Which terminal condition fired. `job_terminal` only returned
        // true if one of them is `status: "True"`, so this always finds
        // the matching entry.
        let Some(condition) =
            job.status.as_ref().and_then(|s| s.conditions.as_ref()).and_then(|cs| {
                cs.iter()
                    .find(|c| c.status == "True" && (c.type_ == "Complete" || c.type_ == "Failed"))
            })
        else {
            return Err(Error::internal(format!(
                "kubernetes job `{k8s_name}` was reported terminal by the watch, but no \
                 Complete or Failed condition was `True` on inspection — the API server \
                 appears to have raced its own status update"
            )));
        };

        if condition.type_ == "Complete" {
            return Ok(());
        }

        // Failed. Capture the pod's log tail for the failed_jobs record;
        // the clone is cheap and keeps the condition's borrows separate
        // from the async-boundary that fetch_pod_log_tail crosses.
        let reason = condition.reason.clone().unwrap_or_else(|| "unspecified".into());
        let message = condition.message.clone().unwrap_or_else(|| "no message".into());
        let logs = fetch_pod_log_tail(client, namespace, k8s_name).await;
        Err(Error::internal(format!(
            "kubernetes job `{k8s_name}` failed ({reason}: {message}){}",
            if logs.is_empty() { String::new() } else { format!("\n--- pod log tail ---\n{logs}") }
        )))
    }

    /// Best-effort log fetch for a failed Job's pod. Returns the empty
    /// string on any error — a missing log is not worth upgrading the
    /// job's failure into a different one.
    async fn fetch_pod_log_tail(client: &Client, namespace: &str, k8s_name: &str) -> String {
        let pods_api: Api<Pod> = Api::namespaced(client.clone(), namespace);
        let selector = format!("job-name={k8s_name}");
        let pods = match pods_api.list(&ListParams::default().labels(&selector)).await {
            Ok(list) => list,
            Err(e) => {
                tracing::warn!(k8s_name, error = %e, "could not list pods for failed job");
                return String::new();
            }
        };
        let Some(pod) = pods.items.into_iter().next() else { return String::new() };
        let Some(name) = pod.metadata.name else { return String::new() };
        match pods_api
            .logs(
                &name,
                &LogParams { tail_lines: Some(FAILURE_LOG_TAIL_LINES), ..LogParams::default() },
            )
            .await
        {
            Ok(logs) => logs,
            Err(e) => {
                tracing::warn!(pod = %name, error = %e, "could not read pod logs for failed job");
                String::new()
            }
        }
    }

    async fn delete_job(jobs_api: &Api<K8sJob>, k8s_name: &str) {
        // `propagation_policy: Background` so the GC sweeps the pods on
        // its own. `Foreground` would make this call block until the
        // pods are gone, which inside a worker's handler would hold the
        // concurrency slot longer than necessary.
        let params = DeleteParams {
            propagation_policy: Some(kube::api::PropagationPolicy::Background),
            ..DeleteParams::default()
        };
        if let Err(e) = jobs_api.delete(k8s_name, &params).await {
            // 404 = already gone (ttl controller beat us), fine.
            if !matches!(&e, kube::Error::Api(err) if err.code == 404) {
                tracing::warn!(k8s_name, error = %e, "could not delete kubernetes job");
            }
        }
    }

    async fn check_verb(
        api: &Api<SelfSubjectAccessReview>,
        namespace: &str,
        group: &str,
        resource: &str,
        verb: &str,
    ) -> Result<bool> {
        let review = SelfSubjectAccessReview {
            spec: SelfSubjectAccessReviewSpec {
                resource_attributes: Some(ResourceAttributes {
                    namespace: Some(namespace.to_string()),
                    verb: Some(verb.to_string()),
                    group: Some(group.to_string()),
                    resource: Some(resource.to_string()),
                    ..ResourceAttributes::default()
                }),
                non_resource_attributes: None,
            },
            ..SelfSubjectAccessReview::default()
        };
        let answer = api.create(&PostParams::default(), &review).await.map_err(|e| {
            Error::internal(format!(
                "SelfSubjectAccessReview for `{verb}` on {group}/{resource}: {e}"
            ))
        })?;
        Ok(answer.status.map(|s| s.allowed).unwrap_or(false))
    }

    /// What `batch/v1 Jobs` operations the dispatcher's current
    /// credentials may perform in [`KubernetesDispatcher::default_namespace`].
    /// Returned by [`KubernetesDispatcher::verify_rbac`]; the application
    /// decides what to do with it (warn, hard-fail, continue).
    #[derive(Debug, Clone)]
    pub struct RbacReport {
        /// The namespace the checks were made against.
        pub namespace: String,
        /// Required to launch any Kubernetes Job.
        pub can_create: bool,
        /// Required by the watcher to poll the Job's status.
        pub can_get: bool,
        /// Required by any future informer-based watch replacement.
        pub can_watch: bool,
        /// Required by the failure path's pod-log capture.
        pub can_read_pod_logs: bool,
    }

    impl RbacReport {
        /// Whether the minimum needed to run and watch a Job is present.
        /// `can_read_pod_logs` is a quality-of-service bit — missing it
        /// means failed Jobs come back without the pod's log tail — so
        /// it is not required here.
        pub fn is_dispatch_ready(&self) -> bool {
            self.can_create && self.can_get
        }

        /// Short human-readable summary suitable for a boot-time log line.
        pub fn summary(&self) -> String {
            let ok = |b: bool| if b { "ok" } else { "MISSING" };
            format!(
                "namespace={} create={} get={} watch={} pods/log:get={}",
                self.namespace,
                ok(self.can_create),
                ok(self.can_get),
                ok(self.can_watch),
                ok(self.can_read_pod_logs),
            )
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn render_job(
        job_name: &str,
        k8s_name: &str,
        queued_id: &str,
        attempt: u32,
        spec: &KubernetesJobSpec,
        image: &str,
        payload: &str,
        default_service_account: Option<&str>,
    ) -> K8sJob {
        let service_account =
            spec.service_account.clone().or_else(|| default_service_account.map(|s| s.to_string()));

        let env = vec![
            EnvVar { name: JOB_NAME_ENV.into(), value: Some(job_name.into()), value_from: None },
            EnvVar { name: PAYLOAD_ENV.into(), value: Some(payload.into()), value_from: None },
            EnvVar { name: QUEUED_ID_ENV.into(), value: Some(queued_id.into()), value_from: None },
            EnvVar { name: ATTEMPT_ENV.into(), value: Some(attempt.to_string()), value_from: None },
            EnvVar {
                name: "RAINIER_K8S_JOB_NAME".into(),
                value: Some(k8s_name.into()),
                value_from: None,
            },
        ];

        let mut requests = BTreeMap::new();
        requests.insert("memory".to_string(), Quantity(spec.memory_request.clone()));
        requests.insert("cpu".to_string(), Quantity(spec.cpu_request.clone()));
        let mut limits = BTreeMap::new();
        limits.insert("memory".to_string(), Quantity(spec.memory_limit.clone()));
        limits.insert("cpu".to_string(), Quantity(spec.cpu_limit.clone()));

        let (volumes, volume_mounts) = if spec.tmpfs_scratch {
            (
                Some(vec![Volume {
                    name: "scratch".into(),
                    empty_dir: Some(EmptyDirVolumeSource {
                        medium: Some("Memory".into()),
                        size_limit: None,
                    }),
                    ..Volume::default()
                }]),
                Some(vec![VolumeMount {
                    name: "scratch".into(),
                    mount_path: "/scratch".into(),
                    ..VolumeMount::default()
                }]),
            )
        } else {
            (None, None)
        };

        let container = Container {
            name: "job".into(),
            image: Some(image.to_string()),
            args: Some(vec![SINGLE_JOB_SUBCOMMAND.into()]),
            env: Some(env),
            resources: Some(ResourceRequirements {
                requests: Some(requests),
                limits: Some(limits),
                ..ResourceRequirements::default()
            }),
            volume_mounts,
            ..Container::default()
        };

        let node_selector =
            if spec.node_selector.is_empty() { None } else { Some(spec.node_selector.clone()) };

        let pod_spec = PodSpec {
            containers: vec![container],
            restart_policy: Some("Never".into()),
            service_account_name: service_account,
            node_selector,
            volumes,
            ..PodSpec::default()
        };

        let labels = labels_for(job_name, k8s_name, queued_id);

        K8sJob {
            metadata: ObjectMeta {
                name: Some(k8s_name.into()),
                labels: Some(labels.clone()),
                ..ObjectMeta::default()
            },
            spec: Some(JobSpec {
                template: PodTemplateSpec {
                    metadata: Some(ObjectMeta { labels: Some(labels), ..ObjectMeta::default() }),
                    spec: Some(pod_spec),
                },
                ttl_seconds_after_finished: Some(spec.ttl_seconds_after_finished),
                backoff_limit: Some(spec.backoff_limit),
                ..JobSpec::default()
            }),
            ..K8sJob::default()
        }
    }

    fn labels_for(job_name: &str, k8s_name: &str, queued_id: &str) -> BTreeMap<String, String> {
        let mut labels = BTreeMap::new();
        labels.insert("app.kubernetes.io/managed-by".into(), "rainier-queue".into());
        labels.insert(LABEL_JOB_NAME.into(), slugify(job_name));
        labels.insert("rainier.job/instance".into(), k8s_name.into());
        labels.insert(LABEL_QUEUED_ID.into(), label_safe(queued_id));
        labels
    }

    /// Deterministic Kubernetes Job name for a (queue job id, attempt)
    /// pair. Kubernetes constrains `metadata.name` to DNS-1123 (lowercase
    /// alphanumeric + `-`, max 63 chars) and uniqueness within the
    /// namespace; both are what reattach-on-worker-restart needs.
    fn kubernetes_name_for(job_name: &str, queued_id: &str, attempt: u32) -> String {
        // 16 chars of slug (readable), 10 chars of hash (unique even if
        // two queued ids slugify the same), 2-digit attempt. Keeps the
        // name well under the 63-char DNS-1123 label limit.
        let slug_short: String = slugify(job_name).chars().take(16).collect();
        let hash = short_hash(queued_id);
        format!("rainier-{slug_short}-{hash}-{attempt}")
    }

    /// A short, printable, deterministic digest of `id` for use inside a
    /// DNS-1123 label. 10 lowercase hex chars = 40 bits, plenty for
    /// distinguishing a single application's in-flight queue rows.
    fn short_hash(id: &str) -> String {
        // A FNV-1a 64 is enough: this needs collision resistance within
        // the set of outstanding queue job ids, not cryptographic
        // strength. Keeps the module from pulling a hash crate.
        const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
        const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
        let mut h = FNV_OFFSET;
        for b in id.as_bytes() {
            h ^= u64::from(*b);
            h = h.wrapping_mul(FNV_PRIME);
        }
        format!("{h:010x}").chars().take(10).collect()
    }

    /// Kubernetes names and labels are RFC 1123 (lowercase alphanumeric
    /// plus `-`, max 253 chars). Rainier job names use dots by
    /// convention — `mail.welcome`, `moderation.photodna-move-to-quarantine`
    /// — so this replaces anything invalid with `-`.
    fn slugify(name: &str) -> String {
        let s: String =
            name.chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '-' {
                        c.to_ascii_lowercase()
                    } else {
                        '-'
                    }
                })
                .collect();
        let s: String = s
            .trim_matches('-')
            .chars()
            .scan(false, |prev_dash, c| {
                let is_dash = c == '-';
                if is_dash && *prev_dash {
                    Some(None)
                } else {
                    *prev_dash = is_dash;
                    Some(Some(c))
                }
            })
            .flatten()
            .take(50)
            .collect();
        if s.is_empty() {
            "job".into()
        } else {
            s
        }
    }

    /// Label values follow the same rules as names, with a 63-char cap.
    fn label_safe(value: &str) -> String {
        slugify(value).chars().take(63).collect()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn kubernetes_name_fits_within_the_dns1123_label_limit() {
            let name = kubernetes_name_for(
                "moderation.photodna-promote-to-radioactive-waste",
                "abcdef0123456789",
                1,
            );
            assert!(name.len() <= 63, "{name} is {} chars", name.len());
            assert!(name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'));
            assert!(name.starts_with("rainier-"));
        }

        #[test]
        fn kubernetes_name_is_deterministic_for_the_same_inputs() {
            let a = kubernetes_name_for("mail.welcome", "17f1234-abcd", 1);
            let b = kubernetes_name_for("mail.welcome", "17f1234-abcd", 1);
            assert_eq!(a, b, "same (name, id, attempt) must yield the same k8s name");
        }

        #[test]
        fn attempts_produce_distinct_names_so_a_retry_is_its_own_job() {
            let first = kubernetes_name_for("mail.welcome", "17f1234", 1);
            let second = kubernetes_name_for("mail.welcome", "17f1234", 2);
            assert_ne!(first, second);
        }

        #[test]
        fn different_queued_ids_produce_different_names() {
            let a = kubernetes_name_for("mail.welcome", "17f1234", 1);
            let b = kubernetes_name_for("mail.welcome", "17f1235", 1);
            assert_ne!(a, b);
        }
    }
}

#[cfg(feature = "kubernetes")]
pub use dispatcher::{KubernetesDispatcher, RbacReport};

/// Run a single serialised job and return. The pod-side entry point for
/// a job dispatched via [`KubernetesDispatcher`].
///
/// The application's CLI matches on [`SINGLE_JOB_SUBCOMMAND`], reads
/// [`JOB_NAME_ENV`] + [`PAYLOAD_ENV`] + [`QUEUED_ID_ENV`] + [`ATTEMPT_ENV`],
/// and calls this with the framework-level [`JobRegistry`](crate::JobRegistry)
/// + a built [`JobContext`](crate::JobContext).
///
/// On success the process exits 0; on error the process exits non-zero
/// and the queue worker watching on the orchestrator side treats that
/// as a job failure (released for retry, then `failed_jobs` on the
/// final attempt).
///
/// Not feature-gated: a service that implements its own dispatcher (not
/// via `kube-rs`) still wants this helper for the pod-side half.
pub async fn run_single_job(
    registry: &crate::JobRegistry,
    name: &str,
    payload: serde_json::Value,
    context: std::sync::Arc<crate::JobContext>,
) -> rainier_support::Result<()> {
    use crate::job::QueuedJob;
    let queued = QueuedJob {
        id: context.id().to_string(),
        name: name.to_string(),
        payload,
        queue: context.queue().to_string(),
        attempts: context.attempt(),
        max_attempts: context.max_attempts(),
        unique_key: None,
        delivery_handle: None,
        available_at: chrono::Utc::now(),
        created_at: chrono::Utc::now(),
    };
    registry.run(&queued, context).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_spec_serialises_and_deserialises_round_trip() {
        let spec = KubernetesJobSpec::new("2Gi", "4Gi", "500m", "1")
            .with_image("ghcr.io/example/svc:1.2.3")
            .with_service_account("csam-uploader")
            .with_namespace("lewd-prod")
            .with_node_selector("role", "sensitive-content")
            .with_ttl_seconds_after_finished(0)
            .with_tmpfs_scratch();
        let s = serde_json::to_string(&spec).unwrap();
        let round: KubernetesJobSpec = serde_json::from_str(&s).unwrap();
        assert_eq!(spec, round);
    }

    #[test]
    fn a_defaulted_spec_omits_optional_fields_in_the_wire_form() {
        let s =
            serde_json::to_string(&KubernetesJobSpec::new("1Gi", "2Gi", "100m", "500m")).unwrap();
        assert!(!s.contains("image"), "image should be absent when None: {s}");
        assert!(!s.contains("serviceAccount") && !s.contains("service_account"), "{s}");
    }

    #[test]
    fn the_default_backoff_limit_is_zero_so_retries_live_in_the_queue_worker() {
        // Load-bearing: raising this to one would let Kubernetes retry a
        // failing pod before the queue worker ever sees the failure, so
        // Job::TRIES stops being the authority on how many attempts the
        // work gets.
        let spec = KubernetesJobSpec::new("1Gi", "2Gi", "100m", "500m");
        assert_eq!(spec.backoff_limit, 0);
    }
}
