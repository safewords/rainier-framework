//! Dispatch a job as a Kubernetes `batch/v1 Job` instead of as a queue
//! message.
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
//! A job opts in by implementing [`Job::kubernetes`] and returning a
//! [`KubernetesJobSpec`] with its resource requirements. A
//! [`KubernetesDispatcher`] (built from an in-cluster `kube::Client`)
//! translates one of those jobs into a `batch/v1 Job` and submits it to
//! the Kubernetes API. The pod runs the SAME binary in a single-job
//! mode (see [`run_single_job`]) with the job name and payload passed
//! as arguments; it executes the job and exits.
//!
//! # Not every job
//!
//! Default [`Job::kubernetes`] returns `None` — the trait method is
//! additive and backward-compatible. A job that doesn't declare a spec
//! runs on the regular queue, which is still the right answer for the
//! vast majority of work. A job that does declare one still falls back
//! to the queue if the application isn't running in a cluster (local
//! dev, CI, a service that happens to not be deployed via Kubernetes);
//! see [`KubernetesDispatcher::try_in_cluster`].
//!
//! # Feature-gated
//!
//! The [`KubernetesJobSpec`] type is always available (every `Job` has a
//! [`kubernetes`](Job::kubernetes) method, feature or no feature). The
//! [`KubernetesDispatcher`] and the [`run_single_job`] helper live
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
    /// pod before marking the Job failed. The application's own
    /// [`Job::TRIES`] still applies inside the pod; this is a second
    /// safety net for pod-level crashes (OOM-kill, node preemption).
    /// Default `1` because the queue's retry is the primary path.
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
    1
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

#[cfg(feature = "kubernetes")]
mod dispatcher {
    //! The `kube-rs` + `k8s-openapi` half of the module. Only compiled
    //! when the `kubernetes` feature is enabled.

    use std::collections::BTreeMap;
    use std::sync::Arc;

    use k8s_openapi::api::authorization::v1::{
        ResourceAttributes, SelfSubjectAccessReview, SelfSubjectAccessReviewSpec,
    };
    use k8s_openapi::api::batch::v1::{Job as K8sJob, JobSpec};
    use k8s_openapi::api::core::v1::{
        Container, EmptyDirVolumeSource, EnvVar, PodSpec, PodTemplateSpec, ResourceRequirements,
        Volume, VolumeMount,
    };
    use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
    use kube::api::{Api, PostParams};
    use kube::Client;
    use rainier_support::{Error, Result};

    use super::{KubernetesJobSpec, JOB_NAME_ENV, PAYLOAD_ENV, SINGLE_JOB_SUBCOMMAND};
    use crate::job::Job;

    /// Dispatches a [`Job`] to Kubernetes as a `batch/v1 Job` resource.
    ///
    /// The dispatcher knows the current pod's image, namespace, and
    /// default service account; a [`KubernetesJobSpec`] may override any
    /// of them per-job. The resulting pod runs the same binary with the
    /// [`SINGLE_JOB_SUBCOMMAND`] argument and the job name + payload in
    /// env vars, which the application's CLI resolves to a single call
    /// to [`super::run_single_job`].
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
        /// allowed to do with `batch/v1 Jobs` in [`default_namespace`](
        /// Self::default_namespace), via `SelfSubjectAccessReview`. The
        /// returned [`RbacReport`] names each verb and whether it was
        /// granted so the application can decide how strict to be — a
        /// cheap boot-time check beats a surprise 403 at the first
        /// dispatch.
        ///
        /// SSAR is a dry run: it asks the API server's authorizer, it
        /// does not actually create or list anything. Safe to call on
        /// every boot even without the `create` permission.
        ///
        /// The dispatcher's own permissions are what this checks, which
        /// is `create jobs` for dispatch, `get`/`list` for post-dispatch
        /// observation. The per-job `ServiceAccount` on the pod
        /// ([`KubernetesJobSpec::with_service_account`]) is a different
        /// identity and can't be checked here without impersonation;
        /// an opt-in job that references a missing SA will fail at pod
        /// creation (the API refuses) — that is a sharper error than
        /// an SSAR could offer anyway.
        ///
        /// Returns `Err` only when the SSAR call itself fails (network,
        /// authentication). A granted/denied verdict is an `Ok`, with
        /// `allowed = false` on each denied verb.
        pub async fn verify_rbac(&self) -> Result<RbacReport> {
            let api: Api<SelfSubjectAccessReview> = Api::all(self.inner.client.clone());
            let namespace = self.inner.default_namespace.clone();

            let can_create = check_verb(&api, &namespace, "create").await?;
            let can_get = check_verb(&api, &namespace, "get").await?;
            let can_list = check_verb(&api, &namespace, "list").await?;

            Ok(RbacReport { namespace, can_create, can_get, can_list })
        }
    }

    async fn check_verb(
        api: &Api<SelfSubjectAccessReview>,
        namespace: &str,
        verb: &str,
    ) -> Result<bool> {
        let review = SelfSubjectAccessReview {
            spec: SelfSubjectAccessReviewSpec {
                resource_attributes: Some(ResourceAttributes {
                    namespace: Some(namespace.to_string()),
                    verb: Some(verb.to_string()),
                    group: Some("batch".to_string()),
                    resource: Some("jobs".to_string()),
                    ..ResourceAttributes::default()
                }),
                non_resource_attributes: None,
            },
            ..SelfSubjectAccessReview::default()
        };
        let answer = api
            .create(&PostParams::default(), &review)
            .await
            .map_err(|e| Error::internal(format!("SelfSubjectAccessReview for `{verb}`: {e}")))?;
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
        /// Required to dispatch any Kubernetes job. Without this,
        /// [`KubernetesDispatcher::dispatch`] errors on every call.
        pub can_create: bool,
        /// Optional; required by any future surface that reads a single
        /// Job's status.
        pub can_get: bool,
        /// Optional; required by any future surface that enumerates
        /// outstanding Jobs (an admin dashboard, a cleanup sweeper).
        pub can_list: bool,
    }

    impl RbacReport {
        /// Whether the minimum needed to dispatch is present.
        pub fn is_dispatch_ready(&self) -> bool {
            self.can_create
        }

        /// Short human-readable summary suitable for a boot-time log line.
        pub fn summary(&self) -> String {
            let ok = |b: bool| if b { "ok" } else { "MISSING" };
            format!(
                "namespace={} create={} get={} list={}",
                self.namespace,
                ok(self.can_create),
                ok(self.can_get),
                ok(self.can_list),
            )
        }
    }

    impl KubernetesDispatcher {
        /// Dispatch `job` as a `batch/v1 Job`. The job must have
        /// [`Job::kubernetes`] returning `Some`; a `None` here is a
        /// contract break (the application dispatched a queue-only job
        /// via the Kubernetes path).
        pub async fn dispatch<J: Job>(&self, job: &J) -> Result<()> {
            let spec = job.kubernetes().ok_or_else(|| {
                Error::internal(format!(
                    "job `{}` was dispatched to Kubernetes but did not declare a \
                     KubernetesJobSpec; implement Job::kubernetes to opt in",
                    J::NAME
                ))
            })?;

            let payload = serde_json::to_string(job).map_err(|e| {
                Error::internal(format!(
                    "serialising job `{}` for Kubernetes dispatch: {e}",
                    J::NAME
                ))
            })?;
            if payload.len() > 900_000 {
                // Env vars have a per-pod 1 MiB cap; staying under 900 KiB
                // leaves room for the other env the pod carries.
                return Err(Error::internal(format!(
                    "job `{}` payload is {} bytes; the Kubernetes dispatcher caps env-var \
                     payloads at ~900 KiB. Switch this job to a ConfigMap payload (not yet \
                     implemented) or shrink the payload.",
                    J::NAME,
                    payload.len(),
                )));
            }

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

            let k8s_job = render_job::<J>(&spec, &image, &payload, &self.inner);

            let api: Api<K8sJob> = Api::namespaced(self.inner.client.clone(), &namespace);
            api.create(&PostParams::default(), &k8s_job).await.map_err(|e| {
                Error::internal(format!(
                    "creating batch/v1 Job for `{}` in namespace `{namespace}`: {e}",
                    J::NAME
                ))
            })?;

            tracing::info!(
                job_name = J::NAME,
                namespace = %namespace,
                memory = %spec.memory_limit,
                cpu = %spec.cpu_limit,
                "dispatched job to Kubernetes"
            );
            Ok(())
        }

        /// Dispatch to Kubernetes when the job declares a spec; otherwise
        /// fall back to `queue`. The ergonomic entry point for an
        /// application that opts a few jobs in but doesn't want to
        /// branch at every call site.
        pub async fn dispatch_or_queue<J: Job, Q: QueueLike>(
            k8s: Option<&Self>,
            queue: &Q,
            job: J,
        ) -> Result<()> {
            if let (Some(k8s), true) = (k8s, job.kubernetes().is_some()) {
                return k8s.dispatch(&job).await;
            }
            queue.dispatch(job).await
        }
    }

    /// The slice of a `Queue`/`QueueManager` [`dispatch_or_queue`](
    /// KubernetesDispatcher::dispatch_or_queue) needs — kept to a method
    /// name so this module doesn't drag in the concrete queue type.
    #[async_trait::async_trait]
    pub trait QueueLike {
        /// Dispatch `job` to the ordinary queue — the fallback path when
        /// Kubernetes is not available or the job did not declare a spec.
        async fn dispatch<J: Job>(&self, job: J) -> Result<()>;
    }

    fn render_job<J: Job>(
        spec: &KubernetesJobSpec,
        image: &str,
        payload: &str,
        inner: &Inner,
    ) -> K8sJob {
        let service_account =
            spec.service_account.clone().or_else(|| inner.default_service_account.clone());

        let name = pod_name_for::<J>();

        let mut env = vec![
            EnvVar { name: JOB_NAME_ENV.into(), value: Some(J::NAME.into()), value_from: None },
            EnvVar { name: PAYLOAD_ENV.into(), value: Some(payload.into()), value_from: None },
        ];
        // Make the pod's own name available to the application so logs can
        // tie back to the Kubernetes resource.
        env.push(EnvVar {
            name: "RAINIER_K8S_JOB_NAME".into(),
            value: Some(name.clone()),
            value_from: None,
        });

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
            // The pod's `command` is the application's binary (image
            // ENTRYPOINT); we add the single-job subcommand so the CLI
            // routes to [`run_single_job`] rather than starting a
            // long-lived worker.
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

        K8sJob {
            metadata: ObjectMeta {
                generate_name: Some(format!("rainier-job-{}-", slugify(J::NAME))),
                labels: Some(labels_for::<J>(&name)),
                ..ObjectMeta::default()
            },
            spec: Some(JobSpec {
                template: PodTemplateSpec {
                    metadata: Some(ObjectMeta {
                        labels: Some(labels_for::<J>(&name)),
                        ..ObjectMeta::default()
                    }),
                    spec: Some(pod_spec),
                },
                ttl_seconds_after_finished: Some(spec.ttl_seconds_after_finished),
                backoff_limit: Some(spec.backoff_limit),
                ..JobSpec::default()
            }),
            ..K8sJob::default()
        }
    }

    fn pod_name_for<J: Job>() -> String {
        format!("rainier-job-{}", slugify(J::NAME))
    }

    fn labels_for<J: Job>(name: &str) -> BTreeMap<String, String> {
        let mut labels = BTreeMap::new();
        labels.insert("app.kubernetes.io/managed-by".into(), "rainier-queue".into());
        labels.insert("rainier.job/name".into(), slugify(J::NAME));
        labels.insert("rainier.job/instance".into(), name.into());
        labels
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
        // Trim leading/trailing dashes; collapse runs. 63 chars is the
        // DNS-1123 label limit (names can be longer with multiple labels,
        // but the simple path stays well under).
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
}

#[cfg(feature = "kubernetes")]
pub use dispatcher::{KubernetesDispatcher, QueueLike, RbacReport};

/// Run a single serialised job and return. The pod-side entry point for
/// a job dispatched via [`KubernetesDispatcher`].
///
/// The application's CLI matches on [`SINGLE_JOB_SUBCOMMAND`], reads
/// [`JOB_NAME_ENV`] + [`PAYLOAD_ENV`], and calls this with the
/// framework-level [`JobRegistry`](crate::JobRegistry) + a built
/// [`JobContext`](crate::JobContext). On success the process exits 0;
/// on error the process exits non-zero and Kubernetes' `backoffLimit`
/// decides whether to retry the pod.
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
}
