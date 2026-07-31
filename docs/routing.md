# Routing

Routes are declared in `src/routes/`. A route file is a **function taking a
`&mut Router`** rather than a script running against a static facade.

```rust
// src/routes/web.rs
use rainier_framework::prelude::*;

pub fn routes(router: &mut Router) {
    router.get("/", HomeController::index).name("home");
    router.get("/health", || async { "ok" }).name("health");

    router.post("/login", AuthController::login)
        .name("login")
        .middleware((kernel::web(), ThrottleRequests::per_minute(5)));
}
```

```rust
// src/bootstrap.rs
.with_routes(|router| {
    routes::web::routes(router);
    routes::api::routes(router);
})
```

## Verbs

```rust
router.get("/posts", index);
router.post("/posts", store);
router.put("/posts/{post}", update);
router.patch("/posts/{post}", update);
router.delete("/posts/{post}", destroy);
router.options("/posts", preflight);
router.any("/webhook", receive);              // every verb

router.add(vec![Method::GET, Method::POST], "/search", search);
```

`get` also answers `HEAD` — that is why `route:list` prints `GET|HEAD`.

Each returns `&mut Route`, so the modifiers chain:

```rust
router.get("/posts/{post}", show)
    .name("posts.show")
    .middleware(CacheResponse::for_seconds(60))
    .where_slug("post");
```

## Route parameters

```rust
router.get("/posts/{post}", show);           // required
router.get("/posts/{post?}", show);          // optional, trailing only
router.get("/files/{path*}", serve);         // wildcard: the rest, with slashes
```

| Pattern | Matches | Captures |
|---|---|---|
| `{name}` | exactly one segment | that segment |
| `{name?}` | one segment or nothing | the segment, if present |
| `{name*}` | everything remaining | including `/` |

Read them in the handler:

```rust
async fn show(request: Req) -> Result<Response> {
    let slug = request.route_param("post").unwrap_or_default();
    // …
}
```

or with the [`Path`](controllers.md#extractors) extractor:

```rust
async fn show(Path(slug): Path<String>) -> Result<Response> { … }
```

### Constraints

Constraints are a **closed set of named kinds** rather than regular
expressions:

```rust
router.get("/posts/{post}", show).where_slug("post");
router.get("/users/{id}", show).where_number("id");
router.get("/orders/{uuid}", show).where_uuid("uuid");
router.get("/reports/{period}", show).where_in("period", ["daily", "weekly"]);

router.get("/x/{y}", show)
    .where_param("y", ParamConstraint::Custom(Arc::new(|v| v.len() == 4)));
```

| Constraint | Allows |
|---|---|
| `Number` | ASCII digits |
| `Alpha` | ASCII letters |
| `AlphaNumeric` | ASCII letters and digits |
| `Slug` | letters, digits, `-`, `_` |
| `Uuid` | canonical 8-4-4-4-12 hex |
| `In(values)` | one of a fixed set |
| `Custom(predicate)` | anything you like |

Named kinds rather than regexes keeps the crate free of a regex dependency and,
more usefully, makes the common constraints declarative and impossible to get
subtly wrong. A route whose parameter fails its constraint simply **does not
match**, so the next route gets a chance.

## Named routes

```rust
router.get("/posts/{post}", show).name("posts.show");
```

Names are what [URL generation](urls.md) works from:

```rust
Url::instance().route("posts.show", &[("post", "hello-world")])?;   // "/posts/hello-world"
```

**Two routes with the same name is a boot error**, not a last-one-wins. URL
generation would otherwise be ambiguous and you would find out from a wrong
link in production:

```
Error: two routes are both named `posts.show` — route names must be unique
       for URL generation to be unambiguous
```

## Groups

```rust
router.group(
    GroupAttributes::new()
        .prefix("api")
        .name("api.")
        .middleware(kernel::api()),
    |router| {
        router.get("/posts", index).name("posts.index");     // GET /api/posts, "api.posts.index"

        router.group(
            GroupAttributes::new().middleware(kernel::auth("api")),
            |router| {
                router.post("/posts", store).name("posts.store");
            },
        );
    },
);
```

| Attribute | Effect |
|---|---|
| `.prefix(p)` | prepends `/p` to each URI |
| `.name(n)` | prepends `n` to each route name |
| `.middleware([…])` | prepends middleware, so outer runs first |
| `.where_param(k, c)` | applies a constraint to every route in the group |

Groups nest. Each attribute is applied **after** any inner group has applied
its own, so the outer prefix ends up outermost and the outer middleware runs
first — which is what you would expect.

## Resource routes

```rust
router.resource("posts", Arc::new(PostController));
```

The seven RESTful routes, in this order:

| Verb | URI | Action | Name |
|---|---|---|---|
| `GET` | `/posts` | `index` | `posts.index` |
| `GET` | `/posts/create` | `create` | `posts.create` |
| `POST` | `/posts` | `store` | `posts.store` |
| `GET` | `/posts/{post}` | `show` | `posts.show` |
| `GET` | `/posts/{post}/edit` | `edit` | `posts.edit` |
| `PUT\|PATCH` | `/posts/{post}` | `update` | `posts.update` |
| `DELETE` | `/posts/{post}` | `destroy` | `posts.destroy` |

The parameter name is the **singular** of the resource name, computed by
[`str::singular`](helpers.md#inflection) — `posts` → `{post}`.

```rust
router.api_resource("posts", Arc::new(PostController));   // the five, no form pages
router.resource_actions("posts", controller, &[ResourceAction::Index, ResourceAction::Show]);
```

Actions are always registered in the canonical order regardless of the order
you list them, because `/posts/create` **must** be declared before
`/posts/{post}` or the parameter swallows it.

See [Controllers](controllers.md#resource-controllers) for writing the
controller.

## Redirects and fallbacks

```rust
router.redirect("/old", "/new").name("legacy");
router.fallback(|| async { Response::html(render_404()) });
```

`redirect` produces a `302`. The fallback runs when nothing matched. Without
one, an unmatched path is a `404` with a plain message.

## Declaration order decides matches

Routes are tried **in declaration order and the first match wins**. The table
is not sorted by specificity.

```rust
router.get("/posts/create", create);      // must come first
router.get("/posts/{post}", show);        // or this swallows "create"
```

This is also why the sample project declares web routes before API routes: `/`
should be matched before any catch-all.

## Method mismatch

A path that matches but a verb that does not produces a `405` carrying an
`Allow` header listing the verbs that would have worked. That header survives
error re-rendering — see
[the lifecycle](lifecycle.md#headers-survive-re-rendering).

## Middleware on routes

```rust
router.get("/admin", dashboard)
    .middleware((kernel::auth("web"), Can::new("admin.access")));

router.get("/public", page).without_middleware::<ThrottleRequests>();
```

The middleware **itself**, not a name for it. There is no registry, nothing to
misspell, and `.middleware(kernel::web())` is a function call your editor can
follow — see [Middleware](middleware.md#why-values-and-not-names).

`without_middleware::<T>()` opts out **by type**, because that is the only
identity a value has. It drops every stage of that concrete type the group
applied.

## Inspecting the table

```sh
cargo run -- route:list
cargo run -- route:list --json
```

```
METHOD    URI                        NAME               MIDDLEWARE
GET|HEAD  /                          home
GET|HEAD  /health                    health
POST      /login                     login              AddHeaders, ThrottleRequests
GET|HEAD  /api/posts                 api.posts.index    HandleCors, ThrottleRequests
```

The middleware column shows what is **actually compiled into each route's
pipeline**, every group flattened and every deferred stage built. The container
holds the same `CompiledRouter` the kernel serves, so this describes what is
really running.

Programmatically:

```rust
let router = app.resolve::<CompiledRouter>()?;
for summary in router.describe() {
    println!("{:?} {} {:?}", summary.methods, summary.uri, summary.name);
}
```

`Router::describe()` answers the same question **without compiling**, which
matters more than it sounds. Compiling builds every middleware stage, and a
`deferred` stage that resolves a service fails if that service is not bound —
right before serving traffic, and wrong for a question as harmless as "what
routes are there?".

```rust
let mut router = Router::new();
routes::web(&mut router);

for summary in router.describe() {           // no container, nothing to fail
    println!("{} {}", summary.uri, summary.middleware.join(", "));
}
```

The middleware column is then the **declared** labels rather than the compiled
pipeline — which is what a documentation generator, a test asserting on the
table, or a `route:list` that must not depend on the container wanted anyway.

## Route model binding

Rainier does not resolve models implicitly from parameter names. Do it in the
handler, with the repository:

```rust
async fn show(request: Req) -> Result<Response> {
    let slug = request.route_param("post").unwrap_or_default();
    let posts = facade_application().resolve::<PostRepository>()?;

    let post = posts.find_by_route_key(slug).await?
        .ok_or_else(|| Error::not_found("No Post matches the given key."))?;

    Ok(Response::json(&post))
}
```

`find_by_route_key` looks up by `Model::route_key_name()`, which defaults to
the primary key and is overridden to bind by slug or UUID. `find_or_fail` gives
you the `404` in one call. See [Models](models.md#route-keys).

Implicit binding would need the router to know about the database, which is
exactly the sideways dependency [the architecture](architecture.md#the-rules-the-graph-obeys)
exists to prevent.
