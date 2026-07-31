# Views

The **V** in MVC. Templates live in `resources/views/`, use a directive
syntax that will be familiar from PHP templating engines, and are addressed by
dotted name.

```
resources/views/
  layouts/app.view.html
  home.view.html
  posts/show.view.html
  mail/welcome.view.html
```

```rust
let html = View::instance().render("posts.show", &json!({ "post": post }))?;
Ok(Response::html(html))
```

`posts.show` → `resources/views/posts/show.view.html`. Dots are directory
separators.

## Rendering

```rust
// Through the facade
View::instance().render("home", &data)?;

// Through a View value
let view = View::with("posts.index", json!({ "posts": posts }))?;
View::instance().render_view(&view)?;

// Building the data up
let view = View::new("home")
    .add("title", "Hello")?
    .add("user", &user)?;
```

The engine is a port. `Views` (the newtype the facade resolves) wraps
`Arc<dyn ViewEngine>`, so swapping `TemplateEngine` for Tera or Askama does not
change a single call site:

```rust
pub trait ViewEngine: Send + Sync + 'static {
    fn render(&self, name: &str, data: &Value) -> Result<String>;
    fn render_view(&self, view: &View) -> Result<String> { … }
}
```

```rust
Rainier::new(".").with_views(Arc::new(MyEngine::new()))
```

## The syntax

### Output

```html
{{ user.name }}          escaped
{!! post.body_html !!}   raw
```

**`{{ }}` HTML-escapes. `{!! !!}` does not.** The safe form is the short one,
so the way you write a value without thinking is the way that cannot inject a
`<script>`. Emitting raw HTML is possible but has to be asked for — and it is
visibly ugly, which is the point.

Paths are dotted, and numeric segments index arrays:

```html
{{ user.address.city }}
{{ items.0.name }}
```

A path that does not resolve renders as empty rather than failing. A template
is the wrong place to discover that the controller forgot a key, and a blank
gap is easier to spot than a 500.

### Conditionals

```html
@if(user)
    Hello, {{ user.name }}.
@elseif(guest_allowed)
    Hello, stranger.
@else
    Please log in.
@endif

@if(!post.published)
    <span class="draft">Draft</span>
@endif

@if(post.views > 1000)
    <span class="popular">Popular</span>
@endif
```

A condition is one of:

| Form | Meaning |
|---|---|
| `@if(path)` | present and truthy |
| `@if(!path)` | absent or falsy |
| `@if(path == literal)` | and `!=`, `>`, `>=`, `<`, `<=` |

A literal is a quoted string, a number, `true`, `false`, or `null`.

### Loops

```html
@foreach(posts as post)
    <li>{{ post.title }}</li>
@endforeach

@foreach(settings as key => value)
    <dt>{{ key }}</dt><dd>{{ value }}</dd>
@endforeach
```

Works over arrays and over objects, where the `key => value` form gives you the
field names.

### Includes

```html
@include('partials.nav')
```

The partial renders with the same data as its parent.

### Layouts

`layouts/app.view.html`:

```html
<!doctype html>
<html>
<head><title>@yield('title')</title></head>
<body>
    <main>@yield('content')</main>
</body>
</html>
```

`home.view.html`:

```html
@extends('layouts.app')

@section('title')Home@endsection

@section('content')
    <h1>{{ heading }}</h1>
    @foreach(posts as post)
        <article>{{ post.title }}</article>
    @endforeach
@endsection
```

`@extends` names the layout, `@section` fills a slot, `@yield` is where the
layout drops it. A `@yield` with no matching section renders empty.

## What it deliberately cannot do

There is **no arbitrary expression evaluation and no way to call a function**.
You cannot write `{{ user.posts().count() }}`, or `{{ price * 1.2 }}`, or
`@if(count(items) > 3)`.

That is not an unfinished feature. A template that can compute is a template
that ends up holding business logic — and business logic in a template is
untestable, uncacheable, and invisible to anyone reading the controller.

Prepare the data in the controller; let the template lay it out:

```rust
// In the controller, where it can be tested.
let data = json!({
    "posts": posts,
    "post_count": posts.len(),
    "total_with_tax": total * 1.2,
    "is_popular": views > 1000,
});
```

The full directive list is `{{ }}`, `{!! !!}`, `@if`/`@elseif`/`@else`/`@endif`,
`@foreach`/`@endforeach`, `@include`, `@extends`, `@section`/`@endsection`,
`@yield`. That is the whole language.

There is also no `{{-- comment --}}` directive — use an HTML comment, or leave
the note in the controller where the data is prepared.

## The engine

```rust
TemplateEngine::new("resources/views")
    .without_cache()                 // re-read every render
    .with_extension("view.html")     // the default
```

Templates are **parsed once and cached** by default. Call `without_cache()` in
development so an edit shows up without a restart — which is what the sample
project does outside production:

```rust
match mode {
    Mode::Running => TemplateEngine::new("resources/views"),
    Mode::Testing => TemplateEngine::new("resources/views").without_cache(),
}
```

`flush()` clears the cache at runtime.

### Path safety

`path_for` sanitises every segment of a view name: empty segments, `.` and
`..` are dropped. A view name is frequently derived from user input somewhere
up the stack, and on Windows `Path::join` with a rooted path *discards the
base* — so `path_for("..secrets")` had to be made incapable of escaping
`resources/views`. It is.

## Testing views

`MemoryEngine` holds templates as strings, so a test needs no files:

```rust
let engine = MemoryEngine::new()
    .with("greeting", "Hi {{ name }}")
    .with("posts.index", "@foreach(posts as p){{ p.title }}@endforeach");

assert_eq!(engine.render("greeting", &json!({ "name": "Ada" }))?, "Hi Ada");
```

This is also how [mailables](mail.md) are tested — the mailer takes a
`ViewEngine`, so a `MemoryEngine` makes the rendered body assertable without a
`resources/` directory.

## Escaping directly

```rust
use rainier_framework::view::escape_html;

escape_html("Hello & <welcome>");   // "Hello &amp; &lt;welcome&gt;"
```

Escapes `&`, `<`, `>`, `"` and `'`.
