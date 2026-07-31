//! Column metadata + the `SqlType` mapping from a Rust type to a column
//! SQL type. Adding a new supported field type is a single `SqlType` impl.

/// The SQL column type, dialect-independent. Rendered per-dialect in
/// [`crate::schema`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnType {
    Bool,
    Int,
    BigInt,
    Uint,
    BigUint,
    Double,
    Text,
    Binary,
    Timestamp,
    Date,
}

/// One column's metadata, produced by `#[derive(Entity)]` as a `const`.
#[derive(Debug, Clone, Copy)]
pub struct ColumnSpec {
    pub name: &'static str,
    pub ty: ColumnType,
    pub nullable: bool,
    pub pk: bool,
    pub auto_increment: bool,
    /// A single-column `UNIQUE` constraint, rendered inline on the column.
    /// Composite uniqueness is an [`IndexSpec`] with `unique = true` instead.
    pub unique: bool,
}

/// A secondary index over one or more columns, produced as a separate
/// `CREATE [UNIQUE] INDEX`. Single-column indexes come from `#[orm(index)]`
/// on a field; composite ones from `#[orm(index = "a, b")]` /
/// `#[orm(unique = "a, b")]` on the struct.
#[derive(Debug, Clone, Copy)]
pub struct IndexSpec {
    pub name: &'static str,
    pub columns: &'static [&'static str],
    pub unique: bool,
}

/// A foreign-key constraint. The local columns reference
/// `foreign_table(foreign_columns)`. Generated from
/// `#[orm(references = "table(col)")]` on a field.
///
/// This is a *constraint* (database-enforced referential integrity within one
/// database), not an ORM relationship: there is no lazy/eager navigation
/// attached to it. See the crate docs on why relationships stay explicit.
#[derive(Debug, Clone, Copy)]
pub struct ForeignKeySpec {
    pub name: &'static str,
    pub columns: &'static [&'static str],
    pub foreign_table: &'static str,
    pub foreign_columns: &'static [&'static str],
    pub on_delete: Option<RefAction>,
    pub on_update: Option<RefAction>,
}

/// The referential action for a foreign key's `ON DELETE` / `ON UPDATE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefAction {
    Cascade,
    Restrict,
    SetNull,
    SetDefault,
    NoAction,
}

/// Maps a Rust field type to its column type. The derive reads
/// `<FieldType as SqlType>::{COLUMN_TYPE, NULLABLE}`, so `Option<T>` becomes
/// a nullable `T` automatically and new types need only this impl.
pub trait SqlType {
    const COLUMN_TYPE: ColumnType;
    const NULLABLE: bool = false;
}

macro_rules! sql_type {
    ($t:ty, $ct:expr) => {
        impl SqlType for $t {
            const COLUMN_TYPE: ColumnType = $ct;
        }
    };
}

sql_type!(bool, ColumnType::Bool);
sql_type!(i32, ColumnType::Int);
sql_type!(i64, ColumnType::BigInt);
sql_type!(u32, ColumnType::Uint);
sql_type!(u64, ColumnType::BigUint);
sql_type!(f64, ColumnType::Double);
sql_type!(String, ColumnType::Text);
sql_type!(Vec<u8>, ColumnType::Binary);
sql_type!(chrono::DateTime<chrono::Utc>, ColumnType::Timestamp);
sql_type!(chrono::NaiveDate, ColumnType::Date);

impl<T: SqlType> SqlType for Option<T> {
    const COLUMN_TYPE: ColumnType = T::COLUMN_TYPE;
    const NULLABLE: bool = true;
}
