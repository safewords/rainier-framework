//! What an issued token is allowed to do — [`Abilities`].
//!
//! ```ignore
//! router.get("/api/posts", index)
//!     .middleware((Authenticate::<User>::resolved(), RequireAbility::any(["posts:read"])));
//! ```
//!
//! Sanctum's shape. A token belongs to somebody, and it may be allowed to do
//! **less** than they are — which is the entire reason to issue one rather than
//! hand over a password. A CI job that only publishes releases should hold a
//! token that can only publish releases, so the leak that eventually happens
//! leaks that and not the account.
//!
//! # This is not the [`Gate`](crate::Gate)
//!
//! They compose, and they answer different questions:
//!
//! | | Asks | Denies |
//! |---|---|---|
//! | [`Gate`](crate::Gate) | may this **actor** do this at all? | `403`, because of who they are |
//! | [`Abilities`] | was this **token** issued for it? | `403`, because of what it was for |
//!
//! An admin's read-only token must be refused a write, and no policy about
//! admins can express that — the policy is about the person, and the person is
//! an admin. Checking both is the point: the token narrows what its owner can
//! reach, and the gate decides what its owner could ever reach.

use serde::{Deserialize, Serialize};

/// The everything wildcard, as Sanctum spells it.
const EVERYTHING: &str = "*";

/// What a token may do.
///
/// Order is not significant and duplicates are harmless; this is a set spelled
/// as a list because that is how it is stored — one `abilities` column holding
/// `posts:read,posts:write`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Abilities(Vec<String>);

impl Abilities {
    /// A token that may do nothing.
    ///
    /// Useful as a deliberate value: a token issued mid-enrolment, or one
    /// suspended without being deleted.
    pub fn none() -> Self {
        Self(Vec::new())
    }

    /// A token that may do whatever its owner may — `*`.
    ///
    /// The default for a provider that does not implement abilities, so
    /// adding this module to an application changes nothing until it starts
    /// issuing narrower tokens.
    pub fn everything() -> Self {
        Self(vec![EVERYTHING.to_string()])
    }

    /// From a list.
    pub fn new(abilities: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self(abilities.into_iter().map(Into::into).collect())
    }

    /// From the comma-separated form a database column holds.
    ///
    /// Whitespace around each entry is trimmed and empty entries are dropped,
    /// so `"posts:read, posts:write,"` is two abilities rather than three with
    /// one of them blank.
    pub fn parse(stored: &str) -> Self {
        Self(
            stored
                .split(',')
                .map(str::trim)
                .filter(|ability| !ability.is_empty())
                .map(str::to_string)
                .collect(),
        )
    }

    /// Back to the comma-separated form.
    pub fn to_csv(&self) -> String {
        self.0.join(",")
    }

    /// Whether this token may `ability`.
    ///
    /// Three things match:
    ///
    /// | Held | Grants |
    /// |---|---|
    /// | `*` | everything |
    /// | `posts:read` | exactly `posts:read` |
    /// | `posts:*` | every ability beginning `posts:` |
    ///
    /// The namespace wildcard is the one extension over Sanctum, and it earns
    /// its place: without it, "this token manages posts" is spelled by listing
    /// every verb, and the list goes stale the day somebody adds a verb.
    ///
    /// Matching is **exact otherwise** — no case folding, no trimming. An
    /// ability is an identifier the application chose, and being lenient about
    /// it means `Posts:Read` silently granting `posts:read`.
    pub fn can(&self, ability: &str) -> bool {
        self.0.iter().any(|held| grants(held, ability))
    }

    /// The inverse of [`can`](Self::can).
    pub fn cannot(&self, ability: &str) -> bool {
        !self.can(ability)
    }

    /// Whether this token may **all** of these.
    pub fn can_all<'a>(&self, abilities: impl IntoIterator<Item = &'a str>) -> bool {
        abilities.into_iter().all(|ability| self.can(ability))
    }

    /// Whether this token may **any** of these.
    ///
    /// An empty list is `false`: "any of nothing" is not a grant, and reading
    /// it as one would make an empty configuration open the door.
    pub fn can_any<'a>(&self, abilities: impl IntoIterator<Item = &'a str>) -> bool {
        abilities.into_iter().any(|ability| self.can(ability))
    }

    /// Whether this token holds the everything wildcard.
    pub fn is_unrestricted(&self) -> bool {
        self.0.iter().any(|held| held == EVERYTHING)
    }

    /// Whether this token may do nothing at all.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// What it holds, verbatim.
    pub fn as_slice(&self) -> &[String] {
        &self.0
    }
}

/// Whether holding `held` grants `wanted`.
fn grants(held: &str, wanted: &str) -> bool {
    if held == EVERYTHING || held == wanted {
        return true;
    }

    // `posts:*` grants `posts:read`. The prefix keeps its separator, so
    // `posts:*` does not grant `postscript:read`.
    match held.strip_suffix('*') {
        Some(prefix) if !prefix.is_empty() => wanted.starts_with(prefix),
        _ => false,
    }
}

impl std::fmt::Display for Abilities {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_csv())
    }
}

impl From<&str> for Abilities {
    fn from(stored: &str) -> Self {
        Self::parse(stored)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_exact_ability_grants_itself_and_nothing_else() {
        let abilities = Abilities::new(["posts:read"]);

        assert!(abilities.can("posts:read"));
        assert!(abilities.cannot("posts:write"));
        assert!(abilities.cannot("posts"));
        assert!(abilities.cannot(""));
    }

    #[test]
    fn the_everything_wildcard_grants_everything() {
        let abilities = Abilities::everything();

        assert!(abilities.can("posts:read"));
        assert!(abilities.can("anything at all"));
        assert!(abilities.is_unrestricted());
    }

    #[test]
    fn a_namespace_wildcard_stops_at_its_prefix() {
        let abilities = Abilities::new(["posts:*"]);

        assert!(abilities.can("posts:read"));
        assert!(abilities.can("posts:write"));
        assert!(abilities.can("posts:"), "the prefix itself");

        // The one that would be a hole: a longer namespace starting the same
        // way. `posts:*` must not reach `postscript:`.
        assert!(abilities.cannot("postscript:read"));
        assert!(abilities.cannot("comments:read"));
        assert!(!abilities.is_unrestricted());
    }

    #[test]
    fn no_abilities_grants_nothing() {
        let abilities = Abilities::none();

        assert!(abilities.cannot("posts:read"));
        assert!(abilities.cannot("*"), "asking for the wildcard is not holding it");
        assert!(abilities.is_empty());
    }

    #[test]
    fn matching_is_exact_rather_than_lenient() {
        // Being helpful here means `Posts:Read` silently granting `posts:read`,
        // and an ability is an identifier the application chose.
        let abilities = Abilities::new(["posts:read"]);

        assert!(abilities.cannot("Posts:Read"));
        assert!(abilities.cannot("posts:read "));
        assert!(abilities.cannot(" posts:read"));
    }

    #[test]
    fn a_stored_column_round_trips() {
        let abilities = Abilities::parse("posts:read, posts:write,");

        assert_eq!(abilities.as_slice(), ["posts:read", "posts:write"]);
        assert_eq!(abilities.to_csv(), "posts:read,posts:write");
        assert_eq!(Abilities::parse(&abilities.to_csv()), abilities);
    }

    #[test]
    fn an_empty_column_is_no_abilities_rather_than_one_blank_one() {
        // A `NOT NULL DEFAULT ''` column is exactly the shape this arrives in,
        // and reading it as a single unnamed ability would be a grant.
        for stored in ["", "   ", ",", " , "] {
            let abilities = Abilities::parse(stored);

            assert!(abilities.is_empty(), "{stored:?}");
            assert!(abilities.cannot(""), "{stored:?}");
        }
    }

    #[test]
    fn all_and_any_do_what_they_say() {
        let abilities = Abilities::new(["posts:read", "comments:*"]);

        assert!(abilities.can_all(["posts:read", "comments:write"]));
        assert!(!abilities.can_all(["posts:read", "posts:write"]));

        assert!(abilities.can_any(["posts:write", "posts:read"]));
        assert!(!abilities.can_any(["posts:write", "users:read"]));
    }

    #[test]
    fn any_of_nothing_is_not_a_grant() {
        // An empty required-list arrives from a misconfigured route, and
        // reading it as "no requirement" opens the door it was guarding.
        let abilities = Abilities::everything();

        assert!(!abilities.can_any(std::iter::empty()));
        assert!(abilities.can_all(std::iter::empty()), "all of nothing is vacuously true");
    }

    #[test]
    fn it_displays_as_what_would_be_stored() {
        assert_eq!(Abilities::new(["a", "b"]).to_string(), "a,b");
        assert_eq!(Abilities::none().to_string(), "");
    }
}
