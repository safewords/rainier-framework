# Controllers

A controller action is a **plain `async fn`**. There is no base class to
extend, and no `Controller` trait to implement.

```rust
// src/app/http/controllers/home_controller.rs
use rainier_framework::prelude::*;

pub async fn index() -> Result<Response> {
    let html = View::instance().render("home", &json!({ "title": "Hello" }))?;
    Ok(Response::html(html))
}
```

```rust
router.get("/", HomeController::index).name("home");
```

Grouping them in a module named after the controller is a convention, not a
requirement — it is what makes `home_controller::index` read like
`HomeController@index`.

## What an action may return

Anything implementing `IntoResponse`:

| Return type | Becomes |
|---|---|
| `Response` | itself |
| `&'static str`, `String` | `200 text/plain` |
| `Html<T>` | `200 text/html` |
| `Json<T>`, `serde_json::Value` | `200 application/json` |
| `Redirect` | `302`/`301`/`303`/`307` |
| `StatusCode` | that status, empty body |
| `()` | `200`, empty body |
| `Body` | `200` with that body |
| `(StatusCode, T)` | `T`'s response, with that status |
| `Option<T>` | `T`'s response, or `404` |
| `Result<T, E>` where `E: Into<Error>` | `T`'s response, or the error's |

So the shortest useful action is:

```rust
async fn health() -> &'static str { "ok" }
```

and the usual one is `Result<Response>`, because `?` then works on anything
returning [`Error`](errors.md).

## Extractors

An action's **parameters** say what it needs. Each must implement
`FromRequest`, and they run in order before the body of the action.

```rust
pub async fn store(
    request: Req,
    Validated(input): Validated<StorePostRequest>,
) -> Result<Response> {
    // `request` is the whole request; `input` is authorised, validated,
    // and contains only the fields the rules named.
}
```

| Extractor | Yields | Fails with |
|---|---|---|
| `Req` (`Arc<Request>`) | the whole request | never |
| `Json<T>` | `T` from a JSON body | `400` |
| `Form<T>` | `T` from a urlencoded body | `400` |
| `Query<T>` | `T` from the query string | `400` |
| `Path<T>` | `T` from route parameters | `400` |
| `Header<K>` | one header's value | `400` if absent |
| `Bearer` | the `Authorization: Bearer` token | `401` |
| `RawBody` | the raw `Bytes` | never |
| `Files<K>` | uploads for a field | never (empty vec) |
| [`Validated<T>`](validation.md#request-contracts) | a validated contract | `403` / `422` |
| `Option<T>` | `Some`/`None` instead of failing | never |
| `Result<T>` | the failure, for you to handle | never |

Up to a reasonable arity, in any order:

```rust
async fn update(
    Path(id): Path<u64>,
    Bearer(token): Bearer,
    Json(body): Json<UpdatePost>,
) -> Result<Response> { … }
```

### `Header<K>` and `Files<K>` need a name

Rust cannot yet take a `&'static str` as a const generic parameter, so the name
arrives as a **marker type**:

```rust
use rainier_framework::http::extract::Header;
use rainier_framework::http::static_name;

static_name!(XTenant, "x-tenant");

async fn show(Header(tenant, ..): Header<XTenant>) -> Result<Response> { … }
```

`static_name!` writes the marker type and its `StaticName` impl. The same
marker works for `Files<K>`.

### Failing softly

Wrap in `Option` or `Result` when a missing value is not an error:

```rust
async fn index(page: Option<Query<Page>>) -> Result<Response> {
    let page = page.map(|Query(p)| p).unwrap_or_default();
    …
}
```

## Getting at services

Handler parameters are for **request-derived** values. Services come from the
[container](container.md):

```rust
pub(crate) fn resolve<T: Send + Sync + 'static>() -> Result<Arc<T>> {
    rainier_framework::container::facade_application().resolve::<T>()
}

pub async fn index() -> Result<Response> {
    let posts = resolve::<PostRepository>()?;
    Ok(Response::json(&posts.all().await?))
}
```

That three-line helper at the bottom of your controllers module is the sample
project's approach, and it is the one to copy. Injecting services as handler
parameters would mean the router knowing how to resolve them, which is a
dependency [the architecture](architecture.md#the-rules-the-graph-obeys)
deliberately does not have.

## Reading the authenticated user

The [`auth` middleware](authentication.md#the-middleware) puts an
`AuthenticatedUser<U>` in the request's extensions:

```rust
pub(crate) fn current_user(request: &Request) -> Result<User> {
    request
        .extension::<AuthenticatedUser<User>>()
        .map(|user| user.get().clone())
        .ok_or_else(|| Error::unauthenticated("Unauthenticated."))
}
```

A `401` rather than a `.expect()`: moving the route out from behind `auth`
should be a wrong answer, not a crash.

## A realistic action

From the sample project — note what it does *not* do:

```rust
/// `POST /api/posts/{post}/publish` — behind `auth:api`.
pub async fn publish(request: Req) -> Result<Response> {
    let author = current_user(&request)?;
    let slug = route_param(&request, "post")?;
    let posts = resolve::<PostRepository>()?;

    let mut post = posts.first_by("slug", slug.into()).await?
        .ok_or_else(|| Error::not_found("No Post matches the given key."))?;

    // Authorisation is a policy, not an `if` buried in the controller.
    PostPolicy::gate().authorize("posts.publish", &author, Some(&post))?;

    if post.published {
        // Publishing twice must not send a second notification.
        return Ok(Response::json(&post));
    }

    post.published = true;
    posts.update(&post).await?;

    Event::instance().dispatch(PostPublished { post: post.clone() }).await?;
    Queue::instance().dispatch(NotifyAuthor { post_id: post.id }).await?;

    Ok(Response::json(&post))
}
```

It does not send mail. It writes the row, raises an [event](events.md) and
queues a [job](queues.md), and the response goes out immediately. Whether the
author's notification succeeds is the worker's problem, not this request's.

## Resource controllers

For the seven RESTful actions, implement `ResourceController` and register it
in one line:

```rust
use rainier_framework::prelude::*;

pub struct PostController;

#[async_trait]
impl ResourceController for PostController {
    async fn index(&self, _request: Request) -> Response {
        Response::text("every post")
    }

    async fn show(&self, request: Request) -> Response {
        Response::text(format!("post {}", request.route_param("post").unwrap_or("?")))
    }
}
```

```rust
router.resource("posts", Arc::new(PostController));
```

Every action has a default that answers `405`, so you implement only the ones
the resource supports. See [Routing](routing.md#resource-routes) for the table
of URIs and names.

Resource actions take `Request` by value and return `Response` — no extractors
and no `Result`, because the trait has to be object-safe. For anything richer,
use plain functions and declare the routes yourself.

### Middleware on a controller

Middleware scoped to some of a controller's actions, with the actions as an
enum and the middleware as a value:

```rust
impl ResourceController for PostController {
    async fn index(&self, request: Request) -> Response { … }
    async fn store(&self, request: Request) -> Response { … }
    async fn destroy(&self, request: Request) -> Response { … }

    fn middleware(&self) -> ControllerMiddleware {
        ControllerMiddleware::new()
            .except(
                [ResourceAction::Index, ResourceAction::Show],
                Authenticate::<User>::resolved(),
            )
            .only([ResourceAction::Destroy], RequireRole::Admin)
    }
}
```

| Method | Applies to |
|---|---|
| `always(m)` | every action |
| `only([actions], m)` | just those |
| `except([actions], m)` | everything else |

**Prefer `except`.** An action added next year arrives guarded; an `only` list
leaves it public until somebody remembers to add it. That is the whole
difference between the two, and it is worth the habit.

Declaring the guard here rather than in the route file puts the rule next to
the code it protects — the person adding `destroy` reads it in the same file
instead of having to know that a route file three directories away is what
stops it being public.

Controller middleware runs **inside** the route's group and outside the action,
so a group's session still wraps the controller's authorisation check.

## Testing an action

Because an action is a function, you can call it:

```rust
#[tokio::test]
async fn health_is_ok() {
    assert_eq!(health().await, "ok");
}
```

For anything touching the container or middleware, drive the real kernel
instead — see [Testing](testing.md#feature-tests).
