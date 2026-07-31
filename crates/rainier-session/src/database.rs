//! [`DatabaseSessionStore`] — sessions on the database you already have.

use chrono::{DateTime, Duration, Utc};
use rainier_database::{Criteria, Database, EntityRepository, Migrator, Model, Repository};
use rainier_orm::Entity;
use rainier_support::{BoxFuture, Error, Result};

use crate::session::SessionData;
use crate::store::SessionStore;

/// A session row.
#[derive(Entity, Clone, Debug, PartialEq)]
#[orm(table = "rainier_sessions")]
pub struct SessionRow {
    /// The session id — the primary key, so a read is a point lookup.
    #[orm(pk)]
    pub id: String,

    /// The bag, as JSON.
    pub payload: String,

    /// When this row stops being valid.
    #[orm(index)]
    pub expires_at: DateTime<Utc>,

    /// When it was last written, for diagnostics.
    pub updated_at: DateTime<Utc>,
}

impl Model for SessionRow {}

/// Sessions in a table.
///
/// The right default for more than one instance: every process reads the same
/// rows, so a user stays logged in wherever the load balancer sends them.
pub struct DatabaseSessionStore {
    rows: EntityRepository<SessionRow>,
    lifetime: Duration,
}

impl DatabaseSessionStore {
    /// A store over `db`, with two-hour sessions.
    pub fn new(db: Database) -> Self {
        Self { rows: EntityRepository::new(db), lifetime: Duration::hours(2) }
    }

    /// Sessions expiring after `lifetime`.
    pub fn with_lifetime(mut self, lifetime: Duration) -> Self {
        self.lifetime = lifetime;
        self
    }

    /// The migration this driver needs.
    ///
    /// Merge it into the application's migrator:
    ///
    /// ```ignore
    /// Migrator::new()
    ///     .create_table::<User>("0001_create_users")
    ///     .merge(DatabaseSessionStore::migrations())
    /// ```
    pub fn migrations() -> Migrator {
        // The `down` is the drop that `create_table` implies. Rolling it back
        // logs everyone out, which is the correct meaning of undoing it.
        Migrator::new().create_table::<SessionRow>("rainier_session_0001_sessions")
    }

    /// Remove every expired row. Returns how many.
    pub async fn prune(&self) -> Result<u64> {
        self.rows.delete_matching(Criteria::new().where_lte("expires_at", Utc::now())).await
    }
}

impl SessionStore for DatabaseSessionStore {
    fn name(&self) -> &str {
        "database"
    }

    fn read<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<Option<SessionData>>> {
        Box::pin(async move {
            let Some(row) = self.rows.find(id.into()).await? else {
                return Ok(None);
            };

            if row.expires_at <= Utc::now() {
                // Expired: treat as absent and clear it out, so a stale row
                // cannot come back if the clock or the lifetime changes.
                self.rows.delete(id.into()).await?;
                return Ok(None);
            }

            match serde_json::from_str(&row.payload) {
                Ok(data) => Ok(Some(data)),
                Err(e) => {
                    // A row we cannot parse is a row from an older shape of
                    // the application. Starting a fresh session logs the user
                    // out; failing the request locks them out permanently,
                    // with no way to recover but clearing their cookies.
                    tracing::warn!(error = %e, "discarding an unreadable session row");
                    self.rows.delete(id.into()).await?;
                    Ok(None)
                }
            }
        })
    }

    fn write<'a>(&'a self, id: &'a str, data: &'a SessionData) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let now = Utc::now();
            let row = SessionRow {
                id: id.to_string(),
                payload: serde_json::to_string(data)
                    .map_err(|e| Error::internal(format!("the session will not serialise: {e}")))?,
                expires_at: now + self.lifetime,
                updated_at: now,
            };

            // Upsert rather than insert-or-update: a session is written on
            // every request, and two concurrent requests for one session
            // would otherwise race between the check and the insert.
            self.rows.upsert(&row, &["id"], &["payload", "expires_at", "updated_at"]).await?;
            Ok(())
        })
    }

    fn destroy<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.rows.delete(id.into()).await?;
            Ok(())
        })
    }

    fn gc(&self) -> BoxFuture<'_, Result<u64>> {
        Box::pin(async move { self.prune().await })
    }
}

impl std::fmt::Debug for DatabaseSessionStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DatabaseSessionStore").field("lifetime", &self.lifetime).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_database::testing::{fake_database, MemoryConnection};
    use rainier_database::Dialect;

    #[test]
    fn the_migration_names_the_table() {
        let migrator = DatabaseSessionStore::migrations();
        assert_eq!(migrator.names(), vec!["rainier_session_0001_sessions"]);

        let ddl = rainier_database::schema::schema_ddl::<SessionRow>(Dialect::Sqlite).join("\n");
        assert!(ddl.contains("rainier_sessions"), "{ddl}");
        assert!(ddl.contains("expires_at"), "{ddl}");
    }

    #[tokio::test]
    async fn writing_upserts_rather_than_inserting() {
        let (db, connection) = fake_database(MemoryConnection::new(Dialect::Sqlite));
        let store = DatabaseSessionStore::new(db);

        store.write("abc", &SessionData::default()).await.unwrap();

        let sql = connection.last_statement().unwrap().to_uppercase();
        assert!(
            sql.contains("CONFLICT") || sql.contains("DUPLICATE") || sql.contains("MERGE"),
            "two concurrent requests for one session must not race: {sql}"
        );
    }

    #[tokio::test]
    async fn an_unknown_session_reads_as_none() {
        let (db, _) = fake_database(MemoryConnection::new(Dialect::Sqlite));
        let store = DatabaseSessionStore::new(db);

        assert!(store.read("nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn pruning_targets_expired_rows() {
        let (db, connection) = fake_database(MemoryConnection::new(Dialect::Sqlite));
        let store = DatabaseSessionStore::new(db);

        store.prune().await.unwrap();

        let sql = connection.last_statement().unwrap();
        assert!(sql.to_uppercase().starts_with("DELETE"), "{sql}");
        assert!(sql.contains("expires_at"), "{sql}");
    }

    #[test]
    fn the_lifetime_is_configurable() {
        let (db, _) = fake_database(MemoryConnection::new(Dialect::Sqlite));
        let store = DatabaseSessionStore::new(db).with_lifetime(Duration::days(14));

        assert_eq!(store.lifetime, Duration::days(14));
    }
}
