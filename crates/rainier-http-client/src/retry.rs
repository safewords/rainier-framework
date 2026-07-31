//! When to try again — [`Backoff`].
//!
//! Retrying is where an outbound client is most often wrong in both
//! directions: not at all, so one dropped connection fails a job that would
//! have worked; or everything, so a `422` is sent four times and a duplicate
//! charge is created three of them.

use std::time::Duration;

/// How long to wait between attempts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backoff {
    /// The same wait every time.
    Fixed(Duration),
    /// Doubling: 100ms, 200ms, 400ms, …
    ///
    /// The default, and the right one for a dependency that is briefly
    /// unavailable — retrying at full speed is what turns a blip into an
    /// outage, because every caller does it at once.
    Exponential {
        /// The wait before the second attempt.
        base: Duration,
        /// The longest it will ever wait.
        max: Duration,
    },
    /// No wait at all.
    ///
    /// For a test, and for the rare case where the failure is known to be
    /// instantaneous and independent.
    None,
}

impl Backoff {
    /// Doubling from 100ms, capped at ten seconds.
    pub fn exponential() -> Self {
        Self::Exponential { base: Duration::from_millis(100), max: Duration::from_secs(10) }
    }

    /// The same wait every time.
    pub fn fixed(wait: Duration) -> Self {
        Self::Fixed(wait)
    }

    /// How long to wait before attempt `attempt`, counting the first as 1.
    ///
    /// Zero before the first, because nothing has failed yet.
    pub fn wait_before(&self, attempt: u32) -> Duration {
        if attempt <= 1 {
            return Duration::ZERO;
        }

        match self {
            Self::None => Duration::ZERO,
            Self::Fixed(wait) => *wait,
            Self::Exponential { base, max } => {
                // `attempt - 2`, so the wait before the *second* attempt is
                // the base rather than twice it.
                let doublings = (attempt - 2).min(20);
                let wait = base.saturating_mul(2u32.saturating_pow(doublings));
                wait.min(*max)
            }
        }
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::exponential()
    }
}

/// Whether a failed attempt is worth repeating.
///
/// The default policy, and the one worth understanding before overriding it:
///
/// | | Retried | Why |
/// |---|---|---|
/// | a transport failure | yes | a refused connection or a timeout is usually transient |
/// | `408`, `425`, `429` | yes | the other end said "later" in as many words |
/// | `5xx` | yes | the other end is having a bad time |
/// | `4xx` otherwise | **no** | the request is wrong, and sending it again keeps it wrong |
///
/// The last row is the one that matters. Retrying a `422` four times does not
/// fix the payload; retrying a `402` charges the card again if the other end
/// is not idempotent.
pub fn is_retryable(status: Option<u16>) -> bool {
    match status {
        // A transport failure — nothing came back at all.
        None => true,
        Some(408 | 425 | 429) => true,
        Some(status) => status >= 500,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_waits_before_the_first_attempt() {
        for backoff in
            [Backoff::exponential(), Backoff::fixed(Duration::from_secs(1)), Backoff::None]
        {
            assert_eq!(backoff.wait_before(1), Duration::ZERO, "{backoff:?}");
            assert_eq!(backoff.wait_before(0), Duration::ZERO, "{backoff:?}");
        }
    }

    #[test]
    fn exponential_starts_at_the_base_and_doubles() {
        let backoff =
            Backoff::Exponential { base: Duration::from_millis(100), max: Duration::from_secs(60) };

        assert_eq!(backoff.wait_before(2), Duration::from_millis(100));
        assert_eq!(backoff.wait_before(3), Duration::from_millis(200));
        assert_eq!(backoff.wait_before(4), Duration::from_millis(400));
    }

    #[test]
    fn exponential_is_capped() {
        // Without a cap, the twentieth attempt waits a day and a half.
        let backoff =
            Backoff::Exponential { base: Duration::from_millis(100), max: Duration::from_secs(2) };

        assert_eq!(backoff.wait_before(20), Duration::from_secs(2));
        assert_eq!(backoff.wait_before(1000), Duration::from_secs(2));
    }

    #[test]
    fn fixed_is_the_same_every_time() {
        let backoff = Backoff::fixed(Duration::from_millis(250));

        assert_eq!(backoff.wait_before(2), Duration::from_millis(250));
        assert_eq!(backoff.wait_before(9), Duration::from_millis(250));
    }

    #[test]
    fn a_transport_failure_is_retried() {
        assert!(is_retryable(None));
    }

    #[test]
    fn the_server_saying_later_is_retried() {
        for status in [408, 425, 429, 500, 502, 503, 504] {
            assert!(is_retryable(Some(status)), "{status}");
        }
    }

    #[test]
    fn a_client_mistake_is_not_retried() {
        // The row that matters. Retrying a 422 four times does not fix the
        // payload, and retrying a 402 charges the card again.
        for status in [400, 401, 402, 403, 404, 409, 422] {
            assert!(!is_retryable(Some(status)), "{status}");
        }
    }

    #[test]
    fn success_is_not_retried() {
        for status in [200, 201, 204, 301, 302] {
            assert!(!is_retryable(Some(status)), "{status}");
        }
    }
}
