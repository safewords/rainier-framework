//! X (formerly Twitter), over OAuth 2.0 with PKCE.
//!
//! # Email is a separate permission
//!
//! X answers `GET /2/users/me` with an id, a name and a handle. An address is
//! there only when the app has been granted email access in the developer
//! portal **and** the token carries the `users.email` scope — and then it is
//! the `confirmed_email` field, which X only fills with an address the account
//! holder has confirmed. So an address from X is reported verified, and its
//! absence is the ordinary case rather than an error.
//!
//! The field is only asked for when the token was granted the scope: asking
//! for a field the token may not read is a request X can refuse outright,
//! which would turn "no email" into "no sign-in".

use async_trait::async_trait;
use rainier_support::Result;

use crate::api::Api;
use crate::provider::{Endpoints, ProfileSource};
use crate::user::{text, AccessToken, Profile};

/// The fields every profile request asks for.
const FIELDS: &str = "id,name,username,profile_image_url";

/// Who an X token belongs to.
#[derive(Debug, Clone, Default)]
pub struct X;

impl X {
    /// x.com.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ProfileSource for X {
    async fn endpoints(&self, _api: &Api) -> Result<Endpoints> {
        Ok(Endpoints {
            authorize: "https://x.com/i/oauth2/authorize".into(),
            token: "https://api.x.com/2/oauth2/token".into(),
        })
    }

    async fn profile(
        &self,
        api: &Api,
        token: &AccessToken,
        _nonce: Option<&str>,
    ) -> Result<Profile> {
        let fields = if token.has_scope("users.email") {
            format!("{FIELDS},confirmed_email")
        } else {
            FIELDS.to_string()
        };
        let url = format!("https://api.x.com/2/users/me?user.fields={fields}");

        let answer = super::fetch(api, "x", &url, &token.access_token).await?;
        // Everything X says about a user is under `data`.
        let user = answer.get("data").cloned().unwrap_or_default();

        let email = text(&user, "confirmed_email");
        Ok(Profile {
            id: text(&user, "id").unwrap_or_default(),
            email_verified: email.as_ref().map(|_| true),
            email,
            name: text(&user, "name"),
            nickname: text(&user, "username"),
            // `_normal` is a 48px thumbnail; the same path without the suffix
            // is the original upload.
            avatar: text(&user, "profile_image_url").map(|url| url.replace("_normal.", ".")),
            raw: answer,
        })
    }
}
