//! Reading and validating the API's query parameters.
//!
//! Every problem is collected, not just the first, and reported in Rainier's
//! validation shape: `422 {"message": "The given data was invalid.",
//! "errors": {"limit": ["…"]}}`.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};
use rainier_support::Error;
use serde_json::Value;

use crate::aggregate::AGGREGATED;
use crate::entry::{BatchKind, EntryType};
use crate::store::{AggregateOrder, AggregateQuery, BatchQuery};

/// The most a list returns.
pub(crate) const MAX_LIMIT: usize = 200;
/// What a list returns when not asked.
pub(crate) const DEFAULT_LIMIT: usize = 50;
/// The most keys an aggregate query returns.
pub(crate) const MAX_AGGREGATE_LIMIT: usize = 100;
/// The longest free-text filter accepted.
const MAX_TEXT: usize = 512;

/// The query string, with the errors found reading it.
pub(crate) struct Params<'a> {
    query: &'a Value,
    errors: BTreeMap<String, Vec<String>>,
}

impl<'a> Params<'a> {
    pub(crate) fn new(query: &'a Value) -> Self {
        Self { query, errors: BTreeMap::new() }
    }

    pub(crate) fn fail(&mut self, field: &str, message: impl Into<String>) {
        self.errors.entry(field.to_string()).or_default().push(message.into());
    }

    /// The raw value, `None` when absent or empty. A repeated key, or one
    /// written `key[]=`, is an error rather than a guess.
    pub(crate) fn raw(&mut self, field: &str) -> Option<String> {
        match self.query.get(field)? {
            Value::String(s) if s.trim().is_empty() => None,
            Value::String(s) => Some(s.trim().to_string()),
            Value::Null => None,
            _ => {
                self.fail(field, format!("The {field} must be a single value."));
                None
            }
        }
    }

    pub(crate) fn text(&mut self, field: &str) -> Option<String> {
        let value = self.raw(field)?;
        if value.chars().count() > MAX_TEXT {
            self.fail(field, format!("The {field} may not be longer than {MAX_TEXT} characters."));
            return None;
        }
        Some(value)
    }

    pub(crate) fn boolean(&mut self, field: &str) -> Option<bool> {
        let value = self.raw(field)?;
        match value.as_str() {
            "1" | "true" | "on" | "yes" => Some(true),
            "0" | "false" | "off" | "no" => Some(false),
            _ => {
                self.fail(field, format!("The {field} must be true or false."));
                None
            }
        }
    }

    pub(crate) fn time(&mut self, field: &str) -> Option<DateTime<Utc>> {
        let value = self.raw(field)?;
        match DateTime::parse_from_rfc3339(&value) {
            Ok(at) => Some(at.with_timezone(&Utc)),
            Err(_) => {
                self.fail(
                    field,
                    format!("The {field} must be an RFC 3339 time, like 2026-11-13T09:30:00Z."),
                );
                None
            }
        }
    }

    pub(crate) fn integer(&mut self, field: &str, min: u64, max: u64) -> Option<u64> {
        let value = self.raw(field)?;
        match value.parse::<u64>() {
            Ok(n) if (min..=max).contains(&n) => Some(n),
            _ => {
                self.fail(field, format!("The {field} must be an integer from {min} to {max}."));
                None
            }
        }
    }

    pub(crate) fn limit(&mut self, max: usize, default: usize) -> usize {
        self.integer("limit", 1, max as u64).map_or(default, |n| n as usize)
    }

    pub(crate) fn kind(&mut self, field: &str) -> Option<BatchKind> {
        let value = self.raw(field)?;
        match parse_kind(&value) {
            Some(kind) => Some(kind),
            None => {
                self.fail(
                    field,
                    format!(
                        "The {field} must be one of: request, job, command, scheduled_task, other."
                    ),
                );
                None
            }
        }
    }

    pub(crate) fn entry_type(&mut self, field: &str) -> Option<EntryType> {
        let value = self.raw(field)?;
        self.parse_type(field, &value)
    }

    pub(crate) fn parse_type(&mut self, field: &str, value: &str) -> Option<EntryType> {
        match EntryType::parse(value) {
            Some(t) => Some(t),
            None => {
                self.fail(field, format!("The {field} is not a known entry type."));
                None
            }
        }
    }

    /// Done reading: the errors, if any, as a `422`.
    pub(crate) fn finish(self) -> Result<(), Error> {
        if self.errors.is_empty() {
            return Ok(());
        }
        Err(Error::validation(serde_json::to_value(self.errors).unwrap_or(Value::Null)))
    }
}

pub(crate) fn parse_kind(value: &str) -> Option<BatchKind> {
    match value {
        "request" => Some(BatchKind::Request),
        "job" => Some(BatchKind::Job),
        "command" => Some(BatchKind::Command),
        "scheduled_task" => Some(BatchKind::ScheduledTask),
        "other" => Some(BatchKind::Other),
        _ => None,
    }
}

/// `GET /api/batches` and the batch list page.
pub(crate) fn batch_query(params: &mut Params<'_>) -> BatchQuery {
    BatchQuery {
        kind: params.kind("kind"),
        entry_type: params.entry_type("type"),
        route_name: params.text("route"),
        user_id: params.text("user"),
        tag: params.text("tag"),
        trace_id: params.text("trace"),
        has_exception: params.boolean("exception"),
        has_slow: params.boolean("slow"),
        before: params.time("before"),
        limit: Some(params.limit(MAX_LIMIT, DEFAULT_LIMIT)),
    }
}

/// `GET /api/entries/{type}`: `(before, limit)`.
pub(crate) fn entries_query(params: &mut Params<'_>) -> (Option<DateTime<Utc>>, usize) {
    (params.time("before"), params.limit(MAX_LIMIT, DEFAULT_LIMIT))
}

/// `GET /api/aggregates/{type}`.
pub(crate) fn aggregate_query(
    params: &mut Params<'_>,
    entry_type: Option<EntryType>,
    now: DateTime<Utc>,
) -> Option<AggregateQuery> {
    if let Some(t) = entry_type {
        if !AGGREGATED.contains(&t) {
            params.fail("type", format!("Entries of type {t} are not aggregated."));
        }
    }
    let period_s = match params.raw("period") {
        None => 3600,
        Some(p) => match p.as_str() {
            "60" => 60,
            "600" => 600,
            "3600" => 3600,
            _ => {
                params.fail("period", "The period must be one of: 60, 600, 3600.");
                3600
            }
        },
    };
    let since = params.time("since").unwrap_or(now - Duration::hours(24));
    let order = match params.raw("order") {
        None => AggregateOrder::Max,
        Some(o) => match o.as_str() {
            "count" => AggregateOrder::Count,
            "average" => AggregateOrder::Average,
            "max" => AggregateOrder::Max,
            _ => {
                params.fail("order", "The order must be one of: count, average, max.");
                AggregateOrder::Max
            }
        },
    };
    let limit = params.limit(MAX_AGGREGATE_LIMIT, 10);
    Some(AggregateQuery { entry_type: entry_type?, period_s, since, order, limit })
}
