//! [`Base64Bytes`] — a binary column stored as **base64 text on every
//! dialect**.
//!
//! Raw binary is the one thing that doesn't travel uniformly: MySQL has
//! `VARBINARY`, but D1/SQLite over JSON has no clean blob binding, so binary is
//! conventionally base64-encoded into a `TEXT` column there. Modelling such a
//! column as `Base64Bytes` makes the representation identical across MySQL,
//! Postgres, SQLite, and D1 — a `TEXT` column holding base64 — so one generic
//! adapter works on all of them. Use plain `Vec<u8>` only when every target is
//! a real binary column (e.g. MySQL-only).

use crate::{Error, Result, Row, StringColumn};
use base64::{engine::general_purpose::STANDARD, Engine as _};

/// Bytes persisted as base64 in a `TEXT` column, uniformly across dialects.
/// Wire it into the ORM with the [`impl_string_column!`](crate::impl_string_column)
/// machinery via its [`StringColumn`] impl below.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Base64Bytes(pub Vec<u8>);

impl Base64Bytes {
    pub fn into_vec(self) -> Vec<u8> {
        self.0
    }
    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }
}

impl From<Vec<u8>> for Base64Bytes {
    fn from(v: Vec<u8>) -> Self {
        Base64Bytes(v)
    }
}
impl From<Base64Bytes> for Vec<u8> {
    fn from(b: Base64Bytes) -> Self {
        b.0
    }
}

impl StringColumn for Base64Bytes {
    fn to_column_str(&self) -> String {
        STANDARD.encode(&self.0)
    }
    fn from_column_str(s: &str) -> Result<Self> {
        STANDARD
            .decode(s)
            .map(Base64Bytes)
            .map_err(|e| Error::msg(format!("invalid base64 in column: {e}")))
    }
}

// Reuse the string-column wiring: Text SqlType + To/FromColumn (incl. the
// blanket Option<Base64Bytes>). This is the macro-free expansion since the
// macro lives in this crate.
impl crate::SqlType for Base64Bytes {
    const COLUMN_TYPE: crate::ColumnType = crate::ColumnType::Text;
}
impl crate::ToColumn for Base64Bytes {
    fn to_value(&self) -> sea_query::Value {
        sea_query::Value::String(Some(Box::new(self.to_column_str())))
    }
}
impl crate::FromColumn for Base64Bytes {
    fn from_column(row: &dyn Row, col: &str) -> Result<Self> {
        let s =
            row.get_string(col)?.ok_or_else(|| Error::msg(format!("column `{col}` was NULL")))?;
        <Base64Bytes as StringColumn>::from_column_str(&s)
    }
}
