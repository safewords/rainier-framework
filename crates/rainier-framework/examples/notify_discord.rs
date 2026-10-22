//! Notifications posted to Discord, offline.
//!
//! What it shows:
//!
//! 1. a `Notifiable` whose `route_for("discord")` is its webhook URL;
//! 2. a notification whose `to_data` carries a [`DiscordMessage`] with an
//!    embed, sent through a webhook-mode [`DiscordChannel`] — and the exact
//!    JSON that went out, mentions switched off;
//! 3. the same notification through a **bot**-mode channel, where the route is
//!    a channel id and the token is the application's;
//! 4. a `429`: the receipt records the failure, and the error says how long
//!    Discord asked to wait — what a queued job backs off by.
//!
//! ```sh
//! cargo run -p rainier-framework --example notify_discord --features notify-discord
//! ```
//!
//! In an application the fake becomes the real transport:
//!
//! ```ignore
//! use rainier_framework::http_client::ReqwestTransport;
//!
//! let http: Arc<dyn Transport> = Arc::new(ReqwestTransport::new());
//! let notifier = Notifier::new().with(DiscordChannel::webhooks(http));
//! // or: DiscordChannel::bot(http, env("DISCORD_BOT_TOKEN"))
//! ```

use std::sync::Arc;

use rainier_framework::http_client::{FakeTransport, Transport};
use rainier_framework::notify::{
    Channels, DiscordChannel, DiscordEmbed, DiscordMessage, Notifiable, Notification, Notifier,
    Sent,
};
use serde_json::Value;

/// A club that pasted a webhook into its settings — or, in bot mode, picked a
/// channel the application's bot was invited to.
struct Club {
    id: u64,
    discord: String,
}

impl Notifiable for Club {
    fn notifiable_id(&self) -> String {
        self.id.to_string()
    }
    fn notifiable_type(&self) -> &'static str {
        "Club"
    }
    fn route_for(&self, channel: &str) -> Option<String> {
        (channel == "discord").then(|| self.discord.clone())
    }
}

/// A member's car finished charging.
struct ChargeFinished {
    vehicle: &'static str,
    added_kwh: f64,
    cost: &'static str,
}

impl Notification<Club> for ChargeFinished {
    fn notification_name(&self) -> &'static str {
        "club.charge-finished"
    }

    fn via(&self, _: &Club) -> Channels {
        Channels::new().with::<DiscordChannel>()
    }

    // The fallback a channel without structured support would use.
    fn to_text(&self, _: &Club) -> Option<String> {
        Some(format!("{} finished charging", self.vehicle))
    }

    fn to_data(&self, _: &Club) -> Option<Value> {
        Some(
            DiscordMessage::new()
                // A name a user chose, interpolated: mentions stay off, so an
                // `@everyone` in it pings nobody.
                .content(format!("@everyone {} finished charging", self.vehicle))
                .username("Charge bot")
                .embed(
                    DiscordEmbed::new()
                        .title(self.vehicle)
                        .color(0x2E7D32)
                        .field("Added", format!("{:.1} kWh", self.added_kwh), true)
                        .field("Cost", self.cost, true),
                )
                .into_data(),
        )
    }
}

#[tokio::main]
async fn main() -> rainier_framework::support::Result<()> {
    let charge = ChargeFinished { vehicle: "Model 3 \"Blue\"", added_kwh: 42.0, cost: "$6.10" };

    // --- 1 & 2: webhook mode -------------------------------------------------
    let fake = Arc::new(FakeTransport::new());
    let notifier =
        Notifier::new().with(DiscordChannel::webhooks(Arc::clone(&fake) as Arc<dyn Transport>));
    let club = Club { id: 7, discord: "https://discord.com/api/webhooks/123/token".into() };

    let receipt = notifier.send(&club, &charge).await?;
    println!("1. webhook mode delivered on: {:?}", receipt.delivered());
    let sent = &fake.recorded()[0];
    println!("   POST {}", sent.url().replace("/token", "/…"));
    println!("{}", serde_json::to_string_pretty(&sent.json().unwrap()).unwrap());

    // --- 3: bot mode ---------------------------------------------------------
    let fake = Arc::new(FakeTransport::new());
    let notifier = Notifier::new()
        .with(DiscordChannel::bot(Arc::clone(&fake) as Arc<dyn Transport>, "bot-token"));
    let club = Club { id: 7, discord: "1029384756102938475".into() };

    notifier.send(&club, &charge).await?;
    let sent = &fake.recorded()[0];
    println!("3. bot mode: POST {}", sent.url());
    println!("   authorization: Bot …  (present: {})", sent.header("authorization").is_some());

    // --- 4: a rate limit -----------------------------------------------------
    let fake = Arc::new(FakeTransport::new());
    fake.responding(429, r#"{"message":"You are being rate limited.","retry_after":2.5}"#);
    let notifier = Notifier::new().with(DiscordChannel::webhooks(fake as Arc<dyn Transport>));
    let club = Club { id: 7, discord: "https://discord.com/api/webhooks/123/token".into() };

    let receipt = notifier.send(&club, &charge).await?;
    for (channel, outcome) in &receipt.outcomes {
        if let Sent::Failed(error) = outcome {
            println!(
                "4. {channel} failed ({:?}); retry after {}s",
                error.kind(),
                error.details().and_then(|d| d.get("retry_after")).cloned().unwrap_or_default()
            );
        }
    }

    Ok(())
}
