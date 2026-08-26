//! The page.
//!
//! One self-contained HTML document: no external stylesheet, no font, no
//! script from a CDN. An error page that needs the network is an error page
//! that does not work when the network is what broke — and this framework's
//! local environment is built to run on a plane.
//!
//! Laid out the way Whoops is, because the layout is the good idea: the stack
//! down the left, the selected frame's source on the right, the request
//! underneath. What differs is that the frames are filtered by origin, because
//! a Rust backtrace through an async runtime is sixty frames of which four are
//! yours.

use std::fmt::Write as _;

use crate::frames::{Excerpt, Frame, Origin};

/// Escape text for HTML.
///
/// A local copy for the same reason `rainier-server` has one: the error page
/// must render in an application with no view layer, and it must not fail.
pub fn escape(value: &str) -> String {
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

/// A named group of key/value rows, rendered as one panel.
pub struct Panel {
    /// The heading.
    pub title: String,
    /// The rows, already redacted.
    pub rows: Vec<(String, String)>,
}

/// Everything the page needs.
pub struct Page<'a> {
    /// The status being rendered.
    pub status: u16,
    /// The status's canonical reason phrase.
    pub reason: &'a str,
    /// The error message.
    pub message: &'a str,
    /// The error's kind, as a short label.
    pub kind: &'a str,
    /// The resolved frames, innermost first.
    pub frames: &'a [Frame],
    /// A source excerpt per frame, by index.
    pub excerpts: &'a [Option<Excerpt>],
    /// The raw backtrace, shown when no frame could be parsed.
    pub raw_backtrace: Option<&'a str>,
    /// The request context panels.
    pub panels: Vec<Panel>,
    /// The editor URL template, e.g. `phpstorm://open?file={file}&line={line}`.
    pub editor: Option<&'a str>,
}

impl Page<'_> {
    /// Render the whole document.
    pub fn render(&self) -> String {
        let mut out = String::with_capacity(16 * 1024);

        let _ = write!(
            out,
            "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
             <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
             <meta name=\"robots\" content=\"noindex,nofollow\">\
             <title>{} {} — {}</title><style>{}</style></head><body>",
            self.status,
            escape(self.reason),
            escape(&truncate_title(self.message)),
            STYLE
        );

        self.header(&mut out);

        out.push_str("<main>");
        self.stack(&mut out);
        self.source(&mut out);
        out.push_str("</main>");

        self.context(&mut out);

        let _ = write!(out, "<script>{SCRIPT}</script></body></html>");
        out
    }

    fn header(&self, out: &mut String) {
        let _ = write!(
            out,
            "<header><div class=\"status\"><span class=\"code\">{}</span>\
             <span class=\"reason\">{}</span><span class=\"kind\">{}</span></div>\
             <h1>{}</h1></header>",
            self.status,
            escape(self.reason),
            escape(self.kind),
            escape(self.message),
        );
    }

    fn stack(&self, out: &mut String) {
        out.push_str("<nav class=\"frames\">");

        if self.frames.is_empty() {
            out.push_str(
                "<p class=\"empty\">No stack was captured.<br><br>\
                 Set <code>RUST_BACKTRACE=1</code> and reproduce this to get one. \
                 It is read once per process, so the application has to restart.</p>",
            );
            out.push_str("</nav>");
            return;
        }

        // The filter. Application frames are the default because they are
        // almost always the answer; the others are one click away rather than
        // gone, because "almost always" is not "always".
        let app_count = self.frames.iter().filter(|f| f.origin == Origin::Application).count();
        let _ = write!(
            out,
            "<div class=\"filter\">\
             <button data-filter=\"app\" class=\"on\">app <span>{}</span></button>\
             <button data-filter=\"framework\">framework</button>\
             <button data-filter=\"vendor\">vendor</button>\
             <button data-filter=\"all\">all <span>{}</span></button></div><ol>",
            app_count,
            self.frames.len(),
        );

        for (index, frame) in self.frames.iter().enumerate() {
            let has_source = self.excerpts.get(index).and_then(Option::as_ref).is_some();
            let _ = write!(
                out,
                "<li class=\"frame {origin}{selected}{nosrc}\" data-frame=\"{index}\" \
                 data-origin=\"{origin}\" tabindex=\"0\">\
                 <span class=\"idx\">{index}</span>\
                 <span class=\"sym\">{sym}</span>\
                 <span class=\"file\">{file}{line}</span></li>",
                origin = frame.origin.as_str(),
                selected = if index == self.default_frame() { " selected" } else { "" },
                nosrc = if has_source { "" } else { " nosource" },
                index = index,
                sym = escape(&frame.short_symbol()),
                file = escape(&frame.short_file()),
                line = frame.line.map(|l| format!(":{l}")).unwrap_or_default(),
            );
        }

        out.push_str("</ol></nav>");
    }

    /// The frame the page opens on: the first application frame, or the first
    /// frame at all. Opening on `std::backtrace::capture` would be useless.
    fn default_frame(&self) -> usize {
        self.frames.iter().position(|f| f.origin == Origin::Application).unwrap_or(0)
    }

    fn source(&self, out: &mut String) {
        out.push_str("<section class=\"source\">");

        if self.frames.is_empty() {
            if let Some(raw) = self.raw_backtrace {
                out.push_str(
                    "<div class=\"panel-note\">The backtrace could not be parsed into \
                              frames, so here it is verbatim.</div>",
                );
                let _ = write!(out, "<pre class=\"raw\">{}</pre>", escape(raw));
            }
            out.push_str("</section>");
            return;
        }

        for (index, frame) in self.frames.iter().enumerate() {
            let shown = if index == self.default_frame() { "" } else { " hidden" };
            let _ = write!(out, "<div class=\"pane{shown}\" data-pane=\"{index}\">");

            let _ = write!(
                out,
                "<div class=\"pane-head\"><code>{}</code>{}</div>",
                escape(&frame.symbol),
                self.editor_link(frame),
            );

            match self.excerpts.get(index).and_then(Option::as_ref) {
                Some(excerpt) => write_excerpt(out, excerpt),
                None => {
                    let _ = write!(
                        out,
                        "<p class=\"panel-note\">No source for <code>{}</code>.<br><br>\
                         Either it was compiled without debug info, or the path in the debug \
                         info does not exist on this machine — which is the normal case in a \
                         container, where the paths are the build machine's.</p>",
                        escape(&frame.location()),
                    );
                }
            }
            out.push_str("</div>");
        }

        out.push_str("</section>");
    }

    fn editor_link(&self, frame: &Frame) -> String {
        let (Some(editor), Some(file)) = (self.editor, &frame.file) else {
            return String::new();
        };
        let line = frame.line.unwrap_or(1);
        // Absolute where possible: an editor cannot resolve `./src/main.rs`.
        let absolute = std::fs::canonicalize(file).unwrap_or_else(|_| file.clone());
        let path = absolute.to_string_lossy().trim_start_matches(r"\\?\").to_string();
        let href = editor.replace("{file}", &path).replace("{line}", &line.to_string());
        format!("<a class=\"open\" href=\"{}\">open in editor</a>", escape(&href))
    }

    fn context(&self, out: &mut String) {
        if self.panels.is_empty() {
            return;
        }
        out.push_str("<section class=\"context\">");
        for panel in &self.panels {
            let _ = write!(out, "<div class=\"panel\"><h2>{}</h2>", escape(&panel.title));
            if panel.rows.is_empty() {
                out.push_str("<p class=\"empty\">empty</p>");
            } else {
                out.push_str("<table>");
                for (key, value) in &panel.rows {
                    let redacted = value.starts_with("[redacted");
                    let _ = write!(
                        out,
                        "<tr><th>{}</th><td{}>{}</td></tr>",
                        escape(key),
                        if redacted { " class=\"redacted\"" } else { "" },
                        escape(value),
                    );
                }
                out.push_str("</table>");
            }
            out.push_str("</div>");
        }
        out.push_str("</section>");
    }
}

fn write_excerpt(out: &mut String, excerpt: &Excerpt) {
    out.push_str("<table class=\"code\">");
    for (offset, text) in excerpt.lines.iter().enumerate() {
        let number = excerpt.first_line + offset as u32;
        let hit = number == excerpt.highlight;
        let _ = write!(
            out,
            "<tr{}><td class=\"ln\">{}</td><td class=\"src\">{}</td></tr>",
            if hit { " class=\"hit\"" } else { "" },
            number,
            escape(text),
        );
    }
    out.push_str("</table>");
}

/// Keep the `<title>` a sensible length — it becomes a browser tab.
fn truncate_title(message: &str) -> String {
    const MAX: usize = 60;
    if message.chars().count() <= MAX {
        return message.to_string();
    }
    let short: String = message.chars().take(MAX).collect();
    format!("{short}…")
}

/// The stylesheet. Dark by default, light when the OS asks — an error page is
/// read at both two in the afternoon and two in the morning.
const STYLE: &str = r#"
*{box-sizing:border-box}
:root{
  --bg:#16161a; --panel:#1e1e24; --edge:#2e2e38; --ink:#e6e6ea; --dim:#9a9aa8;
  --accent:#ff6b6b; --app:#8ce99a; --fw:#74c0fc; --vendor:#6b6b7b; --hit:#3a2a2a;
  --mono:ui-monospace,SFMono-Regular,"SF Mono",Menlo,Consolas,monospace;
}
@media (prefers-color-scheme: light){
  :root{--bg:#f6f6f8;--panel:#fff;--edge:#e2e2e8;--ink:#1a1a1f;--dim:#6b6b7b;--hit:#ffe9e9}
}
body{margin:0;background:var(--bg);color:var(--ink);
  font:14px/1.5 system-ui,-apple-system,"Segoe UI",sans-serif}
header{padding:24px 28px;border-bottom:1px solid var(--edge);background:var(--panel)}
.status{display:flex;gap:10px;align-items:center;margin-bottom:8px}
.code{font:700 13px/1 var(--mono);background:var(--accent);color:#fff;padding:5px 8px;border-radius:4px}
.reason,.kind{color:var(--dim);font-size:12px;text-transform:uppercase;letter-spacing:.06em}
.kind{border:1px solid var(--edge);padding:4px 7px;border-radius:4px}
h1{margin:0;font-size:20px;font-weight:600;font-family:var(--mono);word-break:break-word}
main{display:grid;grid-template-columns:minmax(280px,340px) 1fr;min-height:52vh}
@media (max-width:860px){main{grid-template-columns:1fr}}
.frames{border-right:1px solid var(--edge);background:var(--panel);overflow:auto;max-height:70vh}
.filter{display:flex;gap:4px;padding:10px;border-bottom:1px solid var(--edge);flex-wrap:wrap}
.filter button{font:inherit;font-size:11px;color:var(--dim);background:transparent;
  border:1px solid var(--edge);border-radius:4px;padding:4px 8px;cursor:pointer}
.filter button.on{color:var(--ink);border-color:var(--dim)}
.filter span{opacity:.6;margin-left:4px}
.frames ol{list-style:none;margin:0;padding:0}
.frame{display:grid;grid-template-columns:28px 1fr;gap:2px 8px;padding:9px 12px;
  border-bottom:1px solid var(--edge);cursor:pointer;border-left:3px solid transparent}
.frame:hover{background:rgba(127,127,150,.10)}
.frame.selected{background:rgba(127,127,150,.16);border-left-color:var(--accent)}
.frame:focus-visible{outline:2px solid var(--fw);outline-offset:-2px}
.idx{grid-row:span 2;color:var(--dim);font:11px/1.6 var(--mono);text-align:right}
.sym{font:600 13px/1.3 var(--mono);word-break:break-all}
.file{color:var(--dim);font:11px/1.4 var(--mono);word-break:break-all}
.frame.app .sym{color:var(--app)}
.frame.framework .sym{color:var(--fw)}
.frame.vendor .sym{color:var(--vendor)}
.frame.nosource .file::after{content:" · no source";opacity:.7}
.source{overflow:auto;max-height:70vh}
.pane.hidden{display:none}
.pane-head{display:flex;justify-content:space-between;align-items:center;gap:12px;
  padding:10px 16px;border-bottom:1px solid var(--edge);position:sticky;top:0;background:var(--bg)}
.pane-head code{font:12px/1.4 var(--mono);color:var(--dim);word-break:break-all}
.open{color:var(--fw);font-size:12px;text-decoration:none;white-space:nowrap}
.open:hover{text-decoration:underline}
table.code{border-collapse:collapse;width:100%;font:12px/1.65 var(--mono)}
table.code td{padding:0 10px;white-space:pre;vertical-align:top}
td.ln{color:var(--dim);text-align:right;user-select:none;width:1%;border-right:1px solid var(--edge)}
tr.hit{background:var(--hit)}
tr.hit td.ln{color:var(--accent);font-weight:700}
.panel-note{padding:16px;color:var(--dim);max-width:60ch}
pre.raw{margin:0;padding:16px;font:12px/1.6 var(--mono);white-space:pre-wrap;word-break:break-all}
.context{display:grid;grid-template-columns:repeat(auto-fit,minmax(320px,1fr));
  gap:1px;background:var(--edge);border-top:1px solid var(--edge)}
.panel{background:var(--panel);padding:16px 18px}
.panel h2{margin:0 0 10px;font-size:11px;text-transform:uppercase;letter-spacing:.08em;color:var(--dim)}
.panel table{border-collapse:collapse;width:100%;font:12px/1.6 var(--mono)}
.panel th{text-align:left;color:var(--dim);font-weight:400;padding:3px 10px 3px 0;
  vertical-align:top;width:1%;white-space:nowrap}
.panel td{padding:3px 0;word-break:break-all}
.panel td.redacted{color:var(--accent);opacity:.75;font-style:italic}
.empty{color:var(--dim);font-size:12px;margin:0}
"#;

/// Frame switching and filtering. No framework, no build step, no CDN.
const SCRIPT: &str = r#"
(function(){
  var frames=[].slice.call(document.querySelectorAll('.frame'));
  var panes=[].slice.call(document.querySelectorAll('.pane'));
  function select(i){
    frames.forEach(function(f){f.classList.toggle('selected',f.dataset.frame===String(i))});
    panes.forEach(function(p){p.classList.toggle('hidden',p.dataset.pane!==String(i))});
  }
  frames.forEach(function(f){
    f.addEventListener('click',function(){select(f.dataset.frame)});
    f.addEventListener('keydown',function(e){
      if(e.key==='Enter'||e.key===' '){e.preventDefault();select(f.dataset.frame)}
    });
  });
  var buttons=[].slice.call(document.querySelectorAll('.filter button'));
  function filter(kind){
    buttons.forEach(function(b){b.classList.toggle('on',b.dataset.filter===kind)});
    frames.forEach(function(f){
      f.style.display=(kind==='all'||f.dataset.origin===kind)?'':'none';
    });
    // Never filter down to nothing: an empty list looks like a broken page.
    if(!frames.some(function(f){return f.style.display!=='none'})){
      buttons.forEach(function(b){b.classList.toggle('on',b.dataset.filter==='all')});
      frames.forEach(function(f){f.style.display=''});
    }
  }
  buttons.forEach(function(b){
    b.addEventListener('click',function(){filter(b.dataset.filter)});
  });
  filter('app');
  // j/k, because this is a stack trace and the audience has opinions.
  document.addEventListener('keydown',function(e){
    if(e.target!==document.body)return;
    var visible=frames.filter(function(f){return f.style.display!=='none'});
    var at=visible.findIndex(function(f){return f.classList.contains('selected')});
    if(e.key==='j'&&at<visible.length-1)select(visible[at+1].dataset.frame);
    if(e.key==='k'&&at>0)select(visible[at-1].dataset.frame);
  });
})();
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_markup_in_the_message() {
        let out = escape("<script>alert('x')</script>");
        assert!(!out.contains("<script>"));
        assert!(out.contains("&lt;script&gt;"));
    }

    #[test]
    fn a_page_with_no_frames_says_how_to_get_one() {
        let page = Page {
            status: 500,
            reason: "Internal Server Error",
            message: "boom",
            kind: "Internal",
            frames: &[],
            excerpts: &[],
            raw_backtrace: None,
            panels: vec![],
            editor: None,
        };
        let html = page.render();
        assert!(html.contains("RUST_BACKTRACE=1"));
    }

    #[test]
    fn the_title_is_truncated_but_the_heading_is_not() {
        let long = "x".repeat(200);
        let page = Page {
            status: 500,
            reason: "Internal Server Error",
            message: &long,
            kind: "Internal",
            frames: &[],
            excerpts: &[],
            raw_backtrace: None,
            panels: vec![],
            editor: None,
        };
        let html = page.render();
        assert!(html.contains(&format!("<h1>{long}</h1>")), "the heading keeps the whole message");
        assert!(html.contains("…"), "the tab title is shortened");
    }

    #[test]
    fn a_redacted_row_is_marked_up_as_such() {
        let page = Page {
            status: 500,
            reason: "Internal Server Error",
            message: "boom",
            kind: "Internal",
            frames: &[],
            excerpts: &[],
            raw_backtrace: None,
            panels: vec![Panel {
                title: "Headers".into(),
                rows: vec![("authorization".into(), "[redacted by rainier-debug]".into())],
            }],
            editor: None,
        };
        let html = page.render();
        assert!(html.contains("class=\"redacted\""));
    }

    #[test]
    fn the_page_references_nothing_off_host() {
        let page = Page {
            status: 500,
            reason: "Internal Server Error",
            message: "boom",
            kind: "Internal",
            frames: &[],
            excerpts: &[],
            raw_backtrace: None,
            panels: vec![],
            editor: None,
        };
        let html = page.render();
        // The whole point of inlining the CSS and the script: this page has to
        // render with the network down, which is when it is most needed.
        assert!(!html.contains("http://"), "no off-host reference");
        assert!(!html.contains("https://"), "no off-host reference");
    }
}
