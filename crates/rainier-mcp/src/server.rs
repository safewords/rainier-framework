//! The server: what it offers, and the JSON-RPC methods that reach it.

use std::future::Future;
use std::sync::Arc;

use rainier_http::Request;
use rainier_support::{BoxFuture, Error, Result};
use serde::Serialize;
use serde_json::{json, Map, Value};

use crate::protocol::{self, codes};

/// The protocol revision this server prefers, and answers a client that asks
/// for one it does not know.
pub const LATEST_PROTOCOL_VERSION: &str = "2025-11-25";

/// Every protocol revision this server speaks, newest first. The methods it
/// implements are the same in all of them.
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] =
    &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// What a tool, a prompt or a resource reader is handed: the arguments, and
/// the HTTP request that carried the call when there was one.
#[derive(Debug, Clone, Default)]
pub struct Call {
    arguments: Map<String, Value>,
    request: Option<Arc<Request>>,
}

impl Call {
    /// A call with `arguments`, carried by `request`.
    pub fn new(arguments: Map<String, Value>, request: Option<Arc<Request>>) -> Self {
        Self { arguments, request }
    }

    /// Every argument, as the client sent them.
    pub fn arguments(&self) -> &Map<String, Value> {
        &self.arguments
    }

    /// One argument.
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.arguments.get(name).filter(|value| !value.is_null())
    }

    /// One argument, when it is a string.
    pub fn str(&self, name: &str) -> Option<&str> {
        self.get(name).and_then(Value::as_str)
    }

    /// The HTTP request that carried the call — its headers, for a tool that
    /// decides by who is asking. `None` when the call did not come over HTTP.
    pub fn request(&self) -> Option<&Request> {
        self.request.as_deref()
    }
}

/// One piece of a tool's answer.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Content {
    /// Text, which is what a model reads.
    Text(String),
}

impl Content {
    fn to_value(&self) -> Value {
        match self {
            Content::Text(text) => json!({ "type": "text", "text": text }),
        }
    }
}

/// What a tool answers.
///
/// A tool that fails in a way the model should see and can correct — a bad
/// argument, nothing found — answers [`ToolResult::error`]. One that returns
/// `Err` from its handler is reported the same way, with the error's message
/// (and its details, for a validation error); a server error's message is
/// logged instead, and the model is told only that the tool failed.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolResult {
    content: Vec<Content>,
    structured: Option<Value>,
    is_error: bool,
}

impl ToolResult {
    /// A text answer.
    pub fn text(text: impl Into<String>) -> Self {
        Self { content: vec![Content::Text(text.into())], structured: None, is_error: false }
    }

    /// A value, serialised as JSON text.
    ///
    /// The text is compact: a model reads JSON as well either way, and
    /// whitespace costs tokens.
    pub fn json(value: &impl Serialize) -> Self {
        match serde_json::to_string(value) {
            Ok(text) => Self::text(text),
            Err(e) => Self::error(format!("The answer could not be serialised: {e}")),
        }
    }

    /// A failure the model is shown, so it can try again differently.
    pub fn error(message: impl Into<String>) -> Self {
        Self { is_error: true, ..Self::text(message) }
    }

    /// Also answer `value` as `structuredContent`, for a client that reads
    /// it. It must be a JSON object.
    #[must_use]
    pub fn with_structured(mut self, value: Value) -> Self {
        self.structured = Some(value);
        self
    }

    /// Whether this is a failure.
    pub fn is_error(&self) -> bool {
        self.is_error
    }

    /// The pieces of the answer.
    pub fn content(&self) -> &[Content] {
        &self.content
    }

    /// The answer's text, every piece joined — mostly for tests.
    pub fn text_content(&self) -> String {
        self.content
            .iter()
            .map(|c| match c {
                Content::Text(text) => text.as_str(),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn to_value(&self) -> Value {
        let mut value = json!({
            "content": self.content.iter().map(Content::to_value).collect::<Vec<_>>(),
            "isError": self.is_error,
        });
        if let Some(structured) = &self.structured {
            value["structuredContent"] = structured.clone();
        }
        value
    }

    /// What a handler's `Err` becomes.
    fn from_error(tool: &str, error: &Error) -> Self {
        if error.kind().is_server_error() {
            tracing::error!(target: "rainier_mcp", tool, error = %error.message(), "mcp: a tool failed");
            return Self::error("The tool failed. The server's log has the reason.");
        }
        match error.details() {
            Some(details) => Self::error(format!("{} {details}", error.message())),
            None => Self::error(error.message().to_string()),
        }
    }
}

type ToolHandler = Arc<dyn Fn(Call) -> BoxFuture<'static, Result<ToolResult>> + Send + Sync>;

/// Hints about what a tool does, for the client deciding whether to ask
/// before it runs one. Hints, not guarantees: a client must not trust them
/// from a server it does not trust.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolAnnotations {
    /// It changes nothing.
    pub read_only: Option<bool>,
    /// What it changes may not be undone.
    pub destructive: Option<bool>,
    /// Calling it twice with the same arguments does no more than once.
    pub idempotent: Option<bool>,
    /// It reaches outside this application.
    pub open_world: Option<bool>,
}

/// A function the model may call.
///
/// ```
/// use rainier_mcp::{Tool, ToolResult};
/// use serde_json::json;
///
/// let echo = Tool::new("echo", "Answer with the text it was given.", |call| async move {
///     Ok(ToolResult::text(call.str("text").unwrap_or_default()))
/// })
/// .input_schema(json!({
///     "type": "object",
///     "properties": { "text": { "type": "string" } },
///     "required": ["text"],
/// }))
/// .read_only();
/// # let _ = echo;
/// ```
#[derive(Clone)]
pub struct Tool {
    name: String,
    title: Option<String>,
    description: String,
    input_schema: Value,
    annotations: ToolAnnotations,
    handler: ToolHandler,
}

impl std::fmt::Debug for Tool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tool").field("name", &self.name).finish_non_exhaustive()
    }
}

impl Tool {
    /// A tool named `name` that takes no arguments until
    /// [`input_schema`](Self::input_schema) says otherwise.
    ///
    /// The description is what the model decides by: say what it does, when
    /// to use it, and what it answers.
    pub fn new<F, Fut>(name: impl Into<String>, description: impl Into<String>, handler: F) -> Self
    where
        F: Fn(Call) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<ToolResult>> + Send + 'static,
    {
        Self {
            name: name.into(),
            title: None,
            description: description.into(),
            input_schema: json!({ "type": "object", "properties": {} }),
            annotations: ToolAnnotations::default(),
            handler: Arc::new(move |call| Box::pin(handler(call))),
        }
    }

    /// A name for people, where the client shows one.
    #[must_use]
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// The JSON Schema of its arguments — an object schema.
    #[must_use]
    pub fn input_schema(mut self, schema: Value) -> Self {
        self.input_schema = schema;
        self
    }

    /// It only reads.
    #[must_use]
    pub fn read_only(mut self) -> Self {
        self.annotations.read_only = Some(true);
        self.annotations.destructive = Some(false);
        self
    }

    /// The hints, all at once.
    #[must_use]
    pub fn annotations(mut self, annotations: ToolAnnotations) -> Self {
        self.annotations = annotations;
        self
    }

    /// Its name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Its description.
    pub fn description(&self) -> &str {
        &self.description
    }

    /// Run it, as `tools/call` does — for tests.
    pub async fn call(&self, call: Call) -> ToolResult {
        match (self.handler)(call).await {
            Ok(result) => result,
            Err(error) => ToolResult::from_error(&self.name, &error),
        }
    }

    fn describe(&self) -> Value {
        let mut value = json!({
            "name": self.name,
            "description": self.description,
            "inputSchema": self.input_schema,
        });
        if let Some(title) = &self.title {
            value["title"] = json!(title);
        }
        let a = &self.annotations;
        let mut hints = Map::new();
        for (key, hint) in [
            ("readOnlyHint", a.read_only),
            ("destructiveHint", a.destructive),
            ("idempotentHint", a.idempotent),
            ("openWorldHint", a.open_world),
        ] {
            if let Some(hint) = hint {
                hints.insert(key.into(), json!(hint));
            }
        }
        if let Some(title) = &self.title {
            hints.insert("title".into(), json!(title));
        }
        if !hints.is_empty() {
            value["annotations"] = Value::Object(hints);
        }
        value
    }
}

type ResourceReader = Arc<dyn Fn(Call) -> BoxFuture<'static, Result<String>> + Send + Sync>;

/// A document the client may read and put in front of the model.
#[derive(Clone)]
pub struct Resource {
    uri: String,
    name: String,
    description: Option<String>,
    mime_type: String,
    reader: ResourceReader,
}

impl std::fmt::Debug for Resource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Resource").field("uri", &self.uri).finish_non_exhaustive()
    }
}

impl Resource {
    /// A resource at `uri`, read by `reader` each time it is asked for.
    /// Plain text until [`mime_type`](Self::mime_type) says otherwise.
    pub fn new<F, Fut>(uri: impl Into<String>, name: impl Into<String>, reader: F) -> Self
    where
        F: Fn(Call) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<String>> + Send + 'static,
    {
        Self {
            uri: uri.into(),
            name: name.into(),
            description: None,
            mime_type: "text/plain".into(),
            reader: Arc::new(move |call| Box::pin(reader(call))),
        }
    }

    /// A resource whose text never changes.
    pub fn text(uri: impl Into<String>, name: impl Into<String>, text: impl Into<String>) -> Self {
        let text: Arc<str> = Arc::from(text.into());
        Self::new(uri, name, move |_| {
            let text = Arc::clone(&text);
            async move { Ok(text.to_string()) }
        })
    }

    /// What it is.
    #[must_use]
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Its media type: `text/markdown`, `application/json`.
    #[must_use]
    pub fn mime_type(mut self, mime_type: impl Into<String>) -> Self {
        self.mime_type = mime_type.into();
        self
    }

    /// Its URI.
    pub fn uri(&self) -> &str {
        &self.uri
    }

    fn describe(&self) -> Value {
        let mut value = json!({ "uri": self.uri, "name": self.name, "mimeType": self.mime_type });
        if let Some(description) = &self.description {
            value["description"] = json!(description);
        }
        value
    }
}

/// One argument a [`Prompt`] takes.
#[derive(Debug, Clone, PartialEq)]
pub struct PromptArgument {
    /// Its name, as the template writes it: `{{name}}`.
    pub name: String,
    /// What to give.
    pub description: String,
    /// Whether the prompt refuses to render without it.
    pub required: bool,
}

type PromptRenderer = Arc<dyn Fn(Call) -> BoxFuture<'static, Result<String>> + Send + Sync>;

/// A ready-made instruction the user can pick — a client shows prompts as
/// commands — rendered into a user message.
///
/// ```
/// use rainier_mcp::Prompt;
///
/// let triage = Prompt::template(
///     "triage",
///     "Look into one failing route.",
///     "Find out why {{route}} is failing, and say what to change.",
/// )
/// .argument("route", "The route's name", true);
/// # let _ = triage;
/// ```
#[derive(Clone)]
pub struct Prompt {
    name: String,
    description: String,
    arguments: Vec<PromptArgument>,
    render: PromptRenderer,
}

impl std::fmt::Debug for Prompt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Prompt").field("name", &self.name).finish_non_exhaustive()
    }
}

impl Prompt {
    /// A prompt rendered by `render` from the arguments it was given.
    pub fn new<F, Fut>(name: impl Into<String>, description: impl Into<String>, render: F) -> Self
    where
        F: Fn(Call) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<String>> + Send + 'static,
    {
        Self {
            name: name.into(),
            description: description.into(),
            arguments: Vec::new(),
            render: Arc::new(move |call| Box::pin(render(call))),
        }
    }

    /// A prompt that is `template` with each `{{argument}}` replaced by the
    /// value given for it — by nothing, for an optional one left out.
    pub fn template(
        name: impl Into<String>,
        description: impl Into<String>,
        template: impl Into<String>,
    ) -> Self {
        let template: Arc<str> = Arc::from(template.into());
        Self::new(name, description, move |call| {
            let template = Arc::clone(&template);
            async move { Ok(fill(&template, call.arguments())) }
        })
    }

    /// Say it takes `name`.
    #[must_use]
    pub fn argument(
        mut self,
        name: impl Into<String>,
        description: impl Into<String>,
        required: bool,
    ) -> Self {
        self.arguments.push(PromptArgument {
            name: name.into(),
            description: description.into(),
            required,
        });
        self
    }

    /// Its name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Render it, as `prompts/get` does.
    pub async fn render(&self, call: Call) -> Result<String> {
        for argument in self.arguments.iter().filter(|a| a.required) {
            let given = call.str(&argument.name).is_some_and(|v| !v.trim().is_empty());
            if !given {
                return Err(Error::bad_request(format!(
                    "The prompt `{}` needs the argument `{}`.",
                    self.name, argument.name
                )));
            }
        }
        (self.render)(call).await
    }

    fn describe(&self) -> Value {
        json!({
            "name": self.name,
            "description": self.description,
            "arguments": self.arguments.iter().map(|a| json!({
                "name": a.name,
                "description": a.description,
                "required": a.required,
            })).collect::<Vec<_>>(),
        })
    }
}

/// `template` with each `{{name}}` replaced by the string argument `name`.
fn fill(template: &str, arguments: &Map<String, Value>) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find("}}") {
            Some(end) => {
                let name = after[..end].trim();
                match arguments.get(name) {
                    Some(Value::String(s)) => out.push_str(s),
                    Some(Value::Null) | None => {}
                    Some(other) => out.push_str(&other.to_string()),
                }
                rest = &after[end + 2..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// A Model Context Protocol server: its name, what it tells the model about
/// itself, and the tools, resources and prompts it offers.
///
/// It is transport-free: [`handle`](Self::handle) takes one JSON-RPC message
/// (or a batch) and answers it. [`McpEndpoint`](crate::McpEndpoint) serves it
/// over HTTP.
#[derive(Debug, Clone)]
pub struct McpServer {
    name: String,
    version: String,
    title: Option<String>,
    instructions: Vec<String>,
    tools: Vec<Tool>,
    resources: Vec<Resource>,
    prompts: Vec<Prompt>,
}

impl McpServer {
    /// A server that calls itself `name` at `version`, offering nothing yet.
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
            title: None,
            instructions: Vec::new(),
            tools: Vec::new(),
            resources: Vec::new(),
            prompts: Vec::new(),
        }
    }

    /// A name for people.
    #[must_use]
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Add to what the client is told about this server when it connects —
    /// a client puts it in front of the model. Each call adds a paragraph.
    #[must_use]
    pub fn instructions(mut self, text: impl Into<String>) -> Self {
        self.instructions.push(text.into());
        self
    }

    /// Offer `tool`, replacing one of the same name.
    #[must_use]
    pub fn tool(mut self, tool: Tool) -> Self {
        self.tools.retain(|t| t.name != tool.name);
        self.tools.push(tool);
        self
    }

    /// Offer `resource`, replacing one at the same URI.
    #[must_use]
    pub fn resource(mut self, resource: Resource) -> Self {
        self.resources.retain(|r| r.uri != resource.uri);
        self.resources.push(resource);
        self
    }

    /// Offer `prompt`, replacing one of the same name.
    #[must_use]
    pub fn prompt(mut self, prompt: Prompt) -> Self {
        self.prompts.retain(|p| p.name != prompt.name);
        self.prompts.push(prompt);
        self
    }

    /// Its name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The tools it offers, in the order they were added.
    pub fn tools(&self) -> &[Tool] {
        &self.tools
    }

    /// The prompts it offers.
    pub fn prompts(&self) -> &[Prompt] {
        &self.prompts
    }

    /// The resources it offers.
    pub fn resources(&self) -> &[Resource] {
        &self.resources
    }

    /// One tool, by name.
    pub fn find_tool(&self, name: &str) -> Option<&Tool> {
        self.tools.iter().find(|t| t.name == name)
    }

    /// Answer a request body: one JSON-RPC message, or a batch of them.
    ///
    /// `None` when there is nothing to answer — the body held only
    /// notifications or responses.
    pub async fn handle_str(&self, body: &str, request: Option<Arc<Request>>) -> Option<Value> {
        match serde_json::from_str::<Value>(body) {
            Ok(message) => self.handle(message, request).await,
            Err(e) => Some(protocol::error(Value::Null, codes::PARSE_ERROR, format!("{e}"))),
        }
    }

    /// Answer one JSON-RPC message, or a batch of them.
    pub async fn handle(&self, message: Value, request: Option<Arc<Request>>) -> Option<Value> {
        match message {
            Value::Array(batch) if batch.is_empty() => {
                Some(protocol::error(Value::Null, codes::INVALID_REQUEST, "An empty batch."))
            }
            Value::Array(batch) => {
                let mut answers = Vec::new();
                for message in batch {
                    if let Some(answer) = self.handle_one(message, request.clone()).await {
                        answers.push(answer);
                    }
                }
                (!answers.is_empty()).then_some(Value::Array(answers))
            }
            message => self.handle_one(message, request).await,
        }
    }

    async fn handle_one(&self, message: Value, request: Option<Arc<Request>>) -> Option<Value> {
        let Value::Object(message) = message else {
            return Some(protocol::error(
                Value::Null,
                codes::INVALID_REQUEST,
                "A message is a JSON object.",
            ));
        };
        let id = message.get("id").cloned();
        if message.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return Some(protocol::error(
                id.unwrap_or(Value::Null),
                codes::INVALID_REQUEST,
                "The jsonrpc member must be \"2.0\".",
            ));
        }
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            // A response to something this server asked — it asks nothing —
            // or nonsense. Neither is answered.
            return None;
        };
        // A notification: `notifications/initialized`, `notifications/cancelled`.
        // Nothing this server does outlives its answer, so none needs acting on.
        let id = id?;
        let params = match message.get("params") {
            None | Some(Value::Null) => Map::new(),
            Some(Value::Object(params)) => params.clone(),
            Some(_) => {
                return Some(protocol::error(
                    id,
                    codes::INVALID_PARAMS,
                    "params must be an object.",
                ))
            }
        };
        Some(match self.method(method, params, request).await {
            Ok(result) => protocol::result(id, result),
            Err((code, message)) => protocol::error(id, code, message),
        })
    }

    async fn method(
        &self,
        method: &str,
        params: Map<String, Value>,
        request: Option<Arc<Request>>,
    ) -> std::result::Result<Value, (i64, String)> {
        match method {
            "initialize" => Ok(self.initialize(&params)),
            "ping" => Ok(json!({})),
            "tools/list" => {
                Ok(json!({ "tools": self.tools.iter().map(Tool::describe).collect::<Vec<_>>() }))
            }
            "tools/call" => {
                let name = string_param(&params, "name")?;
                let Some(tool) = self.find_tool(name) else {
                    return Err((codes::INVALID_PARAMS, format!("Unknown tool: {name}")));
                };
                let arguments = object_param(&params, "arguments")?;
                Ok(tool.call(Call::new(arguments, request)).await.to_value())
            }
            "resources/list" => Ok(json!({
                "resources": self.resources.iter().map(Resource::describe).collect::<Vec<_>>(),
            })),
            "resources/templates/list" => Ok(json!({ "resourceTemplates": [] })),
            "resources/read" => {
                let uri = string_param(&params, "uri")?;
                let Some(resource) = self.resources.iter().find(|r| r.uri == uri) else {
                    return Err((codes::RESOURCE_NOT_FOUND, format!("Resource not found: {uri}")));
                };
                let mut arguments = Map::new();
                arguments.insert("uri".into(), json!(uri));
                let text = (resource.reader)(Call::new(arguments, request))
                    .await
                    .map_err(|e| (codes::INTERNAL_ERROR, e.message().to_string()))?;
                Ok(json!({
                    "contents": [{ "uri": uri, "mimeType": resource.mime_type, "text": text }],
                }))
            }
            "prompts/list" => Ok(
                json!({ "prompts": self.prompts.iter().map(Prompt::describe).collect::<Vec<_>>() }),
            ),
            "prompts/get" => {
                let name = string_param(&params, "name")?;
                let Some(prompt) = self.prompts.iter().find(|p| p.name == name) else {
                    return Err((codes::INVALID_PARAMS, format!("Unknown prompt: {name}")));
                };
                let arguments = object_param(&params, "arguments")?;
                let text = prompt.render(Call::new(arguments, request)).await.map_err(|e| {
                    let code = if e.kind().is_server_error() {
                        codes::INTERNAL_ERROR
                    } else {
                        codes::INVALID_PARAMS
                    };
                    (code, e.message().to_string())
                })?;
                Ok(json!({
                    "description": prompt.description,
                    "messages": [{ "role": "user", "content": { "type": "text", "text": text } }],
                }))
            }
            "logging/setLevel" => Ok(json!({})),
            other => Err((codes::METHOD_NOT_FOUND, format!("Method not found: {other}"))),
        }
    }

    fn initialize(&self, params: &Map<String, Value>) -> Value {
        let asked = params.get("protocolVersion").and_then(Value::as_str);
        let version = match asked {
            Some(v) if SUPPORTED_PROTOCOL_VERSIONS.contains(&v) => v,
            _ => LATEST_PROTOCOL_VERSION,
        };
        let mut capabilities = json!({ "tools": { "listChanged": false } });
        if !self.resources.is_empty() {
            capabilities["resources"] = json!({ "subscribe": false, "listChanged": false });
        }
        if !self.prompts.is_empty() {
            capabilities["prompts"] = json!({ "listChanged": false });
        }
        let mut info = json!({ "name": self.name, "version": self.version });
        if let Some(title) = &self.title {
            info["title"] = json!(title);
        }
        let mut result = json!({
            "protocolVersion": version,
            "capabilities": capabilities,
            "serverInfo": info,
        });
        if !self.instructions.is_empty() {
            result["instructions"] = json!(self.instructions.join("\n\n"));
        }
        result
    }
}

fn string_param<'a>(
    params: &'a Map<String, Value>,
    name: &str,
) -> std::result::Result<&'a str, (i64, String)> {
    params
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| (codes::INVALID_PARAMS, format!("params.{name} must be a string.")))
}

fn object_param(
    params: &Map<String, Value>,
    name: &str,
) -> std::result::Result<Map<String, Value>, (i64, String)> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(Map::new()),
        Some(Value::Object(map)) => Ok(map.clone()),
        Some(_) => Err((codes::INVALID_PARAMS, format!("params.{name} must be an object."))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_replaces_arguments_and_drops_missing_ones() {
        let mut args = Map::new();
        args.insert("route".into(), json!("posts.index"));
        args.insert("n".into(), json!(3));
        assert_eq!(
            fill("why is {{ route }} slow ({{n}}){{missing}}? {{unclosed", &args),
            "why is posts.index slow (3)? {{unclosed"
        );
    }
}
