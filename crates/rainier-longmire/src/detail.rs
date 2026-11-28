//! What a Lookout's own viewers read — kept batches whole, entries by type,
//! families, models, insights, one case, the settings — read from one source
//! or across the fleet, and the case actions viewers take.
//!
//! A source answers these only where its Lookout lets Longmire's link reach
//! as far as a viewer (`LOOKOUT_LONGMIRE_REACH=viewer`), or to credentials its
//! viewers' gate admits. Elsewhere it answers 404, and is listed as `unknown`
//! with the reason, like any source that gives nothing.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use chrono::{DateTime, FixedOffset};
use serde_json::{json, Map, Value};

use crate::fleet::{data, filter_params, source_row};
use crate::{Longmire, Part};

/// The longest path segment sent: an id, an entry type, a model's name.
const MAX_SEGMENT: usize = 200;
/// Lookout's own list limits.
const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 200;
const MAX_AGGREGATE_LIMIT: usize = 100;

const BATCH_FILTERS: &[&str] =
    &["kind", "type", "route", "user", "tag", "trace", "exception", "slow", "q", "before", "limit"];
const ENTRY_FILTERS: &[&str] = &["before", "limit", "family", "key", "q"];
const AGGREGATE_PARAMS: &[&str] = &["period", "since", "until", "order", "limit"];
const SERIES_PARAMS: &[&str] = &["since", "until", "period", "family", "key", "keys", "forecast"];
const MODELS_PARAMS: &[&str] = &["since", "period"];
const MODEL_PARAMS: &[&str] = &["before", "limit"];
const INSIGHTS_PARAMS: &[&str] = &["window", "min", "change", "forecast", "limit"];
const TRACES_PARAMS: &[&str] = &["kept", "before", "limit"];

/// Something a Lookout's viewers read, beyond the fleet's [`Endpoint`](crate::Endpoint)s
/// — each with the path segments it names, which are
/// percent-encoded when sent.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Resource {
    /// `/api/batches`: kept batches, newest first, filtered.
    Batches,
    /// `/api/batches/{id}`: one batch whole — every entry, with its content.
    Batch(String),
    /// `/api/entries/{type}`: entries of one type, newest first.
    Entries(String),
    /// `/api/aggregates/{type}`: one type's families, ranked.
    Aggregates(String),
    /// `/api/series/{type}`: one type (or family) over time.
    Series(String),
    /// `/api/families/{type}/{id}`: the key a family id names.
    Family(String, String),
    /// `/api/models`: every model written, with its writes per action.
    Models,
    /// `/api/models/{model}`: one model's changes, newest first.
    Model(String),
    /// `/api/insights`: what changed.
    Insights,
    /// `/api/cases/{id}`: one exception case, with its history.
    Case(String),
    /// `/api/status`: what is recorded, sampled and kept.
    Status,
    /// `/api/traces`: the latest traces this one service took part in.
    Traces,
    /// `/api/traces/{trace}`: this one service's part of a trace.
    Trace(String),
}

impl Resource {
    /// The resource at `tail` (`batches/abc`, `entries/query`), when it is one
    /// and its segments are usable.
    pub fn parse(tail: &str) -> Option<Self> {
        let parts: Vec<&str> = tail.trim_matches('/').split('/').collect();
        let one = |s: &str| usable(s).then(|| s.to_string());
        Some(match parts.as_slice() {
            ["batches"] => Self::Batches,
            ["batches", id] => Self::Batch(one(id)?),
            ["entries", t] => Self::Entries(one(t)?),
            ["aggregates", t] => Self::Aggregates(one(t)?),
            ["series", t] => Self::Series(one(t)?),
            ["families", t, id] => Self::Family(one(t)?, one(id)?),
            ["models"] => Self::Models,
            ["models", m] => Self::Model(one(m)?),
            ["insights"] => Self::Insights,
            ["cases", id] => Self::Case(one(id)?),
            ["status"] => Self::Status,
            ["traces"] => Self::Traces,
            ["traces", id] => Self::Trace(one(id)?),
            _ => return None,
        })
    }

    /// Its path under `{lookout}/api/`, each segment percent-encoded; `None`
    /// when a segment is empty or longer than 200 bytes.
    pub fn tail(&self) -> Option<String> {
        let seg = |s: &str| usable(s).then(|| encode(s));
        Some(match self {
            Self::Batches => "batches".into(),
            Self::Batch(id) => format!("batches/{}", seg(id)?),
            Self::Entries(t) => format!("entries/{}", seg(t)?),
            Self::Aggregates(t) => format!("aggregates/{}", seg(t)?),
            Self::Series(t) => format!("series/{}", seg(t)?),
            Self::Family(t, id) => format!("families/{}/{}", seg(t)?, seg(id)?),
            Self::Models => "models".into(),
            Self::Model(m) => format!("models/{}", seg(m)?),
            Self::Insights => "insights".into(),
            Self::Case(id) => format!("cases/{}", seg(id)?),
            Self::Status => "status".into(),
            Self::Traces => "traces".into(),
            Self::Trace(id) => format!("traces/{}", seg(id)?),
        })
    }

    /// The query parameters forwarded for it.
    pub fn params(&self) -> &'static [&'static str] {
        match self {
            Self::Batches => BATCH_FILTERS,
            Self::Entries(_) => ENTRY_FILTERS,
            Self::Aggregates(_) => AGGREGATE_PARAMS,
            Self::Series(_) => SERIES_PARAMS,
            Self::Models => MODELS_PARAMS,
            Self::Model(_) => MODEL_PARAMS,
            Self::Insights => INSIGHTS_PARAMS,
            Self::Traces => TRACES_PARAMS,
            Self::Batch(_) | Self::Family(..) | Self::Case(_) | Self::Status | Self::Trace(_) => {
                &[]
            }
        }
    }

    /// `raw` with only this resource's parameters, as
    /// [`Endpoint::filter`](crate::Endpoint::filter) does.
    pub fn filter(&self, raw: &str) -> String {
        filter_params(self.params(), raw)
    }
}

fn usable(segment: &str) -> bool {
    !segment.trim().is_empty() && segment.len() <= MAX_SEGMENT
}

/// Everything but the unreserved characters, percent-encoded: nothing in a
/// segment can end it or start a query.
fn encode(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// What a viewer may do to a case: the fields Lookout reads, and nothing else.
fn case_action(action: &Value) -> Result<Vec<u8>, String> {
    let Some(object) = action.as_object() else {
        return Err("A case action is an object: {\"action\", \"severity\"?, \"note\"?}.".into());
    };
    let mut body = Map::new();
    for field in ["action", "severity", "note"] {
        match object.get(field) {
            Some(Value::String(s)) => {
                body.insert(field.into(), json!(s));
            }
            Some(Value::Null) | None => {}
            Some(_) => return Err(format!("The {field} is text.")),
        }
    }
    if !body.contains_key("action") {
        return Err("A case action names its action: resolve, reopen, ignore or severity.".into());
    }
    serde_json::to_vec(&Value::Object(body)).map_err(|_| "The action could not be sent.".into())
}

/// `limit=` from a raw query, within Lookout's bounds.
fn limit_in(raw: &str, default: usize, max: usize) -> usize {
    raw.split('&')
        .find_map(|p| p.strip_prefix("limit=")?.parse::<usize>().ok())
        .unwrap_or(default)
        .clamp(1, max)
}

/// `raw`'s value for `name`, still encoded.
fn param<'a>(raw: &'a str, name: &str) -> Option<&'a str> {
    raw.split('&').find_map(|p| p.strip_prefix(name)?.strip_prefix('='))
}

fn time(value: &Value, at: &str) -> Option<DateTime<FixedOffset>> {
    DateTime::parse_from_rfc3339(value.get(at)?.as_str()?).ok()
}

/// Rows from every source's `data` list, each naming its service.
fn tagged(parts: &[Part]) -> Vec<Value> {
    let mut rows = Vec::new();
    for (source, outcome) in parts {
        let Ok(body) = outcome else { continue };
        for row in data(body).as_array().into_iter().flatten() {
            let mut row = row.clone();
            if let Some(object) = row.as_object_mut() {
                object.insert("service".into(), json!(source.id));
                object.insert("service_name".into(), json!(source.name));
            }
            rows.push(row);
        }
    }
    rows
}

fn services(parts: &[Part]) -> Vec<Value> {
    parts.iter().map(|(source, outcome)| source_row(source, outcome)).collect()
}

/// Every source's newest-first page merged into one, newest first, at most
/// `limit`; `next` is the cursor for the page after, when there is one: any
/// source had more, or the merge left some out.
fn merged_pages(parts: &[Part], at: &str, limit: usize) -> Value {
    let mut rows = tagged(parts);
    rows.sort_by(|a, b| match (time(a, at), time(b, at)) {
        (Some(a), Some(b)) => b.cmp(&a),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    });
    let more_somewhere = parts.iter().any(|(_, outcome)| {
        outcome.as_ref().ok().and_then(|b| b.get("next")).is_some_and(|n| !n.is_null())
    });
    let cut = rows.len() > limit;
    rows.truncate(limit);
    let next = match rows.last().and_then(|last| last.get(at)).and_then(Value::as_str) {
        Some(before) if more_somewhere || cut => json!({ "before": before }),
        _ => Value::Null,
    };
    json!({ "services": services(parts), "data": rows, "next": next })
}

/// An insight's place: the bad first, then the most changed.
fn insight_rank(row: &Value) -> (u8, i64) {
    let severity = match row.get("severity").and_then(Value::as_str) {
        Some("bad") => 0,
        Some("notice") => 1,
        Some("good") => 2,
        _ => 3,
    };
    let change = row
        .get("change_percent")
        .and_then(Value::as_f64)
        .map_or(i64::MIN, |c| -(c.abs() * 100.0) as i64);
    (severity, change)
}

impl Longmire {
    /// One source's answer for `resource`, as its Lookout gave it. `Err` with
    /// the reason when there is none, when `source` is not one, or when a
    /// segment is not usable.
    pub async fn read(
        &self,
        source: &str,
        resource: &Resource,
        raw_query: &str,
    ) -> Result<Value, String> {
        let Some(found) = self.sources.iter().find(|s| s.id == source) else {
            return Err("No such service is a source here.".into());
        };
        let Some(tail) = resource.tail() else {
            return Err("That is not something Lookout can be asked for.".into());
        };
        let query = resource.filter(raw_query);
        tokio::time::timeout(self.timeout, crate::fetch::fetch(self, found, &tail, &query))
            .await
            .unwrap_or_else(|_| {
                Err(format!("No answer within {} seconds.", self.timeout.as_secs()))
            })
    }

    /// Act on one source's case — `{"action": "resolve" | "reopen" |
    /// "ignore" | "severity", "severity"?, "note"?}` — and the case as it now
    /// stands. Over the link the body is signed with the request, so it
    /// cannot be changed on the way. Lookout's own refusal (an unknown
    /// action, a case that is not there) is the `Err`, in its words.
    pub async fn act_on_case(
        &self,
        source: &str,
        case: &str,
        action: &Value,
    ) -> Result<Value, String> {
        let Some(found) = self.sources.iter().find(|s| s.id == source) else {
            return Err("No such service is a source here.".into());
        };
        let Some(tail) = Resource::Case(case.to_string()).tail() else {
            return Err("That is not a case id.".into());
        };
        let body = case_action(action)?;
        tokio::time::timeout(
            self.timeout,
            crate::fetch::send(self, found, "POST", &tail, "", Some(&body)),
        )
        .await
        .unwrap_or_else(|_| Err(format!("No answer within {} seconds.", self.timeout.as_secs())))
    }

    /// Kept batches across every source, newest first: each source asked
    /// with the same filters and cursor (`kind`, `type`, `route`, `user`,
    /// `tag`, `trace`, `exception`, `slow`, `q`, `before`, `limit`), each row
    /// naming its service. `{services, data, next}`; `next.before` is the
    /// cursor for the next page.
    pub async fn batches(&self, raw_query: &str) -> Value {
        let limit = limit_in(raw_query, DEFAULT_LIMIT, MAX_LIMIT);
        let parts = self.ask_all("batches", &Resource::Batches.filter(raw_query)).await;
        merged_pages(&parts, "started_at", limit)
    }

    /// Entries of `entry_type` across every source, newest first (`before`,
    /// `limit`, `family`, `key`, `q`). A family id is the same in every
    /// service — it is the type and key — so `family` follows one query, one
    /// job or one route across the fleet. `{services, data, next}`.
    pub async fn entries(&self, entry_type: &str, raw_query: &str) -> Value {
        let resource = Resource::Entries(entry_type.to_string());
        let Some(tail) = resource.tail() else {
            return json!({ "services": [], "data": [], "next": null });
        };
        let limit = limit_in(raw_query, DEFAULT_LIMIT, MAX_LIMIT);
        let parts = self.ask_all(&tail, &resource.filter(raw_query)).await;
        merged_pages(&parts, "occurred_at", limit)
    }

    /// One type's families across every source, ranked as asked (`order`:
    /// `max` by default, `count`, `average`), at most `limit`, each row
    /// naming its service. `{services, data}`.
    pub async fn aggregates(&self, entry_type: &str, raw_query: &str) -> Value {
        let resource = Resource::Aggregates(entry_type.to_string());
        let Some(tail) = resource.tail() else {
            return json!({ "services": [], "data": [] });
        };
        let parts = self.ask_all(&tail, &resource.filter(raw_query)).await;
        let by = match param(raw_query, "order") {
            Some("count") => "count",
            Some("average") => "average_us",
            _ => "max_us",
        };
        let mut rows = tagged(&parts);
        rows.sort_by(|a, b| {
            let v = |r: &Value| r.get(by).and_then(Value::as_f64).unwrap_or(-1.0);
            v(b).partial_cmp(&v(a)).unwrap_or(Ordering::Equal)
        });
        rows.truncate(limit_in(raw_query, 10, MAX_AGGREGATE_LIMIT));
        json!({ "services": services(&parts), "data": rows })
    }

    /// One type (or, with `family`, one family) over time in every source,
    /// and the fleet's: each source's points under `series`, and the points
    /// added up per bucket under `data` — counts and time summed, the worst
    /// time the worst, the average the sum over the count. Ask with the same
    /// `since` and `period` everywhere (they are forwarded) so the buckets
    /// line up. `{services, data, series, period}`; each source's entry in
    /// `series` is its answer whole (`data`, `forecast`, `period`, and with
    /// `keys=N` a series per busiest key under `keys`).
    pub async fn series(&self, entry_type: &str, raw_query: &str) -> Value {
        let resource = Resource::Series(entry_type.to_string());
        let Some(tail) = resource.tail() else {
            return json!({ "services": [], "data": [], "series": {} });
        };
        let parts = self.ask_all(&tail, &resource.filter(raw_query)).await;
        let mut per_source = Map::new();
        let mut buckets: BTreeMap<String, (u64, u64, u64)> = BTreeMap::new();
        let mut period = Value::Null;
        for (source, outcome) in &parts {
            let Ok(body) = outcome else { continue };
            if period.is_null() {
                period = body.get("period").cloned().unwrap_or(Value::Null);
            }
            let points = data(body).as_array().cloned().unwrap_or_default();
            for point in &points {
                let Some(start) = point.get("bucket_start").and_then(Value::as_str) else {
                    continue;
                };
                let n = |at: &str| point.get(at).and_then(Value::as_u64).unwrap_or(0);
                let key = DateTime::parse_from_rfc3339(start)
                    .map(|t| t.with_timezone(&chrono::Utc).to_rfc3339())
                    .unwrap_or_else(|_| start.to_string());
                let bucket = buckets.entry(key).or_default();
                bucket.0 += n("count");
                bucket.1 += n("sum_us");
                bucket.2 = bucket.2.max(n("max_us"));
            }
            // Each source's answer whole: its points, its projection, its
            // period and, asked with `keys`, a series per busiest key.
            per_source.insert(source.id.clone(), body.clone());
        }
        let fleet: Vec<Value> = buckets
            .into_iter()
            .map(|(start, (count, sum, max))| {
                json!({ "bucket_start": start, "count": count, "sum_us": sum, "max_us": max,
                        "average_us": sum.checked_div(count).unwrap_or(0) })
            })
            .collect();
        json!({ "services": services(&parts), "data": fleet, "series": per_source, "period": period })
    }

    /// Every model written across every source (`since`, `period`), most
    /// written first, each naming its service. `{services, data}`.
    pub async fn models(&self, raw_query: &str) -> Value {
        let parts = self.ask_all("models", &Resource::Models.filter(raw_query)).await;
        let mut rows = tagged(&parts);
        rows.sort_by(|a, b| {
            let v = |r: &Value| r.get("total").and_then(Value::as_u64).unwrap_or(0);
            v(b).cmp(&v(a))
        });
        json!({ "services": services(&parts), "data": rows })
    }

    /// What changed, across every source (`window`, `min`, `change`,
    /// `forecast`, `limit`): the bad first, then the most changed, each
    /// naming its service; and whether each source has insights on.
    /// `{services, data, enabled}`.
    pub async fn insights(&self, raw_query: &str) -> Value {
        let parts = self.ask_all("insights", &Resource::Insights.filter(raw_query)).await;
        let mut rows = tagged(&parts);
        rows.sort_by_key(insight_rank);
        rows.truncate(limit_in(raw_query, 20, MAX_AGGREGATE_LIMIT) * parts.len().max(1));
        let enabled: Map<String, Value> = parts
            .iter()
            .filter_map(|(source, outcome)| {
                let body = outcome.as_ref().ok()?;
                Some((source.id.clone(), body.get("enabled").cloned().unwrap_or(json!(true))))
            })
            .collect();
        json!({ "services": services(&parts), "data": rows, "enabled": enabled })
    }

    /// Every source's settings — store, sample rates, watchers, retention,
    /// insights, and how far the link reaches — one row each, `unknown` with
    /// the reason where there are none. `{services}`, each with `settings`.
    pub async fn status(&self) -> Value {
        let parts = self.ask_all("status", "").await;
        let rows: Vec<Value> = parts
            .iter()
            .map(|(source, outcome)| {
                let mut row = source_row(source, outcome);
                row["settings"] = outcome.as_ref().ok().map(data).cloned().unwrap_or(Value::Null);
                row
            })
            .collect();
        json!({ "services": rows })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resources_round_trip_and_segments_are_encoded() {
        assert_eq!(Resource::parse("batches/abc"), Some(Resource::Batch("abc".into())));
        assert_eq!(
            Resource::parse("families/query/0123456789abcdef"),
            Some(Resource::Family("query".into(), "0123456789abcdef".into()))
        );
        assert_eq!(Resource::parse("batches/a/b"), None);
        assert_eq!(Resource::parse("clear"), None);
        assert_eq!(
            Resource::Model("App\\User?x=1&y".into()).tail().unwrap(),
            "models/App%5CUser%3Fx%3D1%26y"
        );
        assert_eq!(Resource::Batch("../status".into()).tail().unwrap(), "batches/..%2Fstatus");
        assert_eq!(Resource::Batch(String::new()).tail(), None);
        assert_eq!(Resource::Batch("x".repeat(201)).tail(), None);
    }

    #[test]
    fn only_a_resources_own_parameters_are_forwarded() {
        assert_eq!(
            Resource::Batches.filter("kind=request&slow=1&evil=1&q=a%26b&q=second"),
            "kind=request&slow=1&q=a%26b"
        );
        assert_eq!(Resource::Batch("x".into()).filter("q=1"), "");
    }

    #[test]
    fn a_case_action_sends_only_what_lookout_reads() {
        let body =
            case_action(&json!({ "action": "severity", "severity": "high", "who": 1 })).unwrap();
        let sent: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(sent, json!({ "action": "severity", "severity": "high" }));
        assert!(case_action(&json!({ "note": "x" })).is_err());
        assert!(case_action(&json!({ "action": 1 })).is_err());
        assert!(case_action(&json!("resolve")).is_err());
    }
}
