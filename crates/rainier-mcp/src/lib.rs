//! # MCP
//!
//! A [Model Context Protocol](https://modelcontextprotocol.io) server for
//! Rainier: the tools, resources and prompts an application offers to an AI
//! client, served over Streamable HTTP from the application's own router.
//!
//! ```
//! use std::sync::Arc;
//! use rainier_mcp::{McpEndpoint, McpGate, McpServer, Prompt, Tool, ToolResult};
//! use serde_json::json;
//!
//! let server = McpServer::new("blog", "1.0.0")
//!     .instructions("Tools for reading this blog's posts.")
//!     .tool(
//!         Tool::new("count_posts", "How many posts are published.", |_call| async {
//!             Ok(ToolResult::json(&json!({ "published": 42 })))
//!         })
//!         .read_only(),
//!     )
//!     .prompt(Prompt::template("summarise", "Summarise the blog.", "Summarise the blog."));
//!
//! let router = McpEndpoint::new(Arc::new(server))
//!     .gate(McpGate::deny_all().token("a-long-random-string"))
//!     .routes();
//! # let _ = router;
//! ```
//!
//! - [`McpServer`] is the protocol, with no transport: `initialize`, `ping`,
//!   `tools/*`, `resources/*` and `prompts/*`, one message or a batch.
//! - [`McpEndpoint`] serves it at one path. It is stateless — no session id,
//!   no server-initiated stream — so any replica answers any request.
//! - [`McpGate`] decides who may: everyone locally, a bearer token, or the
//!   application's own check. A refused request gets the router's own 404.
//!
//! See `docs/mcp.md`.

#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

mod http;
pub mod protocol;
mod server;

pub use http::{AccessCheck, McpEndpoint, McpGate};
pub use server::{
    Call, Content, McpServer, Prompt, PromptArgument, Resource, Tool, ToolAnnotations, ToolResult,
    LATEST_PROTOCOL_VERSION, SUPPORTED_PROTOCOL_VERSIONS,
};

#[cfg(test)]
mod tests;
