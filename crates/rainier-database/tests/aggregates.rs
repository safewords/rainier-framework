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

#[test]
fn a_calendar_date_is_not_a_day_of_month() {
    // `DateOf` truncates to YYYY-MM-DD; `DatePart(Day)` extracts 1–31. Grouping
    // a daily chart by the latter silently adds January's 3rd to February's.
    let by_date = Criteria::new()
        .select(Projection::DateOf("created_at".into()), "day")
        .group_by(Projection::DateOf("created_at".into()));

    let mysql = sql(Dialect::MySql, &by_date).to_uppercase();
    assert!(mysql.contains("CAST") && mysql.contains("DATE"), "{mysql}");

    let sqlite = sql(Dialect::Sqlite, &by_date);
    assert!(sqlite.contains("date("), "{sqlite}");
}

#[test]
fn an_or_group_is_parenthesised_and_anded_with_the_rest() {
    // The precedence is the whole point: `a AND (b OR c)` keeps `a` required,
    // where `a AND b OR c` would return rows matching only `c`.
    let criteria = Criteria::new()
        .where_eq("id", 1)
        .or_where(|any| any.where_like("case_id", "%x%").where_like("id", "%y%"));

    let rendered = sql(Dialect::MySql, &criteria.select(Projection::CountAll, "n"));
    let upper = rendered.to_uppercase();

    assert!(upper.contains(" OR "), "{rendered}");
    assert!(upper.contains(" AND "), "{rendered}");
    assert!(rendered.contains('('), "the OR must be grouped: {rendered}");
}

#[test]
fn an_empty_or_group_is_ignored_rather_than_rendering_an_empty_paren() {
    let criteria =
        Criteria::new().where_eq("id", 1).or_where(|any| any).select(Projection::CountAll, "n");

    let rendered = sql(Dialect::MySql, &criteria);
    assert!(!rendered.contains("()"), "{rendered}");
}

#[test]
fn case_insensitive_equality_lowers_both_sides() {
    // MySQL's usual collations compare text case-insensitively; SQLite and
    // Postgres do not. A plain equality on a username therefore behaves
    // differently depending on the database behind it — an application ported
    // from MySQL keeps working in production and stops finding rows in its own
    // test suite. Lowering both sides is identical on all three.
    let criteria = Criteria::new().where_eq_ci("case_id", "AdA").select(Projection::CountAll, "n");

    for dialect in [Dialect::MySql, Dialect::Sqlite, Dialect::Postgres] {
        let rendered = sql(dialect, &criteria).to_uppercase();
        assert_eq!(rendered.matches("LOWER").count(), 2, "{dialect:?}: {rendered}");
    }
}

#[test]
fn distinct_is_rendered_when_asked_for() {
    let plain = Criteria::new().select(Projection::Column("case_id".into()), "case_id");
    assert!(!sql(Dialect::MySql, &plain).to_uppercase().contains("DISTINCT"));

    let deduped = plain.distinct();
    assert!(sql(Dialect::MySql, &deduped).to_uppercase().contains("DISTINCT"));
}
