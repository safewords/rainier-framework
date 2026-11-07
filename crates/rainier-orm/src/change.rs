//! What a write changed: [`RowChange`].
//!
//! Every write this crate and `rainier-database` build — an insert, an upsert,
//! an update by key or by criteria, a delete, a counter `SET n = n + 1` —
//! describes the rows it touched as a `RowChange` and hands it to its
//! executor ([`Executor::rows_changed`](crate::Executor::rows_changed)) once the
//! statement has succeeded. An executor that keeps subscribers —
//! `rainier_database::Database::on_change` — passes it on.
//!
//! That is the one place a cache of rows can learn its copy is stale **whatever
//! wrote it**. Lifecycle hooks (`Updated<M>` and friends) fire only for a
//! repository built `with_events`, and not at all for a bulk update; a write
//! the cache never hears about is a stale read nobody notices.
//!
//! # Keys, when they are known
//!
//! [`ChangedKeys::Keys`] lists the primary keys a write touched when the write
//! names them: an entity written by key, an update or delete whose conditions
//! pin the primary key (`id = 7`, `id IN (7, 9)`). Otherwise it is
//! [`ChangedKeys::Unknown`] — some rows of the table, which ones not said — and
//! a subscriber that cannot tell must assume any of them. Keys are only
//! reported for a single-column primary key, as strings.
//!
//! # What is not seen
//!
//! SQL that does not go through a builder: `Executor::execute` with a string,
//! `Database::statement`, a raw `upsert_select`'s source. Those report nothing
//! — route a write through a builder if something is watching its table.

use crate::Entity;
use sea_query::Value;

/// What kind of write it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    /// New rows. A cache of existing rows has nothing to drop.
    Insert,
    /// Existing rows, rewritten in whole or in part.
    Update,
    /// Rows inserted or, on a conflict, rewritten.
    Upsert,
    /// Rows removed.
    Delete,
}

/// Which rows a write touched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangedKeys {
    /// Exactly these primary keys (and possibly fewer: a key named but not
    /// matched is still listed).
    Keys(Vec<String>),
    /// Some rows of the table; which ones the write did not say.
    Unknown,
}

/// A successful write, as subscribers see it. See the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowChange {
    /// The table written.
    pub table: String,
    pub kind: ChangeKind,
    pub keys: ChangedKeys,
}

impl RowChange {
    /// A write to `table` touching rows it does not name.
    pub fn unknown(table: impl Into<String>, kind: ChangeKind) -> Self {
        Self { table: table.into(), kind, keys: ChangedKeys::Unknown }
    }

    /// A write to `table` touching `keys`.
    pub fn keys(table: impl Into<String>, kind: ChangeKind, keys: Vec<String>) -> Self {
        Self { table: table.into(), kind, keys: ChangedKeys::Keys(keys) }
    }

    /// A write of `rows` of `E`, by their own primary keys. Unknown when `E`'s
    /// key is not one column, or a row's key is unset (an auto-increment key
    /// the insert leaves to the database).
    pub fn of_rows<'a, E: Entity + 'a>(
        kind: ChangeKind,
        rows: impl IntoIterator<Item = &'a E>,
    ) -> Self {
        if E::primary_key_columns().len() != 1 {
            return Self::unknown(E::table(), kind);
        }
        let mut keys = Vec::new();
        for row in rows {
            match row.pk_values().first().and_then(key_string) {
                Some(key) => keys.push(key),
                None => return Self::unknown(E::table(), kind),
            }
        }
        Self::keys(E::table(), kind, keys)
    }

    /// A write of `E` restricted to `keys` (primary-key values), or unknown
    /// when there is no restriction or `E`'s key is not one column.
    pub fn of_keys<E: Entity>(kind: ChangeKind, keys: Option<&[Value]>) -> Self {
        match keys {
            Some(keys) if E::primary_key_columns().len() == 1 => {
                let strings: Option<Vec<String>> = keys.iter().map(key_string).collect();
                match strings {
                    Some(keys) => Self::keys(E::table(), kind, keys),
                    None => Self::unknown(E::table(), kind),
                }
            }
            _ => Self::unknown(E::table(), kind),
        }
    }

    /// Whether this change may have touched the row keyed `key` in `table`.
    pub fn may_touch(&self, table: &str, key: &str) -> bool {
        self.table == table
            && match &self.keys {
                ChangedKeys::Keys(keys) => keys.iter().any(|k| k == key),
                ChangedKeys::Unknown => true,
            }
    }
}

/// A primary-key value as a string: integers in decimal, strings as they are.
/// `None` for a null or unset key, and for types a key is never.
///
/// An auto-increment key the insert leaves to the database reads as `0` and is
/// `None` here: the row is not yet known by it.
pub fn key_string(value: &Value) -> Option<String> {
    match value {
        Value::TinyInt(Some(n)) => nonzero(i64::from(*n)),
        Value::SmallInt(Some(n)) => nonzero(i64::from(*n)),
        Value::Int(Some(n)) => nonzero(i64::from(*n)),
        Value::BigInt(Some(n)) => nonzero(*n),
        Value::TinyUnsigned(Some(n)) => nonzero_u(u64::from(*n)),
        Value::SmallUnsigned(Some(n)) => nonzero_u(u64::from(*n)),
        Value::Unsigned(Some(n)) => nonzero_u(u64::from(*n)),
        Value::BigUnsigned(Some(n)) => nonzero_u(*n),
        Value::String(Some(s)) => Some(s.to_string()),
        Value::Char(Some(c)) => Some(c.to_string()),
        #[allow(unreachable_patterns)]
        _ => None,
    }
}

fn nonzero(n: i64) -> Option<String> {
    (n != 0).then(|| n.to_string())
}

fn nonzero_u(n: u64) -> Option<String> {
    (n != 0).then(|| n.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_read_as_strings_and_an_unset_key_reads_as_none() {
        assert_eq!(key_string(&Value::BigUnsigned(Some(7))), Some("7".into()));
        assert_eq!(key_string(&Value::Int(Some(-3))), Some("-3".into()));
        assert_eq!(key_string(&Value::String(Some("ab".into()))), Some("ab".into()));
        assert_eq!(key_string(&Value::BigUnsigned(Some(0))), None);
        assert_eq!(key_string(&Value::BigUnsigned(None)), None);
    }

    #[test]
    fn a_change_touches_its_keys_or_every_row_when_it_does_not_say() {
        let keyed = RowChange::keys("posts", ChangeKind::Update, vec!["7".into()]);
        assert!(keyed.may_touch("posts", "7"));
        assert!(!keyed.may_touch("posts", "8"));
        assert!(!keyed.may_touch("profiles", "7"));

        let unknown = RowChange::unknown("posts", ChangeKind::Update);
        assert!(unknown.may_touch("posts", "8"));
    }
}
