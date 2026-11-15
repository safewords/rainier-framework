//! Real user monitoring: how a request looked from the browser that made it.
//!
//! The server sees a request from when its first byte arrives to when its
//! last leaves. The person sees more: the DNS lookup, the connection and TLS
//! handshake, the time the request queued in the browser, the network both
//! ways. Only the browser can measure that — the Resource Timing API — so a
//! small script on the page reads it and reports it here.
//!
//! # A split frontend
//!
//! The page and the API are usually different origins: `app.example.com`
//! calls `api.example.com`. That is fine, and is the case this is built for:
//!
//! 1. **Every API response names its batch.** With RUM on, each response
//!    carries `Server-Timing: lookout;desc="<token>"` — the batch's trace id,
//!    signed — and `Timing-Allow-Origin: <the frontends>`. A browser exposes a
//!    cross-origin response's detailed timing, and its `Server-Timing`, only
//!    to the origins `Timing-Allow-Origin` lists.
//! 2. **The page loads the script** from this application:
//!    `<script src="https://api.example.com/_lookout/rum.js" async>`. It
//!    watches the page's requests to this origin, reads each one's timing and
//!    token, and adds what the page knows about the device, the connection
//!    and its own load.
//! 3. **It reports with `sendBeacon`** to `{path}/rum`: a `text/plain` POST,
//!    which needs no CORS preflight, sent even as the page closes.
//! 4. **Each report becomes a `browser` batch** with the same trace id as the
//!    request it describes, so the request's page shows the two side by side.
//!
//! # What a report can and cannot do
//!
//! The endpoint is public — every visitor's browser posts to it — so nothing
//! in a report is trusted beyond being shown. It is accepted only from a
//! listed origin, at a bounded size and rate, and only for a request this
//! application answered: the token is an HMAC of the trace id, so a report
//! cannot invent a request, only describe one its sender made. A report never
//! changes the request's own batch.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use hmac::{Hmac, KeyInit, Mac};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::config::{LookoutConfig, Rum};
use crate::entry::{Batch, BatchKind, Entry, EntryType};

/// The most a report may weigh.
pub const MAX_REPORT_BYTES: usize = 64 * 1024;

/// The most requests one report may describe.
pub const MAX_CALLS: usize = 50;

/// The most requests one address may report a minute, per replica.
pub const PER_MINUTE: u32 = 240;

/// The `Server-Timing` metric that carries the token.
pub const METRIC: &str = "lookout";

/// Hex characters of the signature kept in a token: 64 bits.
const SIGNATURE_HEX: usize = 16;

type HmacSha256 = Hmac<Sha256>;

fn signature(secret: &[u8], trace_id: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC takes a key of any length");
    mac.update(b"lookout-rum\n");
    mac.update(trace_id.as_bytes());
    let bytes = mac.finalize().into_bytes();
    bytes.iter().take(SIGNATURE_HEX / 2).map(|b| format!("{b:02x}")).collect()
}

/// A signing key derived from an application key: what every replica of one
/// application shares, without the token revealing the key itself.
pub fn derive_secret(app_key: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(app_key).expect("HMAC takes a key of any length");
    mac.update(b"rainier-lookout-rum-key");
    mac.finalize().into_bytes().to_vec()
}

/// A signing key of this process's own, for when there is nothing shared to
/// derive one from.
pub fn random_secret() -> Vec<u8> {
    use rand::Rng as _;
    let mut bytes = [0u8; 32];
    rand::rand_core::UnwrapErr(rand::rngs::SysRng).fill_bytes(&mut bytes);
    bytes.to_vec()
}

/// The token a response carries: `<trace id>.<signature>`.
pub fn token(secret: &[u8], trace_id: &str) -> String {
    format!("{trace_id}.{}", signature(secret, trace_id))
}

/// The trace id a token names, if this application signed it.
pub fn verify(secret: &[u8], token: &str) -> Option<String> {
    let (trace_id, presented) = token.rsplit_once('.')?;
    if trace_id.is_empty()
        || trace_id.len() > 64
        || !trace_id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return None;
    }
    let expected = signature(secret, trace_id);
    bool::from(presented.as_bytes().ct_eq(expected.as_bytes())).then(|| trace_id.to_string())
}

/// The `Server-Timing` metric that names the route.
pub const ROUTE_METRIC: &str = "lookout-route";

/// What the server knows about a response as it leaves: its own time so far,
/// and the route that answered.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ServerSide {
    /// Milliseconds from the request's first byte to now.
    pub duration_ms: f64,
    /// The matched route's name, if it has one.
    pub route: Option<String>,
}

/// The headers a response carries so the browser can report on it.
///
/// With `server`, the token's metric carries the server's own time as `dur`
/// — `Server-Timing`'s standard duration — and a second metric names the
/// route, so every report can say how much of the wait was the server's and
/// which route it was, even for a request whose own batch was not kept.
pub fn response_headers(
    rum: &Rum,
    trace_id: &str,
    server: Option<&ServerSide>,
) -> Vec<(&'static str, String)> {
    if !rum.enabled || rum.secret.is_empty() {
        return Vec::new();
    }
    let token = token(&rum.secret, trace_id);
    let mut timing = match server {
        Some(server) => format!("{METRIC};dur={:.1};desc=\"{token}\"", server.duration_ms),
        None => format!("{METRIC};desc=\"{token}\""),
    };
    // A route name goes in a quoted string, so it must not be able to end it.
    if let Some(route) = server.and_then(|s| s.route.as_deref()).filter(|r| {
        !r.is_empty()
            && r.len() <= 128
            && r.bytes().all(|b| b.is_ascii_graphic() && b != b'"' && b != b'\\')
    }) {
        timing.push_str(&format!(", {ROUTE_METRIC};desc=\"{route}\""));
    }
    let mut headers = vec![("server-timing", timing)];
    if !rum.origins.is_empty() {
        headers.push(("timing-allow-origin", rum.origins.join(", ")));
    }
    headers
}

/// Counts reports per address, to bound what one sender can write.
#[derive(Debug, Default)]
pub struct Limiter {
    seen: Mutex<HashMap<IpAddr, (Instant, u32)>>,
}

impl Limiter {
    /// Whether `ip` may report `n` more requests this minute; counts them if so.
    pub(crate) fn allow(&self, ip: Option<IpAddr>, n: u32) -> bool {
        let Some(ip) = ip else { return true };
        let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        if seen.len() > 50_000 {
            // A flood of addresses: forget the old windows rather than grow.
            seen.retain(|_, (start, _)| now.duration_since(*start) < Duration::from_secs(60));
        }
        let slot = seen.entry(ip).or_insert((now, 0));
        if now.duration_since(slot.0) >= Duration::from_secs(60) {
            *slot = (now, 0);
        }
        if slot.1.saturating_add(n) > PER_MINUTE {
            return false;
        }
        slot.1 += n;
        true
    }
}

/// A report, as the script sends it. Every field is optional and bounded on
/// the way in: it comes from a browser, which is to say from anyone.
#[derive(Debug, Default, Deserialize)]
pub struct Report {
    #[serde(default)]
    pub(crate) page: Map<String, Value>,
    #[serde(default)]
    pub(crate) device: Map<String, Value>,
    #[serde(default)]
    pub(crate) calls: Vec<Map<String, Value>>,
}

/// Cut a value from a browser down to something safe to store and show:
/// short strings, finite numbers, shallow objects.
fn bounded(value: &Value, depth: usize) -> Option<Value> {
    match value {
        Value::Null | Value::Bool(_) => Some(value.clone()),
        Value::Number(n) => n.as_f64().filter(|f| f.is_finite()).map(|_| value.clone()),
        Value::String(s) => Some(Value::String(s.chars().take(512).collect())),
        Value::Array(items) if depth > 0 => Some(Value::Array(
            items.iter().take(20).filter_map(|v| bounded(v, depth - 1)).collect(),
        )),
        Value::Object(map) if depth > 0 => Some(Value::Object(bounded_map(map, depth - 1))),
        _ => None,
    }
}

fn bounded_map(map: &Map<String, Value>, depth: usize) -> Map<String, Value> {
    map.iter()
        .take(40)
        .filter_map(|(k, v)| Some((k.chars().take(64).collect::<String>(), bounded(v, depth)?)))
        .collect()
}

fn number(map: &Map<String, Value>, key: &str) -> Option<f64> {
    map.get(key).and_then(Value::as_f64).filter(|f| f.is_finite() && *f >= 0.0)
}

/// Turn a report into `browser` batches, one per request it describes whose
/// token verifies. `ip` and `user_agent` are the report's own, as this
/// application received it — the address the browser reports from.
pub(crate) fn batches(
    config: &LookoutConfig,
    report: Report,
    ip: Option<IpAddr>,
    user_agent: Option<&str>,
) -> Vec<Batch> {
    let rum = &config.rum;
    let page = bounded_map(&report.page, 2);
    let mut device = bounded_map(&report.device, 2);
    describe_sender(config, &mut device, ip, user_agent);
    let page_path = page
        .get("url")
        .and_then(Value::as_str)
        .map(|u| u.split(['?', '#']).next().unwrap_or(u).to_string());

    let now = chrono::Utc::now();
    report
        .calls
        .iter()
        .take(MAX_CALLS)
        .filter_map(|call| {
            // A report about a report — from a script cached before Lookout
            // stopped marking its own responses — is not a request.
            if call.get("initiator").and_then(Value::as_str) == Some("beacon") {
                return None;
            }
            let token = call.get("token").and_then(Value::as_str)?;
            let trace_id = verify(&rum.secret, token)?;
            let total_ms = number(call, "duration").unwrap_or(0.0);
            let url = call
                .get("url")
                .and_then(Value::as_str)
                .map(|u| {
                    u.split(['?', '#']).next().unwrap_or(u).chars().take(512).collect::<String>()
                })
                .unwrap_or_default();
            let path =
                url.splitn(4, '/').nth(3).map(|p| format!("/{p}")).unwrap_or_else(|| url.clone());
            let own = config.dashboard.path.trim_end_matches('/');
            if !own.is_empty() && (path == own || path.starts_with(&format!("{own}/"))) {
                return None;
            }
            let status = number(call, "status")
                .and_then(|s| u16::try_from(s as u64).ok())
                .filter(|s| *s > 0);
            let duration_us = (total_ms * 1000.0).round() as u64;

            let mut timing = bounded_map(call, 1);
            timing.remove("token");
            let mut content = Map::new();
            content.insert("url".into(), json!(url));
            content.insert("timing".into(), Value::Object(timing));
            content.insert("page".into(), Value::Object(page.clone()));
            content.insert("device".into(), Value::Object(device.clone()));

            let id = crate::recorder::new_id();
            let started_at = now - chrono::Duration::microseconds(duration_us as i64);
            let summary =
                format!("browser {path}{}", status.map(|s| format!(" {s}")).unwrap_or_default());
            let entry = Entry {
                id: crate::recorder::new_entry_id(),
                batch_id: id.clone(),
                entry_type: EntryType::Custom,
                span_id: None,
                parent_span_id: None,
                family_hash: Some(format!("browser:{path}")),
                summary: summary.clone(),
                occurred_at: started_at,
                duration_us: Some(duration_us),
                tags: vec!["browser".into()],
                content: Value::Object(content),
            };
            // The route the response named, for a link to its page. Only a
            // name's characters: it is shown and filtered on, never trusted.
            let route = call
                .get("route")
                .and_then(Value::as_str)
                .filter(|r| {
                    !r.is_empty()
                        && r.len() <= 128
                        && r.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-:/{}".contains(&b))
                })
                .map(str::to_string);
            Some(Batch {
                id,
                trace_id,
                kind: BatchKind::Browser,
                summary,
                route_name: route,
                route_pattern: None,
                method: None,
                status,
                guard: None,
                user_id: None,
                duration_us,
                has_exception: false,
                has_slow: total_ms >= rum.slow_ms as f64,
                host: page_path.clone(),
                started_at,
                tags: vec!["browser".into()],
                entries: vec![entry],
            })
        })
        .collect()
}

/// The script a page loads. Its settings are written into it and it finds its
/// endpoint from its own URL, so the page needs only the `<script>` tag; a
/// `data-sample` or `data-slow-ms` attribute on the tag overrides them for
/// that page.
pub(crate) fn script(rum: &Rum) -> String {
    let replay = &rum.replay;
    let settings = json!({
        "replay": {
            "enabled": replay.enabled,
            "consent": match replay.consent {
                crate::config::ReplayConsent::Required => "required",
                crate::config::ReplayConsent::Implied => "implied",
            },
            "except": replay.except,
            "maxBytes": replay.max_bytes,
        },
        "sample": rum.sample_rate,
        "slowMs": rum.slow_ms,
        "metric": METRIC,
    });
    SCRIPT.replace("__LOOKOUT_RUM_SETTINGS__", &settings.to_string())
}

/// The browser half, with its settings left to fill in.
const SCRIPT: &str = include_str!("../assets/rum.js");

/// The session-replay recorder (rrweb's), built from `frontend/` into
/// `assets/dist/rum-replay.js`. Served only while replay is on, and loaded by
/// the browser script only on a page that is going to record.
pub(crate) const RECORDER: &str = include_str!("../assets/dist/rum-replay.js");

/// A recording, as the script uploads it.
#[derive(Debug, Default, Deserialize)]
pub struct ReplayUpload {
    #[serde(default)]
    pub(crate) token: String,
    /// What made the page send it: `slow`, `failed` or `error`.
    #[serde(default)]
    pub(crate) trigger: String,
    #[serde(default)]
    pub(crate) page: Map<String, Value>,
    #[serde(default)]
    pub(crate) device: Map<String, Value>,
    /// The rrweb events, as JSON, gzipped, base64.
    #[serde(default)]
    pub(crate) replay: String,
    /// How much time the recording covers, in milliseconds.
    #[serde(default)]
    pub(crate) ms: f64,
}

/// Turn an upload into a `browser` batch tagged `replay`, on the trace of the
/// request whose token it carries — or `None` when it does not verify, is
/// not a gzip, is too large, or comes from a page that must not be recorded.
pub(crate) fn replay_batch(
    config: &LookoutConfig,
    upload: ReplayUpload,
    ip: Option<IpAddr>,
    user_agent: Option<&str>,
) -> Option<Batch> {
    use base64::Engine as _;
    let rum = &config.rum;
    if !rum.replay.enabled {
        return None;
    }
    let trace_id = verify(&rum.secret, &upload.token)?;
    if upload.replay.len() > rum.replay.max_bytes.saturating_mul(4) / 3 + 16 {
        return None;
    }
    // Checked, not unpacked: a gzip's first two bytes, so what is stored is
    // at least what the player expects. It is unpacked only in the viewer's
    // browser, never here.
    let head: String = upload.replay.chars().take(8).collect();
    let magic = base64::engine::general_purpose::STANDARD.decode(head).ok()?;
    if magic.get(..2) != Some(&[0x1f, 0x8b][..]) {
        return None;
    }
    let page = bounded_map(&upload.page, 2);
    let path = page
        .get("url")
        .and_then(Value::as_str)
        .and_then(|u| u.split(['?', '#']).next())
        .map(|u| u.splitn(4, '/').nth(3).map(|p| format!("/{p}")).unwrap_or_else(|| u.to_string()))
        .unwrap_or_else(|| "/".to_string());
    // The server holds the line too: a page excluded here is not stored even
    // from a script that recorded it anyway.
    if rum.replay.except.iter().any(|glob| crate::config::glob(glob, &path)) {
        return None;
    }
    let trigger = match upload.trigger.as_str() {
        t @ ("slow" | "failed" | "error") => t.to_string(),
        _ => "error".to_string(),
    };
    let mut device = bounded_map(&upload.device, 2);
    describe_sender(config, &mut device, ip, user_agent);
    let duration_us = (upload.ms.clamp(0.0, 10.0 * 60.0 * 1000.0) * 1000.0).round() as u64;

    let id = crate::recorder::new_id();
    let now = chrono::Utc::now();
    let summary = format!("replay {path} ({trigger})");
    let bytes = upload.replay.len() * 3 / 4;
    let content = json!({
        "kind": "replay",
        "trigger": trigger,
        "page": Value::Object(page),
        "device": Value::Object(device),
        "replay": upload.replay,
        "bytes": bytes,
    });
    Some(Batch {
        id: id.clone(),
        trace_id,
        kind: BatchKind::Browser,
        summary: summary.clone(),
        route_name: None,
        route_pattern: None,
        method: None,
        status: None,
        guard: None,
        user_id: None,
        duration_us,
        has_exception: trigger == "error",
        has_slow: trigger == "slow",
        host: None,
        started_at: now - chrono::Duration::microseconds(duration_us as i64),
        tags: vec!["browser".into(), "replay".into()],
        entries: vec![Entry {
            id: crate::recorder::new_entry_id(),
            batch_id: id,
            entry_type: EntryType::Custom,
            span_id: None,
            parent_span_id: None,
            family_hash: Some(format!("replay:{path}")),
            summary,
            occurred_at: now,
            duration_us: Some(duration_us),
            tags: vec!["replay".into()],
            content,
        }],
    })
}

/// Add what the server itself knows about a report's sender: its browser and
/// system from the user agent, and its address as far as it is kept.
fn describe_sender(
    config: &LookoutConfig,
    device: &mut Map<String, Value>,
    ip: Option<IpAddr>,
    user_agent: Option<&str>,
) {
    if let Some(ua) = user_agent {
        let agent = crate::client::parse_user_agent(ua);
        if let Some((name, version)) = agent.browser {
            device.insert("browser".into(), json!({ "name": name, "version": version }));
        }
        if let Some((name, version)) = agent.os {
            device.entry("os").or_insert(json!({ "name": name, "version": version }));
        }
        device.entry("kind").or_insert(json!(agent.device.as_str()));
    }
    if let Some(address) = ip.and_then(|ip| crate::client::mask_ip(ip, config.capture.client_ip)) {
        device.insert("ip".into(), json!(address));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_verifies_only_with_its_own_key_and_trace() {
        let t = token(b"key", "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(verify(b"key", &t).as_deref(), Some("4bf92f3577b34da6a3ce929d0e0e4736"));
        assert_eq!(verify(b"other", &t), None, "another application's token");
        let forged = t.replacen("4bf9", "aaaa", 1);
        assert_eq!(verify(b"key", &forged), None, "a trace id it did not sign");
        assert_eq!(verify(b"key", "nonsense"), None);
        assert_eq!(verify(b"key", "a b.c"), None);
    }

    #[test]
    fn responses_name_their_batch_only_when_rum_is_on() {
        let off = Rum::default();
        assert!(response_headers(&off, "t", None).is_empty());
        let on = Rum {
            enabled: true,
            origins: vec!["https://app.example.com".into()],
            secret: b"key".to_vec(),
            ..Rum::default()
        };
        let headers = response_headers(&on, "abc", None);
        assert_eq!(
            headers[0],
            ("server-timing", format!("lookout;desc=\"{}\"", token(b"key", "abc")))
        );
        assert_eq!(headers[1], ("timing-allow-origin", "https://app.example.com".to_string()));

        // With the server's side: its time, and the route that answered.
        let server = ServerSide { duration_ms: 12.34, route: Some("posts.show".into()) };
        let headers = response_headers(&on, "abc", Some(&server));
        assert_eq!(
            headers[0].1,
            format!(
                "lookout;dur=12.3;desc=\"{}\", lookout-route;desc=\"posts.show\"",
                token(b"key", "abc")
            )
        );
        // A route name that could break out of its quotes is left out.
        let odd = ServerSide { duration_ms: 1.0, route: Some("a\"b".into()) };
        assert!(!response_headers(&on, "abc", Some(&odd))[0].1.contains("lookout-route"));
    }

    #[test]
    fn a_sender_is_limited_per_minute() {
        let limiter = Limiter::default();
        let ip: IpAddr = "203.0.113.9".parse().unwrap();
        assert!(limiter.allow(Some(ip), PER_MINUTE));
        assert!(!limiter.allow(Some(ip), 1), "over the minute's budget");
        assert!(limiter.allow(Some("203.0.113.10".parse().unwrap()), 1), "another sender is not");
    }

    #[test]
    fn a_report_becomes_browser_batches_for_the_requests_it_can_prove() {
        let mut config = LookoutConfig::default();
        config.rum = Rum { enabled: true, secret: b"key".to_vec(), ..config.rum };
        let good = token(b"key", "trace1");
        let report: Report = serde_json::from_value(json!({
            "page": { "url": "https://app.example.com/feed?token=x", "lcp": 1200.5 },
            "device": { "memory": 8, "connection": { "type": "4g", "rtt": 50 } },
            "calls": [
                { "token": good, "url": "https://api.example.com/api/posts?page=2", "duration": 812.4,
                  "dns": 0, "connect": 31.2, "waiting": 240.0, "download": 3.1, "protocol": "h2", "status": 200,
                  "server": 61.5, "route": "posts.index" },
                { "token": "trace2.0000000000000000", "url": "https://api.example.com/x", "duration": 5 },
                { "token": token(b"key", "trace3"), "url": "https://api.example.com/_lookout/rum", "duration": 80 },
                { "token": token(b"key", "trace4"), "url": "https://api.example.com/api/x", "initiator": "beacon", "duration": 80 },
            ],
        }))
        .unwrap();
        let ua = "Mozilla/5.0 (iPhone; CPU iPhone OS 18_1 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.1 Mobile/15E148 Safari/604.1";
        let out = batches(&config, report, Some("203.0.113.77".parse().unwrap()), Some(ua));

        assert_eq!(out.len(), 1, "the forged token, and reports about Lookout itself, are dropped");
        let batch = &out[0];
        assert_eq!(batch.kind, BatchKind::Browser);
        assert_eq!(batch.trace_id, "trace1", "on the request's own trace");
        assert_eq!(batch.summary, "browser /api/posts 200");
        assert_eq!(batch.duration_us, 812_400);
        assert_eq!(
            batch.route_name.as_deref(),
            Some("posts.index"),
            "the route the response named"
        );
        assert_eq!(batch.entries[0].content["timing"]["server"], 61.5);
        let content = &batch.entries[0].content;
        assert_eq!(content["url"], "https://api.example.com/api/posts", "no query");
        assert_eq!(content["timing"]["waiting"], 240.0);
        assert!(content["timing"].get("token").is_none());
        assert_eq!(content["device"]["ip"], "203.0.113.0/24");
        assert_eq!(content["device"]["browser"]["name"], "Safari");
        assert_eq!(content["device"]["kind"], "mobile");
        assert_eq!(batch.host.as_deref(), Some("https://app.example.com/feed"));
    }

    fn replay_config() -> LookoutConfig {
        let mut config = LookoutConfig::default();
        config.rum = Rum { enabled: true, secret: b"key".to_vec(), ..config.rum };
        config.rum.replay.enabled = true;
        config.rum.replay.except = vec!["/chats*".into()];
        config
    }

    fn upload(token: &str, url: &str, replay: &str) -> ReplayUpload {
        serde_json::from_value(json!({
            "token": token, "trigger": "failed", "replay": replay, "ms": 14000.0,
            "page": { "url": url }, "device": { "screen": "1920x1080" },
        }))
        .unwrap()
    }

    // `H4sI` is base64 for a gzip's first bytes, 1f 8b 08.
    const GZIP: &str = "H4sIAAAAAAAAA4uOBQApu0wNAgAAAA==";

    #[test]
    fn a_replay_joins_its_request_trace_and_keeps_only_what_it_should() {
        let config = replay_config();
        let good = token(b"key", "trace9");
        let batch = replay_batch(
            &config,
            upload(&good, "https://app.example.com/feed?x=1", GZIP),
            None,
            None,
        )
        .expect("a replay for a signed request");
        assert_eq!(batch.trace_id, "trace9");
        assert_eq!(batch.kind, BatchKind::Browser);
        assert!(batch.tags.contains(&"replay".to_string()));
        assert_eq!(batch.summary, "replay /feed (failed)");
        assert_eq!(batch.duration_us, 14_000_000);
        assert_eq!(batch.entries[0].content["kind"], "replay");
        assert_eq!(batch.entries[0].content["replay"], GZIP);
    }

    #[test]
    fn a_replay_is_refused_when_it_should_not_be_kept() {
        let config = replay_config();
        let good = token(b"key", "trace9");
        let forged = "trace9.0000000000000000";
        assert!(
            replay_batch(&config, upload(forged, "https://a.example/feed", GZIP), None, None)
                .is_none(),
            "unsigned"
        );
        assert!(
            replay_batch(&config, upload(&good, "https://a.example/chats/4", GZIP), None, None)
                .is_none(),
            "an excluded page, whatever the script did"
        );
        assert!(
            replay_batch(
                &config,
                upload(&good, "https://a.example/feed", "eyJub3QiOiJnemlwIn0="),
                None,
                None
            )
            .is_none(),
            "not a gzip"
        );
        let mut small = replay_config();
        small.rum.replay.max_bytes = 4;
        assert!(
            replay_batch(&small, upload(&good, "https://a.example/feed", GZIP), None, None)
                .is_none(),
            "too large"
        );
        let mut off = replay_config();
        off.rum.replay.enabled = false;
        assert!(
            replay_batch(&off, upload(&good, "https://a.example/feed", GZIP), None, None).is_none(),
            "replay off"
        );
    }

    #[test]
    fn the_script_carries_the_replay_settings() {
        let mut rum = replay_config().rum;
        rum.replay.consent = crate::config::ReplayConsent::Implied;
        let script = script(&rum);
        assert!(script.contains("\"consent\":\"implied\""), "the page is told");
        assert!(script.contains("\"except\":[\"/chats*\"]"));
        assert!(!script.contains("__LOOKOUT_RUM_SETTINGS__"));
    }
}
