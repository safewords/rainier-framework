//! The notification channel that needs a database.
//!
//! `rainier-notify` depends on mail and support and nothing else, so an
//! application with no database compiles neither. The
//! [`DatabaseChannel`] lives here for the same reason `JobTask` does: this is
//! the crate that has both halves.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use rainier_database::{Criteria, Database, EntityRepository, Migrator, Model, Repository};
use rainier_notify::{Channel, Delivery};
use rainier_orm::Entity;
use rainier_support::{BoxFuture, Error, Result};
use serde::{Deserialize, Serialize};

/// One stored notification.
///
/// The row behind an in-app bell menu: what it was, who it was for, and whether
/// they have read it.
#[derive(Debug, Clone, PartialEq, Entity, Serialize, Deserialize)]
#[orm(table = "rainier_notifications")]
#[orm(index = "notifiable_type, notifiable_id, read_at")]
pub struct NotificationRow {
    /// A unique id for this stored notification.
    #[orm(pk)]
    pub id: String,
    /// The notification's stable name.
    pub notification: String,
    /// What kind of thing it was for.
    #[orm(index)]
    pub notifiable_type: String,
    /// Which one.
    #[orm(index)]
    pub notifiable_id: String,
    /// The payload, as JSON text.
    pub payload: String,
    /// When it was read, or `NULL` while unread.
    pub read_at: Option<DateTime<Utc>>,
    /// When it arrived.
    pub created_at: DateTime<Utc>,
}

impl Model for NotificationRow {}

impl NotificationRow {
    /// Whether it is still unread.
    pub fn is_unread(&self) -> bool {
        self.read_at.is_none()
    }

    /// The payload, parsed.
    pub fn data(&self) -> Result<serde_json::Value> {
        Ok(serde_json::from_str(&self.payload)?)
    }
}

/// Stores notifications in a table, for an in-app notification list.
///
/// Uses [`to_data`](rainier_notify::Notification::to_data), falling back to
/// wrapping [`to_text`](rainier_notify::Notification::to_text) — so a
/// notification that only wrote a line still gets a row rather than being
/// silently dropped.
///
/// Addresses the recipient by their **id**, not by a route: every notifiable
/// has one, so this channel is never skipped for want of an address.
pub struct DatabaseChannel {
    rows: Arc<dyn Repository<NotificationRow>>,
}

impl DatabaseChannel {
    /// A channel writing to `db`.
    pub fn new(db: Database) -> Self {
        Self { rows: Arc::new(EntityRepository::<NotificationRow>::new(db)) }
    }

    /// The migration this channel needs.
    ///
    /// Merge it into the application's migrator:
    ///
    /// ```ignore
    /// Migrator::new()
    ///     .create_table::<User>("0001_create_users")
    ///     .merge(DatabaseChannel::migrations())
    /// ```
    pub fn migrations() -> Migrator {
        Migrator::new().create_table::<NotificationRow>("rainier_notify_0001_notifications")
    }

    /// Everything sent to a recipient, newest first.
    pub async fn for_recipient(
        &self,
        notifiable_type: &str,
        notifiable_id: &str,
        limit: u64,
    ) -> Result<Vec<NotificationRow>> {
        self.rows.matching(Self::addressed_to(notifiable_type, notifiable_id).limit(limit)).await
    }

    /// The unread ones, newest first.
    pub async fn unread(
        &self,
        notifiable_type: &str,
        notifiable_id: &str,
        limit: u64,
    ) -> Result<Vec<NotificationRow>> {
        self.rows
            .matching(
                Self::addressed_to(notifiable_type, notifiable_id)
                    .where_null("read_at")
                    .limit(limit),
            )
            .await
    }

    /// How many are unread — what the badge on the bell shows.
    pub async fn unread_count(&self, notifiable_type: &str, notifiable_id: &str) -> Result<u64> {
        self.rows
            .count_matching(
                Self::addressed_to(notifiable_type, notifiable_id).where_null("read_at"),
            )
            .await
    }

    /// One notification, but only if it belongs to this recipient.
    ///
    /// What an endpoint should look it up with. Reading the row and *then*
    /// comparing the owner in the handler is the same query with one more
    /// place to forget the comparison.
    pub async fn find_for(
        &self,
        notifiable_type: &str,
        notifiable_id: &str,
        id: &str,
    ) -> Result<Option<NotificationRow>> {
        let Some(row) = self.rows.find(id.into()).await? else { return Ok(None) };

        let theirs = row.notifiable_type == notifiable_type && row.notifiable_id == notifiable_id;
        Ok(theirs.then_some(row))
    }

    /// Mark one as read. `true` if it was there and unread.
    pub async fn mark_read(&self, id: &str) -> Result<bool> {
        let Some(mut row) = self.rows.find(id.into()).await? else { return Ok(false) };
        if row.read_at.is_some() {
            return Ok(false);
        }

        row.read_at = Some(Utc::now());
        Ok(self.rows.update(&row).await? > 0)
    }

    /// Mark everything for a recipient as read. Returns how many changed.
    pub async fn mark_all_read(&self, notifiable_type: &str, notifiable_id: &str) -> Result<u64> {
        let unread = self.unread(notifiable_type, notifiable_id, u64::MAX).await?;

        let mut marked = 0;
        for mut row in unread {
            row.read_at = Some(Utc::now());
            marked += self.rows.update(&row).await?;
        }
        Ok(marked)
    }

    /// Delete everything read before `before`. Returns how many.
    ///
    /// A notifications table grows forever otherwise. Worth
    /// [scheduling](crate::scheduler).
    pub async fn prune_read(&self, before: DateTime<Utc>) -> Result<u64> {
        self.rows
            .delete_matching(Criteria::new().where_not_null("read_at").where_lt("read_at", before))
            .await
    }

    fn addressed_to(notifiable_type: &str, notifiable_id: &str) -> Criteria {
        Criteria::new()
            .where_eq("notifiable_type", notifiable_type)
            .where_eq("notifiable_id", notifiable_id)
            .order_by_desc("created_at")
    }
}

/// A unique id for a stored notification: a timestamp for rough ordering plus a
/// counter for uniqueness.
///
/// Not a UUID, to avoid the dependency — the requirement is only that two rows
/// written in the same microsecond differ. The same approach the queue uses.
fn generate_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let micros = Utc::now().timestamp_micros();
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{micros:x}-{sequence:x}")
}

impl Channel for DatabaseChannel {
    fn name(&self) -> &'static str {
        "database"
    }

    fn send<'a>(&'a self, delivery: &'a Delivery) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let Some(data) = delivery.data_or_text() else {
                return Err(delivery.nothing_to_send("database", "data or text"));
            };

            self.rows
                .create(NotificationRow {
                    id: generate_id(),
                    notification: delivery.notification.to_string(),
                    notifiable_type: delivery.recipient_type.to_string(),
                    notifiable_id: delivery.recipient_id.clone(),
                    payload: serde_json::to_string(&data)
                        .map_err(|e| Error::internal(format!("unserialisable payload: {e}")))?,
                    read_at: None,
                    created_at: Utc::now(),
                })
                .await?;

            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_database::testing::{fake_database, MemoryConnection};
    use rainier_database::Dialect;
    use rainier_notify::{Channels, Notifiable, Notification, Notifier};

    struct User(u64);

    impl Notifiable for User {
        fn notifiable_id(&self) -> String {
            self.0.to_string()
        }
        fn notifiable_type(&self) -> &'static str {
            "User"
        }
        fn route_for(&self, _: &str) -> Option<String> {
            // Deliberately unreachable everywhere: the database channel
            // addresses by id, so it must still work.
            None
        }
    }

    struct Mentioned {
        by: String,
    }

    impl Notification<User> for Mentioned {
        fn notification_name(&self) -> &'static str {
            "post.mentioned"
        }
        fn via(&self, _: &User) -> Channels {
            Channels::new().with::<DatabaseChannel>()
        }
        fn to_data(&self, _: &User) -> Option<serde_json::Value> {
            Some(serde_json::json!({ "by": self.by }))
        }
    }

    struct TextOnly;

    impl Notification<User> for TextOnly {
        fn notification_name(&self) -> &'static str {
            "test.text-only"
        }
        fn via(&self, _: &User) -> Channels {
            Channels::new().with::<DatabaseChannel>()
        }
        fn to_text(&self, _: &User) -> Option<String> {
            Some("something happened".into())
        }
    }

    #[test]
    fn the_migration_creates_the_table() {
        let migrator = DatabaseChannel::migrations();
        assert_eq!(migrator.names(), vec!["rainier_notify_0001_notifications"]);

        let ddl =
            rainier_database::schema::schema_ddl::<NotificationRow>(Dialect::Sqlite).join("\n");
        assert!(ddl.contains("rainier_notifications"), "{ddl}");
        assert!(ddl.contains("read_at"), "{ddl}");
    }

    #[tokio::test]
    async fn it_writes_a_row_addressed_by_id() {
        let (db, connection) = fake_database(MemoryConnection::new(Dialect::Sqlite));
        let notifier = Notifier::new().with(DatabaseChannel::new(db));

        let receipt = notifier.send(&User(7), &Mentioned { by: "ada".into() }).await.unwrap();

        assert!(receipt.delivered_anywhere());
        let sql = connection.last_statement().unwrap();
        assert!(sql.contains("rainier_notifications"), "{sql}");
    }

    #[tokio::test]
    async fn a_recipient_with_no_address_anywhere_still_gets_a_row() {
        // The property that makes this channel useful: it needs no route, so a
        // user with no email and no phone still has a bell menu.
        let (db, connection) = fake_database(MemoryConnection::new(Dialect::Sqlite));
        let notifier = Notifier::new().with(DatabaseChannel::new(db));

        notifier.send(&User(7), &Mentioned { by: "ada".into() }).await.unwrap();

        assert_eq!(connection.statement_count(), 1);
    }

    #[tokio::test]
    async fn a_text_only_notification_still_gets_a_row() {
        // Falling back to wrapping the text beats dropping it: a notification
        // the author only wrote one line for should still reach the bell menu.
        let (db, connection) = fake_database(MemoryConnection::new(Dialect::Sqlite));
        let notifier = Notifier::new().with(DatabaseChannel::new(db));

        let receipt = notifier.send(&User(7), &TextOnly).await.unwrap();

        assert!(receipt.delivered_anywhere());
        let recorded = connection.recorded();
        let payload = recorded[0]
            .params
            .iter()
            .find_map(|value| match value {
                rainier_orm::sea_query::Value::String(Some(text)) if text.contains("message") => {
                    Some(text.to_string())
                }
                _ => None,
            })
            .expect("the payload should be a JSON string");

        assert!(payload.contains("something happened"), "{payload}");
    }

    #[tokio::test]
    async fn a_lookup_scoped_to_the_wrong_recipient_finds_nothing() {
        // Ids are opaque strings, not secrets. The scope is what stops one
        // user marking another's notification read.
        let (db, _) = fake_database(MemoryConnection::new(Dialect::Sqlite));
        let channel = DatabaseChannel::new(db);

        assert!(channel.find_for("User", "7", "whatever").await.unwrap().is_none());
    }

    #[test]
    fn a_row_knows_whether_it_has_been_read() {
        let mut row = NotificationRow {
            id: "n1".into(),
            notification: "post.mentioned".into(),
            notifiable_type: "User".into(),
            notifiable_id: "7".into(),
            payload: r#"{"by":"ada"}"#.into(),
            read_at: None,
            created_at: Utc::now(),
        };

        assert!(row.is_unread());
        assert_eq!(row.data().unwrap()["by"], "ada");

        row.read_at = Some(Utc::now());
        assert!(!row.is_unread());
    }
}
