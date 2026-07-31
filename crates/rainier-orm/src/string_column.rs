//! Support for custom column types stored as text — the common case being a
//! Rust enum persisted as a string (`"pending"`, `"verified"`, …).
//!
//! Implement [`StringColumn`] for your type (how it maps to/from a string),
//! then `impl_string_column!(YourType)` to wire it into the ORM: it gets a
//! `Text` [`SqlType`](crate::SqlType), and [`ToColumn`](crate::ToColumn) /
//! [`FromColumn`](crate::FromColumn) impls (for both `T` and `Option<T>`) so it
//! can be a `#[derive(Entity)]` field like any primitive.
//!
//! ```ignore
//! use rainier_orm::{StringColumn, impl_string_column, Result, Error};
//!
//! #[derive(Clone, PartialEq)]
//! enum Status { Pending, Verified, Rejected }
//!
//! impl StringColumn for Status {
//!     fn to_column_str(&self) -> String {
//!         match self { Status::Pending => "pending", Status::Verified => "verified", Status::Rejected => "rejected" }.into()
//!     }
//!     fn from_column_str(s: &str) -> Result<Self> {
//!         match s {
//!             "pending" => Ok(Status::Pending),
//!             "verified" => Ok(Status::Verified),
//!             "rejected" => Ok(Status::Rejected),
//!             other => Err(Error::msg(format!("unknown Status `{other}`"))),
//!         }
//!     }
//! }
//! impl_string_column!(Status);
//! ```

/// How a custom type maps to and from its stored string form. The string is
/// what lands in a `TEXT`/`VARCHAR` column.
pub trait StringColumn: Sized {
    fn to_column_str(&self) -> String;
    fn from_column_str(s: &str) -> crate::Result<Self>;
}

/// Wire a [`StringColumn`] type into the ORM's column traits. See the
/// [module docs](crate::string_column).
#[macro_export]
macro_rules! impl_string_column {
    ($t:ty) => {
        impl $crate::SqlType for $t {
            const COLUMN_TYPE: $crate::ColumnType = $crate::ColumnType::Text;
        }
        impl $crate::ToColumn for $t {
            fn to_value(&self) -> $crate::sea_query::Value {
                $crate::sea_query::Value::String(::core::option::Option::Some(
                    ::std::boxed::Box::new($crate::StringColumn::to_column_str(self)),
                ))
            }
        }
        impl $crate::FromColumn for $t {
            fn from_column(row: &dyn $crate::Row, col: &str) -> $crate::Result<Self> {
                let s = $crate::Row::get_string(row, col)?
                    .ok_or_else(|| $crate::Error::msg(::std::format!("column `{col}` was NULL")))?;
                <$t as $crate::StringColumn>::from_column_str(&s)
            }
        }
        // `Option<$t>` support comes from the crate's blanket impls (driven by
        // the `Text` SqlType above) — the orphan rule forbids implementing it
        // here, and it isn't needed.
    };
}
