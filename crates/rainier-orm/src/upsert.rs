//! Insert-or-update in one statement — [`Upsert`].
//!
//! ## Why one statement
//!
//! The obvious way to keep a counter is to read the row, add to it, and write it
//! back. Under any concurrency that loses increments: two callers read the same
//! stored value, both add one, and the second write overwrites the first. The
//! count is simply lower than the truth, no statement errors, and nothing in the
//! result says a write was dropped — so the only way to notice is to already know
//! what the number should have been.
//!
//! An upsert closes that window by making the read and the write the same
//! statement, which the database serialises on the row it is already locking.
//! That is why this exists as a first-class operation rather than as a
//! convenience over [`repo::insert`](crate::repo::insert()) and
//! [`repo::update`](crate::repo::update()).
//!
//! ## Why the conflict target is not optional
//!
//! The three dialects disagree about whether you name the colliding columns:
//!
//! | | rendered |
//! |---|---|
//! | MySQL | `ON DUPLICATE KEY UPDATE n = …` |
//! | SQLite / Postgres | `ON CONFLICT (a, b) DO UPDATE SET n = …` |
//!
//! MySQL infers the key and *rejects* a target; SQLite and Postgres require one
//! for `DO UPDATE`. So a builder that let the caller leave the conflict columns
//! out would render a statement that works on MySQL and is a **syntax error**
//! everywhere else — the exact one-dialect trap this layer exists to remove, and
//! one that a MySQL-backed application would ship without ever seeing.
//!
//! [`Upsert::on`] is therefore the only constructor, and an empty target is
//! refused at render time rather than quietly dropped.
//!
//! ## Why `Increment` is its own action
//!
//! [`UpsertAction::Replace`] writes the value being inserted over the stored
//! one; [`UpsertAction::Increment`] adds it to the stored one. Getting that pair
//! the wrong way round is the same silent-undercount bug as the read-then-write
//! above — a counter that should read the running total instead reads whatever
//! the last caller happened to submit — so they are distinct variants rather
//! than a flag on one, and the dialects spell the difference out differently
//! enough that neither can be written by hand once:
//!
//! | | rendered |
//! |---|---|
//! | MySQL | `n = n + VALUES(n)` |
//! | SQLite / Postgres | `n = "t"."n" + "excluded"."n"` |
//!
//! ```ignore
//! use rainier_orm::{repo, Upsert};
//!
//! // Insert the row, or add this row's `n` to the `n` already stored under the
//! // same `(a, b)` pair.
//! repo::upsert_with(&db, &tally, &Upsert::on(["a", "b"]).increment(["n"])).await?;
//! ```

use crate::{Dialect, Error, Result};
use sea_query::{Alias, Expr, Func, OnConflict, SimpleExpr};

/// What one column does when the insert collides with a stored row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpsertAction {
    /// `col = <the value being inserted>` — last write wins.
    Replace,
    /// `col = <the stored value> + <the value being inserted>` — an accumulating
    /// counter.
    ///
    /// Distinct from [`Replace`](UpsertAction::Replace) because confusing the
    /// two is invisible: both render, both run, both report a row affected, and
    /// the only symptom is a total that is too low. See the module docs.
    Increment,
}

/// An `INSERT … ON CONFLICT` plan: which columns collide, and what each updated
/// column does about it.
///
/// Built through [`on`](Upsert::on) so a conflict target is always supplied —
/// see the module docs for why an inferred one is a MySQL-only statement.
///
/// A plan with no actions at all is **insert-or-ignore**: keep the stored row,
/// discard the incoming one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upsert {
    conflict: Vec<String>,
    /// `(column, action)`, in the order they were declared — which is the order
    /// they are rendered in, so a statement reads the way it was written.
    actions: Vec<(String, UpsertAction)>,
}

impl Upsert {
    /// Conflict on `columns` — the unique key whose collision this handles.
    ///
    /// For a composite key, name every column of it. Naming a subset targets a
    /// different (and probably non-existent) constraint, which SQLite and
    /// Postgres reject outright rather than silently widening.
    pub fn on<I, S>(columns: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self { conflict: columns.into_iter().map(Into::into).collect(), actions: Vec::new() }
    }

    /// On a conflict, overwrite `columns` with the values being inserted.
    pub fn replace<I, S>(mut self, columns: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.actions.extend(columns.into_iter().map(|c| (c.into(), UpsertAction::Replace)));
        self
    }

    /// On a conflict, **add** the values being inserted to the stored ones.
    ///
    /// The accumulating-counter case: `INSERT … VALUES (…, 1)` with
    /// `increment(["n"])` raises the stored `n` by one, atomically, without ever
    /// reading it into the process.
    pub fn increment<I, S>(mut self, columns: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.actions.extend(columns.into_iter().map(|c| (c.into(), UpsertAction::Increment)));
        self
    }

    /// The columns whose collision this plan handles.
    pub fn conflict_columns(&self) -> &[String] {
        &self.conflict
    }

    /// The per-column actions, in declaration order.
    pub fn actions(&self) -> &[(String, UpsertAction)] {
        &self.actions
    }

    /// Whether this plan keeps the stored row untouched on a conflict.
    pub fn is_ignore(&self) -> bool {
        self.actions.is_empty()
    }

    /// Render this plan as a `sea_query` conflict clause for `dialect`.
    ///
    /// `table` qualifies the stored row's side of an increment, and `inserted`
    /// is the column list the `INSERT` actually supplies — both are properties
    /// of the statement being built, not of the plan, so they are arguments
    /// rather than fields.
    ///
    /// # Errors
    ///
    /// - No conflict columns. MySQL would render this fine and SQLite and
    ///   Postgres would not; refusing means the mistake surfaces on whichever
    ///   database the caller happens to develop against, instead of on the one
    ///   they deploy to. See the module docs.
    /// - An action naming a column the `INSERT` does not supply. Every action
    ///   reads the value being inserted (`excluded.col`, or MySQL's
    ///   `VALUES(col)`), so a column that is not in the statement has no such
    ///   value — the usual way to reach this is to name an auto-increment key,
    ///   which the insert deliberately omits.
    pub fn to_on_conflict(
        &self,
        dialect: Dialect,
        table: &str,
        inserted: &[&str],
    ) -> Result<OnConflict> {
        if self.conflict.is_empty() {
            return Err(Error::msg(format!(
                "an upsert on `{table}` names no conflict columns; MySQL infers the \
                 colliding key but SQLite and Postgres require `ON CONFLICT (…)`, so \
                 this would run on one dialect and be a syntax error on the others"
            )));
        }

        for (column, _) in &self.actions {
            if !inserted.contains(&column.as_str()) {
                return Err(Error::msg(format!(
                    "an upsert on `{table}` updates `{column}`, which the INSERT does not \
                     supply; the update reads the value being inserted (`excluded.{column}`, \
                     or `VALUES({column})` on MySQL) and there is none"
                )));
            }
        }

        let mut clause = OnConflict::columns(self.conflict.iter().map(|c| Alias::new(c.as_str())));

        if self.actions.is_empty() {
            // Insert-or-ignore. `do_nothing_on` rather than `do_nothing`: on
            // MySQL the latter renders `ON DUPLICATE KEY IGNORE`, which is not
            // MySQL syntax. Given the key columns it emits a no-op self-update
            // there (`a = a`) and a real `DO NOTHING` on SQLite and Postgres.
            clause.do_nothing_on(self.conflict.iter().map(|c| Alias::new(c.as_str())));
            return Ok(clause);
        }

        for (column, action) in &self.actions {
            match action {
                // sea-query already renders this per dialect — `col =
                // VALUES(col)` on MySQL, `col = "excluded".col` elsewhere — so
                // replacement needs no branch of its own.
                UpsertAction::Replace => {
                    clause.update_column(Alias::new(column.as_str()));
                }
                UpsertAction::Increment => {
                    clause.value(Alias::new(column.as_str()), increment(dialect, table, column));
                }
            }
        }

        Ok(clause)
    }
}

/// `col = <stored> + <incoming>`, spelled for `dialect`.
///
/// The two halves of that sum are the part no shared rendering covers, because
/// sea-query only rewrites `excluded` for a whole-column replacement — an
/// expression passed to `value()` is emitted verbatim on every backend. So the
/// branch is here, once, rather than in each caller:
///
/// - **MySQL** has no `excluded` pseudo-table. Inside `ON DUPLICATE KEY UPDATE`
///   a bare column is the stored value and `VALUES(col)` is the incoming one.
/// - **SQLite and Postgres** expose the incoming row as `excluded`, and the
///   stored one under the table's own name. Qualifying the stored side is not
///   decoration: unqualified, `n` and `excluded.n` look alike enough that a
///   later edit dropping the prefix turns an increment into a self-assignment
///   that still compiles and still runs.
fn increment(dialect: Dialect, table: &str, column: &str) -> SimpleExpr {
    match dialect {
        Dialect::MySql => Expr::col(Alias::new(column))
            .add(Func::cust(Alias::new("VALUES")).arg(Expr::col(Alias::new(column)))),
        Dialect::Sqlite | Dialect::Postgres => Expr::col((Alias::new(table), Alias::new(column)))
            .add(Expr::col((Alias::new("excluded"), Alias::new(column)))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_query::{Query, Value};

    /// Render a minimal `INSERT` carrying only the conflict clause, so a test
    /// asserts on the upsert and not on the rest of a statement.
    fn sql(dialect: Dialect, plan: &Upsert) -> String {
        let inserted = ["a", "b", "n"];
        let mut stmt = Query::insert();
        stmt.into_table(Alias::new("t"));
        stmt.columns(inserted.iter().map(|c| Alias::new(*c)));
        stmt.values_panic([1_i64, 2, 5].map(|v| SimpleExpr::from(Expr::val(Value::from(v)))));
        stmt.on_conflict(plan.to_on_conflict(dialect, "t", &inserted).expect("renders"));

        dialect.build_query(&stmt).0
    }

    #[test]
    fn an_increment_adds_the_incoming_value_to_the_stored_one() {
        let plan = Upsert::on(["a", "b"]).increment(["n"]);

        // The whole point: `n + <incoming>`, not `n = <incoming>`. The dialects
        // spell the incoming half differently and neither may lose the `+`.
        assert_eq!(
            sql(Dialect::MySql, &plan),
            "INSERT INTO `t` (`a`, `b`, `n`) VALUES (?, ?, ?) \
             ON DUPLICATE KEY UPDATE `n` = `n` + VALUES(`n`)"
        );
        assert_eq!(
            sql(Dialect::Sqlite, &plan),
            r#"INSERT INTO "t" ("a", "b", "n") VALUES (?, ?, ?) "#.to_owned()
                + r#"ON CONFLICT ("a", "b") DO UPDATE SET "n" = "t"."n" + "excluded"."n""#
        );
        assert_eq!(
            sql(Dialect::Postgres, &plan),
            r#"INSERT INTO "t" ("a", "b", "n") VALUES ($1, $2, $3) "#.to_owned()
                + r#"ON CONFLICT ("a", "b") DO UPDATE SET "n" = "t"."n" + "excluded"."n""#
        );
    }

    #[test]
    fn an_increment_is_never_a_plain_assignment() {
        // The regression guard for the bug that silently loses counts. If
        // `Increment` ever rendered like `Replace`, every one of these would
        // still be valid SQL and the counter would hold the last write instead
        // of the total — so the assertion is on the *shape*, not on acceptance.
        for dialect in [Dialect::MySql, Dialect::Sqlite, Dialect::Postgres] {
            let incremented = sql(dialect, &Upsert::on(["a", "b"]).increment(["n"]));
            let replaced = sql(dialect, &Upsert::on(["a", "b"]).replace(["n"]));

            assert!(incremented.contains('+'), "{dialect:?} lost the addition: {incremented}");
            assert!(!replaced.contains('+'), "{dialect:?}: a replace must not add: {replaced}");
            assert_ne!(incremented, replaced, "{dialect:?}: an increment is not a replace");
        }
    }

    #[test]
    fn a_replace_overwrites_with_the_incoming_value() {
        let plan = Upsert::on(["a"]).replace(["n"]);

        assert!(sql(Dialect::MySql, &plan).ends_with("ON DUPLICATE KEY UPDATE `n` = VALUES(`n`)"));
        for dialect in [Dialect::Sqlite, Dialect::Postgres] {
            assert!(
                sql(dialect, &plan)
                    .ends_with(r#"ON CONFLICT ("a") DO UPDATE SET "n" = "excluded"."n""#),
                "{dialect:?}: {}",
                sql(dialect, &plan)
            );
        }
    }

    #[test]
    fn a_composite_conflict_target_names_every_column() {
        let plan = Upsert::on(["a", "b"]).replace(["n"]);

        // SQLite and Postgres carry the pair; a target of one column would name
        // a constraint that does not exist.
        for dialect in [Dialect::Sqlite, Dialect::Postgres] {
            assert!(sql(dialect, &plan).contains(r#"ON CONFLICT ("a", "b")"#), "{dialect:?}");
        }
        // MySQL infers it, so there is nothing to carry — and asserting that is
        // what proves the target is dropped rather than emitted where MySQL
        // would reject it.
        let mysql = sql(Dialect::MySql, &plan);
        assert!(mysql.contains("ON DUPLICATE KEY UPDATE"), "{mysql}");
        assert!(!mysql.contains("ON CONFLICT"), "{mysql}");
    }

    #[test]
    fn no_conflict_columns_is_refused_rather_than_rendered() {
        // Rendering this would produce valid MySQL and a syntax error on both
        // other dialects, so it must not reach a builder at all.
        let error = Upsert::on(Vec::<String>::new())
            .increment(["n"])
            .to_on_conflict(Dialect::MySql, "t", &["a", "n"])
            .unwrap_err()
            .to_string();

        assert!(error.contains("conflict columns"), "{error}");
        assert!(error.contains('t'), "the error names the table: {error}");
    }

    #[test]
    fn an_action_on_a_column_the_insert_omits_is_refused() {
        // `id` is not in the insert (an auto-increment key is the DB's job), so
        // `excluded.id` names nothing.
        let error = Upsert::on(["a"])
            .replace(["id"])
            .to_on_conflict(Dialect::Sqlite, "t", &["a", "n"])
            .unwrap_err()
            .to_string();

        assert!(error.contains("id"), "{error}");
    }

    #[test]
    fn no_actions_is_insert_or_ignore() {
        let plan = Upsert::on(["a", "b"]);
        assert!(plan.is_ignore());

        // A real `DO NOTHING` where the dialect has one…
        for dialect in [Dialect::Sqlite, Dialect::Postgres] {
            assert!(sql(dialect, &plan).ends_with(r#"ON CONFLICT ("a", "b") DO NOTHING"#));
        }
        // …and a no-op self-update on MySQL, which has no `DO NOTHING` and
        // whose `IGNORE` spelling is not valid in this position.
        let mysql = sql(Dialect::MySql, &plan);
        assert!(mysql.ends_with("ON DUPLICATE KEY UPDATE `a` = `a`, `b` = `b`"), "{mysql}");
    }

    #[test]
    fn replace_and_increment_compose_in_declaration_order() {
        let plan = Upsert::on(["a"]).replace(["b"]).increment(["n"]);
        let sqlite = sql(Dialect::Sqlite, &plan);

        assert!(
            sqlite
                .ends_with(r#"DO UPDATE SET "b" = "excluded"."b", "n" = "t"."n" + "excluded"."n""#),
            "{sqlite}"
        );
    }

    #[test]
    fn the_inserted_values_stay_bound() {
        // An upsert assembled by pasting a caller's value into the SQL is an
        // injection, so the values must remain placeholders on every dialect.
        let mut stmt = Query::insert();
        stmt.into_table(Alias::new("t"));
        stmt.columns([Alias::new("a"), Alias::new("n")]);
        stmt.values_panic([
            SimpleExpr::from(Expr::val(Value::from("'); DROP TABLE t; --"))),
            SimpleExpr::from(Expr::val(Value::from(5_i64))),
        ]);
        stmt.on_conflict(
            Upsert::on(["a"])
                .increment(["n"])
                .to_on_conflict(Dialect::Sqlite, "t", &["a", "n"])
                .unwrap(),
        );

        let (sql, params) = Dialect::Sqlite.build_query(&stmt);
        assert!(!sql.contains("DROP TABLE"), "the value was interpolated: {sql}");
        assert_eq!(params.0.len(), 2);
    }
}
