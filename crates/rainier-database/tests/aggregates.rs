//! Aggregate queries, and the dialect differences they hide.
//!
//! The reason this capability exists is that the alternative is raw SQL in a
//! handler — a query nothing type-checks, which silently commits to one
//! dialect. `MONTH(x)` is MySQL's spelling and does not exist in SQLite, so a
//! report written that way works in production and 500s in the test suite. The
//! builder is what lets the same query mean the same thing on both.

use rainier_database::{Criteria, DatePart, Projection};
use rainier_orm::{Dialect, Entity};

#[derive(Entity, Clone, Debug)]
#[orm(table = "reports")]
struct Report {
    #[orm(pk)]
    id: u64,
    case_id: Option<u64>,
    created_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// The SQL a criteria renders to on a given dialect.
fn sql(dialect: Dialect, criteria: &Criteria) -> String {
    rainier_database::statement::select_aggregate::<Report>(dialect, criteria).sql
}

fn monthly() -> Criteria {
    Criteria::new()
        .select(Projection::DatePart(DatePart::Month, "created_at".into()), "month")
        .select(Projection::CountAll, "total_count")
        .select(
            Projection::CountWhenIn(
                "report_cases.state".into(),
                vec!["closed".into(), "resolved".into()],
            ),
            "closed_resolved_count",
        )
        .left_join("report_cases", "case_id", "id")
        .group_by(Projection::DatePart(DatePart::Month, "created_at".into()))
        .order_by_alias("month", false)
}

#[test]
fn mysql_extracts_the_month_with_its_own_function() {
    let rendered = sql(Dialect::MySql, &monthly());

    assert!(rendered.contains("MONTH("), "{rendered}");
    assert!(!rendered.contains("strftime"), "{rendered}");
}

#[test]
fn sqlite_uses_strftime_and_casts_it_to_a_number() {
    let rendered = sql(Dialect::Sqlite, &monthly());

    assert!(rendered.contains("strftime"), "{rendered}");
    // The cast is not decoration: `strftime` returns text, and without it the
    // month sorts as "01" < "02" < "10" and compares as a string.
    assert!(rendered.to_uppercase().contains("CAST"), "{rendered}");
    assert!(!rendered.contains("MONTH("), "{rendered}");
}

#[test]
fn postgres_uses_date_part() {
    let rendered = sql(Dialect::Postgres, &monthly());

    assert!(rendered.contains("date_part"), "{rendered}");
}

#[test]
fn the_outer_join_stays_outer() {
    // An inner join here would drop every report whose case is missing, which
    // silently under-reports rather than failing.
    let rendered = sql(Dialect::MySql, &monthly()).to_uppercase();

    assert!(rendered.contains("LEFT JOIN"), "{rendered}");
}

#[test]
fn counting_a_subset_becomes_a_case_inside_the_sum() {
    let rendered = sql(Dialect::MySql, &monthly()).to_uppercase();

    assert!(rendered.contains("SUM("), "{rendered}");
    assert!(rendered.contains("CASE"), "{rendered}");
}

#[test]
fn grouping_and_ordering_survive() {
    let rendered = sql(Dialect::MySql, &monthly()).to_uppercase();

    assert!(rendered.contains("GROUP BY"), "{rendered}");
    assert!(rendered.contains("ORDER BY"), "{rendered}");
}

#[test]
fn without_projections_nothing_changes_for_existing_callers() {
    // `select_matching` is the path every existing query takes, and adding
    // projections to `Criteria` must not have altered it.
    let plain = Criteria::new().where_eq("id", 1);
    let rendered =
        rainier_database::statement::select_matching::<Report>(Dialect::MySql, &plain).sql;

    assert!(rendered.to_uppercase().contains("SELECT"), "{rendered}");
    assert!(!rendered.to_uppercase().contains("GROUP BY"), "{rendered}");
}
