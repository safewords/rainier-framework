//! The `tracing` layer: spans in, [`Entry`] values out.
//!
//! [`LookoutLayer`] reads the spans the framework already emits with its
//! `telemetry` features (see `docs/observability.md`, "The catalogue") and
//! turns each closed one into an entry of the batch it ran in:
//!
//! | Span | Entry | Family key |
//! |---|---|---|
//! | `http.server.request` (or `http.request`) | [`Request`](EntryType::Request), opens a [`BatchKind::Request`] batch | route name, else `METHOD pattern` |
//! | `queue.process` | [`Job`](EntryType::Job), opens a [`BatchKind::Job`] batch | `rainier.job.name` |
//! | `console.command` | [`Command`](EntryType::Command), opens a batch | `rainier.command.name` |
//! | `schedule.task` | [`ScheduledTask`](EntryType::ScheduledTask), opens a batch | `rainier.task.name` |
//! | `db.query` | [`Query`](EntryType::Query) | the sanitised `db.query.text` |
//! | `cache.*` | [`Cache`](EntryType::Cache) | `operation (store)` |
//! | `http.client.request` | [`HttpClient`](EntryType::HttpClient) | `METHOD host/path`, ids in the path as `{id}` |
//! | `queue.publish` | [`Job`](EntryType::Job) | `dispatch:` + job name |
//! | `mail.send` | [`Mail`](EntryType::Mail) | the transport |
//! | `notification.send` | [`Notification`](EntryType::Notification) | the notification |
//! | `event.dispatch` | [`Event`](EntryType::Event) — not the framework's own `rainier_*` events | the event |
//! | `view.render` | [`View`](EntryType::View) | the view |
//! | `session.*` | [`Session`](EntryType::Session) | the operation |
//! | `auth.attempt` | [`AuthAttempt`](EntryType::AuthAttempt) | the guard |
//! | `broadcast.*`, `websocket.*` | [`Broadcast`](EntryType::Broadcast) | the event or kind |
//! | `storage.*` | [`Custom`](EntryType::Custom) | the operation |
//!
//! Log events at or above [`LookoutConfig::log_level`]
//! inside a batch become [`Log`](EntryType::Log) entries.
//!
//! **Roots.** A request, command, job or task span with no batch around it
//! opens one. `queue.process` and `schedule.task` open their own batch even
//! inside a `console.command` — `queue:work` and `schedule:work` are commands
//! that run forever, and their jobs are the units worth looking at. Inside a
//! request they are part of the request (a synchronous queue driver). Spans
//! outside every batch are not recorded; see [`recorder`](crate::recorder).
//!
//! **Lookout's own work** runs inside a span declaring
//! `rainier.lookout.internal`; it and everything under it is skipped.
//!
//! **Off is cheap, but not free of `tracing`.** A disabled Lookout's layer
//! returns from every callback at once. It never answers `false` from
//! `enabled` or `Interest::never()` from `register_callsite`: a layer's
//! "never" disables the callsite for *every* layer of the subscriber, and
//! Lookout must not switch off the application's logs or its exporter.

use std::sync::Arc;
use std::time::Instant;

use chrono::{DateTime, Utc};
use serde_json::{Map, Value};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

use crate::config::LookoutConfig;
use crate::entry::{BatchKind, Entry, EntryType};
use crate::recorder::{new_entry_id, sample, BatchCell, Inner, INTERNAL_FIELD};
use crate::redact::{redact_json, truncate};

/// The `tracing` layer. Get one from [`Lookout::layer`](crate::Lookout::layer).
pub struct LookoutLayer {
    inner: Arc<Inner>,
    on: bool,
}

impl std::fmt::Debug for LookoutLayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LookoutLayer").field("on", &self.on).finish()
    }
}

impl LookoutLayer {
    pub(crate) fn new(inner: Arc<Inner>) -> Self {
        let on = inner.config.enabled;
        Self { inner, on }
    }
}

/// Marks a span (and so everything under it) as Lookout's own work.
struct Internal;

/// What the layer keeps on each span inside a batch.
struct SpanState {
    batch: Arc<BatchCell>,
    /// Whether this span opened the batch.
    root: bool,
    /// Set when this span becomes an entry.
    draft: Option<Draft>,
    /// The nearest enclosing span that is an entry: the waterfall's parent.
    entry_parent: Option<String>,
}

/// A span on its way to being an entry.
struct Draft {
    entry_type: EntryType,
    name: &'static str,
    uid: String,
    fields: Vec<(&'static str, FieldValue)>,
    started: Instant,
    started_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
enum FieldValue {
    Str(String),
    I64(i64),
    U64(u64),
    F64(f64),
    Bool(bool),
}

impl FieldValue {
    fn as_str(&self) -> Option<&str> {
        match self {
            FieldValue::Str(s) => Some(s),
            _ => None,
        }
    }

    fn to_json(&self) -> Value {
        match self {
            FieldValue::Str(s) => Value::String(s.clone()),
            FieldValue::I64(n) => Value::from(*n),
            FieldValue::U64(n) => Value::from(*n),
            FieldValue::F64(n) => Value::from(*n),
            FieldValue::Bool(b) => Value::Bool(*b),
        }
    }

    fn render(&self) -> String {
        match self {
            FieldValue::Str(s) => s.clone(),
            FieldValue::I64(n) => n.to_string(),
            FieldValue::U64(n) => n.to_string(),
            FieldValue::F64(n) => n.to_string(),
            FieldValue::Bool(b) => b.to_string(),
        }
    }
}

/// Collects fields; a later value for the same name replaces the earlier.
struct Fields<'a>(&'a mut Vec<(&'static str, FieldValue)>);

impl Fields<'_> {
    fn set(&mut self, field: &Field, value: FieldValue) {
        let name = field.name();
        match self.0.iter_mut().find(|(k, _)| *k == name) {
            Some(slot) => slot.1 = value,
            None => self.0.push((name, value)),
        }
    }
}

impl Visit for Fields<'_> {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.set(field, FieldValue::Str(value.to_string()));
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.set(field, FieldValue::I64(value));
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.set(field, FieldValue::U64(value));
    }
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.set(field, FieldValue::F64(value));
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.set(field, FieldValue::Bool(value));
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.set(field, FieldValue::Str(format!("{value:?}")));
    }
}

/// What a span name records as, and whether it can open a batch.
fn classify(name: &str) -> Option<(EntryType, Option<BatchKind>)> {
    Some(match name {
        "http.server.request" | "http.request" => (EntryType::Request, Some(BatchKind::Request)),
        "queue.process" => (EntryType::Job, Some(BatchKind::Job)),
        "console.command" => (EntryType::Command, Some(BatchKind::Command)),
        "schedule.task" => (EntryType::ScheduledTask, Some(BatchKind::ScheduledTask)),
        "db.query" => (EntryType::Query, None),
        "http.client.request" => (EntryType::HttpClient, None),
        "queue.publish" => (EntryType::Job, None),
        "mail.send" => (EntryType::Mail, None),
        "notification.send" => (EntryType::Notification, None),
        "event.dispatch" => (EntryType::Event, None),
        "view.render" => (EntryType::View, None),
        "auth.attempt" => (EntryType::AuthAttempt, None),
        _ if name.starts_with("cache.") => (EntryType::Cache, None),
        _ if name.starts_with("session.") => (EntryType::Session, None),
        _ if name.starts_with("broadcast.") || name.starts_with("websocket.") => {
            (EntryType::Broadcast, None)
        }
        _ if name.starts_with("storage.") => (EntryType::Custom, None),
        _ => return None,
    })
}

fn field<'a>(fields: &'a [(&'static str, FieldValue)], name: &str) -> Option<&'a FieldValue> {
    fields.iter().find(|(k, _)| *k == name).map(|(_, v)| v)
}

fn text<'a>(fields: &'a [(&'static str, FieldValue)], name: &str) -> Option<&'a str> {
    field(fields, name).and_then(FieldValue::as_str).filter(|s| !s.is_empty())
}

impl<S> Layer<S> for LookoutLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        if !self.on {
            return;
        }
        let Some(span) = ctx.span(id) else { return };
        let meta = attrs.metadata();
        let parent = span.parent();

        // Lookout's own work, and everything under it.
        if meta.fields().field(INTERNAL_FIELD).is_some()
            || parent.as_ref().is_some_and(|p| p.extensions().get::<Internal>().is_some())
        {
            span.extensions_mut().insert(Internal);
            return;
        }

        let enclosing = parent.as_ref().and_then(|p| {
            p.extensions().get::<SpanState>().map(|s| {
                let entry_parent = match &s.draft {
                    Some(d) => Some(d.uid.clone()),
                    None => s.entry_parent.clone(),
                };
                (Arc::clone(&s.batch), entry_parent)
            })
        });
        let class = classify(meta.name());

        let opens = match (&enclosing, class) {
            (None, Some((_, Some(kind)))) => Some(kind),
            (Some((batch, _)), Some((_, Some(kind))))
                if matches!(kind, BatchKind::Job | BatchKind::ScheduledTask)
                    && batch.kind != BatchKind::Request =>
            {
                Some(kind)
            }
            _ => None,
        };

        let mut fields = Vec::new();
        let state = if let Some(kind) = opens {
            if !self.inner.active() {
                return;
            }
            attrs.record(&mut Fields(&mut fields));
            let name = match kind {
                BatchKind::Job => text(&fields, "rainier.job.name"),
                BatchKind::Command => text(&fields, "rainier.command.name"),
                BatchKind::ScheduledTask => text(&fields, "rainier.task.name"),
                _ => None,
            };
            let batch = self.inner.open_batch(kind, text(&fields, "url.path"), name);
            if let Some(trace) = text(&fields, "trace_id") {
                self.inner.set_trace_id(&batch, trace);
            }
            SpanState { batch, root: true, draft: None, entry_parent: None }
        } else if let Some((batch, entry_parent)) = enclosing {
            if class.is_some() && !batch.is_rejected() {
                attrs.record(&mut Fields(&mut fields));
            }
            SpanState { batch, root: false, draft: None, entry_parent }
        } else {
            return;
        };

        let draft = class.and_then(|(entry_type, _)| {
            if !state.root
                && (state.batch.is_rejected() || !self.wants(entry_type, meta.name(), &fields))
            {
                return None;
            }
            Some(Draft {
                entry_type,
                name: meta.name(),
                uid: new_entry_id(),
                fields,
                started: Instant::now(),
                started_at: Utc::now(),
            })
        });
        span.extensions_mut().replace(SpanState { draft, ..state });
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        if !self.on {
            return;
        }
        let Some(span) = ctx.span(id) else { return };
        let mut extensions = span.extensions_mut();
        let Some(state) = extensions.get_mut::<SpanState>() else { return };
        let Some(draft) = state.draft.as_mut() else { return };
        let mut recorded = Vec::new();
        values.record(&mut Fields(&mut recorded));
        let trace_recorded = recorded.iter().any(|(k, _)| *k == "trace_id");
        let route_recorded =
            recorded.iter().any(|(k, _)| matches!(*k, "http.route" | "rainier.route.name"));
        for (name, value) in recorded {
            match draft.fields.iter_mut().find(|(k, _)| *k == name) {
                Some(slot) => slot.1 = value,
                None => draft.fields.push((name, value)),
            }
        }
        if !state.root {
            return;
        }
        // The root learns its trace id and route after it opens.
        let batch = Arc::clone(&state.batch);
        let trace =
            if trace_recorded { text(&draft.fields, "trace_id").map(str::to_string) } else { None };
        let route = route_recorded.then(|| {
            (
                text(&draft.fields, "rainier.route.name").map(str::to_string),
                text(&draft.fields, "http.route").map(str::to_string),
            )
        });
        drop(extensions);
        if let Some(trace) = trace {
            self.inner.set_trace_id(&batch, &trace);
        }
        if let Some((name, pattern)) = route {
            self.inner.set_route(&batch, name.as_deref(), pattern.as_deref());
        }
    }

    fn on_enter(&self, id: &Id, ctx: Context<'_, S>) {
        if !self.on {
            return;
        }
        let Some(span) = ctx.span(id) else { return };
        let batch = span.extensions().get::<SpanState>().map(|s| Arc::clone(&s.batch));
        if let Some(batch) = batch {
            self.inner.enter(&batch);
        }
    }

    fn on_exit(&self, id: &Id, ctx: Context<'_, S>) {
        if !self.on {
            return;
        }
        let Some(span) = ctx.span(id) else { return };
        let batch = span.extensions().get::<SpanState>().map(|s| Arc::clone(&s.batch));
        if let Some(batch) = batch {
            self.inner.exit(&batch);
        }
    }

    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        if !self.on {
            return;
        }
        let Some(span) = ctx.span(&id) else { return };
        let Some(state) = span.extensions_mut().remove::<SpanState>() else { return };
        let entry = state
            .draft
            .map(|draft| build_entry(&self.inner.config, draft, state.entry_parent.clone()));
        if state.root {
            self.inner.close_batch(&state.batch, entry);
        } else if let Some(entry) = entry {
            self.inner.push(&state.batch, entry);
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        if !self.on {
            return;
        }
        let Some(threshold) = self.inner.log_level else { return };
        let meta = event.metadata();
        if *meta.level() > threshold {
            return;
        }
        let Some(span) = ctx.event_span(event) else { return };
        let (batch, entry_parent) = {
            let extensions = span.extensions();
            if extensions.get::<Internal>().is_some() {
                return;
            }
            let Some(state) = extensions.get::<SpanState>() else { return };
            let parent = match &state.draft {
                Some(d) => Some(d.uid.clone()),
                None => state.entry_parent.clone(),
            };
            (Arc::clone(&state.batch), parent)
        };
        if batch.is_rejected() {
            return;
        }
        let watcher = self.inner.config.watcher(EntryType::Log);
        if !watcher.enabled || !sample(watcher.sample_rate) {
            return;
        }
        let mut fields = Vec::new();
        event.record(&mut Fields(&mut fields));
        let message = text(&fields, "message").unwrap_or_default().to_string();
        if watcher.ignore.iter().any(|p| crate::config::glob(p, &message)) {
            return;
        }
        let config = &self.inner.config;
        let mut content = Map::new();
        content.insert("level".into(), Value::String(meta.level().to_string().to_lowercase()));
        content.insert("target".into(), Value::String(meta.target().to_string()));
        for (name, value) in &fields {
            content.insert((*name).to_string(), value.to_json());
        }
        let mut content = Value::Object(content);
        redact_json(&mut content, &config.redaction);
        let level = meta.level().to_string().to_lowercase();
        let entry = Entry {
            id: new_entry_id(),
            batch_id: String::new(),
            entry_type: EntryType::Log,
            span_id: None,
            parent_span_id: entry_parent,
            family_hash: Some(format!("{level} {}", normalize_digits(&message))),
            summary: truncate(&message, 200),
            occurred_at: Utc::now(),
            duration_us: None,
            tags: vec![level],
            content,
        };
        self.inner.push(&batch, entry);
    }
}

impl LookoutLayer {
    /// Whether a watcher wants this span at all: on, sampled in, and — for
    /// `event.dispatch` — not one of the framework's own lifecycle events.
    fn wants(
        &self,
        entry_type: EntryType,
        name: &str,
        fields: &[(&'static str, FieldValue)],
    ) -> bool {
        let watcher = self.inner.config.watcher(entry_type);
        if !watcher.enabled || !sample(watcher.sample_rate) {
            return false;
        }
        if name == "event.dispatch"
            && text(fields, "rainier.event.name").is_some_and(|n| n.starts_with("rainier_"))
        {
            return false;
        }
        true
    }
}

/// Turn a closed span into an entry.
fn build_entry(config: &LookoutConfig, draft: Draft, parent: Option<String>) -> Entry {
    let duration_us = u64::try_from(draft.started.elapsed().as_micros()).unwrap_or(u64::MAX);
    let fields = &draft.fields;
    let t = |name: &str| text(fields, name);
    let num = |name: &str| field(fields, name).map(FieldValue::render);
    let failed = t("otel.status_code") == Some("ERROR");
    let mut tags = Vec::new();
    if failed {
        tags.push("failed".to_string());
    }

    let (family, summary) = match (draft.entry_type, draft.name) {
        (EntryType::Request, _) => {
            let method = t("http.request.method").unwrap_or("?");
            let pattern = t("http.route");
            let family = match (t("rainier.route.name"), pattern) {
                (Some(name), _) => name.to_string(),
                (None, Some(pattern)) => format!("{method} {pattern}"),
                (None, None) => format!("{method} (no route)"),
            };
            let target = pattern.or(t("url.path")).unwrap_or("?");
            let summary = match num("http.response.status_code") {
                Some(status) => format!("{method} {target} {status}"),
                None => format!("{method} {target}"),
            };
            (Some(family), summary)
        }
        (EntryType::Query, _) => {
            let sql = t("db.query.text").or(t("otel.name")).unwrap_or(draft.name);
            (Some(sql.to_string()), truncate(sql, 300))
        }
        (EntryType::Cache, name) => {
            let op = t("cache.operation")
                .map(str::to_string)
                .unwrap_or_else(|| name.trim_start_matches("cache.").to_string());
            let family = match t("cache.store") {
                Some(store) => format!("{op} ({store})"),
                None => op.clone(),
            };
            let mut summary = format!("cache {op}");
            if config.capture.cache_keys {
                if let Some(key) = t("cache.key") {
                    summary.push(' ');
                    summary.push_str(&truncate(key, 200));
                }
            }
            if let Some(FieldValue::Bool(hit)) = field(fields, "cache.hit") {
                summary.push_str(if *hit { " hit" } else { " miss" });
                tags.push(if *hit { "hit" } else { "miss" }.to_string());
            }
            (Some(family), summary)
        }
        (EntryType::HttpClient, _) => {
            let method = t("http.request.method").unwrap_or("?");
            let url = t("url.full").unwrap_or("?");
            let family = format!("{method} {}", url_family(url, t("server.address")));
            let summary = match num("http.response.status_code") {
                Some(status) => format!("{method} {url} {status}"),
                None => format!("{method} {url}"),
            };
            (Some(family), summary)
        }
        (EntryType::Job, "queue.publish") => {
            let job = t("rainier.job.name").unwrap_or("?");
            let queue = t("messaging.destination.name").unwrap_or("default");
            (Some(format!("dispatch:{job}")), format!("dispatched {job} to {queue}"))
        }
        (EntryType::Job, _) => {
            let job = t("rainier.job.name").unwrap_or("?");
            let summary = match t("rainier.job.outcome") {
                Some(outcome) => format!("{job} {outcome}"),
                None => job.to_string(),
            };
            if let Some(outcome) = t("rainier.job.outcome") {
                if matches!(outcome, "failed" | "timed_out" | "error") && !failed {
                    tags.push("failed".to_string());
                }
            }
            (Some(job.to_string()), summary)
        }
        (EntryType::Command, _) => {
            let name = t("rainier.command.name").unwrap_or("?");
            let summary = match num("process.exit.code") {
                Some(code) => format!("{name} exited {code}"),
                None => name.to_string(),
            };
            (Some(name.to_string()), summary)
        }
        (EntryType::ScheduledTask, _) => {
            let name = t("rainier.task.name").unwrap_or("?");
            let summary = match t("rainier.task.outcome") {
                Some(outcome) => format!("{name} {outcome}"),
                None => name.to_string(),
            };
            (Some(name.to_string()), summary)
        }
        (EntryType::Mail, _) => {
            let transport = t("mail.transport").unwrap_or("mail");
            let mut summary = format!("mail via {transport}");
            if let Some(n) = num("mail.recipients") {
                summary.push_str(&format!(" to {n} recipient(s)"));
            }
            if config.capture.mail {
                if let Some(subject) = t("mail.subject") {
                    summary.push_str(&format!(": {}", truncate(subject, 200)));
                }
            }
            (Some(transport.to_string()), summary)
        }
        (EntryType::Notification, _) => {
            let name = t("rainier.notification.name").unwrap_or("?");
            (Some(name.to_string()), format!("notification {name}"))
        }
        (EntryType::Event, _) => {
            let name = t("rainier.event.name").unwrap_or("?");
            (Some(name.to_string()), format!("event {name}"))
        }
        (EntryType::View, _) => {
            let name = t("rainier.view.name").unwrap_or("?");
            (Some(name.to_string()), format!("view {name}"))
        }
        (EntryType::AuthAttempt, _) => {
            let guard = t("rainier.auth.guard").unwrap_or("?");
            let outcome = t("rainier.auth.outcome");
            let summary = match outcome {
                Some(outcome) => format!("auth {guard} {outcome}"),
                None => format!("auth {guard}"),
            };
            (Some(guard.to_string()), summary)
        }
        (EntryType::Broadcast, name) => {
            let what = t("rainier.broadcast.event").or(t("rainier.websocket.kind")).unwrap_or(name);
            (Some(format!("{name} {what}")), format!("{name} {what}"))
        }
        (_, name) => {
            let op = t("storage.operation").or(t("otel.name")).unwrap_or(name);
            (Some(name.to_string()), op.to_string())
        }
    };

    let mut content = Map::new();
    for (name, value) in fields {
        if name.starts_with("otel.") && *name != "otel.status_code" {
            continue;
        }
        if *name == "cache.key" && !config.capture.cache_keys {
            continue;
        }
        if name.starts_with("mail.")
            && !matches!(*name, "mail.transport" | "mail.recipients" | "mail.attachments")
            && !config.capture.mail
        {
            continue;
        }
        content.insert((*name).to_string(), value.to_json());
    }
    let mut content = Value::Object(content);
    redact_json(&mut content, &config.redaction);

    Entry {
        id: draft.uid.clone(),
        batch_id: String::new(),
        entry_type: draft.entry_type,
        span_id: Some(draft.uid),
        parent_span_id: parent,
        family_hash: family,
        summary,
        occurred_at: draft.started_at,
        duration_us: Some(duration_us),
        tags,
        content,
    }
}

/// `host/path` with the parts of the path that identify something — numbers,
/// long hex or UUIDs — replaced by `{id}`, and no query string.
fn url_family(url: &str, host: Option<&str>) -> String {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let rest = rest.split(['?', '#']).next().unwrap_or(rest);
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let host = host.unwrap_or(authority);
    let segments: Vec<&str> = path
        .split('/')
        .map(|segment| {
            let looks_like_id = !segment.is_empty()
                && (segment.chars().all(|c| c.is_ascii_digit())
                    || (segment.len() >= 16
                        && segment.chars().all(|c| c.is_ascii_hexdigit() || c == '-')));
            if looks_like_id {
                "{id}"
            } else {
                segment
            }
        })
        .collect();
    format!("{host}/{}", segments.join("/"))
}

/// Runs of digits replaced by `N`, so messages differing only in ids group.
pub(crate) fn normalize_digits(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_digits = false;
    for c in text.chars() {
        if c.is_ascii_digit() {
            if !in_digits {
                out.push('N');
            }
            in_digits = true;
        } else {
            in_digits = false;
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_families_drop_ids_and_queries() {
        assert_eq!(
            url_family("https://api.example.com/v1/users/42/orders?x=1", None),
            "api.example.com/v1/users/{id}/orders"
        );
        assert_eq!(
            url_family("https://h/x/550e8400-e29b-41d4-a716-446655440000", Some("h")),
            "h/x/{id}"
        );
    }

    #[test]
    fn digits_normalise() {
        assert_eq!(normalize_digits("user 42 not found in 7ms"), "user N not found in Nms");
    }
}
