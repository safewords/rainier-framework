//! Taking over from a PHP OAuth server — its tokens and its tables, left where
//! they are.
//!
//! league/oauth2-server is the OAuth server most PHP applications run, usually
//! through a framework integration that adds the tables. A Rainier
//! [`AuthorizationServer`](rainier_oauth_server::AuthorizationServer) can
//! answer for one without anybody noticing, which takes three things:
//!
//! | | |
//! |---|---|
//! | [`DefuseSealer`] | opens and writes refresh tokens and authorization codes as league seals them — defuse, password-based, keyed by the application key |
//! | [`LeagueOAuthStore`] | the `oauth_clients`, `oauth_access_tokens`, `oauth_refresh_tokens` and `oauth_auth_codes` tables, read and written as the PHP side does |
//! | the existing RSA key pair | access tokens are RS256 JWTs with no `kid` and the claims [`AccessTokenSigner`] already writes; point it at the same private key |
//!
//! With all three, an access token PHP issued yesterday authenticates against
//! the Rainier guard, a refresh token it issued last year refreshes, and either
//! side can be switched off without logging anybody out — the two can run side
//! by side against one database while traffic moves.
//!
//! [`AccessTokenSigner`]: rainier_oauth_server::AccessTokenSigner
//!
//! # The application key is the defuse password
//!
//! league is constructed with an encryption key that the framework integration
//! fills with the application's decoded 32-byte key. A string key makes league
//! use defuse's **password** mode, so that is what [`DefuseSealer`] does —
//! [`DefuseSealer::from_app_key`] takes the key exactly as it appears in the
//! environment, `base64:` prefix and all.
//!
//! # Client rows come in two shapes
//!
//! Older schemas say what a client may do with flags (`redirect`,
//! `personal_access_client`, `password_client`, `user_id`); newer ones say it
//! with JSON lists (`redirect_uris`, `grant_types`) and a polymorphic owner. The
//! store reads `SELECT *` and derives the same grant list the PHP model does
//! from whichever columns are present — see [`client_from_row`]. Which shape
//! the table has is asked once, at first use.
//!
//! # Verify before cutting over
//!
//! `tools/php-oauth-fixtures.php` exports a freshly issued access token,
//! refresh token and authorization code from the running PHP application; the
//! ignored tests at the bottom of this module open them:
//!
//! ```text
//! RAINIER_COMPAT_FIXTURES=/path/to/fixtures.json \
//!     cargo test -p rainier-compat --features php-oauth -- --ignored
//! ```

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use chrono::{DateTime, FixedOffset, NaiveDateTime, TimeZone, Utc};
use rainier_database::{Cell, ColumnRequest, Database, OwnedRow};
use rainier_oauth_server::{
    grant, AccessTokenRecord, AuthCodeRecord, Client, OAuthStore, PurgeOptions, Purged,
    RefreshTokenRecord, TokenSealer,
};
use rainier_orm::sea_query::Value;
use rainier_orm::{ColumnType, Dialect};
use rainier_support::{Error, Result};
use serde_json::Value as Json;

use super::defuse;

// --- sealing -------------------------------------------------------------------

/// Seals refresh tokens and authorization codes as league/oauth2-server does
/// when given a string key: defuse v2, password mode, hex.
pub struct DefuseSealer {
    password: Vec<u8>,
}

impl DefuseSealer {
    /// Seal with `password` — the raw bytes league was given.
    pub fn new(password: impl Into<Vec<u8>>) -> Self {
        Self { password: password.into() }
    }

    /// Seal with the application key as it appears in the environment:
    /// `base64:…` is decoded, anything else is used as its bytes.
    pub fn from_app_key(app_key: &str) -> Result<Self> {
        Ok(Self::new(app_key_bytes(app_key)?))
    }
}

/// The bytes a PHP application uses as its key: `base64:` decoded, otherwise
/// the string itself.
pub fn app_key_bytes(app_key: &str) -> Result<Vec<u8>> {
    match app_key.trim().strip_prefix("base64:") {
        Some(encoded) => STANDARD
            .decode(encoded)
            .map_err(|_| Error::internal("the application key's base64 does not decode")),
        None => Ok(app_key.trim().as_bytes().to_vec()),
    }
}

impl TokenSealer for DefuseSealer {
    fn seal(&self, plain: &[u8]) -> Result<String> {
        Ok(defuse::encrypt_with_password(plain, &self.password))
    }

    fn open(&self, sealed: &str) -> Result<Vec<u8>> {
        defuse::decrypt_with_password(sealed, &self.password)
    }
}

impl std::fmt::Debug for DefuseSealer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DefuseSealer").finish_non_exhaustive()
    }
}

// --- the tables ----------------------------------------------------------------

/// Table names, for a schema that prefixed or renamed them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tables {
    /// Clients.
    pub clients: String,
    /// Access tokens.
    pub access_tokens: String,
    /// Refresh tokens.
    pub refresh_tokens: String,
    /// Authorization codes.
    pub auth_codes: String,
}

impl Default for Tables {
    fn default() -> Self {
        Self {
            clients: "oauth_clients".into(),
            access_tokens: "oauth_access_tokens".into(),
            refresh_tokens: "oauth_refresh_tokens".into(),
            auth_codes: "oauth_auth_codes".into(),
        }
    }
}

/// An [`OAuthStore`] over the tables a PHP OAuth server keeps.
pub struct LeagueOAuthStore {
    db: Arc<Database>,
    tables: Tables,
    timezone: FixedOffset,
    default_provider: Option<String>,
    owner_column: OnceLock<&'static str>,
}

impl LeagueOAuthStore {
    /// A store over `db`, with the default table names, datetimes in UTC.
    pub fn new(db: Arc<Database>) -> Self {
        Self {
            db,
            tables: Tables::default(),
            timezone: FixedOffset::east_opt(0).expect("UTC"),
            default_provider: None,
            owner_column: OnceLock::new(),
        }
    }

    /// Different table names.
    pub fn tables(mut self, tables: Tables) -> Self {
        self.tables = tables;
        self
    }

    /// The timezone the PHP application writes datetimes in — its configured
    /// application timezone. Datetime columns carry no zone, so reading them
    /// in the wrong one shifts every expiry by the offset.
    pub fn timezone(mut self, offset: FixedOffset) -> Self {
        self.timezone = offset;
        self
    }

    /// The user provider a client with no `provider` belongs to — what the
    /// PHP side calls its default API guard's provider. A personal access
    /// client with no provider serves this one.
    pub fn default_provider(mut self, provider: impl Into<String>) -> Self {
        self.default_provider = Some(provider.into());
        self
    }

    fn dialect(&self) -> Dialect {
        self.db.dialect()
    }

    /// `col` as text, whatever its SQL type — so an unsigned id, a signed one
    /// and a UUID all read the same way, as does a zone-less datetime.
    fn text(&self, column: &str) -> String {
        match self.dialect() {
            Dialect::MySql => format!("CAST({column} AS CHAR) AS {column}"),
            _ => format!("CAST({column} AS TEXT) AS {column}"),
        }
    }

    /// A placeholder for a datetime bound as `Y-m-d H:i:s` text.
    fn datetime_placeholder(&self) -> &'static str {
        match self.dialect() {
            Dialect::Postgres => "CAST(? AS TIMESTAMP)",
            _ => "?",
        }
    }

    /// A datetime as the PHP side writes it.
    fn format(&self, at: DateTime<Utc>) -> String {
        at.with_timezone(&self.timezone).format("%Y-%m-%d %H:%M:%S").to_string()
    }

    /// A datetime as the PHP side wrote it.
    fn parse(&self, text: &str) -> Option<DateTime<Utc>> {
        let text = text.trim();
        let naive = NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S%.f")
            .or_else(|_| NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S%.f"))
            .ok()
            .or_else(|| DateTime::parse_from_rfc3339(text).ok().map(|dt| dt.naive_utc()))?;
        self.timezone.from_local_datetime(&naive).single().map(|dt| dt.with_timezone(&Utc))
    }

    fn token_columns(&self) -> String {
        [
            self.text("id"),
            self.text("user_id"),
            self.text("client_id"),
            "name".into(),
            "scopes".into(),
            "revoked".into(),
            self.text("created_at"),
            self.text("expires_at"),
        ]
        .join(", ")
    }

    fn token_requests() -> Vec<ColumnRequest> {
        vec![
            ColumnRequest::new("id", ColumnType::Text),
            ColumnRequest::new("user_id", ColumnType::Text),
            ColumnRequest::new("client_id", ColumnType::Text),
            ColumnRequest::new("name", ColumnType::Text),
            ColumnRequest::new("scopes", ColumnType::Text),
            ColumnRequest::new("revoked", ColumnType::Bool),
            ColumnRequest::new("created_at", ColumnType::Text),
            ColumnRequest::new("expires_at", ColumnType::Text),
        ]
    }

    fn access_token_from(&self, row: &OwnedRow) -> AccessTokenRecord {
        AccessTokenRecord {
            id: text(row, "id").unwrap_or_default(),
            user_id: text(row, "user_id"),
            client_id: text(row, "client_id").unwrap_or_default(),
            name: text(row, "name"),
            scopes: scopes(row),
            revoked: boolean(row, "revoked"),
            created_at: text(row, "created_at").and_then(|t| self.parse(&t)).unwrap_or_default(),
            // A token with no expiry never expires, which is how the PHP side
            // reads a NULL too.
            expires_at: text(row, "expires_at")
                .and_then(|t| self.parse(&t))
                .unwrap_or(DateTime::<Utc>::MAX_UTC),
        }
    }

    fn client_requests() -> Vec<ColumnRequest> {
        let text = |name: &str| ColumnRequest::new(name, ColumnType::Text);
        vec![
            text("id"),
            text("name"),
            text("secret"),
            text("provider"),
            text("redirect"),
            text("redirect_uris"),
            text("grant_types"),
            text("scopes"),
            text("owner_ref"),
            ColumnRequest::new("personal_access_client", ColumnType::Bool),
            ColumnRequest::new("password_client", ColumnType::Bool),
            ColumnRequest::new("revoked", ColumnType::Bool),
        ]
    }

    /// Which column names a client's owner: `user_id` in the flag-shaped
    /// schema, `owner_id` in the list-shaped one. Asked of the database once —
    /// a query naming a column the table lacks fails, and that failure is the
    /// answer — then remembered.
    async fn owner_column(&self) -> &'static str {
        if let Some(column) = self.owner_column.get() {
            return column;
        }
        let probe = format!("SELECT user_id FROM {} WHERE 1 = 0", self.tables.clients);
        let column =
            if self.db.query(probe).execute().await.is_ok() { "user_id" } else { "owner_id" };
        let _ = self.owner_column.set(column);
        column
    }

    /// `SELECT *` plus the owner as text, under one name whichever schema.
    async fn client_select(&self) -> String {
        let owner = self.owner_column().await;
        let cast = self.text(owner).replace(&format!(" AS {owner}"), " AS owner_ref");
        format!("SELECT *, {cast} FROM {}", self.tables.clients)
    }
}

#[async_trait]
impl OAuthStore for LeagueOAuthStore {
    async fn find_client(&self, id: &str) -> Result<Option<Client>> {
        let sql = format!("{} WHERE id = ?", self.client_select().await);
        let rows = self.db.query(sql).bind(id.to_string()).fetch(Self::client_requests()).await?;
        Ok(rows.first().map(client_from_row))
    }

    async fn personal_access_client(&self, provider: Option<&str>) -> Result<Option<Client>> {
        let ids = self
            .db
            .query(format!(
                "SELECT {} FROM {} WHERE revoked = ? ORDER BY created_at DESC",
                self.text("id"),
                self.tables.clients
            ))
            .bind(false)
            .column("id")
            .await?;

        for id in ids {
            let Some(client) = self.find_client(&id).await? else { continue };
            let provider_matches = match (provider, client.provider.as_deref()) {
                (None, None) => true,
                (None, Some(theirs)) => self.default_provider.as_deref() == Some(theirs),
                (Some(wanted), None) => self.default_provider.as_deref() == Some(wanted),
                (Some(wanted), Some(theirs)) => wanted == theirs,
            };
            if provider_matches && client.supports_grant(grant::PERSONAL_ACCESS) {
                return Ok(Some(client));
            }
        }
        Ok(None)
    }

    async fn create_access_token(&self, token: &AccessTokenRecord) -> Result<()> {
        let at = self.datetime_placeholder();
        let created = self.format(token.created_at);
        self.db
            .query(format!(
                "INSERT INTO {} (id, user_id, client_id, name, scopes, revoked, created_at, \
                 updated_at, expires_at) VALUES (?, ?, ?, ?, ?, ?, {at}, {at}, {at})",
                self.tables.access_tokens
            ))
            .bind(token.id.clone())
            .bind(user_id_value(token.user_id.as_deref()))
            .bind(token.client_id.clone())
            .bind(token.name.clone())
            .bind(scopes_json(&token.scopes))
            .bind(token.revoked)
            .bind(created.clone())
            .bind(created)
            .bind(self.format(token.expires_at))
            .execute()
            .await?;
        Ok(())
    }

    async fn find_access_token(&self, id: &str) -> Result<Option<AccessTokenRecord>> {
        let rows = self
            .db
            .query(format!(
                "SELECT {} FROM {} WHERE id = ?",
                self.token_columns(),
                self.tables.access_tokens
            ))
            .bind(id.to_string())
            .fetch(Self::token_requests())
            .await?;
        Ok(rows.first().map(|row| self.access_token_from(row)))
    }

    async fn revoke_access_token(&self, id: &str) -> Result<bool> {
        // The PHP side's update goes through its model layer, which stamps
        // `updated_at`; so does this.
        let outcome = self
            .db
            .query(format!(
                "UPDATE {} SET revoked = ?, updated_at = {} WHERE id = ? AND revoked = ?",
                self.tables.access_tokens,
                self.datetime_placeholder()
            ))
            .bind(true)
            .bind(self.format(Utc::now()))
            .bind(id.to_string())
            .bind(false)
            .execute()
            .await?;
        Ok(outcome.rows_affected > 0)
    }

    async fn create_refresh_token(&self, token: &RefreshTokenRecord) -> Result<()> {
        self.db
            .query(format!(
                "INSERT INTO {} (id, access_token_id, revoked, expires_at) VALUES (?, ?, ?, {})",
                self.tables.refresh_tokens,
                self.datetime_placeholder()
            ))
            .bind(token.id.clone())
            .bind(token.access_token_id.clone())
            .bind(token.revoked)
            .bind(self.format(token.expires_at))
            .execute()
            .await?;
        Ok(())
    }

    async fn find_refresh_token(&self, id: &str) -> Result<Option<RefreshTokenRecord>> {
        let rows = self
            .db
            .query(format!(
                "SELECT {}, {}, revoked, {} FROM {} WHERE id = ?",
                self.text("id"),
                self.text("access_token_id"),
                self.text("expires_at"),
                self.tables.refresh_tokens
            ))
            .bind(id.to_string())
            .fetch(vec![
                ColumnRequest::new("id", ColumnType::Text),
                ColumnRequest::new("access_token_id", ColumnType::Text),
                ColumnRequest::new("revoked", ColumnType::Bool),
                ColumnRequest::new("expires_at", ColumnType::Text),
            ])
            .await?;
        Ok(rows.first().map(|row| RefreshTokenRecord {
            id: text(row, "id").unwrap_or_default(),
            access_token_id: text(row, "access_token_id").unwrap_or_default(),
            revoked: boolean(row, "revoked"),
            expires_at: text(row, "expires_at")
                .and_then(|t| self.parse(&t))
                .unwrap_or(DateTime::<Utc>::MAX_UTC),
        }))
    }

    async fn revoke_refresh_token(&self, id: &str) -> Result<bool> {
        let outcome = self
            .db
            .query(format!(
                "UPDATE {} SET revoked = ? WHERE id = ? AND revoked = ?",
                self.tables.refresh_tokens
            ))
            .bind(true)
            .bind(id.to_string())
            .bind(false)
            .execute()
            .await?;
        Ok(outcome.rows_affected > 0)
    }

    async fn revoke_refresh_tokens_for(&self, access_token_id: &str) -> Result<u64> {
        let outcome = self
            .db
            .query(format!(
                "UPDATE {} SET revoked = ? WHERE access_token_id = ? AND revoked = ?",
                self.tables.refresh_tokens
            ))
            .bind(true)
            .bind(access_token_id.to_string())
            .bind(false)
            .execute()
            .await?;
        Ok(outcome.rows_affected)
    }

    async fn create_auth_code(&self, code: &AuthCodeRecord) -> Result<()> {
        self.db
            .query(format!(
                "INSERT INTO {} (id, user_id, client_id, scopes, revoked, expires_at) \
                 VALUES (?, ?, ?, ?, ?, {})",
                self.tables.auth_codes,
                self.datetime_placeholder()
            ))
            .bind(code.id.clone())
            .bind(user_id_value(Some(&code.user_id)))
            .bind(code.client_id.clone())
            .bind(scopes_json(&code.scopes))
            .bind(code.revoked)
            .bind(self.format(code.expires_at))
            .execute()
            .await?;
        Ok(())
    }

    async fn revoke_auth_code(&self, id: &str) -> Result<bool> {
        let outcome = self
            .db
            .query(format!(
                "UPDATE {} SET revoked = ? WHERE id = ? AND revoked = ?",
                self.tables.auth_codes
            ))
            .bind(true)
            .bind(id.to_string())
            .bind(false)
            .execute()
            .await?;
        Ok(outcome.rows_affected > 0)
    }

    async fn is_auth_code_revoked(&self, id: &str) -> Result<bool> {
        let live = self
            .db
            .query(format!(
                "SELECT COUNT(*) AS live FROM {} WHERE id = ? AND revoked = ?",
                self.tables.auth_codes
            ))
            .bind(id.to_string())
            .bind(false)
            .scalar_i64("live")
            .await?;
        Ok(live.unwrap_or(0) == 0)
    }

    async fn tokens_for(
        &self,
        user_id: &str,
        client_id: Option<&str>,
    ) -> Result<Vec<AccessTokenRecord>> {
        let mut sql = format!(
            "SELECT {} FROM {} WHERE user_id = ? AND revoked = ? AND expires_at > {}",
            self.token_columns(),
            self.tables.access_tokens,
            self.datetime_placeholder()
        );
        if client_id.is_some() {
            sql.push_str(" AND client_id = ?");
        }
        sql.push_str(" ORDER BY created_at DESC");

        let mut query = self
            .db
            .query(sql)
            .bind(user_id_value(Some(user_id)))
            .bind(false)
            .bind(self.format(Utc::now()));
        if let Some(client_id) = client_id {
            query = query.bind(client_id.to_string());
        }
        let rows = query.fetch(Self::token_requests()).await?;
        Ok(rows.iter().map(|row| self.access_token_from(row)).collect())
    }

    async fn purge(&self, options: PurgeOptions) -> Result<Purged> {
        let mut purged = Purged::default();
        for (table, slot) in [
            (&self.tables.access_tokens, &mut purged.access_tokens),
            (&self.tables.refresh_tokens, &mut purged.refresh_tokens),
            (&self.tables.auth_codes, &mut purged.auth_codes),
        ] {
            let (sql, cutoff) = match (options.revoked, options.expired_before) {
                (false, None) => continue,
                (true, None) => (format!("DELETE FROM {table} WHERE revoked = ?"), None),
                (false, Some(at)) => (
                    format!(
                        "DELETE FROM {table} WHERE expires_at < {}",
                        self.datetime_placeholder()
                    ),
                    Some(at),
                ),
                (true, Some(at)) => (
                    format!(
                        "DELETE FROM {table} WHERE revoked = ? OR expires_at < {}",
                        self.datetime_placeholder()
                    ),
                    Some(at),
                ),
            };
            let mut query = self.db.query(sql);
            if options.revoked {
                query = query.bind(true);
            }
            if let Some(at) = cutoff {
                query = query.bind(self.format(at));
            }
            *slot = query.execute().await?.rows_affected;
        }
        Ok(purged)
    }
}

impl std::fmt::Debug for LeagueOAuthStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LeagueOAuthStore")
            .field("tables", &self.tables)
            .field("timezone", &self.timezone)
            .finish_non_exhaustive()
    }
}

/// A client from a row of either schema, deriving its grants the way the PHP
/// model does:
///
/// - `redirect_uris` (a JSON list), else `redirect` split on commas;
/// - `grant_types` (a JSON list) when present, else derived — the
///   authorization code grant when there are redirect URIs; client
///   credentials for a confidential first-party client; password when
///   `password_client`; personal access when `personal_access_client` and
///   confidential; refresh always.
///
/// The owner is read from an `owner_ref` column — the store selects the
/// schema's owner column under that name.
pub fn client_from_row(row: &OwnedRow) -> Client {
    let owner_id = text(row, "owner_ref").filter(|s| !s.is_empty());
    let secret_hash = text(row, "secret").filter(|s| !s.is_empty());
    let confidential = secret_hash.is_some();
    let first_party = owner_id.is_none();

    let redirect_uris: Vec<String> = match text(row, "redirect_uris").filter(|s| !s.is_empty()) {
        Some(json) => serde_json::from_str::<Vec<String>>(&json).unwrap_or_default(),
        None => text(row, "redirect")
            .map(|list| {
                list.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect()
            })
            .unwrap_or_default(),
    };

    let grant_types = match text(row, "grant_types").filter(|s| !s.is_empty()) {
        Some(json) => serde_json::from_str::<Vec<String>>(&json).unwrap_or_default(),
        None => {
            let mut grants = Vec::new();
            if !redirect_uris.is_empty() {
                grants.push(grant::AUTHORIZATION_CODE.to_string());
            }
            if confidential && first_party {
                grants.push(grant::CLIENT_CREDENTIALS.to_string());
            }
            if boolean(row, "password_client") {
                grants.push(grant::PASSWORD.to_string());
            }
            if boolean(row, "personal_access_client") && confidential {
                grants.push(grant::PERSONAL_ACCESS.to_string());
            }
            grants.push(grant::REFRESH_TOKEN.to_string());
            grants
        }
    };

    Client {
        id: text(row, "id").unwrap_or_default(),
        name: text(row, "name").unwrap_or_default(),
        secret_hash,
        redirect_uris,
        grant_types,
        scopes: text(row, "scopes")
            .filter(|s| !s.is_empty())
            .and_then(|json| serde_json::from_str::<Vec<String>>(&json).ok()),
        provider: text(row, "provider").filter(|s| !s.is_empty()),
        owner_id,
        revoked: boolean(row, "revoked"),
    }
}

fn text(row: &OwnedRow, column: &str) -> Option<String> {
    match row.cell(column)? {
        Cell::Text(text) => Some(text.clone()),
        Cell::Int(n) => Some(n.to_string()),
        Cell::Uint(n) => Some(n.to_string()),
        Cell::Bytes(bytes) => String::from_utf8(bytes.clone()).ok(),
        _ => None,
    }
}

fn boolean(row: &OwnedRow, column: &str) -> bool {
    match row.cell(column) {
        Some(Cell::Bool(b)) => *b,
        Some(Cell::Int(n)) => *n != 0,
        Some(Cell::Uint(n)) => *n != 0,
        Some(Cell::Text(t)) => t == "1" || t.eq_ignore_ascii_case("true"),
        _ => false,
    }
}

/// The `scopes` column: a JSON array of names. Entries that are objects (an
/// older writer serialised scope entities) contribute their `id`.
fn scopes(row: &OwnedRow) -> Vec<String> {
    let Some(raw) = text(row, "scopes") else { return Vec::new() };
    let Ok(Json::Array(list)) = serde_json::from_str::<Json>(&raw) else { return Vec::new() };
    list.iter()
        .filter_map(|scope| match scope {
            Json::String(name) => Some(name.clone()),
            Json::Object(entity) => entity.get("id").and_then(Json::as_str).map(String::from),
            _ => None,
        })
        .collect()
}

/// Scopes as the PHP side writes them: `json_encode` of a list of names, which
/// escapes `/` — matched here so a row written by either side is byte-equal.
fn scopes_json(scopes: &[String]) -> String {
    serde_json::to_string(scopes).unwrap_or_else(|_| "[]".into()).replace('/', "\\/")
}

/// A user id bound as an integer when it is one — the column is an integer in
/// the usual schema, and Postgres will not compare an integer column to text.
fn user_id_value(user_id: Option<&str>) -> Value {
    match user_id {
        None => Value::BigInt(None),
        Some(id) => match id.parse::<i64>() {
            Ok(number) => Value::BigInt(Some(number)),
            Err(_) => Value::String(Some(id.to_string())),
        },
    }
}

#[cfg(test)]
mod tests;
