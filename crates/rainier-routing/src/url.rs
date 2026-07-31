//! URL generation from named routes — [`UrlGenerator`].
//!
//! The reason to name routes at all: a template or a redirect refers to
//! `route("posts.show", [("post", "7")])` rather than to `/posts/7`, so the URI
//! can change in one place. Extra parameters that the pattern does not consume
//! become a query string.

use std::collections::HashMap;

use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use rainier_support::{Error, Result};

use crate::route::{compile, Segment};

/// Characters escaped inside a generated path segment. `/` is included — a
/// parameter value containing a slash must not silently become two segments.
const PATH_SEGMENT: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}')
    .add(b'/')
    .add(b'%');

/// Characters escaped inside a query key or value.
///
/// A superset of [`PATH_SEGMENT`] plus the query separators. Escaping `&` and
/// `=` is the load-bearing part: without it a value like `a&admin=1` would
/// silently become a second query parameter.
const QUERY_COMPONENT: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'<')
    .add(b'>')
    .add(b'`')
    .add(b'{')
    .add(b'}')
    .add(b'%')
    .add(b'&')
    .add(b'=')
    .add(b'+')
    .add(b'?');

/// Builds URLs for named routes.
#[derive(Debug, Default, Clone)]
pub struct UrlGenerator {
    routes: HashMap<String, String>,
    base: Option<String>,
}

impl UrlGenerator {
    /// A generator with no routes.
    pub fn new() -> Self {
        Self::default()
    }

    /// Build from `(name, uri pattern)` pairs — normally
    /// [`CompiledRouter::named_routes`](crate::CompiledRouter::named_routes).
    pub fn from_routes(routes: impl IntoIterator<Item = (String, String)>) -> Self {
        Self { routes: routes.into_iter().collect(), base: None }
    }

    /// Set the absolute base (`https://example.com`) used by
    /// [`absolute`](Self::absolute) and [`to`](Self::to).
    pub fn with_base(mut self, base: impl Into<String>) -> Self {
        self.base = Some(base.into().trim_end_matches('/').to_string());
        self
    }

    /// Register one named route.
    pub fn insert(&mut self, name: impl Into<String>, uri: impl Into<String>) {
        self.routes.insert(name.into(), uri.into());
    }

    /// Whether `name` is known.
    pub fn has(&self, name: &str) -> bool {
        self.routes.contains_key(name)
    }

    /// Build a relative URL for a named route.
    ///
    /// Every required parameter must be supplied; anything left over is
    /// appended as a query string.
    ///
    /// ```
    /// # use rainier_routing::UrlGenerator;
    /// let urls = UrlGenerator::from_routes([
    ///     ("posts.show".to_string(), "/posts/{post}".to_string()),
    /// ]);
    ///
    /// assert_eq!(urls.route("posts.show", &[("post", "7")]).unwrap(), "/posts/7");
    /// assert_eq!(
    ///     urls.route("posts.show", &[("post", "7"), ("ref", "email")]).unwrap(),
    ///     "/posts/7?ref=email",
    /// );
    /// ```
    pub fn route(&self, name: &str, params: &[(&str, &str)]) -> Result<String> {
        let pattern = self.routes.get(name).ok_or_else(|| {
            Error::internal(format!(
                "no route is named `{name}` — check the name, or that the route is registered \
                 before URLs are generated"
            ))
        })?;

        let mut supplied: HashMap<&str, &str> = params.iter().copied().collect();
        let mut path = String::new();

        for segment in compile(pattern) {
            match segment {
                Segment::Static(literal) => {
                    path.push('/');
                    path.push_str(&literal);
                }
                Segment::Param(param) => {
                    let value = supplied.remove(param.as_str()).ok_or_else(|| {
                        Error::internal(format!("the `{name}` route needs a `{param}` parameter"))
                    })?;
                    path.push('/');
                    path.push_str(&utf8_percent_encode(value, PATH_SEGMENT).to_string());
                }
                Segment::OptionalParam(param) => {
                    if let Some(value) = supplied.remove(param.as_str()) {
                        path.push('/');
                        path.push_str(&utf8_percent_encode(value, PATH_SEGMENT).to_string());
                    }
                }
                Segment::Wildcard(param) => {
                    if let Some(value) = supplied.remove(param.as_str()) {
                        // A wildcard is *meant* to span segments, so its
                        // slashes survive; each piece is still encoded.
                        let encoded: Vec<String> = value
                            .split('/')
                            .map(|piece| utf8_percent_encode(piece, PATH_SEGMENT).to_string())
                            .collect();
                        path.push('/');
                        path.push_str(&encoded.join("/"));
                    }
                }
            }
        }

        if path.is_empty() {
            path.push('/');
        }

        if !supplied.is_empty() {
            // Sorted so the same call always produces the same URL — which
            // matters for caching and for asserting on URLs in tests.
            let mut extras: Vec<(&str, &str)> = supplied.into_iter().collect();
            extras.sort();
            let query = extras
                .iter()
                .map(|(key, value)| {
                    format!(
                        "{}={}",
                        utf8_percent_encode(key, QUERY_COMPONENT),
                        utf8_percent_encode(value, QUERY_COMPONENT)
                    )
                })
                .collect::<Vec<_>>()
                .join("&");
            path.push('?');
            path.push_str(&query);
        }

        Ok(path)
    }

    /// Build an absolute URL for a named route.
    pub fn absolute(&self, name: &str, params: &[(&str, &str)]) -> Result<String> {
        let path = self.route(name, params)?;
        Ok(self.to(&path))
    }

    /// Prefix a path with the configured base, if there is one.
    ///
    /// A path without a leading `/` gets one, rather than being concatenated
    /// onto the base — `to("assets/app.css")` is the natural way to write an
    /// asset URL and must not silently lose its path.
    pub fn to(&self, path: &str) -> String {
        match &self.base {
            Some(base) if path.starts_with('/') => format!("{base}{path}"),
            Some(base) => format!("{base}/{path}"),
            None => path.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn urls() -> UrlGenerator {
        UrlGenerator::from_routes([
            ("home".to_string(), "/".to_string()),
            ("posts.index".to_string(), "/posts".to_string()),
            ("posts.show".to_string(), "/posts/{post}".to_string()),
            ("posts.comment".to_string(), "/posts/{post}/comments/{comment}".to_string()),
            ("archive".to_string(), "/archive/{year}/{month?}".to_string()),
            ("files".to_string(), "/files/{path*}".to_string()),
        ])
    }

    #[test]
    fn builds_static_urls() {
        assert_eq!(urls().route("posts.index", &[]).unwrap(), "/posts");
        assert_eq!(urls().route("home", &[]).unwrap(), "/");
    }

    #[test]
    fn substitutes_parameters() {
        assert_eq!(urls().route("posts.show", &[("post", "7")]).unwrap(), "/posts/7");
        assert_eq!(
            urls().route("posts.comment", &[("post", "7"), ("comment", "3")]).unwrap(),
            "/posts/7/comments/3"
        );
    }

    #[test]
    fn leftover_parameters_become_a_sorted_query_string() {
        assert_eq!(
            urls().route("posts.index", &[("page", "2"), ("sort", "new")]).unwrap(),
            "/posts?page=2&sort=new"
        );
        // Sorted, so the output does not depend on argument order.
        assert_eq!(
            urls().route("posts.index", &[("sort", "new"), ("page", "2")]).unwrap(),
            "/posts?page=2&sort=new"
        );
    }

    #[test]
    fn a_missing_required_parameter_names_itself() {
        let err = urls().route("posts.show", &[]).err().expect("should fail");
        assert!(err.message().contains("`post`"), "{}", err.message());
        assert!(err.message().contains("posts.show"), "{}", err.message());
    }

    #[test]
    fn an_unknown_route_name_names_itself() {
        let err = urls().route("nope", &[]).err().expect("should fail");
        assert!(err.message().contains("`nope`"), "{}", err.message());
    }

    #[test]
    fn optional_parameters_may_be_omitted() {
        assert_eq!(urls().route("archive", &[("year", "2026")]).unwrap(), "/archive/2026");
        assert_eq!(
            urls().route("archive", &[("year", "2026"), ("month", "07")]).unwrap(),
            "/archive/2026/07"
        );
    }

    #[test]
    fn a_slash_in_a_parameter_is_encoded_not_expanded() {
        // Otherwise a user-supplied value could forge extra path segments.
        assert_eq!(urls().route("posts.show", &[("post", "a/b")]).unwrap(), "/posts/a%2Fb");
    }

    #[test]
    fn a_wildcard_keeps_its_slashes() {
        assert_eq!(
            urls().route("files", &[("path", "docs/a b.txt")]).unwrap(),
            "/files/docs/a%20b.txt"
        );
    }

    #[test]
    fn values_are_percent_encoded() {
        assert_eq!(
            urls().route("posts.show", &[("post", "hello world")]).unwrap(),
            "/posts/hello%20world"
        );
        assert_eq!(urls().route("posts.index", &[("q", "a&b")]).unwrap(), "/posts?q=a%26b");
    }

    #[test]
    fn a_query_value_cannot_forge_an_extra_parameter() {
        // Regression guard: `&` and `=` must be escaped in query components,
        // or a user-supplied value could inject `admin=1`.
        assert_eq!(
            urls().route("posts.index", &[("q", "x&admin=1")]).unwrap(),
            "/posts?q=x%26admin%3D1"
        );
    }

    #[test]
    fn absolute_urls_use_the_base() {
        let urls = urls().with_base("https://example.com/");
        assert_eq!(
            urls.absolute("posts.show", &[("post", "7")]).unwrap(),
            "https://example.com/posts/7"
        );
        assert_eq!(urls.to("/x"), "https://example.com/x");
    }

    #[test]
    fn a_relative_path_keeps_its_path_when_prefixed() {
        let urls = urls().with_base("https://example.com");

        // Regression: this used to drop the path and return just the base.
        assert_eq!(urls.to("assets/app.css"), "https://example.com/assets/app.css");
        assert_eq!(urls.to("/assets/app.css"), "https://example.com/assets/app.css");
    }

    #[test]
    fn without_a_base_absolute_falls_back_to_the_path() {
        assert_eq!(urls().absolute("posts.index", &[]).unwrap(), "/posts");
    }
}
