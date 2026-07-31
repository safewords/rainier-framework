//! Rendering — the [`ViewEngine`] port, the [`View`] value, and the
//! [`TemplateEngine`] implementation.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use rainier_support::{Error, Result};
use serde::Serialize;
use serde_json::{Map, Value};

use crate::template::{parse, CompareOp, Condition, Expr, Literal, Node, Template};
use crate::vite::Vite;

/// A view name plus the data to render it with.
///
/// Returned from a controller and rendered by the response layer, so an action
/// can name a template without reaching for the engine.
#[derive(Debug, Clone, PartialEq)]
pub struct View {
    name: String,
    data: Value,
}

impl View {
    /// A view with no data.
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into(), data: Value::Object(Map::new()) }
    }

    /// A view with `data` — any serialisable value that produces an object.
    pub fn with(name: impl Into<String>, data: impl Serialize) -> Result<Self> {
        let data = serde_json::to_value(data)?;
        if !data.is_object() {
            return Err(Error::internal(
                "view data must be an object — a template reads values by name",
            ));
        }
        Ok(Self { name: name.into(), data })
    }

    /// Add one value.
    pub fn add(mut self, key: impl Into<String>, value: impl Serialize) -> Result<Self> {
        let value = serde_json::to_value(value)?;
        self.data.as_object_mut().expect("view data is always an object").insert(key.into(), value);
        Ok(self)
    }

    /// The template's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The data.
    pub fn data(&self) -> &Value {
        &self.data
    }
}

/// Renders a named template.
pub trait ViewEngine: Send + Sync + 'static {
    /// Render `name` with `data`.
    fn render(&self, name: &str, data: &Value) -> Result<String>;

    /// Render a [`View`].
    fn render_view(&self, view: &View) -> Result<String> {
        self.render(view.name(), view.data())
    }

    /// Whether `name` resolves to a template.
    fn exists(&self, name: &str) -> bool;
}

/// A template engine over a directory of templates.
///
/// A view name is dotted and maps to a path: `posts.show` →
/// `<root>/posts/show.view.html`.
pub struct TemplateEngine {
    root: PathBuf,
    extension: String,
    /// Parsed templates, keyed by name.
    cache: RwLock<HashMap<String, Template>>,
    /// Whether to keep parsed templates. Off in development so an edit shows
    /// up without a restart; on in production so a template is parsed once.
    caching: bool,
    /// The `@vite` resolver, when one is attached. A template using the
    /// directive without one gets an error saying how to attach it.
    vite: Option<std::sync::Arc<Vite>>,
}

impl TemplateEngine {
    /// Load templates from `root`, caching parsed output.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            extension: "view.html".to_string(),
            cache: RwLock::new(HashMap::new()),
            caching: true,
            vite: None,
        }
    }

    /// Attach a [`Vite`] resolver for the `@vite` directive.
    pub fn with_vite(mut self, vite: impl Into<std::sync::Arc<Vite>>) -> Self {
        self.vite = Some(vite.into());
        self
    }

    /// Re-read and re-parse templates on every render.
    pub fn without_cache(mut self) -> Self {
        self.caching = false;
        self
    }

    /// Use a different file extension.
    pub fn with_extension(mut self, extension: impl Into<String>) -> Self {
        self.extension = extension.into();
        self
    }

    /// Where templates are read from.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The file a view name maps to.
    ///
    /// Every segment is sanitised and the result is always **inside** the
    /// template root. That matters because a view name can reach the engine
    /// from a template (`@include('…')`) and, in a careless application, from
    /// user input. Naively replacing dots with separators is not enough: a
    /// name like `..secrets` produces a leading separator, and `Path::join`
    /// with a rooted path *discards the base* — silently reading from the
    /// filesystem root.
    pub fn path_for(&self, name: &str) -> PathBuf {
        let segments: Vec<String> = name
            .split('.')
            .map(|segment| segment.trim().replace(['/', '\\'], ""))
            .filter(|segment| !segment.is_empty() && segment != "." && segment != "..")
            .collect();

        let Some((file, directories)) = segments.split_last() else {
            // Nothing usable was left; return a path that cannot exist rather
            // than the template root itself.
            return self.root.join(format!("_invalid_.{}", self.extension));
        };

        let mut path = self.root.clone();
        for directory in directories {
            path.push(directory);
        }
        path.push(format!("{file}.{}", self.extension));
        path
    }

    /// Discard every cached template.
    pub fn flush(&self) {
        self.cache.write().expect("view cache poisoned").clear();
    }

    fn load(&self, name: &str) -> Result<Template> {
        if self.caching {
            if let Some(cached) = self.cache.read().expect("view cache poisoned").get(name) {
                return Ok(cached.clone());
            }
        }

        let path = self.path_for(name);
        let source = std::fs::read_to_string(&path).map_err(|e| {
            Error::internal(format!("view `{name}` not found at {}: {e}", path.display()))
        })?;

        let template = parse(&source)
            .map_err(|e| Error::internal(format!("view `{name}` failed to parse: {e}")))?;

        if self.caching {
            self.cache
                .write()
                .expect("view cache poisoned")
                .insert(name.to_string(), template.clone());
        }
        Ok(template)
    }
}

impl ViewEngine for TemplateEngine {
    fn render(&self, name: &str, data: &Value) -> Result<String> {
        let mut renderer = Renderer { engine: self, depth: 0 };
        renderer.render_named(name, data)
    }

    fn exists(&self, name: &str) -> bool {
        self.path_for(name).is_file()
    }
}

impl std::fmt::Debug for TemplateEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TemplateEngine")
            .field("root", &self.root)
            .field("caching", &self.caching)
            .finish()
    }
}

/// How deep `@include` / `@extends` may nest before we assume a cycle.
const MAX_DEPTH: usize = 32;

struct Renderer<'a> {
    engine: &'a TemplateEngine,
    depth: usize,
}

impl Renderer<'_> {
    fn render_named(&mut self, name: &str, data: &Value) -> Result<String> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(Error::internal(format!(
                "view `{name}` nests more than {MAX_DEPTH} levels deep — this is almost \
                 certainly a template including itself"
            )));
        }

        let template = self.engine.load(name)?;

        // A child template contributes *sections*, not output: the layout
        // decides where they land. So render the child only to collect them,
        // then render the parent with those in hand.
        if let Some(layout) = &template.extends {
            let layout = unquote(layout);
            let mut sections = HashMap::new();
            self.collect_sections(&template.nodes, data, &mut sections)?;

            let parent = self.engine.load(&layout)?;
            let mut out = String::new();
            self.render_nodes(&parent.nodes, data, &sections, &mut out)?;
            self.depth -= 1;
            return Ok(out);
        }

        let mut out = String::new();
        self.render_nodes(&template.nodes, data, &HashMap::new(), &mut out)?;
        self.depth -= 1;
        Ok(out)
    }

    fn collect_sections(
        &mut self,
        nodes: &[Node],
        data: &Value,
        sections: &mut HashMap<String, String>,
    ) -> Result<()> {
        for node in nodes {
            if let Node::Section { name, body } = node {
                let mut rendered = String::new();
                self.render_nodes(body, data, &HashMap::new(), &mut rendered)?;
                sections.insert(name.clone(), rendered);
            }
        }
        Ok(())
    }

    fn render_nodes(
        &mut self,
        nodes: &[Node],
        data: &Value,
        sections: &HashMap<String, String>,
        out: &mut String,
    ) -> Result<()> {
        for node in nodes {
            match node {
                Node::Text(text) => out.push_str(text),

                Node::Echo { expr, raw } => {
                    let value = lookup(data, expr);
                    let rendered = stringify(value);
                    if *raw {
                        out.push_str(&rendered);
                    } else {
                        out.push_str(&escape_html(&rendered));
                    }
                }

                Node::If { branches, otherwise } => {
                    let mut taken = false;
                    for (condition, body) in branches {
                        if evaluate(data, condition) {
                            self.render_nodes(body, data, sections, out)?;
                            taken = true;
                            break;
                        }
                    }
                    if !taken {
                        if let Some(body) = otherwise {
                            self.render_nodes(body, data, sections, out)?;
                        }
                    }
                }

                Node::Foreach { collection, key, value, body } => {
                    let items = lookup(data, collection);
                    let pairs: Vec<(Value, Value)> = match items {
                        Some(Value::Array(items)) => items
                            .iter()
                            .enumerate()
                            .map(|(index, item)| (Value::from(index), item.clone()))
                            .collect(),
                        Some(Value::Object(map)) => {
                            map.iter().map(|(k, v)| (Value::String(k.clone()), v.clone())).collect()
                        }
                        // Anything else — including a missing key — is an
                        // empty loop rather than an error, so an optional
                        // collection needs no `@if` around it.
                        _ => Vec::new(),
                    };

                    for (index, item) in pairs {
                        // The loop variables shadow the outer scope for the
                        // body only; the parent data is otherwise intact.
                        let mut scope = data.as_object().cloned().unwrap_or_default();
                        scope.insert(value.clone(), item);
                        if let Some(key) = key {
                            scope.insert(key.clone(), index);
                        }
                        self.render_nodes(body, &Value::Object(scope), sections, out)?;
                    }
                }

                Node::Include(name) => {
                    out.push_str(&self.render_named(name, data)?);
                }

                Node::Vite(entries) => {
                    let Some(vite) = &self.engine.vite else {
                        return Err(Error::internal(
                            "the template uses @vite but the engine has no resolver — attach \
                             one with `TemplateEngine::new(…).with_vite(Vite::new(\"public\"))` \
                             (the framework's default bootstrap does)",
                        ));
                    };
                    // Raw on purpose: the resolver emits tags, and it escapes
                    // the attribute values itself.
                    out.push_str(&vite.tags(entries)?);
                }

                Node::Yield(name) => {
                    if let Some(content) = sections.get(name) {
                        out.push_str(content);
                    }
                    // A yield with no matching section renders nothing, which
                    // is what a layout with an optional slot wants.
                }

                // A section outside a layout renders in place, so a template
                // can be viewed on its own.
                Node::Section { body, .. } => self.render_nodes(body, data, sections, out)?,
            }
        }
        Ok(())
    }
}

fn unquote(value: &str) -> String {
    let trimmed = value.trim();
    trimmed
        .strip_prefix('\'')
        .and_then(|v| v.strip_suffix('\''))
        .or_else(|| trimmed.strip_prefix('"').and_then(|v| v.strip_suffix('"')))
        .unwrap_or(trimmed)
        .to_string()
}

/// Read a dotted path out of the view data.
fn lookup<'a>(data: &'a Value, expr: &Expr) -> Option<&'a Value> {
    let mut cursor = data;
    for segment in &expr.path {
        cursor = match cursor {
            Value::Object(map) => map.get(segment)?,
            Value::Array(items) => items.get(segment.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cursor)
}

/// Render a value for output. A missing value is empty rather than an error —
/// a template should not 500 because an optional field is absent.
fn stringify(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => n.to_string(),
        Some(other) => other.to_string(),
    }
}

/// Escape the five characters that can break out of HTML text or an attribute.
///
/// This is why `{{ }}` is the default and `{!! !!}` has to be asked for: the
/// safe form should be the one you reach for without thinking.
pub fn escape_html(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

/// Whether a value counts as true in an `@if`.
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Number(n)) => n.as_f64().is_some_and(|n| n != 0.0),
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::Object(map)) => !map.is_empty(),
    }
}

fn evaluate(data: &Value, condition: &Condition) -> bool {
    match condition {
        Condition::Truthy(expr) => truthy(lookup(data, expr)),
        Condition::Falsy(expr) => !truthy(lookup(data, expr)),
        Condition::Compare { left, op, right } => compare(lookup(data, left), *op, right),
    }
}

fn compare(left: Option<&Value>, op: CompareOp, right: &Literal) -> bool {
    // Equality works for every type; ordering only for numbers, since
    // ordering strings by byte value is rarely what a template meant.
    let ordering = match (left, right) {
        (Some(Value::Number(a)), Literal::Number(b)) => a.as_f64().and_then(|a| a.partial_cmp(b)),
        (Some(Value::String(a)), Literal::Number(b)) => {
            a.parse::<f64>().ok().and_then(|a| a.partial_cmp(b))
        }
        _ => None,
    };

    let equal = match (left, right) {
        (None | Some(Value::Null), Literal::Null) => true,
        (Some(Value::String(a)), Literal::Text(b)) => a == b,
        (Some(Value::Bool(a)), Literal::Bool(b)) => a == b,
        (Some(Value::Number(a)), Literal::Number(b)) => {
            a.as_f64().is_some_and(|a| (a - b).abs() < f64::EPSILON)
        }
        // A form value arrives as a string; comparing it to a number should
        // still work.
        (Some(Value::String(a)), Literal::Number(b)) => {
            a.parse::<f64>().is_ok_and(|a| (a - b).abs() < f64::EPSILON)
        }
        (Some(Value::Number(a)), Literal::Text(b)) => a.to_string() == *b,
        _ => false,
    };

    match op {
        CompareOp::Eq => equal,
        CompareOp::Ne => !equal,
        CompareOp::Gt => ordering == Some(std::cmp::Ordering::Greater),
        CompareOp::Gte => {
            matches!(ordering, Some(std::cmp::Ordering::Greater) | Some(std::cmp::Ordering::Equal))
        }
        CompareOp::Lt => ordering == Some(std::cmp::Ordering::Less),
        CompareOp::Lte => {
            matches!(ordering, Some(std::cmp::Ordering::Less) | Some(std::cmp::Ordering::Equal))
        }
    }
}

/// A [`ViewEngine`] over templates held in memory, for tests and for a service
/// with a handful of built-in templates.
#[derive(Debug, Default)]
pub struct MemoryEngine {
    templates: RwLock<HashMap<String, String>>,
    vite: Option<std::sync::Arc<Vite>>,
}

impl MemoryEngine {
    /// No templates.
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach a [`Vite`] resolver, so a test can render `@vite` too.
    pub fn with_vite(mut self, vite: impl Into<std::sync::Arc<Vite>>) -> Self {
        self.vite = Some(vite.into());
        self
    }

    /// Add a template.
    pub fn with(self, name: impl Into<String>, source: impl Into<String>) -> Self {
        self.templates.write().expect("template lock poisoned").insert(name.into(), source.into());
        self
    }
}

impl ViewEngine for MemoryEngine {
    fn render(&self, name: &str, data: &Value) -> Result<String> {
        let source = self
            .templates
            .read()
            .expect("template lock poisoned")
            .get(name)
            .cloned()
            .ok_or_else(|| Error::internal(format!("view `{name}` is not registered")))?;

        let template = parse(&source)?;
        // Layouts and includes need the engine's loader; an in-memory engine
        // renders one template at a time, which is all a test needs.
        let mut out = String::new();
        let mut engine = TemplateEngine::new(".");
        if let Some(vite) = &self.vite {
            engine = engine.with_vite(std::sync::Arc::clone(vite));
        }
        let mut renderer = Renderer { engine: &engine, depth: 0 };
        renderer.render_nodes(&template.nodes, data, &HashMap::new(), &mut out)?;
        Ok(out)
    }

    fn exists(&self, name: &str) -> bool {
        self.templates.read().expect("template lock poisoned").contains_key(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn render(source: &str, data: Value) -> String {
        MemoryEngine::new().with("t", source).render("t", &data).expect("should render")
    }

    #[test]
    fn vite_renders_through_an_attached_resolver() {
        let public = std::env::temp_dir().join(format!("rainier-view-vite-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&public);
        std::fs::create_dir_all(&public).unwrap();
        std::fs::write(public.join("hot"), "http://localhost:5173").unwrap();

        let engine = MemoryEngine::new()
            .with("t", "@vite('resources/js/app.js')")
            .with_vite(Vite::new(&public));

        let html = engine.render("t", &Value::Object(Map::new())).unwrap();
        assert!(html.contains("http://localhost:5173/@vite/client"), "{html}");
        assert!(html.contains("http://localhost:5173/resources/js/app.js"), "{html}");
    }

    #[test]
    fn vite_without_a_resolver_says_how_to_attach_one() {
        let engine = MemoryEngine::new().with("t", "@vite('resources/js/app.js')");

        let err = engine.render("t", &Value::Object(Map::new())).unwrap_err();
        assert!(err.message().contains("with_vite"), "{}", err.message());
    }

    #[test]
    fn interpolates_values() {
        assert_eq!(render("Hi {{ name }}!", json!({ "name": "Ada" })), "Hi Ada!");
        assert_eq!(render("{{ n }}", json!({ "n": 42 })), "42");
        assert_eq!(render("{{ b }}", json!({ "b": true })), "true");
    }

    #[test]
    fn reads_dotted_paths_and_array_indices() {
        let data = json!({ "user": { "name": "Ada" }, "tags": ["a", "b"] });
        assert_eq!(render("{{ user.name }}", data.clone()), "Ada");
        assert_eq!(render("{{ tags.1 }}", data), "b");
    }

    #[test]
    fn a_missing_value_renders_empty_rather_than_failing() {
        // A 500 because an optional field is absent would be a bad trade.
        assert_eq!(render("[{{ nope }}]", json!({})), "[]");
        assert_eq!(render("[{{ a.b.c }}]", json!({ "a": 1 })), "[]");
        assert_eq!(render("[{{ n }}]", json!({ "n": null })), "[]");
    }

    #[test]
    fn interpolation_escapes_html_by_default() {
        let data = json!({ "body": "<script>alert('xss')</script>" });
        let rendered = render("{{ body }}", data);

        assert!(!rendered.contains("<script>"), "{rendered}");
        assert_eq!(rendered, "&lt;script&gt;alert(&#39;xss&#39;)&lt;/script&gt;");
    }

    #[test]
    fn escaping_covers_attribute_breakouts() {
        assert_eq!(escape_html(r#"" onload="x"#), "&quot; onload=&quot;x");
        assert_eq!(escape_html("a & b"), "a &amp; b");
    }

    #[test]
    fn raw_interpolation_is_opt_in() {
        let data = json!({ "body": "<b>bold</b>" });
        assert_eq!(render("{!! body !!}", data), "<b>bold</b>");
    }

    #[test]
    fn conditionals_pick_a_branch() {
        let template =
            "@if(role == \"admin\")admin@elseif(role == \"editor\")editor@else guest@endif";

        assert_eq!(render(template, json!({ "role": "admin" })), "admin");
        assert_eq!(render(template, json!({ "role": "editor" })), "editor");
        assert_eq!(render(template, json!({ "role": "other" })), " guest");
        assert_eq!(render(template, json!({})), " guest");
    }

    #[test]
    fn truthiness_follows_the_obvious_rules() {
        let template = "@if(v)yes@else no@endif";

        for truthy in [json!(true), json!(1), json!("x"), json!(["a"]), json!({ "a": 1 })] {
            assert_eq!(render(template, json!({ "v": truthy })), "yes");
        }
        for falsy in [json!(false), json!(0), json!(""), json!([]), json!({}), json!(null)] {
            assert_eq!(render(template, json!({ "v": falsy })), " no");
        }
        assert_eq!(render(template, json!({})), " no", "an absent key is falsy");
    }

    #[test]
    fn negation_inverts() {
        assert_eq!(render("@if(!v)empty@endif", json!({ "v": false })), "empty");
        assert_eq!(render("@if(!v)empty@endif", json!({ "v": true })), "");
    }

    #[test]
    fn numeric_comparisons() {
        assert_eq!(render("@if(n > 5)big@endif", json!({ "n": 10 })), "big");
        assert_eq!(render("@if(n > 5)big@endif", json!({ "n": 3 })), "");
        assert_eq!(render("@if(n >= 5)ok@endif", json!({ "n": 5 })), "ok");
        assert_eq!(render("@if(n <= 5)ok@endif", json!({ "n": 5 })), "ok");
        assert_eq!(render("@if(n < 5)small@endif", json!({ "n": 1 })), "small");
    }

    #[test]
    fn a_numeric_string_compares_as_a_number() {
        // Form input arrives as a string; the template should not care.
        assert_eq!(render("@if(n > 5)big@endif", json!({ "n": "10" })), "big");
        assert_eq!(render("@if(n == 10)ten@endif", json!({ "n": "10" })), "ten");
    }

    #[test]
    fn ordering_a_non_number_is_false_rather_than_arbitrary() {
        assert_eq!(render("@if(s > 5)?@endif", json!({ "s": "abc" })), "");
        assert_eq!(render("@if(s < 5)?@endif", json!({ "s": "abc" })), "");
    }

    #[test]
    fn loops_over_an_array() {
        let data = json!({ "posts": [{ "title": "A" }, { "title": "B" }] });
        assert_eq!(render("@foreach(posts as post)[{{ post.title }}]@endforeach", data), "[A][B]");
    }

    #[test]
    fn loops_expose_the_index_or_key() {
        let list = json!({ "items": ["a", "b"] });
        assert_eq!(
            render("@foreach(items as i => v){{ i }}={{ v }};@endforeach", list),
            "0=a;1=b;"
        );

        let map = json!({ "meta": { "x": "1" } });
        assert_eq!(render("@foreach(meta as k => v){{ k }}:{{ v }}@endforeach", map), "x:1");
    }

    #[test]
    fn looping_over_nothing_renders_nothing() {
        assert_eq!(render("[@foreach(items as i)x@endforeach]", json!({})), "[]");
        assert_eq!(render("[@foreach(items as i)x@endforeach]", json!({ "items": [] })), "[]");
        assert_eq!(
            render("[@foreach(items as i)x@endforeach]", json!({ "items": "not a list" })),
            "[]"
        );
    }

    #[test]
    fn the_outer_scope_is_visible_inside_a_loop() {
        let data = json!({ "prefix": "-", "items": ["a"] });
        assert_eq!(render("@foreach(items as i){{ prefix }}{{ i }}@endforeach", data), "-a");
    }

    #[test]
    fn loops_and_conditionals_nest() {
        let data =
            json!({ "posts": [{ "published": true, "t": "A" }, { "published": false, "t": "B" }] });
        assert_eq!(
            render("@foreach(posts as p)@if(p.published){{ p.t }}@endif@endforeach", data),
            "A"
        );
    }

    // --- file-backed engine ------------------------------------------------

    fn temp_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("rainier-view-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("temp dir");
        root
    }

    fn write(root: &Path, name: &str, source: &str) {
        let path = root.join(format!("{name}.view.html"));
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("parent dir");
        }
        std::fs::write(path, source).expect("write template");
    }

    #[test]
    fn renders_a_template_from_disk() {
        let root = temp_root("basic");
        write(&root, "greeting", "Hi {{ name }}");

        let engine = TemplateEngine::new(&root);
        assert!(engine.exists("greeting"));
        assert_eq!(engine.render("greeting", &json!({ "name": "Ada" })).unwrap(), "Hi Ada");
    }

    #[test]
    fn a_missing_template_names_itself_and_its_path() {
        let engine = TemplateEngine::new(temp_root("missing"));
        let err = engine.render("nope", &json!({})).err().expect("should fail");

        assert!(err.message().contains("`nope`"), "{}", err.message());
        assert!(err.message().contains("nope.view.html"), "{}", err.message());
        assert!(!engine.exists("nope"));
    }

    #[test]
    fn a_dotted_name_maps_to_a_subdirectory() {
        let root = temp_root("nested");
        std::fs::create_dir_all(root.join("posts")).unwrap();
        write(&root, "posts/show", "post");

        let engine = TemplateEngine::new(&root);
        assert_eq!(engine.render("posts.show", &json!({})).unwrap(), "post");
    }

    #[test]
    fn a_view_name_cannot_escape_the_template_root() {
        // Regression guard: `..secrets` used to produce a leading separator,
        // and `Path::join` with a rooted path discards the base entirely.
        let root = temp_root("traversal");
        let engine = TemplateEngine::new(&root);

        for hostile in ["..secrets", "../../etc/passwd", ".", "..", "a/../../b", ""] {
            let path = engine.path_for(hostile);
            assert!(path.starts_with(&root), "`{hostile}` escaped to {path:?}");
            assert!(!path.to_string_lossy().contains(".."), "`{hostile}` produced {path:?}");
        }
    }

    #[test]
    fn a_normal_name_still_maps_the_obvious_way() {
        let engine = TemplateEngine::new("views");
        assert_eq!(
            engine.path_for("posts.show"),
            PathBuf::from("views").join("posts").join("show.view.html")
        );
    }

    #[test]
    fn includes_are_rendered_with_the_same_data() {
        let root = temp_root("include");
        write(&root, "page", "<main>@include('parts.nav')</main>");
        write(&root, "parts/nav", "<nav>{{ title }}</nav>");

        let engine = TemplateEngine::new(&root);
        assert_eq!(
            engine.render("page", &json!({ "title": "Home" })).unwrap(),
            "<main><nav>Home</nav></main>"
        );
    }

    #[test]
    fn a_layout_yields_the_childs_sections() {
        let root = temp_root("layout");
        write(
            &root,
            "layout",
            "<html><title>@yield('title')</title><body>@yield('body')</body></html>",
        );
        write(
            &root,
            "page",
            "@extends('layout')@section('title')Hi@endsection@section('body')<p>{{ name }}</p>@endsection",
        );

        let engine = TemplateEngine::new(&root);
        assert_eq!(
            engine.render("page", &json!({ "name": "Ada" })).unwrap(),
            "<html><title>Hi</title><body><p>Ada</p></body></html>"
        );
    }

    #[test]
    fn a_yield_with_no_section_renders_nothing() {
        let root = temp_root("optional-yield");
        write(&root, "layout", "[@yield('sidebar')]");
        write(&root, "page", "@extends('layout')");

        let engine = TemplateEngine::new(&root);
        assert_eq!(engine.render("page", &json!({})).unwrap(), "[]");
    }

    #[test]
    fn a_self_including_template_is_caught_rather_than_overflowing_the_stack() {
        let root = temp_root("cycle");
        write(&root, "loop", "@include('loop')");

        let engine = TemplateEngine::new(&root);
        let err = engine.render("loop", &json!({})).err().expect("should fail");
        assert!(err.message().contains("nests more than"), "{}", err.message());
    }

    #[test]
    fn caching_can_be_turned_off_for_development() {
        let root = temp_root("cache");
        write(&root, "page", "one");

        let cached = TemplateEngine::new(&root);
        assert_eq!(cached.render("page", &json!({})).unwrap(), "one");
        write(&root, "page", "two");
        assert_eq!(cached.render("page", &json!({})).unwrap(), "one", "still cached");

        cached.flush();
        assert_eq!(cached.render("page", &json!({})).unwrap(), "two");

        let live = TemplateEngine::new(&root).without_cache();
        write(&root, "page", "three");
        assert_eq!(live.render("page", &json!({})).unwrap(), "three");
    }

    #[test]
    fn a_view_carries_its_name_and_data() {
        let view = View::new("posts.show").add("id", 7).unwrap();
        assert_eq!(view.name(), "posts.show");
        assert_eq!(view.data()["id"], 7);
    }

    #[test]
    fn view_data_must_be_an_object() {
        assert!(View::with("x", serde_json::json!({ "a": 1 })).is_ok());
        let err = View::with("x", "a bare string").err().expect("should fail");
        assert!(err.message().contains("must be an object"), "{}", err.message());
    }

    #[test]
    fn render_view_goes_through_the_engine() {
        let engine = MemoryEngine::new().with("t", "Hi {{ name }}");
        let view = View::new("t").add("name", "Ada").unwrap();
        assert_eq!(engine.render_view(&view).unwrap(), "Hi Ada");
    }
}
