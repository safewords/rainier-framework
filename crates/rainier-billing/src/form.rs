//! Stripe's request encoding — [`Params`] and [`encode`].
//!
//! Stripe does not take JSON. Every write is
//! `application/x-www-form-urlencoded`, with nesting spelled in brackets:
//!
//! ```text
//! line_items[0][price]=price_123&line_items[0][quantity]=2&metadata[order]=42
//! ```
//!
//! Parameters are held as a JSON object and flattened at the edge, which is
//! the honest representation: Stripe's documentation describes every
//! parameter as a JSON-shaped tree, the escape hatch takes one, and the typed
//! builders build one. One encoder means one place for the rules:
//!
//! | value | encoded as |
//! |---|---|
//! | object | `key[child]=…`, recursively |
//! | array | `key[0]=…`, `key[1]=…` — indexed, never `key[]`, because an array of objects needs the index to keep each object's fields together |
//! | `true` / `false` | `true` / `false` |
//! | `null` | the empty string, which is how Stripe spells "unset this field" |
//!
//! An **empty** array or object encodes as `key=` — the same "unset", and the
//! only way to clear `metadata` or remove every tax rate.

use serde::Serialize;
use serde_json::{Map, Value};

/// Parameters for one Stripe call.
///
/// A JSON object, plus the two things that are request **options** rather
/// than parameters and so must never be encoded into the body: the
/// idempotency key and the connected account.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Params {
    pub(crate) values: Map<String, Value>,
    pub(crate) idempotency_key: Option<String>,
    pub(crate) account: Option<String>,
}

impl Params {
    /// No parameters.
    pub fn new() -> Self {
        Self::default()
    }

    /// Parameters from any serialisable value that serialises to an object —
    /// `json!({...})`, a struct, a map. Anything else is an empty set.
    pub fn from_value(value: impl Serialize) -> Self {
        match serde_json::to_value(value) {
            Ok(Value::Object(values)) => Self { values, ..Self::default() },
            _ => Self::default(),
        }
    }

    /// Set a top-level parameter. `null` unsets the field on Stripe's side.
    pub fn set(mut self, key: impl Into<String>, value: impl Serialize) -> Self {
        self.insert(key, value);
        self
    }

    /// Set a parameter only when there is one — what every optional builder
    /// field does, so `None` is absent rather than an explicit unset.
    pub fn set_opt<T: Serialize>(self, key: impl Into<String>, value: Option<T>) -> Self {
        match value {
            Some(value) => self.set(key, value),
            None => self,
        }
    }

    /// Set a parameter in place.
    pub fn insert(&mut self, key: impl Into<String>, value: impl Serialize) {
        let value = serde_json::to_value(value).unwrap_or(Value::Null);
        self.values.insert(key.into(), value);
    }

    /// One `metadata[key]=value` entry.
    pub fn metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        let entry = self.values.entry("metadata").or_insert_with(|| Value::Object(Map::new()));
        if let Value::Object(map) = entry {
            map.insert(key.into(), Value::String(value.into()));
        }
        self
    }

    /// Ask Stripe to inline a related object — `expand[]=customer`.
    pub fn expand(mut self, path: impl Into<String>) -> Self {
        let entry = self.values.entry("expand").or_insert_with(|| Value::Array(Vec::new()));
        if let Value::Array(list) = entry {
            list.push(Value::String(path.into()));
        }
        self
    }

    /// Use this idempotency key rather than a generated one.
    ///
    /// A generated key protects one HTTP call from a network retry. It does
    /// **not** protect a queued job that runs twice, because the second run
    /// generates a second key — so a job creating a charge should derive the
    /// key from its own record (`"order-42-charge"`), and pass it here.
    pub fn idempotency_key(mut self, key: impl Into<String>) -> Self {
        self.idempotency_key = Some(key.into());
        self
    }

    /// Make this call on behalf of a connected account — `Stripe-Account`.
    pub fn on_account(mut self, account: impl Into<String>) -> Self {
        self.account = Some(account.into());
        self
    }

    /// Merge another set over this one. The other's options win when set.
    pub fn merge(mut self, other: Params) -> Self {
        self.values.extend(other.values);
        if other.idempotency_key.is_some() {
            self.idempotency_key = other.idempotency_key;
        }
        if other.account.is_some() {
            self.account = other.account;
        }
        self
    }

    /// A parameter's current value.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.values.get(key)
    }

    /// Whether there are no parameters.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// The flattened `(key, value)` pairs, in a stable order.
    pub fn pairs(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for (key, value) in &self.values {
            flatten(key, value, &mut out);
        }
        out
    }

    /// The encoded body or query string.
    pub fn encode(&self) -> String {
        encode(&self.pairs())
    }
}

impl From<Value> for Params {
    fn from(value: Value) -> Self {
        Self::from_value(value)
    }
}

/// Flatten one value under `prefix` into bracketed pairs.
fn flatten(prefix: &str, value: &Value, out: &mut Vec<(String, String)>) {
    match value {
        Value::Object(map) if map.is_empty() => out.push((prefix.to_string(), String::new())),
        Value::Object(map) => {
            for (key, child) in map {
                flatten(&format!("{prefix}[{key}]"), child, out);
            }
        }
        Value::Array(list) if list.is_empty() => out.push((prefix.to_string(), String::new())),
        Value::Array(list) => {
            for (index, child) in list.iter().enumerate() {
                flatten(&format!("{prefix}[{index}]"), child, out);
            }
        }
        Value::Null => out.push((prefix.to_string(), String::new())),
        Value::Bool(flag) => out.push((prefix.to_string(), flag.to_string())),
        Value::Number(number) => out.push((prefix.to_string(), number.to_string())),
        Value::String(text) => out.push((prefix.to_string(), text.clone())),
    }
}

/// `application/x-www-form-urlencoded` of the pairs.
pub fn encode(pairs: &[(String, String)]) -> String {
    let mut serializer = form_urlencoded::Serializer::new(String::new());
    for (key, value) in pairs {
        serializer.append_pair(key, value);
    }
    serializer.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pairs(params: &Params) -> Vec<(String, String)> {
        params.pairs()
    }

    fn pair(key: &str, value: &str) -> (String, String) {
        (key.to_string(), value.to_string())
    }

    #[test]
    fn nesting_is_spelled_in_brackets_and_arrays_are_indexed() {
        let params = Params::from_value(json!({
            "mode": "subscription",
            "line_items": [
                { "price": "price_a", "quantity": 2 },
                { "price": "price_b", "quantity": 1 },
            ],
            "subscription_data": { "trial_period_days": 14 },
        }));

        let encoded = pairs(&params);
        assert!(encoded.contains(&pair("mode", "subscription")));
        assert!(encoded.contains(&pair("line_items[0][price]", "price_a")));
        assert!(encoded.contains(&pair("line_items[0][quantity]", "2")));
        assert!(encoded.contains(&pair("line_items[1][price]", "price_b")));
        assert!(encoded.contains(&pair("subscription_data[trial_period_days]", "14")));
    }

    #[test]
    fn metadata_and_expand_accumulate() {
        let params = Params::new()
            .metadata("order", "42")
            .metadata("source", "web")
            .expand("customer")
            .expand("latest_invoice.payment_intent");

        let encoded = pairs(&params);
        assert!(encoded.contains(&pair("metadata[order]", "42")));
        assert!(encoded.contains(&pair("metadata[source]", "web")));
        assert!(encoded.contains(&pair("expand[0]", "customer")));
        assert!(encoded.contains(&pair("expand[1]", "latest_invoice.payment_intent")));
    }

    #[test]
    fn null_and_empty_collections_are_how_stripe_spells_unset() {
        let params = Params::from_value(json!({
            "description": null,
            "metadata": {},
            "default_tax_rates": [],
        }));

        let encoded = pairs(&params);
        assert!(encoded.contains(&pair("description", "")));
        assert!(encoded.contains(&pair("metadata", "")));
        assert!(encoded.contains(&pair("default_tax_rates", "")));
    }

    #[test]
    fn booleans_are_words() {
        let params = Params::new().set("allow_promotion_codes", true).set("livemode", false);
        assert!(pairs(&params).contains(&pair("allow_promotion_codes", "true")));
        assert!(pairs(&params).contains(&pair("livemode", "false")));
    }

    #[test]
    fn the_encoding_escapes_brackets_and_reserved_characters() {
        let params = Params::new().metadata("note", "a&b=c d");
        assert_eq!(params.encode(), "metadata%5Bnote%5D=a%26b%3Dc+d");
    }

    #[test]
    fn options_never_reach_the_body() {
        let params = Params::new().set("email", "a@b.co").idempotency_key("k").on_account("acct_1");
        assert_eq!(params.encode(), "email=a%40b.co");
        assert_eq!(params.idempotency_key.as_deref(), Some("k"));
        assert_eq!(params.account.as_deref(), Some("acct_1"));
    }

    #[test]
    fn absent_optionals_are_absent_rather_than_unset() {
        let params = Params::new().set_opt::<String>("email", None).set_opt("name", Some("Ada"));
        assert_eq!(params.encode(), "name=Ada");
    }

    #[test]
    fn a_merge_keeps_both_and_the_later_wins() {
        let merged = Params::new()
            .set("a", 1)
            .set("b", 1)
            .idempotency_key("first")
            .merge(Params::new().set("b", 2).idempotency_key("second"));

        assert_eq!(merged.get("a"), Some(&json!(1)));
        assert_eq!(merged.get("b"), Some(&json!(2)));
        assert_eq!(merged.idempotency_key.as_deref(), Some("second"));
    }
}
