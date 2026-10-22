//! GitLab.com, or a self-managed GitLab.

use async_trait::async_trait;
use rainier_support::Result;
use serde_json::Value;

use crate::api::Api;
use crate::provider::{Endpoints, ProfileSource};
use crate::user::{text, AccessToken, Profile};

/// Who a GitLab token belongs to.
#[derive(Debug, Clone)]
pub struct GitLab {
    base: String,
}

impl GitLab {
    /// gitlab.com.
    pub fn new() -> Self {
        Self::at("https://gitlab.com")
    }

    /// A self-managed instance at `base`.
    pub fn at(base: impl Into<String>) -> Self {
        Self { base: base.into().trim_end_matches('/').to_string() }
    }
}

impl Default for GitLab {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ProfileSource for GitLab {
    async fn endpoints(&self, _api: &Api) -> Result<Endpoints> {
        Ok(Endpoints {
            authorize: format!("{}/oauth/authorize", self.base),
            token: format!("{}/oauth/token", self.base),
        })
    }

    async fn profile(
        &self,
        api: &Api,
        token: &AccessToken,
        _nonce: Option<&str>,
    ) -> Result<Profile> {
        let user =
            super::fetch(api, "gitlab", &format!("{}/api/v4/user", self.base), &token.access_token)
                .await?;

        // A confirmed account's primary email has been confirmed; an
        // unconfirmed one's has not been, and says nothing.
        let confirmed = !matches!(user.get("confirmed_at"), None | Some(Value::Null));

        Ok(Profile {
            id: text(&user, "id").unwrap_or_default(),
            email: text(&user, "email"),
            email_verified: confirmed.then_some(true),
            name: text(&user, "name"),
            nickname: text(&user, "username"),
            avatar: text(&user, "avatar_url"),
            raw: user,
        })
    }
}
