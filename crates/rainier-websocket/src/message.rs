//! What travels over a socket — [`Message`].

use serde::Serialize;
use serde_json::Value;

use rainier_support::{Error, Result};

/// One WebSocket frame's payload.
///
/// Ping and pong are **not** here. They are keep-alive, the transport answers
/// them, and a handler that had to remember to reply to a ping would have a
/// connection that dies when it forgets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// A text frame. UTF-8 by definition of the protocol.
    Text(String),
    /// A binary frame.
    Binary(Vec<u8>),
    /// The peer is closing, with an optional reason.
    ///
    /// Delivered to [`on_message`](crate::WebSocketHandler::on_message) only if
    /// a handler asked for it; [`on_close`](crate::WebSocketHandler::on_close)
    /// is the usual place.
    Close(Option<String>),
}

impl Message {
    /// A text frame.
    pub fn text(body: impl Into<String>) -> Self {
        Message::Text(body.into())
    }

    /// A binary frame.
    pub fn binary(body: impl Into<Vec<u8>>) -> Self {
        Message::Binary(body.into())
    }

    /// A text frame holding `value` as JSON — how nearly every application
    /// actually talks over a socket.
    pub fn json(value: &impl Serialize) -> Result<Self> {
        serde_json::to_string(value)
            .map(Message::Text)
            .map_err(|e| Error::internal(format!("a socket message must serialise: {e}")))
    }

    /// The text, if this is a text frame.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Message::Text(text) => Some(text),
            _ => None,
        }
    }

    /// The bytes, whichever kind of frame this is.
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            Message::Text(text) => text.as_bytes(),
            Message::Binary(bytes) => bytes,
            Message::Close(reason) => reason.as_deref().unwrap_or("").as_bytes(),
        }
    }

    /// Parse a text frame as JSON.
    ///
    /// A **bad request**, not an internal error: the bytes came from a client,
    /// and a client that sends nonsense is not a bug in your handler.
    pub fn parse_json(&self) -> Result<Value> {
        let text = self
            .as_text()
            .ok_or_else(|| Error::bad_request("expected a text frame, got a binary one."))?;

        serde_json::from_str(text)
            .map_err(|e| Error::bad_request(format!("that is not valid JSON: {e}")))
    }

    /// Whether this is the peer closing.
    pub fn is_close(&self) -> bool {
        matches!(self, Message::Close(_))
    }

    /// How many bytes the payload is.
    pub fn len(&self) -> usize {
        self.as_bytes().len()
    }

    /// Whether the payload is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl From<&str> for Message {
    fn from(text: &str) -> Self {
        Message::text(text)
    }
}

impl From<String> for Message {
    fn from(text: String) -> Self {
        Message::Text(text)
    }
}

impl From<Vec<u8>> for Message {
    fn from(bytes: Vec<u8>) -> Self {
        Message::Binary(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_round_trips_through_a_text_frame() {
        let message = Message::json(&serde_json::json!({ "room": 1 })).unwrap();

        assert!(matches!(message, Message::Text(_)));
        assert_eq!(message.parse_json().unwrap()["room"], 1);
    }

    #[test]
    fn nonsense_from_a_client_is_a_bad_request_not_a_500() {
        let err = Message::text("{not json").parse_json().unwrap_err();
        assert_eq!(err.status(), 400, "{}", err.message());
    }

    #[test]
    fn asking_a_binary_frame_for_json_says_which_kind_it_was() {
        let err = Message::binary(vec![1, 2, 3]).parse_json().unwrap_err();
        assert!(err.message().contains("binary"), "{}", err.message());
    }
}
