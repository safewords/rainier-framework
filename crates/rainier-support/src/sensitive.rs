//! Field names whose values must never be shown — the process-wide list.
//!
//! What a model field marked `#[orm(sensitive)]` or a form request's
//! [`sensitive`](../../rainier_validation/trait.FormRequest.html#method.sensitive)
//! fields end up in, and what anything that displays a payload — Lookout,
//! chiefly — asks before it shows a value:
//!
//! ```
//! use rainier_support::sensitive;
//!
//! sensitive::mark(["recovery_phrase"]);
//! assert!(sensitive::is_marked("recovery_phrase"));
//! assert!(sensitive::is_marked("Recovery_Phrase"), "case-insensitive");
//! assert!(!sensitive::is_marked("display_name"));
//! ```
//!
//! Names, not paths: a name marked anywhere is hidden everywhere, at any depth
//! of any payload. That errs towards hiding, which is the direction to err in
//! — a `password` field is a password whatever object it is in.
//!
//! Marking is cheap to repeat: a name already present costs a read lock.

use std::collections::BTreeSet;
use std::sync::{OnceLock, RwLock};

fn names() -> &'static RwLock<BTreeSet<String>> {
    static NAMES: OnceLock<RwLock<BTreeSet<String>>> = OnceLock::new();
    NAMES.get_or_init(Default::default)
}

/// Mark field names as sensitive, for the life of the process.
pub fn mark<S: AsRef<str>>(fields: impl IntoIterator<Item = S>) {
    let fields: Vec<String> = fields.into_iter().map(|f| f.as_ref().to_ascii_lowercase()).collect();
    if fields.is_empty() {
        return;
    }
    {
        let known = names().read().unwrap_or_else(|e| e.into_inner());
        if fields.iter().all(|f| known.contains(f)) {
            return;
        }
    }
    names().write().unwrap_or_else(|e| e.into_inner()).extend(fields);
}

/// Whether `field` has been marked sensitive. Case-insensitive.
pub fn is_marked(field: &str) -> bool {
    let known = names().read().unwrap_or_else(|e| e.into_inner());
    if known.is_empty() {
        return false;
    }
    known.contains(&field.to_ascii_lowercase())
}

/// Every name marked so far, lower-cased and sorted.
pub fn marked() -> Vec<String> {
    names().read().unwrap_or_else(|e| e.into_inner()).iter().cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marking_twice_is_one_name() {
        mark(["sensitive_test_twice", "SENSITIVE_TEST_TWICE"]);
        mark(["sensitive_test_twice"]);
        assert_eq!(marked().iter().filter(|n| *n == "sensitive_test_twice").count(), 1);
    }

    #[test]
    fn marking_nothing_is_fine() {
        mark(Vec::<&str>::new());
        assert!(!is_marked(""));
    }
}
