//! Failures as entries.
//!
//! The content is the whole [`Failure`] as JSON — its kind, its typed fields
//! (method and path, job and attempt, …), the [`Exception`] and the
//! [`ReportMeta`](rainier_events::report::ReportMeta) — redacted by field name
//! and truncated, with the backtrace cut to [`BACKTRACE_LINES`] lines.
//!
//! The family key is a **fingerprint**: what makes two failures "the same
//! one" for grouping and counting —
//! `failure kind | exception kind | message with digits as N | first in-app frame`.

use chrono::{TimeZone, Utc};
use rainier_events::report::{Exception, Failure};
use serde_json::Value;

use crate::config::LookoutConfig;
use crate::entry::{Entry, EntryType};
use crate::recorder::new_entry_id;
use crate::redact::{redact_json, truncate};
use crate::subscriber::normalize_digits;

/// How many lines of a backtrace are kept.
pub const BACKTRACE_LINES: usize = 60;

/// Frames from these crates are the framework or the runtime, not the
/// application.
const FOREIGN_FRAMES: &[&str] = &[
    "std::",
    "core::",
    "alloc::",
    "tokio::",
    "futures",
    "hyper",
    "tower",
    "backtrace::",
    "rainier_",
    "async_trait",
    "<",
    "__rust",
    "rust_begin_unwind",
    // The test harness, and the runtime and library crates a request passes
    // through on its way to application code: none of them is where the
    // failure is, and a fingerprint keyed on one would change with the call
    // path rather than the bug.
    "test::",
    "tracing",
    "pin_project",
    "sea_orm",
    "sqlx",
    "serde",
    "hashbrown",
    "anyhow::",
];

/// The entry for `failure`.
pub fn entry(config: &LookoutConfig, failure: &Failure) -> Entry {
    let exception = failure.exception();
    let mut content = serde_json::to_value(failure).unwrap_or(Value::Null);
    if let Some(map) = content.as_object_mut() {
        map.insert("kind_label".into(), Value::String(exception.kind_label()));
    }
    if let Some(trace) =
        content.get_mut("exception").and_then(|e| e.get_mut("backtrace")).filter(|b| b.is_string())
    {
        let lines: Vec<&str> = trace.as_str().unwrap_or_default().lines().collect();
        let mut kept = lines[..lines.len().min(BACKTRACE_LINES)].join("\n");
        if lines.len() > BACKTRACE_LINES {
            kept.push_str(&format!("\n… {} more lines", lines.len() - BACKTRACE_LINES));
        }
        *trace = Value::String(kept);
    }
    // Long backtraces are cut by lines above; the generic truncation must not
    // then cut them to a couple of kilobytes.
    let backtrace = content
        .get_mut("exception")
        .and_then(|e| e.as_object_mut())
        .and_then(|e| e.remove("backtrace"));
    let frames = backtrace.as_ref().and_then(Value::as_str).map(|t| app_frames(t, 30));
    redact_json(&mut content, &config.redaction);
    if let (Some(frames), Some(map)) = (frames, content.as_object_mut()) {
        if !frames.is_empty() {
            map.insert("frames".into(), Value::Array(frames));
        }
    }
    if let (Some(backtrace), Some(e)) =
        (backtrace, content.get_mut("exception").and_then(|e| e.as_object_mut()))
    {
        e.insert("backtrace".into(), backtrace);
    }

    let meta = failure.meta();
    let occurred_at = i64::try_from(meta.reported_at_ms)
        .ok()
        .filter(|ms| *ms > 0)
        .and_then(|ms| Utc.timestamp_millis_opt(ms).single())
        .unwrap_or_else(Utc::now);

    let mut tags = vec![failure.kind().as_str().to_string()];
    if exception.panic {
        tags.push("panic".to_string());
    }
    if exception.is_server_error() {
        tags.push("failed".to_string());
    }

    Entry {
        id: new_entry_id(),
        batch_id: String::new(),
        entry_type: EntryType::Exception,
        span_id: None,
        parent_span_id: None,
        family_hash: Some(fingerprint(failure)),
        summary: truncate(&format!("{}: {}", exception.kind_label(), exception.message), 300),
        occurred_at,
        duration_us: None,
        tags,
        content,
    }
}

/// What makes two failures the same one.
pub fn fingerprint(failure: &Failure) -> String {
    let exception = failure.exception();
    let mut key = format!(
        "{}|{}|{}",
        failure.kind().as_str(),
        exception.kind_label(),
        normalize_digits(&truncate(&exception.message, 500)),
    );
    if let Some(frame) = first_app_frame(exception) {
        key.push('|');
        key.push_str(&frame);
    }
    key
}

/// The application's own frames in a rendered backtrace, outermost last, at
/// most `limit`: `{"function": "shop::orders::show", "file": "./src/orders.rs",
/// "line": 9}`, `file` and `line` only when the binary has line tables.
///
/// A stripped binary renders every frame as `<unknown>`, which gives none.
pub fn app_frames(trace: &str, limit: usize) -> Vec<Value> {
    let mut frames: Vec<serde_json::Map<String, Value>> = Vec::new();
    let mut last_was_app = false;
    for line in trace.lines() {
        let line = line.trim();
        if let Some(location) = line.strip_prefix("at ") {
            if last_was_app {
                if let Some(frame) = frames.last_mut() {
                    if frame.contains_key("file") {
                        continue;
                    }
                    // `./src/orders.rs:9:5` — the column is dropped.
                    let mut parts = location.rsplitn(3, ':');
                    let column_or_line = parts.next();
                    let maybe_line = parts.next();
                    let (file, line_no) = match (maybe_line, parts.next()) {
                        (Some(l), Some(file)) if l.chars().all(|c| c.is_ascii_digit()) => {
                            (file, l.parse::<u64>().ok())
                        }
                        _ => match (column_or_line, maybe_line) {
                            (Some(l), Some(file)) => (file, l.parse::<u64>().ok()),
                            _ => (location, None),
                        },
                    };
                    frame.insert("file".into(), Value::String(file.to_string()));
                    if let Some(n) = line_no {
                        frame.insert("line".into(), Value::from(n));
                    }
                }
            }
            continue;
        }
        let Some((index, symbol)) = line.split_once(": ") else { continue };
        if !index.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let symbol = symbol.trim();
        last_was_app =
            symbol.contains("::") && !FOREIGN_FRAMES.iter().any(|p| symbol.starts_with(p));
        if !last_was_app {
            continue;
        }
        if frames.len() >= limit {
            break;
        }
        let symbol = match symbol.rsplit_once("::h") {
            Some((head, hash)) if hash.chars().all(|c| c.is_ascii_hexdigit()) => head,
            _ => symbol,
        };
        // Consecutive frames of one async fn (`{{closure}}` polls) are one.
        let function = without_closures(symbol).to_string();
        if frames.last().and_then(|f| f.get("function")).and_then(Value::as_str)
            == Some(function.as_str())
        {
            last_was_app = false;
            continue;
        }
        let mut frame = serde_json::Map::new();
        frame.insert("function".into(), Value::String(function));
        frames.push(frame);
    }
    frames.into_iter().map(Value::Object).collect()
}

/// `symbol` without its trailing closure segments: `{{closure}}` from older
/// compilers, `{closure#0}` from newer ones.
fn without_closures(mut symbol: &str) -> &str {
    loop {
        let Some((head, last)) = symbol.rsplit_once("::") else { return symbol };
        if last == "{{closure}}" || (last.starts_with("{closure#") && last.ends_with('}')) {
            symbol = head;
        } else {
            return symbol;
        }
    }
}

/// The first frame of the backtrace that is the application's own code.
pub fn first_app_frame(exception: &Exception) -> Option<String> {
    let trace = exception.backtrace.as_deref()?;
    trace.lines().find_map(|line| {
        // `  12: app::handlers::show` — a frame's symbol line.
        let (index, symbol) = line.trim().split_once(": ")?;
        if !index.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        let symbol = symbol.trim();
        // An application frame is a Rust path (`shop::orders::show`); libc's
        // and the OS's (`__pthread_cond_wait`, `start_thread`) are not.
        if !symbol.contains("::") || FOREIGN_FRAMES.iter().any(|p| symbol.starts_with(p)) {
            return None;
        }
        // Drop the `::h0123abcd` hash suffix, which changes per build.
        let symbol = match symbol.rsplit_once("::h") {
            Some((head, hash)) if hash.chars().all(|c| c.is_ascii_hexdigit()) => head,
            _ => symbol,
        };
        Some(symbol.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_events::report::{ApplicationFailed, HttpRequestFailed};
    use rainier_support::Error;

    #[test]
    fn app_frames_keep_the_applications_own_with_their_locations() {
        let trace = "   0: std::backtrace::Backtrace::force_capture\n\
                     \x20 1: rainier_lookout::layer::on_close\n\
                     \x20            at ./crates/rainier-lookout/src/layer.rs:360:9\n\
                     \x20 2: shop::orders::load::{{closure}}::h0123456789abcdef\n\
                     \x20            at ./src/orders.rs:42:17\n\
                     \x20 3: shop::orders::load::{{closure}}\n\
                     \x20            at ./src/orders.rs:40:1\n\
                     \x20 4: tokio::runtime::task::poll\n\
                     \x20 5: shop::http::show::{closure#0}::{closure#1}\n\
                     \x20 6: <unknown>";
        let frames = app_frames(trace, 10);
        assert_eq!(
            frames,
            vec![
                serde_json::json!({ "function": "shop::orders::load", "file": "./src/orders.rs", "line": 42 }),
                serde_json::json!({ "function": "shop::http::show" }),
            ]
        );
        assert_eq!(app_frames(trace, 1).len(), 1);
        assert!(app_frames("   0: <unknown>\n   1: <unknown>", 10).is_empty());
    }

    #[test]
    fn fingerprints_ignore_numbers_and_keep_the_first_app_frame() {
        // Without a stack: `RUST_BACKTRACE`, which CI sets, would give these
        // one, and which frame it starts at is not what this checks.
        let quiet = |message: &str| {
            let mut failure = ApplicationFailed::new(Error::internal(message.to_string()));
            failure.exception.backtrace = None;
            Failure::from(failure)
        };
        let a = quiet("order 42 is missing");
        let b = quiet("order 977 is missing");
        assert_eq!(fingerprint(&a), fingerprint(&b));
        assert_eq!(fingerprint(&a), "Application|Internal|order N is missing");

        let mut exception = Exception::panic("boom", None);
        exception.backtrace = Some(
            "   0: std::panicking::begin_panic\n   1: rainier_server::kernel::handle\n   2: shop::orders::show::h0123456789abcdef\n             at ./src/orders.rs:9:5"
                .to_string(),
        );
        assert_eq!(first_app_frame(&exception).as_deref(), Some("shop::orders::show"));

        // A stack that is all harness and runtime has no application frame.
        exception.backtrace = Some(
            "   0: std::panicking::begin_panic\n   1: test::__rust_begin_short_backtrace::<core::result::Result<(), alloc::string::String>>\n   2: tokio::runtime::task::raw::poll\n   3: tracing::instrument::Instrumented<T>::poll\n   4: __pthread_cond_wait\n   5: start_thread"
                .to_string(),
        );
        assert_eq!(first_app_frame(&exception), None);
    }

    #[test]
    fn content_is_the_redacted_failure() {
        let mut config = LookoutConfig::default();
        config.redaction.fields.insert("card".into());
        let failure = Failure::from(HttpRequestFailed::new(
            Error::internal("declined").with_details(serde_json::json!({"card": "4242", "ok": 1})),
            "POST",
            "/pay",
            Some("/pay".to_string()),
        ));
        let entry = entry(&config, &failure);
        assert_eq!(entry.entry_type, EntryType::Exception);
        assert_eq!(entry.content["type"], "HttpRequest");
        assert_eq!(entry.content["exception"]["details"]["card"], crate::redact::REDACTED);
        assert_eq!(entry.content["exception"]["details"]["ok"], 1);
        assert_eq!(entry.summary, "Internal: declined");
    }
}
