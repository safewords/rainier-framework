//! Model changes, heard on the event bus; and the queue's lifecycle events,
//! recognised by their type names.
//!
//! # Model changes
//!
//! Boot forwards every connection's committed writes to the application's
//! event bus as [`ModelChanged`] — every write a statement builder or
//! `rainier_orm::repo` makes, including the ones no typed hook sees: a
//! repository built without `with_events`, `update_matching`,
//! `delete_matching`, a counter increment. Lookout is one listener of it,
//! like any other ([`Lookout::listen`] registers it), and each change becomes
//! a [`Model`](EntryType::Model) entry in the batch whose work made it — the
//! event is dispatched on the task that wrote, inside the request's or the
//! job's span:
//!
//! - **model** — the entity's short type name, `Post`; its full path is
//!   `model_type`. A change described by table alone is named by its table.
//! - **action** — `created`, `updated`, `upserted` or `deleted`.
//! - **keys** — the primary keys written, when the write named them (a row
//!   written by key, an update pinned to `id = 7`); `null` for a bulk write
//!   that did not say which rows.
//! - **batch** — added when the batch closes: its kind, summary and trace id,
//!   so a list of one model's changes can say what made each.
//!
//! The family key is `"<model> <action>"`, so the aggregates count every
//! model's writes per action, in batches that were kept and in those that were
//! only counted. What a row held before or after is not recorded: the ORM
//! keeps no copy of the original row. A rolled-back write is never announced;
//! SQL passed as a string is not seen; a write with no batch around it
//! (Lookout's own store, a task outside any request or job) records nothing.

use chrono::Utc;
use rainier_database::{ModelAction, ModelChanged};

use crate::entry::{Entry, EntryType};
use crate::recorder::{new_entry_id, Lookout};

/// The family key of `model`'s `action`: `Post updated`.
pub fn family_key(model: &str, action: &str) -> String {
    format!("{model} {action}")
}

/// Split a family key back into its model and action: the inverse of
/// [`family_key`], `None` for a key that is not one.
pub fn split_family_key(key: &str) -> Option<(&str, &str)> {
    let (model, action) = key.rsplit_once(' ')?;
    let known = ModelAction::ALL.iter().any(|a| a.as_str() == action);
    (known && !model.is_empty()).then_some((model, action))
}

/// The entry for a model change.
pub fn entry(change: &ModelChanged) -> Entry {
    let action = change.action.as_str();
    let model = change.model.as_str();
    let which = match change.keys.as_deref() {
        Some([key]) => format!(" #{key}"),
        Some(keys) => format!(" ({} rows)", keys.len()),
        None => String::new(),
    };
    Entry {
        id: new_entry_id(),
        batch_id: String::new(),
        entry_type: EntryType::Model,
        span_id: None,
        parent_span_id: None,
        family_hash: Some(family_key(model, action)),
        summary: format!("{model} {action}{which}"),
        occurred_at: Utc::now(),
        duration_us: None,
        tags: vec![action.to_string()],
        content: serde_json::json!({
            "action": action,
            "model": model,
            "model_type": change.model_type,
            "table": change.table,
            "keys": change.keys,
        }),
    }
}

/// One model's writes over a window, from the aggregates: what the Models
/// page lists. Counted in every batch, kept or not.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ModelSummary {
    /// The model's short name: `Post`.
    pub model: String,
    /// Writes of every action.
    pub total: u64,
    /// Writes per action — `created`, `updated`, `upserted`, `deleted` —
    /// with the actions it had none of left out.
    pub actions: std::collections::BTreeMap<String, u64>,
    /// The start of the newest bucket it was written in: when it last
    /// changed, to the bucket.
    pub last_bucket: chrono::DateTime<Utc>,
}

/// Group [`Model`](EntryType::Model) aggregate rows by model, most written
/// first (then by name). Rows of other types, and keys that are not a model
/// change's, are ignored.
pub fn summarize(rows: &[crate::entry::Aggregate]) -> Vec<ModelSummary> {
    let mut models: std::collections::BTreeMap<&str, ModelSummary> = Default::default();
    for row in rows.iter().filter(|r| r.entry_type == EntryType::Model) {
        let Some((model, action)) = split_family_key(&row.key) else { continue };
        let summary = models.entry(model).or_insert_with(|| ModelSummary {
            model: model.to_string(),
            total: 0,
            actions: Default::default(),
            last_bucket: row.bucket_start,
        });
        summary.total += row.count;
        *summary.actions.entry(action.to_string()).or_insert(0) += row.count;
        summary.last_bucket = summary.last_bucket.max(row.bucket_start);
    }
    let mut out: Vec<ModelSummary> = models.into_values().collect();
    out.sort_by(|a, b| b.total.cmp(&a.total).then_with(|| a.model.cmp(&b.model)));
    out
}

impl Lookout {
    /// Record one model change in the current batch — what Lookout's
    /// [`ModelChanged`] listener does. A no-op outside a batch, or with the
    /// model watcher off.
    ///
    /// Public so a write the bus does not hear — SQL passed as a string — can
    /// be described and recorded by the code that made it. Dispatching the
    /// [`ModelChanged`] yourself does the same, and every other listener hears
    /// it too.
    pub fn record_model_change(&self, change: &ModelChanged) {
        let inner = &self.inner;
        if !inner.active() {
            return;
        }
        let Some(cell) = inner.current_batch() else { return };
        let watcher = inner.config.watcher(EntryType::Model);
        if !watcher.enabled || !crate::recorder::sample(watcher.sample_rate) {
            return;
        }
        let entry = entry(change);
        if watcher.ignore.iter().any(|p| crate::config::glob(p, &entry.summary)) {
            return;
        }
        inner.push(&cell, entry);
    }
}

/// A queue worker's lifecycle event, from its type name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobEvent {
    /// `JobFailed`: the last attempt failed.
    Failed,
    /// `JobReleased`: it failed and goes back on the queue.
    Released,
}

/// Parse `rainier_queue::worker::JobFailed` and `…::JobReleased`.
pub fn job_event(name: &str) -> Option<JobEvent> {
    if !name.starts_with("rainier_queue::") {
        return None;
    }
    match name.rsplit("::").next()? {
        "JobFailed" => Some(JobEvent::Failed),
        "JobReleased" => Some(JobEvent::Released),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_change_becomes_an_entry_named_by_its_model() {
        let change = ModelChanged {
            model: "Post".into(),
            model_type: Some("shop::models::Post"),
            table: "posts".into(),
            action: ModelAction::Updated,
            keys: Some(vec!["7".into()]),
        };
        let recorded = entry(&change);
        assert_eq!(recorded.summary, "Post updated #7");
        assert_eq!(recorded.family_key(), "Post updated");
        assert_eq!(recorded.content["model"], "Post");
        assert_eq!(recorded.content["model_type"], "shop::models::Post");
        assert_eq!(recorded.content["table"], "posts");
        assert_eq!(recorded.content["keys"], serde_json::json!(["7"]));

        let bulk = entry(&ModelChanged {
            model: "audit_log".into(),
            model_type: None,
            table: "audit_log".into(),
            action: ModelAction::Deleted,
            keys: None,
        });
        assert_eq!(bulk.summary, "audit_log deleted");
        assert_eq!(bulk.content["keys"], serde_json::Value::Null);
        assert_eq!(bulk.content["model_type"], serde_json::Value::Null);
    }

    #[test]
    fn aggregates_group_by_model_most_written_first() {
        let row = |key: &str, count: u64, minute: i64| crate::entry::Aggregate {
            bucket_start: chrono::DateTime::from_timestamp(minute * 60, 0).unwrap(),
            period_s: 60,
            entry_type: EntryType::Model,
            key: key.into(),
            count,
            sum_us: 0,
            max_us: 0,
            histogram: Default::default(),
        };
        let mut query = row("select ?", 99, 1);
        query.entry_type = EntryType::Query;
        let rows = [
            row("Post created", 2, 1),
            row("Post updated", 5, 3),
            row("Post created", 1, 2),
            row("Comment deleted", 9, 1),
            row("Tag created", 1, 1),
            query,
        ];

        let summaries = summarize(&rows);
        let names: Vec<&str> = summaries.iter().map(|s| s.model.as_str()).collect();
        assert_eq!(names, ["Comment", "Post", "Tag"]);
        let post = &summaries[1];
        assert_eq!(post.total, 8);
        assert_eq!(post.actions.get("created"), Some(&3));
        assert_eq!(post.actions.get("updated"), Some(&5));
        assert_eq!(post.actions.get("deleted"), None);
        assert_eq!(post.last_bucket.timestamp(), 180);
    }

    #[test]
    fn family_keys_split_back_into_model_and_action() {
        assert_eq!(split_family_key("Post updated"), Some(("Post", "updated")));
        assert_eq!(split_family_key(&family_key("Post", "deleted")), Some(("Post", "deleted")));
        assert_eq!(split_family_key("Post touched"), None);
        assert_eq!(split_family_key("updated"), None);
    }

    #[test]
    fn job_event_names_parse() {
        assert_eq!(job_event("rainier_queue::worker::JobFailed"), Some(JobEvent::Failed));
        assert_eq!(job_event("rainier_queue::worker::JobReleased"), Some(JobEvent::Released));
        assert_eq!(job_event("rainier_queue::worker::JobProcessed"), None);
        assert_eq!(job_event("app::JobFailed"), None);
    }
}
