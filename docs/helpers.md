# Helpers

`rainier-support` is the crate everything else depends on and that depends on
nothing. It holds four things: string inflection, a typed map, the error type,
and future aliases.

```rust
use rainier_framework::support::str;
```

## Inflection

String inflection, as free functions.

```rust
str::camel("user_profile");        // "userProfile"
str::studly("user-profile");       // "UserProfile"
str::snake("UserProfile");         // "user_profile"
str::kebab("UserProfile");         // "user-profile"

str::plural("post");               // "posts"
str::singular("posts");            // "post"

str::slug("Hello, World!");        // "hello-world"
str::slug_with("Hello, World!", '_');

str::ucfirst("hello");             // "Hello"
str::humanize("user_profile");     // "User profile"
str::class_basename("app::models::Post");   // "Post"
str::limit("a long sentence", 6, "…");      // "a long…"
```

These are **load-bearing, not cosmetic**. [Resource
routing](routing.md#resource-routes) derives `{post}` from `posts`,
[models](models.md) derive a table name from a struct name, and
`Model::model_name` uses `class_basename` for the message in a `404`.

### Pluralisation is idempotent

```rust
str::plural("posts") == "posts"        // not "postses"
str::singular("post") == "post"
```

Resource routing feeds these names that may **already** be plural, and a
`postses` table would be a silent, annoying bug that you find in production.

The pluraliser is a naive English one covering the endings that show up in
table and resource names, plus a table of irregulars. It is wrong for genuinely
irregular nouns outside that table — **name the resource explicitly when it
matters**:

```rust
router.resource("people", Arc::new(PersonController));   // don't rely on inflection
```

## `Extensions`

A typed map keyed by `TypeId` — the same idea as the
[container](container.md), scoped to one value:

```rust
use rainier_framework::support::Extensions;

let mut ext = Extensions::new();
ext.insert(RequestId("abc".into()));

ext.get::<RequestId>();                        // Option<&T>
ext.get_mut::<RequestId>();
ext.remove::<RequestId>();
ext.contains::<RequestId>();
ext.get_or_insert_with(|| Expensive::new());
ext.len();
ext.clear();
```

This is what backs [request extensions](requests.md#extensions) and response
extensions. Reach for it directly when you are building something that needs to
carry arbitrary typed baggage.

## `Error` and `Result`

```rust
use rainier_framework::prelude::*;   // Error, ErrorKind, Result, Context

pub type Result<T, E = Error> = std::result::Result<T, E>;
```

One error type across the whole framework. Fully covered in
[Error Handling](errors.md).

`Context` adds `anyhow`-style context to both `Result` and `Option`:

```rust
let config = std::fs::read_to_string(path).context("reading the index config")?;
let user = maybe_user.context("the session referenced a missing user")?;
```

## Future aliases

```rust
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
pub type BoxedFuture<T> = BoxFuture<'static, T>;
pub type LocalBoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

pub fn boxed<'a, F>(future: F) -> BoxFuture<'a, F::Output>;
```

These appear in every object-safe trait in the framework — `Guard`, `Queue`,
`Transport`, `ServiceProvider`, `Connection` — because `async fn` in trait is
not object-safe on stable.

**`BoxFuture` is `Send`; `LocalBoxFuture` is not.** That distinction is the
whole [`Send` story](database.md#the-send-story) in two type aliases. When you
are writing a trait the framework will store behind `dyn`, `BoxFuture` is
almost always what you want — using `LocalBoxFuture` makes the trait unusable
from a multi-threaded server, and the compiler will tell you about it a long
way from the cause.

## `build_info!()`

Which build is actually running — the first question of every incident, and the
usual answer is somebody reading a deploy pipeline backwards.

```rust
router.get("/health/version", || async { Response::json(&build_info!()) });
```

```json
{ "name": "identity", "version": "2.4.1", "commit": "9f3c2ab…", "profile": "release" }
```

The macro expands **in your crate**, so the name and version are your
package's, not Rainier's. The commit and build time come from the environment
at compile time:

| Field | Read from |
|---|---|
| `commit` | `GIT_SHA`, `GITHUB_SHA`, or `VERGEN_GIT_SHA` |
| `built_at` | `BUILD_TIMESTAMP` or `SOURCE_DATE_EPOCH` |

`GITHUB_SHA` is set by GitHub Actions already, so a CI build gets its commit
with nothing added. Anywhere else it is one line:

```dockerfile
ARG GIT_SHA
ENV GIT_SHA=$GIT_SHA
RUN cargo build --release
```

A local `cargo run` has neither and reports `None` — absent from the JSON
rather than `null`, because "this build was not told" is what actually
happened, and a commit inferred from a dirty working tree is worse than no
commit.

```rust
let info = build_info!();

info.short_commit();      // Option<&str> — the first seven characters
info.is_debug();          // worth asserting at boot in production
info.summary();           // "identity 2.4.1 (9f3c2ab, release)" — for a log line
```

## What is not here

PHP frameworks ship large string, array and collection helper libraries
because PHP's standard library is small. Rust's is not.

- `Arr::*` → slice and `Vec` methods
- `Collection` → iterators, which are lazier and faster
- `Str::contains`, `startsWith`, `replace` → `str` methods
- `optional()`, `data_get()` → `Option` and `?`

The helpers that survive are the ones the standard library genuinely does not
have: **inflection**, which is English-specific, and a **type-keyed map**, which
needs `TypeId`.
