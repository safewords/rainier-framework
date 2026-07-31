//! [`Json<T>`] — a column whose Rust value is any `serde` type but whose
//! storage is **JSON text on every dialect**.
//!
//! Databases disagree on JSON: MySQL/Postgres have a native `JSON` type, SQLite
//! and Cloudflare D1 keep it in `TEXT`. Rather than special-case a `JSON`
//! column type (and decode it differently per backend), `Json<T>` declares a
//! plain `Text` column and (de)serializes `T` with `serde_json` at the column
//! boundary. One representation — serialized JSON in a text column — works
//! identically across MySQL, Postgres, SQLite, and D1, so a single generic
//! adapter handles it everywhere. (If you have a native MySQL `JSON` column,
//! migrate it to `TEXT`; the JSON text is byte-compatible.)
//!
//! ```ignore
//! use rainier_orm::{Json, Entity};
//! use serde::{Serialize, Deserialize};
//!
//! #[derive(Clone, Serialize, Deserialize)]
//! struct Scopes(Vec<String>);
//!
//! #[derive(Entity)]
//! struct Grant {
//!     #[orm(pk, auto_increment)]
//!     id: u64,
//!     scopes: Json<Scopes>,            // stored as JSON text
//!     extra: Option<Json<serde_json::Value>>,  // nullable, arbitrary JSON
//! }
//! ```

use crate::{Error, Result, Row};
use serde::{de::DeserializeOwned, Serialize};

/// A `serde` value persisted as JSON text. Deref/`From` make it ergonomic to
/// treat as the inner `T`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Json<T>(pub T);

impl<T> Json<T> {
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> From<T> for Json<T> {
    fn from(v: T) -> Self {
        Json(v)
    }
}

impl<T> core::ops::Deref for Json<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T> core::ops::DerefMut for Json<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0
    }
}

impl<T> crate::SqlType for Json<T> {
    const COLUMN_TYPE: crate::ColumnType = crate::ColumnType::Text;
}

impl<T: Serialize> crate::ToColumn for Json<T> {
    fn to_value(&self) -> sea_query::Value {
        // Serialization failure is unexpected for a well-formed `T`; fall back
        // to JSON null rather than panicking at the binding layer.
        let s = serde_json::to_string(&self.0).unwrap_or_else(|_| "null".to_string());
        sea_query::Value::String(Some(Box::new(s)))
    }
}

impl<T: DeserializeOwned> crate::FromColumn for Json<T> {
    fn from_column(row: &dyn Row, col: &str) -> Result<Self> {
        let s =
            row.get_string(col)?.ok_or_else(|| Error::msg(format!("column `{col}` was NULL")))?;
        serde_json::from_str(&s)
            .map(Json)
            .map_err(|e| Error::msg(format!("column `{col}`: invalid JSON: {e}")))
    }
}
