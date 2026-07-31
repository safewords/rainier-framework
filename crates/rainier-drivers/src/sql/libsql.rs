//! Turso / libSQL executor (feature `libsql-http`).
//!
//! libSQL is SQLite, so the SQL renders with [`Dialect::Sqlite`]; the wire is
//! the **Hrana over HTTP** protocol — `POST /v2/pipeline` with a Bearer token,
//! answering with typed JSON cells. This module owns that protocol's mapping
//! (request encoding, [`LibSqlResult`] parsing, [`LibSqlRow`] decoding) and
//! leaves the bytes-on-the-wire to a caller-supplied [`LibSqlTransport`].
//!
//! Same split as [`crate::sql::d1`], and for the same reason: the transport is the
//! only part that isn't wasm-safe in every host (server-side you'd back it
//! with `reqwest`; inside a Worker with `fetch`). Keeping it a trait means this
//! executor compiles to `wasm32` and runs in a Worker unchanged.
//!
//! Prefer a **native** driver ([`crate::sql::SeaOrmExecutor`] against libSQL's
//! SQLite, or a local file) on a server; reach for this only when the consuming
//! runtime is wasm. See the crate-level "choosing an executor" guidance.

use base64::{engine::general_purpose::STANDARD, Engine as _};
use chrono::{DateTime, NaiveDate, Utc};
use rainier_orm::sea_query::Value;
use rainier_orm::{Dialect, Error, ExecOutcome, Executor, Result, Row};
use serde_json::{json, Value as Json};
use std::sync::Arc;

/// One statement's result, parsed from a `/v2/pipeline` response.
#[derive(Debug, Clone, Default)]
pub struct LibSqlResult {
    /// Column names, in select order.
    pub cols: Vec<String>,
    /// Row data as positional Hrana typed-value cells.
    pub rows: Vec<Vec<Json>>,
    /// `affected_row_count` from the statement result.
    pub affected_row_count: u64,
    /// `last_insert_rowid` (Hrana sends it as a string; parsed here).
    pub last_insert_rowid: i64,
}

impl LibSqlResult {
    /// Parse the `/v2/pipeline` envelope and pull out the first `execute`
    /// statement's result. Surfaces a Hrana `error` entry as an `Err`.
    pub fn from_pipeline(v: &Json) -> Result<Self> {
        let results = v
            .get("results")
            .and_then(Json::as_array)
            .ok_or_else(|| Error::msg("libSQL response missing `results`"))?;
        let first = results
            .first()
            .ok_or_else(|| Error::msg("libSQL response had no statement results"))?;

        if htype(first) == "error" {
            let msg = first
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(Json::as_str)
                .unwrap_or("unknown error");
            return Err(Error::msg(format!("libSQL error: {msg}")));
        }

        let result = first
            .get("response")
            .and_then(|r| r.get("result"))
            .ok_or_else(|| Error::msg("libSQL execute result missing"))?;

        let cols = result
            .get("cols")
            .and_then(Json::as_array)
            .map(|a| {
                a.iter()
                    .map(|c| c.get("name").and_then(Json::as_str).unwrap_or("").to_string())
                    .collect()
            })
            .unwrap_or_default();
        let rows = result
            .get("rows")
            .and_then(Json::as_array)
            .map(|a| a.iter().map(|r| r.as_array().cloned().unwrap_or_default()).collect())
            .unwrap_or_default();
        let affected_row_count =
            result.get("affected_row_count").and_then(Json::as_u64).unwrap_or(0);
        let last_insert_rowid = result.get("last_insert_rowid").map(parse_i64_loose).unwrap_or(0);

        Ok(Self { cols, rows, affected_row_count, last_insert_rowid })
    }
}

/// Build the `/v2/pipeline` request body for one statement (followed by a
/// `close`), so transport impls don't reinvent the envelope.
pub fn pipeline_body(sql: &str, args: &[Json]) -> Json {
    json!({
        "requests": [
            { "type": "execute", "stmt": { "sql": sql, "args": args } },
            { "type": "close" }
        ]
    })
}

/// Sends a single statement to libSQL and returns the parsed [`LibSqlResult`].
/// Implement over `reqwest` (POST [`pipeline_body`] to `<db>/v2/pipeline` with
/// a Bearer token, then [`LibSqlResult::from_pipeline`]) on a server, or over
/// `fetch` inside a Worker.
#[allow(async_fn_in_trait)]
pub trait LibSqlTransport {
    /// Run `sql` with `args` bound, and parse the Hrana response.
    async fn execute(&self, sql: &str, args: Vec<Json>) -> Result<LibSqlResult>;
}

/// An [`Executor`] over any [`LibSqlTransport`]. Speaks the SQLite dialect and
/// lowers sea-query binds to Hrana typed args.
#[derive(Clone)]
pub struct LibSqlExecutor<T> {
    transport: T,
}

impl<T> LibSqlExecutor<T> {
    /// An executor over `transport`.
    pub fn new(transport: T) -> Self {
        Self { transport }
    }

    /// The transport underneath.
    pub fn transport(&self) -> &T {
        &self.transport
    }
}

impl<T: LibSqlTransport> Executor for LibSqlExecutor<T> {
    fn dialect(&self) -> Dialect {
        Dialect::Sqlite
    }

    async fn fetch_all(&self, sql: &str, params: Vec<Value>) -> Result<Vec<Box<dyn Row>>> {
        let res = self.transport.execute(sql, to_hrana_args(params)?).await?;
        let cols = Arc::new(res.cols);
        Ok(res
            .rows
            .into_iter()
            .map(|values| Box::new(LibSqlRow { cols: cols.clone(), values }) as Box<dyn Row>)
            .collect())
    }

    async fn execute(&self, sql: &str, params: Vec<Value>) -> Result<ExecOutcome> {
        let res = self.transport.execute(sql, to_hrana_args(params)?).await?;
        Ok(ExecOutcome {
            rows_affected: res.affected_row_count,
            last_insert_id: res.last_insert_rowid,
        })
    }
}

/// One libSQL row: the shared column-name list plus this row's positional
/// Hrana cells. Hrana types are SQLite's: `integer` (sent as a *string* to
/// preserve 64-bit precision), `float`, `text`, `blob` (base64), and `null`.
pub struct LibSqlRow {
    cols: Arc<Vec<String>>,
    values: Vec<Json>,
}

impl LibSqlRow {
    /// The cell for `col`, or `None` if absent or a Hrana `null`.
    fn cell(&self, col: &str) -> Option<&Json> {
        let idx = self.cols.iter().position(|c| c == col)?;
        let v = self.values.get(idx)?;
        if htype(v) == "null" {
            return None;
        }
        Some(v)
    }
}

impl Row for LibSqlRow {
    fn get_bool(&self, col: &str) -> Result<Option<bool>> {
        Ok(self.get_i64(col)?.map(|v| v != 0))
    }

    fn get_i32(&self, col: &str) -> Result<Option<i32>> {
        Ok(self.get_i64(col)?.map(|v| v as i32))
    }

    fn get_i64(&self, col: &str) -> Result<Option<i64>> {
        match self.cell(col) {
            None => Ok(None),
            Some(v) => hrana_i64(v, col).map(Some),
        }
    }

    fn get_u32(&self, col: &str) -> Result<Option<u32>> {
        Ok(self.get_i64(col)?.map(|v| v as u32))
    }

    fn get_u64(&self, col: &str) -> Result<Option<u64>> {
        Ok(self.get_i64(col)?.map(|v| v as u64))
    }

    fn get_f64(&self, col: &str) -> Result<Option<f64>> {
        match self.cell(col) {
            None => Ok(None),
            Some(v) => match htype(v) {
                "float" => v
                    .get("value")
                    .and_then(Json::as_f64)
                    .map(Some)
                    .ok_or_else(|| type_err(col, "float", v)),
                "integer" => Ok(Some(hrana_i64(v, col)? as f64)),
                _ => Err(type_err(col, "float", v)),
            },
        }
    }

    fn get_string(&self, col: &str) -> Result<Option<String>> {
        match self.cell(col) {
            None => Ok(None),
            Some(v) if htype(v) == "text" => {
                Ok(v.get("value").and_then(Json::as_str).map(str::to_string))
            }
            Some(v) => Err(type_err(col, "text", v)),
        }
    }

    fn get_bytes(&self, col: &str) -> Result<Option<Vec<u8>>> {
        match self.cell(col) {
            None => Ok(None),
            Some(v) if htype(v) == "blob" => {
                let b64 = v
                    .get("base64")
                    .and_then(Json::as_str)
                    .ok_or_else(|| type_err(col, "blob", v))?;
                STANDARD
                    .decode(b64)
                    .map(Some)
                    .map_err(|e| Error::msg(format!("column `{col}`: bad base64 blob: {e}")))
            }
            Some(v) => Err(type_err(col, "blob", v)),
        }
    }

    fn get_datetime(&self, col: &str) -> Result<Option<DateTime<Utc>>> {
        match self.get_string(col)? {
            None => Ok(None),
            Some(s) => DateTime::parse_from_rfc3339(&s)
                .map(|dt| Some(dt.with_timezone(&Utc)))
                .map_err(|e| Error::msg(format!("column `{col}`: not an RFC3339 timestamp: {e}"))),
        }
    }

    fn get_naive_date(&self, col: &str) -> Result<Option<NaiveDate>> {
        match self.get_string(col)? {
            None => Ok(None),
            Some(s) => NaiveDate::parse_from_str(&s, "%Y-%m-%d")
                .map(Some)
                .map_err(|e| Error::msg(format!("column `{col}`: not a YYYY-MM-DD date: {e}"))),
        }
    }
}

fn htype(v: &Json) -> &str {
    v.get("type").and_then(Json::as_str).unwrap_or("")
}

/// Hrana integers arrive as decimal strings (64-bit safe); floats as numbers.
fn hrana_i64(v: &Json, col: &str) -> Result<i64> {
    match htype(v) {
        "integer" => v
            .get("value")
            .and_then(Json::as_str)
            .and_then(|s| s.parse::<i64>().ok())
            .ok_or_else(|| type_err(col, "integer", v)),
        "float" => v
            .get("value")
            .and_then(Json::as_f64)
            .map(|f| f as i64)
            .ok_or_else(|| type_err(col, "integer", v)),
        _ => Err(type_err(col, "integer", v)),
    }
}

/// Accept either a Hrana integer-string or a raw JSON number for fields like
/// `last_insert_rowid`.
fn parse_i64_loose(v: &Json) -> i64 {
    v.as_str().and_then(|s| s.parse::<i64>().ok()).or_else(|| v.as_i64()).unwrap_or(0)
}

fn type_err(col: &str, want: &str, got: &Json) -> Error {
    Error::msg(format!("column `{col}`: expected {want}, got {got}"))
}

/// Lower sea-query bind values to Hrana typed args.
fn to_hrana_args(values: Vec<Value>) -> Result<Vec<Json>> {
    values.into_iter().map(value_to_hrana).collect()
}

fn value_to_hrana(v: Value) -> Result<Json> {
    use rainier_orm::sea_query::Value as V;
    Ok(match v {
        V::Bool(o) => o.map(|b| int_arg(b as i64)).unwrap_or_else(null_arg),
        V::TinyInt(o) => o.map(|n| int_arg(n as i64)).unwrap_or_else(null_arg),
        V::SmallInt(o) => o.map(|n| int_arg(n as i64)).unwrap_or_else(null_arg),
        V::Int(o) => o.map(|n| int_arg(n as i64)).unwrap_or_else(null_arg),
        V::BigInt(o) => o.map(int_arg).unwrap_or_else(null_arg),
        V::TinyUnsigned(o) => o.map(|n| int_arg(n as i64)).unwrap_or_else(null_arg),
        V::SmallUnsigned(o) => o.map(|n| int_arg(n as i64)).unwrap_or_else(null_arg),
        V::Unsigned(o) => o.map(|n| int_arg(n as i64)).unwrap_or_else(null_arg),
        V::BigUnsigned(o) => o.map(|n| int_str_arg(n.to_string())).unwrap_or_else(null_arg),
        V::Float(o) => o.map(|f| float_arg(f as f64)).unwrap_or_else(null_arg),
        V::Double(o) => o.map(float_arg).unwrap_or_else(null_arg),
        V::String(o) => o.map(|s| text_arg(&s)).unwrap_or_else(null_arg),
        V::Char(o) => o.map(|c| text_arg(&c.to_string())).unwrap_or_else(null_arg),
        V::Bytes(o) => o.map(|b| blob_arg(&b)).unwrap_or_else(null_arg),
        V::ChronoDateTimeUtc(o) => o.map(|dt| text_arg(&dt.to_rfc3339())).unwrap_or_else(null_arg),
        V::ChronoDate(o) => {
            o.map(|d| text_arg(&d.format("%Y-%m-%d").to_string())).unwrap_or_else(null_arg)
        }
        other => {
            return Err(Error::msg(format!("libSQL bind: unsupported sea-query value {other:?}")))
        }
    })
}

fn null_arg() -> Json {
    json!({ "type": "null" })
}
fn int_arg(n: i64) -> Json {
    json!({ "type": "integer", "value": n.to_string() })
}
fn int_str_arg(s: String) -> Json {
    json!({ "type": "integer", "value": s })
}
fn float_arg(f: f64) -> Json {
    json!({ "type": "float", "value": f })
}
fn text_arg(s: &str) -> Json {
    json!({ "type": "text", "value": s })
}
fn blob_arg(b: &[u8]) -> Json {
    json!({ "type": "blob", "base64": STANDARD.encode(b) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pipeline_ok() {
        let env = json!({
            "results": [
                { "type": "ok", "response": { "type": "execute", "result": {
                    "cols": [ {"name": "id"}, {"name": "name"} ],
                    "rows": [
                        [ {"type":"integer","value":"1"}, {"type":"text","value":"a"} ],
                        [ {"type":"integer","value":"2"}, {"type":"null"} ]
                    ],
                    "affected_row_count": 0,
                    "last_insert_rowid": "2"
                }}},
                { "type": "ok", "response": { "type": "close" } }
            ]
        });
        let res = LibSqlResult::from_pipeline(&env).unwrap();
        assert_eq!(res.cols, ["id", "name"]);
        assert_eq!(res.rows.len(), 2);
        assert_eq!(res.last_insert_rowid, 2);
    }

    #[test]
    fn surfaces_error() {
        let env = json!({ "results": [
            { "type": "error", "error": { "message": "no such table: x" } }
        ]});
        assert!(LibSqlResult::from_pipeline(&env).is_err());
    }

    #[test]
    fn row_decodes_hrana_typing() {
        let cols = Arc::new(vec![
            "n".to_string(),
            "big".to_string(),
            "r".to_string(),
            "s".to_string(),
            "blob".to_string(),
            "flag".to_string(),
            "nothing".to_string(),
        ]);
        let values = vec![
            json!({"type":"integer","value":"42"}),
            json!({"type":"integer","value":"9000000000"}),
            json!({"type":"float","value":1.5}),
            json!({"type":"text","value":"hi"}),
            json!({"type":"blob","base64":"AQL/"}),
            json!({"type":"integer","value":"1"}),
            json!({"type":"null"}),
        ];
        let row = LibSqlRow { cols, values };
        assert_eq!(row.get_i32("n").unwrap(), Some(42));
        assert_eq!(row.get_u64("big").unwrap(), Some(9_000_000_000));
        assert_eq!(row.get_f64("r").unwrap(), Some(1.5));
        assert_eq!(row.get_string("s").unwrap().as_deref(), Some("hi"));
        assert_eq!(row.get_bytes("blob").unwrap(), Some(vec![1u8, 2, 255]));
        assert_eq!(row.get_bool("flag").unwrap(), Some(true));
        assert_eq!(row.get_string("nothing").unwrap(), None);
        assert_eq!(row.get_string("absent").unwrap(), None);
    }

    #[test]
    fn encodes_args() {
        let args = to_hrana_args(vec![
            Value::BigInt(Some(5)),
            Value::BigUnsigned(Some(18_000_000_000_000_000_000)),
            Value::String(Some(Box::new("hi".into()))),
            Value::Bool(Some(true)),
            Value::Bytes(Some(Box::new(vec![1u8, 2, 255]))),
            Value::Int(None),
        ])
        .unwrap();
        assert_eq!(args[0], json!({"type":"integer","value":"5"}));
        // u64 past i64::MAX survives as a string.
        assert_eq!(args[1], json!({"type":"integer","value":"18000000000000000000"}));
        assert_eq!(args[2], json!({"type":"text","value":"hi"}));
        assert_eq!(args[3], json!({"type":"integer","value":"1"}));
        assert_eq!(args[4], json!({"type":"blob","base64":"AQL/"}));
        assert_eq!(args[5], json!({"type":"null"}));
    }

    #[test]
    fn pipeline_body_shape() {
        let body = pipeline_body("SELECT 1", &[int_arg(1)]);
        let reqs = body.get("requests").and_then(Json::as_array).unwrap();
        assert_eq!(reqs.len(), 2);
        assert_eq!(htype_request(&reqs[0]), "execute");
        assert_eq!(htype_request(&reqs[1]), "close");
    }

    fn htype_request(v: &Json) -> &str {
        v.get("type").and_then(Json::as_str).unwrap_or("")
    }
}
