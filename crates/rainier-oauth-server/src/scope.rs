//! Scopes — what a token may do, and which ones exist — [`Scopes`].
//!
//! # Inheritance
//!
//! A scope names a namespace: holding `vehicles` grants `vehicles:read` and
//! `vehicles:read:location`, the way holding a directory grants its files.
//! Holding `*` grants everything. The reverse never holds — `vehicles:read`
//! does not grant `vehicles`.
//!
//! The guard hands a token's scopes to `rainier-auth` as
//! [`Abilities`](rainier_auth::Abilities), whose own wildcard is `ns:*`, so
//! each held scope goes over as itself **and** `scope:*` — and `RequireAbility`
//! answers exactly as the scope check here does.

use std::collections::BTreeMap;

use crate::error::OAuthError;
use crate::model::{grant, Client};

/// The everything scope.
pub const EVERYTHING: &str = "*";

/// The scopes this server knows, with their consent-screen descriptions.
#[derive(Debug, Clone, Default)]
pub struct Scopes {
    known: BTreeMap<String, String>,
    default: Vec<String>,
}

impl Scopes {
    /// No scopes. Every request then asks for nothing and gets nothing,
    /// which is a working server for an API with no scopes at all.
    pub fn new() -> Self {
        Self::default()
    }

    /// Declare `scope`, described for a consent screen.
    pub fn with(mut self, scope: impl Into<String>, description: impl Into<String>) -> Self {
        self.known.insert(scope.into(), description.into());
        self
    }

    /// What a request that names no scope gets. Empty by default.
    pub fn default_scopes(mut self, scopes: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.default = scopes.into_iter().map(Into::into).collect();
        self
    }

    /// Whether `scope` exists. `*` always does.
    pub fn exists(&self, scope: &str) -> bool {
        scope == EVERYTHING || self.known.contains_key(scope)
    }

    /// A scope's description.
    pub fn description(&self, scope: &str) -> Option<&str> {
        self.known.get(scope).map(String::as_str)
    }

    /// Every declared scope and its description.
    pub fn all(&self) -> impl Iterator<Item = (&str, &str)> {
        self.known.iter().map(|(scope, description)| (scope.as_str(), description.as_str()))
    }

    /// Parse a space-delimited `scope` parameter, falling back to the default,
    /// refusing an unknown scope by name.
    pub fn validate(&self, requested: Option<&str>) -> Result<Vec<String>, OAuthError> {
        let requested: Vec<String> = match requested.map(str::trim).filter(|s| !s.is_empty()) {
            Some(list) => list.split(' ').filter(|s| !s.is_empty()).map(str::to_string).collect(),
            None => self.default.clone(),
        };
        for scope in &requested {
            if !self.exists(scope) {
                return Err(OAuthError::invalid_scope(scope));
            }
        }
        Ok(requested)
    }

    /// The scopes a token is actually issued with.
    ///
    /// `*` survives only where no user consents to it scope by scope — the
    /// password, client credentials and personal access grants; over an
    /// authorization code or a refresh it is dropped, so a third party cannot
    /// talk a user into "everything" behind a consent screen that listed
    /// nothing. Then the client's own allow-list, and duplicates removed.
    pub fn finalize(&self, scopes: Vec<String>, grant_type: &str, client: &Client) -> Vec<String> {
        let keeps_everything = matches!(
            grant_type,
            grant::PASSWORD | grant::PERSONAL_ACCESS | grant::CLIENT_CREDENTIALS
        );
        let mut out: Vec<String> = Vec::new();
        for scope in scopes {
            if scope == EVERYTHING && !keeps_everything {
                continue;
            }
            if self.exists(&scope) && client.allows_scope(&scope) && !out.contains(&scope) {
                out.push(scope);
            }
        }
        out
    }
}

/// Whether holding `held` grants `wanted` — exactly, by namespace, or by `*`.
pub fn grants(held: &str, wanted: &str) -> bool {
    held == EVERYTHING
        || held == wanted
        || (wanted.len() > held.len()
            && wanted.starts_with(held)
            && wanted.as_bytes()[held.len()] == b':')
}

/// Whether any of `held` grants `wanted`.
pub fn held_grants(held: &[String], wanted: &str) -> bool {
    held.iter().any(|scope| grants(scope, wanted))
}

/// Scopes as `rainier-auth` abilities: each as itself and as its namespace
/// wildcard, so `Abilities::can` agrees with [`grants`].
pub fn to_abilities(scopes: &[String]) -> rainier_auth::Abilities {
    let mut abilities = Vec::with_capacity(scopes.len() * 2);
    for scope in scopes {
        abilities.push(scope.clone());
        if scope != EVERYTHING {
            abilities.push(format!("{scope}:*"));
        }
    }
    rainier_auth::Abilities::new(abilities)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scopes() -> Scopes {
        Scopes::new().with("account", "Your account").with("vehicles", "Your vehicles")
    }

    fn client(allowed: Option<Vec<&str>>) -> Client {
        Client {
            id: "c".into(),
            name: "n".into(),
            secret_hash: None,
            redirect_uris: vec![],
            grant_types: vec![],
            scopes: allowed.map(|list| list.into_iter().map(String::from).collect()),
            provider: None,
            owner_id: None,
            revoked: false,
        }
    }

    #[test]
    fn an_unknown_scope_is_refused_by_name() {
        let err = scopes().validate(Some("account nope")).unwrap_err();
        assert_eq!(err.error, "invalid_scope");
        assert_eq!(err.hint.as_deref(), Some("Check the `nope` scope"));
    }

    #[test]
    fn no_scope_falls_back_to_the_default() {
        let scopes = scopes().default_scopes(["account"]);
        assert_eq!(scopes.validate(None).unwrap(), vec!["account"]);
        assert_eq!(scopes.validate(Some("  ")).unwrap(), vec!["account"]);
    }

    #[test]
    fn everything_survives_only_where_no_consent_screen_was_involved() {
        let s = scopes();
        let c = client(None);
        let asked = vec!["*".to_string(), "account".to_string()];
        assert_eq!(s.finalize(asked.clone(), grant::AUTHORIZATION_CODE, &c), vec!["account"]);
        assert_eq!(s.finalize(asked.clone(), grant::REFRESH_TOKEN, &c), vec!["account"]);
        assert_eq!(s.finalize(asked, grant::PASSWORD, &c), vec!["*", "account"]);
    }

    #[test]
    fn a_client_allow_list_narrows_the_grant() {
        let s = scopes();
        let c = client(Some(vec!["vehicles"]));
        let asked = vec!["account".to_string(), "vehicles".to_string()];
        assert_eq!(s.finalize(asked, grant::PASSWORD, &c), vec!["vehicles"]);
    }

    #[test]
    fn a_namespace_grants_what_is_under_it_and_not_the_reverse() {
        assert!(grants("vehicles", "vehicles:read"));
        assert!(grants("vehicles", "vehicles:read:location"));
        assert!(!grants("vehicles:read", "vehicles"));
        assert!(!grants("vehicles", "vehiclesx"));
        assert!(grants("*", "anything"));
    }

    #[test]
    fn abilities_agree_with_the_scope_check() {
        let abilities = to_abilities(&["vehicles".to_string()]);
        assert!(abilities.can("vehicles"));
        assert!(abilities.can("vehicles:read"));
        assert!(!abilities.can("account"));
        assert!(to_abilities(&["*".to_string()]).can("account"));
    }
}
