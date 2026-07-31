//! What gets broadcast — [`Broadcastable`] and the [`Broadcast`] it becomes.

use serde::Serialize;
use serde_json::Value;

use rainier_support::{str::class_basename, Error, Result};

use crate::channel::Channel;

/// An event that should reach a browser.
///
/// Nothing here is discovered:
/// implementing this makes an event *broadcastable*, and something still has to
/// broadcast it — see [`crate::Broadcasting`].
///
/// ```ignore
/// #[derive(Clone, Serialize)]
/// pub struct OrderShipped {
///     pub order_id: u64,
///     pub tracking: String,
/// }
///
/// impl Broadcastable for OrderShipped {
///     fn broadcast_on(&self) -> Vec<Channel> {
///         vec![Channel::private(format!("orders.{}", self.order_id))]
///     }
/// }
/// ```
///
/// # Everything it sends is public
///
/// The payload leaves your process for a browser you do not control, so it is
/// the one place in an application where "this field is internal" has to be
/// enforced by what you put in rather than by who asks. `Serialize` is required
/// precisely so that `#[serde(skip)]` is the tool for it, the same one a
/// response body uses.
pub trait Broadcastable: Serialize + Send + Sync + 'static {
    /// The channels to publish on.
    ///
    /// Empty means nothing is sent — a legitimate answer, and the shape
    /// `broadcast_on` takes when visibility depends on the event's own data.
    fn broadcast_on(&self) -> Vec<Channel>;

    /// The event's name on the wire.
    ///
    /// Defaults to the type's short name, `OrderShipped` — which is what a
    /// JavaScript client listens for. **Permanent** once a client listens for
    /// it: renaming the struct renames the event, and the listener goes quiet
    /// rather than erroring. Override it to pin the name down:
    ///
    /// ```ignore
    /// fn broadcast_as(&self) -> String { "order.shipped".into() }
    /// ```
    fn broadcast_as(&self) -> String {
        class_basename(std::any::type_name::<Self>()).to_string()
    }

    /// The payload.
    ///
    /// Defaults to the event serialised, field for field.
    fn broadcast_with(&self) -> Result<Value> {
        serde_json::to_value(self)
            .map_err(|e| Error::internal(format!("a broadcast payload must serialise: {e}")))
    }

    /// Whether to send it at all.
    fn broadcast_when(&self) -> bool {
        true
    }
}

/// One rendered broadcast, on its way to a [`Broadcaster`](crate::Broadcaster).
///
/// Not itself `Serialize`: what goes over the wire is
/// [`wire_payload`](Self::wire_payload), which is the shape a relay reads, and
/// the channels are addressing rather than content.
#[derive(Debug, Clone, PartialEq)]
pub struct Broadcast {
    /// Where it goes.
    pub channels: Vec<Channel>,
    /// The event name clients listen for.
    pub event: String,
    /// The payload.
    pub payload: Value,
    /// A socket id to **exclude**.
    ///
    /// The browser that caused the event has usually updated itself already,
    /// and echoing the change back makes its own edit flicker.
    pub except: Option<String>,
}

impl Broadcast {
    /// Render `event`.
    pub fn of<E: Broadcastable + ?Sized>(event: &E) -> Result<Self> {
        Ok(Self {
            channels: event.broadcast_on(),
            event: event.broadcast_as(),
            payload: event.broadcast_with()?,
            except: None,
        })
    }

    /// A broadcast assembled by hand, for something that is not a struct.
    pub fn new(event: impl Into<String>, channels: Vec<Channel>, payload: Value) -> Self {
        Self { channels, event: event.into(), payload, except: None }
    }

    /// Exclude the socket that caused it.
    pub fn except(mut self, socket_id: impl Into<String>) -> Self {
        self.except = Some(socket_id.into());
        self
    }

    /// Exclude a socket, when there is one — what a request handler has.
    pub fn except_maybe(mut self, socket_id: Option<String>) -> Self {
        self.except = socket_id;
        self
    }

    /// Whether it has anywhere to go.
    pub fn is_empty(&self) -> bool {
        self.channels.is_empty()
    }

    /// The wire form one channel's subscribers receive.
    ///
    /// The shape Pusher-protocol relays read: the event, the
    /// data, and the socket to skip.
    pub fn wire_payload(&self) -> Value {
        let mut body = serde_json::json!({
            "event": self.event,
            "data": self.payload,
        });

        // Only present when there is one: a relay treats the key's
        // presence as "there is a socket to skip", not its value.
        if let Some(socket) = &self.except {
            body["socket"] = Value::String(socket.clone());
        }
        body
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize)]
    struct OrderShipped {
        order_id: u64,
        /// Read by nothing on purpose: the point is that it never leaves.
        #[serde(skip)]
        #[allow(dead_code)]
        internal_cost: u64,
    }

    impl Broadcastable for OrderShipped {
        fn broadcast_on(&self) -> Vec<Channel> {
            vec![Channel::private(format!("orders.{}", self.order_id))]
        }
    }

    fn shipped() -> OrderShipped {
        OrderShipped { order_id: 7, internal_cost: 1200 }
    }

    #[test]
    fn the_event_name_defaults_to_the_type_name() {
        assert_eq!(shipped().broadcast_as(), "OrderShipped");
    }

    #[test]
    fn the_payload_defaults_to_the_serialised_event() {
        let payload = shipped().broadcast_with().unwrap();

        assert_eq!(payload["order_id"], 7);
        assert!(payload.get("internal_cost").is_none(), "`skip` is the visibility control");
    }

    #[test]
    fn an_overridden_name_wins() {
        struct Pinned;
        impl Serialize for Pinned {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_unit()
            }
        }
        impl Broadcastable for Pinned {
            fn broadcast_on(&self) -> Vec<Channel> {
                vec![]
            }
            fn broadcast_as(&self) -> String {
                "order.shipped".into()
            }
        }

        assert_eq!(Broadcast::of(&Pinned).unwrap().event, "order.shipped");
    }

    #[test]
    fn the_wire_payload_omits_the_socket_unless_there_is_one() {
        let broadcast = Broadcast::of(&shipped()).unwrap();
        assert!(broadcast.wire_payload().get("socket").is_none());

        let to_others = broadcast.except("1234.5678");
        assert_eq!(to_others.wire_payload()["socket"], "1234.5678");
    }

    #[test]
    fn an_event_with_no_channels_has_nowhere_to_go() {
        struct Nowhere;
        impl Serialize for Nowhere {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_unit()
            }
        }
        impl Broadcastable for Nowhere {
            fn broadcast_on(&self) -> Vec<Channel> {
                vec![]
            }
        }

        assert!(Broadcast::of(&Nowhere).unwrap().is_empty());
    }
}
