# Lookout dashboard (frontend)

The Lookout dashboard is a Vue 3 + Tailwind CSS v4 app, built with Vite. The
build is committed to `../assets/dist/` (`lookout.js`, `lookout.css` — stable
names, no hashes), and the crate compiles those two files in with
`include_bytes!` (`src/http/shell.rs`). Building the Rust crate never needs Node.

## Rebuilding

After changing anything under `src/`:

```sh
cd crates/rainier-lookout/frontend
npm ci          # exact versions from package-lock.json
npm run build   # writes ../assets/dist/lookout.js and lookout.css
```

Commit `assets/dist/` together with the source change. `cargo test -p
rainier-lookout` fails (`the_built_frontend_is_committed`) if the build is
missing or empty.

`npm run dev` rebuilds on every save; reload the dashboard to pick it up.

## How it is served

`rainier_lookout::routes` serves a minimal HTML shell at the dashboard path
(`/_lookout` by default) and for every client-side route under it, plus
`/assets/lookout.js` and `/assets/lookout.css`. The shell passes the base path
and API base to the app in a `<script type="application/json"
id="lookout-config">` element (`src/config.js` reads it).

The dashboard's Content-Security-Policy allows nothing inline and nothing from
another origin (`script-src 'self'; style-src 'self'; connect-src 'self'`), so:

- no `v-html` on API data, and no `<style>` / `style="…"` in templates —
  Tailwind classes, plus Vue `:style` object bindings (applied through the
  CSSOM, which the policy allows) for computed widths such as the waterfall
  bars;
- Vue's runtime-only build (templates are compiled by Vite), so no `eval`;
- no CDN, web font or external image.

## Layout

- `src/main.js` — router (history mode under the configured base)
- `src/App.vue` — header, collapsible sidebar, Live toggle, Clear
- `src/views/` — Dashboard (aggregate cards), Batches, BatchDetail
  (waterfall, entry content, related batches in the same trace), Entries
  (per type), Status
- `src/components/` — shared pieces
- `src/api.js` — the JSON API client; `DELETE` sends `X-Lookout: 1`
