//! The Pusher protocol's subscription signature — [`PusherAuth`].
//!
//! Every Pusher-compatible server (Pusher itself, soketi, and the rest)
//! gates a private or presence subscription the same way: the browser asks your
//! application, and your application answers with an HMAC that proves it
//! agreed. The server can then verify the answer without asking you anything,
//! because it has the same secret.
//!
//! There is no HTTP here. Signing is the whole of the application's side of
//! the protocol — publishing goes out over Redis — so this is a pure function
//! of the socket id, the channel and the secret.

use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::Sha256;

use rainier_support::{Error, Result};

use crate::channel::Channel;

type HmacSha256 = Hmac<Sha256>;

/// The app credentials a Pusher-protocol server shares with you.
///
/// The **secret** is what makes a signature mean anything, so it belongs in the
/// environment beside the encryption key, not in a config file.
#[derive(Clone)]
pub struct PusherAuth {
    key: String,
    secret: Vec<u8>,
}

impl PusherAuth {
    /// The app key (public, sent to the browser) and secret (never).
    pub fn new(key: impl Into<String>, secret: impl Into<String>) -> Self {
        Self { key: key.into(), secret: secret.into().into_bytes() }
    }

    /// The public app key.
    pub fn key(&self) -> &str {
        &self.key
    }

    /// The `auth` string for a subscription: `key:hmac`.
    ///
    /// The signed message is `socket_id:channel`, and for a presence channel
    /// the member JSON is appended — which is what stops a subscriber
    /// substituting someone else's roster entry after your application signed
    /// their own.
    pub fn sign(&self, socket_id: &str, channel: &Channel, member: Option<&str>) -> String {
        let message = match member {
            Some(member) => format!("{socket_id}:{}:{member}", channel.wire_name()),
            None => format!("{socket_id}:{}", channel.wire_name()),
        };

        let mut mac =
            HmacSha256::new_from_slice(&self.secret).expect("HMAC accepts a key of any length");
        mac.update(message.as_bytes());

        format!("{}:{}", self.key, hex(&mac.finalize().into_bytes()))
    }

    /// The body `/broadcasting/auth` should return.
    ///
    /// The member data is serialised **once** and both signed and returned, so
    /// the two can never disagree — signing a different rendering than the one
    /// sent is the classic way to make presence auth fail intermittently.
    pub fn auth_response(
        &self,
        socket_id: &str,
        channel: &Channel,
        member: Option<&Value>,
    ) -> Result<Value> {
        validate_socket_id(socket_id)?;

        match member {
            Some(member) => {
                let encoded = serde_json::to_string(member).map_err(|e| {
                    Error::internal(format!("presence channel data must serialise: {e}"))
                })?;

                Ok(json!({
                    "auth": self.sign(socket_id, channel, Some(&encoded)),
                    "channel_data": encoded,
                }))
            }
            None => Ok(json!({ "auth": self.sign(socket_id, channel, None) })),
        }
    }
}

impl std::fmt::Debug for PusherAuth {
    /// Prints the key and **not** the secret.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PusherAuth").field("key", &self.key).field("secret", &"<redacted>").finish()
    }
}

/// A socket id is `<digits>.<digits>`, and it goes into the signed message.
///
/// Checked rather than trusted: it arrives in the request body, and a client
/// that can put a `:` in it can shift the boundary between the socket and the
/// channel in the signed string — signing `1:private-a` when it claimed
/// `1:private-a:b`.
fn validate_socket_id(socket_id: &str) -> Result<()> {
    let shape = socket_id
        .split_once('.')
        .is_some_and(|(a, b)| !a.is_empty() && !b.is_empty() && [a, b].iter().all(is_digits));

    if !shape {
        return Err(Error::bad_request("that is not a socket id."));
    }
    Ok(())
}

fn is_digits(value: &&str) -> bool {
    value.bytes().all(|byte| byte.is_ascii_digit())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth() -> PusherAuth {
        PusherAuth::new("app-key", "app-secret")
    }

    #[test]
    fn a_signature_is_the_key_and_an_hmac() {
        let signed = auth().sign("1234.5678", &Channel::private("orders.7"), None);

        let (key, mac) = signed.split_once(':').expect("key:hmac");
        assert_eq!(key, "app-key");
        assert_eq!(mac.len(), 64, "sha256, hex");
    }

    #[test]
    fn the_signature_covers_the_channel_and_the_socket() {
        let auth = auth();
        let base = auth.sign("1234.5678", &Channel::private("orders.7"), None);

        assert_ne!(base, auth.sign("1234.5679", &Channel::private("orders.7"), None));
        assert_ne!(base, auth.sign("1234.5678", &Channel::private("orders.8"), None));
        // The prefix is part of the name, so a private and a presence channel
        // of the same name do not share a signature.
        assert_ne!(base, auth.sign("1234.5678", &Channel::presence("orders.7"), None));
    }

    #[test]
    fn presence_data_is_signed_and_returned_as_the_same_string() {
        let auth = auth();
        let member = json!({ "user_id": 7, "user_info": { "name": "Ada" } });

        let response = auth.auth_response("1234.5678", &Channel::presence("room.1"), Some(&member));
        let response = response.unwrap();

        let encoded = response["channel_data"].as_str().unwrap();
        let expected = auth.sign("1234.5678", &Channel::presence("room.1"), Some(encoded));

        assert_eq!(response["auth"], expected, "the signed and the sent must match exactly");
    }

    #[test]
    fn a_private_channel_gets_no_channel_data() {
        let response =
            auth().auth_response("1234.5678", &Channel::private("orders.7"), None).unwrap();

        assert!(response["auth"].is_string());
        assert!(response.get("channel_data").is_none());
    }

    #[test]
    fn a_socket_id_that_could_shift_the_signed_boundary_is_refused() {
        for hostile in ["1234.5678:private-other", "not-a-socket", "", ".", "1234.", "a.b"] {
            let err = auth()
                .auth_response(hostile, &Channel::private("orders.7"), None)
                .expect_err("should be refused");

            assert_eq!(err.status(), 400, "{hostile}");
        }
    }

    #[test]
    fn the_secret_is_not_in_the_debug_output() {
        let printed = format!("{:?}", auth());
        assert!(!printed.contains("app-secret"), "{printed}");
        assert!(printed.contains("app-key"));
    }
}
