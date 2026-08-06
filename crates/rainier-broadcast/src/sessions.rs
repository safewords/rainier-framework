//! Realtime delivery addressed to a **session** rather than to an account.
//!
//! # The problem
//!
//! Broadcast addresses a channel, and everyone authorised for that channel
//! receives everything published to it. The obvious channel for per-user data
//! is the user — `private-feed.{profile}` — and that is one channel for every
//! device they are signed in on.
//!
//! For some things that is right. An unread count is a property of the
//! account: identical on every device, and a second device learns nothing from
//! it the first did not already know.
//!
//! For anything answering a *request*, it is wrong. A page built because a
//! phone asked for it is delivered to the laptop as well — nothing crosses
//! accounts, and it is still somebody's reading history arriving on a screen
//! they are not looking at. A session is the unit a person thinks in.
//!
//! # The shape
//!
//! [`SessionChannels`] names a channel after a key held in the session, so a
//! reply goes to the one session that asked:
//!
//! ```text
//! private-{stem}{key}
//! ```
//!
//! The key is **server-generated and lives in the session**. It is never in a
//! URL, never chosen by the client, and never accepted from a request — which
//! closes two holes at once. A key in a query string reaches proxy logs, CDN
//! logs, browser history and any `Referer` that leaves the page; and a caller
//! who could name a channel could have their content published onto somebody
//! else's.
//!
//! The name is not a secret and does not need to be. It is a private channel:
//! subscribing takes a signature, and the auth route only signs a key the
//! caller's own session already holds. Knowing the name buys nothing.
//!
//! # Reaching every session of one subject
//!
//! Session channels give up the one thing an account channel was good at:
//! telling every device at once. [`SessionRoster`] gives it back — which
//! sessions a subject has open, so a change that happened *underneath* them
//! can be published to each.
//!
//! Prefer the single session. Something sent to every session because it was
//! easier is the habit this exists to break.

use std::time::Duration;

use async_trait::async_trait;
use rainier_support::Result;

use crate::channel::Channel;

/// How a session's channel is named.
///
/// The stem is the application's — `feed-session.`, `inbox-session.` — so one
/// application can address more than one kind of per-session stream without
/// them colliding.
#[derive(Debug, Clone)]
pub struct SessionChannels {
    stem: String,
}

impl SessionChannels {
    /// Channels named `private-{stem}{key}`.
    ///
    /// The stem should end in a separator (`.` or `-`). Without one,
    /// `feedsession{key}` still works and reads as a mistake in every log it
    /// appears in.
    pub fn new(stem: impl Into<String>) -> Self {
        Self { stem: stem.into() }
    }

    /// What this application calls its per-session channels.
    pub fn stem(&self) -> &str {
        &self.stem
    }

    /// The channel to publish a reply for `key` on.
    pub fn channel(&self, key: &str) -> Channel {
        Channel::private(format!("{}{key}", self.stem))
    }

    /// The name a client subscribes to, `private-` included.
    pub fn wire_name(&self, key: &str) -> String {
        format!("private-{}{key}", self.stem)
    }

    /// The key a subscribe request is asking about, if it is even one of ours.
    ///
    /// `None` for any other channel, which an auth route must treat as "not
    /// mine to answer for" — answering would authorise a channel without
    /// whatever guard its own route runs.
    pub fn key_of<'a>(&self, wire_name: &'a str) -> Option<&'a str> {
        wire_name
            .strip_prefix("private-")
            .and_then(|rest| rest.strip_prefix(self.stem.as_str()))
            .filter(|key| Self::is_valid_key(key))
    }

    /// A new key.
    ///
    /// Alphanumeric and long enough not to be guessed. Guessing one buys
    /// nothing on its own — a subscription still needs a signature — but a key
    /// is also what an auth route compares against, and a short one makes that
    /// comparison worth attempting.
    pub fn mint() -> String {
        use rand::Rng;

        // Two u64s of randomness rendered as hex: 32 alphanumeric characters,
        // from the OS entropy source rather than a seeded generator. A key
        // that a restart could produce twice would put two sessions on one
        // channel, which is what this module exists to prevent.
        let mut rng = rand::thread_rng();
        format!("{:016x}{:016x}", rng.gen::<u64>(), rng.gen::<u64>())
    }

    /// Whether a key may be used as part of a channel name.
    ///
    /// Keys are minted by [`mint`](Self::mint) and so are always well formed.
    /// This is here because the value round-trips through a session store, and
    /// a channel name is not where to discover that it came back wrong.
    ///
    /// No `:` — a Pusher server splits an application prefix on it — and no
    /// `-`, which is how `private-` and `presence-` are recognised. A key
    /// carrying either could name a different kind of channel entirely.
    pub fn is_valid_key(key: &str) -> bool {
        (16..=64).contains(&key.len()) && key.chars().all(|c| c.is_ascii_alphanumeric())
    }
}

/// Which sessions a subject has open.
///
/// A **hint**, and nothing may depend on it being complete. A missing key
/// costs one device an update it would have had; a stale one costs a message
/// published to a channel nobody is listening on. Both are cheap, which is
/// what lets an implementation be a cache entry rather than a table.
#[async_trait]
pub trait SessionRoster: Send + Sync {
    /// Note that `subject` is reading in `key`.
    ///
    /// Must not fail its caller. A request that could not record a session
    /// still gets its own answer; it is other devices that miss an update.
    async fn remember(&self, subject: u64, key: &str);

    /// The keys `subject` is reading on, most recently seen first.
    ///
    /// Empty when nothing is known, which is the safe answer: a caller fanning
    /// out publishes nothing.
    async fn of(&self, subject: u64) -> Vec<String>;

    /// Drop one session, when it ends.
    ///
    /// Optional in the sense that a TTL would drop it eventually, and worth
    /// doing at sign-out so a signed-out device stops being published to
    /// immediately rather than for the rest of its lifetime.
    async fn forget(&self, subject: u64, key: &str);
}

/// How long a roster entry stands without being seen again, by default.
///
/// Longer than a session sits idle and far shorter than for ever. Too long
/// costs a message published to nobody; too short and a device that has been
/// quiet stops getting live updates until it next asks for something.
pub const DEFAULT_ROSTER_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// How many sessions to track per subject, by default.
///
/// Generous for a person, and bounded so one account cannot accumulate keys
/// without limit — every one of them is a message published on every fan-out.
pub const DEFAULT_ROSTER_LIMIT: usize = 12;

/// A roster that remembers nothing.
///
/// For an application that only ever replies to the session that asked, which
/// is the case worth defaulting to. Fanning out with this installed publishes
/// nothing rather than failing, so a feature that needs a real roster and does
/// not have one is quiet rather than broken — check here first if a fan-out
/// reaches no device.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoRoster;

#[async_trait]
impl SessionRoster for NoRoster {
    async fn remember(&self, _subject: u64, _key: &str) {}

    async fn of(&self, _subject: u64) -> Vec<String> {
        Vec::new()
    }

    async fn forget(&self, _subject: u64, _key: &str) {}
}

/// Every channel a subject's open sessions are listening on.
///
/// The fan-out helper: read the roster, name a channel per key. Publishing to
/// the result reaches every device without any of them sharing a channel.
pub async fn channels_of(
    roster: &dyn SessionRoster,
    channels: &SessionChannels,
    subject: u64,
) -> Vec<Channel> {
    roster.of(subject).await.iter().map(|key| channels.channel(key)).collect()
}

/// Reasons an auth route refuses to sign a session channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Not a session channel at all. Another route's to answer for.
    NotOurs,
    /// A session channel, and not this caller's.
    NotYours,
}

/// Whether this caller may subscribe to this channel.
///
/// `held` is the key from the caller's **own session**, read and never
/// created: minting one here would sign whatever was asked for, which is the
/// whole thing this prevents.
///
/// Compared in constant time. It is a secret against a caller-supplied string,
/// and returning at the first difference says how much of it was right.
pub fn authorize(
    channels: &SessionChannels,
    wire_name: &str,
    held: Option<&str>,
) -> Result<(), Refusal> {
    let requested = channels.key_of(wire_name).ok_or(Refusal::NotOurs)?;
    let held = held.ok_or(Refusal::NotYours)?;

    if held.len() != requested.len() {
        return Err(Refusal::NotYours);
    }

    let differing =
        held.bytes().zip(requested.bytes()).fold(0u8, |differing, (a, b)| differing | (a ^ b));

    if differing == 0 {
        Ok(())
    } else {
        Err(Refusal::NotYours)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "0123456789abcdef0123456789abcdef";
    const OTHER: &str = "fedcba9876543210fedcba9876543210";

    fn channels() -> SessionChannels {
        SessionChannels::new("feed-session.")
    }

    #[test]
    fn two_sessions_do_not_share_a_channel() {
        // The property the whole module exists for.
        assert_ne!(channels().wire_name(KEY), channels().wire_name(OTHER));
    }

    #[test]
    fn a_channel_is_private_so_the_name_need_not_be_secret() {
        // Public would mean the name *is* the credential, which puts a bearer
        // token everywhere a channel name goes.
        assert!(channels().wire_name(KEY).starts_with("private-"));
    }

    #[test]
    fn a_key_round_trips_through_its_channel_name() {
        assert_eq!(channels().key_of(&channels().wire_name(KEY)), Some(KEY));
    }

    #[test]
    fn another_kind_of_channel_is_not_ours_to_answer_for() {
        // An auth route that answered for these would authorise them without
        // whatever guard their own route runs.
        for other in ["private-profile.7", "presence-room.7", "private-feed.7", "feed-session.x"] {
            assert_eq!(channels().key_of(other), None, "{other}");
        }
    }

    #[test]
    fn a_minted_key_is_usable_as_a_channel_name() {
        let key = SessionChannels::mint();

        assert!(SessionChannels::is_valid_key(&key), "{key}");
        assert_eq!(channels().key_of(&channels().wire_name(&key)), Some(key.as_str()));
    }

    #[test]
    fn minting_twice_does_not_give_the_same_key() {
        assert_ne!(SessionChannels::mint(), SessionChannels::mint());
    }

    #[test]
    fn a_key_that_could_name_another_kind_of_channel_is_refused() {
        // Keys are server-generated, so this is a backstop — but the value
        // round-trips through a session store, and a channel name is not where
        // to find out it came back wrong.
        for hostile in [
            "private-feed.7",
            "presence-room.7",
            "app-prefix:private-feed.7",
            "feed.7",
            "short",
            "",
        ] {
            assert!(!SessionChannels::is_valid_key(hostile), "{hostile}");
        }
    }

    #[test]
    fn a_session_may_subscribe_to_its_own_channel() {
        assert_eq!(authorize(&channels(), &channels().wire_name(KEY), Some(KEY)), Ok(()));
    }

    #[test]
    fn a_session_may_not_subscribe_to_another_sessions_channel() {
        // Including another session of the same person, which is the case this
        // whole module was written for.
        assert_eq!(
            authorize(&channels(), &channels().wire_name(OTHER), Some(KEY)),
            Err(Refusal::NotYours),
        );
    }

    #[test]
    fn a_caller_with_no_session_may_not_subscribe() {
        assert_eq!(
            authorize(&channels(), &channels().wire_name(KEY), None),
            Err(Refusal::NotYours),
        );
    }

    #[test]
    fn a_prefix_of_a_key_is_not_the_key() {
        // A length check after the byte loop, or a `starts_with`, would let a
        // caller narrow a key one character at a time.
        let short = &KEY[..KEY.len() - 1];

        assert_eq!(
            authorize(&channels(), &format!("private-feed-session.{short}"), Some(KEY)),
            Err(Refusal::NotYours),
        );
    }

    #[tokio::test]
    async fn a_roster_that_remembers_nothing_fans_out_to_nothing() {
        // Quiet rather than broken: a fan-out with no roster installed
        // publishes nowhere instead of failing.
        let reached = channels_of(&NoRoster, &channels(), 7).await;

        assert!(reached.is_empty());
    }
}
