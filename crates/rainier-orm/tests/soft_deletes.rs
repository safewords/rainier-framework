//! Automatic soft-delete scoping, asserted on the SQL that actually reaches the
//! executor.
//!
//! Two failures are in scope here and they point in opposite directions, which
//! is why both are covered rather than just the interesting one.
//!
//! The failure the feature exists to remove is a read that **forgets** the
//! predicate: it parses, it runs, it decodes, and it puts deleted rows in front
//! of a user. Nothing about the result says the filter was missing. So the tests
//! below assert on the rendered statement rather than on a returned row count —
//! a stub executor returns nothing either way, and "no rows" would pass whether
//! or not the predicate was there.
//!
//! The failure the feature *introduces* is the mirror image: a read that gains
//! the predicate when its author meant to see tombstoned rows, and silently
//! returns nothing. `every_read_of_an_unmarked_entity_is_byte_identical` is the
//! guard for every existing caller — it pins the exact SQL, so any drift in an
//! entity that never opted in fails here — and the `with_trashed` /
//! `only_trashed` tests are the guard for the callers that opt in and then need
//! their trash back.
//!
//! `no_select_builder_is_left_unscoped` is the structural one: it reads the two
//! query modules' own source and fails if a `SELECT` is built anywhere that does
//! not go through the scoping helper. An inconsistent scope is worse than no
//! scope, because it is invisible — one builder honouring it and its neighbour
//! not is a difference nobody can see from a call site.

use core::cell::RefCell;

use rainier_orm::sea_query::Value;
use rainier_orm::{repo, Dialect, Entity, ExecOutcome, Executor, Result, Row, ShardRoute};

/// Soft-deleting: one field marked, so every read is scoped.
#[derive(Entity, Clone, Debug)]
#[orm(table = "documents")]
struct Document {
    #[orm(pk, auto_increment)]
    id: u64,
    title: String,
    #[orm(soft_delete)]
    deleted_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// The control group: structurally the same, with no marker. Every assertion
/// about scoped behaviour has a counterpart here, because "nothing changes for
/// an entity that did not opt in" is the property every existing caller of this
/// crate is relying on.
#[derive(Entity, Clone, Debug)]
#[orm(table = "records")]
struct Record {
    #[orm(pk, auto_increment)]
    id: u64,
    title: String,
    /// Named exactly like a tombstone and deliberately *not* marked — this is
    /// the table whose `deleted_at` is domain data. Inferring the scope from the
    /// column name would silently change what this entity returns.
    deleted_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Composite key plus a tombstone: the scope has to survive alongside a key
/// predicate that is itself several `AND`-ed parts.
#[derive(Entity, Clone, Debug)]
#[orm(table = "memberships")]
struct Membership {
    #[orm(pk)]
    team_id: u64,
    #[orm(pk)]
    user_id: u64,
    role: String,
    #[orm(soft_delete)]
    deleted_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Sharded and soft-deleting, to prove the added predicate does not disturb the
/// route an equality on a shard-encoded column pins.
#[derive(Entity, Clone, Debug)]
#[orm(table = "tokens")]
struct Token {
    #[orm(pk, shard_key)]
    id: u64,
    #[orm(shard_key)]
    user_id: u64,
    #[orm(soft_delete)]
    deleted_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Records every statement it is handed, and returns no rows.
struct Recorder {
    calls: RefCell<Vec<(String, Vec<Value>)>>,
    routes: RefCell<Vec<ShardRoute>>,
}

impl Recorder {
    fn new() -> Self {
        Self { calls: RefCell::new(Vec::new()), routes: RefCell::new(Vec::new()) }
    }

    /// The one statement that was run. Panics unless there was exactly one, so a
    /// test can never assert against the wrong call.
    fn only(&self) -> String {
        let calls = self.calls.borrow();
        assert_eq!(calls.len(), 1, "expected exactly one statement, got {}", calls.len());
        calls[0].0.clone()
    }

    fn last(&self) -> String {
        self.calls.borrow().last().expect("a statement was run").0.clone()
    }
}

impl Executor for Recorder {
    fn dialect(&self) -> Dialect {
        Dialect::Sqlite
    }

    async fn fetch_all(&self, sql: &str, params: Vec<Value>) -> Result<Vec<Box<dyn Row>>> {
        self.calls.borrow_mut().push((sql.to_string(), params));
        Ok(Vec::new())
    }

    async fn execute(&self, sql: &str, params: Vec<Value>) -> Result<ExecOutcome> {
        self.calls.borrow_mut().push((sql.to_string(), params));
        Ok(ExecOutcome { rows_affected: 1, last_insert_id: 0 })
    }

    async fn fetch_all_routed(
        &self,
        route: ShardRoute,
        sql: &str,
        params: Vec<Value>,
    ) -> Result<Vec<Box<dyn Row>>> {
        self.routes.borrow_mut().push(route);
        self.fetch_all(sql, params).await
    }
}

/// `deleted_at IS NULL`, as SQLite renders it.
const ACTIVE: &str = r#""deleted_at" IS NULL"#;
/// `deleted_at IS NOT NULL`.
const TRASHED: &str = r#""deleted_at" IS NOT NULL"#;

// ---------------------------------------------------------------------------
// metadata
// ---------------------------------------------------------------------------

#[test]
fn only_a_marked_field_makes_an_entity_soft_deleting() {
    assert_eq!(Document::soft_delete_column(), Some("deleted_at"));
    assert_eq!(Membership::soft_delete_column(), Some("deleted_at"));
    assert_eq!(
        Record::soft_delete_column(),
        None,
        "a `deleted_at` column alone must not opt a table in — some tables record \
         a deletion date as domain data, and scoping those silently hides most of \
         their rows"
    );
}

// ---------------------------------------------------------------------------
// the guard for every existing caller
// ---------------------------------------------------------------------------

/// Pins the exact SQL of every read over an entity with no marker.
///
/// Not `assert!(!sql.contains("deleted_at"))`: that would still pass if the
/// scope started emitting something else. Turning this feature on changes
/// behaviour under every call site at once, and the one thing that must be true
/// is that an entity nobody opted in renders character-for-character what it
/// rendered before. So the strings are literal.
#[tokio::test]
async fn every_read_of_an_unmarked_entity_is_byte_identical() {
    let expected = [
        ("all", r#"SELECT "id", "title", "deleted_at" FROM "records""#),
        (
            "find_by_pk",
            r#"SELECT "id", "title", "deleted_at" FROM "records" WHERE "id" = ? LIMIT ?"#,
        ),
        ("find_by", r#"SELECT "id", "title", "deleted_at" FROM "records" WHERE "title" = ?"#),
        (
            "find_one_by",
            r#"SELECT "id", "title", "deleted_at" FROM "records" WHERE "title" = ? LIMIT ?"#,
        ),
        // `WHERE TRUE` is how the fluent builder has always rendered an empty
        // condition — it is pinned here precisely because it is the odd one:
        // scoping must not have taken the opportunity to tidy it, since a
        // downstream test asserting on that SQL would break for no reason.
        (
            "query.all",
            r#"SELECT "records"."id", "records"."title", "records"."deleted_at" FROM "records" WHERE TRUE"#,
        ),
        ("query.count", r#"SELECT COUNT(*) AS "cnt" FROM "records" WHERE TRUE"#),
    ];

    for (name, sql) in expected {
        let db = Recorder::new();
        match name {
            "all" => {
                repo::all::<Record, _>(&db).await.unwrap();
            }
            "find_by_pk" => {
                repo::find_by_pk::<Record, _, _>(&db, 1_u64).await.unwrap();
            }
            "find_by" => {
                repo::find_by::<Record, _, _>(&db, "title", "x").await.unwrap();
            }
            "find_one_by" => {
                repo::find_one_by::<Record, _, _>(&db, "title", "x").await.unwrap();
            }
            "query.all" => {
                repo::query::<Record>().all(&db).await.unwrap();
            }
            "query.count" => {
                repo::query::<Record>().count(&db).await.unwrap();
            }
            other => panic!("unhandled: {other}"),
        }
        assert_eq!(db.only(), sql, "`{name}` over an unmarked entity must not have changed");
    }
}

// ---------------------------------------------------------------------------
// every read builder is scoped
// ---------------------------------------------------------------------------

#[tokio::test]
async fn every_read_of_a_marked_entity_excludes_tombstoned_rows() {
    let db = Recorder::new();

    repo::all::<Document, _>(&db).await.unwrap();
    assert!(db.last().contains(ACTIVE), "all: {}", db.last());

    repo::find_by_pk::<Document, _, _>(&db, 1_u64).await.unwrap();
    assert!(db.last().contains(ACTIVE), "find_by_pk: {}", db.last());

    repo::find_by::<Document, _, _>(&db, "title", "x").await.unwrap();
    assert!(db.last().contains(ACTIVE), "find_by: {}", db.last());

    repo::find_one_by::<Document, _, _>(&db, "title", "x").await.unwrap();
    assert!(db.last().contains(ACTIVE), "find_one_by: {}", db.last());

    repo::query::<Document>().all(&db).await.unwrap();
    assert!(db.last().contains(ACTIVE), "query.all: {}", db.last());

    repo::query::<Document>().first(&db).await.unwrap();
    assert!(db.last().contains(ACTIVE), "query.first: {}", db.last());

    repo::query::<Document>().count(&db).await.unwrap();
    assert!(db.last().contains(ACTIVE), "query.count: {}", db.last());

    repo::cursor::<Document, _>(&db, 10).next_page().await.unwrap();
    assert!(db.last().contains(ACTIVE), "cursor: {}", db.last());
}

#[tokio::test]
async fn a_composite_key_lookup_is_scoped_alongside_its_key_predicate() {
    let db = Recorder::new();
    repo::find_by_keys::<Membership, _>(&db, vec![7_u64.into(), 9_u64.into()]).await.unwrap();

    let sql = db.only();
    assert!(sql.contains(r#""team_id" = ?"#), "{sql}");
    assert!(sql.contains(r#""user_id" = ?"#), "{sql}");
    assert!(sql.contains(ACTIVE), "the tombstone predicate joins the key, not replaces it: {sql}");
}

#[tokio::test]
async fn the_predicate_is_qualified_when_the_query_can_join() {
    // The builder that can join qualifies every column it writes, and the
    // tombstone predicate has to be qualified with them: a bare `deleted_at`
    // becomes ambiguous the moment the joined table has one too, and an
    // ambiguous column is an error out of the database rather than a wrong
    // answer — but only on the deployments where the joined table happens to
    // have the column.
    let db = Recorder::new();
    repo::query::<Document>().join("authors", "id", "document_id").all(&db).await.unwrap();

    let sql = db.only();
    assert!(sql.contains(r#""documents"."deleted_at" IS NULL"#), "{sql}");
}

// ---------------------------------------------------------------------------
// the escape hatches
// ---------------------------------------------------------------------------

#[tokio::test]
async fn with_trashed_suppresses_the_predicate_entirely() {
    let db = Recorder::new();
    repo::query::<Document>().where_eq("title", "x").with_trashed().all(&db).await.unwrap();

    let sql = db.only();
    assert!(!sql.contains("IS NULL"), "no predicate at all, not an inverted one: {sql}");
    assert!(sql.contains(r#""title" = ?"#), "the caller's own filters survive: {sql}");
}

#[tokio::test]
async fn only_trashed_inverts_it() {
    let db = Recorder::new();
    repo::query::<Document>().only_trashed().all(&db).await.unwrap();
    assert!(db.only().contains(TRASHED), "{}", db.last());
}

#[tokio::test]
async fn a_cursor_can_walk_the_tombstones_for_a_purge() {
    let db = Recorder::new();
    repo::cursor::<Document, _>(&db, 100).only_trashed().next_page().await.unwrap();
    assert!(db.only().contains(TRASHED), "{}", db.last());

    let db = Recorder::new();
    repo::cursor::<Document, _>(&db, 100).with_trashed().next_page().await.unwrap();
    assert!(!db.only().contains("IS NULL"), "{}", db.last());
}

// ---------------------------------------------------------------------------
// writes are deliberately not scoped
// ---------------------------------------------------------------------------

/// A scoped write breaks the two operations a soft delete exists for.
///
/// *Restore* is an `UPDATE` against a row that is by definition tombstoned;
/// *purge* is a `DELETE` selecting exactly the tombstoned rows. Under a scope
/// each matches nothing — silently, forever, with a plausible `0 rows affected`
/// and a table that never stops growing. That is a worse failure than the one
/// the scope removes, so the writers render the caller's `WHERE` and nothing
/// else.
#[tokio::test]
async fn writes_are_not_scoped_so_restore_and_purge_stay_writable() {
    let db = Recorder::new();

    repo::update(&db, &Document { id: 1, title: "t".into(), deleted_at: None }).await.unwrap();
    assert!(!db.last().contains("IS NULL"), "repo::update: {}", db.last());

    repo::delete_by_pk::<Document, _, _>(&db, 1_u64).await.unwrap();
    assert!(!db.last().contains("IS NULL"), "repo::delete_by_pk: {}", db.last());

    repo::delete_by_keys::<Membership, _>(&db, vec![7_u64.into(), 9_u64.into()]).await.unwrap();
    assert!(!db.last().contains("IS NULL"), "repo::delete_by_keys: {}", db.last());

    // The purge, spelled the way it would actually be written.
    repo::query::<Document>().where_not_null("deleted_at").delete(&db).await.unwrap();
    let purge = db.last();
    assert!(purge.starts_with("DELETE"), "{purge}");
    assert!(!purge.contains("IS NULL"), "a scope here would purge nothing, ever: {purge}");

    // The restore, likewise.
    repo::query::<Document>()
        .where_eq("id", 1_u64)
        .update(&db, vec![("deleted_at", Value::Int(None))])
        .await
        .unwrap();
    let restore = db.last();
    assert!(restore.starts_with("UPDATE"), "{restore}");
    assert!(!restore.contains("IS NULL"), "a scope here would restore nothing: {restore}");

    repo::query::<Document>().where_eq("id", 1_u64).increment(&db, "id", 1).await.unwrap();
    assert!(!db.last().contains("IS NULL"), "increment: {}", db.last());
}

// ---------------------------------------------------------------------------
// the scope does not disturb sharding
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_route_is_still_pinned_by_the_shard_key() {
    let db = Recorder::new();
    repo::find_by_pk::<Token, _, _>(&db, 4242_u64).await.unwrap();

    assert!(db.only().contains(ACTIVE));
    assert_eq!(
        db.routes.borrow().last().copied(),
        Some(ShardRoute::Key(4242)),
        "the added predicate names no shard-encoded column, so it must not move the route"
    );
}

// ---------------------------------------------------------------------------
// the structural guard
// ---------------------------------------------------------------------------

/// Fails if a `SELECT` is built anywhere in the two query modules without going
/// through the scoping helper.
///
/// The behavioural tests above prove that the builders they name are scoped.
/// They cannot prove the *list* is complete, and a builder added later and
/// scoped nowhere is precisely the regression that would not show up: the SQL
/// stays valid, the rows still decode, and one code path quietly returns deleted
/// rows while its neighbours do not. Nothing about a call site says which kind
/// it reached.
///
/// So this reads the source. A new function that constructs a `SELECT` has to
/// either call the helper or be named here with a reason.
#[test]
fn no_select_builder_is_left_unscoped() {
    /// Functions that build a `SELECT` and legitimately do not scope it.
    ///
    /// Empty on purpose. If something belongs here later, the entry needs a
    /// reason beside it — "it isn't over an entity", "it is a write" — because
    /// the whole value of this test is that the list is short enough to read.
    const EXEMPT: &[&str] = &[];

    /// What counts as having been scoped, per module.
    const SCOPED_BY: &[&str] = &["scope_select::<E>", "read_condition()"];

    for (module, source) in
        [("repo.rs", include_str!("../src/repo.rs")), ("query.rs", include_str!("../src/query.rs"))]
    {
        let mut unscoped = Vec::new();

        for (name, body) in functions(source) {
            if !body.contains("::select()") || EXEMPT.contains(&name.as_str()) {
                continue;
            }
            if !SCOPED_BY.iter().any(|marker| body.contains(marker)) {
                unscoped.push(name);
            }
        }

        assert!(
            unscoped.is_empty(),
            "{module} builds a SELECT in {unscoped:?} without applying the soft-delete \
             scope. Call the module's scoping helper, or add the name to EXEMPT in this \
             test with the reason it cannot be scoped."
        );
    }
}

/// Every `fn` in a Rust source, as `(name, body)`.
///
/// Deliberately crude — indentation-delimited rather than brace-balanced —
/// because it only has to be right about `rustfmt`-formatted source from this
/// workspace, and a parser would be a larger thing to trust than what it checks.
fn functions(source: &str) -> Vec<(String, String)> {
    let mut found = Vec::new();
    let mut current: Option<(String, usize, String)> = None;

    for line in source.lines() {
        let indent = line.len() - line.trim_start().len();
        let trimmed = line.trim_start();

        if let Some((_, open_indent, body)) = current.as_mut() {
            // The closing brace of the signature's own block, at its indent.
            if indent == *open_indent && trimmed == "}" {
                let (name, _, body) = current.take().expect("checked above");
                found.push((name, body));
                continue;
            }
            body.push_str(line);
            body.push('\n');
            continue;
        }

        let signature = trimmed
            .strip_prefix("pub ")
            .unwrap_or(trimmed)
            .strip_prefix("pub(crate) ")
            .unwrap_or_else(|| trimmed.strip_prefix("pub ").unwrap_or(trimmed));
        let signature = signature.strip_prefix("async ").unwrap_or(signature);

        if let Some(rest) = signature.strip_prefix("fn ") {
            let name: String =
                rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
            if !name.is_empty() {
                current = Some((name, indent, String::new()));
            }
        }
    }

    found
}
