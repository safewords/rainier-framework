//! Lookout's event-bus subscriber: activities in, [`Entry`] values out.
//!
//! [`Lookout::subscribe`](crate::Lookout::subscribe) registers one subscriber,
//! `lookout`, on the application's bus. Like every bus subscriber it runs on
//! a thread of its own: the code that did the work only pushed an event onto
//! an inbox, and everything below — building entries, redacting, scoping —
//! happens here, off the request's thread.
//!
//! Every [`Activity`] the framework publishes about its own work — a query, a
//! cache read, the request itself — is heard by its [`kind`](Activity::kind),
//! which is the name of the span the framework emits for the same work, and
//! read by the same field names the span carries:
//!
//! | Activity kind | Entry | Family key |
//! |---|---|---|
//! | `http.server.request` (or `http.request`) | [`Request`](EntryType::Request), the root of a [`BatchKind::Request`] batch | route name, else `METHOD pattern` |
//! | `queue.process` | [`Job`](EntryType::Job), the root of a [`BatchKind::Job`] batch | `rainier.job.name` |
//! | `console.command` | [`Command`](EntryType::Command), the root of a batch | `rainier.command.name` |
//! | `schedule.task` | [`ScheduledTask`](EntryType::ScheduledTask), the root of a batch | `rainier.task.name` |
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
//! | `log` | [`Log`](EntryType::Log), at or above [`LookoutConfig::log_level`] | `level message`, digits as `N` |
//!
//! **Batches are units of work.** An event carries the
//! [`Unit`](rainier_events::Unit) it was published in — a request, a job's
//! run, a console command, a scheduled task — and a batch is one unit: its id
//! is the unit's [`id_string`](rainier_events::Unit::id_string). The batch
//! opens with the first thing heard about the unit and closes when
//! [`UnitCompleted`] arrives, which the bus publishes only once every event of
//! the unit has been handled. The **root** entry is the activity whose kind
//! is the unit's own (`queue.process` in a job's unit); the same kind inside a
//! unit of another kind — a job run synchronously inside a request — is an
//! ordinary entry. Work outside every unit is not recorded.
//!
//! **Nesting.** An activity that other work ran inside names its
//! [`Operation`](rainier_events::Operation) (`Activity::operation`), and that
//! work names it as its [`parent`](Context::parent): that is the waterfall.
//! Work with no parent hangs off the root.
//!
//! **Lookout's own work** — its writer's store calls — runs outside every
//! unit, so what it publishes lands in no batch.
//!
//! [`LookoutConfig::log_level`]: crate::config::LookoutConfig::log_level

use std::backtrace::Backtrace;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use rainier_events::{Activity, Context, Fields, UnitCompleted, Value as FieldValue};
use serde_json::{Map, Value};

use crate::config::LookoutConfig;
use crate::entry::{BatchKind, Entry, EntryType};
use crate::recorder::{new_entry_id, sample, BatchCell, Inner};
use crate::redact::{redact_json, truncate};

/// What an activity kind records as, and the kind of unit it is the root of.
pub(crate) fn classify(kind: &str) -> Option<(EntryType, Option<BatchKind>)> {
    Some(match kind {
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
        _ if kind.starts_with("cache.") => (EntryType::Cache, None),
        _ if kind.starts_with("session.") => (EntryType::Session, None),
        _ if kind.starts_with("broadcast.") || kind.starts_with("websocket.") => {
            (EntryType::Broadcast, None)
        }
        _ if kind.starts_with("storage.") => (EntryType::Custom, None),
        _ => return None,
    })
}

/// An operation or unit id as an entry's span id: sixteen hex digits.
pub(crate) fn span_id(id: u64) -> String {
    format!("{id:016x}")
}

fn render(value: &FieldValue) -> String {
    value.render()
}

fn to_json(value: FieldValue) -> Value {
    match value {
        FieldValue::Str(s) => Value::String(s.into_owned()),
        FieldValue::I64(n) => Value::from(n),
        FieldValue::U64(n) => Value::from(n),
        FieldValue::F64(n) => Value::from(n),
        FieldValue::Bool(b) => Value::Bool(b),
    }
}

fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

impl Inner {
    /// One activity, on the subscriber's thread.
    pub(crate) fn on_activity(&self, activity: &dyn Activity, cx: &Context) {
        let Some(unit) = cx.unit() else { return };
        let kind = activity.kind();
        if kind == "log" {
            if self.log_level.is_none() {
                return;
            }
            let Some(cell) = self.cell_for(unit) else { return };
            self.on_log(&cell, activity, cx);
            return;
        }
        let Some((entry_type, roots)) = classify(kind) else { return };
        let Some(cell) = self.cell_for(unit) else { return };
        if cell.ignored {
            return;
        }
        if let Some(trace) = unit.trace_id() {
            cell.learn_trace(trace);
        }

        let root = roots == Some(cell.kind) && cell.take_root();
        if !root && cell.is_rejected() {
            return;
        }
        let fields = activity.collect_fields();
        let parent = cx.parent().map(span_id);
        let own = activity.operation().map(span_id);

        if root {
            // The root learns the trace and the route; the route stage of the
            // scope runs on them.
            if let Some(trace) = fields.text("trace_id") {
                cell.learn_trace(trace);
            }
            if cell.kind == BatchKind::Request {
                let name = fields.text("rainier.route.name");
                let pattern = fields.text("http.route");
                if name.is_some() || pattern.is_some() {
                    self.set_route(&cell, name, pattern);
                }
            }
            let draft = Draft {
                entry_type,
                kind,
                uid: cell.root_span.clone(),
                fields,
                duration: activity.duration(),
                failed: activity.failed(),
                at: cx.at(),
            };
            let entry = build_entry(&self.config, draft, None);
            self.set_root(&cell, entry);
            return;
        }

        // Remember which operation this one ran inside, so an entry under an
        // operation that is not itself recorded hangs off the nearest one
        // that is.
        if let Some(own) = &own {
            cell.note_operation(own.clone(), parent.clone());
        }
        if !self.wants(entry_type, kind, &fields) {
            return;
        }
        let draft = Draft {
            entry_type,
            kind,
            uid: own.unwrap_or_else(new_entry_id),
            fields,
            duration: activity.duration(),
            failed: activity.failed(),
            at: cx.at(),
        };
        let entry = build_entry(
            &self.config,
            draft,
            Some(parent.unwrap_or_else(|| cell.root_span.clone())),
        );
        let trace = code_trace(&self.config, &cell, &entry, activity.code_trace());
        self.push_traced(&cell, entry, trace);
    }

    /// A `log` activity: a [`Log`](EntryType::Log) entry, at or above the
    /// configured level.
    fn on_log(&self, cell: &BatchCell, activity: &dyn Activity, cx: &Context) {
        let Some(threshold) = self.log_level else { return };
        if cell.ignored || cell.is_rejected() {
            return;
        }
        let fields = activity.collect_fields();
        let level = fields
            .get("level")
            .map(render)
            .map(|l| l.to_ascii_lowercase())
            .unwrap_or_else(|| "info".to_string());
        let Some(parsed) = crate::recorder::parse_level(&level) else { return };
        if parsed > threshold {
            return;
        }
        let watcher = self.config.watcher(EntryType::Log);
        if !watcher.enabled || !sample(watcher.sample_rate) {
            return;
        }
        let message = fields.text("message").unwrap_or_default().to_string();
        if watcher.ignore.iter().any(|p| crate::config::glob(p, &message)) {
            return;
        }
        let mut content = Map::new();
        content.insert("level".into(), Value::String(level.clone()));
        for (name, value) in fields.into_vec() {
            if name == "level" {
                continue;
            }
            content.insert(name.to_string(), to_json(value));
        }
        let mut content = Value::Object(content);
        redact_json(&mut content, &self.config.redaction);
        let entry = Entry {
            id: new_entry_id(),
            batch_id: String::new(),
            entry_type: EntryType::Log,
            span_id: None,
            parent_span_id: Some(
                cx.parent().map(span_id).unwrap_or_else(|| cell.root_span.clone()),
            ),
            family_hash: Some(format!("{level} {}", normalize_digits(&message))),
            summary: truncate(&message, 200),
            occurred_at: DateTime::<Utc>::from(cx.at()),
            duration_us: None,
            tags: vec![level],
            content,
        };
        self.push(cell, entry);
    }

    /// A unit is over and every event of it has been handled.
    pub(crate) fn on_unit_completed(&self, done: &UnitCompleted) {
        let cell = self.open.lock().unwrap_or_else(|e| e.into_inner()).remove(&done.id);
        if let Some(cell) = cell {
            self.close_batch(&cell, Some(done));
        }
    }

    /// Whether a watcher wants this activity at all: on, sampled in, and —
    /// for `event.dispatch` — not one of the framework's own lifecycle events.
    fn wants(&self, entry_type: EntryType, kind: &str, fields: &Fields) -> bool {
        let watcher = self.config.watcher(entry_type);
        if !watcher.enabled || !sample(watcher.sample_rate) {
            return false;
        }
        if kind == "event.dispatch"
            && fields.text("rainier.event.name").is_some_and(|n| n.starts_with("rainier_"))
        {
            return false;
        }
        true
    }
}

/// An activity on its way to being an entry.
struct Draft {
    entry_type: EntryType,
    kind: &'static str,
    uid: String,
    fields: Fields,
    duration: Option<Duration>,
    failed: bool,
    /// When it was published: when it ended.
    at: std::time::SystemTime,
}

/// Turn an activity into an entry.
fn build_entry(config: &LookoutConfig, draft: Draft, parent: Option<String>) -> Entry {
    let duration_us = draft.duration.map(micros);
    let started = match draft.duration {
        Some(d) => draft.at.checked_sub(d).unwrap_or(draft.at),
        None => draft.at,
    };
    let fields = &draft.fields;
    let t = |name: &str| fields.text(name);
    let num = |name: &str| fields.get(name).map(render);
    let failed = draft.failed || t("otel.status_code") == Some("ERROR");
    let mut tags = Vec::new();
    if failed {
        tags.push("failed".to_string());
    }

    let (family, summary) = match (draft.entry_type, draft.kind) {
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
            let sql = t("db.query.text").or(t("otel.name")).unwrap_or(draft.kind);
            (Some(sql.to_string()), truncate(sql, 300))
        }
        (EntryType::Cache, kind) => {
            let op = t("cache.operation")
                .map(str::to_string)
                .unwrap_or_else(|| kind.trim_start_matches("cache.").to_string());
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
            if let Some(FieldValue::Bool(hit)) = fields.get("cache.hit") {
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
        (EntryType::Broadcast, kind) => {
            let what = t("rainier.broadcast.event").or(t("rainier.websocket.kind")).unwrap_or(kind);
            (Some(format!("{kind} {what}")), format!("{kind} {what}"))
        }
        (_, kind) => {
            let op = t("storage.operation").or(t("otel.name")).unwrap_or(kind);
            (Some(kind.to_string()), op.to_string())
        }
    };

    // The fields move into the content: nothing reads them after this.
    let mut content = Map::new();
    for (name, value) in draft.fields.into_vec() {
        if name.starts_with("otel.") && name != "otel.status_code" {
            continue;
        }
        if name == "cache.key" && !config.capture.cache_keys {
            continue;
        }
        if name.starts_with("mail.")
            && !matches!(name, "mail.transport" | "mail.recipients" | "mail.attachments")
            && !config.capture.mail
        {
            continue;
        }
        content.insert(name.to_string(), to_json(value));
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
        occurred_at: DateTime::<Utc>::from(started),
        duration_us,
        tags,
        content,
    }
}

/// The entry types awaited by application code, whose publisher captures a
/// code trace when one is asked for.
pub(crate) const TRACED: &[EntryType] = &[
    EntryType::Query,
    EntryType::Cache,
    EntryType::HttpClient,
    EntryType::Mail,
    EntryType::Notification,
    EntryType::View,
    EntryType::Custom,
];

/// The lowest slow threshold of the traced entry types: what Lookout asks
/// publishers to capture code traces over
/// ([`request_code_traces`](rainier_events::code_trace::request_code_traces)).
pub(crate) fn code_trace_threshold(config: &LookoutConfig) -> Option<Duration> {
    TRACED
        .iter()
        .filter_map(|t| config.watchers.get(t))
        .filter(|w| w.enabled)
        .flat_map(|w| w.slow_ms.into_iter().chain(w.slow_overrides.values().copied()))
        .min()
        .map(Duration::from_millis)
}

/// The stack for a slow entry's `code.trace`: the one its publisher captured
/// on the thread that ran it (see [`rainier_events::code_trace`]), kept when
/// the entry is over its own slow threshold — once per family, and at most
/// [`MAX_CODE_TRACES_PER_BATCH`] a batch. Symbolised later, by the writer.
///
/// [`MAX_CODE_TRACES_PER_BATCH`]: crate::recorder::MAX_CODE_TRACES_PER_BATCH
fn code_trace(
    config: &LookoutConfig,
    batch: &BatchCell,
    entry: &Entry,
    captured: Option<&Arc<Backtrace>>,
) -> Option<Arc<Backtrace>> {
    let captured = captured?;
    if !config.capture.code_traces || !TRACED.contains(&entry.entry_type) {
        return None;
    }
    let duration = entry.duration_us?;
    let threshold = crate::recorder::slow_threshold_ms(config, entry)?;
    if duration < threshold.saturating_mul(1000) || !batch.claim_trace(entry.family_key()) {
        return None;
    }
    Some(Arc::clone(captured))
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

    #[test]
    fn the_code_trace_threshold_is_the_lowest_of_the_traced_watchers() {
        let config = LookoutConfig::default()
            .watch(EntryType::Query, |mut w| {
                w.slow_ms = Some(100);
                w.slow_overrides.insert("SELECT *".into(), 40);
                w
            })
            .watch(EntryType::Request, |mut w| {
                w.slow_ms = Some(1);
                w
            });
        assert_eq!(code_trace_threshold(&config), Some(Duration::from_millis(40)));
    }
}
