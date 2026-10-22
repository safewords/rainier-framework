//! A `db.query` span per statement — behind the `telemetry` feature.
//!
//! Applied in [`Database`](crate::Database), the handle every query goes
//! through — repositories, `Criteria`, raw SQL, the migrator — so nothing above
//! it has to know.
//!
//! # The statement, never its values
//!
//! `db.query.text` is the SQL **with its placeholders**, which is what a
//! prepared statement already is: the bound values travel separately and are
//! never recorded. That is not enough on its own, because not every statement
//! is prepared — a raw `statement("DELETE FROM sessions WHERE id = 'abc'")`
//! carries its values inline. So the text is [sanitised](sanitize) as well:
//! every string and numeric literal becomes `?`, which is the OpenTelemetry
//! convention and makes the text a stable grouping key besides.
//!
//! `db.collection.name` and `db.operation.name` are read from the statement —
//! the first keyword, and the table after `FROM`, `INTO`, `UPDATE` or `TABLE`.
//! Best-effort: a `WITH` query or a join reports what it can, and nothing is
//! recorded rather than something wrong.

use rainier_orm::Dialect;

/// The `db.system.name` for a dialect.
pub(crate) fn system(dialect: Dialect) -> &'static str {
    match dialect {
        Dialect::MySql => "mysql",
        Dialect::Postgres => "postgresql",
        Dialect::Sqlite => "sqlite",
    }
}

/// Run `future` inside `span`, recording how it ended.
///
/// `rows` reads a row count off a success, for `db.response.returned_rows`.
pub(crate) async fn traced<T, E: std::fmt::Display, F>(
    span: tracing::Span,
    future: F,
    rows: impl FnOnce(&T) -> Option<usize>,
) -> std::result::Result<T, E>
where
    F: std::future::Future<Output = std::result::Result<T, E>>,
{
    let traced = span.clone();
    let result = tracing::Instrument::instrument(future, span).await;
    match &result {
        Ok(value) => {
            if let Some(count) = rows(value) {
                traced.record("db.response.returned_rows", count);
            }
        }
        // `error.type` only: a driver's message is where a bound value comes
        // back out — "Duplicate entry 'ada@example.com'".
        Err(_) => {
            traced.record("otel.status_code", "ERROR");
            traced.record("error.type", "db_error");
        }
    }
    result
}

/// Open the span for one statement.
pub(crate) fn span(dialect: Dialect, sql: &str) -> tracing::Span {
    let (operation, collection) = describe(sql);
    let name = match (&operation, &collection) {
        (Some(operation), Some(collection)) => format!("{operation} {collection}"),
        (Some(operation), None) => operation.clone(),
        _ => system(dialect).to_string(),
    };

    rainier_support::span!(
        "db.query",
        otel.name = %name,
        otel.kind = "client",
        db.system.name = system(dialect),
        db.operation.name = operation.as_deref().unwrap_or_default(),
        db.collection.name = collection.as_deref().unwrap_or_default(),
        db.query.text = %sanitize(sql),
        db.response.returned_rows = tracing::field::Empty,
    )
}

/// The statement's operation and the table it names, when they can be read.
fn describe(sql: &str) -> (Option<String>, Option<String>) {
    let mut words = sql.split_whitespace();
    let operation = words.next().map(|word| word.to_ascii_uppercase());

    let mut previous = String::new();
    for word in sql.split_whitespace() {
        let upper = word.to_ascii_uppercase();
        if matches!(previous.as_str(), "FROM" | "INTO" | "UPDATE" | "TABLE" | "EXISTS")
            && !matches!(upper.as_str(), "IF" | "NOT" | "EXISTS" | "ONLY")
            && !word.starts_with('(')
        {
            let table: String = word
                .trim_matches(|c: char| matches!(c, '`' | '"' | '[' | ']' | '(' | ')' | ';' | ','))
                .to_string();
            if !table.is_empty() && !table.starts_with('?') && !table.starts_with('$') {
                return (operation, Some(table));
            }
        }
        previous = upper;
    }

    (operation, None)
}

/// The statement with every literal replaced by `?`, at most 2 KiB.
///
/// String literals (`'…'`, with `''` escapes) and numbers standing on their
/// own become `?`; identifiers, placeholders (`?`, `$1`) and quoted names stay.
/// A number inside an identifier — `table2`, `v1_users` — is part of the
/// identifier, not a literal.
pub fn sanitize(sql: &str) -> String {
    const LIMIT: usize = 2048;

    let mut out = String::with_capacity(sql.len().min(LIMIT));
    let chars: Vec<char> = sql.chars().collect();
    let mut index = 0;

    while index < chars.len() && out.len() < LIMIT {
        let c = chars[index];

        if c == '\'' {
            // Skip to the closing quote, treating `''` as an escaped quote.
            index += 1;
            while index < chars.len() {
                if chars[index] == '\'' {
                    if chars.get(index + 1) == Some(&'\'') {
                        index += 2;
                        continue;
                    }
                    break;
                }
                index += 1;
            }
            out.push('?');
            index += 1;
            continue;
        }

        let starts_word = index == 0 || {
            let before = chars[index - 1];
            !(before.is_alphanumeric() || before == '_' || before == '$')
        };
        if c.is_ascii_digit() && starts_word {
            while index < chars.len()
                && (chars[index].is_ascii_alphanumeric() || chars[index] == '.')
            {
                index += 1;
            }
            out.push('?');
            continue;
        }

        out.push(c);
        index += 1;
    }

    if index < chars.len() {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literals_become_placeholders_and_identifiers_survive() {
        assert_eq!(
            sanitize("DELETE FROM sessions WHERE id = 'abc''d' AND age > 30 AND v2_col = ?"),
            "DELETE FROM sessions WHERE id = ? AND age > ? AND v2_col = ?"
        );
        assert_eq!(
            sanitize("SELECT * FROM t WHERE x = $1 LIMIT 10"),
            "SELECT * FROM t WHERE x = $1 LIMIT ?"
        );
        assert_eq!(sanitize("SELECT 1.5"), "SELECT ?");
    }

    #[test]
    fn a_long_statement_is_cut_rather_than_exported_whole() {
        let long = format!("SELECT {}", "a, ".repeat(2000));
        let cut = sanitize(&long);
        assert!(cut.len() <= 2048 + '…'.len_utf8());
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn the_operation_and_table_are_read_from_the_statement() {
        let cases = [
            ("SELECT `id` FROM `users` WHERE `id` = ?", Some("SELECT"), Some("users")),
            ("insert into \"posts\" (title) values ($1)", Some("INSERT"), Some("posts")),
            ("UPDATE jobs SET attempts = attempts + 1", Some("UPDATE"), Some("jobs")),
            ("CREATE TABLE IF NOT EXISTS migrations (id int)", Some("CREATE"), Some("migrations")),
            ("SELECT COUNT(*) AS cnt FROM (SELECT 1) t", Some("SELECT"), None),
        ];
        for (sql, operation, table) in cases {
            let (got_operation, got_table) = describe(sql);
            assert_eq!(got_operation.as_deref(), operation, "{sql}");
            assert_eq!(got_table.as_deref(), table, "{sql}");
        }
    }
}
