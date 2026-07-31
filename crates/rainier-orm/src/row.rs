//! Row decoding (`Row` + `FromColumn`) and value encoding (`ToColumn`).
//!
//! An [`Executor`](crate::Executor) yields opaque `Box<dyn Row>` values; the
//! generated `Entity::from_row` reads each field through [`FromColumn`], which
//! dispatches to the typed getter on [`Row`]. Encoding for inserts/updates
//! goes the other way through [`ToColumn`], which lowers a field to a
//! `sea_query::Value`. Supporting a new field type is one `SqlType` impl plus
//! — if it isn't already `Into<sea_query::Value>` and readable from a getter —
//! a `FromColumn` impl.

use crate::{ColumnType, Error, Result, SqlType};
use chrono::{DateTime, NaiveDate, Utc};
use sea_query::Value;

/// A single result row, addressed by column name. Every getter returns
/// `Option` so a NULL is distinguishable from a missing/zero value; the
/// non-`Option` `FromColumn` impls turn a `None` into a decode error.
///
/// Implementors back this with whatever their driver hands back — a
/// `sea_orm::QueryResult`, a parsed D1 JSON object, etc.
pub trait Row {
    fn get_bool(&self, col: &str) -> Result<Option<bool>>;
    fn get_i32(&self, col: &str) -> Result<Option<i32>>;
    fn get_i64(&self, col: &str) -> Result<Option<i64>>;
    fn get_u32(&self, col: &str) -> Result<Option<u32>>;
    fn get_u64(&self, col: &str) -> Result<Option<u64>>;
    fn get_f64(&self, col: &str) -> Result<Option<f64>>;
    fn get_string(&self, col: &str) -> Result<Option<String>>;
    fn get_bytes(&self, col: &str) -> Result<Option<Vec<u8>>>;
    fn get_datetime(&self, col: &str) -> Result<Option<DateTime<Utc>>>;
    fn get_naive_date(&self, col: &str) -> Result<Option<NaiveDate>>;
}

/// Decodes one column of a [`Row`] into a Rust value. The generated decoder
/// calls `T::from_column(row, "col")?` per field. The bare `T` impls treat a
/// NULL as a decode error; the blanket `Option<T>` impl below turns it into
/// `None`.
pub trait FromColumn: Sized {
    fn from_column(row: &dyn Row, col: &str) -> Result<Self>;
}

macro_rules! from_column {
    ($t:ty, $getter:ident) => {
        impl FromColumn for $t {
            fn from_column(row: &dyn Row, col: &str) -> Result<Self> {
                row.$getter(col)?.ok_or_else(|| Error::msg(format!("column `{col}` was NULL")))
            }
        }
    };
}

from_column!(bool, get_bool);
from_column!(i32, get_i32);
from_column!(i64, get_i64);
from_column!(u32, get_u32);
from_column!(u64, get_u64);
from_column!(f64, get_f64);
from_column!(String, get_string);
from_column!(Vec<u8>, get_bytes);
from_column!(DateTime<Utc>, get_datetime);
from_column!(NaiveDate, get_naive_date);

/// Blanket nullability: any decodable, typed column is decodable as `Option`.
/// Living in this crate (not a per-type macro) keeps it coherent — a custom
/// type gets `Option` support for free from its bare `FromColumn` + `SqlType`,
/// without a downstream `impl … for Option<Custom>` (which the orphan rule
/// forbids). Presence is probed through the getter matching the type's
/// [`ColumnType`]; only a present cell is decoded.
impl<T: FromColumn + SqlType> FromColumn for Option<T> {
    fn from_column(row: &dyn Row, col: &str) -> Result<Self> {
        let present = match T::COLUMN_TYPE {
            ColumnType::Bool => row.get_bool(col)?.is_some(),
            ColumnType::Int => row.get_i32(col)?.is_some(),
            ColumnType::BigInt => row.get_i64(col)?.is_some(),
            ColumnType::Uint => row.get_u32(col)?.is_some(),
            ColumnType::BigUint => row.get_u64(col)?.is_some(),
            ColumnType::Double => row.get_f64(col)?.is_some(),
            ColumnType::Text => row.get_string(col)?.is_some(),
            ColumnType::Binary => row.get_bytes(col)?.is_some(),
            ColumnType::Timestamp => row.get_datetime(col)?.is_some(),
            ColumnType::Date => row.get_naive_date(col)?.is_some(),
        };
        if present {
            Ok(Some(T::from_column(row, col)?))
        } else {
            Ok(None)
        }
    }
}

/// Lowers a field value to a `sea_query::Value` for binding.
///
/// Impl'd explicitly (not via a blanket `impl<T: Into<Value>>`) for each
/// supported type and its `Option`. The blanket form is tempting but is a
/// coherence trap: it claims *every* `T`, so a downstream crate could never add
/// `ToColumn` for its own enum/newtype. Keeping these concrete leaves the trait
/// open — a custom column type adds one `ToColumn` impl (see
/// [`impl_string_column!`](crate::impl_string_column) for the string-enum case).
pub trait ToColumn {
    fn to_value(&self) -> Value;
}

macro_rules! to_column {
    ($t:ty) => {
        impl ToColumn for $t {
            fn to_value(&self) -> Value {
                self.clone().into()
            }
        }
    };
}

to_column!(bool);
to_column!(i32);
to_column!(i64);
to_column!(u32);
to_column!(u64);
to_column!(f64);
to_column!(String);
to_column!(Vec<u8>);
to_column!(DateTime<Utc>);
to_column!(NaiveDate);

/// Blanket nullability for binding, mirroring the `FromColumn` side: `None`
/// binds as a *typed* NULL chosen from the inner type's [`ColumnType`] (some
/// drivers need the type even for NULL), `Some` delegates. Coherent in-crate,
/// so custom types are nullable without a downstream `Option` impl.
impl<T: ToColumn + SqlType> ToColumn for Option<T> {
    fn to_value(&self) -> Value {
        match self {
            Some(v) => v.to_value(),
            None => null_value(T::COLUMN_TYPE),
        }
    }
}

/// The typed NULL for a column type.
fn null_value(ty: ColumnType) -> Value {
    match ty {
        ColumnType::Bool => Value::Bool(None),
        ColumnType::Int => Value::Int(None),
        ColumnType::BigInt => Value::BigInt(None),
        ColumnType::Uint => Value::Unsigned(None),
        ColumnType::BigUint => Value::BigUnsigned(None),
        ColumnType::Double => Value::Double(None),
        ColumnType::Text => Value::String(None),
        ColumnType::Binary => Value::Bytes(None),
        ColumnType::Timestamp => Value::ChronoDateTimeUtc(None),
        ColumnType::Date => Value::ChronoDate(None),
    }
}
