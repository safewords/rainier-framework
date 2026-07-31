# Vite

Frontend assets, the way a PHP framework does them: `resources/js` and
`resources/css` are
source, Vite compiles them into `public/build`, and a template asks for an
**entry** with a directive rather than hard-coding a filename that changes on
every build.

```html
<head>
    @vite(['resources/css/app.css', 'resources/js/app.js'])
</head>
```

Nothing here is mandatory. An application that renders plain templates never
touches any of this — the opt-in is *using the directive*, not enabling a
feature.

## The two modes

`@vite` renders differently depending on which artefact exists under
`public/`:

| Artefact | Written by | What renders |
|---|---|---|
| `public/hot` | the dev server, while `npm run dev` runs | the Vite client plus every entry served from the dev server — edits hot-reload, nothing is compiled to disk |
| `public/build/manifest.json` | `npm run build` | each entry's content-hashed file, plus the stylesheets it imports, under `/build/…` |

The hot file wins when both exist, and it is checked on every render — its
whole point is appearing and disappearing while the process runs. The
manifest is parsed once and cached; a deploy that replaces it replaces the
process too.

Missing both is never a 500: the directive renders an HTML comment
naming the two commands, and the page arrives unstyled instead of down. A
fresh `git clone && cargo run` works before npm has ever run — an application
without a frontend build has simply not opted in. Misconfiguration against a
build that *does* exist — an entry the manifest does not name, a manifest
that does not parse — is a hard error.

## Setup

The framework's default bootstrap attaches a resolver over `<base>/public`
already. An application that builds its own view engine attaches one itself:

```rust
use rainier_framework::view::{TemplateEngine, Vite};

TemplateEngine::new("resources/views").with_vite(Vite::new("public"))
```

`Vite::new` takes the web root; `with_build_dir` moves the `build` directory
(and the URL prefix) if yours differs.

On the JavaScript side, `vite.config.js` needs three things: the entries, a
manifest, and something to write `public/hot` while the dev server runs. The
sample project carries the canonical config — a ~20-line inline plugin, no
extra npm dependency.

## Serving the build

Rainier is the web server, so the built files need a route. The sample
project's `asset_controller` is the pattern: a `{path*}` wildcard route that
resolves strictly inside `public/build`, sets the content type by extension,
and marks responses `immutable` — safe because Vite content-hashes every
filename, so a given URL's bytes never change.

## What is deliberately absent

No SSR entries, no asset helper for images, no dev-server proxying. Those can
arrive when something needs them; entries-to-tags is the part every page
needs on day one.
