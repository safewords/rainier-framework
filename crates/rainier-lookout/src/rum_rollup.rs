//! Real users' page views and script errors, rolled up: what
//! `/api/rum/vitals` and `/api/rum/errors` answer from.
//!
//! Each accepted page report (see [`rum`](crate::rum)) is counted, on the
//! report's own request, into aggregate rows of two types — no table or key
//! of their own, so every store keeps, prunes and clears them like the rest:
//!
//! - [`RumView`](EntryType::RumView), keyed by the view's dimensions —
//!   `["page", "browser", "os", "device", "connection", "country"]`, as a
//!   JSON array. `count` is page views. The histogram holds six metrics side
//!   by side, each in its own range of slots (`metric × 1024 +`
//!   [`slot`]): LCP, INP, CLS, TTFB, FCP and load, in
//!   microseconds (CLS in millionths); and two counters, script errors and
//!   long tasks, in slots of their own. Histograms add when rows merge, so
//!   counters and metrics merge exactly as every other aggregate does.
//! - [`RumError`](EntryType::RumError), keyed by
//!   `["message", "file", line, page, browser, os, device, connection,
//!   country]`. `count` is occurrences; `max_us` holds the Unix second of the
//!   last one (the largest wins on a merge, as a maximum should).
//!
//! # Which report counts what
//!
//! A page view sends several reports; each thing is counted once:
//!
//! - **a view**, with TTFB, FCP and load, from the report carrying its load
//!   (`activity.load`, sent once per view);
//! - **LCP, INP and CLS** from the report saying they settled
//!   (`activity.settled`, sent once, when the page is first hidden or
//!   closed), filed under the page the view landed on;
//! - **script errors and long tasks** from every report, as they are sent
//!   once each.
//!
//! Views and vitals are of sampled page views (`LOOKOUT_RUM_SAMPLE_RATE`);
//! script errors are of every page view, since any page that throws
//! reports.
//!
//! # Untrusted input
//!
//! A report is anyone's. Every value is clamped (a time to ten minutes, CLS
//! to 100), and every dimension is either from a closed set — the browser
//! and system from this server's own reading of the user agent, the device
//! class, the connection's effective type, a two-letter country — or capped:
//! at most [`MAX_PAGES`] distinct pages, [`MAX_VIEW_KEYS`] distinct view keys,
//! [`MAX_ERROR_GROUPS`] distinct errors and [`MAX_ERROR_KEYS`] distinct error
//! keys an hour on each replica; past a cap, a value is filed under `other`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};

use crate::aggregate::{self, slot, Histogram, PERIODS};
use crate::entry::{Aggregate, EntryType};
use crate::facets::round;
use crate::rum::Report;

/// The most distinct pages counted an hour, per replica.
pub const MAX_PAGES: usize = 200;
/// The most distinct view keys (page × browser × … × country) an hour.
pub const MAX_VIEW_KEYS: usize = 1000;
/// The most distinct script errors (message, file, line) an hour.
pub const MAX_ERROR_GROUPS: usize = 200;
/// The most distinct error keys (error × dimensions) an hour.
pub const MAX_ERROR_KEYS: usize = 1000;

/// What a value past its cap, or outside its closed set, is filed under.
pub const OTHER: &str = "other";
/// What a value that was not given is filed under.
pub const UNKNOWN: &str = "unknown";

/// The dimensions, in key order.
pub const DIMENSIONS: [&str; 6] = ["page", "browser", "os", "device", "connection", "country"];

/// The metrics, in slot order.
const METRICS: [&str; 6] = ["lcp", "inp", "cls", "ttfb", "fcp", "load"];
const LCP: usize = 0;
const INP: usize = 1;
const CLS: usize = 2;
const TTFB: usize = 3;
const FCP: usize = 4;
const LOAD: usize = 5;
/// Slots per metric: more than [`slot`] ever returns (257).
const METRIC_SLOTS: u16 = 1024;
/// The counter slots, past the metrics.
const JS_ERRORS_SLOT: u16 = METRIC_SLOTS * METRICS.len() as u16;
const LONG_TASKS_SLOT: u16 = JS_ERRORS_SLOT + 1;

/// The longest a timing is believed, in milliseconds: ten minutes.
const MAX_MS: f64 = 600_000.0;
/// The largest layout shift believed.
const MAX_CLS: f64 = 100.0;
/// The most long tasks one report is believed to carry.
const MAX_LONG_TASKS: u64 = 10_000;
/// The most script errors one report carries — the script's own limit.
const MAX_ERRORS: usize = 20;

const BROWSERS: &[&str] =
    &["Chrome", "Safari", "Firefox", "Edge", "Opera", "Samsung Internet", "Chromium"];
const SYSTEMS: &[&str] = &["iOS", "Android", "Windows", "ChromeOS", "macOS", "Linux"];
const CONNECTIONS: &[&str] = &["slow-2g", "2g", "3g", "4g"];

/// The distinct values seen this hour, for the caps.
#[derive(Debug, Default)]
pub struct Caps {
    inner: Mutex<Seen>,
}

#[derive(Debug, Default)]
struct Seen {
    hour: i64,
    pages: HashSet<String>,
    views: HashSet<String>,
    groups: HashSet<String>,
    errors: HashSet<String>,
}

impl Seen {
    fn roll(&mut self, now: DateTime<Utc>) {
        let hour = now.timestamp().div_euclid(3600);
        if hour != self.hour {
            *self = Seen { hour, ..Seen::default() };
        }
    }
}

/// `value` if it is in `seen`, or there is room for it there; else `None`.
fn admit(seen: &mut HashSet<String>, cap: usize, value: &str) -> bool {
    if seen.contains(value) {
        return true;
    }
    if seen.len() >= cap {
        return false;
    }
    seen.insert(value.to_string());
    true
}

/// One view's dimensions, in [`DIMENSIONS`] order.
type Dims = [String; 6];

/// The dimensions a report is filed under, before the caps: everything but
/// the page.
struct Sender {
    browser: String,
    os: String,
    device: String,
    connection: String,
    country: String,
}

impl Sender {
    fn of(report: &Report, user_agent: Option<&str>, country: Option<&str>) -> Self {
        let agent = user_agent.map(crate::client::parse_user_agent).unwrap_or_default();
        let closed = |value: Option<&str>, set: &[&str]| match value {
            None => UNKNOWN.to_string(),
            Some(v) => set.iter().find(|s| **s == v).map_or(OTHER, |s| s).to_string(),
        };
        let connection = report
            .device
            .get("connection")
            .and_then(Value::as_object)
            .and_then(|c| c.get("type"))
            .and_then(Value::as_str);
        let country = country
            .map(str::trim)
            .filter(|c| c.len() == 2 && c.bytes().all(|b| b.is_ascii_alphanumeric()))
            .map(str::to_ascii_uppercase)
            .unwrap_or_else(|| UNKNOWN.to_string());
        Self {
            browser: closed(agent.browser.as_ref().map(|(n, _)| n.as_str()), BROWSERS),
            os: closed(agent.os.as_ref().map(|(n, _)| n.as_str()), SYSTEMS),
            device: agent.device.as_str().to_string(),
            connection: closed(connection, CONNECTIONS),
            country,
        }
    }

    fn dims(&self, page: String) -> Dims {
        [
            page,
            self.browser.clone(),
            self.os.clone(),
            self.device.clone(),
            self.connection.clone(),
            self.country.clone(),
        ]
    }
}

/// A page's path as a route: no scheme, host, query or fragment; a segment
/// that is a number, a long hex string or a UUID — or holds anything but a
/// path's plain characters — is `{id}`; at most eight segments and 120
/// characters.
pub fn page_route(url: &str) -> String {
    let rest =
        url.split_once("://").map_or(url, |(_, rest)| rest.split_once('/').map_or("", |(_, p)| p));
    let path = rest.split(['?', '#']).next().unwrap_or("");
    let segments: Vec<&str> = path
        .split('/')
        .filter(|s| !s.is_empty())
        .take(8)
        .map(|segment| {
            let plain = segment.len() <= 64
                && segment.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.~:@".contains(&b));
            let id = segment.bytes().all(|b| b.is_ascii_digit())
                || (segment.len() >= 16
                    && segment.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-'));
            if !plain || id {
                "{id}"
            } else {
                segment
            }
        })
        .collect();
    let route = format!("/{}", segments.join("/"));
    if route.len() > 120 {
        let mut cut = 120;
        while !route.is_char_boundary(cut) {
            cut -= 1;
        }
        route[..cut].to_string()
    } else {
        route
    }
}

/// Milliseconds as stored: microseconds, clamped. `None` when absent,
/// negative or not a number.
fn ms(map: &Map<String, Value>, key: &str) -> Option<u64> {
    let v = map.get(key)?.as_f64().filter(|v| v.is_finite() && *v >= 0.0)?;
    Some((v.min(MAX_MS) * 1000.0).round() as u64)
}

/// What one report adds, before it is spread over the periods.
#[derive(Default)]
struct Tally {
    views: BTreeMap<String, (u64, Histogram)>,
    errors: BTreeMap<String, u64>,
}

impl Tally {
    fn view(&mut self, key: &str) -> &mut (u64, Histogram) {
        self.views.entry(key.to_string()).or_default()
    }
}

fn metric(h: &mut Histogram, metric: usize, value: u64) {
    *h.entry(metric as u16 * METRIC_SLOTS + slot(value)).or_insert(0) += 1;
}

/// The rollup rows one accepted page report adds — empty when it carries no
/// page activity. `country` is the value of the configured country header.
/// Caps are counted in `caps`.
pub fn observe(
    caps: &Caps,
    report: &Report,
    user_agent: Option<&str>,
    country: Option<&str>,
    now: DateTime<Utc>,
) -> Vec<Aggregate> {
    let Some(activity) = &report.activity else { return Vec::new() };
    let sender = Sender::of(report, user_agent, country);
    let sampled = activity.get("sampled").and_then(Value::as_bool).unwrap_or(true);
    let current = page_route(report.page.get("url").and_then(Value::as_str).unwrap_or(""));
    let empty = Map::new();
    let vitals = activity.get("vitals").and_then(Value::as_object).unwrap_or(&empty);

    let mut guard = caps.inner.lock().unwrap_or_else(|e| e.into_inner());
    let seen: &mut Seen = &mut guard;
    seen.roll(now);
    let view_key = |seen: &mut Seen, page: &str| -> String {
        let page = if admit(&mut seen.pages, MAX_PAGES, page) { page } else { OTHER };
        let key = Value::from(sender.dims(page.to_string()).to_vec()).to_string();
        if admit(&mut seen.views, MAX_VIEW_KEYS, &key) {
            key
        } else {
            Value::from(vec![OTHER; 6]).to_string()
        }
    };

    let mut tally = Tally::default();
    let here = view_key(seen, &current);
    if sampled {
        if let Some(load) = activity.get("load").and_then(Value::as_object) {
            let (views, h) = tally.view(&here);
            *views += 1;
            if let Some(v) = ms(load, "ttfb") {
                metric(h, TTFB, v);
            }
            if let Some(v) = ms(load, "load").filter(|v| *v > 0) {
                metric(h, LOAD, v);
            }
            if let Some(v) = ms(vitals, "fcp") {
                metric(h, FCP, v);
            }
        }
        if activity.get("settled").and_then(Value::as_bool) == Some(true) {
            let landing = activity.get("landing").and_then(Value::as_str).map(page_route);
            let key = match landing {
                Some(page) => view_key(seen, &page),
                None => here.clone(),
            };
            let (_, h) = tally.view(&key);
            if let Some(v) = ms(vitals, "lcp") {
                metric(h, LCP, v);
            }
            if let Some(v) = ms(vitals, "inp") {
                metric(h, INP, v);
            }
            if let Some(cls) =
                vitals.get("cls").and_then(Value::as_f64).filter(|v| v.is_finite() && *v >= 0.0)
            {
                metric(h, CLS, (cls.min(MAX_CLS) * 1_000_000.0).round() as u64);
            }
        }
    }
    let long_tasks = activity
        .get("long_tasks")
        .and_then(Value::as_object)
        .and_then(|l| l.get("count"))
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(MAX_LONG_TASKS);
    if long_tasks > 0 {
        *tally.view(&here).1.entry(LONG_TASKS_SLOT).or_insert(0) += long_tasks;
    }

    let errors = activity.get("errors").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
    for error in errors.iter().take(MAX_ERRORS).filter_map(Value::as_object) {
        let page =
            error.get("page").and_then(Value::as_str).map_or_else(|| current.clone(), page_route);
        let key = view_key(seen, &page);
        *tally.view(&key).1.entry(JS_ERRORS_SLOT).or_insert(0) += 1;

        let message: String = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("error")
            .chars()
            .filter(|c| !c.is_control())
            .take(300)
            .collect();
        let file: String = error
            .get("source")
            .and_then(Value::as_str)
            .map(|f| f.split(['?', '#']).next().unwrap_or(f))
            .unwrap_or("")
            .chars()
            .filter(|c| !c.is_control())
            .take(300)
            .collect();
        let line =
            error.get("line").and_then(Value::as_u64).filter(|l| *l <= 10_000_000).unwrap_or(0);
        let group = json!([message, file, line]).to_string();
        let (message, file, line) = if admit(&mut seen.groups, MAX_ERROR_GROUPS, &group) {
            (message, file, line)
        } else {
            (OTHER.to_string(), String::new(), 0)
        };
        let dims: Vec<Value> = serde_json::from_str::<Vec<Value>>(&key).unwrap_or_default();
        let mut parts = vec![json!(message), json!(file), json!(line)];
        parts.extend(dims);
        let mut error_key = Value::Array(parts).to_string();
        if !admit(&mut seen.errors, MAX_ERROR_KEYS, &error_key) {
            let mut parts = vec![json!(message), json!(file), json!(line)];
            parts.extend(std::iter::repeat_n(json!(OTHER), 6));
            error_key = Value::Array(parts).to_string();
        }
        *tally.errors.entry(error_key).or_insert(0) += 1;
    }
    drop(guard);

    let last_seen = u64::try_from(now.timestamp()).unwrap_or(0);
    let mut rows = Vec::new();
    for &period in PERIODS {
        let bucket = aggregate::bucket_start(now, period);
        for (key, (views, histogram)) in &tally.views {
            rows.push(Aggregate {
                bucket_start: bucket,
                period_s: period,
                entry_type: EntryType::RumView,
                key: key.clone(),
                count: *views,
                sum_us: 0,
                max_us: 0,
                histogram: histogram.clone(),
            });
        }
        for (key, count) in &tally.errors {
            rows.push(Aggregate {
                bucket_start: bucket,
                period_s: period,
                entry_type: EntryType::RumError,
                key: key.clone(),
                count: *count,
                sum_us: 0,
                max_us: last_seen,
                histogram: Histogram::new(),
            });
        }
    }
    rows
}

/// What `/api/rum/vitals` and `/api/rum/errors` narrow to: a glob over the
/// page, the rest exact and in any case. `None` means any.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RumFilter {
    /// One per [`DIMENSIONS`], in order.
    pub dims: [Option<String>; 6],
}

impl RumFilter {
    fn matches(&self, dims: &[String]) -> bool {
        self.dims.iter().zip(dims).enumerate().all(|(i, (want, have))| match want {
            None => true,
            Some(want) if i == 0 => crate::config::glob(want, have),
            Some(want) => want.eq_ignore_ascii_case(have),
        })
    }
}

/// How `/api/rum/vitals` orders its rows, each descending.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VitalsOrder {
    /// Most views first.
    #[default]
    Views,
    /// Slowest p75 LCP first.
    Lcp,
    /// Slowest p75 INP first.
    Inp,
    /// Largest p75 CLS first.
    Cls,
    /// Most script errors first.
    Errors,
}

/// The `p`th percentile of `histogram`, placing each value at the middle of
/// its share of its slot rather than at the top.
///
/// [`aggregate::percentile`] reads a lone value as its slot's upper bound
/// and then clamps it to the exact maximum; a metric here has no exact
/// maximum to clamp to, so the top of the slot would read up to 19% high.
/// The middle is within 9% either way.
fn centred_percentile(histogram: &Histogram, p: f64) -> Option<u64> {
    let total: u64 = histogram.values().sum();
    if total == 0 {
        return None;
    }
    let rank = ((p.clamp(0.0, 100.0) / 100.0) * total as f64).ceil().max(1.0) as u64;
    let mut seen = 0;
    for (&s, &count) in histogram {
        if seen + count >= rank {
            if s == 0 {
                return Some(0);
            }
            let into = ((rank - seen) as f64 - 0.5) / count as f64;
            let lower = (f64::from(s - 1) / 4.0).exp2();
            return Some((lower * (into / 4.0).exp2()).round() as u64);
        }
        seen += count;
    }
    None
}

/// Rows of `entry_type` in the window.
fn in_window(
    rows: &[Aggregate],
    entry_type: EntryType,
    period_s: u32,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
) -> impl Iterator<Item = &Aggregate> {
    let first = aggregate::bucket_start(since, period_s);
    let last = aggregate::bucket_start(until, period_s);
    rows.iter().filter(move |r| {
        r.entry_type == entry_type
            && r.period_s == period_s
            && r.bucket_start >= first
            && r.bucket_start <= last
    })
}

#[derive(Default)]
struct Vitals {
    views: u64,
    histogram: Histogram,
}

impl Vitals {
    fn add(&mut self, row: &Aggregate) {
        self.views += row.count;
        aggregate::add_histogram(&mut self.histogram, &row.histogram);
    }

    fn p75(&self, metric: usize) -> Option<u64> {
        let from = metric as u16 * METRIC_SLOTS;
        let own: Histogram =
            self.histogram.range(from..from + METRIC_SLOTS).map(|(&s, &c)| (s - from, c)).collect();
        centred_percentile(&own, 75.0)
    }

    fn counter(&self, slot: u16) -> u64 {
        self.histogram.get(&slot).copied().unwrap_or(0)
    }

    fn json(&self, key: Option<&str>) -> Value {
        let ms = |m| self.p75(m).map(|us| round(us as f64 / 1000.0, 1));
        json!({
            "key": key,
            "views": self.views,
            "lcp_p75_ms": ms(LCP),
            "inp_p75_ms": ms(INP),
            "cls_p75": self.p75(CLS).map(|v| round(v as f64 / 1_000_000.0, 3)),
            "ttfb_p75_ms": ms(TTFB),
            "fcp_p75_ms": ms(FCP),
            "load_p75_ms": ms(LOAD),
            "js_errors": self.counter(JS_ERRORS_SLOT),
            "long_tasks": self.counter(LONG_TASKS_SLOT),
        })
    }
}

/// The `/api/rum/vitals` rows — grouped by dimension `group` (an index into
/// [`DIMENSIONS`]), ordered and cut — and the totals over the filtered set.
#[allow(clippy::too_many_arguments, reason = "the query's parts, each read once")]
pub fn vitals(
    rows: &[Aggregate],
    period_s: u32,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    filter: &RumFilter,
    group: usize,
    order: VitalsOrder,
    limit: usize,
) -> (Vec<Value>, Value) {
    let mut groups: BTreeMap<String, Vitals> = BTreeMap::new();
    let mut totals = Vitals::default();
    for row in in_window(rows, EntryType::RumView, period_s, since, until) {
        let Ok(dims) = serde_json::from_str::<Vec<String>>(&row.key) else { continue };
        if dims.len() != DIMENSIONS.len() || !filter.matches(&dims) {
            continue;
        }
        groups.entry(dims[group].clone()).or_default().add(row);
        totals.add(row);
    }
    let score = |v: &Vitals| -> f64 {
        match order {
            VitalsOrder::Views => v.views as f64,
            VitalsOrder::Lcp => v.p75(LCP).map_or(-1.0, |x| x as f64),
            VitalsOrder::Inp => v.p75(INP).map_or(-1.0, |x| x as f64),
            VitalsOrder::Cls => v.p75(CLS).map_or(-1.0, |x| x as f64),
            VitalsOrder::Errors => v.counter(JS_ERRORS_SLOT) as f64,
        }
    };
    let mut ordered: Vec<(String, Vitals)> = groups.into_iter().collect();
    ordered.sort_by(|(ka, a), (kb, b)| {
        score(b).total_cmp(&score(a)).then_with(|| b.views.cmp(&a.views)).then_with(|| ka.cmp(kb))
    });
    let rows = ordered.iter().take(limit).map(|(k, v)| v.json(Some(k))).collect();
    (rows, totals.json(None))
}

/// The `/api/rum/errors` rows: one per message, file and line in the filtered
/// set, most frequent first.
pub fn errors(
    rows: &[Aggregate],
    period_s: u32,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    filter: &RumFilter,
    limit: usize,
) -> Vec<Value> {
    #[derive(Default)]
    struct Group {
        count: u64,
        pages: HashSet<String>,
        browsers: HashMap<String, u64>,
        first: Option<DateTime<Utc>>,
        last: u64,
    }
    let mut groups: BTreeMap<(String, String, u64), Group> = BTreeMap::new();
    for row in in_window(rows, EntryType::RumError, period_s, since, until) {
        let Ok(parts) = serde_json::from_str::<Vec<Value>>(&row.key) else { continue };
        if parts.len() != 3 + DIMENSIONS.len() {
            continue;
        }
        let dims: Vec<String> =
            parts[3..].iter().map(|v| v.as_str().unwrap_or_default().to_string()).collect();
        if !filter.matches(&dims) {
            continue;
        }
        let id = (
            parts[0].as_str().unwrap_or_default().to_string(),
            parts[1].as_str().unwrap_or_default().to_string(),
            parts[2].as_u64().unwrap_or(0),
        );
        let g = groups.entry(id).or_default();
        g.count += row.count;
        g.pages.insert(dims[0].clone());
        *g.browsers.entry(dims[1].clone()).or_default() += row.count;
        g.first = Some(g.first.map_or(row.bucket_start, |f| f.min(row.bucket_start)));
        g.last = g.last.max(row.max_us);
    }
    let mut ordered: Vec<_> = groups.into_iter().collect();
    ordered.sort_by(|(ka, a), (kb, b)| b.count.cmp(&a.count).then_with(|| ka.cmp(kb)));
    ordered
        .into_iter()
        .take(limit)
        .map(|((message, file, line), g)| {
            let mut browsers: Vec<(String, u64)> = g.browsers.into_iter().collect();
            browsers.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            browsers.truncate(3);
            // A bucket can start before the window does; the first sighting
            // in the window is no earlier than its start.
            let first = g.first.map(|f| f.max(since));
            let last = i64::try_from(g.last)
                .ok()
                .and_then(|s| DateTime::<Utc>::from_timestamp(s, 0))
                .or(first);
            json!({
                "message": message,
                "file": file,
                "line": line,
                "count": g.count,
                "pages": g.pages.len(),
                "browsers": browsers.into_iter().map(|(n, c)| json!([n, c])).collect::<Vec<_>>(),
                "first_seen": first.map(|f| f.to_rfc3339()),
                "last_seen": last.map(|l| l.to_rfc3339()),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    const CHROME_MAC: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
    const IPHONE: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 18_1 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.1 Mobile/15E148 Safari/604.1";

    fn report(v: Value) -> Report {
        serde_json::from_value(v).unwrap()
    }

    fn now() -> DateTime<Utc> {
        Utc.timestamp_opt(1_800_000_030, 0).unwrap()
    }

    fn load_report(url: &str, lcp: f64) -> Report {
        report(json!({
            "page": { "url": url },
            "device": { "connection": { "type": "4g" } },
            "activity": {
                "sampled": true,
                "load": { "ttfb": 120.0, "load": 900.0 },
                "vitals": { "fcp": 300.0, "lcp": lcp, "cls": 0.05, "inp": 80.0 },
                "settled": true,
                "landing": url,
                "errors": [],
            }
        }))
    }

    #[test]
    fn page_routes_hide_ids_and_hosts() {
        assert_eq!(page_route("https://app.example.com/posts/123?x=1#y"), "/posts/{id}");
        assert_eq!(page_route("/u/0f8fad5b-d9cb-469f-a165-70867728950e/edit"), "/u/{id}/edit");
        assert_eq!(page_route("https://x.example"), "/");
        assert_eq!(page_route("/a b/<script>"), "/{id}/{id}");
        assert!(page_route(&"/abc".repeat(100)).len() <= 120);
    }

    #[test]
    fn a_view_is_counted_once_with_its_vitals() {
        let caps = Caps::default();
        let rows = observe(
            &caps,
            &load_report("https://app/posts/1", 2500.0),
            Some(CHROME_MAC),
            Some("de"),
            now(),
        );
        let minute: Vec<_> = rows.iter().filter(|r| r.period_s == 60).collect();
        assert_eq!(minute.len(), 1);
        assert_eq!(minute[0].entry_type, EntryType::RumView);
        assert_eq!(minute[0].key, r#"["/posts/{id}","Chrome","macOS","desktop","4g","DE"]"#);
        assert_eq!(minute[0].count, 1);

        let t = now();
        let (rows_json, totals) =
            vitals(&rows, 60, t, t, &RumFilter::default(), 0, VitalsOrder::Views, 10);
        assert_eq!(rows_json[0]["key"], "/posts/{id}");
        assert_eq!(totals["views"], 1);
        assert_eq!(totals["key"], Value::Null);
        for (field, want) in [
            ("lcp_p75_ms", 2500.0),
            ("ttfb_p75_ms", 120.0),
            ("fcp_p75_ms", 300.0),
            ("load_p75_ms", 900.0),
            ("inp_p75_ms", 80.0),
        ] {
            // A histogram reads within a slot — about 9% — of the truth.
            let got = totals[field].as_f64().unwrap();
            assert!((got / want - 1.0).abs() < 0.1, "{field}: {got}");
        }
        assert!((totals["cls_p75"].as_f64().unwrap() / 0.05 - 1.0).abs() < 0.1);
    }

    #[test]
    fn unsampled_reports_count_only_their_errors() {
        let caps = Caps::default();
        let r = report(json!({
            "page": { "url": "/a" },
            "activity": {
                "sampled": false,
                "load": { "ttfb": 1.0 },
                "long_tasks": { "count": 3 },
                "errors": [
                    { "message": "boom", "source": "https://cdn/app.js?v=2", "line": 10, "page": "/a/9" },
                    { "message": "boom", "source": "https://cdn/app.js?v=3", "line": 10, "page": "/a/7" },
                ],
            }
        }));
        let rows = observe(&caps, &r, Some(IPHONE), None, now());
        let t = now();
        let (_, totals) = vitals(&rows, 60, t, t, &RumFilter::default(), 0, VitalsOrder::Views, 10);
        assert_eq!(totals["views"], 0);
        assert_eq!(totals["js_errors"], 2);
        assert_eq!(totals["long_tasks"], 3);
        assert_eq!(totals["lcp_p75_ms"], Value::Null);

        let errs = errors(&rows, 60, t, t, &RumFilter::default(), 10);
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0]["message"], "boom");
        assert_eq!(errs[0]["file"], "https://cdn/app.js");
        assert_eq!(errs[0]["count"], 2);
        assert_eq!(errs[0]["pages"], 1, "both on /a/{{id}}");
        assert_eq!(errs[0]["browsers"], json!([["Safari", 2]]));
        assert_eq!(errs[0]["last_seen"], json!(now().to_rfc3339()));
    }

    #[test]
    fn filters_and_grouping_by_another_dimension() {
        let caps = Caps::default();
        let mut rows = observe(&caps, &load_report("/a", 1000.0), Some(CHROME_MAC), None, now());
        rows.extend(observe(&caps, &load_report("/a", 4000.0), Some(IPHONE), None, now()));
        rows.extend(observe(&caps, &load_report("/b", 2000.0), Some(IPHONE), None, now()));
        rows = aggregate::combine(rows);
        let t = now();
        let device = DIMENSIONS.iter().position(|d| *d == "device").unwrap();
        let (by_device, _) =
            vitals(&rows, 60, t, t, &RumFilter::default(), device, VitalsOrder::Views, 10);
        assert_eq!(by_device[0]["key"], "mobile");
        assert_eq!(by_device[0]["views"], 2);

        let mut only_a = RumFilter::default();
        only_a.dims[0] = Some("/a*".into());
        only_a.dims[1] = Some("safari".into());
        let (rows_a, totals) = vitals(&rows, 60, t, t, &only_a, 0, VitalsOrder::Lcp, 10);
        assert_eq!(totals["views"], 1);
        assert_eq!(rows_a.len(), 1);
        assert!((totals["lcp_p75_ms"].as_f64().unwrap() / 4000.0 - 1.0).abs() < 0.1);
    }

    #[test]
    fn hostile_reports_are_clamped_and_capped() {
        let caps = Caps::default();
        let mut rows = Vec::new();
        for i in 0..(MAX_PAGES + 50) {
            let r = report(json!({
                "page": { "url": format!("/p{i}x") },
                "device": { "connection": { "type": "warp-drive" } },
                "activity": {
                    "load": { "ttfb": 1e300, "load": -5.0 },
                    "long_tasks": { "count": u64::MAX },
                    "errors": [{ "message": format!("evil {i}\u{0007}"), "line": u64::MAX }],
                }
            }));
            rows.extend(observe(&caps, &r, Some("curl/8"), Some("<script>"), now()));
        }
        let rows = aggregate::combine(rows);
        let views: Vec<_> = rows
            .iter()
            .filter(|r| r.entry_type == EntryType::RumView && r.period_s == 60)
            .collect();
        assert_eq!(views.len(), MAX_PAGES + 1, "the rest are `other`");
        assert!(views.iter().any(|r| r.key.starts_with(r#"["other","#)));
        assert!(
            views.iter().all(|r| r.key.contains(r#""other","unknown","bot","other","unknown""#)),
            "{}",
            views[0].key
        );
        let t = now();
        let (_, totals) = vitals(&rows, 60, t, t, &RumFilter::default(), 0, VitalsOrder::Views, 10);
        assert!(totals["ttfb_p75_ms"].as_f64().unwrap() <= MAX_MS * 1.1);
        assert_eq!(totals["load_p75_ms"], Value::Null, "negative is dropped");
        assert_eq!(
            totals["long_tasks"].as_u64().unwrap(),
            MAX_LONG_TASKS * (MAX_PAGES as u64 + 50)
        );
        let errs = errors(&rows, 60, t, t, &RumFilter::default(), 500);
        assert_eq!(errs.len(), MAX_ERROR_GROUPS + 1);
        assert!(errs
            .iter()
            .all(|e| e["line"] == 0 && !e["message"].as_str().unwrap().contains('\u{7}')));
        assert_eq!(errs[0]["message"], "other");
    }

    #[test]
    fn the_caps_reset_each_hour() {
        let caps = Caps::default();
        for i in 0..MAX_PAGES {
            observe(&caps, &load_report(&format!("/p{i}x"), 1.0), None, None, now());
        }
        let full = observe(&caps, &load_report("/new", 1.0), None, None, now());
        assert!(full[0].key.starts_with(r#"["other""#));
        let later = now() + chrono::Duration::hours(1);
        let fresh = observe(&caps, &load_report("/new", 1.0), None, None, later);
        assert!(fresh[0].key.starts_with(r#"["/new""#));
    }
}
