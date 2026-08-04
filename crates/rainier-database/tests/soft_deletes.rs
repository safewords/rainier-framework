//! Automatic soft-delete scoping on the [`Criteria`] path, asserted on the SQL
//! that is actually rendered.
//!
//! `rainier-orm`'s `tests/soft_deletes.rs` covers `repo::` and `Query`. This is
//! the other half, and the half that was missing: every read this framework
//! offers through a `Criteria` — `matching`, `count_matching`,
//! `paginate_matching`, `aggregate`, and every relationship load, which resolves
//! through the far side's `matching` — is built here in `statement`.
//!
//! It was unscoped, and the shape of that bug is why these tests assert on
//! rendered SQL rather than on returned rows. A stub executor returns nothing
//! either way, so "no rows" would pass whether or not the predicate was there.
//!
//! Three failures are in scope, pointing in different directions:
//!
//! - A read that **forgets** the predicate parses, runs, decodes, and puts
//!   deleted rows in front of a user. Nothing about the result says the filter
//!   was missing.
//! - A read that **gains** one where its author meant to see tombstoned rows
//!   returns nothing, just as silently. `with_trashed` and `only_trashed` are
//!   the cure, so they get their own tests.
//! - A read that is scoped **inconsistently** with its neighbour is the worst of
//!   the three, because no call site can see it. `all()` hiding tombstones while
//!   `matching(Criteria::new())` showed them is the state this file exists to
//!   stop returning to, so the `SELECT`/`COUNT` agreement is asserted directly.
//!
//! `no_select_builder_is_left_unscoped` is the structural guard. Its sibling in
//! `rainier-orm` could not have caught the original bug: that one reads its own
//! crate's two query modules, and this module is outside its loop. A structural
//! test that cannot see the module where the bug lives reads as coverage
//! without being any, which is worse than not having one.

use rainier_database::{statement, Criteria, DatePart, Projection};
use rainier_orm::{Dialect, Entity};

/// Soft-deleting: one column marked, so every read of it is scoped.
#[derive(Entity, Clone, Debug)]
#[orm(table = "documents")]
struct Document {
    #[orm(pk, auto_increment)]
    id: u64,
    title: String,
    author_id: u64,
    #[orm(soft_delete)]
    deleted_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Not soft-deleting, and it has a `deleted_at` **on purpose**.
///
/// This is the entity that proves the marker is what scopes a table and the
/// column name is not. A real schema records a deletion date as domain data —
/// when an account was closed, when a document was retracted by its author —
/// and sniffing for the name would silently stop returning most of that table's
/// rows.
#[derive(Entity, Clone, Debug)]
#[orm(table = "widgets")]
struct Widget {
    #[orm(pk, auto_increment)]
    id: u64,
    name: String,
    deleted_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// The predicate a scoped read of `documents` carries.
const LIVE: &str = r#""documents"."deleted_at" IS NULL"#;
/// …and the one `only_trashed` carries instead.
const TRASHED: &str = r#""documents"."deleted_at" IS NOT NULL"#;

// --- the reads a criteria drives -----------------------------------------

#[test]
fn every_criteria_read_is_scoped() {
    // One assertion per builder rather than one loop, so a failure names the
    // builder that regressed instead of an index.
    let criteria = Criteria::new().where_eq("author_id", 7_u64);

    let matching = statement::select_matching::<Document>(Dialect::Sqlite, &criteria);
    assert!(matching.sql.contains(LIVE), "select_matching: {}", matching.sql);

    let counted = statement::count_matching::<Document>(Dialect::Sqlite, &criteria);
    assert!(counted.sql.contains(LIVE), "count_matching: {}", counted.sql);

    let grouped = statement::count_grouped::<Document>(Dialect::Sqlite, "author_id", &criteria);
    assert!(grouped.sql.contains(LIVE), "count_grouped: {}", grouped.sql);

    let aggregate = statement::select_aggregate::<Document>(
        Dialect::Sqlite,
        &Criteria::new()
            .select(Projection::CountAll, "total")
            .group_by(Projection::DatePart(DatePart::Month, "created_at".into())),
    );
    assert!(aggregate.sql.contains(LIVE), "select_aggregate: {}", aggregate.sql);
}

#[test]
fn a_select_and_its_count_agree_about_which_rows_exist() {
    // The pair a paginator issues. Disagreement here is a page that reports a
    // total it cannot produce — and it is the specific shape the original bug
    // took, since one of these was scoped and the other was not.
    let criteria = Criteria::new().where_eq("author_id", 7_u64).limit(20);

    let page = statement::select_matching::<Document>(Dialect::Sqlite, &criteria);
    let total = statement::count_matching::<Document>(Dialect::Sqlite, &criteria);

    assert!(page.sql.contains(LIVE), "{}", page.sql);
    assert!(total.sql.contains(LIVE), "{}", total.sql);
}

#[test]
fn the_criteria_path_and_the_keyed_path_scope_alike() {
    // `all()` against `matching(Criteria::new())`. These are the two ways to ask
    // for every row, and a table that answered them differently is what made the
    // gap invisible from any call site.
    let keyed = statement::select_all::<Document>(Dialect::Sqlite);
    let criteria = statement::select_matching::<Document>(Dialect::Sqlite, &Criteria::new());

    assert!(keyed.sql.contains(LIVE), "select_all: {}", keyed.sql);
    assert!(criteria.sql.contains(LIVE), "select_matching: {}", criteria.sql);
}

#[test]
fn a_relationship_load_is_scoped() {
    // A relation loads the far side through its own repository's `matching`, so
    // this is the statement `HasMany::load`, `BelongsTo::load` and
    // `BelongsToMany::load` all reach. Scoping it is what stops a parent's
    // children coming back tombstoned.
    let load = statement::select_matching::<Document>(
        Dialect::Sqlite,
        &Criteria::new().where_in("author_id", [1_u64, 2, 3]),
    );

    assert!(load.sql.contains(LIVE), "{}", load.sql);
}

// --- saying so on purpose -------------------------------------------------

#[test]
fn with_trashed_suppresses_the_predicate() {
    let sql =
        statement::select_matching::<Document>(Dialect::Sqlite, &Criteria::new().with_trashed())
            .sql;

    // Asserted against the predicates rather than the string `deleted_at`,
    // which is also a selected *column* of this entity and always present.
    assert!(!sql.contains(LIVE), "{sql}");
    assert!(!sql.contains(TRASHED), "{sql}");
}

#[test]
fn only_trashed_selects_the_trash_and_not_the_table() {
    // The leaking direction, and the reason this could not ship undone: while
    // the scope was inert, `only_trashed` returned every **live** row. A trash
    // view built on it listed the whole table.
    let sql =
        statement::select_matching::<Document>(Dialect::Sqlite, &Criteria::new().only_trashed())
            .sql;

    assert!(sql.contains(TRASHED), "{sql}");
    assert!(!sql.contains(LIVE), "the live predicate must not survive alongside it: {sql}");
}

#[test]
fn only_trashed_counts_the_trash_too() {
    // A trash view paginates, so its count has to mean the same thing its page
    // does.
    let sql =
        statement::count_matching::<Document>(Dialect::Sqlite, &Criteria::new().only_trashed()).sql;

    assert!(sql.contains(TRASHED), "{sql}");
}

// --- the entity that never opted in ---------------------------------------

#[test]
fn an_unmarked_entity_renders_exactly_as_it_did() {
    // The guard for every existing caller. `Widget` has a `deleted_at` column
    // and no marker, so none of the three scopes may put a predicate on it —
    // including `only_trashed`, whose `Criteria` form cannot be refused at
    // compile time the way `Query::only_trashed` is, because a `Criteria` does
    // not know which model it will run against.
    //
    // The `WHERE TRUE` is what an empty criteria has always rendered — an empty
    // `Cond::all()`. It is pinned rather than trimmed because this test's job is
    // to notice *any* drift for an entity that never opted in.
    for criteria in [Criteria::new(), Criteria::new().with_trashed()] {
        let sql = statement::select_matching::<Widget>(Dialect::Sqlite, &criteria).sql;
        assert_eq!(
            sql,
            r#"SELECT "widgets"."id", "widgets"."name", "widgets"."deleted_at" FROM "widgets" WHERE TRUE"#,
            "an entity with no marker must build the SQL it always did"
        );
    }
}

#[test]
fn only_trashed_on_an_unmarked_entity_matches_nothing() {
    // The one case where an unmarked entity does *not* render as it always did,
    // and the direction is deliberate. A table that cannot tombstone a row has
    // no tombstoned rows, so the honest answer to "show me the trash" is the
    // empty set.
    //
    // The alternative reading — "no column, so no predicate" — hands a trash
    // view every live row in the table, which is the leaking direction. A
    // `Criteria` cannot refuse the call at compile time the way
    // `Query::only_trashed` does, because it is built without knowing which
    // model it will run against, so the safe answer has to be in the rendering.
    let sql =
        statement::select_matching::<Widget>(Dialect::Sqlite, &Criteria::new().only_trashed()).sql;

    assert!(sql.contains("1 = 0"), "a trash view of a table with no trash is empty: {sql}");
}

// --- writes stay unscoped -------------------------------------------------

#[test]
fn a_delete_is_not_scoped() {
    // "Remove everything tombstoned more than thirty days ago" is a predicate
    // over `deleted_at` and a delete. Under a scope it matches nothing, forever,
    // and the only symptom is a table that never stops growing.
    let sql = statement::delete_matching::<Document>(
        Dialect::Sqlite,
        &Criteria::new().where_not_null("deleted_at"),
    )
    .sql;

    assert!(!sql.contains("IS NULL"), "a purge must be able to see the rows it purges: {sql}");
}

#[test]
fn a_bulk_update_is_not_scoped() {
    // Restoring rows means writing `NULL` over the tombstone of rows that are,
    // by definition, tombstoned. A scope here makes the restore match nothing.
    let sql = statement::update_matching::<Document>(
        Dialect::Sqlite,
        &Criteria::new().where_eq("author_id", 7_u64),
        vec![("deleted_at".to_string(), rainier_orm::sea_query::Value::Int(None))],
    )
    .sql;

    assert!(!sql.contains("IS NULL"), "a bulk restore must reach tombstoned rows: {sql}");
}

// --- the shapes that could have broken the rendering ----------------------

#[test]
fn the_scope_survives_an_or_group() {
    // An `OR` group is a nested condition, and appending the scope to a
    // statement that already holds one must `AND` with the whole group rather
    // than join it as another branch — which would widen the query instead of
    // narrowing it.
    let sql = statement::select_matching::<Document>(
        Dialect::Sqlite,
        &Criteria::new()
            .or_where(|any| any.where_eq("author_id", 1_u64).where_eq("author_id", 2_u64)),
    )
    .sql;

    assert!(sql.contains(LIVE), "{sql}");
    assert!(sql.contains(" AND "), "the scope must be AND-ed with the group: {sql}");
}

#[test]
fn the_predicate_is_qualified_to_the_models_own_table() {
    // A criteria may join, and a bare `deleted_at` is ambiguous the moment the
    // joined table has one too — an error out of the database, and only on the
    // deployments whose schema happens to collide.
    let sql = statement::select_matching::<Document>(
        Dialect::Sqlite,
        &Criteria::new().join("widgets", "author_id", "id"),
    )
    .sql;

    assert!(sql.contains(LIVE), "{sql}");
}

#[test]
fn a_subquery_is_not_scoped_and_says_so() {
    // The one place automatic scoping stops, pinned so it is a documented
    // boundary rather than a surprise.
    //
    // A `Subquery` names its table as a string. There is no `Entity` behind it,
    // so nothing can read a `#[orm(soft_delete)]` marker off it, and scoping it
    // with the *outer* entity's column would be a wrong predicate rather than a
    // missing one. A subquery over a soft-deleting table therefore counts
    // tombstoned rows unless the caller says otherwise — which is one call, on
    // the subquery itself.
    use rainier_database::Subquery;

    let unscoped = statement::select_matching::<Document>(
        Dialect::Sqlite,
        &Criteria::new().where_exists(Subquery::count("comments").correlate("document_id", "id")),
    )
    .sql;
    // Qualified to the subquery's own alias, so this cannot be satisfied by the
    // outer table's predicate sitting elsewhere in the same statement.
    const INNER_LIVE: &str = r#""_rainier_sub"."deleted_at" IS NULL"#;

    assert!(!unscoped.contains(INNER_LIVE), "the inner table is not scoped for you: {unscoped}");
    assert!(unscoped.contains(LIVE), "the outer table still is: {unscoped}");

    let stated = statement::select_matching::<Document>(
        Dialect::Sqlite,
        &Criteria::new().where_exists(
            Subquery::count("comments").correlate("document_id", "id").where_null("deleted_at"),
        ),
    )
    .sql;
    assert!(stated.contains(INNER_LIVE), "saying so is one call: {stated}");
}

#[test]
fn every_dialect_renders_the_scope() {
    for dialect in [Dialect::Sqlite, Dialect::MySql, Dialect::Postgres] {
        let sql = statement::select_matching::<Document>(dialect, &Criteria::new()).sql;
        assert!(
            sql.contains("deleted_at") && sql.to_uppercase().contains("IS NULL"),
            "{dialect:?}: {sql}"
        );
    }
}

// --- the structural guard -------------------------------------------------

/// Every `SELECT` built in `statement.rs` goes through the scoping helper.
///
/// The behavioural tests above prove that the builders they name are scoped.
/// They cannot prove the *list* is complete, and a builder added later and
/// scoped nowhere is precisely the regression that would not show up: the SQL
/// stays valid, the rows still decode, and one code path quietly returns deleted
/// rows while its neighbours do not.
///
/// So this reads the source. A new function that constructs a `SELECT` has to
/// either scope it or be named here with a reason.
#[test]
fn no_select_builder_is_left_unscoped() {
    /// Functions that build a `SELECT` and legitimately do not scope it.
    ///
    /// Short on purpose: the whole value of this test is that the list is
    /// readable, so an entry needs a reason beside it.
    const EXEMPT: &[(&str, &str)] = &[
        // Returns a bare `SELECT` for a caller to add its own `WHERE` to. Every
        // one of those callers is checked by this test in its own right, so
        // scoping here would double the predicate rather than add it.
        ("select_columns", "a helper that hands its statement back unfiltered"),
        // A pivot table has no `Entity`, so it has no marked column and nothing
        // to scope. The rows it links to are scoped when they are loaded.
        ("select_pivot", "not over an entity"),
        // A `Subquery` names its inner table as a **string**, so there is no
        // type to read a marker from. `E` here is the *outer* entity, so
        // scoping this with it would filter the inner table by the outer
        // table's tombstone column — a wrong predicate rather than a missing
        // one. See `a_subquery_is_not_scoped_and_says_so`.
        ("subquery_select", "the inner table is a name, not an entity"),
    ];

    /// What counts as having been scoped.
    ///
    /// `apply_criteria` qualifies because it calls the helper itself, with the
    /// criteria's own scope — which is the seam that was missing. Delegating to
    /// it is therefore proof, but only as long as *it* is scoped, which is
    /// asserted separately below rather than assumed. Accepting the name on
    /// trust is how a guard passes over the exact bug it exists to catch.
    const SCOPED_BY: &[&str] = &["scope_select::<E>", "apply_criteria::<E>"];

    /// What counts as building a `SELECT` in this module.
    const BUILDS_SELECT: &[&str] = &["SqQuery::select()", "select_columns::<E>()"];

    let source = include_str!("../src/statement.rs");
    let mut unscoped = Vec::new();

    for (name, body) in functions(source) {
        if !BUILDS_SELECT.iter().any(|marker| body.contains(marker)) {
            continue;
        }
        if EXEMPT.iter().any(|(exempt, _)| *exempt == name) {
            continue;
        }
        if !SCOPED_BY.iter().any(|marker| body.contains(marker)) {
            unscoped.push(name);
        }
    }

    assert!(
        unscoped.is_empty(),
        "statement.rs builds a SELECT in {unscoped:?} without applying the soft-delete scope. \
         Call `scope_select` (or route through `apply_criteria`), or add the name to EXEMPT in \
         this test with the reason it cannot be scoped."
    );
}

/// The one function the guard above takes on trust, checked directly.
///
/// `SCOPED_BY` accepts a delegation to `apply_criteria` as proof that a builder
/// is scoped. That is only true while `apply_criteria` scopes, and it does not
/// build a `SELECT` of its own — it takes one by reference — so the guard skips
/// it entirely and the marker degrades into a rubber stamp.
///
/// This was not hypothetical: deleting the scope call from `apply_criteria`
/// leaves `no_select_builder_is_left_unscoped` **passing**. Eleven behavioural
/// tests in this file catch it, and the structural one did not, which is the
/// same shape as the original bug — a guard reading as coverage over the exact
/// line it was meant to protect.
#[test]
fn apply_criteria_passes_the_criterias_own_scope() {
    let body = functions(include_str!("../src/statement.rs"))
        .into_iter()
        .find(|(name, _)| name == "apply_criteria")
        .map(|(_, body)| body)
        .expect("apply_criteria should be in statement.rs");

    assert!(
        body.contains("scope_select::<E>(stmt, criteria.trash_scope())"),
        "`apply_criteria` must apply the criteria's own scope — it is the single seam every \
         criteria-driven read passes through, and the marker the structural guard trusts"
    );
}

/// The guard above is only worth anything if it can see the functions it is
/// checking, and its parser is deliberately crude. This pins that it found the
/// ones we know are there — otherwise a parser that silently matched nothing
/// would report a clean sweep of an empty set.
#[test]
fn the_structural_guard_actually_reads_the_builders() {
    let names: Vec<String> =
        functions(include_str!("../src/statement.rs")).into_iter().map(|(name, _)| name).collect();

    for expected in
        ["select_all", "select_by_pk", "select_matching", "count_matching", "apply_criteria"]
    {
        assert!(names.contains(&expected.to_string()), "the parser missed `{expected}`");
    }
}

/// Every `fn` in a Rust source, as `(name, body)`.
///
/// Deliberately crude — indentation-delimited rather than brace-balanced —
/// because it only has to be right about `rustfmt`-formatted source from this
/// workspace, and a parser would be a larger thing to trust than what it checks.
/// The same shape as its sibling in `rainier-orm`, kept separate rather than
/// shared because a test helper crate between the two would be a dependency
/// added to make a forty-line function reusable.
fn functions(source: &str) -> Vec<(String, String)> {
    let mut found = Vec::new();
    let mut current: Option<(String, usize, String)> = None;

    for line in source.lines() {
        let indent = line.len() - line.trim_start().len();
        let trimmed = line.trim_start();

        if let Some((_, open_indent, body)) = current.as_mut() {
            if indent == *open_indent && trimmed == "}" {
                let (name, _, body) = current.take().expect("checked above");
                found.push((name, body));
                continue;
            }
            body.push_str(line);
            body.push('\n');
            continue;
        }

        let signature = trimmed.strip_prefix("pub ").unwrap_or(trimmed);
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
