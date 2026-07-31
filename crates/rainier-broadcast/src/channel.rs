//! Where a broadcast goes — [`Channel`], and the three kinds there are.

use std::fmt;

/// One channel a broadcast is published to.
///
/// Three kinds, and the difference is entirely about who may
/// subscribe:
///
/// | | Who may listen | Prefix on the wire |
/// |---|---|---|
/// | [`public`](Channel::public) | anyone who knows the name | none |
/// | [`private`](Channel::private) | whoever the authoriser allows | `private-` |
/// | [`presence`](Channel::presence) | ditto, and everyone sees who else is there | `presence-` |
///
/// The prefix is not decoration. It is how a Pusher-protocol server knows to
/// demand an authorisation signature before it lets a socket subscribe, so a
/// channel that should be private and is not prefixed is a channel anyone can
/// read.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Channel {
    /// Anyone may subscribe. For a public feed and nothing else.
    Public(String),
    /// Subscribing requires authorisation.
    Private(String),
    /// Like private, and members can see each other.
    Presence(String),
}

impl Channel {
    /// A public channel. No authorisation, so assume the whole internet is
    /// listening.
    pub fn public(name: impl Into<String>) -> Self {
        Self::Public(name.into())
    }

    /// A private channel, gated by an [authoriser](crate::ChannelRegistry).
    pub fn private(name: impl Into<String>) -> Self {
        Self::Private(name.into())
    }

    /// A presence channel — private, plus a roster.
    pub fn presence(name: impl Into<String>) -> Self {
        Self::Presence(name.into())
    }

    /// The bare name, without the kind's prefix.
    ///
    /// What an [authoriser](crate::ChannelRegistry) pattern is matched
    /// against: `orders.7`, not `private-orders.7`.
    pub fn name(&self) -> &str {
        match self {
            Channel::Public(name) | Channel::Private(name) | Channel::Presence(name) => name,
        }
    }

    /// The name as it appears on the wire, prefix and all.
    pub fn wire_name(&self) -> String {
        match self {
            Channel::Public(name) => name.clone(),
            Channel::Private(name) => format!("private-{name}"),
            Channel::Presence(name) => format!("presence-{name}"),
        }
    }

    /// Whether subscribing to it has to be authorised.
    pub fn needs_authorisation(&self) -> bool {
        !matches!(self, Channel::Public(_))
    }

    /// Whether it carries a roster.
    pub fn is_presence(&self) -> bool {
        matches!(self, Channel::Presence(_))
    }

    /// Read a wire name back into a channel.
    ///
    /// What the authorisation endpoint receives: the client sends
    /// `private-orders.7` and the prefix is the only thing saying which kind it
    /// meant.
    pub fn from_wire_name(wire: &str) -> Self {
        if let Some(name) = wire.strip_prefix("private-") {
            Channel::Private(name.to_string())
        } else if let Some(name) = wire.strip_prefix("presence-") {
            Channel::Presence(name.to_string())
        } else {
            Channel::Public(wire.to_string())
        }
    }
}

impl fmt::Display for Channel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.wire_name())
    }
}

impl From<&str> for Channel {
    /// A bare string is a **public** channel.
    ///
    /// Deliberately the safe reading of an ambiguous one: a name with no
    /// prefix has claimed no protection, and inferring privacy from a name
    /// that looks sensitive would be guessing.
    fn from(name: &str) -> Self {
        Channel::public(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_prefix_is_what_a_pusher_server_reads() {
        assert_eq!(Channel::public("chat").wire_name(), "chat");
        assert_eq!(Channel::private("orders.7").wire_name(), "private-orders.7");
        assert_eq!(Channel::presence("room.1").wire_name(), "presence-room.1");
    }

    #[test]
    fn a_wire_name_round_trips() {
        for channel in
            [Channel::public("chat"), Channel::private("orders.7"), Channel::presence("room.1")]
        {
            assert_eq!(Channel::from_wire_name(&channel.wire_name()), channel);
        }
    }

    #[test]
    fn the_authoriser_sees_the_bare_name() {
        // The pattern in `routes/channels.rs` is `orders.{order}`, so matching
        // has to happen without the prefix.
        assert_eq!(Channel::private("orders.7").name(), "orders.7");
    }

    #[test]
    fn only_public_channels_skip_authorisation() {
        assert!(!Channel::public("chat").needs_authorisation());
        assert!(Channel::private("chat").needs_authorisation());
        assert!(Channel::presence("chat").needs_authorisation());
    }
}
