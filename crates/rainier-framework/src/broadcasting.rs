//! The half of broadcasting that needs HTTP: the authorisation endpoint, the
//! socket header, and the notification channel.
//!
//! `rainier-broadcast` depends on support and nothing else, so it can be used
//! from a worker with no router. What lives here is what genuinely needs a
//! request: `/broadcasting/auth`, which is how a browser proves to your
//! WebSocket server that it may subscribe to a private channel.
//!
//! ```text
//! browser ──subscribe private-orders.7──▶ soketi
//!    │                                      │
//!    └──POST /broadcasting/auth─────────────┘  "prove it"
//!          ▲
//!          └── this module: is this user allowed, and here is the signature
//! ```

use std::sync::Arc;

use rainier_auth::AuthenticatedUser;
use rainier_broadcast::{Broadcasting, Channel, ChannelRegistry};
use rainier_container::Application;
use rainier_http::{Request, Response, StatusCode};
use rainier_notify::{Channel as NotificationChannel, Delivery};
use rainier_routing::Req;
use rainier_support::{BoxFuture, Error, Result};

/// The header a Pusher client sends its socket id in.
///
/// It is what makes `to_others` possible: the
/// browser tells you which socket it is, so you can leave it out.
pub const SOCKET_ID_HEADER: &str = "X-Socket-ID";

/// The socket id this request came from, if the client sent one.
///
/// Pass it to [`Broadcasting::event_except`] to skip the browser that caused
/// the change — it has already updated itself, and echoing the change back
/// makes its own edit flicker.
pub fn socket_id(request: &Request) -> Option<String> {
    request.header(SOCKET_ID_HEADER).map(str::to_string)
}

/// `POST /broadcasting/auth` — decide whether this user may subscribe.
///
/// One handler you attach yourself:
///
/// ```ignore
/// router
///     .post("/broadcasting/auth", broadcasting::authorize::<User>)
///     .name("broadcasting.auth")
///     .middleware(kernel::auth("api"));
/// ```
///
/// **Put it behind the auth middleware.** It reads the authenticated user from
/// the request, and without a guard in front there is none — every private
/// channel then answers `401`, which looks like a broken client rather than a
/// missing route.
///
/// The request carries `socket_id` and `channel_name`, form-encoded or JSON,
/// which is what every Pusher client sends.
pub async fn authorize<U>(request: Req) -> Result<Response>
where
    U: Send + Sync + 'static,
{
    let socket = required(&request, "socket_id")?;
    let wire_name = required(&request, "channel_name")?;
    let channel = Channel::from_wire_name(&wire_name);

    let app = rainier_container::facade_application();
    let registry = app.resolve::<ChannelRegistry<U>>()?;
    let broadcasting = app.resolve::<Broadcasting>()?;

    let Some(user) = request.extension::<AuthenticatedUser<U>>() else {
        return Err(Error::unauthenticated("Unauthenticated."));
    };

    let access = registry.authorize(user.get(), &channel).await?;
    if !access.is_allowed() {
        // A 403 rather than a 404: the client already knows the channel name —
        // it just sent it — so there is nothing left to conceal, and a Pusher
        // client shows "denied" for this and hangs on a 404.
        return Ok(Response::new(StatusCode::FORBIDDEN));
    }

    // Presence channels carry a roster entry; private ones do not. The driver
    // decides what proof looks like — an HMAC for a Pusher-protocol server,
    // nothing for a relay that trusts the application.
    let body = broadcasting.driver().auth_response(&socket, &channel, access.member())?;

    Ok(Response::json(&body))
}

/// A field the client must send, from the body or the query.
fn required(request: &Request, name: &str) -> Result<String> {
    request
        .input(name)
        .map(|value| value.to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::bad_request(format!("`{name}` is required.")))
}

/// Sends notifications to a private channel for the recipient.
///
/// The channel name is
/// `notifications.{type}.{id}` — for example `private-notifications.User.7` on
/// the wire — so one subscription receives everything sent to that recipient,
/// whatever the notification turns out to be.
///
/// Uses [`to_data`](rainier_notify::Notification::to_data), falling back to
/// [`to_text`](rainier_notify::Notification::to_text) wrapped as
/// `{"message": …}`.
///
/// # It is not a delivery
///
/// A broadcast reaches whoever is connected at that instant and nobody else.
/// Pair it with the [database channel](crate::notifications::DatabaseChannel),
/// which is what
/// makes the notification survive a reload — this one only makes the bell
/// menu move without one.
pub struct BroadcastChannel {
    broadcasting: Arc<Broadcasting>,
}

impl BroadcastChannel {
    /// A channel publishing through `broadcasting`.
    pub fn new(broadcasting: Arc<Broadcasting>) -> Self {
        Self { broadcasting }
    }

    /// The channel a recipient's notifications are published on.
    ///
    /// The pattern to authorise is `notifications.{type}.{id}`.
    pub fn channel_for(notifiable_type: &str, notifiable_id: &str) -> Channel {
        Channel::private(format!("notifications.{notifiable_type}.{notifiable_id}"))
    }
}

impl NotificationChannel for BroadcastChannel {
    fn name(&self) -> &'static str {
        "broadcast"
    }

    fn send<'a>(&'a self, delivery: &'a Delivery) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let Some(payload) = delivery.data_or_text() else {
                return Err(delivery.nothing_to_send("broadcast", "data or text"));
            };

            let channel = Self::channel_for(delivery.recipient_type, &delivery.recipient_id);

            self.broadcasting.to(vec![channel], delivery.notification, payload).await
        })
    }
}

/// Register the notification channel's authoriser.
///
/// Every recipient may listen to their own notifications and nobody else's.
/// Without this the channel publishes and no browser is ever allowed to
/// subscribe, which looks exactly like a broken WebSocket server.
///
/// `identify` says which id a user is, so the comparison is against the same
/// string [`Notifiable::notifiable_id`](rainier_notify::Notifiable) produced.
pub fn authorize_notifications<U, F>(
    registry: &mut ChannelRegistry<U>,
    notifiable_type: &'static str,
    identify: F,
) where
    U: Send + Sync + 'static,
    F: Fn(&U) -> String + Send + Sync + 'static,
{
    registry.channel("notifications.{type}.{id}", move |user: &U, params| {
        let allowed = params.get("type") == Some(notifiable_type)
            && params.get("id") == Some(identify(user).as_str());

        Box::pin(async move { Ok(rainier_broadcast::ChannelAccess::allowed_if(allowed)) })
    });
}

/// Bind broadcasting into `app` with the log driver and an empty channel table.
///
/// What [`Rainier`](crate::Rainier) does at boot unless the application says
/// otherwise: nothing reaches a browser, and every private channel is denied.
/// Both are the safe end of their respective mistakes.
pub fn bind_defaults<U: Send + Sync + 'static>(app: &Application) {
    app.instance(Broadcasting::log());
    app.instance(ChannelRegistry::<U>::new());
}

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_broadcast::{ChannelAccess, MemoryBroadcaster};
    use rainier_notify::{Channels, Notifiable, Notification, Notifier};

    struct User(u64);

    impl Notifiable for User {
        fn notifiable_id(&self) -> String {
            self.0.to_string()
        }
        fn notifiable_type(&self) -> &'static str {
            "User"
        }
        fn route_for(&self, _: &str) -> Option<String> {
            None
        }
    }

    struct Mentioned;

    impl Notification<User> for Mentioned {
        fn notification_name(&self) -> &'static str {
            "post.mentioned"
        }
        fn via(&self, _: &User) -> Channels {
            Channels::new().with::<BroadcastChannel>()
        }
        fn to_text(&self, _: &User) -> Option<String> {
            Some("someone mentioned you".into())
        }
    }

    #[tokio::test]
    async fn a_notification_broadcasts_to_the_recipients_own_channel() {
        let driver = Arc::new(MemoryBroadcaster::new());
        let broadcasting = Arc::new(Broadcasting::new(driver.clone()));
        let notifier = Notifier::new().with(BroadcastChannel::new(broadcasting));

        notifier.send(&User(7), &Mentioned).await.unwrap();

        driver.assert_broadcast("post.mentioned", "private-notifications.User.7");
        assert_eq!(driver.sent()[0].payload["message"], "someone mentioned you");
    }

    #[tokio::test]
    async fn a_recipient_may_listen_to_their_own_and_nobody_elses() {
        let mut registry = ChannelRegistry::<User>::new();
        authorize_notifications(&mut registry, "User", |user| user.notifiable_id());

        let mine = BroadcastChannel::channel_for("User", "7");
        assert_eq!(registry.authorize(&User(7), &mine).await.unwrap(), ChannelAccess::Allowed);
        assert_eq!(registry.authorize(&User(8), &mine).await.unwrap(), ChannelAccess::Denied);
    }

    #[tokio::test]
    async fn another_notifiable_type_with_the_same_id_is_denied() {
        // `Team.7` is not `User.7`, and an authoriser that only compared ids
        // would let a user read their team's notifications.
        let mut registry = ChannelRegistry::<User>::new();
        authorize_notifications(&mut registry, "User", |user| user.notifiable_id());

        let theirs = BroadcastChannel::channel_for("Team", "7");
        assert_eq!(registry.authorize(&User(7), &theirs).await.unwrap(), ChannelAccess::Denied);
    }
}
