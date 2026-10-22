//! Discord — [`DiscordChannel`], behind the `discord` feature.
//!
//! Two ways in, because Discord has two and they suit different recipients:
//!
//! | | the route is | authenticates with |
//! |---|---|---|
//! | [`DiscordChannel::webhooks`] | the recipient's **webhook URL** | the URL itself |
//! | [`DiscordChannel::bot`] | a **channel id** | `Authorization: Bot …` |
//!
//! A webhook suits a recipient who pasted one into their settings — a team, a
//! server, a club. A bot suits an application posting into channels it was
//! invited to, where the recipient names a channel rather than holding a
//! secret.
//!
//! ```ignore
//! impl Notifiable for Team {
//!     fn route_for(&self, channel: &str) -> Option<String> {
//!         match channel {
//!             "discord" => self.discord_webhook.clone(),
//!             _ => None,
//!         }
//!     }
//!     // …
//! }
//!
//! impl Notification<Team> for ChargeFinished {
//!     fn via(&self, _: &Team) -> Channels { Channels::new().with::<DiscordChannel>() }
//!
//!     fn to_data(&self, _: &Team) -> Option<Value> {
//!         Some(DiscordMessage::new()
//!             .content("Charging finished")
//!             .embed(DiscordEmbed::new().title("Model 3").field("Added", "42 kWh", true))
//!             .into_data())
//!     }
//! }
//! ```
//!
//! # What gets posted
//!
//! A notification's [structured form](crate::Notification::to_data) under a
//! `discord` key, which is what [`DiscordMessage::into_data`] produces — so the
//! same `to_data` can carry a database channel's row beside it. Failing that,
//! the [text form](crate::Notification::to_text) as the message content.
//!
//! # Mentions are off unless a message turns them on
//!
//! A message gets `allowed_mentions: {"parse": []}` unless it set its own. A
//! notification that interpolates a vehicle name or a user's display name into
//! its content is otherwise one `@everyone` away from pinging a whole server
//! on somebody else's behalf.
//!
//! # A webhook URL is somebody's input
//!
//! A webhook route usually came from a settings form, so the channel only posts
//! to `https://discord.com/api/webhooks/…` (and the `discordapp.com`,
//! `canary.` and `ptb.` spellings). Anything else is refused before a request
//! is made — a notification channel that will POST to any URL a user typed is
//! a server-side request forgery primitive with extra steps.
//!
//! # Rate limits
//!
//! A `429` is an [`Error::service_unavailable`] — the retryable kind — with
//! the `retry_after` Discord asked for, in seconds, in its
//! [details](rainier_support::Error::details). A queued job sending the
//! notification can back off by exactly that much.

use std::sync::Arc;
use std::time::Duration;

use rainier_http_client::transport::{OutboundRequest, RawResponse};
use rainier_http_client::Transport;
use rainier_support::{BoxFuture, Error, Result};
use serde_json::{json, Map, Value};

use crate::channel::Channel;
use crate::notification::Delivery;

/// Who the channel authenticates as.
enum Mode {
    /// The route is a webhook URL.
    Webhooks,
    /// The route is a channel id; the bot token is ours.
    Bot { token: String },
}

/// Posts notifications to Discord.
pub struct DiscordChannel {
    http: Arc<dyn Transport>,
    mode: Mode,
    api: String,
    timeout: Duration,
}

impl DiscordChannel {
    /// Post to each recipient's webhook — `route_for("discord")` is the URL.
    pub fn webhooks(http: Arc<dyn Transport>) -> Self {
        Self {
            http,
            mode: Mode::Webhooks,
            api: "https://discord.com/api/v10".into(),
            timeout: Duration::from_secs(15),
        }
    }

    /// Post as a bot — `route_for("discord")` is a channel id.
    pub fn bot(http: Arc<dyn Transport>, token: impl Into<String>) -> Self {
        Self { mode: Mode::Bot { token: token.into() }, ..Self::webhooks(http) }
    }

    /// The API base for bot mode — a proxy, a test double.
    pub fn with_api_base(mut self, base: impl Into<String>) -> Self {
        self.api = base.into().trim_end_matches('/').to_string();
        self
    }

    /// The wall clock on one post. Fifteen seconds by default.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    fn request(&self, route: &str, body: Vec<u8>) -> Result<OutboundRequest> {
        let mut headers = std::collections::BTreeMap::new();
        headers.insert("content-type".to_string(), "application/json".to_string());

        let url = match &self.mode {
            Mode::Webhooks => {
                if !is_discord_webhook(route) {
                    return Err(Error::internal(
                        "a discord route must be a https://discord.com/api/webhooks/… URL; \
                         refusing to post anywhere else",
                    ));
                }
                // `wait=true` makes Discord validate the message and answer
                // with it, rather than 204 for a message it later drops.
                let separator = if route.contains('?') { '&' } else { '?' };
                format!("{route}{separator}wait=true")
            }
            Mode::Bot { token } => {
                if route.is_empty() || !route.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(Error::internal(format!(
                        "a discord bot route is a channel id (a snowflake), not `{route}`"
                    )));
                }
                headers.insert("authorization".to_string(), format!("Bot {token}"));
                format!("{}/channels/{route}/messages", self.api)
            }
        };

        Ok(OutboundRequest {
            method: "POST".into(),
            url,
            headers,
            body: Some(body),
            timeout: Some(self.timeout),
        })
    }
}

impl Channel for DiscordChannel {
    fn name(&self) -> &'static str {
        "discord"
    }

    fn send<'a>(&'a self, delivery: &'a Delivery) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let Some(route) = delivery.route.as_deref() else {
                return Err(delivery.no_route("discord"));
            };
            let Some(message) = message_for(delivery) else {
                return Err(delivery.nothing_to_send("discord", "`discord` data or text"));
            };

            let request = self.request(route, serde_json::to_vec(&message)?)?;
            verdict(self.http.send(request).await?)
        })
    }
}

impl std::fmt::Debug for DiscordChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The bot token stays out of Debug for the same reason it stays out of
        // logs.
        let mode = match self.mode {
            Mode::Webhooks => "webhooks",
            Mode::Bot { .. } => "bot",
        };
        f.debug_struct("DiscordChannel").field("mode", &mode).field("api", &self.api).finish()
    }
}

/// The message body for this delivery, with mentions off unless it said so.
fn message_for(delivery: &Delivery) -> Option<Value> {
    let mut message = match delivery.data().and_then(|data| data.get("discord")) {
        Some(Value::Object(object)) => object.clone(),
        _ => {
            let mut object = Map::new();
            object.insert("content".into(), delivery.text()?.into());
            object
        }
    };
    message.entry("allowed_mentions").or_insert_with(|| json!({ "parse": [] }));
    Some(Value::Object(message))
}

/// Whether `url` is a Discord webhook — and nothing that merely contains one.
fn is_discord_webhook(url: &str) -> bool {
    const HOSTS: [&str; 5] = [
        "discord.com",
        "discordapp.com",
        "canary.discord.com",
        "ptb.discord.com",
        "canary.discordapp.com",
    ];
    let Some(rest) = url.strip_prefix("https://") else { return false };
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    HOSTS.contains(&host) && path.starts_with("api/") && path.contains("webhooks/")
}

/// A 2xx is a post; a 429 is a back-off; anything else is Discord's refusal.
fn verdict(response: RawResponse) -> Result<()> {
    if (200..300).contains(&response.status) {
        return Ok(());
    }

    let body: Value = serde_json::from_slice(&response.body).unwrap_or(Value::Null);
    let said = body
        .get("message")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| String::from_utf8_lossy(&response.body).chars().take(300).collect());

    if response.status == 429 {
        // The body's `retry_after` is seconds with a fraction; the header is
        // whole seconds. Prefer the precise one.
        let retry_after = body
            .get("retry_after")
            .and_then(Value::as_f64)
            .or_else(|| response.headers.get("retry-after").and_then(|v| v.parse().ok()))
            .unwrap_or(1.0);
        return Err(Error::service_unavailable(format!(
            "discord rate-limited the post; retry after {retry_after}s"
        ))
        .with_details(json!({ "retry_after": retry_after })));
    }

    let message = format!("discord answered {}: {said}", response.status);
    if response.status >= 500 {
        Err(Error::service_unavailable(message))
    } else {
        Err(Error::internal(message))
    }
}

/// A Discord message — content, embeds, a username override.
#[derive(Debug, Clone, Default)]
pub struct DiscordMessage {
    body: Map<String, Value>,
    embeds: Vec<Value>,
}

impl DiscordMessage {
    /// An empty message.
    pub fn new() -> Self {
        Self::default()
    }

    /// The text, up to 2000 characters.
    pub fn content(mut self, content: impl Into<String>) -> Self {
        self.body.insert("content".into(), content.into().into());
        self
    }

    /// Add an embed. Discord takes up to ten.
    pub fn embed(mut self, embed: DiscordEmbed) -> Self {
        self.embeds.push(Value::Object(embed.0));
        self
    }

    /// Post under another name (webhooks only).
    pub fn username(mut self, username: impl Into<String>) -> Self {
        self.body.insert("username".into(), username.into().into());
        self
    }

    /// Post with another avatar (webhooks only).
    pub fn avatar_url(mut self, url: impl Into<String>) -> Self {
        self.body.insert("avatar_url".into(), url.into().into());
        self
    }

    /// Let these user ids be pinged. Everything else stays silent.
    pub fn allow_user_mentions<I, S>(mut self, ids: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let ids: Vec<Value> = ids.into_iter().map(|id| Value::String(id.into())).collect();
        self.body.insert("allowed_mentions".into(), json!({ "parse": [], "users": ids }));
        self
    }

    /// The message as Discord's JSON.
    pub fn to_value(&self) -> Value {
        let mut body = self.body.clone();
        if !self.embeds.is_empty() {
            body.insert("embeds".into(), Value::Array(self.embeds.clone()));
        }
        Value::Object(body)
    }

    /// The message as a notification's `to_data`: `{"discord": …}`.
    pub fn into_data(self) -> Value {
        json!({ "discord": self.to_value() })
    }
}

/// One embed: a title, a description, a colour, fields.
#[derive(Debug, Clone, Default)]
pub struct DiscordEmbed(Map<String, Value>);

impl DiscordEmbed {
    /// An empty embed.
    pub fn new() -> Self {
        Self::default()
    }

    /// The title.
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.0.insert("title".into(), title.into().into());
        self
    }

    /// Where the title links to.
    pub fn url(mut self, url: impl Into<String>) -> Self {
        self.0.insert("url".into(), url.into().into());
        self
    }

    /// The body text.
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.0.insert("description".into(), description.into().into());
        self
    }

    /// The side colour, as `0xRRGGBB`.
    pub fn color(mut self, rgb: u32) -> Self {
        self.0.insert("color".into(), rgb.into());
        self
    }

    /// An ISO-8601 timestamp shown in the footer.
    pub fn timestamp(mut self, iso8601: impl Into<String>) -> Self {
        self.0.insert("timestamp".into(), iso8601.into().into());
        self
    }

    /// The footer text.
    pub fn footer(mut self, text: impl Into<String>) -> Self {
        self.0.insert("footer".into(), json!({ "text": text.into() }));
        self
    }

    /// A large image.
    pub fn image(mut self, url: impl Into<String>) -> Self {
        self.0.insert("image".into(), json!({ "url": url.into() }));
        self
    }

    /// A small image in the corner.
    pub fn thumbnail(mut self, url: impl Into<String>) -> Self {
        self.0.insert("thumbnail".into(), json!({ "url": url.into() }));
        self
    }

    /// A name/value field; `inline` lays fields side by side.
    pub fn field(
        mut self,
        name: impl Into<String>,
        value: impl Into<String>,
        inline: bool,
    ) -> Self {
        let fields = self.0.entry("fields").or_insert_with(|| Value::Array(Vec::new()));
        if let Value::Array(fields) = fields {
            fields.push(json!({ "name": name.into(), "value": value.into(), "inline": inline }));
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notification::{Channels, Notifiable, Notification};
    use rainier_http_client::FakeTransport;

    const HOOK: &str = "https://discord.com/api/webhooks/123/secret-token";

    struct Team {
        route: Option<String>,
    }

    impl Notifiable for Team {
        fn notifiable_id(&self) -> String {
            "7".into()
        }
        fn notifiable_type(&self) -> &'static str {
            "Team"
        }
        fn route_for(&self, channel: &str) -> Option<String> {
            (channel == "discord").then(|| self.route.clone()).flatten()
        }
    }

    struct Text;
    impl Notification<Team> for Text {
        fn notification_name(&self) -> &'static str {
            "team.text"
        }
        fn via(&self, _: &Team) -> Channels {
            Channels::new().with::<DiscordChannel>()
        }
        fn to_text(&self, _: &Team) -> Option<String> {
            Some("@everyone charging finished".into())
        }
    }

    struct Rich;
    impl Notification<Team> for Rich {
        fn notification_name(&self) -> &'static str {
            "team.rich"
        }
        fn via(&self, _: &Team) -> Channels {
            Channels::new().with::<DiscordChannel>()
        }
        fn to_text(&self, _: &Team) -> Option<String> {
            Some("ignored when data has a discord key".into())
        }
        fn to_data(&self, _: &Team) -> Option<Value> {
            Some(
                DiscordMessage::new()
                    .content("Charging finished")
                    .username("Scope")
                    .embed(
                        DiscordEmbed::new()
                            .title("Model 3")
                            .color(0xC75232)
                            .field("Added", "42 kWh", true)
                            .field("Cost", "$6.10", true),
                    )
                    .into_data(),
            )
        }
    }

    fn delivery<T: Notification<Team>>(notification: &T, route: Option<&str>) -> Delivery {
        let team = Team { route: route.map(str::to_string) };
        Delivery::render(notification, &team).routed_for("discord", &team)
    }

    #[tokio::test]
    async fn text_posts_to_the_webhook_with_mentions_off() {
        let fake = Arc::new(FakeTransport::new());
        let channel = DiscordChannel::webhooks(Arc::clone(&fake) as _);

        channel.send(&delivery(&Text, Some(HOOK))).await.unwrap();

        let sent = &fake.recorded()[0];
        assert_eq!(sent.method(), "POST");
        assert_eq!(sent.url(), format!("{HOOK}?wait=true"));
        assert_eq!(sent.header("authorization"), None, "a webhook needs no token");
        let body = sent.json().unwrap();
        assert_eq!(body["content"], "@everyone charging finished");
        assert_eq!(body["allowed_mentions"], json!({ "parse": [] }));
    }

    #[tokio::test]
    async fn structured_data_under_discord_becomes_the_message() {
        let fake = Arc::new(FakeTransport::new());
        let channel = DiscordChannel::webhooks(Arc::clone(&fake) as _);

        channel.send(&delivery(&Rich, Some(HOOK))).await.unwrap();

        let body = fake.recorded()[0].json().unwrap();
        assert_eq!(body["content"], "Charging finished");
        assert_eq!(body["username"], "Scope");
        assert_eq!(body["embeds"][0]["title"], "Model 3");
        assert_eq!(body["embeds"][0]["color"], 0xC75232);
        assert_eq!(body["embeds"][0]["fields"][1]["name"], "Cost");
        assert_eq!(body["embeds"][0]["fields"][1]["inline"], true);
        assert_eq!(body["allowed_mentions"], json!({ "parse": [] }));
    }

    #[tokio::test]
    async fn a_bot_posts_to_a_channel_id_with_its_token() {
        let fake = Arc::new(FakeTransport::new());
        let channel = DiscordChannel::bot(Arc::clone(&fake) as _, "bot-secret");

        channel.send(&delivery(&Text, Some("1029384756"))).await.unwrap();

        let sent = &fake.recorded()[0];
        assert_eq!(sent.url(), "https://discord.com/api/v10/channels/1029384756/messages");
        assert_eq!(sent.header("authorization"), Some("Bot bot-secret"));
    }

    #[tokio::test]
    async fn a_route_that_is_not_a_discord_webhook_is_refused_before_any_request() {
        let fake = Arc::new(FakeTransport::new());
        let channel = DiscordChannel::webhooks(Arc::clone(&fake) as _);

        for hostile in [
            "http://discord.com/api/webhooks/1/x",
            "https://evil.example/api/webhooks/1/x",
            "https://discord.com.evil.example/api/webhooks/1/x",
            "https://169.254.169.254/latest/meta-data",
            "https://discord.com/users/1",
        ] {
            assert!(channel.send(&delivery(&Text, Some(hostile))).await.is_err(), "{hostile}");
        }
        let bot = DiscordChannel::bot(Arc::clone(&fake) as _, "t");
        assert!(bot.send(&delivery(&Text, Some("../../guilds/1"))).await.is_err());

        fake.assert_nothing_sent();
    }

    #[tokio::test]
    async fn no_route_and_nothing_to_send_are_errors() {
        let fake = Arc::new(FakeTransport::new());
        let channel = DiscordChannel::webhooks(Arc::clone(&fake) as _);

        let err = channel.send(&delivery(&Text, None)).await.unwrap_err();
        assert!(err.message().contains("no `discord` address"), "{}", err.message());

        struct Silent;
        impl Notification<Team> for Silent {
            fn notification_name(&self) -> &'static str {
                "silent"
            }
            fn via(&self, _: &Team) -> Channels {
                Channels::new()
            }
        }
        assert!(channel.send(&delivery(&Silent, Some(HOOK))).await.is_err());
        fake.assert_nothing_sent();
    }

    #[tokio::test]
    async fn a_rate_limit_is_retryable_and_says_how_long() {
        let fake = Arc::new(FakeTransport::new());
        fake.responding(
            429,
            r#"{"message":"You are being rate limited.","retry_after":1.25,"global":false}"#,
        );
        let channel = DiscordChannel::webhooks(Arc::clone(&fake) as _);

        let err = channel.send(&delivery(&Text, Some(HOOK))).await.unwrap_err();
        assert_eq!(err.kind(), rainier_support::ErrorKind::ServiceUnavailable);
        assert_eq!(err.details().unwrap()["retry_after"], 1.25);
    }

    #[tokio::test]
    async fn a_rejection_quotes_discord_and_is_not_retryable() {
        let fake = Arc::new(FakeTransport::new());
        fake.responding(400, r#"{"message":"Cannot send an empty message","code":50006}"#);
        let channel = DiscordChannel::webhooks(Arc::clone(&fake) as _);

        let err = channel.send(&delivery(&Text, Some(HOOK))).await.unwrap_err();
        assert!(err.message().contains("Cannot send an empty message"), "{}", err.message());
        assert_ne!(err.kind(), rainier_support::ErrorKind::ServiceUnavailable);
    }

    #[test]
    fn a_message_can_allow_specific_mentions() {
        let message = DiscordMessage::new().content("hi <@42>").allow_user_mentions(["42"]);
        assert_eq!(message.to_value()["allowed_mentions"], json!({ "parse": [], "users": ["42"] }));
    }

    #[test]
    fn the_bot_token_stays_out_of_debug() {
        let channel = DiscordChannel::bot(Arc::new(FakeTransport::new()), "extremely-secret");
        assert!(!format!("{channel:?}").contains("extremely-secret"));
    }
}
