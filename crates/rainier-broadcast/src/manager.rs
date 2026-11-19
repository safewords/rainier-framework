//! The thing an application holds — [`Broadcasting`].

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;

use rainier_events::activity::publish_activity_with;
use rainier_events::{Activity, Fields};
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
    ///
    /// Publishes [`BroadcastSent`] on the event bus once the driver has
    /// answered.
    pub async fn send(&self, broadcast: Broadcast) -> Result<()> {
        // No channels is not a failure — an event whose `broadcast_on`
        // returned nothing has decided this instance is not visible to anyone.
        if broadcast.is_empty() {
            return Ok(());
        }

        let started = Instant::now();
        let result = self.send_traced(&broadcast).await;
        publish_activity_with(|| BroadcastSent {
            driver: self.driver.name(),
            event: broadcast.event.clone(),
            channels: broadcast.channels.len(),
            duration: started.elapsed(),
            error_type: result.as_ref().err().map(|e| e.kind().name()),
        });
        result
    }

    /// The publish, in a `broadcast.publish` span when `telemetry` is on.
    async fn send_traced(&self, broadcast: &Broadcast) -> Result<()> {
        // The event and how many channels — not the channels' names, which
        // carry ids (`private-user.42`), and never the payload.
        #[cfg(feature = "telemetry")]
        {
            let span = rainier_support::span!(
                "broadcast.publish",
                otel.name = %format!("broadcast {}", broadcast.event),
                otel.kind = "producer",
                rainier.broadcast.driver = self.driver.name(),
                rainier.broadcast.event = %broadcast.event,
                rainier.broadcast.channels = broadcast.channels.len(),
            );
            let traced = span.clone();
            return tracing::Instrument::instrument(
                async move {
                    rainier_support::telemetry::record_result(
                        &traced,
                        self.driver.publish(broadcast).await,
                    )
                },
                span,
            )
            .await;
        }

        #[cfg(not(feature = "telemetry"))]
        self.driver.publish(broadcast).await
    }
}

/// A broadcast was handed to the driver — or the driver refused it.
///
/// The bus's view of a `broadcast.publish` span, published whether or not the
/// crate's `telemetry` feature is on, with the same fields. Carries the event
/// and how many channels — not the channels' names, which carry ids
/// (`private-user.42`), and never the payload.
#[derive(Debug, Clone)]
pub struct BroadcastSent {
    /// The driver's name: `redis`, `kafka`, `log`, `memory`, …
    pub driver: &'static str,
    /// The event name clients listen for.
    pub event: String,
    /// How many channels it went to.
    pub channels: usize,
    /// How long the driver took.
    pub duration: Duration,
    /// The failure's kind, in `snake_case`, when the driver failed.
    pub error_type: Option<&'static str>,
}

impl Activity for BroadcastSent {
    fn kind(&self) -> &'static str {
        "broadcast.publish"
    }

    fn fields(&self, fields: &mut Fields) {
        fields
            .set("otel.name", format!("broadcast {}", self.event))
            .set("otel.kind", "producer")
            .set("rainier.broadcast.driver", self.driver)
            .set("rainier.broadcast.event", self.event.clone())
            .set("rainier.broadcast.channels", self.channels);
        if let Some(error_type) = self.error_type {
            fields.set("otel.status_code", "ERROR").set("error.type", error_type);
        }
    }

    fn duration(&self) -> Option<Duration> {
        Some(self.duration)
    }

    fn failed(&self) -> bool {
        self.error_type.is_some()
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
    async fn a_publish_is_a_broadcast_sent_with_the_spans_fields() {
        use rainier_events::{Dispatcher, Value as Field};

        let events = Arc::new(Dispatcher::new());
        let _bus = rainier_events::set_default(Arc::clone(&events));
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let s = Arc::clone(&seen);
        events.on::<BroadcastSent>().run_sync(move |sent, _| {
            s.lock().unwrap().push((*sent).clone());
            Ok(())
        });
        let (broadcasting, _) = manager();

        broadcasting.event(&Ping { room: 1, visible: true }).await.unwrap();
        // Nothing to publish, nothing published.
        broadcasting.event(&Ping { room: 1, visible: false }).await.unwrap();
        events.settled().await;

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].kind(), "broadcast.publish");
        assert!(!seen[0].failed());
        let fields = seen[0].collect_fields();
        assert_eq!(fields.text("otel.name"), Some("broadcast Ping"));
        assert_eq!(fields.text("otel.kind"), Some("producer"));
        assert_eq!(fields.text("rainier.broadcast.driver"), Some("memory"));
        assert_eq!(fields.text("rainier.broadcast.event"), Some("Ping"));
        assert_eq!(fields.get("rainier.broadcast.channels"), Some(&Field::U64(1)));
        assert!(!format!("{fields:?}").contains("room.1"), "never the channel names");
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
