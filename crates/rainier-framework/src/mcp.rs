//! The Model Context Protocol server, wired into the framework — the `mcp`
//! feature.
//!
//! Everything [`rainier_mcp`] exports is re-exported here. What this module
//! adds is what [`Rainier::boot`](crate::Rainier::boot) does with it.
//!
//! # What boot does
//!
//! With the feature compiled in and `MCP_ENABLED=true`, boot:
//!
//! 1. builds an [`McpServer`] named after `APP_NAME`, at `APP_VERSION` (or
//!    the framework's version);
//! 2. applies every [`Rainier::with_mcp`](crate::Rainier::with_mcp), which is
//!    where the application adds its own tools, resources and prompts;
//! 3. when Lookout is on too (the `lookout` feature and `LOOKOUT_ENABLED`),
//!    and `MCP_LOOKOUT` is not `false`, adds Lookout's tools and prompts —
//!    see [`rainier_lookout::mcp`];
//! 4. binds the server in the container as `Arc<McpServer>`;
//! 5. mounts it at `MCP_PATH` (`/_mcp`), outside every middleware group,
//!    behind an [`McpGate`]: everyone in the local environment (unless
//!    `MCP_OPEN_LOCALLY=false`), a request bearing one of `MCP_TOKENS`, a
//!    named read-only client from `MCP_READ_ONLY_CLIENTS`, or whoever [`Rainier::with_mcp_access`](crate::Rainier::with_mcp_access)
//!    lets through. Anybody else gets the router's own 404.
//!
//! Off, it binds nothing and mounts nothing.

use rainier_config::Env;
use rainier_support::{Error, Result};

pub use rainier_mcp::*;

/// What Lookout's dashboard is told about the server, for its MCP page —
/// nothing without Lookout.
#[cfg(feature = "lookout")]
pub(crate) type DashboardLink = rainier_lookout::config::McpLink;
/// What Lookout's dashboard is told about the server — nothing without
/// Lookout.
#[cfg(not(feature = "lookout"))]
pub(crate) type DashboardLink = ();

/// One adjustment to the server, from [`Rainier::with_mcp`](crate::Rainier::with_mcp).
pub(crate) type Adjustment = Box<dyn FnOnce(McpServer) -> McpServer>;

/// The deployment's settings, from the environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpSettings {
    /// `MCP_ENABLED`, `false` by default.
    pub enabled: bool,
    /// `MCP_PATH`, `/_mcp` by default.
    pub path: String,
    /// `MCP_TOKENS`: bearer tokens that are let in, comma-separated. Two at
    /// once so one can be rotated without a gap.
    pub tokens: Vec<String>,
    /// `MCP_READ_ONLY_CLIENTS`: named clients that may only read, as
    /// `name=token` pairs, comma-separated. Each is shown, and may call, only
    /// the read-only tools, and is logged by name on every call.
    pub read_only_clients: Vec<(String, String)>,
    /// `MCP_ALLOWED_ORIGINS`: browser origins allowed besides the server's
    /// own host, comma-separated.
    pub allowed_origins: Vec<String>,
    /// `MCP_OPEN_LOCALLY`, `true` by default: anybody may in the local
    /// environment.
    pub open_locally: bool,
    /// `MCP_LOOKOUT`, `true` by default: offer Lookout's tools when Lookout
    /// is on.
    pub lookout: bool,
}

impl Default for McpSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            path: "/_mcp".into(),
            tokens: Vec::new(),
            read_only_clients: Vec::new(),
            allowed_origins: Vec::new(),
            open_locally: true,
            lookout: true,
        }
    }
}

impl McpSettings {
    /// Read the `MCP_*` variables.
    ///
    /// A token shorter than 32 characters fails: it is the only thing
    /// between the internet and every tool, Lookout's included. So does a
    /// read-only client that is not `name=token`, a name that is not
    /// letters, digits, `.`, `_` or `-`, a name given twice, and a token used
    /// twice — two clients on one token could not be told apart, and a
    /// client on a full token would not be read-only.
    pub fn from_env(env: &Env) -> Result<Self> {
        let list = |name: &str| -> Vec<String> {
            env.get(name)
                .unwrap_or_default()
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        };
        let mut read_only_clients = Vec::new();
        for entry in list("MCP_READ_ONLY_CLIENTS") {
            let Some((name, token)) = entry.split_once('=') else {
                return Err(Error::internal(
                    "mcp: every MCP_READ_ONLY_CLIENTS entry must be name=token",
                ));
            };
            let (name, token) = (name.trim(), token.trim());
            let valid_name = !name.is_empty()
                && name.len() <= 64
                && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
            if !valid_name {
                return Err(Error::internal(
                    "mcp: an MCP_READ_ONLY_CLIENTS name must be 1-64 letters, digits, `.`, `_` or `-`",
                ));
            }
            read_only_clients.push((name.to_string(), token.to_string()));
        }
        let settings = Self {
            enabled: env.bool("MCP_ENABLED", false),
            path: env.string("MCP_PATH", "/_mcp"),
            tokens: list("MCP_TOKENS"),
            read_only_clients,
            allowed_origins: list("MCP_ALLOWED_ORIGINS"),
            open_locally: env.bool("MCP_OPEN_LOCALLY", true),
            lookout: env.bool("MCP_LOOKOUT", true),
        };
        if let Some(short) = settings.tokens.iter().find(|t| t.chars().count() < 32) {
            return Err(Error::internal(format!(
                "mcp: every MCP_TOKENS entry must be at least 32 characters; one is {}",
                short.chars().count()
            )));
        }
        if let Some((name, token)) =
            settings.read_only_clients.iter().find(|(_, t)| t.chars().count() < 32)
        {
            return Err(Error::internal(format!(
                "mcp: the MCP_READ_ONLY_CLIENTS token of `{name}` must be at least 32 characters; it is {}",
                token.chars().count()
            )));
        }
        let mut names = std::collections::HashSet::new();
        let mut tokens: std::collections::HashSet<&str> =
            settings.tokens.iter().map(String::as_str).collect();
        for (name, token) in &settings.read_only_clients {
            if !names.insert(name.as_str()) {
                return Err(Error::internal(format!(
                    "mcp: MCP_READ_ONLY_CLIENTS names `{name}` twice"
                )));
            }
            if !tokens.insert(token.as_str()) {
                return Err(Error::internal(format!(
                    "mcp: the MCP_READ_ONLY_CLIENTS token of `{name}` is also another token"
                )));
            }
        }
        if !settings.path.starts_with('/') {
            return Err(Error::internal("mcp: MCP_PATH must start with `/`"));
        }
        Ok(settings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_read_the_environment() {
        let env = Env::from_map([
            ("MCP_ENABLED", "true"),
            ("MCP_PATH", "/mcp"),
            ("MCP_TOKENS", " aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa , bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb,"),
            ("MCP_ALLOWED_ORIGINS", "https://console.example.com"),
            ("MCP_LOOKOUT", "false"),
        ])
        .isolated();
        let settings = McpSettings::from_env(&env).unwrap();
        assert!(settings.enabled);
        assert_eq!(settings.path, "/mcp");
        assert_eq!(settings.tokens.len(), 2);
        assert_eq!(settings.allowed_origins, vec!["https://console.example.com".to_string()]);
        assert!(settings.open_locally);
        assert!(!settings.lookout);
    }

    #[test]
    fn defaults_are_off_and_safe() {
        let settings =
            McpSettings::from_env(&Env::from_map::<&str, &str, _>([]).isolated()).unwrap();
        assert_eq!(settings, McpSettings::default());
    }

    #[test]
    fn a_short_token_fails() {
        let env = Env::from_map([("MCP_TOKENS", "short")]).isolated();
        assert!(McpSettings::from_env(&env).is_err());
    }

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn clients(value: &str) -> Result<McpSettings> {
        McpSettings::from_env(
            &Env::from_map([("MCP_TOKENS", A), ("MCP_READ_ONLY_CLIENTS", value)]).isolated(),
        )
    }

    #[test]
    fn read_only_clients_are_name_token_pairs() {
        let settings = clients(&format!(" reader-1 = {B} , ")).unwrap();
        assert_eq!(settings.read_only_clients, vec![("reader-1".to_string(), B.to_string())]);
    }

    #[test]
    fn a_malformed_read_only_client_fails() {
        assert!(clients(B).is_err(), "no name");
        assert!(clients(&format!("={B}")).is_err(), "empty name");
        assert!(clients(&format!("two words={B}")).is_err(), "bad name");
        assert!(clients("reader=short").is_err(), "short token");
    }

    #[test]
    fn a_read_only_client_must_be_told_apart() {
        let c = "cccccccccccccccccccccccccccccccc";
        assert!(clients(&format!("x={B},x={c}")).is_err(), "a name twice");
        assert!(clients(&format!("x={B},y={B}")).is_err(), "a token twice");
        assert!(clients(&format!("x={A}")).is_err(), "a full token as a read-only one");
    }
}

#[cfg(test)]
mod boot_tests {
    use rainier_http::{Method, Request};
    use serde_json::{json, Value};

    use super::McpSettings;
    use crate::testing::TestApp;
    use crate::Rainier;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    const READER: &str = "fedcba9876543210fedcba9876543210";

    fn on(local: bool) -> impl FnOnce(McpSettings) -> McpSettings {
        move |settings| McpSettings {
            enabled: true,
            tokens: vec![TOKEN.into()],
            read_only_clients: vec![("reader".into(), READER.into())],
            open_locally: local,
            ..settings
        }
    }

    fn local(app: Rainier) -> Rainier {
        app.configure(|config| {
            config.set(crate::keys::APP_ENV, rainier_config::AppEnv::Local).unwrap();
        })
    }

    fn builder() -> Rainier {
        Rainier::new(".").without_facades().without_tracing().with_mcp(|server| {
            server
                .tool(
                    super::Tool::new("ping", "Answer pong.", |_| async {
                        Ok(super::ToolResult::text("pong"))
                    })
                    .read_only(),
                )
                .tool(super::Tool::new(
                    "poke",
                    "Change something; answer who did.",
                    |call| async move {
                        Ok(super::ToolResult::text(call.client().unwrap_or("full").to_string()))
                    },
                ))
        })
    }

    fn rpc(method: &str, params: Value, token: Option<&str>) -> Request {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let mut request = Request::builder()
            .method(Method::POST)
            .uri("/_mcp")
            .header("content-type", "application/json")
            .body(body.to_string());
        if let Some(token) = token {
            request = request.header("authorization", &format!("Bearer {token}"));
        }
        request.build()
    }

    fn tool_names(answer: &Value) -> Vec<String> {
        answer["result"]["tools"]
            .as_array()
            .map(|tools| {
                tools.iter().filter_map(|t| t["name"].as_str().map(String::from)).collect()
            })
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn off_mounts_nothing() {
        let app = TestApp::new(builder().boot().await.unwrap()).unwrap();
        assert!(app.resolve::<std::sync::Arc<super::McpServer>>().is_err());
        app.send(rpc("ping", json!({}), Some(TOKEN))).await.assert_not_found();
    }

    #[tokio::test]
    async fn on_it_serves_the_applications_tools_to_a_token_holder() {
        let app =
            TestApp::new(builder().with_mcp_settings(on(false)).boot().await.unwrap()).unwrap();
        assert!(app.resolve::<std::sync::Arc<super::McpServer>>().is_ok());

        app.send(rpc("tools/list", json!({}), None)).await.assert_not_found();
        app.send(rpc("tools/list", json!({}), Some("wrong-wrong-wrong-wrong-wrong-wrong")))
            .await
            .assert_not_found();

        let listed = app.send(rpc("tools/list", json!({}), Some(TOKEN))).await;
        listed.assert_ok();
        let names = tool_names(&listed.json());
        assert!(names.contains(&"ping".to_string()), "{names:?}");
        assert!(!names.iter().any(|n| n.starts_with("lookout_")), "Lookout is off: {names:?}");

        let called = app.send(rpc("tools/call", json!({ "name": "ping" }), Some(TOKEN))).await;
        called.assert_json_path("result.content.0.text", "pong");
    }

    #[tokio::test]
    async fn a_read_only_client_is_shown_and_may_call_only_read_only_tools() {
        let app =
            TestApp::new(builder().with_mcp_settings(on(false)).boot().await.unwrap()).unwrap();

        let full = tool_names(&app.send(rpc("tools/list", json!({}), Some(TOKEN))).await.json());
        assert!(full.contains(&"poke".to_string()), "{full:?}");
        let poked = app.send(rpc("tools/call", json!({ "name": "poke" }), Some(TOKEN))).await;
        poked.assert_json_path("result.content.0.text", "full");

        let read = tool_names(&app.send(rpc("tools/list", json!({}), Some(READER))).await.json());
        assert_eq!(read, vec!["ping".to_string()]);
        let pinged = app.send(rpc("tools/call", json!({ "name": "ping" }), Some(READER))).await;
        pinged.assert_json_path("result.content.0.text", "pong");
        let refused = app.send(rpc("tools/call", json!({ "name": "poke" }), Some(READER))).await;
        refused.assert_ok();
        assert_eq!(refused.json()["error"]["code"], -32602, "{}", refused.json());
    }

    #[tokio::test]
    async fn taking_a_read_only_client_out_of_the_settings_shuts_it_out() {
        let without =
            |settings| McpSettings { read_only_clients: Vec::new(), ..on(false)(settings) };
        let app = TestApp::new(builder().with_mcp_settings(without).boot().await.unwrap()).unwrap();
        app.send(rpc("tools/list", json!({}), Some(READER))).await.assert_not_found();
        app.send(rpc("tools/list", json!({}), Some(TOKEN))).await.assert_ok();
    }

    #[cfg(feature = "lookout")]
    #[tokio::test]
    async fn a_read_only_client_cannot_change_lookout_cases() {
        let app = local(builder())
            .with_mcp_settings(on(false))
            .with_lookout(|config| config.enabled(true).local(true))
            .boot()
            .await
            .unwrap();
        let app = TestApp::new(app).unwrap();

        let names = tool_names(&app.send(rpc("tools/list", json!({}), Some(READER))).await.json());
        assert!(names.contains(&"lookout_status".to_string()), "{names:?}");
        assert!(!names.contains(&"lookout_update_case".to_string()), "{names:?}");
        let refused = app
            .send(rpc(
                "tools/call",
                json!({ "name": "lookout_update_case", "arguments": { "id": "x", "action": "resolve" } }),
                Some(READER),
            ))
            .await;
        assert_eq!(refused.json()["error"]["code"], -32602, "{}", refused.json());
    }

    #[cfg(feature = "lookout")]
    #[tokio::test]
    async fn with_lookout_on_its_tools_and_prompts_join_the_server() {
        let app = local(builder())
            .with_mcp_settings(on(true))
            .with_lookout(|config| config.enabled(true).local(true))
            .boot()
            .await
            .unwrap();
        let app = TestApp::new(app).unwrap();

        let listed = app.send(rpc("tools/list", json!({}), None)).await;
        listed.assert_ok();
        let names = tool_names(&listed.json());
        for name in rainier_lookout::mcp::tool_names() {
            assert!(names.contains(&name.to_string()), "{name} missing from {names:?}");
        }
        assert!(names.contains(&"ping".to_string()));

        let status = app.send(rpc("tools/call", json!({ "name": "lookout_status" }), None)).await;
        status.assert_ok();
        assert_eq!(status.json()["result"]["isError"], false);

        let prompts = app.send(rpc("prompts/list", json!({}), None)).await;
        assert_eq!(prompts.json()["result"]["prompts"].as_array().map(Vec::len), Some(4));

        let initialize = app.send(rpc("initialize", json!({}), None)).await;
        let instructions =
            initialize.json()["result"]["instructions"].as_str().unwrap_or_default().to_string();
        assert!(instructions.contains("Lookout"), "{instructions}");

        // MCP_LOOKOUT=false keeps them off.
        let app = local(builder())
            .with_mcp_settings(|s| McpSettings { lookout: false, ..on(true)(s) })
            .with_lookout(|config| config.enabled(true).local(true))
            .boot()
            .await
            .unwrap();
        let app = TestApp::new(app).unwrap();
        let names = tool_names(&app.send(rpc("tools/list", json!({}), None)).await.json());
        assert!(!names.iter().any(|n| n.starts_with("lookout_")), "{names:?}");
    }

    #[cfg(feature = "lookout")]
    #[tokio::test]
    async fn reading_lookout_over_mcp_is_not_recorded_by_lookout() {
        use crate::lookout::{BatchQuery, Lookout, Store};

        let app = local(builder())
            .with_mcp_settings(on(true))
            .with_lookout(|mut config| {
                config.ingest.flush_interval_ms = 3_600_000;
                config.enabled(true).local(true)
            })
            .with_routes(|router| {
                router.get("/posts", || async { "posts" }).name("posts.index");
            })
            .boot()
            .await
            .unwrap();
        let app = TestApp::new(app).unwrap();

        app.get("/posts").await.assert_ok();
        app.send(rpc("tools/call", json!({ "name": "lookout_batches" }), None)).await.assert_ok();

        app.resolve::<rainier_events::Dispatcher>().unwrap().settled().await;
        app.resolve::<Lookout>().unwrap().flush().await;
        let store = app.resolve::<std::sync::Arc<dyn Store>>().unwrap();
        let routes: Vec<Option<String>> = store
            .batches(BatchQuery::default())
            .await
            .unwrap()
            .into_iter()
            .map(|b| b.route_name)
            .collect();
        assert!(routes.contains(&Some("posts.index".into())), "{routes:?}");
        assert!(!routes.contains(&Some("mcp".into())), "{routes:?}");
    }

    #[tokio::test]
    async fn locally_it_is_open_unless_told_otherwise() {
        let app = TestApp::new(local(builder()).with_mcp_settings(on(true)).boot().await.unwrap())
            .unwrap();
        app.send(rpc("ping", json!({}), None)).await.assert_json_path("result", json!({}));

        let app = TestApp::new(local(builder()).with_mcp_settings(on(false)).boot().await.unwrap())
            .unwrap();
        app.send(rpc("ping", json!({}), None)).await.assert_not_found();
    }
}
