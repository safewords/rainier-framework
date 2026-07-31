# Repositories

Active-record ORMs put queries on the model: `Post::where('published', true)->get()`.
Rainier puts them behind a **contract**, because a static method on a struct is
not something a test can substitute and not something a container can hold.

```rust
let posts: Arc<dyn Repository<Post>> = app.resolve()?;
let published = posts.matching(Criteria::new().where_eq("published", true)).await?;
```

The good news is that **declaring a repository is no code**:
`EntityRepository<M>` implements the whole `Repository<M>` contract for *any*
model.

```rust
let posts = EntityRepository::<Post>::new(db);
let posts = EntityRepository::<Post>::new(db).with_events(dispatcher);   // + hooks
```

## The contract

### Reading

```rust
posts.all().await?;                                  // every row
posts.find(7.into()).await?;                         // by primary key
posts.find_key(7).await?;                            // …with conversion
posts.find_or_fail(7).await?;                        // …or a 404 naming the model
posts.find_by_route_key("hello").await?;             // by Model::route_key_name()

posts.find_by("author_id", 7.into()).await?;         // every match
posts.first_by("slug", "hello".into()).await?;       // the first
posts.where_eq("author_id", 7).await?;               // …with conversion

posts.matching(criteria).await?;
posts.first_matching(criteria).await?;
posts.count_matching(criteria).await?;
posts.count().await?;
posts.exists(criteria).await?;

posts.paginate(1, 20).await?;
posts.paginate_matching(criteria, 1, 20).await?;
```

### Writing

```rust
let created = posts.create(post).await?;             // returns it with its assigned key
posts.update(&post).await?;                          // every non-key column, by primary key
posts.upsert(&post, &["slug"], &["title", "body"]).await?;
posts.delete(7.into()).await?;
posts.delete_matching(criteria).await?;
```

`create` returns the model with any database-assigned key filled in. It follows
`last_insert_id` **only when the model actually has an auto-increment key** —
an application-assigned string key is left alone, which is the bug you would
otherwise hit the first time you used a UUID primary key.

`upsert` with an empty `update_columns` is insert-or-ignore.

### Object safety

```rust
async fn find(&self, key: Value) -> Result<Option<M>>;      // in the vtable
async fn find_key(&self, key: impl Into<Value> + Send) -> Result<Option<M>>
where Self: Sized;                                           // not in the vtable
```

Keys and values arrive as `Value` rather than through generic parameters, so
`Arc<dyn Repository<Post>>` is a usable dependency. The ergonomic generic
wrappers are provided methods bounded on `Self: Sized`, which keeps them out of
the vtable.

The practical consequence: on an `Arc<dyn Repository<Post>>` you write
`find(7.into())`; on a concrete `EntityRepository<Post>` you can write
`find_key(7)`.

## Criteria

`Criteria` is a query scope as a value — something you can build up, pass
around, and merge.

```rust
let criteria = Criteria::new()
    .where_eq("published", true)
    .where_gte("created_at", cutoff)
    .where_like("title", "%rust%")
    .order_by_desc("created_at")
    .limit(20);
```

| Group | Methods |
|---|---|
| equality | `where_eq`, `where_ne` |
| ordering | `where_gt`, `where_gte`, `where_lt`, `where_lte` |
| pattern | `where_like`, `where_not_like` |
| sets | `where_in`, `where_not_in` |
| null | `where_null`, `where_not_null` |
| joins | `join(table, local, foreign)` |
| ordering | `order_by`, `order_by_desc` |
| paging | `limit`, `offset` |
| composition | `merge`, `when` |

### Conditional clauses

`when` keeps an optional filter declarative instead of branching around two
nearly identical queries:

```rust
let criteria = Criteria::new()
    .where_eq("published", true)
    .order_by_desc("created_at")
    .when(term.is_some(), |criteria| {
        criteria.where_like("title", format!("%{}%", term.unwrap_or_default()))
    });
```

### Composing

```rust
fn published() -> Criteria {
    Criteria::new().where_eq("published", true)
}

let recent = published().merge(Criteria::new().order_by_desc("created_at").limit(10));
```

### Joins

```rust
Criteria::new()
    .join("users", "author_id", "id")
    .where_eq("users.active", true)
```

A real `JOIN`, for when both sides genuinely are in one database and you want
it to do the work. A [relationship](relationships.md) is the other tool: it
loads the far side through its own repository with a `WHERE key IN (…)`, which
is what keeps the two free to live in different backends.

## Named queries

`EntityRepository<M>` already does CRUD for every model, so an
application-specific repository exists for **one reason: to give this
application's queries a name**.

Newtype it, and `Deref` to keep everything the generic one already does:

```rust
pub struct PostRepository {
    inner: EntityRepository<Post>,
}

impl PostRepository {
    pub fn new(db: Database, events: Arc<Dispatcher>) -> Self {
        Self { inner: EntityRepository::<Post>::new(db).with_events(events) }
    }

    /// A page of published posts, newest first, optionally filtered by title.
    pub async fn published_page(
        &self,
        page: u64,
        per_page: u64,
        search: Option<&str>,
    ) -> Result<Paginated<Post>> {
        let term = search.map(str::trim).filter(|t| !t.is_empty());

        let criteria = Criteria::new()
            .where_eq("published", true)
            .order_by_desc("created_at")
            .when(term.is_some(), |c| {
                c.where_like("title", format!("%{}%", term.unwrap_or_default()))
            });

        self.inner.paginate_matching(criteria, page, per_page).await
    }
}

impl Deref for PostRepository {
    type Target = EntityRepository<Post>;
    fn deref(&self) -> &Self::Target { &self.inner }
}
```

`published_page` lives here rather than being a `Criteria` each controller
assembles, so **"what published means" is defined once**. That is the whole
argument for the repository pattern, and the `Deref` makes the newtype free.

A more interesting one — enforcing an invariant the database cannot:

```rust
/// Store a post, giving it a slug nothing else has taken.
///
/// Two posts with the same title would otherwise collide on the unique
/// index, and a constraint violation is a worse answer than a suffix.
pub async fn create_unique(&self, mut post: Post) -> Result<Post> {
    let base = post.slug.clone();
    let mut suffix = 2;

    while self.inner.exists(Criteria::new().where_eq("slug", post.slug.clone())).await? {
        post.slug = format!("{base}-{suffix}");
        suffix += 1;
    }

    self.inner.create(post).await
}
```

## Registering repositories

```rust
// src/app/providers/repositories.rs
pub fn register(app: &Application, database: &Database) {
    let events = app.expect_resolve::<Dispatcher>();

    app.instance(PostRepository::new(database.clone(), events));
    app.instance(UserRepository::new(database.clone()));
}
```

Bind the trait object too when something should depend on the contract rather
than the concrete type — the [auth user
provider](authentication.md#user-providers) does:

```rust
let users: Arc<dyn Repository<User>> = Arc::new(EntityRepository::new(database.clone()));
app.instance(users);
```

See [Container: binding a trait object](container.md#binding-a-trait-object).

## Hooks

A repository built `.with_events(dispatcher)` dispatches
[lifecycle events](models.md#lifecycle-hooks) around every write, and a `-ing`
listener returning `Err` vetoes it. Without a dispatcher, writes are silent —
which is what you want in a seeder.

## Testing

Two levels, and both are useful:

**Against a recording connection**, to assert on the SQL:

```rust
let (db, connection) = fake_database(MemoryConnection::new(Dialect::Sqlite));
let posts = EntityRepository::<Post>::new(db);

posts.first_by("slug", "hello".into()).await?;
assert!(connection.last_statement().unwrap().contains("posts"));
```

**Against your own implementation of the contract**, when the test is about the
caller:

```rust
struct StubPosts(Vec<Post>);

#[async_trait]
impl Repository<Post> for StubPosts { … }
```

That is what depending on `Arc<dyn Repository<Post>>` rather than a concrete
type buys you.
