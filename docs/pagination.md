# Pagination

```rust
let page = posts.paginate(1, 20).await?;
let page = posts.paginate_matching(criteria, 1, 20).await?;
```

Both return `Paginated<T>` — the rows, plus the counts a page needs.

```rust
pub struct Paginated<T> {
    pub data: Vec<T>,
    pub total: u64,
    pub page: u64,
    pub per_page: u64,
}
```

Two queries run: a `COUNT` with the criteria's filters but **not** its paging,
and a `SELECT` with both.

## What it computes

```rust
page.last_page();          // total pages, at least 1
page.from();               // Option<u64> — 1-based index of the first row
page.to();                 // Option<u64> — …and the last
page.has_more_pages();
page.next_page();          // Option<u64>
page.previous_page();      // Option<u64>
page.offset();             // the SQL offset this page used
page.len();                // rows on this page
page.is_empty();
```

`from` and `to` are `Option` because an empty result has neither — rendering
"Showing 1 to 0 of 0" is a bug, and this makes it one you cannot write by
accident.

## Serialising

`Paginated<T>` is `Serialize`, so an API endpoint is one line:

```rust
pub async fn index(Validated(query): Validated<ListPostsRequest>) -> Result<Response> {
    let posts = resolve::<PostRepository>()?;
    let page = posts.published_page(query.page, query.per_page, query.search.as_deref()).await?;

    Ok(Response::json(&page))
}
```

```json
{
  "data": [ … ],
  "total": 137,
  "page": 2,
  "per_page": 20
}
```

The computed values are methods rather than fields, so the payload stays the
four facts a client needs to compute the rest — and stays stable if the
computations change.

## Transforming

`map` keeps the counts and changes the rows, which is how a model becomes a
view struct without rebuilding the page:

```rust
let page = posts.paginate(1, 20).await?.map(PublicPost::from);
```

That is the answer to [serialising a model with a secret in
it](models.md#serialising).

## Validating the page parameters

Page numbers come from the client, so they belong in a
[request contract](validation.md#request-contracts):

```rust
#[derive(Deserialize)]
pub struct ListPostsRequest {
    pub page: u64,
    pub per_page: u64,
    pub search: Option<String>,
}

#[async_trait]
impl FormRequest for ListPostsRequest {
    fn rules() -> RuleSet {
        vec![
            field("page", [Rule::Integer, Rule::Min(1.0)]),
            field("per_page", [Rule::Integer, Rule::Between(1.0, 100.0)]),
            field("search", [Rule::String, Rule::Max(100.0)]),
        ]
    }

    fn from_input(input: Value) -> Result<Self> {
        Ok(Self {
            page: input.get("page").and_then(|v| v.as_u64()).unwrap_or(1),
            per_page: input.get("per_page").and_then(|v| v.as_u64()).unwrap_or(20),
            search: input.get("search").and_then(|v| v.as_str()).map(str::to_string),
        })
    }
}
```

Note the `Between(1, 100)` on `per_page`. Without an upper bound, a client
asking for `per_page=1000000` is a denial-of-service against your own database
— and it is the first thing anyone tries.

The custom `from_input` supplies defaults for absent parameters, which is the
case where the derived deserialisation is not what you want.

## Building one directly

```rust
Paginated::new(rows, total, page, per_page);
Paginated::empty(page, per_page);
```

Useful when the rows come from somewhere other than a repository — a search
index, a cache, an upstream API.

## Cursor pagination

Rainier does not ship it. For a large, frequently-written table, offset paging
gets slower as the offset grows and can skip or repeat rows when the data
changes underneath a user.

Use a keyset query, which `Criteria` expresses directly:

```rust
let next = posts.matching(
    Criteria::new()
        .where_lt("created_at", cursor)
        .order_by_desc("created_at")
        .limit(20),
).await?;
```

Rainier ORM also has a cursor API, reachable through `Database` because it
[implements `Executor`](database.md#why-the-indirection-exists).
