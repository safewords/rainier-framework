//! End-to-end proof that a declared connection setting is the one the *server*
//! ends up using.
//!
//! The unit tests in `databases.rs` assert what a declaration renders — which
//! is the right tool for asserting a parameter is spelled the way the driver
//! parses it, and the wrong tool for asserting the driver did anything with it.
//! `charset` and `strict` are the two settings whose whole point is what
//! happens to a value after it arrives, and the only place that can be checked
//! is a database.
//!
//! MySQL only, and only when `TEST_DATABASE_URL` names one — there is nothing
//! here SQLite can stand in for. The rest of the suite runs without it.
#![cfg(feature = "sea-orm-executor")]

use rainier_database::{DatabaseConfig, Databases, ServerDatabase};

/// The connection under test, from `TEST_DATABASE_URL`, as discrete fields.
///
/// Taken apart rather than passed through as a DSN because these settings are
/// declared beside a host, and a DSN would refuse all but one of them — which
/// is the arrangement being checked.
fn declared() -> Option<ServerDatabase> {
    let url = std::env::var("TEST_DATABASE_URL").ok()?;
    let rest = url.strip_prefix("mysql://").or_else(|| url.strip_prefix("mariadb://"))?;

    let (userinfo, rest) = rest.rsplit_once('@')?;
    let (authority, database) = rest.split_once('/')?;
    let database = database.split(['?', '#']).next().unwrap_or(database);

    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (host, port.parse::<u16>().ok()),
        None => (authority, None),
    };

    let mut server = ServerDatabase::mysql(database).host(host);
    if let Some(port) = port {
        server = server.port(port);
    }
    server = match userinfo.split_once(':') {
        Some((user, password)) => server.credentials(user, password),
        None => server.user(userinfo),
    };
    Some(server)
}

#[tokio::test]
async fn a_declared_charset_is_the_one_the_server_negotiates() {
    let Some(server) = declared() else {
        eprintln!("TEST_DATABASE_URL unset — skipping MySQL connection-settings check");
        return;
    };

    // Four bytes wide. MySQL's own `utf8` is three, and a connection on it
    // stores a truncated or replaced value for anything outside the BMP without
    // failing the write.
    let manager = Databases::new("primary")
        .with("primary", server.charset("utf8mb4").collation("utf8mb4_unicode_ci"))
        .build()
        .await
        .expect("open the declared connection");
    let database = manager.default_connection();

    let charset = database
        .query("SELECT @@character_set_client AS value")
        .scalar_string("value")
        .await
        .expect("select");
    assert_eq!(
        charset.as_deref(),
        Some("utf8mb4"),
        "the declared charset did not reach the server"
    );

    let collation = database
        .query("SELECT @@collation_connection AS value")
        .scalar_string("value")
        .await
        .expect("select");
    assert_eq!(collation.as_deref(), Some("utf8mb4_unicode_ci"));

    // And the thing the setting exists for: a four-byte character survives the
    // round trip instead of being cut off at it.
    let text = database
        .query("SELECT CONCAT('a', _utf8mb4 0xF09F9880, 'b') AS value")
        .scalar_string("value")
        .await
        .expect("select")
        .expect("not null");
    assert_eq!(text.chars().count(), 3, "a four-byte character was truncated: {text:?}");
}

#[tokio::test]
async fn strict_mode_reaches_every_connection_in_the_pool() {
    let Some(server) = declared() else {
        eprintln!("TEST_DATABASE_URL unset — skipping MySQL strict-mode check");
        return;
    };

    for (declaration, expected) in
        [(server.clone().strict(true), true), (server.clone().strict(false), false)]
    {
        let manager = Databases::new("primary")
            .with("primary", declaration)
            .build()
            .await
            .expect("open the declared connection");

        // Ten reads, so the pool hands out more than the connection the first
        // one opened. This is the assertion that fails if the statement is
        // issued once through the pool rather than attached to the pool's
        // connect hook — one connection would answer strict and the rest would
        // answer whatever the server's parameter group says.
        for _ in 0..10 {
            let modes = manager
                .default_connection()
                .query("SELECT @@SESSION.sql_mode AS value")
                .scalar_string("value")
                .await
                .expect("select")
                .expect("not null");

            assert_eq!(
                modes.split(',').any(|mode| mode == "STRICT_ALL_TABLES"),
                expected,
                "sql_mode was `{modes}`"
            );
            // The modes that are nobody's business here survived the edit: the
            // driver appends this one itself before the hook runs.
            assert!(
                modes.split(',').any(|mode| mode == "NO_ENGINE_SUBSTITUTION"),
                "an unrelated mode was dropped: {modes}"
            );
        }
    }
}

#[tokio::test]
async fn strict_decides_whether_an_over_long_value_errors_or_is_stored_short() {
    let Some(server) = declared() else {
        eprintln!("TEST_DATABASE_URL unset — skipping MySQL truncation check");
        return;
    };

    // What the setting is actually for. Reading `@@sql_mode` says the mode is
    // on; this says the mode does the thing it exists to do, which is the
    // assertion that survives somebody changing which modes get named.
    let strict = Databases::new("primary")
        .with("primary", server.clone().strict(true))
        .build()
        .await
        .expect("open");
    let lenient = Databases::new("primary")
        .with("primary", server.strict(false))
        .build()
        .await
        .expect("open");

    let table = "rainier_strict_probe";
    for database in [strict.default_connection(), lenient.default_connection()] {
        database.statement(&format!("DROP TABLE IF EXISTS {table}")).await.expect("drop");
    }
    strict
        .default_connection()
        .statement(&format!("CREATE TABLE {table} (value VARCHAR(4) NOT NULL)"))
        .await
        .expect("create");

    // Five characters into four. Non-strict stores `overl` as `over` and
    // reports success; strict refuses.
    let refused = strict
        .default_connection()
        .query(format!("INSERT INTO {table} (value) VALUES (?)"))
        .bind("overlong")
        .execute()
        .await;
    assert!(refused.is_err(), "a strict connection stored an over-long value");

    lenient
        .default_connection()
        .query(format!("INSERT INTO {table} (value) VALUES (?)"))
        .bind("overlong")
        .execute()
        .await
        .expect("a non-strict connection truncates rather than failing");

    let stored = lenient
        .default_connection()
        .query(format!("SELECT value FROM {table}"))
        .scalar_string("value")
        .await
        .expect("select")
        .expect("not null");
    assert_eq!(stored, "over", "the write that succeeded stored something else");

    strict.default_connection().statement(&format!("DROP TABLE {table}")).await.expect("drop");
}

#[tokio::test]
async fn a_declaration_that_names_no_setting_opens_exactly_as_it_did_before() {
    let Some(server) = declared() else {
        eprintln!("TEST_DATABASE_URL unset — skipping MySQL baseline check");
        return;
    };

    // The path every existing deployment takes. It has no session statements,
    // so it goes through the driver's own connector rather than the hand-built
    // pool the strict path uses — and this is what says so.
    let config = DatabaseConfig::from(server);
    assert!(config.session_statements().is_empty());

    let manager = Databases::new("primary").with("primary", config).build().await.expect("open");
    manager.default_connection().statement("SELECT 1").await.expect("query");
}
