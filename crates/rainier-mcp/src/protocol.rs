//! JSON-RPC 2.0 envelopes, as the Model Context Protocol uses them.

use serde_json::{json, Value};

/// The error codes this server answers with.
pub mod codes {
    /// The body is not JSON.
    pub const PARSE_ERROR: i64 = -32700;
    /// The JSON is not a JSON-RPC request.
    pub const INVALID_REQUEST: i64 = -32600;
    /// No such method.
    pub const METHOD_NOT_FOUND: i64 = -32601;
    /// The method's parameters are wrong: an unknown tool or prompt, a
    /// missing argument.
    pub const INVALID_PARAMS: i64 = -32602;
    /// The server failed.
    pub const INTERNAL_ERROR: i64 = -32603;
    /// `resources/read` of a URI the server does not offer.
    pub const RESOURCE_NOT_FOUND: i64 = -32002;
}

/// A successful response to request `id`.
pub fn result(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// An error response to request `id` (`null` when it could not be read).
pub fn error(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message.into() } })
}
