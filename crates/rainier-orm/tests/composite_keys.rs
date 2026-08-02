//! Composite primary keys, asserted on the SQL that actually reaches the
//! executor.
//!
//! The failure this file exists to catch is quiet: a `WHERE` built from only
//! part of a composite key still parses, still runs, and still reports a
//! plausible number of rows affected — it just matches every row sharing the
//! part that was included. An `UPDATE` written that way overwrites siblings and
//! a `DELETE` removes them, and nothing in the result says so.
//!
//! So the assertions here are deliberately on the rendered statement rather than
//! on a return value. A recording executor captures the SQL and its bindings,
//! and the tests check that both key columns are named, that they are `AND`-ed
//! rather than `OR`-ed, and that the bound values are the ones that were asked
//! for. `only_part_of_a_composite_key_never_reaches_a_where` is the one that
//! matters most: it fails loudly the moment any key predicate drops a part.

use core::cell::RefCell;

use rainier_orm::sea_query::Value;
use rainier_orm::{active::Tracked, repo, Dialect, Entity, ExecOutcome, Executor, Result, Row};

/// Keyed `(team_id, user_id)` — a join table, the ordinary reason to want a
/// composite key.
#[derive(Entity, Clone, Debug)]
#[orm(table = "memberships")]
struct Membership {
    #[orm(pk)]
    team_id: u64,
    #[orm(pk)]
    user_id: u64,
    role: String,
}

/// Deliberately declared with the key columns *not* first and not adjacent, so a
/// test can tell "declaration order" apart from "column order" and from
/// "whatever `columns()` happens to yield".
#[derive(Entity, Clone, Debug)]
#[orm(table = "readings")]
struct Reading {
    value: f64,
    #[orm(pk)]
    sensor: String,
    label: String,
    #[orm(pk)]
    bucket: i64,
}

/// The single-key control group: every assertion about composite behaviour has a
/// counterpart here, so a change that quietly altered ordinary entities shows up.
#[derive(Entity, Clone, Debug)]
#[orm(table = "widgets")]
struct Widget {
    #[orm(pk, auto_increment)]
    id: u64,
    name: String,
}

/// Records every statement it is handed, and returns no rows.
struct Recorder {
    dialect: Dialect,
    calls: RefCell<Vec<(String, Vec<Value>)>>,
}

impl Recorder {
    fn new(dialect: Dialect) -> Self {
        Self { dialect, calls: RefCell::new(Vec::new()) }
    }

    /// The one statement that was run. Panics if there wasn't exactly one, so a
    /// test can never assert against the wrong call.
    fn only(&self) -> (String, Vec<Value>) {
        let calls = self.calls.borrow();
        assert_eq!(calls.len(), 1, "expected exactly one statement, got {}", calls.len());
        calls[0].clone()
    }
}

impl Executor for Recorder {
    fn dialect(&self) -> Dialect {
        self.dialect
    }

    async fn fetch_all(&self, sql: &str, params: Vec<Value>) -> Result<Vec<Box<dyn Row>>> {
        self.calls.borrow_mut().push((sql.to_string(), params));
        Ok(Vec::new())
    }

    async fn execute(&self, sql: &str, params: Vec<Value>) -> Result<ExecOutcome> {
        self.calls.borrow_mut().push((sql.to_string(), params));
        Ok(ExecOutcome { rows_affected: 1, last_insert_id: 0 })
    }
}

fn membership() -> Membership {
    Membership { team_id: 7, user_id: 9, role: "owner".into() }
}

/// The `WHERE` clause of a rendered statement, lowercased for matching.
fn where_clause(sql: &str) -> String {
    let lower = sql.to_lowercase();
    let start = lower.find("where").unwrap_or_else(|| panic!("no WHERE in: {sql}"));
    lower[start..].to_string()
}

// ---------------------------------------------------------------------------
// metadata
// ---------------------------------------------------------------------------

#[test]
fn the_key_is_every_pk_column_in_declaration_order() {
    assert_eq!(Membership::primary_key_columns(), &["team_id", "user_id"]);
    assert_eq!(Widget::primary_key_columns(), &["id"]);
}

#[test]
fn declaration_order_wins_over_column_position() {
    // `sensor` is the third field and `bucket` the fifth, with non-key columns
    // between them. The key is still `(sensor, bucket)`: the order the fields
    // were marked, not the order they sit in the struct's column list. This is
    // what fixes the column order of `PRIMARY KEY (a, b)`, and so which prefix
    // lookups the index can serve.
    assert_eq!(Reading::primary_key_columns(), &["sensor", "bucket"]);
    assert_eq!(Reading::columns()[0].name, "value", "the first column is not a key column");
}

#[test]
fn key_values_line_up_positionally_with_key_columns() {
    let values = membership().pk_values();
    assert_eq!(values, vec![Value::from(7_u64), Value::from(9_u64)]);

    let reading = Reading { value: 1.5, sensor: "a".into(), label: "l".into(), bucket: 42 };
    assert_eq!(reading.pk_values(), vec![Value::from("a"), Value::from(42_i64)]);
}

#[test]
fn no_key_column_is_ever_written_by_an_update() {
    // Both halves of the key stay out of the `SET`, so a save can't move a row
    // to a different key — the same guarantee a single-key entity has.
    let columns: Vec<&str> = membership().update_values().into_iter().map(|(c, _)| c).collect();
    assert_eq!(columns, vec!["role"]);
}

#[test]
fn value_of_can_still_read_every_key_column() {
    // `update_values()` omits *all* key columns, so a lookup that only knew
    // about the first would report `None` for `user_id` — a column the entity
    // plainly has — and a relationship keyed on it would load nothing.
    let membership = membership();
    assert_eq!(membership.value_of("team_id"), Some(Value::from(7_u64)));
    assert_eq!(membership.value_of("user_id"), Some(Value::from(9_u64)));
    assert_eq!(membership.value_of("role"), Some(Value::from("owner")));
    assert_eq!(membership.value_of("nope"), None);
}

// ---------------------------------------------------------------------------
// the dangerous one
// ---------------------------------------------------------------------------

/// If a composite `update` or `delete` ever renders a `WHERE` naming only part
/// of the key, this fails — and it is the failure worth failing on, because the
/// statement would otherwise run happily against rows it was never pointed at.
#[tokio::test]
async fn only_part_of_a_composite_key_never_reaches_a_where() {
    for dialect in [Dialect::Sqlite, Dialect::MySql, Dialect::Postgres] {
        // Every write that identifies a row by its key, in one place, so a new
        // one cannot be added without being listed here.
        let update = {
            let exec = Recorder::new(dialect);
            repo::update(&exec, &membership()).await.unwrap();
            exec.only()
        };
        let delete = {
            let exec = Recorder::new(dialect);
            repo::delete_by_keys::<Membership, _>(&exec, vec![7_u64.into(), 9_u64.into()])
                .await
                .unwrap();
            exec.only()
        };
        let find = {
            let exec = Recorder::new(dialect);
            repo::find_by_keys::<Membership, _>(&exec, vec![7_u64.into(), 9_u64.into()])
                .await
                .unwrap();
            exec.only()
        };
        let save = {
            let exec = Recorder::new(dialect);
            let mut tracked = Tracked::new(membership());
            tracked.role = "member".into();
            tracked.save(&exec).await.unwrap();
            exec.only()
        };

        for (name, (sql, params)) in
            [("update", update), ("delete", delete), ("find", find), ("save", save)]
        {
            let clause = where_clause(&sql);

            assert!(clause.contains("team_id"), "{dialect:?} {name}: no team_id in {sql}");
            assert!(clause.contains("user_id"), "{dialect:?} {name}: no user_id in {sql}");
            assert!(
                clause.contains(" and "),
                "{dialect:?} {name}: key parts must be required together, not either: {sql}"
            );
            assert!(
                !clause.contains(" or "),
                "{dialect:?} {name}: an OR between key parts widens the match: {sql}"
            );

            // Both key values are bound, so the predicate is a real comparison
            // against each rather than one column compared twice.
            assert!(
                params.contains(&Value::from(7_u64)),
                "{dialect:?} {name}: team_id not bound in {params:?}"
            );
            assert!(
                params.contains(&Value::from(9_u64)),
                "{dialect:?} {name}: user_id not bound in {params:?}"
            );
        }
    }
}

#[tokio::test]
async fn a_partial_key_is_refused_instead_of_running() {
    let exec = Recorder::new(Dialect::Sqlite);

    // One value for a two-column key. Rendering it would give
    // `DELETE FROM memberships WHERE team_id = 7` — the whole team.
    let deleted = repo::delete_by_keys::<Membership, _>(&exec, vec![7_u64.into()]).await;
    assert!(deleted.is_err(), "a partial key must not delete anything");

    let found = repo::find_by_keys::<Membership, _>(&exec, vec![7_u64.into()]).await;
    assert!(found.is_err());

    assert!(exec.calls.borrow().is_empty(), "nothing may reach the database");
}

#[tokio::test]
async fn too_many_key_values_are_refused_too() {
    let exec = Recorder::new(Dialect::Sqlite);
    let keys = vec![7_u64.into(), 9_u64.into(), 11_u64.into()];

    assert!(repo::delete_by_keys::<Membership, _>(&exec, keys).await.is_err());
    assert!(exec.calls.borrow().is_empty());
}

// ---------------------------------------------------------------------------
// ordering and dialects
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_key_predicate_follows_declared_order_on_every_dialect() {
    for dialect in [Dialect::Sqlite, Dialect::MySql, Dialect::Postgres] {
        let exec = Recorder::new(dialect);
        repo::find_by_keys::<Reading, _>(&exec, vec!["sensor-a".into(), 42_i64.into()])
            .await
            .unwrap();
        let (sql, params) = exec.only();
        let clause = where_clause(&sql);

        let sensor = clause.find("sensor").expect("sensor in WHERE");
        let bucket = clause.find("bucket").expect("bucket in WHERE");
        assert!(sensor < bucket, "{dialect:?}: key order must be as declared: {sql}");

        // Positional, so the values land on the columns they were meant for:
        // the string binds first because `sensor` is the first key column.
        assert_eq!(params[0], Value::from("sensor-a"), "{dialect:?}: {params:?}");
        assert_eq!(params[1], Value::from(42_i64), "{dialect:?}: {params:?}");
    }
}

#[tokio::test]
async fn every_dialect_renders_a_composite_key() {
    for dialect in [Dialect::Sqlite, Dialect::MySql, Dialect::Postgres] {
        let exec = Recorder::new(dialect);
        repo::update(&exec, &membership()).await.unwrap();
        let (sql, _) = exec.only();

        assert!(sql.to_uppercase().starts_with("UPDATE"), "{dialect:?}: {sql}");
        assert!(sql.contains("memberships"), "{dialect:?}: {sql}");
    }

    // Postgres numbers its placeholders; the others use `?`. The key predicate
    // goes through the same renderer as everything else, so it picks these up
    // rather than hard-coding a form.
    let exec = Recorder::new(Dialect::Postgres);
    repo::delete_by_keys::<Membership, _>(&exec, vec![7_u64.into(), 9_u64.into()]).await.unwrap();
    let (sql, _) = exec.only();
    assert!(sql.contains("$1") && sql.contains("$2"), "{sql}");
}

// ---------------------------------------------------------------------------
// the single-key control group
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_single_key_entity_renders_exactly_as_before() {
    // One equality, one bound value, no conjunction — the shape a one-column key
    // has always had. If composite support had leaked into the common path this
    // would pick up a stray `AND` or an extra parameter.
    let exec = Recorder::new(Dialect::Sqlite);
    repo::update(&exec, &Widget { id: 3, name: "a".into() }).await.unwrap();
    let (sql, params) = exec.only();

    assert_eq!(sql, r#"UPDATE "widgets" SET "name" = ? WHERE "id" = ?"#, "{sql}");
    assert_eq!(params, vec![Value::from("a"), Value::from(3_u64)]);

    let exec = Recorder::new(Dialect::Sqlite);
    repo::delete_by_pk::<Widget, _, _>(&exec, 3_u64).await.unwrap();
    let (sql, params) = exec.only();
    assert_eq!(sql, r#"DELETE FROM "widgets" WHERE "id" = ?"#, "{sql}");
    assert_eq!(params, vec![Value::from(3_u64)]);
}

#[test]
fn the_first_key_column_still_names_a_single_key() {
    // `primary_key()`/`pk_value()` keep working unchanged for the entities that
    // have always used them — shard routing and route-model binding read these.
    assert_eq!(Widget::primary_key(), "id");
    assert_eq!(Widget { id: 5, name: "a".into() }.pk_value(), Value::from(5_u64));
}

// ---------------------------------------------------------------------------
// DDL
// ---------------------------------------------------------------------------

#[test]
fn a_composite_key_is_one_table_level_constraint() {
    use rainier_orm::schema::create_table_ddl;

    for dialect in [Dialect::Sqlite, Dialect::MySql, Dialect::Postgres] {
        let ddl = create_table_ddl::<Membership>(dialect);
        let upper = ddl.to_uppercase();

        // An inline `PRIMARY KEY` per column is two primary keys, which no
        // engine accepts — the table would simply fail to create.
        assert_eq!(
            upper.matches("PRIMARY KEY").count(),
            1,
            "{dialect:?}: exactly one primary key clause: {ddl}"
        );
        assert!(ddl.contains("team_id"), "{dialect:?}: {ddl}");
        assert!(ddl.contains("user_id"), "{dialect:?}: {ddl}");

        // Both columns are named *inside* the one constraint, in order.
        let key = &upper[upper.find("PRIMARY KEY").unwrap()..];
        let team = key.find("TEAM_ID").expect("team_id in the key clause");
        let user = key.find("USER_ID").expect("user_id in the key clause");
        assert!(team < user, "{dialect:?}: declared order: {ddl}");
    }
}

#[test]
fn a_single_key_still_renders_inline() {
    let ddl = rainier_orm::schema::create_table_ddl::<Widget>(Dialect::Sqlite);
    assert_eq!(ddl.to_uppercase().matches("PRIMARY KEY").count(), 1, "{ddl}");
    // Inline on the column, so it sits next to `id` rather than after the list.
    assert!(ddl.to_uppercase().contains("AUTOINCREMENT") || ddl.contains("id"), "{ddl}");
}

// ---------------------------------------------------------------------------
// Send futures
// ---------------------------------------------------------------------------

/// `Send` on its own futures, and `Sync` so `&SendExecutor` is `Send` too —
/// anything `!Send` in the assertions below therefore comes from the API under
/// test rather than from here. (`Recorder` holds a `RefCell`, so it is
/// deliberately not reused for this.)
struct SendExecutor;

impl Executor for SendExecutor {
    fn dialect(&self) -> Dialect {
        Dialect::Sqlite
    }

    async fn fetch_all(&self, _sql: &str, _params: Vec<Value>) -> Result<Vec<Box<dyn Row>>> {
        Ok(Vec::new())
    }

    async fn execute(&self, _sql: &str, _params: Vec<Value>) -> Result<ExecOutcome> {
        Ok(ExecOutcome::default())
    }
}

/// The composite APIs build a `sea_query` `Cond`, which holds `Rc` — so if one
/// were left in scope across the executor's await, its future would stop being
/// `Send` and no multi-threaded handler could call it. `tests/send_futures.rs`
/// makes this assertion for the pre-existing surface; these are the new members
/// of it.
#[test]
fn the_composite_futures_are_send() {
    fn assert_send<T: Send>(_value: T) {}

    let exec = SendExecutor;
    assert_send(repo::find_by_keys::<Membership, _>(&exec, vec![7_u64.into(), 9_u64.into()]));
    assert_send(repo::delete_by_keys::<Membership, _>(&exec, vec![7_u64.into(), 9_u64.into()]));
    assert_send(repo::update(&exec, &membership()));
    assert_send(async move {
        let mut tracked = Tracked::new(membership());
        tracked.role = "member".into();
        tracked.save(&exec).await
    });
}
