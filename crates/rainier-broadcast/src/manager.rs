//! The thing an application holds — [`Broadcasting`].

use std::sync::Arc;

use serde_json::Value;

use rainier_support::Result;

use crate::broadcaster::{Broadcaster, LogBroadcaster};
use crate::channel::Channel;
use crate::event::{Broadcast, Broadcastable};

/// Publishes broadcasts through the configured driver.
///
/// What the `Broadcast` facade
/// resolves to.
pub struct Broadcasting {
    driver: Arc<dyn Broadcaster>,
}

impl Broadcasting {
    /// Publishing through `driver`.
    pub fn new(driver: Arc<dyn Broadcaster>) -> Self {
        Self { driver }
    }

    /// Publishing to the log — the safe default, and what an unconfigured
    /// application gets.
    pub fn log() -> Self {
        Self::new(Arc::new(LogBroadcaster))
    }

    /// The driver's name.
    pub fn driver_name(&self) -> &'static str {
        self.driver.name()
    }

    /// The driver, for the auth endpoint's signature.
    pub fn driver(&self) -> &Arc<dyn Broadcaster> {
        &self.driver
    }

    /// Publish an event.
    ///
    /// Does nothing when the event says [`broadcast_when`](Broadcastable::broadcast_when)
    /// is false, or when it named no channels — both are answers, not errors.
    pub async fn event<E: Broadcastable + ?Sized>(&self, event: &E) -> Result<()> {
        if !event.broadcast_when() {
            return Ok(());
        }
        self.send(Broadcast::of(event)?).await
    }

    /// Publish an event to everyone **except** one socket.
    ///
    /// The browser that caused the change has already applied it locally;
    /// echoing it back makes its own edit flicker. `socket_id` comes from the
    /// request's `X-Socket-ID` header, and `None` means broadcast to all.
    pub async fn event_except<E: Broadcastable + ?Sized>(
        &self,
        event: &E,
        socket_id: Option<String>,
    ) -> Result<()> {
        if !event.broadcast_when() {
            return Ok(());
        }
        self.send(Broadcast::of(event)?.except_maybe(socket_id)).await
    }

    /// Publish a payload directly, without an event type.
    ///
    /// For something assembled at run time — a channel name computed from
    /// data, a payload proxied from elsewhere.
    pub async fn to(
        &self,
        channels: Vec<Channel>,
        event: impl Into<String>,
        payload: Value,
    ) -> Result<()> {
        self.send(Broadcast::new(event, channels, payload)).await
    }

    /// Publish an already-rendered broadcast.
    pub async fn send(&self, broadcast: Broadcast) -> Result<()> {
        // No channels is not a failure — an event whose `broadcast_on`
        // returned nothing has decided this instance is not visible to anyone.
        if broadcast.is_empty() {
            return Ok(());
        }
        self.driver.publish(&broadcast).await
    }
}

impl std::fmt::Debug for Broadcasting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Broadcasting").field("driver", &self.driver.name()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::broadcaster::MemoryBroadcaster;
    use serde::Serialize;

    #[derive(Serialize)]
    struct Ping {
        room: u64,
        #[serde(skip)]
        visible: bool,
    }

    impl Broadcastable for Ping {
        fn broadcast_on(&self) -> Vec<Channel> {
            vec![Channel::private(format!("room.{}", self.room))]
        }
        fn broadcast_when(&self) -> bool {
            self.visible
        }
    }

    fn manager() -> (Broadcasting, Arc<MemoryBroadcaster>) {
        let driver = Arc::new(MemoryBroadcaster::new());
        (Broadcasting::new(driver.clone()), driver)
    }

    #[tokio::test]
    async fn an_event_reaches_the_driver() {
        let (broadcasting, driver) = manager();

        broadcasting.event(&Ping { room: 1, visible: true }).await.unwrap();

        driver.assert_broadcast("Ping", "private-room.1");
    }

    #[tokio::test]
    async fn broadcast_when_false_sends_nothing() {
        let (broadcasting, driver) = manager();

        broadcasting.event(&Ping { room: 1, visible: false }).await.unwrap();

        driver.assert_nothing_broadcast();
    }

    #[tokio::test]
    async fn to_others_carries_the_socket_to_skip() {
        let (broadcasting, driver) = manager();

        broadcasting
            .event_except(&Ping { room: 1, visible: true }, Some("1234.5678".into()))
            .await
            .unwrap();

        assert_eq!(driver.sent()[0].except.as_deref(), Some("1234.5678"));
    }

    #[tokio::test]
    async fn a_request_with_no_socket_header_broadcasts_to_everyone() {
        let (broadcasting, driver) = manager();

        broadcasting.event_except(&Ping { room: 1, visible: true }, None).await.unwrap();

        assert_eq!(driver.sent()[0].except, None);
    }

    #[tokio::test]
    async fn an_event_with_no_channels_never_reaches_the_driver() {
        let (broadcasting, driver) = manager();

        broadcasting.to(vec![], "manual", serde_json::json!({ "a": 1 })).await.unwrap();

        driver.assert_nothing_broadcast();
    }
}
