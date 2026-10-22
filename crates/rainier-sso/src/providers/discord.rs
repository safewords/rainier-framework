//! Discord.

use async_trait::async_trait;
use rainier_support::Result;

use crate::api::Api;
use crate::provider::{Endpoints, ProfileSource};
use crate::user::{flag, text, AccessToken, Profile};

/// Who a Discord token belongs to.
#[derive(Debug, Clone, Default)]
pub struct Discord;

impl Discord {
    /// discord.com.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ProfileSource for Discord {
    async fn endpoints(&self, _api: &Api) -> Result<Endpoints> {
        Ok(Endpoints {
            authorize: "https://discord.com/oauth2/authorize".into(),
            token: "https://discord.com/api/oauth2/token".into(),
        })
    }

    async fn profile(
        &self,
        api: &Api,
        token: &AccessToken,
        _nonce: Option<&str>,
    ) -> Result<Profile> {
        let user =
            super::fetch(api, "discord", "https://discord.com/api/users/@me", &token.access_token)
                .await?;

        let id = text(&user, "id").unwrap_or_default();
        let email = text(&user, "email");
        let avatar = text(&user, "avatar")
            .map(|hash| format!("https://cdn.discordapp.com/avatars/{id}/{hash}.png"));

        Ok(Profile {
            email_verified: email.as_ref().and_then(|_| flag(&user, "verified")),
            email,
            name: text(&user, "global_name").or_else(|| text(&user, "username")),
            nickname: text(&user, "username"),
            avatar,
            id,
            raw: user,
        })
    }
}
