//! Linking every source's part of a trace into one.
//!
//! Every outbound call names a span of its own on the wire (request
//! tagging), the caller's Lookout records it as the call's `call.span_id`,
//! and the callee's records it as its batch's `remote_parent_span_id`. So a
//! batch hangs under the call whose span id it names. Pure functions over the
//! sources' answers, so the linking is tested without a network.

use std::collections::{HashMap, HashSet};

use serde_json::{json, Value};

use crate::{valid_trace_id, Source};

/// One source's answer: the JSON it gave, or why there is none.
pub type Part = (Source, Result<Value, String>);

/// The most rows [`merge_recent`] returns.
pub const RECENT_LIMIT: usize = 50;

/// What each source said, for showing which answered.
fn source_rows(parts: &[Part], count: impl Fn(&Value) -> usize) -> Vec<Value> {
    parts
        .iter()
        .map(|(source, outcome)| match outcome {
            Ok(body) => json!({
                "id": source.id, "name": source.name, "status": "ok", "reason": null,
                "count": count(body),
            }),
            Err(reason) => json!({
                "id": source.id, "name": source.name, "status": "unknown", "reason": reason,
                "count": 0,
            }),
        })
        .collect()
}

fn text(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

/// Put the sources' parts of one trace back together.
///
/// Every batch is tagged with its source (`service`, `service_name`) and
/// placed under the outbound call that names its `remote_parent_span_id`;
/// batches with no such call are roots. The result is in tree order — each
/// batch followed by the batches under it, both by start time — each with its
/// `depth` and `parent`. Calls that carried the trace and that no batch here
/// answered are `unanswered`: into a service that let its part go, one that
/// is not a source, or another company's API.
pub fn stitch(trace_id: &str, parts: &[Part]) -> Value {
    struct Node {
        batch: Value,
        started: String,
    }

    let mut nodes: Vec<Node> = Vec::new();
    for (source, outcome) in parts {
        let Ok(body) = outcome else { continue };
        let batches =
            body.pointer("/data/batches").and_then(Value::as_array).cloned().unwrap_or_default();
        for mut batch in batches {
            if !batch.is_object() {
                continue;
            }
            batch["service"] = json!(source.id);
            batch["service_name"] = json!(source.name);
            let started = text(&batch, "started_at").unwrap_or_default();
            nodes.push(Node { batch, started });
        }
    }

    // Every outbound call's span: which batch made it.
    let mut calls: HashMap<String, (usize, String)> = HashMap::new();
    for (index, node) in nodes.iter().enumerate() {
        for span in node.batch.get("spans").and_then(Value::as_array).into_iter().flatten() {
            if let Some(sent) = span.pointer("/call/span_id").and_then(Value::as_str) {
                let own = text(span, "span_id").unwrap_or_default();
                calls.insert(sent.to_string(), (index, own));
            }
        }
    }

    // Each batch's parent: the batch that made the call it answered.
    let mut parent: Vec<Option<usize>> = vec![None; nodes.len()];
    let mut answered: HashSet<String> = HashSet::new();
    for (index, node) in nodes.iter_mut().enumerate() {
        let Some(remote) = text(&node.batch, "remote_parent_span_id") else {
            node.batch["parent"] = Value::Null;
            continue;
        };
        match calls.get(&remote) {
            Some((caller, call_span)) if *caller != index => {
                parent[index] = Some(*caller);
                answered.insert(remote.clone());
                node.batch["parent"] = json!({ "call_span_id": remote, "span_id": call_span });
            }
            _ => node.batch["parent"] = json!({ "call_span_id": remote, "span_id": null }),
        }
    }

    // Tree order: roots by start, each followed by its children by start. A
    // batch whose ancestors loop back to it (a broken id) goes to the root.
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
    let mut roots: Vec<usize> = Vec::new();
    for index in 0..nodes.len() {
        let mut seen = HashSet::from([index]);
        let mut up = parent[index];
        let mut loops = false;
        while let Some(p) = up {
            if !seen.insert(p) {
                loops = true;
                break;
            }
            up = parent[p];
        }
        match parent[index] {
            Some(p) if !loops => children[p].push(index),
            _ => roots.push(index),
        }
    }
    let by_start =
        |list: &mut Vec<usize>| list.sort_by(|a, b| nodes[*a].started.cmp(&nodes[*b].started));
    by_start(&mut roots);
    for list in &mut children {
        by_start(list);
    }
    let mut order: Vec<(usize, usize)> = Vec::new();
    let mut stack: Vec<(usize, usize)> = roots.iter().rev().map(|r| (*r, 0)).collect();
    while let Some((index, depth)) = stack.pop() {
        order.push((index, depth));
        for child in children[index].iter().rev() {
            stack.push((*child, depth + 1));
        }
    }

    let mut unanswered = Vec::new();
    for node in &nodes {
        for span in node.batch.get("spans").and_then(Value::as_array).into_iter().flatten() {
            let Some(sent) = span.pointer("/call/span_id").and_then(Value::as_str) else {
                continue;
            };
            if answered.contains(sent) {
                continue;
            }
            unanswered.push(json!({
                "service": node.batch["service"],
                "batch_id": node.batch["id"],
                "span_id": span["span_id"],
                "call_span_id": sent,
                "summary": span["summary"],
                "host": span.pointer("/call/host"),
                "status": span.pointer("/call/status"),
            }));
        }
    }

    let started_at = nodes.iter().map(|n| n.started.clone()).filter(|s| !s.is_empty()).min();
    let batches: Vec<Value> = order
        .into_iter()
        .map(|(index, depth)| {
            let mut batch = nodes[index].batch.clone();
            batch["depth"] = json!(depth);
            batch
        })
        .collect();
    json!({
        "trace_id": trace_id,
        "started_at": started_at,
        "services": source_rows(parts, |body| {
            body.pointer("/data/batches").and_then(Value::as_array).map_or(0, Vec::len)
        }),
        "batches": batches,
        "unanswered": unanswered,
    })
}

/// Merge the sources' lists of distributed batches into one row per trace,
/// newest first, at most [`RECENT_LIMIT`]: when it started, which services
/// hold a part, the earliest part's summary, the longest duration, whether
/// any part failed or was slow, and whether it was kept whole.
pub fn merge_recent(parts: &[Part]) -> Value {
    struct Row {
        trace_id: String,
        started_at: String,
        summary: Value,
        services: Vec<String>,
        batches: usize,
        duration_us: u64,
        has_exception: bool,
        has_slow: bool,
        kept: bool,
        failed: bool,
    }

    let mut rows: Vec<Row> = Vec::new();
    for (source, outcome) in parts {
        let Ok(body) = outcome else { continue };
        for item in body.get("data").and_then(Value::as_array).into_iter().flatten() {
            let Some(trace_id) = text(item, "trace_id").filter(|t| valid_trace_id(t)) else {
                continue;
            };
            let started = text(item, "started_at").unwrap_or_default();
            let index = match rows.iter().position(|r| r.trace_id == trace_id) {
                Some(index) => index,
                None => {
                    rows.push(Row {
                        trace_id,
                        started_at: started.clone(),
                        summary: item["summary"].clone(),
                        services: Vec::new(),
                        batches: 0,
                        duration_us: 0,
                        has_exception: false,
                        has_slow: false,
                        kept: false,
                        failed: false,
                    });
                    rows.len() - 1
                }
            };
            let row = &mut rows[index];
            if !started.is_empty() && started < row.started_at {
                row.started_at = started;
                row.summary = item["summary"].clone();
            }
            if !row.services.contains(&source.id) {
                row.services.push(source.id.clone());
            }
            row.batches += 1;
            row.duration_us = row.duration_us.max(item["duration_us"].as_u64().unwrap_or(0));
            row.has_exception |= item["has_exception"].as_bool().unwrap_or(false);
            row.has_slow |= item["has_slow"].as_bool().unwrap_or(false);
            row.kept |= item["kept"].as_bool().unwrap_or(false);
            row.failed |= item["status"].as_u64().is_some_and(|s| s >= 500);
        }
    }
    rows.sort_by(|a, b| b.started_at.cmp(&a.started_at));
    rows.truncate(RECENT_LIMIT);
    let traces: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "trace_id": r.trace_id,
                "started_at": r.started_at,
                "summary": r.summary,
                "services": r.services,
                "batches": r.batches,
                "duration_us": r.duration_us,
                "has_exception": r.has_exception,
                "has_slow": r.has_slow,
                "failed": r.failed,
                "kept": r.kept,
            })
        })
        .collect();
    json!({
        "services": source_rows(parts, |body| body.get("data").and_then(Value::as_array).map_or(0, Vec::len)),
        "traces": traces,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRACE: &str = "4bf92f3577b34da6a3ce929d0e0e4736";

    fn call(span: &str, sent: &str, host: &str) -> Value {
        json!({ "span_id": span, "type": "http_client", "summary": format!("GET http://{host}/x"),
                "call": { "span_id": sent, "host": host, "status": 200 } })
    }

    fn batch(id: &str, started: &str, remote: Option<&str>, spans: Vec<Value>) -> Value {
        json!({ "id": id, "started_at": started, "remote_parent_span_id": remote, "spans": spans })
    }

    fn part(id: &str, batches: Vec<Value>) -> Part {
        (
            Source::new(id, format!("http://{id}.test")),
            Ok(json!({ "data": { "trace_id": TRACE, "batches": batches } })),
        )
    }

    #[test]
    fn a_trace_across_three_services_is_one_tree_in_call_order() {
        let parts = vec![
            // Listed callee first: the order sources answer in does not matter.
            part(
                "billing",
                vec![batch(
                    "b1",
                    "2026-11-27T10:00:00.020Z",
                    Some("c-billing"),
                    vec![call("s-ledger", "c-ledger", "ledger")],
                )],
            ),
            part(
                "orders",
                vec![batch(
                    "o1",
                    "2026-11-27T10:00:00.000Z",
                    None,
                    vec![
                        call("s-billing", "c-billing", "billing"),
                        call("s-mail", "c-mail", "mail.example.com"),
                    ],
                )],
            ),
            part("ledger", vec![batch("l1", "2026-11-27T10:00:00.030Z", Some("c-ledger"), vec![])]),
        ];

        let trace = stitch(TRACE, &parts);
        let order: Vec<(&str, u64)> = trace["batches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| (b["id"].as_str().unwrap(), b["depth"].as_u64().unwrap()))
            .collect();
        assert_eq!(order, [("o1", 0), ("b1", 1), ("l1", 2)]);
        assert_eq!(trace["batches"][1]["service"], "billing");
        assert_eq!(trace["batches"][1]["parent"]["span_id"], "s-billing", "under the call");
        assert_eq!(trace["batches"][0]["parent"], Value::Null);
        assert_eq!(trace["started_at"], "2026-11-27T10:00:00.000Z");

        let unanswered = trace["unanswered"].as_array().unwrap();
        assert_eq!(unanswered.len(), 1, "only the call out of our services");
        assert_eq!(unanswered[0]["host"], "mail.example.com");
    }

    #[test]
    fn a_silent_source_is_named_and_its_callers_calls_are_unanswered() {
        let parts = vec![
            part(
                "orders",
                vec![batch("o1", "2026-11-27T10:00:00Z", None, vec![call("s1", "c1", "billing")])],
            ),
            (
                Source::new("billing", "http://billing.test"),
                Err("Lookout answered 404.".to_string()),
            ),
        ];

        let trace = stitch(TRACE, &parts);
        assert_eq!(trace["services"][1]["status"], "unknown");
        assert_eq!(trace["services"][1]["reason"], "Lookout answered 404.");
        assert_eq!(trace["services"][0]["count"], 1);
        assert_eq!(trace["unanswered"][0]["call_span_id"], "c1");
    }

    #[test]
    fn a_batch_whose_caller_is_not_a_source_is_a_root() {
        let parts = vec![part(
            "billing",
            vec![batch("b1", "2026-11-27T10:00:00Z", Some("elsewhere"), vec![])],
        )];
        let trace = stitch(TRACE, &parts);
        assert_eq!(trace["batches"][0]["depth"], 0);
        assert_eq!(trace["batches"][0]["parent"]["span_id"], Value::Null);
    }

    #[test]
    fn ids_that_loop_do_not_hang_the_stitch() {
        let parts = vec![part(
            "a",
            vec![
                batch("x", "2026-11-27T10:00:00Z", Some("cy"), vec![call("sx", "cx", "a")]),
                batch("y", "2026-11-27T10:00:01Z", Some("cx"), vec![call("sy", "cy", "a")]),
            ],
        )];
        let trace = stitch(TRACE, &parts);
        assert_eq!(trace["batches"].as_array().unwrap().len(), 2, "both shown once");
    }

    #[test]
    fn recent_traces_are_one_row_per_trace_newest_first() {
        let item = |trace: &str, at: &str, summary: &str, status: u64, kept: bool| {
            json!({ "trace_id": trace, "started_at": at, "summary": summary, "status": status,
                    "duration_us": 1000, "has_exception": false, "has_slow": false, "kept": kept })
        };
        let parts = vec![
            (
                Source::new("orders", "http://orders.test"),
                Ok(json!({ "data": [
                    item("aaaa", "2026-11-27T10:00:00Z", "GET /checkout 200", 200, true),
                    item("bbbb", "2026-11-27T11:00:00Z", "GET /cart 200", 200, false),
                ]})),
            ),
            (
                Source::new("billing", "http://billing.test"),
                Ok(json!({ "data": [
                    item("aaaa", "2026-11-27T10:00:01Z", "POST /charge 500", 500, true),
                    item("not a trace!", "2026-11-27T12:00:00Z", "x", 200, false),
                ]})),
            ),
            (
                Source::new("mail", "http://mail.test"),
                Err("No answer within 8 seconds.".to_string()),
            ),
        ];

        let merged = merge_recent(&parts);
        let traces = merged["traces"].as_array().unwrap();
        assert_eq!(traces.len(), 2);
        assert_eq!(traces[0]["trace_id"], "bbbb", "newest first");
        let checkout = &traces[1];
        assert_eq!(checkout["summary"], "GET /checkout 200", "the first part's summary");
        assert_eq!(checkout["services"], json!(["orders", "billing"]));
        assert_eq!(checkout["failed"], true);
        assert_eq!(checkout["kept"], true);
        assert_eq!(merged["services"][2]["status"], "unknown");
    }
}
