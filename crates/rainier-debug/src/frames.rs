//! Turning a captured backtrace into something worth looking at.
//!
//! A [`std::backtrace::Backtrace`] is a `Display` impl and nothing else — the
//! structured frame API is still unstable. So this parses the text, which is
//! the trade this crate makes deliberately:
//!
//! - **against** parsing: the format is not a stability guarantee, and a
//!   future toolchain could change it.
//! - **for** parsing: the alternative is a `backtrace` crate dependency in
//!   `rainier-support`, where every application would link it forever so that
//!   a development-only error page can exist.
//!
//! The failure mode decides it. If the format changes, [`parse`] returns no
//! frames and the page falls back to rendering the raw text — which is exactly
//! what a developer would otherwise have read in the log. A dependency in the
//! core for that is the wrong price.
//!
//! ## The format being parsed
//!
//! ```text
//!    3: rainier_support::error::Error::internal
//!              at ./crates/rainier-support/src/error.rs:118:9
//!    4: myapp::app::http::controllers::posts::show::{{closure}}
//!              at ./src/app/http/controllers/posts.rs:42:5
//! ```
//!
//! An index and a symbol on one line; an optional `at path:line:col` on the
//! next. Frames without a location are kept — they are usually the interesting
//! boundary between your code and something compiled without debug info.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// Where a frame's code came from, which decides whether it is shown by
/// default.
///
/// Whoops draws the same distinction between application and vendor frames,
/// and it is the single thing that makes a deep stack readable: a Rust
/// backtrace through an async runtime is ~60 frames, of which perhaps four are
/// yours.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Inside the application's own source tree.
    Application,
    /// A Rainier crate.
    Framework,
    /// A third-party crate, the standard library, or the runtime.
    Vendor,
}

impl Origin {
    /// The CSS class and the filter label.
    pub fn as_str(self) -> &'static str {
        match self {
            Origin::Application => "app",
            Origin::Framework => "framework",
            Origin::Vendor => "vendor",
        }
    }
}

/// One resolved stack frame.
#[derive(Debug, Clone)]
pub struct Frame {
    /// The demangled symbol, e.g. `myapp::controllers::posts::show`.
    pub symbol: String,
    /// The source file, when the binary carried debug info for it.
    pub file: Option<PathBuf>,
    /// The 1-based line.
    pub line: Option<u32>,
    /// Where this frame's code came from.
    pub origin: Origin,
}

impl Frame {
    /// `file:line`, or the symbol when there is no location.
    pub fn location(&self) -> String {
        match (&self.file, self.line) {
            (Some(file), Some(line)) => format!("{}:{}", file.display(), line),
            (Some(file), None) => file.display().to_string(),
            _ => "<no location>".to_string(),
        }
    }

    /// The last two path components — enough to recognise, short enough to fit.
    pub fn short_file(&self) -> String {
        let Some(file) = &self.file else { return "<unknown>".to_string() };
        let mut parts: Vec<_> =
            file.components().rev().take(2).map(|c| c.as_os_str().to_string_lossy()).collect();
        parts.reverse();
        parts.join("/")
    }

    /// The trailing segment of the symbol path, without the noise.
    ///
    /// Three things are stripped, all of them observed on a real page:
    ///
    /// - `::{{closure}}` — every async fn produces one and it says nothing;
    ///   the enclosing function name is the useful part.
    /// - **generic arguments.** MSVC's demangler renders them in its own
    ///   mangled form, so `Error::internal` arrives as
    ///   `Error::internal<ref$<str$> >` and a naive `rsplit("::")` yields
    ///   `internal<ref$<str$> >`. Cutting at the first `<` fixes it on every
    ///   platform, because a generic argument can never precede the function
    ///   name in a path.
    /// - the module path, which is what makes the list readable at all.
    pub fn short_symbol(&self) -> String {
        let cleaned = self.symbol.replace("::{{closure}}", "");
        let without_generics = cleaned.split('<').next().unwrap_or(&cleaned).trim_end_matches("::");
        let short = without_generics.rsplit("::").next().unwrap_or(without_generics).trim();
        if short.is_empty() {
            cleaned
        } else {
            short.to_string()
        }
    }
}

/// A window of source around a frame's line.
#[derive(Debug, Clone)]
pub struct Excerpt {
    /// The 1-based line number of the first line in `lines`.
    pub first_line: u32,
    /// The lines, in order.
    pub lines: Vec<String>,
    /// The 1-based line the frame points at.
    pub highlight: u32,
}

/// Read `radius` lines either side of `line` from `path`.
///
/// `None` when the file is not there, which is the normal case in a container:
/// the paths baked into the debug info are the paths of the *build* machine.
/// The page degrades to the frame list, and says why rather than showing an
/// empty panel.
pub fn excerpt(path: &Path, line: u32, radius: u32) -> Option<Excerpt> {
    let source = std::fs::read_to_string(path).ok()?;

    let first = line.saturating_sub(radius).max(1);
    let last = line.saturating_add(radius);

    let lines: Vec<String> = source
        .lines()
        .skip(first as usize - 1)
        .take((last - first + 1) as usize)
        .map(|l| l.to_string())
        .collect();

    if lines.is_empty() {
        return None;
    }
    Some(Excerpt { first_line: first, lines, highlight: line })
}

/// Classify a path as application, framework or vendor.
///
/// `app_root` is the application's source directory. Everything under it is
/// the developer's own code; a Rainier crate is the framework; anything from
/// the registry, a git checkout or the toolchain is vendor.
///
/// Ordering matters: a Rainier crate developed as a path dependency *inside*
/// the app root would otherwise read as application code. Framework is checked
/// first for that reason.
fn classify(path: &Path, app_root: Option<&Path>) -> Origin {
    let text = path.to_string_lossy().replace('\\', "/");

    // The toolchain and the registry, first and unambiguously.
    if text.contains("/.cargo/registry/")
        || text.contains("/.cargo/git/")
        || text.starts_with("/rustc/")
        || text.contains("/rustlib/src/rust/")
    {
        return Origin::Vendor;
    }

    // A Rainier crate, however it was reached — a path dependency in a
    // workspace, or a checkout somewhere else entirely.
    if text.contains("/rainier-") || text.contains("crates/rainier") {
        return Origin::Framework;
    }

    if let Some(root) = app_root {
        let root = root.to_string_lossy().replace('\\', "/");
        if !root.is_empty() && text.starts_with(&root) {
            return Origin::Application;
        }
    }

    // A relative path is what rustc emits for the crate being compiled, so it
    // is the application unless something above already claimed it.
    if !text.starts_with('/') && !text.contains(':') {
        return Origin::Application;
    }

    Origin::Vendor
}

/// Parse the `Display` form of a backtrace into frames.
///
/// Returns an empty vec if nothing matched, which the caller treats as "show
/// the raw text instead" rather than as an error.
pub fn parse(rendered: &str, app_root: Option<&Path>) -> Vec<Frame> {
    let mut frames: Vec<Frame> = Vec::new();

    for raw in rendered.lines() {
        let line = raw.trim();

        // `at <path>:<line>:<col>` — belongs to the frame just pushed.
        if let Some(rest) = line.strip_prefix("at ") {
            if let Some(frame) = frames.last_mut() {
                if frame.file.is_none() {
                    let (file, lineno) = split_location(rest);
                    frame.origin = classify(&file, app_root);
                    frame.file = Some(file);
                    frame.line = lineno;
                }
            }
            continue;
        }

        // `<index>: <symbol>` — a new frame.
        let Some((index, symbol)) = line.split_once(": ") else { continue };
        if index.is_empty() || !index.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }

        let symbol = symbol.trim();
        if symbol.is_empty() {
            continue;
        }

        frames.push(Frame {
            symbol: symbol.to_string(),
            file: None,
            line: None,
            // Until an `at` line says otherwise. A frame with no debug info is
            // never the developer's own code in a debug build.
            origin: Origin::Vendor,
        });
    }

    frames
}

/// Split `path:line:col` into a path and a line.
///
/// Careful with Windows: `C:\src\main.rs:42:9` has three colons and the first
/// one is a drive letter. Splitting from the right and requiring digits gets
/// it right on both platforms without knowing which it is on.
fn split_location(text: &str) -> (PathBuf, Option<u32>) {
    let mut rest = text;
    let mut line = None;

    // Strip up to two trailing `:<digits>` groups (column, then line).
    for _ in 0..2 {
        let Some((head, tail)) = rest.rsplit_once(':') else { break };
        let Ok(number) = tail.parse::<u32>() else { break };
        // The first one parsed is the column, the second is the line.
        line = Some(number);
        rest = head;
    }

    (PathBuf::from(rest), line)
}

/// Drop the frames nobody wants to read, from both ends.
///
/// A Rust backtrace is bracketed by noise: the capture machinery at the top,
/// and the runtime's entry point at the bottom. Neither has ever helped
/// anybody. The frames between them are kept in full — this hides nothing in
/// the middle, because the middle is where the answer is.
pub fn trim_noise(frames: Vec<Frame>) -> Vec<Frame> {
    let is_capture_noise = |f: &Frame| {
        let s = &f.symbol;
        s.starts_with("std::backtrace")
            || s.starts_with("core::")
            || s.contains("Backtrace::capture")
            // Every constructor, not just `new`. `Error::internal` calls
            // `Error::new`, so trimming only `new` left `internal` sitting at
            // the top of the list — pointing at `error.rs`, which is never the
            // frame anybody opened the page to find.
            || s.contains("rainier_support::error::Error::")
    };

    let mut frames = frames;
    while frames.first().is_some_and(is_capture_noise) {
        frames.remove(0);
    }
    frames
}

/// Render frames as plain text, for the JSON body and the log.
pub fn as_text(frames: &[Frame]) -> String {
    let mut out = String::new();
    for (index, frame) in frames.iter().enumerate() {
        let _ = writeln!(out, "{index:>3}: {} ({})", frame.symbol, frame.location());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
   0: std::backtrace::Backtrace::capture
             at /rustc/abcdef/library/std/src/backtrace.rs:1:1
   1: rainier_support::error::Error::new
             at ./crates/rainier-support/src/error.rs:107:9
   2: myapp::app::http::controllers::posts::show::{{closure}}
             at ./src/app/http/controllers/posts.rs:42:5
   3: tokio::runtime::task::harness::poll
             at /home/x/.cargo/registry/src/index.crates.io-1/tokio-1.0/src/task.rs:9:1
";

    #[test]
    fn parses_symbols_and_locations() {
        let frames = parse(SAMPLE, None);
        assert_eq!(frames.len(), 4);
        assert_eq!(frames[2].short_symbol(), "show");
        assert_eq!(frames[2].line, Some(42));
        assert_eq!(
            frames[2].file.as_ref().unwrap().to_string_lossy().replace('\\', "/"),
            "./src/app/http/controllers/posts.rs"
        );
    }

    #[test]
    fn classifies_registry_and_toolchain_as_vendor() {
        let frames = parse(SAMPLE, None);
        assert_eq!(frames[0].origin, Origin::Vendor, "the toolchain");
        assert_eq!(frames[3].origin, Origin::Vendor, "the registry");
    }

    #[test]
    fn classifies_rainier_crates_as_framework() {
        let frames = parse(SAMPLE, None);
        assert_eq!(frames[1].origin, Origin::Framework);
    }

    #[test]
    fn a_relative_path_is_the_application() {
        let frames = parse(SAMPLE, None);
        assert_eq!(frames[2].origin, Origin::Application);
    }

    #[test]
    fn trims_the_capture_machinery_from_the_top() {
        let frames = trim_noise(parse(SAMPLE, None));
        // Both the `Backtrace::capture` frame and `Error::new` go; the first
        // frame left is the one a developer would point at.
        assert_eq!(frames[0].short_symbol(), "show");
    }

    #[test]
    fn a_windows_path_keeps_its_drive_letter() {
        let (path, line) = split_location(r"C:\src\main.rs:42:9");
        assert_eq!(path.to_string_lossy(), r"C:\src\main.rs");
        assert_eq!(line, Some(42));
    }

    #[test]
    fn an_unparseable_backtrace_yields_no_frames_rather_than_panicking() {
        assert!(parse("something entirely different", None).is_empty());
        assert!(parse("", None).is_empty());
    }

    #[test]
    fn strips_msvc_generic_arguments_from_a_symbol() {
        // What the MSVC demangler actually produces, observed on a real page.
        let frame = Frame {
            symbol: "rainier_support::error::Error::internal<ref$<str$> >".into(),
            file: None,
            line: None,
            origin: Origin::Framework,
        };
        assert_eq!(frame.short_symbol(), "internal");
    }

    #[test]
    fn strips_the_closure_suffix_an_async_fn_adds() {
        let frame = Frame {
            symbol: "myapp::controllers::show::{{closure}}".into(),
            file: None,
            line: None,
            origin: Origin::Application,
        };
        assert_eq!(frame.short_symbol(), "show");
    }

    #[test]
    fn trims_every_error_constructor_not_just_new() {
        // `Error::internal` calls `Error::new`, so a stack has both and
        // trimming only `new` leaves `internal` on top pointing at error.rs.
        let raw = "\
   0: rainier_support::error::Error::new
             at ./crates/rainier-support/src/error.rs:107:9
   1: rainier_support::error::Error::internal<ref$<str$> >
             at ./crates/rainier-support/src/error.rs:119:9
   2: myapp::handler
             at ./src/handler.rs:7:5
";
        let frames = trim_noise(parse(raw, None));
        assert_eq!(frames[0].short_symbol(), "handler", "the first frame is the developer's");
    }

    #[test]
    fn a_frame_with_no_location_is_kept() {
        let frames = parse("   0: some::opaque::symbol\n", None);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].location(), "<no location>");
    }
}
