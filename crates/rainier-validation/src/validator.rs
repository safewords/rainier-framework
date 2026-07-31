//! The [`Validator`] and its [`ValidationErrors`].

use std::collections::BTreeMap;

use rainier_support::Error;
use serde_json::{Map, Value};

use crate::rule::{Outcome, Rule};

/// Field failures, keyed by field name, in field order.
///
/// A `BTreeMap` rather than a `HashMap` so the JSON body a client receives is
/// deterministic — otherwise the same invalid request would produce a
/// different key order each time, which makes responses awkward to snapshot,
/// cache or diff.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ValidationErrors {
    errors: BTreeMap<String, Vec<String>>,
}

impl ValidationErrors {
    /// No failures.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a failure against `field`.
    pub fn add(&mut self, field: impl Into<String>, message: impl Into<String>) {
        self.errors.entry(field.into()).or_default().push(message.into());
    }

    /// Whether anything failed.
    pub fn is_empty(&self) -> bool {
        self.errors.is_empty()
    }

    /// How many fields failed.
    pub fn len(&self) -> usize {
        self.errors.len()
    }

    /// Whether `field` failed.
    pub fn has(&self, field: &str) -> bool {
        self.errors.contains_key(field)
    }

    /// Every message for `field`.
    pub fn get(&self, field: &str) -> &[String] {
        self.errors.get(field).map(Vec::as_slice).unwrap_or(&[])
    }

    /// The first message for `field`.
    pub fn first(&self, field: &str) -> Option<&str> {
        self.errors.get(field)?.first().map(String::as_str)
    }

    /// The first message across every field.
    pub fn first_message(&self) -> Option<&str> {
        self.errors.values().next()?.first().map(String::as_str)
    }

    /// Every message, flattened.
    pub fn all_messages(&self) -> Vec<&str> {
        self.errors.values().flatten().map(String::as_str).collect()
    }

    /// Every failing field name.
    pub fn fields(&self) -> Vec<&str> {
        self.errors.keys().map(String::as_str).collect()
    }

    /// As the JSON payload a client receives: `{"field": ["message", …]}`.
    pub fn to_json(&self) -> Value {
        let mut map = Map::new();
        for (field, messages) in &self.errors {
            map.insert(
                field.clone(),
                Value::Array(messages.iter().cloned().map(Value::String).collect()),
            );
        }
        Value::Object(map)
    }
}

impl From<ValidationErrors> for Error {
    /// A `422` carrying the failures as structured details, which the HTTP
    /// layer renders without knowing anything about validation.
    fn from(errors: ValidationErrors) -> Self {
        Error::validation(errors.to_json())
    }
}

impl std::fmt::Display for ValidationErrors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.first_message() {
            Some(message) => f.write_str(message),
            None => f.write_str("The given data was valid."),
        }
    }
}

/// A set of rules per field.
pub type RuleSet = Vec<(&'static str, Vec<Rule>)>;

/// Checks input against a rule set.
///
/// ```
/// use rainier_validation::{Rule, Validator};
/// use serde_json::json;
///
/// let validator = Validator::new()
///     .rules("email", vec![Rule::Required, Rule::Email])
///     .rules("age", vec![Rule::Integer, Rule::Min(18.0)]);
///
/// let errors = validator.validate(&json!({ "email": "not-an-email" })).unwrap_err();
/// assert_eq!(errors.first("email"), Some("The email field must be a valid email address."));
/// assert!(!errors.has("age"), "an absent optional field does not fail");
/// ```
#[derive(Debug, Default)]
pub struct Validator {
    rules: Vec<(String, Vec<Rule>)>,
    messages: Vec<(String, String)>,
}

impl Validator {
    /// An empty validator.
    pub fn new() -> Self {
        Self::default()
    }

    /// Build from a [`RuleSet`].
    pub fn from_rules(rules: RuleSet) -> Self {
        Self {
            rules: rules.into_iter().map(|(field, rules)| (field.to_string(), rules)).collect(),
            messages: Vec::new(),
        }
    }

    /// Add rules for `field`.
    ///
    /// `field` may be a dotted path (`address.city`) or contain `*` to reach
    /// into an array (`items.*.quantity`).
    pub fn rules(mut self, field: impl Into<String>, rules: Vec<Rule>) -> Self {
        self.rules.push((field.into(), rules));
        self
    }

    /// Override the message for one `field.rule` pair —
    /// `custom_message("email.required", "We need your email.")`.
    pub fn custom_message(mut self, key: impl Into<String>, message: impl Into<String>) -> Self {
        self.messages.push((key.into(), message.into()));
        self
    }

    /// Add several custom messages.
    pub fn custom_messages(
        mut self,
        messages: impl IntoIterator<Item = (impl Into<String>, impl Into<String>)>,
    ) -> Self {
        self.messages
            .extend(messages.into_iter().map(|(key, message)| (key.into(), message.into())));
        self
    }

    /// Validate `input`, returning the failures.
    pub fn validate(&self, input: &Value) -> Result<(), ValidationErrors> {
        let mut errors = ValidationErrors::new();

        for (pattern, rules) in &self.rules {
            for field in expand(pattern, input) {
                let value = crate::lookup(input, &field);

                for rule in rules {
                    match rule.check(&field, value, input) {
                        Outcome::Pass => {}
                        Outcome::Fail(message) => {
                            errors.add(&field, self.message_for(pattern, rule, message));
                            // A failed presence rule makes every later rule
                            // for this field meaningless — "is required" plus
                            // "must be a valid email" is noise, not detail.
                            if rule.applies_to_missing() {
                                break;
                            }
                        }
                        // Absent or null, and nothing demanded otherwise.
                        Outcome::Skip => break,
                    }
                }
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// Validate and hand back the input, for chaining.
    pub fn validated(&self, input: &Value) -> Result<Value, ValidationErrors> {
        self.validate(input)?;
        Ok(input.clone())
    }

    /// Validate and keep only the fields that had rules.
    ///
    /// This is a security feature, not a convenience: it is what stops a
    /// client from smuggling an `is_admin` field into a mass assignment. Only
    /// keys you wrote a rule for survive.
    pub fn validated_only(&self, input: &Value) -> Result<Value, ValidationErrors> {
        self.validate(input)?;

        let mut kept = Map::new();
        for (pattern, _) in &self.rules {
            // The top-level key of the pattern: `address.city` keeps
            // `address` wholesale, since a nested rule implies the parent.
            let root = pattern.split(['.', '*']).next().unwrap_or(pattern);
            if root.is_empty() || kept.contains_key(root) {
                continue;
            }
            if let Some(value) = input.get(root) {
                kept.insert(root.to_string(), value.clone());
            }
        }
        Ok(Value::Object(kept))
    }

    fn message_for(&self, pattern: &str, rule: &Rule, default: String) -> String {
        let key = format!("{pattern}.{}", rule.name());
        self.messages
            .iter()
            .find(|(candidate, _)| *candidate == key)
            .map(|(_, message)| message.clone())
            .unwrap_or(default)
    }
}

/// Expand a `*` wildcard in `pattern` against `input`.
///
/// `items.*.name` with three items becomes `items.0.name`, `items.1.name`,
/// `items.2.name`. A pattern with no `*` expands to itself.
fn expand(pattern: &str, input: &Value) -> Vec<String> {
    if !pattern.contains('*') {
        return vec![pattern.to_string()];
    }

    let mut expanded = vec![String::new()];
    for segment in pattern.split('.') {
        let mut next = Vec::new();
        for prefix in &expanded {
            if segment != "*" {
                next.push(join(prefix, segment));
                continue;
            }
            // Fan out over however many elements are actually there. An
            // absent or non-array parent contributes nothing, so rules under
            // it simply do not run — use a rule on the parent to require it.
            let Some(Value::Array(items)) = crate::lookup(input, prefix) else {
                continue;
            };
            for index in 0..items.len() {
                next.push(join(prefix, &index.to_string()));
            }
        }
        expanded = next;
    }
    expanded
}

fn join(prefix: &str, segment: &str) -> String {
    if prefix.is_empty() {
        segment.to_string()
    } else {
        format!("{prefix}.{segment}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn validator() -> Validator {
        Validator::new()
            .rules("name", vec![Rule::Required, Rule::String, Rule::Max(10.0)])
            .rules("email", vec![Rule::Required, Rule::Email])
            .rules("age", vec![Rule::Integer, Rule::Min(18.0)])
    }

    #[test]
    fn valid_input_passes() {
        let input = json!({ "name": "Ada", "email": "ada@example.com", "age": "36" });
        assert!(validator().validate(&input).is_ok());
    }

    #[test]
    fn optional_fields_may_be_absent() {
        let input = json!({ "name": "Ada", "email": "ada@example.com" });
        assert!(validator().validate(&input).is_ok());
    }

    #[test]
    fn collects_a_failure_per_field() {
        let input = json!({ "email": "nope", "age": "12" });
        let errors = validator().validate(&input).unwrap_err();

        assert_eq!(errors.len(), 3);
        assert!(errors.has("name"));
        assert!(errors.has("email"));
        assert!(errors.has("age"));
    }

    #[test]
    fn a_failed_required_rule_silences_the_rest_for_that_field() {
        let input = json!({ "email": "" });
        let errors = validator().validate(&input).unwrap_err();

        // Just "is required" — not also "must be a valid email address".
        assert_eq!(errors.get("email").len(), 1);
        assert!(errors.first("email").unwrap().contains("required"));
    }

    #[test]
    fn non_presence_failures_accumulate() {
        let validator = Validator::new().rules("code", vec![Rule::Alpha, Rule::Min(5.0)]);
        let errors = validator.validate(&json!({ "code": "a1" })).unwrap_err();
        assert_eq!(errors.get("code").len(), 2);
    }

    #[test]
    fn custom_messages_replace_the_default() {
        let validator = validator().custom_message("email.required", "We need your email.");
        let errors = validator.validate(&json!({ "name": "Ada" })).unwrap_err();
        assert_eq!(errors.first("email"), Some("We need your email."));
    }

    #[test]
    fn a_custom_message_only_replaces_its_own_rule() {
        let validator = validator().custom_message("email.required", "We need your email.");
        let errors = validator.validate(&json!({ "name": "Ada", "email": "x" })).unwrap_err();
        assert!(errors.first("email").unwrap().contains("valid email address"));
    }

    #[test]
    fn dotted_paths_reach_into_objects() {
        let validator = Validator::new().rules("address.city", vec![Rule::Required]);
        assert!(validator.validate(&json!({ "address": { "city": "Paris" } })).is_ok());

        let errors = validator.validate(&json!({ "address": {} })).unwrap_err();
        assert!(errors.has("address.city"));
    }

    #[test]
    fn wildcards_fan_out_over_arrays() {
        let validator = Validator::new()
            .rules("items.*.quantity", vec![Rule::Required, Rule::Integer, Rule::Min(1.0)]);

        let input = json!({ "items": [{ "quantity": "2" }, { "quantity": "0" }, {}] });
        let errors = validator.validate(&input).unwrap_err();

        assert!(!errors.has("items.0.quantity"));
        assert!(errors.has("items.1.quantity"), "0 is below the minimum");
        assert!(errors.has("items.2.quantity"), "missing entirely");
    }

    #[test]
    fn a_wildcard_over_a_missing_array_produces_no_failures() {
        let validator = Validator::new().rules("items.*.quantity", vec![Rule::Required]);
        assert!(validator.validate(&json!({})).is_ok());
    }

    #[test]
    fn nested_wildcards_expand() {
        let validator = Validator::new().rules("orders.*.items.*.sku", vec![Rule::Required]);
        let input = json!({
            "orders": [
                { "items": [{ "sku": "a" }, {}] },
                { "items": [{ "sku": "c" }] }
            ]
        });
        let errors = validator.validate(&input).unwrap_err();
        assert_eq!(errors.fields(), vec!["orders.0.items.1.sku"]);
    }

    #[test]
    fn validated_only_drops_unvalidated_keys() {
        let input = json!({
            "name": "Ada",
            "email": "ada@example.com",
            "is_admin": true
        });
        let kept = validator().validated_only(&input).unwrap();

        assert_eq!(kept.get("name"), Some(&json!("Ada")));
        assert!(kept.get("is_admin").is_none(), "mass assignment must not slip through");
    }

    #[test]
    fn validated_only_keeps_the_parent_of_a_nested_rule() {
        let validator = Validator::new()
            .rules("address.city", vec![Rule::Required])
            .rules("items.*.sku", vec![Rule::Required]);

        let input = json!({
            "address": { "city": "Paris", "postcode": "75001" },
            "items": [{ "sku": "a" }],
            "sneaky": 1
        });
        let kept = validator.validated_only(&input).unwrap();

        assert!(kept.get("address").is_some());
        assert!(kept.get("items").is_some());
        assert!(kept.get("sneaky").is_none());
    }

    #[test]
    fn errors_render_as_a_stable_json_payload() {
        let errors = validator().validate(&json!({})).unwrap_err();
        let payload = errors.to_json();

        // BTreeMap ordering: email before name.
        let keys: Vec<&String> = payload.as_object().unwrap().keys().collect();
        assert_eq!(keys, vec!["email", "name"]);
        assert!(payload["name"][0].as_str().unwrap().contains("required"));
    }

    #[test]
    fn errors_convert_into_a_422() {
        let errors = validator().validate(&json!({})).unwrap_err();
        let error: Error = errors.into();
        assert_eq!(error.status(), 422);
        assert!(error.details().is_some());
    }

    #[test]
    fn error_accessors() {
        let errors = validator().validate(&json!({})).unwrap_err();
        assert!(!errors.is_empty());
        assert!(errors.first_message().is_some());
        assert_eq!(errors.all_messages().len(), 2);
        assert!(errors.get("nope").is_empty());
        assert!(errors.first("nope").is_none());
    }

    #[test]
    fn from_rules_builds_from_a_rule_set() {
        let rules: RuleSet = vec![("title", vec![Rule::Required])];
        let validator = Validator::from_rules(rules);
        assert!(validator.validate(&json!({})).is_err());
        assert!(validator.validate(&json!({ "title": "x" })).is_ok());
    }
}
