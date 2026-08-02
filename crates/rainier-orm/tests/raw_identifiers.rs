//! A column whose name is a Rust keyword.
//!
//! `type`, `match`, `ref`, `box`, `move` — all unremarkable column names, all
//! keywords in Rust. The only way to spell one as a field is the raw form,
//! `pub r#type`, and `Ident::to_string()` keeps the `r#`.
//!
//! Left alone, the derive names the column `r#type` and every query touching it
//! fails at run time with `no such column: r#type` — on every dialect, since no
//! database has a column by that name. The failure is invisible until the query
//! runs, because the struct compiles perfectly.

use rainier_orm::Entity;

#[derive(Entity, Clone, Debug)]
#[orm(table = "posts")]
struct Post {
    #[orm(pk)]
    id: u64,
    /// The keyword case this test exists for.
    r#type: String,
    /// An ordinary field, so the test would notice a fix that broke everything
    /// else in the pursuit of the one above.
    title: String,
}

#[test]
fn a_raw_identifier_names_the_column_without_its_prefix() {
    let columns: Vec<&str> = <Post as Entity>::columns().iter().map(|c| c.name).collect();

    assert!(
        columns.contains(&"type"),
        "the column should be `type`, not `r#type` — got {columns:?}"
    );
    assert!(
        !columns.iter().any(|c| c.starts_with("r#")),
        "no column name should carry a raw-identifier prefix — got {columns:?}"
    );
}

#[test]
fn ordinary_fields_are_unaffected() {
    let columns: Vec<&str> = <Post as Entity>::columns().iter().map(|c| c.name).collect();

    assert!(columns.contains(&"id"), "{columns:?}");
    assert!(columns.contains(&"title"), "{columns:?}");
}
