# AGENTS.md — rainier-framework

Rainier is an MVC framework for Rust: a workspace of crates under `crates/`,
the documentation under `docs/`, and the changelog. This file is what an agent
working here needs before it changes anything.

## The documentation is the website

`docs/` is the **only** copy of Rainier's documentation. rainierframework.io
does not keep its own: it builds its docs section from this directory **as it
is on `main`**, checking for a new revision every half hour, so a change to a
doc merged here is live on the site within about that long, with nothing to
copy or sync by hand. Nothing in the website's repository is edited to change
what a doc says.

What that means when you change something:

- **A new feature is not finished until `docs/` says how to use it.** The doc
  for its area — `docs/lookout.md`, `docs/database.md`, … — in the same commit
  as the code, and a line in `CHANGELOG.md` under `[Unreleased]`.
- **A new page** is a new `docs/<slug>.md` **and** an entry in
  `docs/_sidebar.json`, the site's table of contents: a group and its items,
  each a slug (or `{ "slug", "title" }`). `README.md` is the slug `overview`.
  `crates/rainier-framework/tests/docs_sidebar.rs` fails when a page is
  missing from the sidebar or the sidebar names a page that does not exist,
  so the mistake is caught here rather than as a failed site build.
- **Link between docs with relative `.md` links** (`[routing](routing.md#groups)`)
  — they work on GitHub, and the site rewrites them to its own URLs and
  checks every link and `#fragment` on each build.
- **Diagrams are Mermaid** fenced as ` ```mermaid `. GitHub renders them and
  so does the site.
- **Examples that name a public API** belong in
  `crates/rainier-framework/tests/documented_surface.rs` too, which fails to
  compile when a doc's example stops matching the code.

## This repository is public

Write nothing about the private applications built on Rainier — their names,
hosts, routes, tables or incidents — into code, tests, docs, examples or commit
messages. Describe a problem in the framework's own terms (`posts`, `users`,
`app.example.com`). Fake secrets in tests and docs must not look like a real
provider's key (no `sk_live_…`): the push is refused by secret scanning.

## Before you commit

`.githooks/pre-commit` runs `cargo fmt --check` and
`cargo clippy --workspace --all-targets --all-features -D warnings`; enable it
with `git config core.hooksPath .githooks`. Run the tests of the crates you
touched (`cargo test -p <crate> --all-targets`, and `--doc`), and keep the
Lookout dashboard's built assets (`crates/rainier-lookout/assets/dist`)
rebuilt (`npm run build` in `crates/rainier-lookout/frontend`) whenever its
`frontend/src` changes — they are committed.

`main` is what applications pin by revision and what the website shows.
