//! Who made a request, as the application describes them —
//! [`Lookout::describe_user`](crate::Lookout::describe_user).
//!
//! The batch already carries the user's id when
//! [`Capture::user_id`](crate::config::Capture::user_id) is on. A description
//! is the rest of what a person reading the dashboard needs to recognise
//! someone: a name, an address, an avatar, a plan — whatever the application
//! chooses to give. Lookout never looks a user up itself: the application
//! knows where its users are and what is fit to show, and usually already has
//! the user loaded by the time it authenticates the request.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// A description of the user a request was made by.
///
/// ```
/// use rainier_lookout::LookoutUser;
///
/// let user = LookoutUser::new("42")
///     .name("Ada Lovelace")
///     .email("ada@example.com")
///     .with("plan", "pro");
/// assert_eq!(user.id, "42");
/// ```
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct LookoutUser {
    /// The id — the same one the batch's `user_id` carries.
    pub id: String,
    /// What to call them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Their address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// A picture of them, by URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar_url: Option<String>,
    /// Anything else worth seeing: a username, a plan, a role, an account.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

impl LookoutUser {
    /// A user known only by `id`.
    pub fn new(id: impl Into<String>) -> Self {
        Self { id: id.into(), ..Self::default() }
    }

    /// With a name.
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// With an address.
    pub fn email(mut self, email: impl Into<String>) -> Self {
        self.email = Some(email.into());
        self
    }

    /// With an avatar.
    pub fn avatar_url(mut self, url: impl Into<String>) -> Self {
        self.avatar_url = Some(url.into());
        self
    }

    /// With one more field, shown as it is.
    pub fn with(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.extra.insert(key.into(), value.into());
        self
    }
}
