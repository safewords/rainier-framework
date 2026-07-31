# Relationships

`HasOne`, `HasMany`, `BelongsTo` and `BelongsToMany`, as **values you load**
rather than properties that query themselves.

```rust
impl Post {
    pub fn author() -> BelongsTo<Post, User> {
        BelongsTo::new().foreign_key("author_id")
    }
}

let posts = post_repo.all().await?;
let authors = Post::author().load(&posts, &*users).await?;   // one query

for post in &posts {
    println!("{} by {}", post.title, authors.one(post).unwrap().name);
}
```

---

## Why loading is a call and not a property

A lazy-loading ORM gives you `$post->author`, and the price is that the same
expression is one query inside a loop and none after an eager load. You cannot
tell which by reading it.

PHP can do that because `__get` can run a query. Rust has no such hook, and
forging one — a `RefCell`, a connection handle on every model, a blocking call
inside `Deref` — would buy the syntax by making every model carry a database.

So Rainier keeps the two apart. **Declaring** a relationship is a value;
**loading** it is a call that takes the whole slice of parents and issues one
query for all of them.

The consequence is the point: **N+1 is not mitigated, it is unrepresentable.**
There is no per-model load to accidentally put in a loop, because `load` does
not take a model — it takes a slice.

> This page replaces the older "relationships are explicit, write the query
> yourself" advice. That advice was right about the constraint and wrong about
> the ergonomics: batching by hand is what everybody was going to do, so it
> belongs in the framework.

## What "one query" means across backends

Loading is a `WHERE key IN (…)` against the **other side's own repository**,
never a `JOIN`. That is what lets the two sides live in different databases, or
on different shards — and it is the strategy every eager loader converges on,
for the same reason.

[`Criteria::join`](repositories.md#joins) is still there for when both sides
genuinely are in one place and you want the database to do the work.

---

## The four kinds

| Kind | Declared as |
|---|---|
| One parent, many children | `HasMany::<User, Post>::new()` |
| One parent, one child | `HasOne::<User, Profile>::new()` |
| The inverse | `BelongsTo::<Post, User>::new()` |
| Many to many | `BelongsToMany::<Post, Tag>::new("post_tag")` |

The type parameters read *from* the near side *to* the far one, which is also
the direction the lookup goes.

### Conventions, and overriding them

Keys default by convention — `User` → `user_id`, matched against the user's
primary key:

```rust
HasMany::<User, Post>::new()                       // posts.user_id = users.id
HasMany::<User, Post>::new().foreign_key("author_id")
BelongsTo::<Post, User>::new().owner_key("uuid")
BelongsToMany::<Post, Tag>::new("post_tag")        // post_tag(post_id, tag_id)
```

`BelongsToMany::conventional_pivot()` gives the conventional pivot name — the
two singulars in alphabetical order, `post_tag` — if you would rather assert on
it than hard-code it.

### Declaring both sides

Declaring `Post::tags()` and `Tag::posts()` is not duplication. Which side is
"near" decides what you can look a row up **by**, and an application usually
wants both directions.

---

## Reading what was loaded

`load` returns a `Related<C>`, which is keyed by the near side — so you hand it
the model, not a key:

```rust
let comments = Post::comments().load(&posts, &*comment_repo).await?;

comments.of(&post);           // &[Comment]        — every one
comments.one(&post);          // Option<&Comment>  — the first, for has_one/belongs_to
comments.count_of(&post);     // usize             — how many were loaded
comments.len();               // across every parent
```

Or pair them up and be done:

```rust
let pairs: Vec<(Post, Vec<Comment>)> = comments.zip(posts);
```

`zip` keeps a parent with no children — it pairs with an empty `Vec` rather
than vanishing, which is what a list rendering needs.

### Constrained loads

```rust
Post::comments()
    .matching(Criteria::new().where_eq("approved", true).order_by_desc("created_at"))
    .load(&posts, &*comments)
    .await?
```

One caveat, stated because the alternative is silent wrongness: **a `limit`
here limits the whole query, not each parent's share.** One query cannot take
"the newest three per parent" without a window function, so it does not
pretend to.

### Counting without loading

```rust
let counts = User::posts().count(&users, &*post_repo).await?;

counts.of(&user);   // u64 — 0 for a user with none
counts.total();
```

One `SELECT author_id, COUNT(*) … GROUP BY author_id`. A parent with no
children produces no row at all, which is why `of` reports `0` rather than
`None` — "none" and "zero" are the same answer here.

### One parent

```rust
let posts = User::posts().for_one(&user, &*post_repo).await?;
```

The one-off case — a show page for one user. Inside a loop this is the N+1 that
`load` exists to prevent, so it is deliberately the longer-named method.

---

## Many to many

```rust
let tags = Post::tags().load(&posts, &*tag_repo).await?;
```

**Two queries**, not one: the pivot rows, then the related rows. Not three, and
not one per parent — the count does not grow with the number of posts, which is
the property that matters.

A tag linked from three posts is fetched once and appears under all three.

### The pivot needs no model

It is two foreign keys. Nothing reads a row of it on its own, and
`BelongsToMany` fetches it as two columns through
[`Repository::pivot_links`](repositories.md).

A pivot that carries its own data — `role`, `created_at`, `sort_order` — is a
model in its own right. Two `has_many`s through it say so more clearly than a
pivot with attributes, and you get a repository and lifecycle hooks for it as a
side effect.

### The migration

A [blueprint](migrations.md#the-schema-builder), because there is no entity to
derive it from:

```rust
Step::create("0007_create_post_tag", "post_tag", |table| {
    table.foreign_id("post_id").constrained_on("posts").cascade_on_delete();
    table.foreign_id("tag_id").constrained_on("tags").cascade_on_delete();

    table.primary(["post_id", "tag_id"]);
})
```

Three things worth having, and the builder gives you two of them for asking:

- the **composite primary key**, so a double-click cannot attach the same tag
  twice;
- **cascades**, so deleting a post takes its links rather than leaving rows
  pointing at nothing;
- an index on **each** column, which `foreign_id` adds — a pivot is read from
  both directions, and the primary key only helps the one that leads with
  `post_id`.

---

## Keys

Lookup keys are normalised to text, so a `u64` primary key and the `u64`
foreign key pointing at it match whichever integer width each side's driver
hands back. Two different columns are never compared, so the normalisation
cannot conflate unrelated rows.

A `NULL` foreign key matches nothing — including a `NULL` key on the other
side, which is what SQL would also say.

---

## In a controller

```rust
pub async fn index(Validated(query): Validated<ListPostsRequest>) -> Result<Response> {
    let page = posts.published_page(query.page, query.per_page, None).await?;

    let authors = Post::author().load(&page.data, &**users).await?;
    let tags = Post::tags().load(&page.data, &*tag_rows).await?;

    // …one JSON object per post, with its author and tags
}
```

Three queries for a page of twenty. Four for a page of a thousand.

---

## What is not here

**No lazy loading.** Deliberately — see [above](#why-loading-is-a-call-and-not-a-property).

**No `has_many_through` or polymorphic relations.** Both are expressible as two
loads and a `where_in`; neither has earned a type yet. Say if you want one.

**No `attach`/`detach`/`sync` on a pivot.** Writing to a pivot is an `INSERT`
into a two-column table, and the repository's escape hatch —
`repository.database()` — is the honest way to do it until there is a reason
for more.

**No cascading saves.** Saving a parent does not save its children. Rust makes
"which of these are dirty?" a question you cannot answer without tracking every
field, and a save that quietly writes rows you did not ask about is worse than
two calls.
