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
posts.update(&post).await?;                          // EVERY non-key column, by primary key
posts.update_matching(criteria, set).await?;         // only the named columns
posts.update_column(criteria, "seen_at", now).await?;  // …when there is one
posts.upsert(&post, &["slug"], &["title", "body"]).await?;
posts.delete(7.into()).await?;
posts.delete_matching(criteria).await?;
```

`create` returns the model with any database-assigned key filled in. It follows
`last_insert_id` **only when the model actually has an auto-increment key** —
an application-assigned string key is left alone, which is the bug you would
otherwise hit the first time you used a UUID primary key.

`upsert` with an empty `update_columns` is insert-or-ignore.

#### The conflict target must be a column the insert supplies

An upsert collides on the values it inserts, so a conflict target naming a
column the statement leaves out cannot collide with anything. The database has
nothing to match the stored row against, takes the insert branch, writes a new
row, and reports one row affected — which is worse than an error, because the
caller is told it worked.

The way to reach that is an **auto-increment primary key**, and it is the first
target a caller reaches for, because `id` is the key they think in.
`Entity::insert_values` omits an auto-increment key on purpose — assigning it is
the database's job — so `on(["id"])` names a column that is never in the
statement. The two designs are individually right and silently incompatible, so
the combination is **refused** rather than rendered. Before it was, an
`Upsert::on(["id"]).increment(["views"])` meant to raise one row's counter
appended a fresh row per call, and the table filled with duplicates under a key
that was supposed to be unique.

Conflict on the columns carrying the uniqueness the upsert is arbitrating: the
natural key the row is identified by, matching a `UNIQUE` constraint.

The conflict target is also **not optional**, for a portability reason. MySQL
infers the key and *rejects* a target (`ON DUPLICATE KEY UPDATE …`); SQLite and
Postgres require one (`ON CONFLICT (a, b) DO UPDATE …`). A builder letting the
columns be left out would render a statement that works on MySQL and is a syntax
error everywhere else — which a MySQL-backed application would ship without ever
seeing.

For a counter, `Upsert` has an accumulating form. `UpsertAction::Replace` writes
the inserted value over the stored one; `UpsertAction::Increment` adds it:

```rust
use rainier_orm::{repo, Upsert};

// Insert the row, or add this row's `n` to the `n` already stored under the
// same (a, b) pair.
repo::upsert_with(&db, &tally, &Upsert::on(["a", "b"]).increment(["n"])).await?;
```

They are distinct variants rather than a flag because getting the pair the wrong
way round is the same silent undercount as a read-then-write — a counter that
should read the running total instead reads whatever the last caller submitted —
and the dialects spell the difference out too differently to write by hand once
(`n = n + VALUES(n)` against `n = "t"."n" + "excluded"."n"`).

#### `update` writes every column, and that is usually not what you meant

`update(&model)` is the more obvious call and it is the wrong one whenever you
mean to change a *subset* of a row. It writes every non-key column from a struct
that was read at some earlier moment, so the columns nobody meant to touch are
written back as they were **then**:

```text
t0  A reads the row              (seen_at = NULL, email_sent_at = NULL)
t1  B writes seen_at = now       (seen_at = 12:01)
t2  A sets email_sent_at on its copy and calls `update`
      → UPDATE … SET seen_at = NULL, email_sent_at = 12:02 WHERE id = 7
```

`seen_at` is `NULL` again. Nothing errored, one row was affected, and the return
value is the same `1` a correct write produces — the damage is visible only in
the stored data, later, to somebody who does not know to look.

`update_matching` names the columns it means, and the rest are left alone. It
also takes a **predicate over more than the key**, which `update` structurally
cannot: `update`'s `WHERE` *is* the primary key, so a guarded write ("stamp it,
but only if it is still unstamped") has to do the check in the process between
the read and the write — which is where the race lives.

```rust
// Stamp only the rows that are still unstamped, in one statement.
let stamped = notifications
    .update_column(
        Criteria::new().where_in("id", ids).where_null("email_sent_at"),
        "email_sent_at",
        now,
    )
    .await?;
```

Three things to know about it:

- **No lifecycle hooks.** Like `delete_matching`, it fires no
  `Updating`/`Updated`: there is no model to hand a hook, and the statement may
  match none or a million rows. Reading them first to build one would restore
  the table scan the bulk form exists to avoid. If you need per-row hooks, use
  `matching` and a loop — and know you are asking for N statements.
- **No soft-delete scoping.** `deleted_at IS NULL` is not appended, which is
  what makes this the way to tombstone or restore more than one row at a time:
  set the column to a timestamp to trash them, to `NULL` to restore them. A
  scope here would make the restore match nothing, silently and forever.
- **An empty `set` runs no statement** and returns `Ok(0)`. "These zero columns
  changed" is a real answer for a caller building the list from a diff, and
  `SET` with no assignments is not valid SQL.

For a column that must *accumulate* rather than be assigned — a counter — no
`Vec<(String, Value)>` can express it, because the new value is not known until
the stored one is. That is `statement::update_matching_with` and its
[`Assignment`](#counters-and-correlated-writes).

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
| subqueries | `where_exists`, `where_not_exists`, `where_subquery` |
| `OR` | `or_where(\|group\| …)` |
| joins | `join(table, local, foreign)` |
| aggregates | `select(projection, alias)`, `group_by`, `order_by_alias` |
| soft deletes | `with_trashed`, `only_trashed` — the scope is [automatic](models.md#soft-deletes); these opt out |
| ordering | `order_by`, `order_by_desc` |
| paging | `limit`, `offset`, `distinct` |
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

### `OR` groups

`Criteria`'s own predicates are `AND`-combined. `or_where` adds a group whose
branches are `OR`-ed with each other and `AND`-ed with the rest:

```rust
Criteria::new().where_eq("state", "active").or_where(|any| {
    any.where_like("name", "%ada%")
        .where_exists(Subquery::count("owners").correlate("thing_id", "id"))
})
```

The closure receives an `OrGroup` rather than a `Criteria`, and that is
load-bearing. When it took a `Criteria`, only the plain constraints survived —
so a group mixing a column predicate with a `where_exists` **lost the `EXISTS`
branch** and narrowed the result, and a group holding *only* an `EXISTS` came
out empty and was skipped entirely, taking the whole `AND (…)` with it and
returning rows the query was written to exclude. Both compiled, both ran,
neither said anything.

So the bound is the type. An `OR` group holds predicates and nothing else, and
what cannot be `OR`-ed is not a method on it: a join, a limit, an ordering or a
nested group is a compile error rather than something quietly dropped on the way
to SQL.

### Correlated subqueries

`EXISTS (SELECT 1 FROM t WHERE t.owner = <the outer row>.id)` compares a column
against **another column**, not against a bound value — which a builder that
only knows `column = value` cannot say. These were the last two query shapes an
application still had to write as raw SQL.

```rust
use rainier_framework::database::{Comparison, Criteria, Subquery};

// rows that have at least one approved child
Criteria::new().where_exists(
    Subquery::count("children").correlate("parent_id", "id").where_eq("approved", true),
);

// rows with more than ten of them
Criteria::new().where_subquery(
    Subquery::count("children").correlate("parent_id", "id"),
    Comparison::Gt,
    10,
);
```

`where_not_exists` is the third. All three are available on an
[`OrGroup`](#or-groups) too.

#### A subquery carries its correlation from construction

**You cannot build one without it.** `Subquery::count` and `Subquery::select`
hand back a `SubqueryDraft`, and `correlate` is the only way to turn a draft
into a `Subquery` — which is what every method accepting a subquery takes.

That is not ceremony. `EXISTS (SELECT 1 FROM t)` with no correlation is true for
*every* outer row the moment `t` holds a single row, so the predicate silently
matches the entire table. Nothing errors, the SQL reads plausibly, and the only
symptom is more rows than the caller meant to expose — which, for a visibility
filter, is the rows of every other user. The mistake is not caught late; it is
unwritable.

Call `correlate` again for a second column pair, which is what a composite
foreign key needs — matching on half of one is the same silent over-match a
missing correlation is.

`inner_column` is a column of the subquery's own table. `outer_column` reads
like every other column spec: `"name"` is the outer query's own table,
`"table.name"` one it joined.

A subquery holds `AND`-combined column-against-value predicates and its
correlations, and nothing else — no joins, no `OR` groups, no subquery of its
own. That bound is the type rather than a rule documented and dropped at render
time.

### Aggregates

`select` names a projection and an alias, `group_by` groups, and `aggregate`
returns the projected rows rather than models:

```rust
use rainier_framework::database::{DatePart, Projection};

let by_month = posts
    .aggregate(
        Criteria::new()
            .select(Projection::DatePart(DatePart::Month, "created_at".into()), "month")
            .select(Projection::CountAll, "posts")
            .group_by(Projection::DatePart(DatePart::Month, "created_at".into()))
            .order_by_alias("month", false),
    )
    .await?;
```

The point is that the dialects disagree: `MONTH(x)` is MySQL's spelling and does
not exist in SQLite, so the same report written as raw SQL works in production
and fails in the test suite. `Projection::DateOf` is deliberately distinct from
`DatePart(Day, …)` — a day-of-month is 1–31, so grouping by it collapses the
same day of different months into one bucket and a daily chart silently adds
January to February.

`aggregate` is on `Repository`, which is bounded on `Model`. A composite-key
entity reaches the same query through `EntityRepository::aggregate_rows`, which
is bounded on `Entity` alone — an aggregate never identifies a row by anything,
so leaving it `Model`-bound left a composite-key entity's only route to a `SUM`
as raw SQL, or loading every row and adding them up in process, which is the
same table scan moved somewhere it cannot be indexed.

### Counters and correlated writes

An `UPDATE … SET` takes an `Assignment`, which is a bound value, an increment,
or a correlated subquery.

```rust
use rainier_framework::database::{Assignment, statement};

// n = n + 1, without reading it first
let prepared = statement::update_matching_with::<Post>(
    db.dialect(),
    &Criteria::new().where_eq("id", 7),
    vec![("views".into(), Assignment::Increment(1))],
);
db.execute(prepared).await?;
```

**`Increment` exists because a value cannot express it.** The new total is not
known until the stored one is, so working it out in the process means reading
the row, adding, and writing the result back — and under any concurrency that
loses additions: two callers read the same stored value, both compute the same
total, and the second write overwrites the first. No statement errors and no row
count says a write was dropped; the number is merely too low, so the only way to
notice is to already know what it should have been. Doing the arithmetic inside
the statement makes the read and the write one operation, which the database
serialises on the row it is already locking.

The amount is **signed**, so a decrement is the same primitive with a negative
argument rather than a second variant — one rendering means one place for the
`+` to be right, and it keeps a decrement from being spelled as an unsigned
subtraction that wraps underneath.

`NULL + n` is `NULL` on every dialect, so this leaves a counter that has never
been set at `NULL` rather than raising it to `n`. Give such a column a
`NOT NULL DEFAULT 0`; the arithmetic cannot fix it, and it fails by writing
nothing rather than by erroring.

`Assignment::Subquery` is the other one, and it makes a bulk counter
recomputation a single statement:

```rust
Assignment::Subquery(
    Subquery::count("children").correlate("parent_id", "id").where_eq("approved", true),
)
```

Because a `COUNT` over no rows is `0` rather than no row at all, that writes zero
to the rows with no related records instead of leaving whatever was there. The
loop it replaces cannot: a `GROUP BY` produces no group for a count of zero, so
a per-row fill has to zero every counter first and put them back, leaving a
window in which every row reads zero. One statement has no such window.

Mind the empty-set behaviour for the others — `SUM`, `MIN`, `MAX` and `AVG` over
no rows are `NULL`, which writes `NULL` into the target column, or fails outright
if it is `NOT NULL`.

`Assignment::Subquery` is not a stand-in for `Increment` on two counts:
`Projection` has no arithmetic, so there is nothing to write `n + ?` with; and a
subquery reading the table being updated is MySQL error 1093, with no portable
way around it.

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
