//! The debug error page — Rainier's answer to [Whoops].
//!
//! A 500 in development should tell you where it came from. Rainier's default
//! renderer gives you a status and a sentence, which is right for production
//! and useless at 2am. This gives you the stack, the source around each frame,
//! and the request that caused it.
//!
//! ```no_run
//! use std::sync::Arc;
//! use rainier_debug::DebugExceptionRenderer;
//! # use rainier_server::Kernel;
//! # fn wire(kernel: Kernel) -> Kernel {
//! kernel.with_renderer(Arc::new(DebugExceptionRenderer::new()))
//! # }
//! ```
//!
//! # It cannot render in production
//!
//! Three independent locks, because this page exists to disclose exactly the
//! things an attacker would like to read, and "we set the flag correctly" is
//! not a security control:
//!
//! 1. **`debug` must be true.** The kernel passes it from `app.debug`
//!    (`APP_DEBUG`). False is the default, and false means this renderer
//!    delegates to the plain one without looking at anything.
//! 2. **`APP_ENV=production` refuses regardless.** Even with `APP_DEBUG=true`.
//!    A production environment with debug accidentally on is precisely the
//!    accident that has to be survivable, and the two flags disagreeing is a
//!    misconfiguration this crate does not resolve in favour of disclosure.
//!    [`DebugExceptionRenderer::allow_in_production`] exists and is the only
//!    way past it — it is deliberately awkward and deliberately named.
//! 3. **Values are redacted anyway.** See [`redact`]: keys that look like
//!    credentials are replaced, everything else is truncated, and the
//!    environment panel is an allowlist. This is what makes a *screenshot* of
//!    the page safe, which is how Whoops has historically leaked secrets — not
//!    by being reachable, but by being pasted into a ticket.
//!
//! # What you need for a useful page
//!
//! ```env
//! APP_DEBUG=true
//! RUST_BACKTRACE=1
//! ```
//!
//! `RUST_BACKTRACE` is read once per process, so it has to be set before the
//! application starts. Without it every error still renders — with the message,
//! the request and no stack, and the page says so rather than looking broken.
//!
//! Source excerpts additionally need the source to be *on the machine*, at the
//! path the debug info recorded. True when you `cargo run`; false inside a
//! container built elsewhere, where the page shows frames without code and
//! explains why.
//!
//! # The source excerpt is the one thing redaction cannot reach
//!
//! [`redact`] filters the request. It cannot filter your source, and the page
//! prints sixteen lines of it around every frame. So a credential hardcoded
//! near a failing line **will be on the page**, and no denylist will catch it.
//!
//! This is not a flaw to be fixed — a debug page that hides your source is not
//! a debug page — and Whoops has exactly the same property. It is why the page
//! is debug-only rather than merely redacted, and it is worth knowing before
//! screenshotting one into a ticket.
//!
//! Found the same way everything here was found: a test asserted a fake token
//! was absent from the page, and it was present, because the token was a
//! literal in the test file and a frame resolved to that file.
//!
//! [Whoops]: https://github.com/filp/whoops

#![forbid(unsafe_code)]

pub mod frames;
pub mod redact;
pub mod render;

use std::path::PathBuf;
use std::sync::Arc;

use rainier_http::{RenderedError, Request, Response, StatusCode};
use rainier_server::{DefaultExceptionRenderer, ExceptionRenderer};

/// Re-exported from `rainier-support`, where it has to live: the *kernel* is
/// what catches a panic, and the kernel cannot depend on this crate.
///
/// Call it once during bootstrap. Without it a panic still renders an error
/// page — with the panic message and no stack, because `catch_unwind` does not
/// carry one.
pub use rainier_support::panic_backtrace::install_panic_hook;

/// How many lines either side of a frame's line to show.
const SOURCE_RADIUS: u32 = 8;

/// Does this environment name mean production?
///
/// `prod` as well as `production`, because both are in use across this estate
/// and the one that is not recognised is the one that leaks.
fn env_names_production(name: Option<&str>) -> bool {
    let Some(name) = name else { return false };
    let name = name.trim().to_ascii_lowercase();
    name == "production" || name == "prod"
}

/// An [`ExceptionRenderer`] that renders a developer-facing error page.
///
/// Falls back to [`DefaultExceptionRenderer`] whenever it must not render —
/// see the module docs for the three locks.
pub struct DebugExceptionRenderer {
    fallback: DefaultExceptionRenderer,
    app_root: Option<PathBuf>,
    editor: Option<String>,
    allow_in_production: bool,
    /// Whether `APP_ENV` named production, read **once** at construction.
    ///
    /// Not per-render, for two reasons. It is a syscall on the error path, and
    /// — the one that actually bit — a process-wide mutable global makes the
    /// renderer's behaviour depend on whatever else the process has done since,
    /// which is untestable in parallel: one test setting `APP_ENV` changed the
    /// result of another running beside it.
    env_is_production: bool,
}

impl Default for DebugExceptionRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl DebugExceptionRenderer {
    /// A renderer with the defaults: the current directory as the application
    /// root, and no editor links.
    pub fn new() -> Self {
        Self {
            fallback: DefaultExceptionRenderer,
            app_root: std::env::current_dir().ok(),
            editor: None,
            allow_in_production: false,
            env_is_production: env_names_production(std::env::var("APP_ENV").ok().as_deref()),
        }
    }

    /// State the environment rather than reading it from the process.
    ///
    /// What the bootstrap should use when it already has the parsed config —
    /// `app.env` — rather than making this crate re-read the variable and
    /// possibly disagree with the application about which environment it is in.
    pub fn with_environment(mut self, name: &str) -> Self {
        self.env_is_production = env_names_production(Some(name));
        self
    }

    /// Where the application's own source lives.
    ///
    /// Used to tell your frames from the framework's and the registry's. It
    /// defaults to the process's working directory, which is right for
    /// `cargo run` and wrong for a binary started from elsewhere.
    pub fn with_app_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.app_root = Some(root.into());
        self
    }

    /// A URL template for the "open in editor" link.
    ///
    /// `{file}` and `{line}` are substituted. The common ones:
    ///
    /// ```text
    /// phpstorm://open?file={file}&line={line}
    /// idea://open?file={file}&line={line}
    /// vscode://file/{file}:{line}
    /// ```
    ///
    /// Off unless set: a link to a scheme the machine has no handler for is a
    /// dead link, and guessing the developer's editor is not this crate's job.
    pub fn with_editor(mut self, template: impl Into<String>) -> Self {
        self.editor = Some(template.into());
        self
    }

    /// Render even when `APP_ENV` says production.
    ///
    /// **Almost certainly not what you want.** The one legitimate use is an
    /// environment that calls itself production and is not — a staging copy
    /// built from the production profile, say. If you are reaching for this on
    /// something the public can reach, the page will show request bodies,
    /// headers and file paths to whoever triggers a 500.
    pub fn allow_in_production(mut self) -> Self {
        self.allow_in_production = true;
        self
    }

    /// Lock 2. An environment naming production vetoes the page.
    fn production(&self) -> bool {
        self.env_is_production && !self.allow_in_production
    }

    /// The request panels, redacted.
    fn panels(&self, request: &Request) -> Vec<render::Panel> {
        let mut panels = Vec::new();

        panels.push(render::Panel {
            title: "Request".into(),
            rows: vec![
                ("method".into(), request.method().to_string()),
                ("path".into(), redact::truncate(request.path())),
                ("query".into(), redact::truncate(request.query_string())),
                ("version".into(), format!("{:?}", request.version())),
                ("content-type".into(), request.content_type().unwrap_or_else(|| "—".into())),
            ],
        });

        panels.push(render::Panel {
            title: "Headers".into(),
            rows: request
                .headers()
                .iter()
                .map(|(name, value)| {
                    let name = name.as_str().to_string();
                    let raw = value.to_str().unwrap_or("<binary>");
                    let shown = redact::value(&name, raw);
                    (name, shown)
                })
                .collect(),
        });

        // The body, as parsed input rather than raw bytes — it is what the
        // handler actually saw, and `redact::json` can walk it.
        let input = request.all();
        if !input.is_null() {
            let redacted = redact::json(&input);
            let rows = match &redacted {
                serde_json::Value::Object(map) => map
                    .iter()
                    .map(|(k, v)| {
                        (k.clone(), v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()))
                    })
                    .collect(),
                other => vec![("body".into(), other.to_string())],
            };
            panels.push(render::Panel { title: "Input".into(), rows });
        }

        let cookies = request.cookies();
        if !cookies.is_empty() {
            panels.push(render::Panel {
                title: "Cookies".into(),
                // Every cookie is redacted by name — `is_secret_key` matches
                // "cookie" and "session" — but the NAMES are worth showing:
                // "which cookies were present" is a real debugging question
                // and does not require their values.
                rows: cookies
                    .keys()
                    .map(|name| (name.clone(), "[redacted by rainier-debug]".to_string()))
                    .collect(),
            });
        }

        let env = redact::environment();
        if !env.is_empty() {
            panels.push(render::Panel { title: "Environment".into(), rows: env });
        }

        panels
    }

    /// The JSON form, for a client that asked for JSON.
    ///
    /// Same information, same redaction — an API developer debugging a 500
    /// deserves the stack too, and telling them to change their `Accept`
    /// header to see it would be a silly thing to require.
    fn json(
        &self,
        error: &RenderedError,
        frames: &[frames::Frame],
        status: StatusCode,
    ) -> Response {
        let stack: Vec<serde_json::Value> = frames
            .iter()
            .map(|frame| {
                serde_json::json!({
                    "symbol": frame.symbol,
                    "file": frame.file.as_ref().map(|f| f.display().to_string()),
                    "line": frame.line,
                    "origin": frame.origin.as_str(),
                })
            })
            .collect();

        Response::json(&serde_json::json!({
            "message": error.message,
            "errors": error.details,
            "debug": {
                "note": "rainier-debug is on. This is never rendered when APP_DEBUG is false.",
                "stack": stack,
            }
        }))
        .with_status(status)
    }
}

impl ExceptionRenderer for DebugExceptionRenderer {
    fn render(&self, request: &Request, error: &RenderedError, debug: bool) -> Response {
        // Lock 2, first, because it is the stricter one.
        //
        // **`false`, not `debug`.** Forwarding `debug` here was a real hole,
        // and the test below is the one that caught it:
        // `DefaultExceptionRenderer` discloses a 5xx message whenever `debug`
        // is true, so passing it through meant `APP_ENV=production` plus
        // `APP_DEBUG=true` suppressed the pretty page and leaked the message
        // anyway — the exact combination this lock exists for. Suppressing the
        // page while disclosing its most sensitive line is worse than either
        // alternative on its own.
        //
        // If the environment names production, nothing discloses. That is the
        // whole claim, and it has to hold regardless of the other flag.
        if self.production() {
            if debug {
                tracing::warn!(
                    "APP_DEBUG is true but the environment names production — the debug error \
                     page was suppressed AND 5xx messages were hidden. Set APP_DEBUG=false, or \
                     correct APP_ENV."
                );
            }
            return self.fallback.render(request, error, false);
        }

        // Lock 1. Not debugging: behave exactly as though this crate were not
        // installed, `debug` included — outside production it is the
        // application's flag to honour.
        if !debug {
            return self.fallback.render(request, error, debug);
        }

        let status =
            StatusCode::from_u16(error.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

        let raw = error.backtrace.as_deref().unwrap_or_default();
        let parsed = frames::trim_noise(frames::parse(raw, self.app_root.as_deref()));

        if request.expects_json() {
            return self.json(error, &parsed, status);
        }

        let excerpts: Vec<Option<frames::Excerpt>> = parsed
            .iter()
            .map(|frame| match (&frame.file, frame.line) {
                (Some(file), Some(line)) => frames::excerpt(file, line, SOURCE_RADIUS),
                _ => None,
            })
            .collect();

        let page = render::Page {
            status: error.status,
            reason: status.canonical_reason().unwrap_or("Error"),
            message: &error.message,
            kind: error.kind.as_deref().unwrap_or("Error"),
            frames: &parsed,
            excerpts: &excerpts,
            raw_backtrace: if parsed.is_empty() && !raw.is_empty() { Some(raw) } else { None },
            panels: self.panels(request),
            editor: self.editor.as_deref(),
        };

        Response::html(page.render()).with_status(status)
    }
}

/// Build a renderer from the environment.
///
/// Reads `RAINIER_DEBUG_EDITOR` for the editor template, so a developer can
/// turn on editor links without touching the application's bootstrap — which
/// matters because the right value differs per person on the same team.
pub fn from_env() -> Arc<DebugExceptionRenderer> {
    let mut renderer = DebugExceptionRenderer::new();
    if let Ok(template) = std::env::var("RAINIER_DEBUG_EDITOR") {
        if !template.trim().is_empty() {
            renderer = renderer.with_editor(template);
        }
    }
    Arc::new(renderer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_http::{Method, Request};

    fn request() -> Request {
        Request::builder().method(Method::GET).uri("/posts/1").build()
    }

    /// Collect the response body. `Response::body` is a stream, so this is
    /// what every test here needs and none of them should repeat.
    async fn body_of(response: Response) -> String {
        response.into_string().await.expect("the error page body should be readable")
    }

    fn error() -> RenderedError {
        RenderedError {
            status: 500,
            message: "the search index is unreachable".into(),
            details: None,
            disclosable: false,
            kind: Some("Internal".into()),
            backtrace: None,
        }
    }

    #[tokio::test]
    async fn debug_off_falls_back_to_the_plain_renderer() {
        let renderer = DebugExceptionRenderer::new().with_environment("local");
        let response = renderer.render(&request(), &error(), false);
        let body = body_of(response).await;
        assert!(!body.contains("rainier-debug"), "no debug page");
        // The plain renderer hides a 5xx message when debug is off.
        assert!(body.contains("Server Error"));
    }

    #[tokio::test]
    async fn a_production_environment_refuses_even_with_debug_on() {
        let renderer = DebugExceptionRenderer::new().with_environment("production");
        let response = renderer.render(&request(), &error(), true);
        let body = body_of(response).await;

        assert!(
            !body.contains("the search index is unreachable"),
            "a 5xx message must not reach the client in production, debug or not —              forwarding `debug` to the fallback renderer used to leak exactly this"
        );
        assert!(body.contains("Server Error"), "and it gets the generic message instead");
    }

    #[tokio::test]
    async fn prod_is_spelled_both_ways() {
        for name in ["production", "prod", "PRODUCTION", " Prod "] {
            let renderer = DebugExceptionRenderer::new().with_environment(name);
            let body = body_of(renderer.render(&request(), &error(), true)).await;
            assert!(!body.contains("the search index is unreachable"), "{name} should refuse");
        }
    }

    #[tokio::test]
    async fn the_escape_hatch_is_the_only_way_past_production() {
        let renderer =
            DebugExceptionRenderer::new().with_environment("production").allow_in_production();
        let body = body_of(renderer.render(&request(), &error(), true)).await;

        assert!(body.contains("the search index is unreachable"));
    }

    #[tokio::test]
    async fn debug_on_renders_the_page() {
        let renderer = DebugExceptionRenderer::new().with_environment("local");
        let response = renderer.render(&request(), &error(), true);
        let body = body_of(response).await;
        assert!(body.contains("the search index is unreachable"));
        assert!(body.contains("500"));
    }

    #[tokio::test]
    async fn an_authorization_header_never_reaches_the_page() {
        let request = Request::builder()
            .method(Method::GET)
            .uri("/posts/1")
            .header("authorization", "Bearer super-secret-value")
            .build();

        let renderer = DebugExceptionRenderer::new().with_environment("local");
        let response = renderer.render(&request, &error(), true);
        let body = body_of(response).await;

        assert!(!body.contains("super-secret-value"), "the token must be redacted");
        assert!(body.contains("authorization"), "but the header name is still listed");
    }

    #[tokio::test]
    async fn a_json_client_gets_the_stack_as_json() {
        let request = Request::builder()
            .method(Method::GET)
            .uri("/api/posts")
            .header("accept", "application/json")
            .build();

        let renderer = DebugExceptionRenderer::new().with_environment("local");
        let response = renderer.render(&request, &error(), true);
        let body = body_of(response).await;

        assert!(body.contains("\"debug\""));
        assert!(body.contains("the search index is unreachable"));
    }
}
