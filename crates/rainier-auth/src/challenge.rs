//! Short-lived, single-use, attempt-limited codes — [`Challenges`].
//!
//! ```ignore
//! let code = challenges.issue(user.id, "email-change").await?;
//! mail.send(&user, &EmailChangeCode { code: code.clone() }).await?;
//!
//! // Later, from the form:
//! challenges.consume(user.id, "email-change", &submitted).await?;
//! ```
//!
//! The six-digit code somebody types. Signed URLs (`rainier_crypt::UrlSigner`)
//! cover the stateless half of this — a link that proves the application sent
//! it — and cannot cover this half, because a code short enough to read over
//! the phone is short enough to guess, so it needs an attempt counter, and an
//! attempt counter is state.
//!
//! # The three tables this replaces
//!
//! `mfa_challenges`, `verification_codes` and `email_change_requests` are the
//! same table three times: a subject, a purpose, a secret, an expiry, an
//! attempt count. They arrive separately because each is written when its
//! feature is, and they diverge in exactly the ways that matter — one forgets
//! to count attempts, another forgets to expire, a third is swept by a
//! scheduled job the others do not have.
//!
//! # What it guarantees
//!
//! - **Single use.** A consumed challenge is gone, so a code cannot be
//!   replayed by anyone who saw it in a notification, a log or a screenshot.
//! - **Attempt-limited.** After [`max_attempts`](Challenges::max_attempts)
//!   wrong answers the challenge is destroyed, not merely refused. A
//!   six-digit code has a million possibilities and a determined script has
//!   more than a million minutes.
//! - **Expiring.** The store drops it; nothing needs sweeping.
//! - **Constant-time comparison**, so the answer does not leak a digit at a
//!   time.
//!
//! # There is no sweep command, and that is the point
//!
//! Every one of those three tables came with a scheduled job to delete its
//! expired rows, and every one of those jobs is a thing to write, schedule,
//! monitor and eventually notice has been failing for a month.
//!
//! A challenge here is a cache entry with a TTL. The store drops it — Redis on
//! expiry, the in-memory one on read, DynamoDB by its own TTL — so there is
//! nothing accumulating and nothing to sweep. The right answer to "where is
//! the purge job" is that the design removed the need for one.
//!
//! # What it is not
//!
//! Not TOTP. An authenticator app's code is derived from a shared secret and a
//! clock, is not issued, and is not consumed — that is a library
//! (`totp-rs` is a good one), not a framework concern.

use std::sync::Arc;
use std::time::Duration;

use rainier_cache::{Cache, CacheExt};
use rainier_support::{Error, Result};
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;

/// A challenge in the store.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Pending {
    /// The code the holder must produce.
    code: String,
    /// How many wrong answers have been given.
    attempts: u32,
    /// How many are allowed before it is destroyed.
    max_attempts: u32,
}

/// Issues and consumes challenges.
///
/// Backed by a [`Cache`], so a challenge issued on one replica can be consumed
/// on another and the expiry is the store's problem rather than a scheduled
/// job's. Over a per-process cache it works and does not survive a restart or
/// reach a second replica — which for a code somebody is about to type is a
/// support ticket, so [`is_shared`](Self::is_shared) is worth asserting at
/// boot.
pub struct Challenges {
    cache: Arc<dyn Cache>,
    prefix: String,
    ttl: Duration,
    max_attempts: u32,
    digits: usize,
}

impl Challenges {
    /// Challenges in `cache`, lasting fifteen minutes, five attempts, six
    /// digits.
    pub fn new(cache: Arc<dyn Cache>) -> Self {
        Self {
            cache,
            prefix: "challenge:".to_string(),
            ttl: Duration::from_secs(900),
            max_attempts: 5,
            digits: 6,
        }
    }

    /// How long an issued challenge lasts.
    #[must_use = "this returns a configured issuer rather than configuring in place"]
    pub fn lasting(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    /// How many wrong answers destroy it.
    ///
    /// Destroyed rather than locked: a challenge that refuses further attempts
    /// but stays in the store is a challenge somebody has to expire, and the
    /// thing being protected is better served by making the holder start
    /// again.
    #[must_use = "this returns a configured issuer rather than configuring in place"]
    pub fn max_attempts(mut self, attempts: u32) -> Self {
        self.max_attempts = attempts;
        self
    }

    /// How many digits an issued code has.
    ///
    /// Six is the convention and the right default: short enough to read over
    /// the phone, and — with five attempts and fifteen minutes — leaving a
    /// guess one chance in two hundred thousand.
    #[must_use = "this returns a configured issuer rather than configuring in place"]
    pub fn digits(mut self, digits: usize) -> Self {
        self.digits = digits.clamp(4, 12);
        self
    }

    /// Namespace the keys.
    #[must_use = "this returns a configured issuer rather than configuring in place"]
    pub fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self
    }

    /// Whether a challenge issued here can be consumed on another replica.
    pub fn is_shared(&self) -> bool {
        self.cache.is_shared()
    }

    /// Issue a challenge for `subject` and `purpose`, returning the code.
    ///
    /// Issuing again **replaces** any outstanding one, which is what "resend
    /// the code" should do: two live codes for one purpose doubles an
    /// attacker's chances and confuses the person holding them.
    pub async fn issue(&self, subject: impl std::fmt::Display, purpose: &str) -> Result<String> {
        let code = self.generate();
        self.issue_code(subject, purpose, &code).await?;
        Ok(code)
    }

    /// Issue a challenge with a code you generated.
    ///
    /// For a code that has to come from somewhere else — a hardware token, a
    /// partner's API, a fixture in a test.
    pub async fn issue_code(
        &self,
        subject: impl std::fmt::Display,
        purpose: &str,
        code: &str,
    ) -> Result<()> {
        let pending =
            Pending { code: code.to_string(), attempts: 0, max_attempts: self.max_attempts };

        self.cache.put_json(&self.key(subject, purpose), &pending, Some(self.ttl)).await
    }

    /// Check `answer` and consume the challenge.
    ///
    /// # Errors
    ///
    /// - **No challenge** — none was issued, or it expired, or it was already
    ///   used. All three are one answer on purpose: distinguishing them tells
    ///   a guesser whether they are even attacking a live challenge.
    /// - **Wrong answer** — the attempt is counted, and the message says how
    ///   many are left so the holder is not surprised by the lockout.
    /// - **Too many attempts** — the challenge is destroyed and they start
    ///   again.
    pub async fn consume(
        &self,
        subject: impl std::fmt::Display + Clone,
        purpose: &str,
        answer: &str,
    ) -> Result<()> {
        let key = self.key(subject.clone(), purpose);

        let Some(mut pending) = self.cache.get_json::<Pending>(&key).await? else {
            return Err(Error::unauthorized("That code is not valid."));
        };

        // Constant time: comparing digit by digit and stopping at the first
        // mismatch leaks the code one digit at a time, and this one is short.
        if pending.code.as_bytes().ct_eq(answer.as_bytes()).into() {
            // Single use: gone before the caller acts on it, so a replay
            // arriving a moment later finds nothing.
            self.cache.forget(&key).await?;
            return Ok(());
        }

        pending.attempts += 1;

        if pending.attempts >= pending.max_attempts {
            self.cache.forget(&key).await?;
            return Err(Error::unauthorized(
                "That code is not valid, and there have been too many attempts. Request a new one.",
            ));
        }

        let remaining = pending.max_attempts - pending.attempts;

        // Written back with a fresh TTL rather than the remaining one, because
        // the cache port cannot read a key's remaining time. The effect is
        // that a wrong answer extends the window — which is the safe
        // direction: it costs an attacker nothing (they are limited by
        // attempts, not by time) and it stops a slow typist being cut off.
        self.cache.put_json(&key, &pending, Some(self.ttl)).await?;

        Err(Error::unauthorized(format!(
            "That code is not valid. {remaining} attempt(s) remaining."
        )))
    }

    /// Whether a challenge is outstanding, without consuming it.
    ///
    /// For a page that asks "we sent you a code" rather than "request one".
    pub async fn is_pending(&self, subject: impl std::fmt::Display, purpose: &str) -> Result<bool> {
        Ok(self.cache.get_json::<Pending>(&self.key(subject, purpose)).await?.is_some())
    }

    /// How many attempts remain, if a challenge is outstanding.
    pub async fn attempts_remaining(
        &self,
        subject: impl std::fmt::Display,
        purpose: &str,
    ) -> Result<Option<u32>> {
        Ok(self
            .cache
            .get_json::<Pending>(&self.key(subject, purpose))
            .await?
            .map(|pending| pending.max_attempts.saturating_sub(pending.attempts)))
    }

    /// Throw away an outstanding challenge.
    ///
    /// What "cancel" does, and what a completed flow calls for the challenges
    /// it did not use.
    pub async fn cancel(&self, subject: impl std::fmt::Display, purpose: &str) -> Result<()> {
        self.cache.forget(&self.key(subject, purpose)).await?;
        Ok(())
    }

    fn key(&self, subject: impl std::fmt::Display, purpose: &str) -> String {
        // The purpose is part of the key, so a code issued to change an email
        // address cannot be spent on removing a second factor.
        format!("{}{}:{}", self.prefix, purpose, subject)
    }

    /// A numeric code of [`digits`](Self::digits) digits.
    ///
    /// From the OS random source, not from a `%` of a timestamp: a code
    /// derived from the clock is a code somebody can predict, and the whole
    /// value of this is that they cannot.
    fn generate(&self) -> String {
        use rand::Rng;

        let mut rng = rand::thread_rng();
        (0..self.digits).map(|_| char::from(b'0' + rng.gen_range(0..10))).collect()
    }
}

impl std::fmt::Debug for Challenges {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Challenges")
            .field("store", &self.cache.name())
            .field("shared", &self.cache.is_shared())
            .field("ttl", &self.ttl)
            .field("max_attempts", &self.max_attempts)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_cache::MemoryCache;

    fn challenges() -> Challenges {
        Challenges::new(Arc::new(MemoryCache::new()))
    }

    #[tokio::test]
    async fn a_code_is_issued_and_consumed_once() {
        let challenges = challenges();
        let code = challenges.issue(42, "email-change").await.unwrap();

        assert_eq!(code.len(), 6);
        assert!(code.chars().all(|c| c.is_ascii_digit()));

        challenges.consume(42, "email-change", &code).await.unwrap();

        // The second time is a replay, and finds nothing.
        assert!(challenges.consume(42, "email-change", &code).await.is_err());
    }

    #[tokio::test]
    async fn a_code_is_bound_to_its_purpose() {
        // The one that matters: a code emailed to confirm an address must not
        // remove a second factor.
        let challenges = challenges();
        let code = challenges.issue(42, "email-change").await.unwrap();

        assert!(challenges.consume(42, "remove-factor", &code).await.is_err());
        assert!(challenges.consume(42, "email-change", &code).await.is_ok());
    }

    #[tokio::test]
    async fn a_code_is_bound_to_its_subject() {
        let challenges = challenges();
        let code = challenges.issue(42, "email-change").await.unwrap();

        assert!(challenges.consume(43, "email-change", &code).await.is_err());
    }

    #[tokio::test]
    async fn attempts_are_counted_and_then_it_is_destroyed() {
        let challenges = challenges().max_attempts(3);
        let code = challenges.issue(42, "verify").await.unwrap();

        assert_eq!(challenges.attempts_remaining(42, "verify").await.unwrap(), Some(3));

        assert!(challenges.consume(42, "verify", "000000").await.is_err());
        assert_eq!(challenges.attempts_remaining(42, "verify").await.unwrap(), Some(2));

        assert!(challenges.consume(42, "verify", "000000").await.is_err());
        assert!(challenges.consume(42, "verify", "000000").await.is_err());

        // Destroyed, not merely refused — so even the right code is now gone.
        assert!(!challenges.is_pending(42, "verify").await.unwrap());
        assert!(challenges.consume(42, "verify", &code).await.is_err());
    }

    #[tokio::test]
    async fn the_message_says_how_many_attempts_are_left() {
        let challenges = challenges().max_attempts(3);
        challenges.issue(42, "verify").await.unwrap();

        let error = challenges.consume(42, "verify", "000000").await.unwrap_err();

        assert!(error.message().contains('2'), "{}", error.message());
    }

    #[tokio::test]
    async fn a_wrong_answer_does_not_consume_the_challenge() {
        // Otherwise one typo means requesting a new code, which is the
        // behaviour everybody complains about.
        let challenges = challenges();
        let code = challenges.issue(42, "verify").await.unwrap();

        assert!(challenges.consume(42, "verify", "000000").await.is_err());
        assert!(challenges.consume(42, "verify", &code).await.is_ok());
    }

    #[tokio::test]
    async fn reissuing_replaces_the_outstanding_one() {
        // "Resend the code" must not leave two live codes: it doubles a
        // guesser's chances and confuses the person holding them.
        let challenges = challenges();
        let first = challenges.issue(42, "verify").await.unwrap();
        let second = challenges.issue(42, "verify").await.unwrap();

        assert!(challenges.consume(42, "verify", &first).await.is_err());
        assert!(challenges.consume(42, "verify", &second).await.is_ok());
    }

    #[tokio::test]
    async fn a_challenge_expires_on_its_own() {
        let challenges = challenges().lasting(Duration::from_millis(40));
        let code = challenges.issue(42, "verify").await.unwrap();

        tokio::time::sleep(Duration::from_millis(60)).await;

        assert!(!challenges.is_pending(42, "verify").await.unwrap());
        assert!(challenges.consume(42, "verify", &code).await.is_err());
    }

    #[tokio::test]
    async fn an_absent_and_an_expired_challenge_answer_identically() {
        // Telling them apart says whether there is even a live challenge to
        // attack.
        let challenges = challenges();

        let absent = challenges.consume(42, "verify", "000000").await.unwrap_err();

        challenges.issue(43, "verify").await.unwrap();
        challenges.cancel(43, "verify").await.unwrap();
        let used = challenges.consume(43, "verify", "000000").await.unwrap_err();

        assert_eq!(absent.message(), used.message());
    }

    #[tokio::test]
    async fn a_supplied_code_is_used_as_given() {
        let challenges = challenges();
        challenges.issue_code(42, "verify", "let-me-in").await.unwrap();

        assert!(challenges.consume(42, "verify", "let-me-in").await.is_ok());
    }

    #[tokio::test]
    async fn cancelling_removes_it() {
        let challenges = challenges();
        let code = challenges.issue(42, "verify").await.unwrap();

        challenges.cancel(42, "verify").await.unwrap();

        assert!(!challenges.is_pending(42, "verify").await.unwrap());
        assert!(challenges.consume(42, "verify", &code).await.is_err());
    }

    #[tokio::test]
    async fn codes_are_not_predictable() {
        // A weak check, but it catches the failure that matters: a generator
        // returning the same thing, or one derived from the clock.
        let challenges = challenges();
        let mut seen = std::collections::HashSet::new();

        for subject in 0..50 {
            seen.insert(challenges.issue(subject, "verify").await.unwrap());
        }

        assert!(seen.len() > 45, "only {} distinct codes in 50", seen.len());
    }

    #[test]
    fn the_digit_count_is_clamped_to_something_sensible() {
        // A two-digit code is a hundred guesses; a hundred-digit one is not a
        // code anybody types.
        assert_eq!(challenges().digits(1).generate().len(), 4);
        assert_eq!(challenges().digits(100).generate().len(), 12);
        assert_eq!(challenges().digits(8).generate().len(), 8);
    }

    #[tokio::test]
    async fn it_reports_whether_it_is_shared() {
        assert!(!challenges().is_shared());
    }
}
