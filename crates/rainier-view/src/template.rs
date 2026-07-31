//! Parsing a template into a [`Node`] tree.
//!
//! A deliberately small language — the directives that carry their weight in
//! an HTML template, and nothing else. There is no arbitrary expression
//! evaluation and no way to call a function from a template, because a
//! template that can compute is a template that ends up holding business
//! logic. Anything harder than "read a value, compare it, loop over it"
//! belongs in the controller.

use rainier_support::{Error, Result};

/// A parsed template.
#[derive(Debug, Clone, PartialEq)]
pub struct Template {
    /// The layout this template extends, if any.
    pub extends: Option<String>,
    /// The body, in order.
    pub nodes: Vec<Node>,
}

/// One piece of a template.
#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    /// Literal text.
    Text(String),
    /// `{{ expr }}` (escaped) or `{!! expr !!}` (raw).
    Echo {
        /// The path to read.
        expr: Expr,
        /// Whether to emit the value without HTML-escaping it.
        raw: bool,
    },
    /// `@if` … `@elseif` … `@else` … `@endif`.
    If {
        /// Each `(condition, body)` in order.
        branches: Vec<(Condition, Vec<Node>)>,
        /// The `@else` body, if there is one.
        otherwise: Option<Vec<Node>>,
    },
    /// `@foreach(items as item)` … `@endforeach`.
    Foreach {
        /// The collection to walk.
        collection: Expr,
        /// The variable bound to each key, for `key => value` form.
        key: Option<String>,
        /// The variable bound to each element.
        value: String,
        /// The loop body.
        body: Vec<Node>,
    },
    /// `@include('partial')`.
    Include(String),
    /// `@yield('name')` — where a layout drops a child's section.
    Yield(String),
    /// `@section('name')` … `@endsection`.
    Section {
        /// The section's name.
        name: String,
        /// Its content.
        body: Vec<Node>,
    },
}

/// A dotted path into the view data — `user.address.city`, `items.0.name`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expr {
    /// The path segments.
    pub path: Vec<String>,
}

impl Expr {
    /// Parse a dotted path.
    pub fn parse(source: &str) -> Self {
        Self { path: source.trim().split('.').map(str::to_string).collect() }
    }

    /// The path as written.
    pub fn as_string(&self) -> String {
        self.path.join(".")
    }
}

/// An `@if` test.
#[derive(Debug, Clone, PartialEq)]
pub enum Condition {
    /// `@if(user)` — true when the value is present and truthy.
    Truthy(Expr),
    /// `@if(!user)`.
    Falsy(Expr),
    /// `@if(a == "b")` and friends.
    Compare {
        /// The left-hand path.
        left: Expr,
        /// The operator.
        op: CompareOp,
        /// The right-hand literal.
        right: Literal,
    },
}

/// A comparison operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareOp {
    /// `==`
    Eq,
    /// `!=`
    Ne,
    /// `>`
    Gt,
    /// `>=`
    Gte,
    /// `<`
    Lt,
    /// `<=`
    Lte,
}

/// A literal on the right of a comparison.
#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    /// A quoted string.
    Text(String),
    /// A number.
    Number(f64),
    /// `true` or `false`.
    Bool(bool),
    /// `null`.
    Null,
}

/// One lexed piece of the source.
#[derive(Debug, Clone, PartialEq)]
enum Token {
    Text(String),
    Echo { expr: String, raw: bool },
    Directive { name: String, arg: Option<String> },
}

/// Parse a template's source.
pub fn parse(source: &str) -> Result<Template> {
    let tokens = lex(source);
    let mut cursor = 0;
    let mut extends = None;

    // `@extends` is only meaningful as the template's first directive, so it
    // is lifted out here rather than being a node the renderer must look for.
    if let Some(Token::Directive { name, arg }) =
        tokens.iter().find(|token| !matches!(token, Token::Text(text) if text.trim().is_empty()))
    {
        if name == "extends" {
            extends =
                Some(arg.clone().ok_or_else(|| Error::internal("@extends needs a layout name"))?);
        }
    }

    let nodes = parse_nodes(&tokens, &mut cursor, &[])?;
    Ok(Template { extends, nodes })
}

/// Split the source into text, echoes and directives.
fn lex(source: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut text = String::new();
    let bytes: Vec<char> = source.chars().collect();
    let mut index = 0;

    while index < bytes.len() {
        // `{!! raw !!}` before `{{ escaped }}`: the raw opener starts with the
        // same two braces, so checking the shorter one first would swallow it.
        if starts_with(&bytes, index, "{!!") {
            if let Some(end) = find_from(&bytes, index + 3, "!!}") {
                flush(&mut tokens, &mut text);
                tokens.push(Token::Echo { expr: collect(&bytes, index + 3, end), raw: true });
                index = end + 3;
                continue;
            }
        }

        if starts_with(&bytes, index, "{{") {
            if let Some(end) = find_from(&bytes, index + 2, "}}") {
                flush(&mut tokens, &mut text);
                tokens.push(Token::Echo { expr: collect(&bytes, index + 2, end), raw: false });
                index = end + 2;
                continue;
            }
        }

        if bytes[index] == '@' {
            // `@@` escapes a literal at-sign, so an email address in a
            // template does not read as a directive.
            if bytes.get(index + 1) == Some(&'@') {
                text.push('@');
                index += 2;
                continue;
            }

            if let Some((name, arg, next)) = lex_directive(&bytes, index) {
                flush(&mut tokens, &mut text);
                tokens.push(Token::Directive { name, arg });
                index = next;
                continue;
            }
        }

        text.push(bytes[index]);
        index += 1;
    }

    flush(&mut tokens, &mut text);
    tokens
}

/// Read `@name` and an optional `(argument)`.
fn lex_directive(bytes: &[char], at: usize) -> Option<(String, Option<String>, usize)> {
    let mut index = at + 1;
    let mut name = String::new();
    while index < bytes.len() && (bytes[index].is_ascii_alphanumeric() || bytes[index] == '_') {
        name.push(bytes[index]);
        index += 1;
    }
    if name.is_empty() {
        return None;
    }

    if bytes.get(index) != Some(&'(') {
        return Some((name, None, index));
    }

    // Balance nested parentheses so `@if(a == "(")` survives.
    let mut depth = 0;
    let start = index + 1;
    while index < bytes.len() {
        match bytes[index] {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    let arg = collect(bytes, start, index);
                    return Some((name, Some(arg.trim().to_string()), index + 1));
                }
            }
            _ => {}
        }
        index += 1;
    }
    // Unbalanced: treat it as plain text rather than eating the rest.
    None
}

fn starts_with(bytes: &[char], at: usize, needle: &str) -> bool {
    needle.chars().enumerate().all(|(offset, expected)| bytes.get(at + offset) == Some(&expected))
}

fn find_from(bytes: &[char], from: usize, needle: &str) -> Option<usize> {
    (from..bytes.len()).find(|&index| starts_with(bytes, index, needle))
}

fn collect(bytes: &[char], from: usize, to: usize) -> String {
    bytes[from..to].iter().collect()
}

fn flush(tokens: &mut Vec<Token>, text: &mut String) {
    if !text.is_empty() {
        tokens.push(Token::Text(std::mem::take(text)));
    }
}

/// Parse tokens into nodes until one of `terminators` is reached.
fn parse_nodes(tokens: &[Token], cursor: &mut usize, terminators: &[&str]) -> Result<Vec<Node>> {
    let mut nodes = Vec::new();

    while *cursor < tokens.len() {
        match &tokens[*cursor] {
            Token::Text(text) => {
                nodes.push(Node::Text(text.clone()));
                *cursor += 1;
            }
            Token::Echo { expr, raw } => {
                nodes.push(Node::Echo { expr: Expr::parse(expr), raw: *raw });
                *cursor += 1;
            }
            Token::Directive { name, arg } => {
                if terminators.contains(&name.as_str()) {
                    return Ok(nodes);
                }
                match name.as_str() {
                    // Already lifted into `Template::extends`.
                    "extends" => *cursor += 1,
                    "if" => nodes.push(parse_if(tokens, cursor, arg.as_deref())?),
                    "foreach" => nodes.push(parse_foreach(tokens, cursor, arg.as_deref())?),
                    "section" => nodes.push(parse_section(tokens, cursor, arg.as_deref())?),
                    "include" => {
                        nodes.push(Node::Include(unquote(require_arg(arg, "@include")?)));
                        *cursor += 1;
                    }
                    "yield" => {
                        nodes.push(Node::Yield(unquote(require_arg(arg, "@yield")?)));
                        *cursor += 1;
                    }
                    other => {
                        return Err(Error::internal(format!(
                            "unknown template directive `@{other}`"
                        )))
                    }
                }
            }
        }
    }

    if terminators.is_empty() {
        Ok(nodes)
    } else {
        Err(Error::internal(format!(
            "the template ended while still inside a block — expected @{}",
            terminators.join(" or @")
        )))
    }
}

fn parse_if(tokens: &[Token], cursor: &mut usize, arg: Option<&str>) -> Result<Node> {
    let mut branches = Vec::new();
    let mut otherwise = None;
    let mut condition = parse_condition(require_arg(&arg.map(str::to_string), "@if")?)?;
    *cursor += 1;

    loop {
        let body = parse_nodes(tokens, cursor, &["elseif", "else", "endif"])?;
        branches.push((condition.clone(), body));

        let Some(Token::Directive { name, arg }) = tokens.get(*cursor) else {
            return Err(Error::internal("@if was never closed with @endif"));
        };

        match name.as_str() {
            "elseif" => {
                condition = parse_condition(require_arg(arg, "@elseif")?)?;
                *cursor += 1;
            }
            "else" => {
                *cursor += 1;
                otherwise = Some(parse_nodes(tokens, cursor, &["endif"])?);
                *cursor += 1; // @endif
                break;
            }
            _ => {
                *cursor += 1; // @endif
                break;
            }
        }
    }

    Ok(Node::If { branches, otherwise })
}

fn parse_foreach(tokens: &[Token], cursor: &mut usize, arg: Option<&str>) -> Result<Node> {
    let arg = require_arg(&arg.map(str::to_string), "@foreach")?;
    let (collection, binding) = arg.split_once(" as ").ok_or_else(|| {
        Error::internal(format!("@foreach({arg}) should read `@foreach(items as item)`"))
    })?;

    let (key, value) = match binding.split_once("=>") {
        Some((key, value)) => (Some(key.trim().to_string()), value.trim().to_string()),
        None => (None, binding.trim().to_string()),
    };

    *cursor += 1;
    let body = parse_nodes(tokens, cursor, &["endforeach"])?;
    *cursor += 1;

    Ok(Node::Foreach { collection: Expr::parse(collection), key, value, body })
}

fn parse_section(tokens: &[Token], cursor: &mut usize, arg: Option<&str>) -> Result<Node> {
    let name = unquote(require_arg(&arg.map(str::to_string), "@section")?);
    *cursor += 1;
    let body = parse_nodes(tokens, cursor, &["endsection"])?;
    *cursor += 1;
    Ok(Node::Section { name, body })
}

fn require_arg(arg: &Option<String>, directive: &str) -> Result<String> {
    arg.clone().ok_or_else(|| Error::internal(format!("{directive} needs an argument")))
}

fn unquote(value: String) -> String {
    let trimmed = value.trim();
    trimmed
        .strip_prefix('\'')
        .and_then(|v| v.strip_suffix('\''))
        .or_else(|| trimmed.strip_prefix('"').and_then(|v| v.strip_suffix('"')))
        .unwrap_or(trimmed)
        .to_string()
}

/// Parse an `@if` condition.
fn parse_condition(source: String) -> Result<Condition> {
    let source = source.trim();

    for (token, op) in [
        ("==", CompareOp::Eq),
        ("!=", CompareOp::Ne),
        (">=", CompareOp::Gte),
        ("<=", CompareOp::Lte),
        (">", CompareOp::Gt),
        ("<", CompareOp::Lt),
    ] {
        if let Some((left, right)) = source.split_once(token) {
            return Ok(Condition::Compare {
                left: Expr::parse(left),
                op,
                right: parse_literal(right.trim()),
            });
        }
    }

    match source.strip_prefix('!') {
        Some(rest) => Ok(Condition::Falsy(Expr::parse(rest))),
        None => Ok(Condition::Truthy(Expr::parse(source))),
    }
}

fn parse_literal(source: &str) -> Literal {
    if let Some(text) = source
        .strip_prefix('\'')
        .and_then(|v| v.strip_suffix('\''))
        .or_else(|| source.strip_prefix('"').and_then(|v| v.strip_suffix('"')))
    {
        return Literal::Text(text.to_string());
    }
    match source {
        "true" => Literal::Bool(true),
        "false" => Literal::Bool(false),
        "null" | "nil" => Literal::Null,
        other => match other.parse::<f64>() {
            Ok(number) => Literal::Number(number),
            // An unquoted, non-numeric word reads as a string, which is what
            // someone writing `@if(status == active)` meant.
            Err(_) => Literal::Text(other.to_string()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nodes(source: &str) -> Vec<Node> {
        parse(source).expect("should parse").nodes
    }

    #[test]
    fn plain_text_is_one_node() {
        assert_eq!(nodes("hello"), vec![Node::Text("hello".into())]);
    }

    #[test]
    fn interpolation_is_parsed_with_its_escaping_mode() {
        assert_eq!(nodes("{{ name }}"), vec![Node::Echo { expr: Expr::parse("name"), raw: false }]);
        assert_eq!(
            nodes("{!! body !!}"),
            vec![Node::Echo { expr: Expr::parse("body"), raw: true }]
        );
    }

    #[test]
    fn a_raw_echo_is_not_mistaken_for_an_escaped_one() {
        // `{!!` and `{{` share a prefix; the longer opener must win.
        let parsed = nodes("{!! a !!}");
        assert!(matches!(parsed[0], Node::Echo { raw: true, .. }));
        assert_eq!(parsed.len(), 1);
    }

    #[test]
    fn dotted_paths_become_segments() {
        assert_eq!(Expr::parse("user.address.city").path, vec!["user", "address", "city"]);
    }

    #[test]
    fn text_and_interpolation_interleave() {
        assert_eq!(
            nodes("Hi {{ name }}!"),
            vec![
                Node::Text("Hi ".into()),
                Node::Echo { expr: Expr::parse("name"), raw: false },
                Node::Text("!".into()),
            ]
        );
    }

    #[test]
    fn a_doubled_at_sign_is_a_literal() {
        assert_eq!(nodes("ada@@example.com"), vec![Node::Text("ada@example.com".into())]);
    }

    #[test]
    fn an_at_sign_that_is_not_a_directive_stays_text() {
        assert_eq!(nodes("50% @ once"), vec![Node::Text("50% @ once".into())]);
    }

    #[test]
    fn an_if_without_an_else() {
        let parsed = nodes("@if(admin)yes@endif");
        let Node::If { branches, otherwise } = &parsed[0] else {
            panic!("expected an if, got {parsed:?}");
        };

        assert_eq!(branches.len(), 1);
        assert_eq!(branches[0].0, Condition::Truthy(Expr::parse("admin")));
        assert_eq!(branches[0].1, vec![Node::Text("yes".into())]);
        assert!(otherwise.is_none());
    }

    #[test]
    fn an_if_with_elseif_and_else() {
        let parsed = nodes("@if(a)A@elseif(b)B@else C@endif");
        let Node::If { branches, otherwise } = &parsed[0] else {
            panic!("expected an if");
        };

        assert_eq!(branches.len(), 2);
        assert_eq!(branches[1].0, Condition::Truthy(Expr::parse("b")));
        assert_eq!(otherwise.as_ref().unwrap(), &vec![Node::Text(" C".into())]);
    }

    #[test]
    fn conditions_parse_every_operator() {
        for (source, expected) in [
            ("a == \"x\"", CompareOp::Eq),
            ("a != \"x\"", CompareOp::Ne),
            ("a >= 1", CompareOp::Gte),
            ("a <= 1", CompareOp::Lte),
            ("a > 1", CompareOp::Gt),
            ("a < 1", CompareOp::Lt),
        ] {
            let Condition::Compare { op, .. } = parse_condition(source.into()).unwrap() else {
                panic!("expected a comparison for `{source}`");
            };
            assert_eq!(op, expected, "for `{source}`");
        }
    }

    #[test]
    fn two_character_operators_win_over_their_prefixes() {
        // `>=` must not lex as `>` followed by `=`.
        let Condition::Compare { op, right, .. } = parse_condition("n >= 5".into()).unwrap() else {
            panic!("expected a comparison");
        };
        assert_eq!(op, CompareOp::Gte);
        assert_eq!(right, Literal::Number(5.0));
    }

    #[test]
    fn negation_is_recognised() {
        assert_eq!(
            parse_condition("!admin".into()).unwrap(),
            Condition::Falsy(Expr::parse("admin"))
        );
    }

    #[test]
    fn literals_are_typed() {
        assert_eq!(parse_literal("\"x\""), Literal::Text("x".into()));
        assert_eq!(parse_literal("'x'"), Literal::Text("x".into()));
        assert_eq!(parse_literal("42"), Literal::Number(42.0));
        assert_eq!(parse_literal("true"), Literal::Bool(true));
        assert_eq!(parse_literal("null"), Literal::Null);
        assert_eq!(parse_literal("active"), Literal::Text("active".into()));
    }

    #[test]
    fn a_foreach_binds_its_element() {
        let parsed = nodes("@foreach(posts as post){{ post.title }}@endforeach");
        let Node::Foreach { collection, key, value, body } = &parsed[0] else {
            panic!("expected a foreach");
        };

        assert_eq!(collection, &Expr::parse("posts"));
        assert!(key.is_none());
        assert_eq!(value, "post");
        assert_eq!(body.len(), 1);
    }

    #[test]
    fn a_foreach_can_bind_a_key_too() {
        let parsed = nodes("@foreach(meta as k => v){{ k }}@endforeach");
        let Node::Foreach { key, value, .. } = &parsed[0] else {
            panic!("expected a foreach");
        };

        assert_eq!(key.as_deref(), Some("k"));
        assert_eq!(value, "v");
    }

    #[test]
    fn blocks_nest() {
        let parsed = nodes("@foreach(a as b)@if(b)x@endif@endforeach");
        let Node::Foreach { body, .. } = &parsed[0] else { panic!("expected a foreach") };
        assert!(matches!(body[0], Node::If { .. }));
    }

    #[test]
    fn layout_directives_are_recognised() {
        let template = parse("@extends('layout')@section('body')hi@endsection").unwrap();
        assert_eq!(template.extends.as_deref(), Some("'layout'"));

        let Node::Section { name, body } = &template.nodes[0] else {
            panic!("expected a section, got {:?}", template.nodes);
        };
        assert_eq!(name, "body");
        assert_eq!(body, &vec![Node::Text("hi".into())]);
    }

    #[test]
    fn include_and_yield_unquote_their_names() {
        assert_eq!(nodes("@include('parts.nav')"), vec![Node::Include("parts.nav".into())]);
        assert_eq!(nodes("@yield(\"content\")"), vec![Node::Yield("content".into())]);
    }

    #[test]
    fn an_unclosed_block_is_reported() {
        let err = parse("@if(a)oops").err().expect("should fail");
        assert!(err.message().contains("endif"), "{}", err.message());
    }

    #[test]
    fn an_unknown_directive_is_reported() {
        let err = parse("@nonsense(1)").err().expect("should fail");
        assert!(err.message().contains("@nonsense"), "{}", err.message());
    }

    #[test]
    fn a_directive_missing_its_argument_is_reported() {
        assert!(parse("@if hello @endif").is_err());
    }

    #[test]
    fn an_unterminated_interpolation_stays_text() {
        assert_eq!(nodes("{{ oops"), vec![Node::Text("{{ oops".into())]);
    }
}
