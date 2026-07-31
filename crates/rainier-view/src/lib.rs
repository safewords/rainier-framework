//! # rainier-view
//!
//! The **V** in MVC: a [`View`] value a controller returns, a [`ViewEngine`]
//! port, and [`TemplateEngine`] — the file-backed template engine. The directive
//! syntax will be familiar from PHP templating engines.
//!
//! ```
//! use rainier_view::{MemoryEngine, View, ViewEngine};
//!
//! let engine = MemoryEngine::new().with(
//!     "posts.index",
//!     "<ul>@foreach(posts as post)<li>{{ post.title }}</li>@endforeach</ul>",
//! );
//!
//! let view = View::with("posts.index", serde_json::json!({
//!     "posts": [{ "title": "Hello & welcome" }],
//! })).unwrap();
//!
//! assert_eq!(
//!     engine.render_view(&view).unwrap(),
//!     "<ul><li>Hello &amp; welcome</li></ul>",
//! );
//! ```
//!
//! ## Escaped by default
//!
//! `{{ … }}` HTML-escapes; `{!! … !!}` does not. The safe form is the short
//! one, so the way you write a value without thinking is the way that cannot
//! inject a `<script>`. Emitting raw HTML is possible but has to be asked for.
//!
//! ## A deliberately small language
//!
//! `{{ }}`, `{!! !!}`, `@if`/`@elseif`/`@else`, `@foreach`, `@include`, and
//! `@extends`/`@section`/`@yield`. There is no arbitrary expression evaluation
//! and no way to call a function, because a template that can compute is a
//! template that ends up holding business logic. Prepare the data in the
//! controller; let the template lay it out.

#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

pub mod engine;
pub mod template;

pub use engine::{escape_html, MemoryEngine, TemplateEngine, View, ViewEngine};
pub use template::{CompareOp, Condition, Expr, Literal, Node, Template};
