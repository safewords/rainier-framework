//! # rainier-validation
//!
//! Validation and **request contracts**: the [`Rule`] set, the [`Validator`]
//! that applies it, and [`FormRequest`] — the contract a controller action
//! declares instead of checking input by hand.
//!
//! ```
//! use rainier_validation::{Rule, Validator};
//! use serde_json::json;
//!
//! let validator = Validator::new()
//!     .rules("email", vec![Rule::Required, Rule::Email])
//!     .rules("password", vec![Rule::Required, Rule::Min(8.0), Rule::Confirmed]);
//!
//! let errors = validator
//!     .validate(&json!({ "email": "ada@example.com", "password": "short" }))
//!     .unwrap_err();
//!
//! assert!(errors.first("password").unwrap().contains("at least 8"));
//! ```
//!
//! ## The three states of a field
//!
//! Absent, null and empty are distinct, and only the presence rules
//! (`Required`, `Present`, `NotNull`) look at the first two. Every other rule
//! skips a field that is not there — so `[Rule::Email]` means "if supplied it
//! must be an email" and `[Rule::Required, Rule::Email]` means "must be
//! supplied, and must be an email". See [`rule`] for the full argument.

#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

pub mod form_request;
pub mod rule;
pub mod validator;

pub use form_request::{field, FormRequest, Validated};
pub use rule::{Outcome, Rule};
pub use validator::{RuleSet, ValidationErrors, Validator};

use serde_json::Value;

/// Read a dotted path out of a JSON tree, indexing arrays numerically.
///
/// Shared by the validator (to find a field) and the cross-field rules (to
/// find the field they compare against).
pub fn lookup<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cursor = root;
    for segment in path.split('.') {
        cursor = match cursor {
            Value::Object(map) => map.get(segment)?,
            Value::Array(items) => items.get(segment.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cursor)
}

// Re-exported so contracts get the attribute macro without adding the
// dependency themselves.
pub use async_trait::async_trait;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn lookup_walks_objects_and_arrays() {
        let value = json!({ "items": [{ "sku": "a" }] });
        assert_eq!(lookup(&value, "items.0.sku"), Some(&json!("a")));
        assert_eq!(lookup(&value, "items.1.sku"), None);
        assert_eq!(lookup(&value, "missing"), None);
        assert_eq!(lookup(&value, "items.0.sku.deeper"), None);
    }
}
