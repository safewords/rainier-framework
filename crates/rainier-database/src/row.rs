//! Owned row snapshots — [`OwnedRow`], [`Cell`] and [`ColumnRequest`].
//!
//! Rainier ORM hands back `Box<dyn Row>`, and `Row` carries no `Send` bound —
//! correctly, since a driver's row may borrow from a `!Send` connection. But a
//! value that is not `Send` cannot be the output of a `Send` future, so a row
//! in that form can never leave the request path's thread.
//!
//! So the connection port copies each row into an [`OwnedRow`] before returning
//! it: a plain map of name → [`Cell`], which is `Send`, and which implements
//! Rainier ORM's own [`Row`] trait so the derived `Entity::from_row` decoder
//! reads it unchanged. Decoding still belongs entirely to the ORM; only the
//! transport of the raw values is ours.
//!
//! Reading a row requires knowing which columns to ask for, because `Row` is
//! addressed by name and type rather than enumerable — hence
//! [`ColumnRequest`], which the caller builds from `E::columns()`.

use std::collections::HashMap;

use chrono::{DateTime, NaiveDate, Utc};
use rainier_orm::{ColumnType, Row};

/// A column to read out of a driver row, and the type to read it as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnRequest {
    /// The column's name.
    pub name: String,
    /// The type its getter should use.
    pub ty: ColumnType,
}

impl ColumnRequest {
    /// Request `name` as `ty`.
    pub fn new(name: impl Into<String>, ty: ColumnType) -> Self {
        Self { name: name.into(), ty }
    }

    /// Every column of an entity, in declaration order — what a `SELECT *`
    /// of that entity should be read back as.
    pub fn for_entity<E: rainier_orm::Entity>() -> Vec<ColumnRequest> {
        E::columns().iter().map(|column| ColumnRequest::new(column.name, column.ty)).collect()
    }
}

/// One cell's value.
#[derive(Debug, Clone, PartialEq)]
pub enum Cell {
    /// SQL `NULL`, or a column the driver did not return.
    Null,
    /// A boolean.
    Bool(bool),
    /// A signed integer.
    Int(i64),
    /// An unsigned integer.
    Uint(u64),
    /// A floating-point number.
    Float(f64),
    /// Text.
    Text(String),
    /// Binary.
    Bytes(Vec<u8>),
    /// A UTC timestamp.
    Timestamp(DateTime<Utc>),
    /// A calendar date.
    Date(NaiveDate),
}

impl Cell {
    /// The cell as an unsigned count, for a `COUNT(*)` column.
    ///
    /// A driver may report the count as signed or unsigned depending on the
    /// engine; a negative one is impossible, so it clamps rather than failing.
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Cell::Int(value) => Some((*value).max(0) as u64),
            Cell::Uint(value) => Some(*value),
            Cell::Float(value) => Some(value.max(0.0) as u64),
            Cell::Text(value) => value.parse().ok(),
            _ => None,
        }
    }

    /// The cell as a bindable [`Value`](rainier_orm::sea_query::Value), for
    /// feeding one query's output into the next — reading a pivot, then the
    /// rows it points at.
    ///
    /// `Null` is `None`: a NULL key links nothing, and binding it would make
    /// `IN (NULL)` match no row while looking like it might.
    pub fn to_value(&self) -> Option<rainier_orm::sea_query::Value> {
        use rainier_orm::sea_query::Value as V;
        match self {
            Cell::Null => None,
            Cell::Bool(value) => Some(V::Bool(Some(*value))),
            Cell::Int(value) => Some(V::BigInt(Some(*value))),
            Cell::Uint(value) => Some(V::BigUnsigned(Some(*value))),
            Cell::Float(value) => Some(V::Double(Some(*value))),
            Cell::Text(value) => Some(V::String(Some(Box::new(value.clone())))),
            Cell::Bytes(value) => Some(V::Bytes(Some(Box::new(value.clone())))),
            Cell::Timestamp(value) => Some(V::ChronoDateTimeUtc(Some(Box::new(*value)))),
            Cell::Date(value) => Some(V::ChronoDate(Some(Box::new(*value)))),
        }
    }
}

macro_rules! cell_from {
    ($ty:ty, $variant:ident) => {
        impl From<$ty> for Cell {
            fn from(value: $ty) -> Self {
                Cell::$variant(value)
            }
        }
    };
    ($ty:ty, $variant:ident, as $cast:ty) => {
        impl From<$ty> for Cell {
            fn from(value: $ty) -> Self {
                Cell::$variant(value as $cast)
            }
        }
    };
}

cell_from!(bool, Bool);
cell_from!(i32, Int, as i64);
cell_from!(i64, Int);
cell_from!(u32, Uint, as u64);
cell_from!(u64, Uint);
cell_from!(f64, Float);
cell_from!(String, Text);
cell_from!(Vec<u8>, Bytes);
cell_from!(DateTime<Utc>, Timestamp);
cell_from!(NaiveDate, Date);

impl From<&str> for Cell {
    fn from(value: &str) -> Self {
        Cell::Text(value.to_string())
    }
}

impl<T: Into<Cell>> From<Option<T>> for Cell {
    fn from(value: Option<T>) -> Self {
        value.map(Into::into).unwrap_or(Cell::Null)
    }
}

/// A row copied out of a driver, addressable by column name.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OwnedRow {
    columns: HashMap<String, Cell>,
}

impl OwnedRow {
    /// An empty row.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set a column.
    pub fn with(mut self, name: impl Into<String>, value: impl Into<Cell>) -> Self {
        self.columns.insert(name.into(), value.into());
        self
    }

    /// Set a column in place.
    pub fn set(&mut self, name: impl Into<String>, value: impl Into<Cell>) {
        self.columns.insert(name.into(), value.into());
    }

    /// Copy the requested columns out of a driver row.
    ///
    /// Each column is read through the getter its [`ColumnType`] names, which
    /// is the same dispatch Rainier ORM's own decoder performs — so a value that
    /// round-trips through here decodes identically to one read directly.
    pub fn snapshot(row: &dyn Row, columns: &[ColumnRequest]) -> rainier_orm::Result<Self> {
        let mut owned = OwnedRow::new();
        for request in columns {
            let name = request.name.as_str();
            let cell = match request.ty {
                ColumnType::Bool => Cell::from(row.get_bool(name)?),
                ColumnType::Int => Cell::from(row.get_i32(name)?),
                ColumnType::BigInt => Cell::from(row.get_i64(name)?),
                ColumnType::Uint => Cell::from(row.get_u32(name)?),
                ColumnType::BigUint => Cell::from(row.get_u64(name)?),
                ColumnType::Double => Cell::from(row.get_f64(name)?),
                ColumnType::Text => Cell::from(row.get_string(name)?),
                ColumnType::Binary => Cell::from(row.get_bytes(name)?),
                ColumnType::Timestamp => Cell::from(row.get_datetime(name)?),
                ColumnType::Date => Cell::from(row.get_naive_date(name)?),
            };
            owned.set(request.name.clone(), cell);
        }
        Ok(owned)
    }

    /// Whether the row holds no columns.
    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }

    /// How many columns the row holds.
    pub fn len(&self) -> usize {
        self.columns.len()
    }

    /// The raw cell at `name`, if it was read.
    pub fn cell(&self, name: &str) -> Option<&Cell> {
        self.columns.get(name)
    }

    /// The cell at `name`, treating `Null` and "absent" alike — the two are
    /// indistinguishable to a caller asking for a value.
    fn value(&self, name: &str) -> Option<&Cell> {
        match self.columns.get(name) {
            Some(Cell::Null) | None => None,
            Some(cell) => Some(cell),
        }
    }
}

fn wrong_type<T>(column: &str, cell: &Cell, wanted: &str) -> rainier_orm::Result<T> {
    Err(anyhow::anyhow!("column `{column}` holds {cell:?}, which is not {wanted}"))
}

impl Row for OwnedRow {
    fn get_bool(&self, col: &str) -> rainier_orm::Result<Option<bool>> {
        match self.value(col) {
            None => Ok(None),
            Some(Cell::Bool(value)) => Ok(Some(*value)),
            // SQLite and MySQL both store booleans as integers.
            Some(Cell::Int(value)) => Ok(Some(*value != 0)),
            Some(Cell::Uint(value)) => Ok(Some(*value != 0)),
            Some(other) => wrong_type(col, other, "a boolean"),
        }
    }

    fn get_i32(&self, col: &str) -> rainier_orm::Result<Option<i32>> {
        Ok(self.get_i64(col)?.map(|value| value as i32))
    }

    fn get_i64(&self, col: &str) -> rainier_orm::Result<Option<i64>> {
        match self.value(col) {
            None => Ok(None),
            Some(Cell::Int(value)) => Ok(Some(*value)),
            Some(Cell::Uint(value)) => Ok(Some(*value as i64)),
            Some(Cell::Bool(value)) => Ok(Some(*value as i64)),
            Some(other) => wrong_type(col, other, "an integer"),
        }
    }

    fn get_u32(&self, col: &str) -> rainier_orm::Result<Option<u32>> {
        Ok(self.get_u64(col)?.map(|value| value as u32))
    }

    fn get_u64(&self, col: &str) -> rainier_orm::Result<Option<u64>> {
        match self.value(col) {
            None => Ok(None),
            Some(Cell::Uint(value)) => Ok(Some(*value)),
            // SQLite and Postgres have no unsigned types, so an unsigned
            // column comes back signed and is cast — the same accommodation
            // Rainier ORM makes in its own drivers.
            Some(Cell::Int(value)) => Ok(Some(*value as u64)),
            Some(Cell::Bool(value)) => Ok(Some(*value as u64)),
            Some(other) => wrong_type(col, other, "an unsigned integer"),
        }
    }

    fn get_f64(&self, col: &str) -> rainier_orm::Result<Option<f64>> {
        match self.value(col) {
            None => Ok(None),
            Some(Cell::Float(value)) => Ok(Some(*value)),
            Some(Cell::Int(value)) => Ok(Some(*value as f64)),
            Some(Cell::Uint(value)) => Ok(Some(*value as f64)),
            Some(other) => wrong_type(col, other, "a number"),
        }
    }

    fn get_string(&self, col: &str) -> rainier_orm::Result<Option<String>> {
        match self.value(col) {
            None => Ok(None),
            Some(Cell::Text(value)) => Ok(Some(value.clone())),
            Some(other) => wrong_type(col, other, "text"),
        }
    }

    fn get_bytes(&self, col: &str) -> rainier_orm::Result<Option<Vec<u8>>> {
        match self.value(col) {
            None => Ok(None),
            Some(Cell::Bytes(value)) => Ok(Some(value.clone())),
            Some(Cell::Text(value)) => Ok(Some(value.as_bytes().to_vec())),
            Some(other) => wrong_type(col, other, "bytes"),
        }
    }

    fn get_datetime(&self, col: &str) -> rainier_orm::Result<Option<DateTime<Utc>>> {
        match self.value(col) {
            None => Ok(None),
            Some(Cell::Timestamp(value)) => Ok(Some(*value)),
            // SQLite stores timestamps as text.
            Some(Cell::Text(value)) => value
                .parse::<DateTime<Utc>>()
                .map(Some)
                .map_err(|e| anyhow::anyhow!("column `{col}` is not a timestamp: {e}")),
            Some(other) => wrong_type(col, other, "a timestamp"),
        }
    }

    fn get_naive_date(&self, col: &str) -> rainier_orm::Result<Option<NaiveDate>> {
        match self.value(col) {
            None => Ok(None),
            Some(Cell::Date(value)) => Ok(Some(*value)),
            Some(Cell::Text(value)) => NaiveDate::parse_from_str(value, "%Y-%m-%d")
                .map(Some)
                .map_err(|e| anyhow::anyhow!("column `{col}` is not a date: {e}")),
            Some(other) => wrong_type(col, other, "a date"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_orm::Entity as _;

    #[derive(rainier_orm::Entity, Clone, Debug, PartialEq)]
    #[orm(table = "posts")]
    struct Post {
        #[orm(pk, auto_increment)]
        id: u64,
        title: String,
        views: Option<i64>,
    }

    #[test]
    fn an_owned_row_is_send() {
        // The whole reason this type exists.
        fn assert_send<T: Send>() {}
        assert_send::<OwnedRow>();
        assert_send::<Vec<OwnedRow>>();
    }

    #[test]
    fn reads_back_every_cell_type() {
        let row = OwnedRow::new()
            .with("b", true)
            .with("i", 7_i64)
            .with("u", 9_u64)
            .with("f", 1.5_f64)
            .with("s", "text")
            .with("bytes", vec![1_u8, 2]);

        assert_eq!(row.get_bool("b").unwrap(), Some(true));
        assert_eq!(row.get_i64("i").unwrap(), Some(7));
        assert_eq!(row.get_u64("u").unwrap(), Some(9));
        assert_eq!(row.get_f64("f").unwrap(), Some(1.5));
        assert_eq!(row.get_string("s").unwrap().as_deref(), Some("text"));
        assert_eq!(row.get_bytes("bytes").unwrap(), Some(vec![1, 2]));
    }

    #[test]
    fn null_and_absent_both_read_as_none() {
        let row = OwnedRow::new().with("nickname", None::<String>);
        assert_eq!(row.get_string("nickname").unwrap(), None);
        assert_eq!(row.get_string("never_selected").unwrap(), None);
    }

    #[test]
    fn integers_widen_across_the_signedness_the_dialects_disagree_about() {
        // SQLite and Postgres have no unsigned types, so an unsigned column
        // arrives signed.
        let row = OwnedRow::new().with("count", 5_i64);
        assert_eq!(row.get_u64("count").unwrap(), Some(5));

        let row = OwnedRow::new().with("count", 5_u64);
        assert_eq!(row.get_i64("count").unwrap(), Some(5));
    }

    #[test]
    fn booleans_read_from_the_integers_sqlite_stores() {
        assert_eq!(OwnedRow::new().with("f", 0_i64).get_bool("f").unwrap(), Some(false));
        assert_eq!(OwnedRow::new().with("f", 1_i64).get_bool("f").unwrap(), Some(true));
    }

    #[test]
    fn a_type_mismatch_is_an_error_rather_than_a_silent_default() {
        let row = OwnedRow::new().with("title", "not a number");
        let err = row.get_i64("title").unwrap_err();
        assert!(err.to_string().contains("title"), "{err}");
    }

    #[test]
    fn timestamps_parse_from_the_text_sqlite_stores() {
        let row = OwnedRow::new().with("at", "2026-07-25T10:30:00Z");
        assert!(row.get_datetime("at").unwrap().is_some());

        let row = OwnedRow::new().with("on", "2026-07-25");
        assert_eq!(row.get_naive_date("on").unwrap(), NaiveDate::from_ymd_opt(2026, 7, 25));
        assert!(OwnedRow::new().with("at", "not a date").get_datetime("at").is_err());
    }

    #[test]
    fn column_requests_come_from_the_entity_metadata() {
        let requested = ColumnRequest::for_entity::<Post>();
        assert_eq!(requested.len(), 3);
        assert_eq!(requested[0], ColumnRequest::new("id", ColumnType::BigUint));
        assert_eq!(requested[1].name, "title");
        assert_eq!(requested[2].ty, ColumnType::BigInt, "Option<i64> is still a BigInt column");
    }

    #[test]
    fn a_snapshot_decodes_back_into_its_entity() {
        // The round trip that matters: driver row → OwnedRow → Entity.
        let driver_row =
            OwnedRow::new().with("id", 7_u64).with("title", "Hello").with("views", None::<i64>);

        let snapshot =
            OwnedRow::snapshot(&driver_row, &ColumnRequest::for_entity::<Post>()).unwrap();
        let post = Post::from_row(&snapshot).unwrap();

        assert_eq!(post, Post { id: 7, title: "Hello".into(), views: None });
    }

    #[test]
    fn a_snapshot_only_copies_what_was_requested() {
        let driver_row = OwnedRow::new().with("id", 1_u64).with("secret", "hidden");
        let snapshot =
            OwnedRow::snapshot(&driver_row, &[ColumnRequest::new("id", ColumnType::BigUint)])
                .unwrap();

        assert_eq!(snapshot.len(), 1);
        assert!(snapshot.cell("secret").is_none());
    }

    #[test]
    fn snapshotting_a_missing_column_records_a_null_rather_than_failing() {
        let driver_row = OwnedRow::new().with("id", 1_u64);
        let snapshot =
            OwnedRow::snapshot(&driver_row, &[ColumnRequest::new("absent", ColumnType::Text)])
                .unwrap();

        assert_eq!(snapshot.cell("absent"), Some(&Cell::Null));
    }
}
