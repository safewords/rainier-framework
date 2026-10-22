//! GitHub, and GitHub Enterprise Server.

use async_trait::async_trait;
use rainier_support::Result;
use serde_json::Value;

use crate::api::Api;
use crate::provider::{Endpoints, ProfileSource};
use crate::user::{text, AccessToken, Profile};

/// Who a GitHub token belongs to.
#[derive(Debug, Clone)]
pub struct GitHub {
    web: String,
    api: String,
}

impl GitHub {
    /// github.com.
    pub fn new() -> Self {
        Self { web: "https://github.com".into(), api: "https://api.github.com".into() }
    }

    /// A GitHub Enterprise Server at `base` (`https://github.example.com`),
    /// whose API lives under `/api/v3`.
    pub fn enterprise(base: impl Into<String>) -> Self {
        let web = base.into().trim_end_matches('/').to_string();
        Self { api: format!("{web}/api/v3"), web }
    }
}

impl Default for GitHub {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ProfileSource for GitHub {
    async fn endpoints(&self, _api: &Api) -> Result<Endpoints> {
        Ok(Endpoints {
            authorize: format!("{}/login/oauth/authorize", self.web),
            token: format!("{}/login/oauth/access_token", self.web),
        })
    }

    async fn profile(
        &self,
        api: &Api,
        token: &AccessToken,
        _nonce: Option<&str>,
    ) -> Result<Profile> {
        let user =
            super::fetch(api, "github", &format!("{}/user", self.api), &token.access_token).await?;

        // The verified primary address, which needs `user:email`. Without
        // that scope the call is refused, and the person simply has no
        // verified email as far as this sign-in knows — not an error.
        let emails =
            api.get(&format!("{}/user/emails", self.api), Some(&token.access_token)).await?;
        let verified = (200..300)
            .contains(&emails.status)
            .then(|| serde_json::from_slice::<Value>(&emails.body).ok())
            .flatten()
            .and_then(|list| {
                list.as_array()?
                    .iter()
                    .find(|e| {
                        e["primary"] == Value::Bool(true) && e["verified"] == Value::Bool(true)
                    })
                    .and_then(|e| text(e, "email"))
            });

        let (email, email_verified) = match verified {
            Some(email) => (Some(email), Some(true)),
            // The profile's public email is free text the person typed. It is
            // passed through, and never as verified.
            None => (text(&user, "email"), None),
        };

        Ok(Profile {
            id: text(&user, "id").unwrap_or_default(),
            email,
            email_verified,
            name: text(&user, "name"),
            nickname: text(&user, "login"),
            avatar: text(&user, "avatar_url"),
            raw: user,
        })
    }
}
