//! `#[orm(sensitive)]`: a model field whose value must never be displayed.
//!
//! The derive reports the field under both of its spellings, and a repository
//! for the model makes them known process-wide (`rainier_support::sensitive`)
//! — which is what Lookout asks before it shows a payload.

use rainier_database::testing::MemoryConnection;
use rainier_database::{Database, Dialect, Entity, EntityRepository};

#[derive(Entity, Clone, Debug)]
#[orm(table = "sensitive_accounts")]
struct Account {
    #[orm(pk)]
    id: u64,
    email: String,
    #[orm(sensitive, column = "sensitive_test_recovery_hash")]
    recovery: String,
    #[orm(sensitive)]
    sensitive_test_pin: String,
}

#[test]
fn the_derive_lists_column_and_field_names() {
    assert_eq!(
        Account::sensitive_columns(),
        &["sensitive_test_recovery_hash", "recovery", "sensitive_test_pin"]
    );
}

#[test]
fn a_repository_marks_them_for_the_process() {
    use rainier_support::sensitive::is_marked;

    let db = Database::new(MemoryConnection::new(Dialect::Sqlite));
    let _accounts = EntityRepository::<Account>::new(db);

    assert!(is_marked("sensitive_test_recovery_hash"));
    assert!(is_marked("recovery"));
    assert!(is_marked("SENSITIVE_TEST_PIN"));
    assert!(!is_marked("email"));
}
