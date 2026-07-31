//! Who can log in — [`Authenticatable`], [`Credentials`] and [`UserProvider`].

use std::collections::HashMap;
use std::sync::Arc;

use rainier_database::{Model, Repository};
use rainier_orm::sea_query::Value;
use rainier_support::Result;

use crate::abilities::Abilities;
use crate::hashing::Hasher;

/// A model that can be authenticated.
///
/// Implemented on the application's user model. The framework only ever needs
/// these three facts about it, which is what keeps the guards independent of
/// whatever else a user row holds.
pub trait Authenticatable: Send + Sync + 'static {
    /// The value that identifies this user in a session or a token subject.
    fn auth_identifier(&self) -> String;

    /// The stored password hash, or `None` for a user who cannot log in with a
    /// password (an SSO-only or machine account).
    fn auth_password_hash(&self) -> Option<&str>;

    /// The column credentials are looked up by — usually `email`.
    fn auth_username_column() -> &'static str
    where
        Self: Sized,
    {
        "email"
    }

    /// The column holding an API token, when the application issues them.
    fn auth_token_column() -> &'static str
    where
        Self: Sized,
    {
        "api_token"
    }
}

/// The values a user submitted to log in.
///
/// A map rather than a struct because the identifying field varies (`email`,
/// `username`, `phone`) and an application may authenticate on more than one
/// (`email` plus `tenant_id`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Credentials {
    values: HashMap<String, String>,
}

impl Credentials {
    /// Empty credentials.
    pub fn new() -> Self {
        Self::default()
    }

    /// The usual pair.
    pub fn password(username: impl Into<String>, password: impl Into<String>) -> Self {
        Self::new().with("email", username).with("password", password)
    }

    /// Add a field.
    pub fn with(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.values.insert(key.into(), value.into());
        self
    }

    /// Read a field.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }

    /// The submitted password.
    pub fn password_value(&self) -> Option<&str> {
        self.get("password")
    }

    /// Every field except `password` — what a provider looks a user up by.
    ///
    /// The password is excluded because it is *verified*, never queried:
    /// a `WHERE password = ?` against a hash column could never match, and
    /// against a plaintext column would be a much worse bug.
    pub fn lookup_fields(&self) -> Vec<(&str, &str)> {
        self.values
            .iter()
            .filter(|(key, _)| key.as_str() != "password")
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect()
    }

    /// Whether there is anything to look a user up by.
    pub fn is_empty(&self) -> bool {
        self.lookup_fields().is_empty()
    }
}

/// Finds and verifies users.
///
/// Generic over the user type rather than yielding `dyn Authenticatable`: an
/// application has one user model and wants it back with its own type, so
/// erasing it here would only force a downcast at every call site.
#[async_trait::async_trait]
pub trait UserProvider<U: Authenticatable>: Send + Sync + 'static {
    /// The user with this identifier.
    async fn retrieve_by_id(&self, id: &str) -> Result<Option<U>>;

    /// The user matching these credentials, **without** checking the password.
    async fn retrieve_by_credentials(&self, credentials: &Credentials) -> Result<Option<U>>;

    /// Whether the credentials are valid for `user`.
    async fn validate_credentials(&self, user: &U, credentials: &Credentials) -> Result<bool>;

    /// The user holding this API token.
    async fn retrieve_by_token(&self, token: &str) -> Result<Option<U>> {
        let _ = token;
        Ok(None)
    }

    /// What the token itself is allowed to do.
    ///
    /// Defaults to [`Abilities::everything`], which is what a provider with no
    /// `abilities` column means: the token may do whatever its owner may. So
    /// nothing changes for an application until it starts issuing narrower
    /// tokens.
    ///
    /// Only called for a token that already resolved to a user, so this does
    /// not need to re-authenticate — it is answering "what was this token
    /// issued for", not "is it valid".
    async fn retrieve_abilities_by_token(&self, token: &str) -> Result<Abilities> {
        let _ = token;
        Ok(Abilities::everything())
    }
}

/// A [`UserProvider`] backed by a [`Repository`] — the usual one.
pub struct RepositoryUserProvider<U: Model + Authenticatable> {
    users: Arc<dyn Repository<U>>,
    hasher: Arc<dyn Hasher>,
}

impl<U: Model + Authenticatable> RepositoryUserProvider<U> {
    /// Look users up through `users`, verifying passwords with `hasher`.
    pub fn new(users: Arc<dyn Repository<U>>, hasher: Arc<dyn Hasher>) -> Self {
        Self { users, hasher }
    }

    /// The repository users are read from.
    pub fn repository(&self) -> &Arc<dyn Repository<U>> {
        &self.users
    }
}

#[async_trait::async_trait]
impl<U> UserProvider<U> for RepositoryUserProvider<U>
where
    U: Model + Authenticatable,
{
    async fn retrieve_by_id(&self, id: &str) -> Result<Option<U>> {
        // The identifier is a string because that is what a session cookie or
        // a token subject carries. Numeric keys are parsed back so the query
        // binds an integer rather than a string, which would never match on a
        // strictly-typed backend.
        let key: Value = match id.parse::<i64>() {
            Ok(numeric) => numeric.into(),
            Err(_) => id.into(),
        };
        self.users.find(key).await
    }

    async fn retrieve_by_credentials(&self, credentials: &Credentials) -> Result<Option<U>> {
        let fields = credentials.lookup_fields();
        if fields.is_empty() {
            // Without this, an empty credential set would select the first
            // user in the table and let anyone in as them.
            return Ok(None);
        }

        let mut criteria = rainier_database::Criteria::new();
        for (column, value) in fields {
            criteria = criteria.where_eq(column, value);
        }
        self.users.first_matching(criteria).await
    }

    async fn validate_credentials(&self, user: &U, credentials: &Credentials) -> Result<bool> {
        let (Some(submitted), Some(stored)) =
            (credentials.password_value(), user.auth_password_hash())
        else {
            // No password submitted, or an account that has none: refuse
            // rather than treat "nothing to compare" as a match.
            return Ok(false);
        };
        Ok(self.hasher.verify(submitted, stored))
    }

    async fn retrieve_by_token(&self, token: &str) -> Result<Option<U>> {
        if token.is_empty() {
            return Ok(None);
        }
        self.users.first_by(U::auth_token_column(), token.into()).await
    }
}

impl<U: Model + Authenticatable> std::fmt::Debug for RepositoryUserProvider<U> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RepositoryUserProvider").field("model", &U::model_name()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hashing::Argon2Hasher;
    use rainier_database::testing::{fake_database, MemoryConnection};
    use rainier_database::{EntityRepository, OwnedRow};
    use rainier_orm::Dialect;

    #[derive(rainier_orm::Entity, Clone, Debug, PartialEq)]
    #[orm(table = "users")]
    struct User {
        #[orm(pk, auto_increment)]
        id: u64,
        #[orm(unique)]
        email: String,
        password: String,
        api_token: Option<String>,
    }

    impl Model for User {}

    impl Authenticatable for User {
        fn auth_identifier(&self) -> String {
            self.id.to_string()
        }
        fn auth_password_hash(&self) -> Option<&str> {
            Some(&self.password)
        }
    }

    fn hasher() -> Arc<dyn Hasher> {
        Arc::new(Argon2Hasher::insecure_for_tests())
    }

    fn user_row(hash: &str) -> OwnedRow {
        OwnedRow::new()
            .with("id", 1_u64)
            .with("email", "ada@example.com")
            .with("password", hash)
            .with("api_token", "tok_123")
    }

    fn provider(
        connection: MemoryConnection,
    ) -> (RepositoryUserProvider<User>, Arc<MemoryConnection>) {
        let (db, handle) = fake_database(connection);
        let users: Arc<dyn Repository<User>> = Arc::new(EntityRepository::<User>::new(db));
        (RepositoryUserProvider::new(users, hasher()), handle)
    }

    #[test]
    fn credentials_exclude_the_password_from_lookups() {
        let credentials = Credentials::password("ada@example.com", "secret");

        assert_eq!(credentials.password_value(), Some("secret"));
        assert_eq!(credentials.lookup_fields(), vec![("email", "ada@example.com")]);
        assert!(!credentials.is_empty());
    }

    #[test]
    fn credentials_can_carry_extra_lookup_fields() {
        let credentials = Credentials::password("ada@example.com", "secret").with("tenant_id", "7");

        let mut fields = credentials.lookup_fields();
        fields.sort();
        assert_eq!(fields, vec![("email", "ada@example.com"), ("tenant_id", "7")]);
    }

    #[test]
    fn credentials_with_only_a_password_have_nothing_to_look_up_by() {
        let credentials = Credentials::new().with("password", "secret");
        assert!(credentials.is_empty());
    }

    #[tokio::test]
    async fn retrieves_a_user_by_credentials() {
        let (provider, connection) =
            provider(MemoryConnection::new(Dialect::Sqlite).returning([user_row("$argon2id$x")]));

        let found = provider
            .retrieve_by_credentials(&Credentials::password("ada@example.com", "secret"))
            .await
            .unwrap();

        assert_eq!(found.unwrap().email, "ada@example.com");

        // The password column is *selected* (it has to be, to verify against)
        // but must never appear as a predicate: a `WHERE password = ?` against
        // a hash could never match, and against plaintext would be far worse.
        let sql = connection.last_statement().unwrap();
        let (_, filters) = sql.split_once("WHERE").expect("the lookup should be filtered");
        assert!(filters.contains("email"), "{sql}");
        assert!(!filters.contains("password"), "the password must never be queried: {sql}");
        assert_eq!(connection.bindings()[0][0], "ada@example.com".into());
    }

    #[tokio::test]
    async fn empty_credentials_never_match_a_user() {
        // Regression guard: with no lookup fields, an unfiltered query would
        // return the first user in the table and let anyone in as them.
        let (provider, connection) =
            provider(MemoryConnection::new(Dialect::Sqlite).returning([user_row("$argon2id$x")]));

        assert!(provider.retrieve_by_credentials(&Credentials::new()).await.unwrap().is_none());
        assert_eq!(connection.statement_count(), 0, "and it should not even query");
    }

    #[tokio::test]
    async fn validates_a_password_against_the_stored_hash() {
        let hasher = Argon2Hasher::insecure_for_tests();
        let stored = hasher.hash("secret").unwrap();
        let (provider, _) = provider(MemoryConnection::new(Dialect::Sqlite));

        let user =
            User { id: 1, email: "ada@example.com".into(), password: stored, api_token: None };

        assert!(provider
            .validate_credentials(&user, &Credentials::password("ada@example.com", "secret"))
            .await
            .unwrap());

        assert!(!provider
            .validate_credentials(&user, &Credentials::password("ada@example.com", "wrong"))
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn a_missing_password_never_validates() {
        let (provider, _) = provider(MemoryConnection::new(Dialect::Sqlite));
        let user = User {
            id: 1,
            email: "ada@example.com".into(),
            password: String::new(),
            api_token: None,
        };

        // No password submitted at all.
        let no_password = Credentials::new().with("email", "ada@example.com");
        assert!(!provider.validate_credentials(&user, &no_password).await.unwrap());
    }

    #[tokio::test]
    async fn retrieves_by_id_binding_a_numeric_key_as_a_number() {
        let (provider, connection) =
            provider(MemoryConnection::new(Dialect::Sqlite).returning([user_row("h")]));

        assert!(provider.retrieve_by_id("1").await.unwrap().is_some());
        assert_eq!(connection.bindings()[0][0], 1_i64.into(), "not the string \"1\"");
    }

    #[tokio::test]
    async fn retrieves_by_token() {
        let (provider, connection) =
            provider(MemoryConnection::new(Dialect::Sqlite).returning([user_row("h")]));

        assert!(provider.retrieve_by_token("tok_123").await.unwrap().is_some());
        assert!(connection.last_statement().unwrap().contains("api_token"));
    }

    #[tokio::test]
    async fn an_empty_token_never_matches() {
        let (provider, connection) =
            provider(MemoryConnection::new(Dialect::Sqlite).returning([user_row("h")]));

        assert!(provider.retrieve_by_token("").await.unwrap().is_none());
        assert_eq!(connection.statement_count(), 0);
    }
}
