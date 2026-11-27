//! Longmire — one view across many Lookouts.
//!
//! Each Rainier service's Lookout records its own work, and with request
//! tracing on (`LOOKOUT_REQUEST_TRACING`) it records how that work joins the
//! work of others: which call from another service a request answered, and
//! which calls it made. It serves its side on `{path}/api/traces` and
//! `{path}/api/traces/{trace}`. Lookouts never talk to one another.
//!
//! Longmire is the other end: an application that can reach every
//! Lookout — an operations console — asks all of them at once, and links what
//! they say into one tree per request: each service's part under the call
//! that reached it.
//!
//! ```no_run
//! use std::sync::Arc;
//! use rainier_longmire::{Longmire, Source};
//!
//! # async fn run() -> rainier_support::Result<()> {
//! let longmire = Longmire::new(vec![
//!     Source::new("orders", "http://orders.internal"),
//!     Source::new("billing", "http://billing.internal").path("/ops/lookout"),
//! ])
//! // How to sign in to each: a bearer token its Lookout's reader accepts.
//! .credentials(|source| {
//!     let audience = source.id.clone();
//!     Box::pin(async move { Some(format!("token-for-{audience}")) })
//! });
//!
//! let trace = longmire.trace("4bf92f3577b34da6a3ce929d0e0e4736").await?;
//! let recent = longmire.recent(&Default::default()).await;
//! # let _ = (trace, recent);
//! # Ok(())
//! # }
//! ```
//!
//! What the application binds is which Lookouts ([`Source`]), how to sign in
//! to each ([`Longmire::credentials`]), and — with [`routes`] — who may
//! read the result. The linking is here: [`stitch`] and [`merge_recent`].
//!
//! See `docs/longmire.md`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod fetch;
mod http;
mod stitch;

use std::sync::Arc;
use std::time::Duration;

use rainier_http_client::Transport;
use rainier_support::{BoxFuture, Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use http::{routes, Gate};
pub use stitch::{merge_recent, stitch, Part, RECENT_LIMIT};

/// One Lookout to read: a service, where it is, and where it mounts Lookout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    /// A short id, used in what Longmire answers (`orders`).
    pub id: String,
    /// What a person reads (`Orders`). The id when not given.
    #[serde(default)]
    pub name: String,
    /// The service's base URL, reachable from here, no trailing slash.
    pub url: String,
    /// Where it mounts Lookout (`LOOKOUT_PATH`): `/_lookout` when not given.
    #[serde(default = "default_path")]
    pub path: String,
}

fn default_path() -> String {
    "/_lookout".into()
}

impl Source {
    /// A Lookout at `url`, mounted at the default path.
    pub fn new(id: impl Into<String>, url: impl Into<String>) -> Self {
        Self { id: id.into(), name: String::new(), url: url.into(), path: default_path() }.tidy()
    }

    /// What a person reads.
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self.tidy()
    }

    /// Where the service mounts Lookout.
    pub fn path(mut self, path: impl Into<String>) -> Self {
        self.path = path.into();
        self.tidy()
    }

    /// Trailing slashes off the URL, a leading one on the path, the name
    /// filled in. What a source read from configuration should go through.
    pub fn tidy(mut self) -> Self {
        self.id = self.id.trim().to_string();
        self.url = self.url.trim().trim_end_matches('/').to_string();
        let path = self.path.trim().trim_end_matches('/');
        self.path = if path.starts_with('/') { path.to_string() } else { format!("/{path}") };
        if self.name.trim().is_empty() {
            self.name = self.id.clone();
        }
        self
    }

    /// `{url}{path}/api/{tail}`, with `query` when there is one.
    pub fn api_url(&self, tail: &str, query: &str) -> String {
        let mut url = format!("{}{}/api/{}", self.url, self.path, tail);
        if !query.is_empty() {
            url.push('?');
            url.push_str(query);
        }
        url
    }
}

/// How to sign in to a source's Lookout: the bearer token its reader
/// accepts, or `None` to call without one.
pub type Credentials = Arc<dyn Fn(&Source) -> BoxFuture<'static, Option<String>> + Send + Sync>;

/// What to list in [`Longmire::recent`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentQuery {
    /// Only traces every service kept whole (the shared sample).
    #[serde(default)]
    pub kept: bool,
    /// Only batches that started before this (RFC 3339): the cursor.
    #[serde(default)]
    pub before: Option<String>,
    /// How many each Lookout is asked for (at most 200).
    #[serde(default)]
    pub limit: Option<u32>,
}

impl RecentQuery {
    fn to_query(&self) -> String {
        let mut parts = Vec::new();
        if self.kept {
            parts.push("kept=1".to_string());
        }
        if let Some(before) = self.before.as_deref().filter(|b| is_cursor(b)) {
            parts.push(format!("before={}", before.replace('+', "%2B")));
        }
        if let Some(limit) = self.limit {
            parts.push(format!("limit={}", limit.clamp(1, 200)));
        }
        parts.join("&")
    }
}

/// An RFC 3339 time, and nothing that could start another parameter.
fn is_cursor(value: &str) -> bool {
    value.len() <= 40
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b':' | b'.' | b'+'))
}

/// Many Lookouts, read as one.
#[derive(Clone)]
pub struct Longmire {
    sources: Vec<Source>,
    credentials: Option<Credentials>,
    timeout: Duration,
    transport: Option<Arc<dyn Transport>>,
}

impl std::fmt::Debug for Longmire {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Longmire")
            .field("sources", &self.sources)
            .field("credentials", &self.credentials.is_some())
            .field("timeout", &self.timeout)
            .finish()
    }
}

/// How long one Lookout may take to answer before it is shown as silent.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(8);

impl Longmire {
    /// Longmire over `sources`. A repeated id, or a source with no id or no
    /// http(s) URL, is dropped.
    pub fn new(sources: impl IntoIterator<Item = Source>) -> Self {
        let mut kept: Vec<Source> = Vec::new();
        for source in sources.into_iter().map(Source::tidy) {
            let usable = !source.id.is_empty()
                && source.id.len() <= 40
                && source.id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                && source.url.starts_with("http");
            if usable && !kept.iter().any(|s| s.id == source.id) {
                kept.push(source);
            }
        }
        Self { sources: kept, credentials: None, timeout: DEFAULT_TIMEOUT, transport: None }
    }

    /// Longmire over the sources in `json` — an array of
    /// `{"id", "name"?, "url", "path"?}`, as an environment variable holds
    /// them. Invalid JSON is an error that names what was wrong.
    pub fn from_json(json: &str) -> Result<Self> {
        let sources: Vec<Source> = serde_json::from_str(json)
            .map_err(|e| Error::bad_request(format!("Longmire's sources are not valid: {e}")))?;
        Ok(Self::new(sources))
    }

    /// How to sign in to each source's Lookout.
    pub fn credentials(
        mut self,
        credentials: impl Fn(&Source) -> BoxFuture<'static, Option<String>> + Send + Sync + 'static,
    ) -> Self {
        self.credentials = Some(Arc::new(credentials));
        self
    }

    /// How long each Lookout may take ([`DEFAULT_TIMEOUT`] by default).
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Send through `transport` rather than the process's — for tests.
    pub fn through(mut self, transport: Arc<dyn Transport>) -> Self {
        self.transport = Some(transport);
        self
    }

    /// The sources, as given (tidied, deduplicated).
    pub fn sources(&self) -> &[Source] {
        &self.sources
    }

    /// One trace across every source: each one's part, linked into one tree
    /// ([`stitch`]). A source that does not answer is listed with the reason.
    ///
    /// The id is a trace id as Lookout keys it, or a UUID (as a frontend that
    /// started the trace shows it), which is the same id without its dashes.
    pub async fn trace(&self, trace_id: &str) -> Result<Value> {
        let id = normalize_trace_id(trace_id);
        if !valid_trace_id(&id) {
            return Err(Error::bad_request("A trace id is up to 64 letters, digits and dashes."));
        }
        let parts = self.ask_all(&format!("traces/{id}"), "").await;
        Ok(stitch(&id, &parts))
    }

    /// The latest traces that crossed services, merged from every source's
    /// list into one row per trace, newest first ([`merge_recent`]).
    pub async fn recent(&self, query: &RecentQuery) -> Value {
        let parts = self.ask_all("traces", &query.to_query()).await;
        merge_recent(&parts)
    }

    /// Every source asked for `tail` at once, each within the timeout.
    async fn ask_all(&self, tail: &str, query: &str) -> Vec<Part> {
        let calls = self.sources.iter().map(|source| async move {
            let outcome =
                tokio::time::timeout(self.timeout, fetch::fetch(self, source, tail, query))
                    .await
                    .unwrap_or_else(|_| {
                        Err(format!("No answer within {} seconds.", self.timeout.as_secs()))
                    });
            (source.clone(), outcome)
        });
        futures_util::future::join_all(calls).await
    }
}

/// A trace id as Lookout keys it: a UUID loses its dashes (request tagging
/// stores a frontend's UUID that way), and case is lowered.
pub fn normalize_trace_id(id: &str) -> String {
    let id = id.trim().to_ascii_lowercase();
    let bare: String = id.chars().filter(|c| *c != '-').collect();
    if id.len() == 36 && bare.len() == 32 && bare.bytes().all(|b| b.is_ascii_hexdigit()) {
        bare
    } else {
        id
    }
}

/// Whether `id` can be a trace id: what Lookout accepts — letters, digits and
/// dashes, at most 64.
pub fn valid_trace_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use rainier_http_client::transport::{OutboundRequest, RawResponse};
    use serde_json::json;

    use super::*;

    const TRACE: &str = "4bf92f3577b34da6a3ce929d0e0e4736";

    /// Every Lookout at once, answering by URL; and what each was sent.
    #[derive(Default)]
    struct Lookouts {
        answers: BTreeMap<String, (u16, Value)>,
        asked: Mutex<Vec<(String, Option<String>)>>,
    }

    impl Transport for Lookouts {
        fn send<'a>(&'a self, request: OutboundRequest) -> BoxFuture<'a, Result<RawResponse>> {
            self.asked
                .lock()
                .unwrap()
                .push((request.url.clone(), request.header("authorization").map(str::to_string)));
            let (status, body) =
                self.answers.get(&request.url).cloned().unwrap_or((404, json!({})));
            Box::pin(async move {
                Ok(RawResponse {
                    status,
                    headers: BTreeMap::new(),
                    body: serde_json::to_vec(&body).unwrap(),
                })
            })
        }

        fn name(&self) -> &str {
            "lookouts"
        }
    }

    fn lookouts() -> Arc<Lookouts> {
        let mut answers = BTreeMap::new();
        answers.insert(
            format!("http://orders.test/_lookout/api/traces/{TRACE}"),
            (200, json!({ "data": { "trace_id": TRACE, "batches": [{
                "id": "o1", "started_at": "2026-11-27T10:00:00Z", "remote_parent_span_id": null,
                "spans": [{ "span_id": "c", "type": "http_client", "summary": "GET http://billing/x",
                            "call": { "span_id": "S1", "host": "billing", "status": 500 } }]
            }]}})),
        );
        answers.insert(
            format!("http://billing.test/ops/lookout/api/traces/{TRACE}"),
            (200, json!({ "data": { "trace_id": TRACE, "batches": [{
                "id": "b1", "started_at": "2026-11-27T10:00:00.010Z", "remote_parent_span_id": "S1",
                "spans": []
            }]}})),
        );
        Arc::new(Lookouts { answers, ..Lookouts::default() })
    }

    fn longmire(transport: &Arc<Lookouts>) -> Longmire {
        Longmire::new([
            Source::new("orders", "http://orders.test/"),
            Source::new("billing", "http://billing.test").path("ops/lookout/").name("Billing"),
            Source::new("mail", "http://mail.test"),
            Source::new("orders", "http://elsewhere.test"),
            Source::new("", "http://nameless.test"),
        ])
        .credentials(|source| {
            let id = source.id.clone();
            Box::pin(async move { Some(format!("token-{id}")) })
        })
        .through(Arc::clone(transport) as Arc<dyn Transport>)
    }

    #[tokio::test]
    async fn every_lookout_is_asked_with_its_own_credentials_and_the_parts_are_linked() {
        let transport = lookouts();
        let longmire = longmire(&transport);
        assert_eq!(longmire.sources().len(), 3, "a repeated id and a nameless one are dropped");

        // As a frontend shows it: a UUID, dashes and all.
        let trace = longmire.trace("4BF92F35-77B3-4DA6-A3CE-929D0E0E4736").await.unwrap();

        let order: Vec<&str> = trace["batches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["id"].as_str().unwrap())
            .collect();
        assert_eq!(order, ["o1", "b1"], "billing's part under orders' call");
        assert_eq!(trace["batches"][1]["service_name"], "Billing");
        assert_eq!(trace["services"][2]["status"], "unknown");
        assert!(trace["services"][2]["reason"].as_str().unwrap().contains("404"));

        let asked = transport.asked.lock().unwrap();
        assert!(asked.contains(&(
            format!("http://billing.test/ops/lookout/api/traces/{TRACE}"),
            Some("Bearer token-billing".into())
        )));
    }

    #[test]
    fn sources_come_from_json_as_configuration_holds_them() {
        let longmire = Longmire::from_json(
            r#"[{"id":"orders","url":"http://orders.test/"},{"id":"billing","name":"Billing","url":"http://billing.test","path":"ops/lookout"}]"#,
        )
        .unwrap();
        assert_eq!(longmire.sources()[0].path, "/_lookout");
        assert_eq!(
            longmire.sources()[1].api_url("traces", ""),
            "http://billing.test/ops/lookout/api/traces"
        );
        assert!(Longmire::from_json("{").is_err());
    }

    #[tokio::test]
    async fn a_malformed_id_asks_nobody() {
        let transport = lookouts();
        assert!(longmire(&transport).trace("a/b").await.is_err());
        assert!(transport.asked.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_recent_list_forwards_only_its_own_parameters() {
        let transport = lookouts();
        let query = RecentQuery {
            kept: true,
            before: Some("2026-11-27T10:00:00+00:00".into()),
            limit: Some(500),
        };
        longmire(&transport).recent(&query).await;

        let asked = transport.asked.lock().unwrap();
        assert!(
            asked.iter().any(|(url, _)| url
                == "http://orders.test/_lookout/api/traces?kept=1&before=2026-11-27T10:00:00%2B00:00&limit=200"),
            "{asked:?}"
        );

        let sneaky = RecentQuery { before: Some("x&q=drop".into()), ..RecentQuery::default() };
        assert_eq!(sneaky.to_query(), "", "nothing that could start another parameter");
    }

    #[tokio::test]
    async fn the_routes_answer_only_whom_the_gate_lets_through() {
        use rainier_http::{Method, StatusCode};

        let transport = lookouts();
        let router = routes(
            Arc::new(longmire(&transport)),
            Arc::new(|request: &rainier_http::Request| {
                let ok = request.header("authorization") == Some("Bearer staff");
                Box::pin(async move { ok })
            }),
        );
        let router = router.compile(&rainier_container::Container::new()).unwrap();
        let get = |auth: Option<&str>| {
            let mut builder = rainier_http::Request::builder()
                .method(Method::GET)
                .uri(format!("/traces/{TRACE}"));
            if let Some(auth) = auth {
                builder = builder.header("authorization", auth);
            }
            builder.build()
        };

        let refused = router.dispatch(get(None)).await;
        assert_eq!(refused.status(), StatusCode::NOT_FOUND);

        let mut answered = router.dispatch(get(Some("Bearer staff"))).await;
        assert_eq!(answered.status(), StatusCode::OK);
        let body = answered.take_body().collect().await.unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["data"]["batches"][1]["id"], "b1");
    }
}
