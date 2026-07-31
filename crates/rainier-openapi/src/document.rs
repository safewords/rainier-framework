//! The document — [`OpenApi`], built from the router.

use std::collections::BTreeMap;

use serde_json::{json, Map, Value};

use rainier_routing::CompiledRouter;
use rainier_validation::RuleSet;

use crate::schema::schema_for;

/// One endpoint's extra description — what the router cannot know.
///
/// The router has the method, the path and the name. It does not have a
/// summary, and it does not know which [request contract](rainier_validation)
/// an action takes, because a handler's parameters are erased by the time it is
/// a `RouteHandler`. So that part is declared.
#[derive(Debug, Clone, Default)]
pub struct Endpoint {
    summary: Option<String>,
    description: Option<String>,
    tags: Vec<String>,
    request: Option<RuleSet>,
    responses: BTreeMap<u16, String>,
    deprecated: bool,
}

impl Endpoint {
    /// An endpoint with nothing said about it yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// A one-line summary.
    pub fn summary(mut self, summary: impl Into<String>) -> Self {
        self.summary = Some(summary.into());
        self
    }

    /// The longer description.
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Group it under a tag.
    pub fn tag(mut self, tag: impl Into<String>) -> Self {
        self.tags.push(tag.into());
        self
    }

    /// The body this endpoint accepts, from a contract's rules.
    ///
    /// ```ignore
    /// Endpoint::new().accepts(StorePostRequest::rules())
    /// ```
    ///
    /// The schema is **derived from the rules the validator runs**, so it
    /// cannot describe a body the endpoint would reject — which is the failure
    /// mode of every hand-written OpenAPI file.
    pub fn accepts(mut self, rules: RuleSet) -> Self {
        self.request = Some(rules);
        self
    }

    /// Document a response.
    pub fn returns(mut self, status: u16, description: impl Into<String>) -> Self {
        self.responses.insert(status, description.into());
        self
    }

    /// Mark it deprecated.
    pub fn deprecated(mut self) -> Self {
        self.deprecated = true;
        self
    }
}

/// An OpenAPI 3.1 document.
///
/// ```ignore
/// let document = OpenApi::new("Rainier Sample", "1.0.0")
///     .server("https://api.example.com")
///     .describe("api.posts.store", Endpoint::new()
///         .summary("Create a draft")
///         .accepts(StorePostRequest::rules())
///         .returns(201, "The created post"))
///     .build(&router);
/// ```
///
/// # What is generated and what is declared
///
/// **Generated** from the compiled router: every path, its methods, its path
/// parameters, and whether it sits behind authentication middleware. That half
/// cannot go stale, because it is read from the routes that are actually
/// served.
///
/// **Declared** per endpoint: the summary, the tags, the request body's rules
/// and the responses. Rust erases a handler's parameter types by the time the
/// router holds it, so there is nothing to introspect — and a document that
/// guessed would be worse than one that admits it was told.
///
/// An endpoint nobody described still appears, with its path and parameters.
/// A document that omitted undocumented routes would be a document that hides
/// exactly what you have forgotten about.
pub struct OpenApi {
    title: String,
    version: String,
    description: Option<String>,
    servers: Vec<String>,
    endpoints: BTreeMap<String, Endpoint>,
}

impl OpenApi {
    /// A document for an API called `title` at `version`.
    pub fn new(title: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            version: version.into(),
            description: None,
            servers: Vec::new(),
            endpoints: BTreeMap::new(),
        }
    }

    /// The API's description.
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Rename it.
    ///
    /// The title and the version are deployment facts rather than code, so
    /// configuration sets them over whatever the document was built with.
    pub fn titled(mut self, title: impl Into<String>, version: impl Into<String>) -> Self {
        self.title = title.into();
        self.version = version.into();
        self
    }

    /// A base URL clients should use.
    pub fn server(mut self, url: impl Into<String>) -> Self {
        self.servers.push(url.into());
        self
    }

    /// Describe the route with this **name**.
    ///
    /// By name rather than by path, because a path plus a method is two things
    /// to keep in step and a name is one — and an unnamed route cannot be
    /// linked to anyway.
    pub fn describe(mut self, route: impl Into<String>, endpoint: Endpoint) -> Self {
        self.endpoints.insert(route.into(), endpoint);
        self
    }

    /// Whether this route has a description.
    ///
    /// The half of the coverage question [`dangling`](Self::dangling) does not
    /// answer. A document is generated from the router, so every route appears
    /// in it either way — an undescribed one appears with no summary, no body
    /// and no responses, which reads as a documented endpoint rather than a
    /// missing description.
    pub fn describes(&self, route: &str) -> bool {
        self.endpoints.contains_key(route)
    }

    /// The description of `route`, if it has one.
    pub fn endpoint(&self, route: &str) -> Option<&Endpoint> {
        self.endpoints.get(route)
    }

    /// Every route name this document describes.
    pub fn described(&self) -> Vec<&str> {
        self.endpoints.keys().map(String::as_str).collect()
    }

    /// Named routes with no description.
    ///
    /// The assertion worth putting in a test: a new endpoint arrives
    /// documented, or the test that says so fails. Unnamed routes are not
    /// listed — they cannot be described, since descriptions are keyed by
    /// name.
    pub fn undocumented(&self, router: &CompiledRouter) -> Vec<String> {
        router
            .describe()
            .into_iter()
            .filter_map(|row| row.name)
            .filter(|name| !self.describes(name))
            .collect()
    }

    /// Which described routes do not exist.
    ///
    /// A renamed route silently loses its documentation otherwise. Worth
    /// asserting on in a test — it is the one failure this design has, and it
    /// is cheap to catch.
    pub fn dangling(&self, router: &CompiledRouter) -> Vec<String> {
        let names: Vec<String> = router.describe().into_iter().filter_map(|row| row.name).collect();

        self.endpoints.keys().filter(|described| !names.contains(described)).cloned().collect()
    }

    /// Build the document.
    pub fn build(&self, router: &CompiledRouter) -> Value {
        let mut paths: Map<String, Value> = Map::new();

        for route in router.describe() {
            let path = to_openapi_path(&route.uri);
            let entry = paths.entry(path).or_insert_with(|| json!({}));

            let described = route.name.as_ref().and_then(|name| self.endpoints.get(name));

            for method in &route.methods {
                // HEAD is generated by the router for every GET, and a document
                // listing both says nothing extra while doubling its length.
                if method.eq_ignore_ascii_case("head") {
                    continue;
                }

                entry[method.to_lowercase()] = self.operation(&route, described, &route.uri);
            }
        }

        let mut document = json!({
            "openapi": "3.1.0",
            "info": { "title": self.title, "version": self.version },
            "paths": paths,
        });

        if let Some(description) = &self.description {
            document["info"]["description"] = json!(description);
        }
        if !self.servers.is_empty() {
            document["servers"] =
                Value::Array(self.servers.iter().map(|url| json!({ "url": url })).collect());
        }

        // Only declared when something uses it, so a document for an API with
        // no authentication does not advertise a scheme nobody honours.
        if router.describe().iter().any(|route| is_guarded(&route.middleware)) {
            document["components"] = json!({
                "securitySchemes": {
                    "bearerAuth": { "type": "http", "scheme": "bearer" }
                }
            });
        }

        document
    }

    /// The document as pretty JSON.
    pub fn to_json(&self, router: &CompiledRouter) -> String {
        serde_json::to_string_pretty(&self.build(router)).unwrap_or_else(|_| "{}".into())
    }

    fn operation(
        &self,
        route: &rainier_routing::RouteSummary,
        described: Option<&Endpoint>,
        uri: &str,
    ) -> Value {
        let mut operation = Map::new();

        if let Some(name) = &route.name {
            operation.insert("operationId".into(), json!(name));
        }

        if let Some(endpoint) = described {
            if let Some(summary) = &endpoint.summary {
                operation.insert("summary".into(), json!(summary));
            }
            if let Some(description) = &endpoint.description {
                operation.insert("description".into(), json!(description));
            }
            if !endpoint.tags.is_empty() {
                operation.insert("tags".into(), json!(endpoint.tags));
            }
            if endpoint.deprecated {
                operation.insert("deprecated".into(), json!(true));
            }
            if let Some(rules) = &endpoint.request {
                operation.insert(
                    "requestBody".into(),
                    json!({
                        "required": true,
                        "content": { "application/json": { "schema": schema_for(rules) } }
                    }),
                );
            }
        }

        let parameters = path_parameters(uri);
        if !parameters.is_empty() {
            operation.insert("parameters".into(), Value::Array(parameters));
        }

        operation.insert("responses".into(), responses_for(described, &route.middleware));

        if is_guarded(&route.middleware) {
            operation.insert("security".into(), json!([{ "bearerAuth": [] }]));
        }

        Value::Object(operation)
    }
}

/// `/posts/{post}` is already OpenAPI's spelling — Rainier uses the same
/// braces, so there is nothing to translate.
fn to_openapi_path(uri: &str) -> String {
    if uri.starts_with('/') {
        uri.to_string()
    } else {
        format!("/{uri}")
    }
}

/// A `parameters` entry per `{name}` in the path.
///
/// Always required and always a string: a path parameter that could be absent
/// would be a different path, and everything in a URL is text until something
/// parses it.
fn path_parameters(uri: &str) -> Vec<Value> {
    uri.split('/')
        .filter_map(|segment| segment.strip_prefix('{')?.strip_suffix('}'))
        .map(|name| {
            json!({
                "name": name,
                "in": "path",
                "required": true,
                "schema": { "type": "string" }
            })
        })
        .collect()
}

/// The responses, with the ones the framework produces filled in.
///
/// A `401` on a guarded route and a `422` on one that validates are not things
/// an author should have to remember: they are what the middleware and the
/// contract will actually return.
fn responses_for(described: Option<&Endpoint>, middleware: &[&str]) -> Value {
    let mut responses = Map::new();

    for (status, description) in described.map(|e| &e.responses).into_iter().flatten() {
        responses.insert(status.to_string(), json!({ "description": description }));
    }

    if responses.is_empty() {
        responses.insert("200".into(), json!({ "description": "OK" }));
    }

    if described.is_some_and(|endpoint| endpoint.request.is_some()) {
        responses
            .entry("422")
            .or_insert_with(|| json!({ "description": "The input failed validation" }));
    }
    if is_guarded(middleware) {
        responses.entry("401").or_insert_with(|| json!({ "description": "Unauthenticated" }));
    }

    Value::Object(responses)
}

/// Whether a route's middleware includes something that authenticates.
///
/// By name, which is the one place this crate has to guess — the pipeline
/// exposes labels rather than types. A middleware called something else that
/// authenticates will not be spotted, so the document under-claims rather than
/// over-claims, which is the right direction for a security statement.
fn is_guarded(middleware: &[&str]) -> bool {
    middleware.iter().any(|name| name.contains("Authenticate"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_container::Container;
    use rainier_routing::Router;
    use rainier_validation::{field, Rule};

    fn router() -> CompiledRouter {
        let mut router = Router::new();
        router.get("/posts", || async { "ok" }).name("posts.index");
        router.get("/posts/{post}", || async { "ok" }).name("posts.show");
        router.post("/posts", || async { "ok" }).name("posts.store");

        router.compile(&Container::new()).expect("compiles")
    }

    fn document() -> OpenApi {
        OpenApi::new("Test API", "1.0.0").describe(
            "posts.store",
            Endpoint::new()
                .summary("Create a post")
                .tag("Posts")
                .accepts(vec![field("title", [Rule::Required, Rule::String, Rule::Max(120.0)])])
                .returns(201, "The created post"),
        )
    }

    #[test]
    fn every_route_appears_even_undocumented_ones() {
        // A document that omitted them would hide what you have forgotten.
        let built = document().build(&router());
        let paths = built["paths"].as_object().expect("paths");

        assert!(paths.contains_key("/posts"));
        assert!(paths.contains_key("/posts/{post}"));
        assert!(paths["/posts"]["get"].is_object(), "undocumented, but present");
    }

    #[test]
    fn head_is_not_listed_beside_every_get() {
        let built = document().build(&router());

        assert!(built["paths"]["/posts"]["head"].is_null(), "{built}");
    }

    #[test]
    fn a_path_parameter_becomes_a_parameter_entry() {
        let built = document().build(&router());
        let parameters = &built["paths"]["/posts/{post}"]["get"]["parameters"];

        assert_eq!(parameters[0]["name"], "post");
        assert_eq!(parameters[0]["in"], "path");
        assert_eq!(parameters[0]["required"], true);
    }

    #[test]
    fn a_contract_becomes_the_request_body_schema() {
        let built = document().build(&router());
        let schema = &built["paths"]["/posts"]["post"]["requestBody"]["content"]
            ["application/json"]["schema"];

        assert_eq!(schema["required"], json!(["title"]));
        assert_eq!(schema["properties"]["title"]["maxLength"], 120);
    }

    #[test]
    fn an_endpoint_that_validates_documents_its_422() {
        // Nobody should have to remember this: it is what the contract does.
        let built = document().build(&router());
        let responses = &built["paths"]["/posts"]["post"]["responses"];

        assert_eq!(responses["201"]["description"], "The created post");
        assert!(responses["422"].is_object(), "{responses}");
    }

    #[test]
    fn the_route_name_becomes_the_operation_id() {
        let built = document().build(&router());

        assert_eq!(built["paths"]["/posts/{post}"]["get"]["operationId"], "posts.show");
    }

    #[test]
    fn a_document_for_an_unguarded_api_declares_no_security_scheme() {
        let built = document().build(&router());

        assert!(built["components"].is_null(), "nothing here authenticates: {built}");
        assert!(built["paths"]["/posts"]["get"]["security"].is_null());
    }

    #[test]
    fn describing_a_route_that_no_longer_exists_is_reported() {
        // The one way this design rots: a rename orphans its documentation.
        let document = document().describe("posts.renamed-away", Endpoint::new());

        assert_eq!(document.dangling(&router()), vec!["posts.renamed-away".to_string()]);
    }

    #[test]
    fn a_document_with_no_dangling_references_reports_none() {
        assert!(document().dangling(&router()).is_empty());
    }

    #[test]
    fn the_info_block_carries_what_it_was_given() {
        let built = OpenApi::new("Test API", "2.1.0")
            .description("An API")
            .server("https://api.example.com")
            .build(&router());

        assert_eq!(built["openapi"], "3.1.0");
        assert_eq!(built["info"]["title"], "Test API");
        assert_eq!(built["info"]["version"], "2.1.0");
        assert_eq!(built["info"]["description"], "An API");
        assert_eq!(built["servers"][0]["url"], "https://api.example.com");
    }

    #[test]
    fn a_document_says_which_routes_it_describes() {
        let document = OpenApi::new("Test", "1.0")
            .describe("posts.index", Endpoint::new().summary("List posts"));

        assert!(document.describes("posts.index"));
        assert!(!document.describes("posts.store"));
        assert_eq!(document.described(), vec!["posts.index"]);
        assert!(document.endpoint("posts.index").is_some());
        assert!(document.endpoint("posts.store").is_none());
    }
}
