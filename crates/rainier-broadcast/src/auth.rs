//! Who may subscribe — [`ChannelRegistry`].
//!
//! A private channel is only private because something refuses the
//! subscription. That something is this: a list of patterns, each with a
//! callback that is handed the authenticated user and the pattern's captures.
//!
//! ```ignore
//! channels.channel("orders.{order}", |user: &User, params| {
//!     let order_id: u64 = params.parse("order")?;
//!     Box::pin(async move { Ok(ChannelAccess::allowed_if(owns_order(user, order_id).await?)) })
//! });
//! ```
//!
//! # Failing closed
//!
//! A channel with no matching pattern is **denied**. Not "allowed because
//! nobody said otherwise" — a typo in a pattern would then publish a private
//! channel to anyone who asked, and the failure would be silent in exactly the
//! direction that matters.

use std::sync::Arc;

use serde_json::Value;

use rainier_support::{BoxFuture, Error, Result};

use crate::channel::Channel;

/// The answer to "may this user subscribe?".
#[derive(Debug, Clone, PartialEq)]
pub enum ChannelAccess {
    /// No.
    Denied,
    /// Yes.
    Allowed,
    /// Yes, and this is what the other members of a presence channel should
    /// see about them.
    ///
    /// Whatever is put here is visible to **every other subscriber**, so it is
    /// a display name and an id, not a record.
    AllowedAs(Value),
}

impl ChannelAccess {
    /// Allowed if `condition`, denied otherwise — the shape most authorisers
    /// take.
    pub fn allowed_if(condition: bool) -> Self {
        if condition {
            ChannelAccess::Allowed
        } else {
            ChannelAccess::Denied
        }
    }

    /// Whether it was allowed at all.
    pub fn is_allowed(&self) -> bool {
        !matches!(self, ChannelAccess::Denied)
    }

    /// The presence roster entry, if there is one.
    pub fn member(&self) -> Option<&Value> {
        match self {
            ChannelAccess::AllowedAs(member) => Some(member),
            _ => None,
        }
    }
}

/// The values a channel pattern captured.
///
/// `orders.{order}` matched against `orders.7` captures `order = "7"`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChannelParams {
    values: Vec<(String, String)>,
}

impl ChannelParams {
    /// A capture by name.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.values.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str())
    }

    /// A capture, parsed.
    ///
    /// Fails with a `400` rather than a `500`: the value came from the client,
    /// so `orders.banana` is a bad request, not a bug.
    pub fn parse<T>(&self, name: &str) -> Result<T>
    where
        T: std::str::FromStr,
        T::Err: std::fmt::Display,
    {
        let raw = self
            .get(name)
            .ok_or_else(|| Error::internal(format!("the channel pattern captured no `{name}`")))?;

        raw.parse::<T>().map_err(|e| {
            Error::bad_request(format!("`{name}` in the channel name is invalid: {e}"))
        })
    }

    /// Every capture, in the pattern's order.
    pub fn all(&self) -> &[(String, String)] {
        &self.values
    }
}

/// What an authoriser is: the user, the captures, and an answer.
type Authorizer<U> = Arc<
    dyn for<'a> Fn(&'a U, &'a ChannelParams) -> BoxFuture<'a, Result<ChannelAccess>> + Send + Sync,
>;

struct ChannelRoute<U> {
    pattern: String,
    segments: Vec<Segment>,
    authorizer: Authorizer<U>,
}

#[derive(Debug, Clone, PartialEq)]
enum Segment {
    Literal(String),
    Capture(String),
}

/// The channel authorisation table.
///
/// Generic over the user model for the same reason
/// [`AuthManager`](rainier_support) is: an authoriser wants *your* user, with
/// its own columns, not a `dyn` that has to be downcast before it can answer
/// anything.
pub struct ChannelRegistry<U> {
    routes: Vec<ChannelRoute<U>>,
}

impl<U: Send + Sync + 'static> Default for ChannelRegistry<U> {
    fn default() -> Self {
        Self::new()
    }
}

impl<U: Send + Sync + 'static> ChannelRegistry<U> {
    /// An empty table. Every channel is denied until something is declared.
    pub fn new() -> Self {
        Self { routes: Vec::new() }
    }

    /// Authorise `pattern` with `authorizer`.
    ///
    /// `pattern` is the **bare** name — `orders.{order}`, never
    /// `private-orders.{order}`. The prefix says which kind of channel it is,
    /// and the same rule can serve a private and a presence channel.
    ///
    /// The first matching pattern wins, so declare the specific before the
    /// general.
    pub fn channel<F>(&mut self, pattern: impl Into<String>, authorizer: F) -> &mut Self
    where
        F: for<'a> Fn(&'a U, &'a ChannelParams) -> BoxFuture<'a, Result<ChannelAccess>>
            + Send
            + Sync
            + 'static,
    {
        let pattern = pattern.into();
        self.routes.push(ChannelRoute {
            segments: parse_pattern(&pattern),
            pattern,
            authorizer: Arc::new(authorizer),
        });
        self
    }

    /// May `user` subscribe to `channel`?
    ///
    /// A public channel is allowed without consulting the table — there is
    /// nothing to authorise, and a Pusher-protocol server never asks.
    pub async fn authorize(&self, user: &U, channel: &Channel) -> Result<ChannelAccess> {
        if !channel.needs_authorisation() {
            return Ok(ChannelAccess::Allowed);
        }

        let Some((route, params)) = self.matching(channel.name()) else {
            // Failing closed, and saying so: an undeclared channel is almost
            // always a pattern that does not match what the client sent.
            tracing::warn!(
                channel = channel.name(),
                "no channel authoriser matched; denying the subscription"
            );
            return Ok(ChannelAccess::Denied);
        };

        (route.authorizer)(user, &params).await
    }

    /// The declared patterns, for `channel:list` and for tests.
    pub fn patterns(&self) -> Vec<&str> {
        self.routes.iter().map(|route| route.pattern.as_str()).collect()
    }

    /// How many are declared.
    pub fn len(&self) -> usize {
        self.routes.len()
    }

    /// Whether none are — every private channel denied.
    pub fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }

    fn matching(&self, name: &str) -> Option<(&ChannelRoute<U>, ChannelParams)> {
        self.routes
            .iter()
            .find_map(|route| match_segments(&route.segments, name).map(|p| (route, p)))
    }
}

/// `orders.{order}` → `[Literal("orders"), Capture("order")]`.
fn parse_pattern(pattern: &str) -> Vec<Segment> {
    pattern
        .split('.')
        .map(|segment| match segment.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
            Some(name) => Segment::Capture(name.to_string()),
            None => Segment::Literal(segment.to_string()),
        })
        .collect()
}

/// Match a channel name against a pattern's segments.
///
/// Segment count must match exactly: `orders.{order}` does not match
/// `orders.7.items`, because a pattern that swallowed extra segments would
/// authorise channels its author never considered.
fn match_segments(segments: &[Segment], name: &str) -> Option<ChannelParams> {
    let parts: Vec<&str> = name.split('.').collect();
    if parts.len() != segments.len() {
        return None;
    }

    let mut params = ChannelParams::default();
    for (segment, part) in segments.iter().zip(parts) {
        match segment {
            Segment::Literal(literal) if literal == part => {}
            Segment::Literal(_) => return None,
            // An empty capture — `orders.` — is not a match. It would parse to
            // nothing and read as "any order".
            Segment::Capture(_) if part.is_empty() => return None,
            Segment::Capture(name) => params.values.push((name.clone(), part.to_string())),
        }
    }
    Some(params)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct User {
        id: u64,
    }

    fn registry() -> ChannelRegistry<User> {
        let mut registry = ChannelRegistry::new();

        registry.channel("orders.{order}", |user: &User, params: &ChannelParams| {
            Box::pin(async move {
                let order: u64 = params.parse("order")?;
                Ok(ChannelAccess::allowed_if(order == user.id))
            })
        });

        registry.channel("room.{room}", |user: &User, _| {
            Box::pin(async move {
                Ok(ChannelAccess::AllowedAs(serde_json::json!({ "user_id": user.id })))
            })
        });

        registry
    }

    #[tokio::test]
    async fn a_pattern_captures_and_the_authoriser_decides() {
        let registry = registry();

        let allowed =
            registry.authorize(&User { id: 7 }, &Channel::private("orders.7")).await.unwrap();
        assert_eq!(allowed, ChannelAccess::Allowed);

        let denied =
            registry.authorize(&User { id: 7 }, &Channel::private("orders.8")).await.unwrap();
        assert_eq!(denied, ChannelAccess::Denied);
    }

    #[tokio::test]
    async fn an_undeclared_channel_is_denied_rather_than_allowed() {
        // The direction a mistake has to fail in.
        let registry = registry();
        let access =
            registry.authorize(&User { id: 7 }, &Channel::private("secrets.1")).await.unwrap();

        assert_eq!(access, ChannelAccess::Denied);
    }

    #[tokio::test]
    async fn an_empty_registry_denies_everything_private() {
        let registry: ChannelRegistry<User> = ChannelRegistry::new();
        let access =
            registry.authorize(&User { id: 7 }, &Channel::private("orders.7")).await.unwrap();

        assert_eq!(access, ChannelAccess::Denied);
    }

    #[tokio::test]
    async fn a_public_channel_needs_no_declaration() {
        let registry: ChannelRegistry<User> = ChannelRegistry::new();
        let access = registry.authorize(&User { id: 7 }, &Channel::public("news")).await.unwrap();

        assert_eq!(access, ChannelAccess::Allowed);
    }

    #[tokio::test]
    async fn a_presence_channel_answers_with_its_roster_entry() {
        let access =
            registry().authorize(&User { id: 7 }, &Channel::presence("room.1")).await.unwrap();

        assert!(access.is_allowed());
        assert_eq!(access.member().unwrap()["user_id"], 7);
    }

    #[tokio::test]
    async fn a_pattern_does_not_swallow_extra_segments() {
        // `orders.{order}` must not authorise `orders.7.invoices`.
        let access = registry()
            .authorize(&User { id: 7 }, &Channel::private("orders.7.invoices"))
            .await
            .unwrap();

        assert_eq!(access, ChannelAccess::Denied);
    }

    #[tokio::test]
    async fn an_unparseable_capture_is_a_bad_request_not_a_panic() {
        let err = registry()
            .authorize(&User { id: 7 }, &Channel::private("orders.banana"))
            .await
            .unwrap_err();

        assert_eq!(err.status(), 400, "{}", err.message());
    }

    #[test]
    fn patterns_are_parsed_into_literals_and_captures() {
        assert_eq!(
            parse_pattern("orders.{order}.items"),
            vec![
                Segment::Literal("orders".into()),
                Segment::Capture("order".into()),
                Segment::Literal("items".into()),
            ]
        );
    }

    #[test]
    fn an_empty_capture_does_not_match() {
        assert!(match_segments(&parse_pattern("orders.{order}"), "orders.").is_none());
    }
}
