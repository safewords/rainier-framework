//! The bus's view of a socket's work: [`WebSocketConnected`] and
//! [`WebSocketMessageHandled`], the activities for the `websocket.connect` and
//! `websocket.message` spans.
//!
//! This crate is the contract, not the transport: the server that does the
//! upgrade and the framing (`rainier-server`) times the handler's
//! [`on_connect`](crate::WebSocketHandler::on_connect) and
//! [`on_message`](crate::WebSocketHandler::on_message) and publishes these,
//! whether or not its `telemetry` feature is on, with the same fields its
//! spans carry. They live here so a subscriber can name them without
//! depending on the server.
//!
//! Never the socket's path or a message's contents — the kind and the size.

use std::time::Duration;

use rainier_events::{Activity, Fields};
use rainier_support::Error;

use crate::Message;

/// A handler's [`on_connect`](crate::WebSocketHandler::on_connect) ran.
#[derive(Debug, Clone)]
pub struct WebSocketConnected {
    /// How long `on_connect` took.
    pub duration: Duration,
    /// The failure's kind, in `snake_case`, when `on_connect` failed and the
    /// socket was closed.
    pub error_type: Option<&'static str>,
}

impl WebSocketConnected {
    /// The event for an `on_connect` that took `duration` and returned
    /// `result`.
    pub fn new(duration: Duration, result: &Result<(), Error>) -> Self {
        Self { duration, error_type: result.as_ref().err().map(|e| e.kind().name()) }
    }
}

impl Activity for WebSocketConnected {
    fn kind(&self) -> &'static str {
        "websocket.connect"
    }

    fn fields(&self, fields: &mut Fields) {
        fields.set("otel.name", "websocket connect");
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

/// A handler's [`on_message`](crate::WebSocketHandler::on_message) ran for
/// one message from the client.
#[derive(Debug, Clone)]
pub struct WebSocketMessageHandled {
    /// `text`, `binary` or `close`.
    pub message_kind: &'static str,
    /// The payload's size in bytes; 0 for a close.
    pub bytes: usize,
    /// How long `on_message` took.
    pub duration: Duration,
    /// The failure's kind, in `snake_case`, when `on_message` failed.
    pub error_type: Option<&'static str>,
}

impl WebSocketMessageHandled {
    /// What [`WebSocketMessageHandled`] records of `message`: its kind and
    /// its size. Take it before the message is handed to the handler.
    pub fn shape(message: &Message) -> (&'static str, usize) {
        match message {
            Message::Text(text) => ("text", text.len()),
            Message::Binary(bytes) => ("binary", bytes.len()),
            Message::Close(_) => ("close", 0),
        }
    }

    /// The event for a message of `shape` whose `on_message` took `duration`
    /// and returned `result`.
    pub fn new(
        shape: (&'static str, usize),
        duration: Duration,
        result: &Result<(), Error>,
    ) -> Self {
        Self {
            message_kind: shape.0,
            bytes: shape.1,
            duration,
            error_type: result.as_ref().err().map(|e| e.kind().name()),
        }
    }
}

impl Activity for WebSocketMessageHandled {
    fn kind(&self) -> &'static str {
        "websocket.message"
    }

    fn fields(&self, fields: &mut Fields) {
        fields
            .set("otel.name", "websocket message")
            .set("rainier.websocket.kind", self.message_kind)
            .set("rainier.websocket.bytes", self.bytes);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_records_its_kind_and_size_and_never_its_contents() {
        let message = Message::Text("hello there".into());
        let event = WebSocketMessageHandled::new(
            WebSocketMessageHandled::shape(&message),
            Duration::from_millis(2),
            &Ok(()),
        );
        assert_eq!(event.kind(), "websocket.message");
        let fields = event.collect_fields();
        assert_eq!(fields.text("otel.name"), Some("websocket message"));
        assert_eq!(fields.text("rainier.websocket.kind"), Some("text"));
        assert_eq!(fields.get("rainier.websocket.bytes"), Some(&rainier_events::Value::U64(11)));
        assert!(!format!("{fields:?}").contains("hello"));
        assert!(!event.failed());
    }

    #[test]
    fn a_failed_connect_records_the_errors_kind() {
        let event = WebSocketConnected::new(Duration::ZERO, &Err(Error::unauthorized("no")));
        assert_eq!(event.kind(), "websocket.connect");
        assert!(event.failed());
        let fields = event.collect_fields();
        assert_eq!(fields.text("otel.name"), Some("websocket connect"));
        assert_eq!(fields.text("otel.status_code"), Some("ERROR"));
        assert_eq!(fields.text("error.type"), Some("unauthorized"));
    }
}
