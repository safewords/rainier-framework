# Responses

A handler can return a `Response`, or anything that becomes one.

```rust
async fn index() -> Response {
    Response::json(&posts)
}

async fn health() -> &'static str {
    "ok"
}
```

## Constructors

| Constructor | Status | Content-Type |
|---|---|---|
| `Response::ok(body)` | 200 | none set |
| `Response::text(s)` | 200 | `text/plain; charset=utf-8` |
| `Response::html(s)` | 200 | `text/html; charset=utf-8` |
| `Response::json(&v)` | 200 | `application/json` |
| `Response::no_content()` | 204 | — |
| `Response::created(location)` | 201 | with `Location` |
| `Response::download(bytes, name)` | 200 | with `Content-Disposition: attachment` |
| `Response::stream(s)` | 200 | streamed body |
| `Response::new(status)` | yours | — |

```rust
Response::json(&post).with_status(StatusCode::CREATED)
Response::created("/posts/hello")
Response::download(pdf_bytes, "invoice.pdf")
```

## Modifiers

Every modifier consumes and returns the response, so they chain:

```rust
Response::json(&post)
    .with_status(StatusCode::CREATED)
    .with_header("x-request-id", &id)
    .with_added_header("vary", "accept")
    .with_cookie(&Cookie::new("seen", "1"))
```

| Method | |
|---|---|
| `.with_status(s)` | |
| `.with_body(b)` | |
| `.with_header(n, v)` | replaces |
| `.with_added_header(n, v)` | appends — for `Set-Cookie`, `Vary`, `Link` |
| `.with_content_type(t)` | |
| `.with_cookie(&c)` | |
| `.without_cookie(name)` | sends a removal cookie |

And to read one back — which is most of what a test does:

```rust
response.status()
response.header("location")
response.headers()
response.body()
response.is_successful()          // 2xx
response.extensions()

response.into_bytes().await?      // consumes it — a stream can only be read once
response.into_string().await?
response.into_json::<PostView>().await?
```

`into_json` quotes the start of the body in its error, because a parse failure
is nearly always an error response nobody expected, and `expected value at line
1 column 1` says nothing about which one.

In a test, reach for [`TestResponse`](testing.md#testresponse) instead — it
reads the body once and lets every assertion chain.

`take_body()` takes the body and leaves an empty one behind, for middleware
that has to look at what was produced and put something else back — compressing
it, hashing it for an `ETag`.

## `IntoResponse`

Handlers rarely build a `Response` by hand, because these all convert:

| Type | Result |
|---|---|
| `Response` | itself |
| `()` | 200, empty |
| `StatusCode` | that status, empty |
| `String`, `&'static str` | 200 `text/plain` |
| `Html<T>` | 200 `text/html` |
| `Json<T>` where `T: Serialize` | 200 `application/json` |
| `serde_json::Value` | 200 `application/json` |
| `Redirect` | 302/301/303/307 |
| `Body` | 200 with that body |
| `(StatusCode, T)` | `T`'s response with that status |
| `Option<T>` | `T`'s response, or a 404 |
| `Result<T, E: Into<Error>>` | `T`'s response, or the error's |
| `Error` | see [Error Handling](errors.md) |

```rust
async fn create() -> (StatusCode, Json<Post>) {
    (StatusCode::CREATED, Json(post))
}

async fn show(id: u64) -> Option<Json<Post>> {
    find(id).map(Json)          // None becomes a 404
}
```

Implement it for your own types when a handler returns something domain-shaped:

```rust
impl IntoResponse for PostView {
    fn into_response(self) -> Response {
        Response::json(&self).with_header("x-post-id", &self.id.to_string())
    }
}
```

## Redirects

```rust
Redirect::to("/posts")            // 302 Found
Redirect::permanent("/new")       // 301
Redirect::see_other("/posts")     // 303 — after a POST
Redirect::temporary("/retry")     // 307 — preserves the method
```

`see_other` is the one you want after a successful form submission: it turns
the follow-up into a `GET`, which is what stops a refresh from re-submitting.

## Cookies

```rust
use rainier_framework::http::{Cookie, SameSite};

let cookie = Cookie::new("session", &id)
    .path("/")
    .domain("example.com")
    .max_age(3600)
    .secure(true)
    .http_only(true)
    .same_site(SameSite::Lax);

Response::no_content().with_cookie(&cookie)
```

`Cookie::new` starts **`http_only` and `SameSite::Lax`**, because the common
case is a session cookie and those are the settings it should have. Turn them
off deliberately if you have a reason.

Removal:

```rust
Response::no_content().without_cookie("session")
// or
Cookie::removal("session")
```

## Streaming

A `Body` is either bytes or a stream, so a large response does not have to fit
in memory:

```rust
use futures::stream;

let stream = stream::iter(chunks.into_iter().map(Ok));
Response::stream(stream).with_content_type("text/event-stream")
```

This is the asymmetry with [buffered request
bodies](requests.md#why-bodies-are-buffered): requests are bounded by a limit
you set and benefit from synchronous accessors; responses are unbounded and
benefit from streaming.

To collect one in a test:

```rust
let bytes = response.into_http().into_body().collect().await?;
let text = String::from_utf8(bytes.to_vec())?;
```

## Views

```rust
let html = View::instance().render("posts.show", &json!({ "post": post }))?;
Ok(Response::html(html))
```

See [Views](views.md).

## Errors

Returning `Err` from a handler produces a response through `IntoResponse for
Error` — status from the `ErrorKind`, body shaped by whether the client
[expects JSON](requests.md#headers). The kernel may then re-render it through
the [`ExceptionRenderer`](errors.md#the-exception-renderer).

```rust
async fn show(request: Req) -> Result<Response> {
    let post = posts.find_or_fail(id).await?;      // 404, with a message naming the model
    Ok(Response::json(&post))
}
```
