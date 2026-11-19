//! [`TouchLedger`] — when this process last extended each session's expiry.
//!
//! A session that was read and not changed has its expiry moved forward so an
//! active visitor is not logged out mid-visit. Doing that on **every** request
//! costs a store round trip per request to move an expiry measured in hours or
//! days by a few seconds. The ledger lets [`StartSession`](crate::StartSession)
//! skip the touch when it already made one for that session recently: the
//! expiry ends up at most one interval earlier than a touch on every request
//! would have put it, which against a lifetime a hundred times longer is
//! nothing a visitor can notice.
//!
//! Per process and bounded. Several replicas each touch a session at most once
//! an interval, which is still a handful of writes per session per interval
//! rather than one per request. Entries hold a keyed hash of the session id,
//! never the id itself, which is a bearer credential.

use std::collections::HashMap;
use std::hash::{BuildHasher, RandomState};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The most sessions a process remembers touching. Past it, the oldest are
/// forgotten — which costs nothing worse than an extra touch.
const CAPACITY: usize = 100_000;

/// The longest the ledger waits between touches of one session, however long
/// the session lives.
const MAX_INTERVAL: Duration = Duration::from_secs(60);

pub(crate) struct TouchLedger {
    interval: Duration,
    hasher: RandomState,
    touched: Mutex<HashMap<u64, Instant>>,
}

impl TouchLedger {
    /// A ledger for sessions that live `lifetime`: at most one touch per
    /// session per hundredth of it, and at most a minute between touches.
    pub(crate) fn for_lifetime(lifetime: Duration) -> Self {
        Self::with_interval((lifetime / 100).min(MAX_INTERVAL))
    }

    pub(crate) fn with_interval(interval: Duration) -> Self {
        Self { interval, hasher: RandomState::new(), touched: Mutex::new(HashMap::new()) }
    }

    /// Whether `id` is due a touch, recording one now if it is.
    ///
    /// Recorded before the touch runs: a touch that fails leaves the session's
    /// old expiry in place, which still has nearly the whole lifetime left, and
    /// retrying it on every request while the store is down would add load to a
    /// store that is already struggling.
    pub(crate) fn due(&self, id: &str) -> bool {
        if self.interval.is_zero() {
            return true;
        }
        let key = self.hasher.hash_one(id);
        let now = Instant::now();
        let mut touched = self.touched.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if touched.get(&key).is_some_and(|at| now.duration_since(*at) < self.interval) {
            return false;
        }
        if touched.len() >= CAPACITY {
            let interval = self.interval;
            touched.retain(|_, at| now.duration_since(*at) < interval);
            if touched.len() >= CAPACITY {
                touched.clear();
            }
        }
        touched.insert(key, now);
        true
    }

    /// Record that `id`'s expiry was just set — by a write, which sets a fresh
    /// one of its own — so the next interval's requests need not touch it.
    pub(crate) fn record(&self, id: &str) {
        if self.interval.is_zero() {
            return;
        }
        let key = self.hasher.hash_one(id);
        let mut touched = self.touched.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if touched.len() >= CAPACITY {
            touched.clear();
        }
        touched.insert(key, Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_is_touched_once_per_interval() {
        let ledger = TouchLedger::with_interval(Duration::from_secs(60));
        assert!(ledger.due("a"));
        assert!(!ledger.due("a"), "touched a moment ago");
        assert!(ledger.due("b"), "another session is its own");
    }

    #[test]
    fn a_zero_interval_touches_every_time() {
        let ledger = TouchLedger::with_interval(Duration::ZERO);
        assert!(ledger.due("a"));
        assert!(ledger.due("a"));
    }

    #[test]
    fn the_interval_is_a_hundredth_of_the_lifetime_and_at_most_a_minute() {
        assert_eq!(
            TouchLedger::for_lifetime(Duration::from_secs(1000)).interval,
            Duration::from_secs(10)
        );
        assert_eq!(
            TouchLedger::for_lifetime(Duration::from_secs(90 * 86_400)).interval,
            MAX_INTERVAL
        );
    }

    #[test]
    fn the_interval_elapsing_makes_a_session_due_again() {
        let ledger = TouchLedger::with_interval(Duration::from_millis(20));
        assert!(ledger.due("a"));
        std::thread::sleep(Duration::from_millis(30));
        assert!(ledger.due("a"));
    }

    #[test]
    fn a_session_just_written_is_not_due() {
        let ledger = TouchLedger::with_interval(Duration::from_secs(60));
        ledger.record("a");
        assert!(!ledger.due("a"));
    }
}
