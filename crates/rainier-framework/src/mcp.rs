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
//!    `MCP_OPEN_LOCALLY=false`), a request bearing one of `MCP_TOKENS`, or
//!    whoever [`Rainier::with_mcp_access`](crate::Rainier::with_mcp_access)
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
    /// between the internet and every tool, Lookout's included.
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
        let settings = Self {
            enabled: env.bool("MCP_ENABLED", false),
            path: env.string("MCP_PATH", "/_mcp"),
            tokens: list("MCP_TOKENS"),
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
}

#[cfg(test)]
mod boot_tests {
    use rainier_http::{Method, Request};
    use serde_json::{json, Value};

    use super::McpSettings;
    use crate::testing::TestApp;
    use crate::Rainier;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    fn on(local: bool) -> impl FnOnce(McpSettings) -> McpSettings {
        move |settings| McpSettings {
            enabled: true,
            tokens: vec![TOKEN.into()],
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
            server.tool(
                super::Tool::new("ping", "Answer pong.", |_| async {
                    Ok(super::ToolResult::text("pong"))
                })
                .read_only(),
            )
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
