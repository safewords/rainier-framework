//! `GET /api/stream`: new batches as they are stored, and the counts as they
//! move, as server-sent events.
//!
//! What the dashboard's "Live" switch subscribes to. Two kinds of event:
//!
//! - `batch`: one batch summary — the same shape `/api/batches` lists — for
//!   each batch the store keeps. Lists put it on top of what they show.
//! - `pulse`: the newest buckets of the request, exception and job counts, at
//!   every period the store rolls up, whenever they have changed — at most
//!   every [`PULSE_EVERY`]. Most requests are counted and not kept, so on a
//!   healthy service this, not `batch`, is what says something happened: the
//!   dashboard's totals and charts move with it.
//!
//! ```text
//! retry: 3000
//!
//! event: batch
//! data: {"id":"…","kind":"request","summary":"GET /posts/{post} 200",…}
//!
//! event: pulse
//! data: {"request":{"60":[{"bucket_start":"…","count":41,…},…],"600":[…],"3600":[…]},"exception":{…},"job":{…}}
//!
//! : keepalive
//! ```
//!
//! Each period's array holds the bucket now open and the one before it — the
//! one before still grows for a few seconds after it closes, while replicas
//! write the counts they were holding. A page replaces the points with the
//! same `bucket_start` and appends a new one.
//!
//! # Where the batches come from
//!
//! The store, not this process's own writer: on several replicas a batch is
//! written by whichever one served the request, and every viewer should see
//! every replica's. So one task per process **tails the store** — it reads the
//! newest batches every second and sends on the ones it has not sent — and
//! every connected viewer shares it. It runs only while somebody is
//! connected, and stops within a second of the last one leaving.
//!
//! A viewer that falls more than [`BUFFER`] batches behind is sent `reset`,
//! which the dashboard answers by reloading what it shows.
//!
//! A comment line goes out every [`KEEPALIVE`], so a proxy that closes quiet
//! connections does not close this one.

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use rainier_http::Response;
use rainier_routing::Req;
use rainier_support::{Error, Result};
use tokio::sync::broadcast;

use super::Shared;
use crate::aggregate::{bucket_start, PERIODS};
use crate::entry::EntryType;
use crate::store::BatchQuery;
use crate::trends;

/// How often the store is read for new batches while anyone is watching.
pub(crate) const TAIL_EVERY: Duration = Duration::from_secs(1);

/// How many batches a viewer may fall behind before it is sent `reset`.
const BUFFER: usize = 256;

/// How often a quiet stream sends a comment, to stay open through proxies.
const KEEPALIVE: Duration = Duration::from_secs(15);

/// How many recent batch ids the tail remembers, to send each only once.
const REMEMBER: usize = 1000;

/// How often the tail reads the counts for a `pulse`. The writer stores the
/// counts it holds every two seconds (`COUNTS_EVERY` in the recorder), so
/// reading faster would only read the same numbers.
pub(crate) const PULSE_EVERY: Duration = Duration::from_secs(2);

/// The counts a `pulse` carries.
const PULSED: [EntryType; 3] = [EntryType::Request, EntryType::Exception, EntryType::Job];

/// One server-sent event, as the tail hands it to every viewer.
#[derive(Clone)]
enum Frame {
    Batch(Arc<str>),
    Pulse(Arc<str>),
}

/// The one tail a process runs, shared by every viewer.
pub(crate) struct Tail {
    sender: broadcast::Sender<Frame>,
    running: AtomicBool,
}

impl Tail {
    pub(crate) fn new() -> Arc<Self> {
        let (sender, _) = broadcast::channel(BUFFER);
        Arc::new(Self { sender, running: AtomicBool::new(false) })
    }

    /// A receiver of new batches, starting the tail if nobody else had.
    fn subscribe(self: &Arc<Self>, state: &Arc<Shared>) -> broadcast::Receiver<Frame> {
        let receiver = self.sender.subscribe();
        if !self.running.swap(true, Ordering::SeqCst) {
            tokio::spawn(Arc::clone(self).run(Arc::clone(state)));
        }
        receiver
    }

    async fn run(self: Arc<Self>, state: Arc<Shared>) {
        let mut seen: HashSet<String> = HashSet::new();
        let mut order: VecDeque<String> = VecDeque::new();
        let mut first = true;
        let mut last_pulse: Option<String> = None;
        let mut pulsed_at: Option<tokio::time::Instant> = None;
        let mut tick = tokio::time::interval(TAIL_EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            if self.sender.receiver_count() == 0 {
                self.running.store(false, Ordering::SeqCst);
                // Someone may have subscribed between the count and the store;
                // if so, and nobody else has restarted it, carry on.
                if self.sender.receiver_count() == 0 || self.running.swap(true, Ordering::SeqCst) {
                    return;
                }
            }
            if pulsed_at.is_none_or(|at| at.elapsed() >= PULSE_EVERY) {
                pulsed_at = Some(tokio::time::Instant::now());
                // Sent only when it differs from the last one, so a quiet
                // service sends nothing; a viewer who just connected loads
                // the numbers itself, as with batches.
                if let Some(pulse) = pulse(&state).await {
                    if last_pulse.as_deref() != Some(pulse.as_str()) {
                        if last_pulse.is_some() {
                            let _ = self.sender.send(Frame::Pulse(Arc::from(pulse.as_str())));
                        }
                        last_pulse = Some(pulse);
                    }
                }
            }
            let query = BatchQuery { limit: Some(50), ..BatchQuery::default() };
            let Ok(newest) = state.store.batches(query).await else { continue };
            // Oldest first, so a viewer receives them in the order they ran.
            for summary in newest.into_iter().rev() {
                if !seen.insert(summary.id.clone()) {
                    continue;
                }
                order.push_back(summary.id.clone());
                if order.len() > REMEMBER {
                    if let Some(old) = order.pop_front() {
                        seen.remove(&old);
                    }
                }
                // The first read only learns what is already there: a viewer
                // who just connected has loaded those itself.
                if first {
                    continue;
                }
                if let Ok(json) = serde_json::to_string(&summary) {
                    let _ = self.sender.send(Frame::Batch(Arc::from(json)));
                }
            }
            first = false;
        }
    }
}

/// The newest two buckets of each pulsed type at each period, as JSON — or
/// `None` when the store could not be read, which skips this pulse.
async fn pulse(state: &Shared) -> Option<String> {
    let now = chrono::Utc::now();
    let reads = PULSED.iter().flat_map(|&entry_type| {
        PERIODS.iter().map(move |&period| async move {
            let since = bucket_start(now, period) - chrono::Duration::seconds(i64::from(period));
            let rows = state.store.aggregate_rows(entry_type, period, since).await?;
            Ok::<_, Error>((entry_type, period, trends::series(&rows, period, since, now)))
        })
    });
    // A handful of small reads, once per process however many are watching.
    let read = futures_util::future::try_join_all(reads).await.ok()?;
    let mut out = serde_json::Map::new();
    for (entry_type, period, points) in read {
        let by_period = out
            .entry(entry_type.as_str().to_string())
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
        if let Some(by_period) = by_period.as_object_mut() {
            by_period.insert(period.to_string(), serde_json::to_value(points).ok()?);
        }
    }
    serde_json::to_string(&out).ok()
}

/// `GET /api/stream`.
pub(crate) async fn stream(state: Arc<Shared>, _request: Req) -> Result<Response> {
    if !state.config.dashboard.stream {
        return Err(Error::not_found("Live streaming is off (LOOKOUT_STREAM=false)."));
    }
    let receiver = state.tail.subscribe(&state);
    let opening = futures_util::stream::once(async { Ok(Bytes::from_static(b"retry: 3000\n\n")) });
    let events = futures_util::stream::unfold(
        (receiver, tokio::time::interval_at(tokio::time::Instant::now() + KEEPALIVE, KEEPALIVE)),
        |(mut receiver, mut keepalive)| async move {
            let frame = tokio::select! {
                received = receiver.recv() => match received {
                    Ok(Frame::Batch(json)) => format!("event: batch\ndata: {json}\n\n"),
                    Ok(Frame::Pulse(json)) => format!("event: pulse\ndata: {json}\n\n"),
                    // Too far behind to catch up one by one.
                    Err(broadcast::error::RecvError::Lagged(_)) => "event: reset\ndata: {}\n\n".to_string(),
                    Err(broadcast::error::RecvError::Closed) => return None,
                },
                _ = keepalive.tick() => ": keepalive\n\n".to_string(),
            };
            Some((Ok(Bytes::from(frame)), (receiver, keepalive)))
        },
    );
    Ok(Response::stream(futures_util::StreamExt::chain(opening, events))
        .with_content_type("text/event-stream")
        .with_header("cache-control", "no-store")
        // nginx and its kin buffer a response unless told not to.
        .with_header("x-accel-buffering", "no"))
}
