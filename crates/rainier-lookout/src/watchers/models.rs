//! Model and job lifecycle events, recognised by their type names.
//!
//! `rainier_database`'s model events are generic — `Created<M>`,
//! `Updated<M>`, `Deleted<M>` — and the bus keys listeners by concrete type,
//! so one typed listener cannot hear every model. Their names can:
//! `rainier_database::model::Created<shop::models::Order>` says what happened
//! and to which model. That is what a [`Model`](EntryType::Model) entry
//! records — the action and the model's name — without Lookout depending on
//! the database crate. The row's key is not recorded (the payload is opaque
//! here).
//!
//! The before-events (`Creating`, …) are not recorded: the after-event
//! follows, or the write was vetoed and nothing changed.

use chrono::Utc;

use crate::entry::{Entry, EntryType};
use crate::recorder::new_entry_id;

/// A model event, parsed from its type name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelEvent {
    /// `created`, `updated` or `deleted`.
    pub action: &'static str,
    /// The model's short name: `Order`.
    pub model: String,
    /// The model's full type path: `shop::models::Order`.
    pub model_path: String,
}

/// Parse `rainier_database::model::Created<shop::Order>`.
pub fn parse(name: &str) -> Option<ModelEvent> {
    let rest = name.strip_prefix("rainier_database::")?;
    let (head, generic) = rest.split_once('<')?;
    let action = match head.rsplit("::").next()? {
        "Created" => "created",
        "Updated" => "updated",
        "Deleted" => "deleted",
        _ => return None,
    };
    let model_path = generic.strip_suffix('>')?.trim().to_string();
    let model = model_path
        .split('<')
        .next()
        .and_then(|p| p.rsplit("::").next())
        .unwrap_or(&model_path)
        .to_string();
    Some(ModelEvent { action, model, model_path })
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

/// The entry for a model event.
pub fn entry(event: &ModelEvent) -> Entry {
    Entry {
        id: new_entry_id(),
        batch_id: String::new(),
        entry_type: EntryType::Model,
        span_id: None,
        parent_span_id: None,
        family_hash: Some(format!("{} {}", event.model, event.action)),
        summary: format!("{} {}", event.model, event.action),
        occurred_at: Utc::now(),
        duration_us: None,
        tags: vec![event.action.to_string()],
        content: serde_json::json!({
            "action": event.action,
            "model": event.model,
            "model_type": event.model_path,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_event_names_parse() {
        let event = parse("rainier_database::model::Created<shop::models::Order>").expect("parses");
        assert_eq!(event.action, "created");
        assert_eq!(event.model, "Order");
        assert_eq!(event.model_path, "shop::models::Order");
        assert_eq!(parse("rainier_database::model::Deleted<Post>").expect("parses").model, "Post");
        assert!(parse("rainier_database::model::Creating<Post>").is_none());
        assert!(parse("shop::Created<Post>").is_none());
    }

    #[test]
    fn job_event_names_parse() {
        assert_eq!(job_event("rainier_queue::worker::JobFailed"), Some(JobEvent::Failed));
        assert_eq!(job_event("rainier_queue::worker::JobReleased"), Some(JobEvent::Released));
        assert_eq!(job_event("rainier_queue::worker::JobProcessed"), None);
        assert_eq!(job_event("app::JobFailed"), None);
    }
}
