//! The protocol, driven through a compiled router.

use std::sync::Arc;

use rainier_container::Container;
use rainier_http::{Method, Request, StatusCode};
use rainier_routing::{CompiledRouter, Router};
use rainier_support::Error;
use serde_json::{json, Value};

use crate::*;

fn server() -> McpServer {
    McpServer::new("test-app", "1.2.3")
        .title("Test App")
        .instructions("Use echo to repeat things.")
        .tool(
            Tool::new("echo", "Repeat the text.", |call| async move {
                match call.str("text") {
                    Some(text) => Ok(ToolResult::text(text)),
                    None => Ok(ToolResult::error("Give me text.")),
                }
            })
            .input_schema(json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"],
            }))
            .read_only(),
        )
        .tool(Tool::new("invalid", "Always a validation error.", |_| async {
            Err(Error::validation(json!({ "limit": ["The limit must be an integer."] })))
        }))
        .tool(Tool::new("broken", "Always a server error.", |_| async {
            Err(Error::internal("connection string with a password in it"))
        }))
        .tool(Tool::new("who", "Who is asking.", |call| async move {
            let agent = call.request().and_then(|r| r.header("user-agent")).unwrap_or("nobody");
            Ok(ToolResult::text(agent.to_string()))
        }))
        .resource(Resource::text("app://readme", "readme", "# Hello").mime_type("text/markdown"))
        .prompt(
            Prompt::template("look", "Look at a route.", "Look at {{route}}{{why}}.")
                .argument("route", "The route", true)
                .argument("why", "Why", false),
        )
}

fn router(gate: McpGate, environment: &str) -> CompiledRouter {
    let mut router = Router::new();
    router.merge(
        McpEndpoint::new(Arc::new(server()))
            .gate(gate)
            .environment(environment)
            .allow_origin("https://console.example.com")
            .routes(),
    );
    router.compile(&Container::new()).unwrap()
}

struct Answer {
    status: StatusCode,
    text: String,
}

impl Answer {
    fn json(&self) -> Value {
        serde_json::from_str(&self.text).unwrap_or(Value::Null)
    }
}

async fn send(
    router: &CompiledRouter,
    method: Method,
    body: &str,
    headers: &[(&str, &str)],
) -> Answer {
    let mut builder = Request::builder().method(method).uri("/_mcp").body(body.to_string());
    builder = builder.header("content-type", "application/json");
    for (name, value) in headers {
        builder = builder.header(name, value);
    }
    let mut response = router.dispatch(builder.build()).await;
    let bytes = response.take_body().collect().await.unwrap();
    Answer { status: response.status(), text: String::from_utf8_lossy(&bytes).into_owned() }
}

async fn rpc(router: &CompiledRouter, method: &str, params: Value) -> Value {
    let body = json!({ "jsonrpc": "2.0", "id": 7, "method": method, "params": params });
    let answer = send(router, Method::POST, &body.to_string(), &[]).await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.text);
    answer.json()
}

fn open() -> CompiledRouter {
    router(McpGate::allow_all(), "production")
}

#[tokio::test]
async fn initialize_negotiates_the_version_and_describes_the_server() {
    let router = open();
    let answer = rpc(&router, "initialize", json!({ "protocolVersion": "2025-06-18" })).await;
    assert_eq!(answer["id"], 7);
    let result = &answer["result"];
    assert_eq!(result["protocolVersion"], "2025-06-18");
    assert_eq!(
        result["serverInfo"],
        json!({ "name": "test-app", "version": "1.2.3", "title": "Test App" })
    );
    assert_eq!(result["instructions"], "Use echo to repeat things.");
    assert!(result["capabilities"]["tools"].is_object());
    assert!(result["capabilities"]["prompts"].is_object());
    assert!(result["capabilities"]["resources"].is_object());

    let unknown = rpc(&router, "initialize", json!({ "protocolVersion": "1999-01-01" })).await;
    assert_eq!(unknown["result"]["protocolVersion"], LATEST_PROTOCOL_VERSION);
}

#[tokio::test]
async fn tools_are_listed_and_called() {
    let router = open();
    let list = rpc(&router, "tools/list", json!({})).await;
    let tools = list["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 4);
    assert_eq!(tools[0]["name"], "echo");
    assert_eq!(tools[0]["inputSchema"]["required"], json!(["text"]));
    assert_eq!(tools[0]["annotations"]["readOnlyHint"], true);

    let called =
        rpc(&router, "tools/call", json!({ "name": "echo", "arguments": { "text": "hi" } })).await;
    assert_eq!(
        called["result"],
        json!({ "content": [{ "type": "text", "text": "hi" }], "isError": false })
    );

    let soft = rpc(&router, "tools/call", json!({ "name": "echo" })).await;
    assert_eq!(soft["result"]["isError"], true);

    let unknown = rpc(&router, "tools/call", json!({ "name": "nope" })).await;
    assert_eq!(unknown["error"]["code"], -32602);
}

#[tokio::test]
async fn a_failing_tool_tells_the_model_what_it_may_know() {
    let router = open();
    let invalid = rpc(&router, "tools/call", json!({ "name": "invalid" })).await;
    let text = invalid["result"]["content"][0]["text"].as_str().unwrap();
    assert_eq!(invalid["result"]["isError"], true);
    assert!(text.contains("The limit must be an integer."), "{text}");

    let broken = rpc(&router, "tools/call", json!({ "name": "broken" })).await;
    let text = broken["result"]["content"][0]["text"].as_str().unwrap();
    assert!(!text.contains("password"), "{text}");
}

#[tokio::test]
async fn a_tool_sees_the_request_that_carried_it() {
    let router = open();
    let body =
        json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": "who" } });
    let answer = send(&router, Method::POST, &body.to_string(), &[("user-agent", "claude")]).await;
    assert_eq!(answer.json()["result"]["content"][0]["text"], "claude");
}

#[tokio::test]
async fn prompts_render_their_arguments() {
    let router = open();
    let list = rpc(&router, "prompts/list", json!({})).await;
    assert_eq!(list["result"]["prompts"][0]["arguments"][0]["required"], true);

    let got = rpc(
        &router,
        "prompts/get",
        json!({ "name": "look", "arguments": { "route": "posts.index" } }),
    )
    .await;
    assert_eq!(got["result"]["messages"][0]["role"], "user");
    assert_eq!(got["result"]["messages"][0]["content"]["text"], "Look at posts.index.");

    let missing = rpc(&router, "prompts/get", json!({ "name": "look" })).await;
    assert_eq!(missing["error"]["code"], -32602);
}

#[tokio::test]
async fn resources_are_listed_and_read() {
    let router = open();
    let list = rpc(&router, "resources/list", json!({})).await;
    assert_eq!(list["result"]["resources"][0]["uri"], "app://readme");
    let read = rpc(&router, "resources/read", json!({ "uri": "app://readme" })).await;
    assert_eq!(read["result"]["contents"][0]["text"], "# Hello");
    assert_eq!(read["result"]["contents"][0]["mimeType"], "text/markdown");
    let missing = rpc(&router, "resources/read", json!({ "uri": "app://nope" })).await;
    assert_eq!(missing["error"]["code"], -32002);
}

#[tokio::test]
async fn notifications_are_accepted_without_an_answer() {
    let router = open();
    let body = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
    let answer = send(&router, Method::POST, &body.to_string(), &[]).await;
    assert_eq!(answer.status, StatusCode::ACCEPTED);
    assert!(answer.text.is_empty());
}

#[tokio::test]
async fn a_batch_is_answered_as_one() {
    let router = open();
    let body = json!([
        { "jsonrpc": "2.0", "id": 1, "method": "ping" },
        { "jsonrpc": "2.0", "method": "notifications/initialized" },
        { "jsonrpc": "2.0", "id": 2, "method": "nope" },
    ]);
    let answer = send(&router, Method::POST, &body.to_string(), &[]).await.json();
    assert_eq!(answer.as_array().unwrap().len(), 2);
    assert_eq!(answer[0]["result"], json!({}));
    assert_eq!(answer[1]["error"]["code"], -32601);
}

#[tokio::test]
async fn malformed_bodies_are_json_rpc_errors() {
    let router = open();
    let parse = send(&router, Method::POST, "{nope", &[]).await.json();
    assert_eq!(parse["error"]["code"], -32700);
    let version = send(&router, Method::POST, r#"{"jsonrpc":"1.0","id":1,"method":"ping"}"#, &[])
        .await
        .json();
    assert_eq!(version["error"]["code"], -32600);
}

#[tokio::test]
async fn other_methods_are_not_allowed() {
    let router = open();
    let answer = send(&router, Method::GET, "", &[]).await;
    assert_eq!(answer.status, StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn an_unsupported_protocol_header_is_refused() {
    let router = open();
    let ping = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
    let answer = send(&router, Method::POST, ping, &[("mcp-protocol-version", "1999-01-01")]).await;
    assert_eq!(answer.status, StatusCode::BAD_REQUEST);
    let answer = send(&router, Method::POST, ping, &[("mcp-protocol-version", "2025-06-18")]).await;
    assert_eq!(answer.status, StatusCode::OK);
}

#[tokio::test]
async fn a_foreign_origin_is_refused() {
    let router = open();
    let ping = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
    let evil = send(&router, Method::POST, ping, &[("origin", "https://evil.example")]).await;
    assert_eq!(evil.status, StatusCode::FORBIDDEN);
    let listed =
        send(&router, Method::POST, ping, &[("origin", "https://console.example.com")]).await;
    assert_eq!(listed.status, StatusCode::OK);
}

#[tokio::test]
async fn the_gate_admits_a_token_and_the_local_environment_and_hides_otherwise() {
    let ping = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
    let guarded = router(McpGate::deny_all().token("let-me-in"), "production");

    let refused = send(&guarded, Method::POST, ping, &[]).await;
    assert_eq!(refused.status, StatusCode::NOT_FOUND);
    let wrong = send(&guarded, Method::POST, ping, &[("authorization", "Bearer let-me")]).await;
    assert_eq!(wrong.status, StatusCode::NOT_FOUND);
    let refused_get = send(&guarded, Method::GET, "", &[]).await;
    assert_eq!(refused_get.status, StatusCode::NOT_FOUND, "a refused GET must not say 405");

    let right = send(&guarded, Method::POST, ping, &[("authorization", "Bearer let-me-in")]).await;
    assert_eq!(right.status, StatusCode::OK);

    let local = router(McpGate::deny_all(), "local");
    assert_eq!(send(&local, Method::POST, ping, &[]).await.status, StatusCode::OK);
    let closed = router(McpGate::deny_all().open_locally(false), "local");
    assert_eq!(send(&closed, Method::POST, ping, &[]).await.status, StatusCode::NOT_FOUND);
}

#[test]
fn an_empty_token_never_matches() {
    let gate = McpGate::deny_all().token("  ");
    assert!(format!("{gate:?}").contains("tokens: 0"));
}
