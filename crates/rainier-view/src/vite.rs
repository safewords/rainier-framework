//! Vite — the `@vite` directive's resolver.
//!
//! The shape will be familiar from PHP frameworks: `resources/js` and `resources/css`
//! are source, Vite compiles them into `public/build`, and a template asks for
//! an *entry* rather than a file:
//!
//! ```text
//! @vite('resources/js/app.js')
//! @vite(['resources/css/app.css', 'resources/js/app.js'])
//! ```
//!
//! What that renders depends on which of two artefacts exists:
//!
//! - **`public/hot`** — written by the dev server while `npm run dev` runs,
//!   holding its origin (`http://localhost:5173`). The directive emits the
//!   Vite client and points every entry at the dev server, so edits hot-reload
//!   and nothing is compiled to disk.
//! - **`public/build/manifest.json`** — written by `npm run build`. The
//!   directive resolves each entry to its content-hashed file, plus the
//!   stylesheets that entry imports, and emits tags under `/build/…`.
//!
//! Missing both is not an error — the directive renders an HTML comment
//! naming the two commands, so a page arrives unstyled rather than down. An
//! application with no frontend build never notices any of this; the opt-in
//! is running Vite, not configuring a feature.
//!
//! # What is deliberately absent
//!
//! No dev-server proxying, no SSR entries, no asset helpers for images —
//! those can arrive when something needs them. This resolves entries to tags,
//! which is the part every page needs on day one.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use rainier_support::{Error, Result};
use serde::Deserialize;

use crate::engine::escape_html;

/// Resolves `@vite` entries to `<script>` and `<link>` tags.
///
/// One per engine, attached with
/// [`TemplateEngine::with_vite`](crate::TemplateEngine::with_vite). The
/// framework's default bootstrap attaches one over `<base>/public`, so an
/// application only builds its own to move the directories.
pub struct Vite {
    /// The web root — where `hot` and `build/` live.
    public: PathBuf,
    /// The build directory under `public`, and the URL prefix the built
    /// files are served from. The PHP-framework convention, kept: `build`.
    build: String,
    /// The parsed manifest, once read. `None` until first use.
    manifest: RwLock<Option<Arc<HashMap<String, ManifestEntry>>>>,
    /// Whether to keep the parsed manifest. On in production — the manifest
    /// only changes when a deploy replaces it, and a deploy replaces the
    /// process too. The hot file is checked every render regardless: that is
    /// the file whose whole point is appearing and disappearing mid-process.
    caching: bool,
}

/// One entry of Vite's `manifest.json`. Unknown fields are ignored on
/// purpose — the manifest schema grows, and tags need only these.
#[derive(Debug, Clone, Deserialize)]
struct ManifestEntry {
    /// The content-hashed output file.
    file: String,
    /// Stylesheets this entry imports, already content-hashed.
    #[serde(default)]
    css: Vec<String>,
}

impl Vite {
    /// A resolver over `public` — the directory holding `hot` and `build/`.
    pub fn new(public: impl Into<PathBuf>) -> Self {
        Self {
            public: public.into(),
            build: "build".to_string(),
            manifest: RwLock::new(None),
            caching: true,
        }
    }

    /// Use a different build directory (and URL prefix) than `build`.
    pub fn with_build_dir(mut self, dir: impl Into<String>) -> Self {
        self.build = dir.into();
        self
    }

    /// Re-read the manifest on every render.
    ///
    /// For development against a built bundle. The hot file needs no such
    /// setting — it is consulted every render always.
    pub fn without_cache(mut self) -> Self {
        self.caching = false;
        self
    }

    /// The web root this resolver watches — where `hot` and the build live.
    ///
    /// For the code around the directive that must agree with it: the route
    /// serving the built files, a test writing a manifest to exercise the
    /// built branch.
    pub fn public_root(&self) -> &std::path::Path {
        &self.public
    }

    /// Whether a frontend is present at all — a dev server running or a
    /// build compiled.
    ///
    /// For the application code that has to *choose*, not render: a
    /// controller deciding between the page that mounts the frontend and a
    /// server-rendered fallback asks this, where a template just says
    /// `@vite` and gets the comment when the answer would have been no.
    /// Checked fresh every call, like the hot file — this answer's whole
    /// point is changing while the process runs.
    pub fn is_active(&self) -> bool {
        self.hot_origin().is_some()
            || self.public.join(&self.build).join("manifest.json").is_file()
            || self.public.join(&self.build).join(".vite").join("manifest.json").is_file()
    }

    /// The tags for `entries`, in order.
    ///
    /// Dev server first: when `public/hot` exists its contents are the
    /// server's origin, and every entry is served from there un-compiled.
    /// Otherwise the manifest resolves each entry to its hashed build
    /// output.
    ///
    /// **Neither existing renders an HTML comment, not an error.** The state
    /// means nobody has run `npm run dev` or `npm run build` — which is a
    /// choice an application without a frontend build has made on purpose,
    /// and one a `git clone && cargo run` is in before it has made any choice
    /// at all. The page arrives unstyled with the fix written into its
    /// source, instead of being down. A *misconfiguration* against a build
    /// that exists — an entry the manifest does not name, a manifest that
    /// does not parse — stays a hard error.
    pub fn tags(&self, entries: &[String]) -> Result<String> {
        if let Some(origin) = self.hot_origin() {
            return Ok(Self::dev_tags(&origin, entries));
        }

        let Some(manifest) = self.manifest()? else {
            return Ok(format!(
                "<!-- @vite: no dev server and no build under {} — run `npm run dev` or \
                 `npm run build` -->",
                escape_html(&format!("{}/{}", self.public.display(), self.build)),
            ));
        };
        let mut out = String::new();

        for entry in entries {
            let Some(resolved) = manifest.get(entry) else {
                return Err(Error::internal(format!(
                    "`@vite` entry `{entry}` is not in the build manifest — is it named in \
                     vite.config.js under build.rollupOptions.input?"
                )));
            };

            // The entry's imported stylesheets first, then the entry itself,
            // so styles are applied before the script that assumes them runs.
            for css in &resolved.css {
                Self::push_stylesheet(&mut out, &format!("/{}/{}", self.build, css));
            }
            let href = format!("/{}/{}", self.build, resolved.file);
            if looks_like_css(&resolved.file) {
                Self::push_stylesheet(&mut out, &href);
            } else {
                Self::push_module(&mut out, &href);
            }
        }

        Ok(out)
    }

    /// The dev server's origin, when it is running.
    fn hot_origin(&self) -> Option<String> {
        let contents = std::fs::read_to_string(self.public.join("hot")).ok()?;
        let origin = contents.trim().trim_end_matches('/').to_string();
        (!origin.is_empty()).then_some(origin)
    }

    /// Tags pointing at the dev server: the Vite client, then each entry.
    fn dev_tags(origin: &str, entries: &[String]) -> String {
        let mut out = String::new();
        Self::push_module(&mut out, &format!("{origin}/@vite/client"));
        for entry in entries {
            let src = format!("{origin}/{entry}");
            if looks_like_css(entry) {
                Self::push_stylesheet(&mut out, &src);
            } else {
                Self::push_module(&mut out, &src);
            }
        }
        out
    }

    /// The parsed manifest, reading it on first use — `None` when no build
    /// exists at all, which the caller renders as a comment rather than an
    /// error.
    fn manifest(&self) -> Result<Option<Arc<HashMap<String, ManifestEntry>>>> {
        if self.caching {
            if let Some(cached) =
                self.manifest.read().expect("vite manifest lock poisoned").as_ref()
            {
                return Ok(Some(Arc::clone(cached)));
            }
        }

        // Two candidate paths: Vite writes `manifest.json` at the build root
        // when told to by name, and `.vite/manifest.json` when merely told
        // `true`. Reading both means a config that says either works.
        let build = self.public.join(&self.build);
        let Ok(source) = std::fs::read_to_string(build.join("manifest.json"))
            .or_else(|_| std::fs::read_to_string(build.join(".vite").join("manifest.json")))
        else {
            return Ok(None);
        };

        let manifest: HashMap<String, ManifestEntry> = serde_json::from_str(&source)
            .map_err(|e| Error::internal(format!("the Vite manifest did not parse: {e}")))?;
        let manifest = Arc::new(manifest);

        if self.caching {
            *self.manifest.write().expect("vite manifest lock poisoned") =
                Some(Arc::clone(&manifest));
        }
        Ok(Some(manifest))
    }

    fn push_module(out: &mut String, src: &str) {
        out.push_str(&format!("<script type=\"module\" src=\"{}\"></script>", escape_html(src)));
    }

    fn push_stylesheet(out: &mut String, href: &str) {
        out.push_str(&format!("<link rel=\"stylesheet\" href=\"{}\">", escape_html(href)));
    }
}

impl std::fmt::Debug for Vite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vite")
            .field("public", &self.public)
            .field("build", &self.build)
            .field("caching", &self.caching)
            .finish()
    }
}

/// Whether an entry or output file is a stylesheet, by extension.
fn looks_like_css(path: &str) -> bool {
    let lowered = path.to_ascii_lowercase();
    ["css", "scss", "sass", "less", "styl", "stylus", "pcss", "postcss"]
        .iter()
        .any(|extension| lowered.ends_with(&format!(".{extension}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rainier-vite-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn entries(list: &[&str]) -> Vec<String> {
        list.iter().map(|entry| (*entry).to_string()).collect()
    }

    #[test]
    fn a_hot_file_points_every_entry_at_the_dev_server() {
        let public = temp_dir("hot");
        std::fs::write(public.join("hot"), "http://localhost:5173\n").unwrap();

        let tags = Vite::new(&public)
            .tags(&entries(&["resources/js/app.js", "resources/css/app.css"]))
            .unwrap();

        // The client first — it is what makes hot reload work at all.
        assert!(tags.starts_with(
            "<script type=\"module\" src=\"http://localhost:5173/@vite/client\"></script>"
        ));
        assert!(tags.contains("src=\"http://localhost:5173/resources/js/app.js\""));
        assert!(tags.contains(
            "<link rel=\"stylesheet\" href=\"http://localhost:5173/resources/css/app.css\">"
        ));
    }

    #[test]
    fn a_manifest_resolves_entries_to_hashed_files_with_their_css() {
        let public = temp_dir("manifest");
        std::fs::create_dir_all(public.join("build")).unwrap();
        std::fs::write(
            public.join("build").join("manifest.json"),
            r#"{
                "resources/js/app.js": {
                    "file": "assets/app-Bx91.js",
                    "css": ["assets/app-Cd12.css"],
                    "src": "resources/js/app.js",
                    "isEntry": true
                }
            }"#,
        )
        .unwrap();

        let tags = Vite::new(&public).tags(&entries(&["resources/js/app.js"])).unwrap();

        // The stylesheet lands before the script that assumes it.
        let css = tags.find("/build/assets/app-Cd12.css").expect("the css tag");
        let js = tags.find("/build/assets/app-Bx91.js").expect("the js tag");
        assert!(css < js, "{tags}");
    }

    #[test]
    fn the_dot_vite_manifest_location_works_too() {
        let public = temp_dir("dot-vite");
        std::fs::create_dir_all(public.join("build").join(".vite")).unwrap();
        std::fs::write(
            public.join("build").join(".vite").join("manifest.json"),
            r#"{ "resources/js/app.js": { "file": "assets/app-Zz.js" } }"#,
        )
        .unwrap();

        let tags = Vite::new(&public).tags(&entries(&["resources/js/app.js"])).unwrap();
        assert!(tags.contains("/build/assets/app-Zz.js"), "{tags}");
    }

    #[test]
    fn an_entry_the_manifest_does_not_name_is_an_error_naming_the_entry() {
        let public = temp_dir("missing-entry");
        std::fs::create_dir_all(public.join("build")).unwrap();
        std::fs::write(public.join("build").join("manifest.json"), "{}").unwrap();

        let err = Vite::new(&public).tags(&entries(&["resources/js/other.js"])).unwrap_err();
        assert!(err.message().contains("resources/js/other.js"), "{}", err.message());
    }

    #[test]
    fn no_dev_server_and_no_build_renders_a_comment_that_says_what_to_run() {
        // Unstyled, not down: an application without a frontend build made a
        // choice, and a fresh clone has not made one yet. The fix is written
        // into the page source instead of a 500.
        let public = temp_dir("nothing");

        let html = Vite::new(&public).tags(&entries(&["resources/js/app.js"])).unwrap();
        assert!(html.starts_with("<!--"), "{html}");
        assert!(html.contains("npm run dev"), "{html}");
        assert!(html.contains("npm run build"), "{html}");
    }

    #[test]
    fn activity_tracks_the_artefacts() {
        let public = temp_dir("activity");
        let vite = Vite::new(&public);
        assert!(!vite.is_active(), "an empty public dir has no frontend");

        std::fs::write(public.join("hot"), "http://localhost:5173").unwrap();
        assert!(vite.is_active(), "a dev server counts");
        std::fs::remove_file(public.join("hot")).unwrap();

        std::fs::create_dir_all(public.join("build")).unwrap();
        std::fs::write(public.join("build").join("manifest.json"), "{}").unwrap();
        assert!(vite.is_active(), "a build counts");
    }

    #[test]
    fn the_hot_origin_wins_over_a_build() {
        let public = temp_dir("both");
        std::fs::write(public.join("hot"), "http://localhost:5173").unwrap();
        std::fs::create_dir_all(public.join("build")).unwrap();
        std::fs::write(
            public.join("build").join("manifest.json"),
            r#"{ "resources/js/app.js": { "file": "assets/app-Aa.js" } }"#,
        )
        .unwrap();

        let tags = Vite::new(&public).tags(&entries(&["resources/js/app.js"])).unwrap();
        assert!(tags.contains("localhost:5173"), "{tags}");
        assert!(!tags.contains("app-Aa.js"), "{tags}");
    }
}
