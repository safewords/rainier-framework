//! A request nobody is waiting for any more — [`Abandonment`] and
//! [`AbandonPolicy`].
//!
//! A request is **abandoned** when the answer can no longer reach anyone:
//!
//! - the client hung up — closed the tab, lost signal, gave up
//!   ([`AbandonReason::ClientGone`]);
//! - a `Timeout` middleware gave up on it and answered `408`
//!   ([`AbandonReason::TimedOut`]).
//!
//! What happens to the handler's work then is the route's choice, made once
//! when the route is declared — [`OnAbandon::Cancel`] stops it at its next
//! `await`, [`OnAbandon::Finish`] lets it run to the end with its response
//! discarded. See `docs/abandoned-requests.md` for when to pick which.
//!
//! A handler that wants to stop early on its own terms reads the signal:
//!
//! ```
//! # use rainier_http::{Request, Response};
//! async fn export(request: Request) -> Response {
//!     let abandoned = request.abandonment();
//!     for _batch in 0..10 {
//!         if abandoned.is_abandoned() {
//!             return Response::text("stopped early");
//!         }
//!         // ... one batch of work ...
//!     }
//!     Response::text("done")
//! }
//! ```

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

/// Why a request was abandoned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AbandonReason {
    /// The client closed the connection before the response was ready.
    ClientGone,
    /// A `Timeout` middleware answered `408` before the handler finished.
    TimedOut,
}

impl AbandonReason {
    fn code(self) -> u8 {
        match self {
            Self::ClientGone => 1,
            Self::TimedOut => 2,
        }
    }

    fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::ClientGone),
            2 => Some(Self::TimedOut),
            _ => None,
        }
    }

    /// `client_gone` or `timed_out` — for a log field.
    pub fn name(self) -> &'static str {
        match self {
            Self::ClientGone => "client_gone",
            Self::TimedOut => "timed_out",
        }
    }
}

/// What to do with a handler's work once its request is abandoned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OnAbandon {
    /// Stop the work at its next `await`. Nothing after that point runs —
    /// including the second of two writes. What was already sent (a query, an
    /// HTTP call) still lands; only the rest is skipped.
    Cancel,
    /// Run the work to the end and discard its response. Costs whatever the
    /// rest of the work costs, for nobody.
    Finish,
}

/// What a route does with abandoned work, for each way of being abandoned.
///
/// Each half is optional: a route that leaves one unset inherits its group's,
/// and a group that leaves it unset inherits the framework's
/// [default](Self::DEFAULT) — finish when the client hangs up, cancel at a
/// timeout.
///
/// ```
/// use rainier_http::{AbandonPolicy, AbandonReason, OnAbandon};
///
/// let policy = AbandonPolicy::new().on_timeout(OnAbandon::Finish);
/// assert_eq!(policy.for_reason(AbandonReason::TimedOut), OnAbandon::Finish);
/// // Left unset, so the default: finish when the client hangs up.
/// assert_eq!(policy.for_reason(AbandonReason::ClientGone), OnAbandon::Finish);
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct AbandonPolicy {
    on_disconnect: Option<OnAbandon>,
    on_timeout: Option<OnAbandon>,
}

impl AbandonPolicy {
    /// What a route gets when nothing says otherwise.
    ///
    /// **Finish when the client hangs up**: a closed tab is common, the work is
    /// almost always short, and a handler stopped between two writes leaves
    /// half of them behind with no error anywhere.
    ///
    /// **Cancel at a timeout**: a request that slow is already in trouble, and
    /// cancelling it is what gives its connection and its database handle back
    /// while the thing it was waiting on recovers.
    pub const DEFAULT: Self =
        Self { on_disconnect: Some(OnAbandon::Finish), on_timeout: Some(OnAbandon::Cancel) };

    /// Nothing set: everything inherited.
    pub const fn new() -> Self {
        Self { on_disconnect: None, on_timeout: None }
    }

    /// Finish the work however the request was abandoned.
    pub const fn finish() -> Self {
        Self { on_disconnect: Some(OnAbandon::Finish), on_timeout: Some(OnAbandon::Finish) }
    }

    /// Cancel the work however the request was abandoned — what every route
    /// did before this policy existed.
    pub const fn cancel() -> Self {
        Self { on_disconnect: Some(OnAbandon::Cancel), on_timeout: Some(OnAbandon::Cancel) }
    }

    /// What to do when the client hangs up.
    pub const fn on_disconnect(mut self, action: OnAbandon) -> Self {
        self.on_disconnect = Some(action);
        self
    }

    /// What to do when a timeout gives up on the request.
    pub const fn on_timeout(mut self, action: OnAbandon) -> Self {
        self.on_timeout = Some(action);
        self
    }

    /// The disconnect half, if set.
    pub const fn disconnect(&self) -> Option<OnAbandon> {
        self.on_disconnect
    }

    /// The timeout half, if set.
    pub const fn timeout(&self) -> Option<OnAbandon> {
        self.on_timeout
    }

    /// This policy, with anything it leaves unset taken from `outer`.
    ///
    /// How a group's policy reaches its routes: the route's own choice wins.
    pub const fn or(self, outer: Self) -> Self {
        Self {
            on_disconnect: match self.on_disconnect {
                Some(action) => Some(action),
                None => outer.on_disconnect,
            },
            on_timeout: match self.on_timeout {
                Some(action) => Some(action),
                None => outer.on_timeout,
            },
        }
    }

    /// This policy with every gap filled from [`DEFAULT`](Self::DEFAULT).
    pub const fn resolved(self) -> Self {
        self.or(Self::DEFAULT)
    }

    /// What to do for `reason`, falling back to the default for an unset half.
    pub const fn for_reason(&self, reason: AbandonReason) -> OnAbandon {
        let resolved = self.resolved();
        let action = match reason {
            AbandonReason::ClientGone => resolved.on_disconnect,
            AbandonReason::TimedOut => resolved.on_timeout,
        };
        match action {
            Some(action) => action,
            None => OnAbandon::Cancel,
        }
    }
}

/// Whether, and why, a request has been abandoned.
///
/// One per request, shared: the server marks it when the client hangs up, a
/// `Timeout` marks it when it gives up, and anyone holding a clone — the
/// router, a middleware, the handler — can read it or wait for it. The first
/// reason recorded is the one that sticks.
///
/// Cheap to clone; every clone is the same signal.
#[derive(Clone, Default)]
pub struct Abandonment {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    reason: AtomicU8,
    wakers: Mutex<Vec<Waker>>,
}

impl Abandonment {
    /// A signal that has not fired.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that the request was abandoned, and wake whoever is waiting.
    ///
    /// Only the first call counts: a request that timed out and whose client
    /// then hung up on the `408` was abandoned by the timeout. Returns whether
    /// this call was the one that recorded it.
    pub fn abandon(&self, reason: AbandonReason) -> bool {
        let recorded = self
            .inner
            .reason
            .compare_exchange(0, reason.code(), Ordering::AcqRel, Ordering::Acquire)
            .is_ok();
        if recorded {
            let wakers =
                std::mem::take(&mut *self.inner.wakers.lock().unwrap_or_else(|p| p.into_inner()));
            for waker in wakers {
                waker.wake();
            }
        }
        recorded
    }

    /// Why the request was abandoned, if it has been.
    pub fn reason(&self) -> Option<AbandonReason> {
        AbandonReason::from_code(self.inner.reason.load(Ordering::Acquire))
    }

    /// Whether the request has been abandoned, for any reason.
    pub fn is_abandoned(&self) -> bool {
        self.reason().is_some()
    }

    /// Resolves when the request is abandoned, with the reason.
    pub fn abandoned(&self) -> Abandoned {
        Abandoned { abandonment: self.clone(), only: None }
    }

    /// Resolves when the request is abandoned **for `reason`**; never, if it is
    /// abandoned for another.
    pub fn abandoned_for(&self, reason: AbandonReason) -> Abandoned {
        Abandoned { abandonment: self.clone(), only: Some(reason) }
    }

    /// Whether two handles are the same signal.
    pub fn same_as(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

impl std::fmt::Debug for Abandonment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Abandonment").field("reason", &self.reason()).finish()
    }
}

/// The future [`Abandonment::abandoned`] returns.
#[must_use = "a future does nothing unless awaited"]
pub struct Abandoned {
    abandonment: Abandonment,
    only: Option<AbandonReason>,
}

impl Abandoned {
    fn outcome(&self) -> Option<Poll<AbandonReason>> {
        match (self.abandonment.reason(), self.only) {
            (Some(reason), None) => Some(Poll::Ready(reason)),
            (Some(reason), Some(only)) if reason == only => Some(Poll::Ready(reason)),
            // Abandoned for something else: this one never fires.
            (Some(_), Some(_)) => Some(Poll::Pending),
            (None, _) => None,
        }
    }
}

impl Future for Abandoned {
    type Output = AbandonReason;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<AbandonReason> {
        if let Some(outcome) = self.outcome() {
            return outcome;
        }
        {
            let mut wakers =
                self.abandonment.inner.wakers.lock().unwrap_or_else(|p| p.into_inner());
            if !wakers.iter().any(|w| w.will_wake(cx.waker())) {
                wakers.push(cx.waker().clone());
            }
        }
        // Checked again with the waker registered: an `abandon` between the
        // first check and the push would otherwise be missed.
        self.outcome().unwrap_or(Poll::Pending)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_reason_sticks() {
        let signal = Abandonment::new();
        assert_eq!(signal.reason(), None);
        assert!(signal.abandon(AbandonReason::TimedOut));
        assert!(!signal.abandon(AbandonReason::ClientGone));
        assert_eq!(signal.reason(), Some(AbandonReason::TimedOut));
    }

    #[test]
    fn clones_are_one_signal() {
        let signal = Abandonment::new();
        let clone = signal.clone();
        clone.abandon(AbandonReason::ClientGone);
        assert!(signal.is_abandoned());
        assert!(signal.same_as(&clone));
        assert!(!signal.same_as(&Abandonment::new()));
    }

    #[test]
    fn the_default_finishes_on_disconnect_and_cancels_at_a_timeout() {
        let policy = AbandonPolicy::new();
        assert_eq!(policy.for_reason(AbandonReason::ClientGone), OnAbandon::Finish);
        assert_eq!(policy.for_reason(AbandonReason::TimedOut), OnAbandon::Cancel);
    }

    #[test]
    fn a_route_choice_wins_over_its_group() {
        let group = AbandonPolicy::cancel();
        let route = AbandonPolicy::new().on_timeout(OnAbandon::Finish);
        let effective = route.or(group);
        assert_eq!(effective.for_reason(AbandonReason::TimedOut), OnAbandon::Finish);
        assert_eq!(effective.for_reason(AbandonReason::ClientGone), OnAbandon::Cancel);
    }

    #[test]
    fn abandoned_resolves_once_marked() {
        let signal = Abandonment::new();
        let mut waiting = Box::pin(signal.abandoned());
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        assert!(waiting.as_mut().poll(&mut cx).is_pending());
        signal.abandon(AbandonReason::ClientGone);
        assert_eq!(waiting.as_mut().poll(&mut cx), Poll::Ready(AbandonReason::ClientGone));
    }

    #[test]
    fn abandoned_for_ignores_other_reasons() {
        let signal = Abandonment::new();
        let mut waiting = Box::pin(signal.abandoned_for(AbandonReason::ClientGone));
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        signal.abandon(AbandonReason::TimedOut);
        assert!(waiting.as_mut().poll(&mut cx).is_pending());
    }
}
