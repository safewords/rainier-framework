//! The `public/` directory — files served as they sit on disk.
//!
//! Laravel has a document root: everything under `public/` is reachable at the
//! URL matching its path, and anything else on disk is not. `favicon.ico`,
//! `robots.txt`, an image an editor uploaded, a built stylesheet. No route, no
//! controller, no code.
//!
//! Rainier is its own server, so there is no nginx in front deciding this. This
//! is that decision.
//!
//! # It is a fallback, which Laravel's is not
//!
//! Under nginx the file wins: `try_files $uri /index.php` looks on disk first
//! and only then reaches the application. Here it is the router's fallback, so
//! **a route wins and the file is tried after**.
//!
//! Two reasons. A file cannot silently shadow a route, which is the failure
//! that is hardest to see — an upload named `users.json` landing where
//! `/users.json` is an endpoint. And the common request is an API call, which
//! would otherwise stat the filesystem on the way past.
//!
//! The practical difference is nil: asset paths and route paths do not overlap
//! in an application that has not already confused itself.
//!
//! # What it will not serve
//!
//! - **Anything outside the root.** `..`, an absolute path, a percent-encoded
//!   `%2e%2e`, or a symlink pointing out are all refused after resolving, not
//!   by pattern-matching the request. Patterns are how traversal gets through;
//!   the check here is that the resolved path still starts with the resolved
//!   root.
//! - **Dotfiles.** `.env`, `.git/config`, `.htpasswd`. A deployment that
//!   copies its repository into `public/` should leak a 404, not a secret.
//! - **Directories.** Optionally an `index.html` inside one — off by default,
//!   because a directory that quietly serves something is how a listing turns
//!   into a disclosure.
//!
//! # An application that already has a fallback
//!
//! The install is skipped — a fallback somebody declared is one they meant,
//! and replacing it would be the framework arguing. Chain the two by hand
//! instead, file-last, so a 404 the application shapes deliberately keeps its
//! shape:
//!
//! ```ignore
//! pub async fn fallback(request: Req) -> Result<Response> {
//!     if request.path().starts_with("/api/") {
//!         return Ok(my_json_404(&request));
//!     }
//!
//!     let served = PublicFiles::at("public").serve(&request).await;
//!     if served.status() != StatusCode::NOT_FOUND {
//!         return Ok(served);
//!     }
//!
//!     Ok(Error::not_found("Not Found").into_response())
//! }
//! ```
//!
//! Worth knowing because the symptom of forgetting is that files ship, sit on
//! disk in the running container, and 404 anyway.
//!
//! # Caching
//!
//! `ETag` and `Last-Modified` from the file's length and modification time, and
//! `304` when the request already has it. That is what makes a stable asset URL
//! cheap without guessing a `max-age` on the application's behalf — set
//! `cache_control` when the answer is known (a content-hashed filename can be
//! cached for a year; `index.html` cannot be cached at all).

use std::path::{Component, Path, PathBuf};

use percent_encoding::percent_decode_str;
use rainier_http::{IntoResponse, Request, Response, StatusCode};
use rainier_support::Error;

/// Files under a directory, served at the URL matching their path.
#[derive(Debug, Clone)]
pub struct PublicFiles {
    root: PathBuf,
    index: Option<String>,
    cache_control: Option<String>,
}

impl PublicFiles {
    /// Serve the contents of `root`.
    ///
    /// A relative path resolves against the working directory, which for a
    /// deployed binary is wherever it was started — the same assumption
    /// `.env` already makes.
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into(), index: None, cache_control: None }
    }

    /// Serve `name` when the request names a directory.
    ///
    /// Off by default: a directory that serves something without being asked
    /// is how a listing becomes a disclosure. Turn it on for a single-page
    /// application whose entry lives here.
    #[must_use = "this returns a configured server rather than configuring in place"]
    pub fn with_index(mut self, name: impl Into<String>) -> Self {
        self.index = Some(name.into());
        self
    }

    /// The `Cache-Control` to send with every file.
    ///
    /// Unset sends none, which leaves the `ETag` below to do the work: a
    /// conditional request every time, answered `304` when nothing changed.
    /// Correct, and one round trip per asset per page.
    ///
    /// Set it when the answer is actually known. `public, max-age=31536000,
    /// immutable` is right for a content-hashed filename and catastrophic for
    /// one that is not — see [`crate::public`] on why a stable entry name and a
    /// long max-age is a deploy nobody receives.
    #[must_use = "this returns a configured server rather than configuring in place"]
    pub fn cached_for(mut self, cache_control: impl Into<String>) -> Self {
        self.cache_control = Some(cache_control.into());
        self
    }

    /// The path this request names, if it names one inside the root.
    ///
    /// `None` for anything that escapes, is a dotfile, or cannot be decoded.
    /// Separated from the serving so the rule can be asserted on without a
    /// filesystem — traversal is the part worth testing exhaustively, and it
    /// is pure.
    pub fn resolve(&self, uri_path: &str) -> Option<PathBuf> {
        let decoded = percent_decode_str(uri_path.trim_start_matches('/')).decode_utf8().ok()?;

        // Rejected by *component*, after decoding, rather than by looking for
        // `..` in the string. `%2e%2e%2f`, `..\` on Windows and a bare `..`
        // are one thing once parsed, and three things to a pattern.
        let mut safe = PathBuf::new();
        for component in Path::new(decoded.as_ref()).components() {
            match component {
                Component::Normal(part) => {
                    // A dotfile anywhere in the path, not only at the end:
                    // `.git/config` is `.git` then `config`.
                    if part.to_str()?.starts_with('.') {
                        return None;
                    }
                    safe.push(part);
                }
                // `..`, `/`, `C:\` and `.` all mean the request is trying to
                // say something about where the root is. Nothing legitimate
                // needs to.
                Component::ParentDir
                | Component::RootDir
                | Component::Prefix(_)
                | Component::CurDir => return None,
            }
        }

        if safe.as_os_str().is_empty() {
            // The root itself. Only meaningful with an index.
            return self.index.as_ref().map(|name| self.root.join(name));
        }

        Some(self.root.join(safe))
    }

    /// Serve the file this request names, or answer `404`.
    ///
    /// Never an error for a request that names nothing: a missing file is a
    /// 404, which is what the router would have said anyway.
    pub async fn serve(&self, request: &Request) -> Response {
        let Some(path) = self.resolve(request.path()) else {
            return not_found();
        };

        let Ok(metadata) = tokio::fs::metadata(&path).await else {
            return not_found();
        };

        let path = if metadata.is_dir() {
            match &self.index {
                Some(name) => path.join(name),
                None => return not_found(),
            }
        } else {
            path
        };

        // Resolved last, and this is the check that matters: everything above
        // reasons about the request, and this reasons about what the
        // filesystem actually produced — which is the only thing that knows
        // where a symlink points.
        let (Ok(resolved), Ok(root)) =
            (tokio::fs::canonicalize(&path).await, tokio::fs::canonicalize(&self.root).await)
        else {
            return not_found();
        };
        if !resolved.starts_with(&root) {
            tracing::warn!(path = %resolved.display(), "a public path resolved outside the root");
            return not_found();
        }

        let Ok(metadata) = tokio::fs::metadata(&resolved).await else {
            return not_found();
        };
        if !metadata.is_file() {
            return not_found();
        }

        let tag = etag(&metadata);

        // Answered before the file is read, which is the point of asking.
        if request.header("if-none-match").is_some_and(|given| given.trim() == tag) {
            return self.headers(Response::new(StatusCode::NOT_MODIFIED), &resolved, &tag);
        }

        let Ok(bytes) = tokio::fs::read(&resolved).await else {
            return not_found();
        };

        // A HEAD is a GET whose body is dropped — the headers, including
        // length, must be the ones a GET would have produced.
        let body_len = bytes.len();
        let response = if request.method().as_str() == "HEAD" {
            Response::new(StatusCode::OK)
        } else {
            Response::new(StatusCode::OK).with_body(bytes)
        };

        self.headers(response, &resolved, &tag).with_header("content-length", &body_len.to_string())
    }

    fn headers(&self, response: Response, path: &Path, tag: &str) -> Response {
        let mut response = response
            .with_header("content-type", content_type(path))
            .with_header("etag", tag)
            // Says the range unit is understood even though ranges are not
            // served, so a client asks rather than assuming — see the module
            // docs. `none` is the honest answer.
            .with_header("accept-ranges", "none");

        if let Some(cache_control) = &self.cache_control {
            response = response.with_header("cache-control", cache_control);
        }

        response
    }
}

fn not_found() -> Response {
    Error::not_found("Not Found").into_response()
}

/// A tag from what the filesystem knows without reading the file.
///
/// Length and modification time. Not a hash of the contents: this runs on every
/// request for every asset, and hashing a megabyte to decide whether to send a
/// megabyte is a trade with only one side. Two files of identical length
/// written in the same millisecond would collide, which is a rebuild that
/// produced byte-identical output.
fn etag(metadata: &std::fs::Metadata) -> String {
    let modified = metadata
        .modified()
        .ok()
        .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|since| since.as_millis())
        .unwrap_or(0);

    format!("\"{:x}-{:x}\"", metadata.len(), modified)
}

/// The `Content-Type` for a file, by extension.
///
/// A fixed table rather than sniffing. Sniffing a file somebody uploaded is
/// how a `.png` becomes `text/html` and runs on this origin; an extension the
/// table does not know is `application/octet-stream`, which downloads rather
/// than renders.
fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() {
        Some("html" | "htm") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("json") => "application/json",
        Some("map") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("avif") => "image/avif",
        Some("ico") => "image/x-icon",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("ttf") => "font/ttf",
        Some("otf") => "font/otf",
        Some("txt") => "text/plain; charset=utf-8",
        Some("xml") => "application/xml",
        Some("pdf") => "application/pdf",
        Some("webmanifest") => "application/manifest+json",
        Some("mp4") => "video/mp4",
        Some("webm") => "video/webm",
        Some("mp3") => "audio/mpeg",
        Some("wasm") => "application/wasm",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files() -> PublicFiles {
        PublicFiles::at("public")
    }

    #[test]
    fn an_ordinary_path_resolves_under_the_root() {
        assert_eq!(files().resolve("/img/logo.png"), Some(PathBuf::from("public/img/logo.png")));
        assert_eq!(files().resolve("favicon.ico"), Some(PathBuf::from("public/favicon.ico")));
    }

    #[test]
    fn a_percent_encoded_name_is_decoded() {
        // A space in a filename is ordinary and arrives encoded.
        assert_eq!(files().resolve("/my%20file.txt"), Some(PathBuf::from("public/my file.txt")));
    }

    #[test]
    fn nothing_climbs_out_of_the_root() {
        // The one that matters. Every spelling of it, because rejecting `..`
        // as a substring catches the first and misses the rest.
        for hostile in [
            "/../secrets.txt",
            "/img/../../secrets.txt",
            "/%2e%2e/secrets.txt",
            "/%2e%2e%2fsecrets.txt",
            "/..%2fsecrets.txt",
            "/./../secrets.txt",
        ] {
            assert_eq!(files().resolve(hostile), None, "{hostile}");
        }
    }

    #[test]
    fn an_absolute_path_is_not_a_request_for_that_file() {
        for hostile in ["//etc/passwd", "/%2fetc%2fpasswd"] {
            let resolved = files().resolve(hostile);
            assert!(
                resolved.is_none_or(|p| p.starts_with("public")),
                "{hostile} escaped: {:?}",
                files().resolve(hostile),
            );
        }
    }

    #[test]
    fn a_dotfile_is_not_served_wherever_it_sits() {
        // `.env` is the one that ends a company. `.git/config` carries a
        // remote with a token in it more often than anybody admits.
        for hidden in ["/.env", "/.git/config", "/img/.htpasswd", "/.well-known/../.env"] {
            assert_eq!(files().resolve(hidden), None, "{hidden}");
        }
    }

    #[test]
    fn the_root_serves_nothing_without_an_index() {
        // A directory that serves something unasked is how a listing becomes
        // a disclosure.
        assert_eq!(files().resolve("/"), None);
        assert_eq!(files().resolve(""), None);

        let with_index = files().with_index("index.html");
        assert_eq!(with_index.resolve("/"), Some(PathBuf::from("public/index.html")));
    }

    #[test]
    fn a_type_comes_from_the_extension_and_not_the_bytes() {
        // Sniffing an uploaded file is how a `.png` becomes `text/html` and
        // runs on this origin.
        assert_eq!(content_type(Path::new("a/b/logo.png")), "image/png");
        assert_eq!(content_type(Path::new("app.js")), "text/javascript; charset=utf-8");
        assert_eq!(content_type(Path::new("style.CSS")), "text/css; charset=utf-8");
        assert_eq!(content_type(Path::new("archive.tar.gz")), "application/octet-stream");
        assert_eq!(content_type(Path::new("noextension")), "application/octet-stream");
    }

    #[test]
    fn a_tag_changes_when_the_file_does() {
        // Same length, different mtime — a rebuild that produced the same
        // number of bytes must still invalidate.
        let one = format!("\"{:x}-{:x}\"", 100u64, 1u64);
        let two = format!("\"{:x}-{:x}\"", 100u64, 2u64);

        assert_ne!(one, two);
    }
}
