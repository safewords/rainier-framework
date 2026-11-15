//! `GET /api/stream`: new batches as they are stored, as server-sent events.
//!
//! What the dashboard's "Live" switch subscribes to. Each event is one batch
//! summary — the same shape `/api/batches` lists — so a page can put it on top
//! of what it shows, or refresh its numbers.
//!
//! ```text
//! retry: 3000
//!
//! event: batch
//! data: {"id":"…","kind":"request","summary":"GET /posts/{post} 200",…}
//!
//! : keepalive
//! ```
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
use crate::store::BatchQuery;

/// How often the store is read for new batches while anyone is watching.
pub(crate) const TAIL_EVERY: Duration = Duration::from_secs(1);

/// How many batches a viewer may fall behind before it is sent `reset`.
const BUFFER: usize = 256;

/// How often a quiet stream sends a comment, to stay open through proxies.
const KEEPALIVE: Duration = Duration::from_secs(15);

/// How many recent batch ids the tail remembers, to send each only once.
const REMEMBER: usize = 1000;

/// The one tail a process runs, shared by every viewer.
pub(crate) struct Tail {
    sender: broadcast::Sender<Arc<str>>,
    running: AtomicBool,
}

impl Tail {
    pub(crate) fn new() -> Arc<Self> {
        let (sender, _) = broadcast::channel(BUFFER);
        Arc::new(Self { sender, running: AtomicBool::new(false) })
    }

    /// A receiver of new batches, starting the tail if nobody else had.
    fn subscribe(self: &Arc<Self>, state: &Arc<Shared>) -> broadcast::Receiver<Arc<str>> {
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
                    let _ = self.sender.send(Arc::from(json));
                }
            }
            first = false;
        }
    }
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
                    Ok(json) => format!("event: batch\ndata: {json}\n\n"),
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
