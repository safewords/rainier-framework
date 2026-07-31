//! Validation rules — [`Rule`] and the built-in set.
//!
//! A rule inspects one field's value and either passes or explains why not.
//! Rules are values rather than strings (`Rule::Email`, not `"email"`), so a
//! typo is a compile error and a rule that takes an argument takes a *typed*
//! argument.
//!
//! ## Present, null, and empty are three different things
//!
//! This trips people up in every validation library, so the rules here are
//! explicit about it:
//!
//! - **Absent** — the key is not in the input at all.
//! - **Null** — the key is present with a `null` value (which is what
//!   `ConvertEmptyStringsToNull` turns a blank text input into).
//! - **Empty** — present, non-null, but `""` or `[]`.
//!
//! [`Rule::Required`] rejects all three. Every *other* rule **skips** absent
//! and null values, so `Rule::Email` on an optional field does not fire when
//! the field was left out. That is what makes `[Rule::Email]` mean "if
//! supplied, it must be an email" and `[Rule::Required, Rule::Email]` mean
//! "must be supplied, and must be an email".

use std::sync::Arc;

use chrono::NaiveDate;
use serde_json::Value;

/// What a rule concluded about one value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The value is acceptable.
    Pass,
    /// The value is not acceptable, for this reason.
    Fail(String),
    /// The value is absent or null and this rule has nothing to say, so the
    /// remaining rules for the field should be skipped too.
    Skip,
}

/// One validation rule.
#[derive(Clone)]
pub enum Rule {
    /// Present, not null, and not empty.
    Required,
    /// Present (may be null or empty).
    Present,
    /// Not null when present.
    NotNull,
    /// A string (or a value that reads as one).
    String,
    /// An integer.
    Integer,
    /// A number, integer or not.
    Numeric,
    /// A boolean, or one of the strings a form sends for one.
    Boolean,
    /// An array.
    Array,
    /// At least this length: characters for a string, elements for an array,
    /// value for a number.
    Min(f64),
    /// At most this length, by the same measure as [`Rule::Min`].
    Max(f64),
    /// Exactly this length.
    Size(f64),
    /// Between these bounds, inclusive.
    Between(f64, f64),
    /// A plausible email address.
    Email,
    /// An absolute `http`/`https` URL.
    Url,
    /// Letters, digits, `-` and `_`.
    Slug,
    /// A canonical UUID.
    Uuid,
    /// ASCII letters only.
    Alpha,
    /// ASCII letters and digits.
    AlphaNumeric,
    /// Letters, digits, `-` and `_`.
    AlphaDash,
    /// One of these values.
    In(Vec<String>),
    /// None of these values.
    NotIn(Vec<String>),
    /// Starts with this prefix.
    StartsWith(String),
    /// Ends with this suffix.
    EndsWith(String),
    /// A `YYYY-MM-DD` date.
    Date,
    /// A date on or after this one.
    After(NaiveDate),
    /// A date on or before this one.
    Before(NaiveDate),
    /// Equal to another field's value — `password_confirmation`.
    Confirmed,
    /// Equal to the named field's value.
    Same(String),
    /// Different from the named field's value.
    Different(String),
    /// Any predicate, with the message to use when it fails.
    Custom {
        /// Shown when the predicate returns `false`.
        message: String,
        /// The predicate.
        check: Arc<dyn Fn(&Value) -> bool + Send + Sync>,
    },
}

impl std::fmt::Debug for Rule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Rule::Custom { message, .. } => write!(f, "Custom({message:?})"),
            other => f.write_str(other.name()),
        }
    }
}

impl Rule {
    /// The rule's name, as it appears in a failure message.
    pub fn name(&self) -> &'static str {
        match self {
            Rule::Required => "required",
            Rule::Present => "present",
            Rule::NotNull => "not_null",
            Rule::String => "string",
            Rule::Integer => "integer",
            Rule::Numeric => "numeric",
            Rule::Boolean => "boolean",
            Rule::Array => "array",
            Rule::Min(_) => "min",
            Rule::Max(_) => "max",
            Rule::Size(_) => "size",
            Rule::Between(_, _) => "between",
            Rule::Email => "email",
            Rule::Url => "url",
            Rule::Slug => "slug",
            Rule::Uuid => "uuid",
            Rule::Alpha => "alpha",
            Rule::AlphaNumeric => "alpha_num",
            Rule::AlphaDash => "alpha_dash",
            Rule::In(_) => "in",
            Rule::NotIn(_) => "not_in",
            Rule::StartsWith(_) => "starts_with",
            Rule::EndsWith(_) => "ends_with",
            Rule::Date => "date",
            Rule::After(_) => "after",
            Rule::Before(_) => "before",
            Rule::Confirmed => "confirmed",
            Rule::Same(_) => "same",
            Rule::Different(_) => "different",
            Rule::Custom { .. } => "custom",
        }
    }

    /// Whether this rule cares about a value that is absent or null.
    ///
    /// Only the presence rules do. Everything else defers to them, which is
    /// what makes optional fields work without a rule combinator.
    pub fn applies_to_missing(&self) -> bool {
        matches!(self, Rule::Required | Rule::Present | Rule::NotNull)
    }

    /// Check `value` (the field being validated) against this rule.
    ///
    /// `field` names it for the message, and `all` is the whole input, which
    /// the cross-field rules need.
    pub fn check(&self, field: &str, value: Option<&Value>, all: &Value) -> Outcome {
        // Presence first: every other rule sits this one out.
        match self {
            Rule::Required => {
                return if is_filled(value) {
                    Outcome::Pass
                } else {
                    Outcome::Fail(format!("The {} field is required.", label(field)))
                };
            }
            Rule::Present => {
                return if value.is_some() {
                    Outcome::Pass
                } else {
                    Outcome::Fail(format!("The {} field must be present.", label(field)))
                };
            }
            Rule::NotNull => {
                return match value {
                    Some(Value::Null) => {
                        Outcome::Fail(format!("The {} field must not be null.", label(field)))
                    }
                    _ => Outcome::Pass,
                };
            }
            _ => {}
        }

        let Some(value) = value else { return Outcome::Skip };
        if value.is_null() {
            return Outcome::Skip;
        }

        let name = label(field);
        match self {
            Rule::Required | Rule::Present | Rule::NotNull => Outcome::Pass,

            Rule::String => check(value.is_string(), format!("The {name} field must be a string.")),
            Rule::Integer => {
                check(as_integer(value).is_some(), format!("The {name} field must be an integer."))
            }
            Rule::Numeric => {
                check(as_number(value).is_some(), format!("The {name} field must be a number."))
            }
            Rule::Boolean => {
                check(as_bool(value).is_some(), format!("The {name} field must be true or false."))
            }
            Rule::Array => check(value.is_array(), format!("The {name} field must be an array.")),

            Rule::Min(min) => match measure(value) {
                Measure::Number(n) => check(
                    n >= *min,
                    format!("The {name} field must be at least {}.", trim_float(*min)),
                ),
                Measure::Length(len) => check(
                    len as f64 >= *min,
                    format!(
                        "The {name} field must be at least {} {}.",
                        trim_float(*min),
                        unit(value)
                    ),
                ),
            },
            Rule::Max(max) => match measure(value) {
                Measure::Number(n) => check(
                    n <= *max,
                    format!("The {name} field must not be greater than {}.", trim_float(*max)),
                ),
                Measure::Length(len) => check(
                    len as f64 <= *max,
                    format!(
                        "The {name} field must not be greater than {} {}.",
                        trim_float(*max),
                        unit(value)
                    ),
                ),
            },
            Rule::Size(size) => match measure(value) {
                Measure::Number(n) => check(
                    (n - *size).abs() < f64::EPSILON,
                    format!("The {name} field must be {}.", trim_float(*size)),
                ),
                Measure::Length(len) => check(
                    len as f64 == *size,
                    format!("The {name} field must be {} {}.", trim_float(*size), unit(value)),
                ),
            },
            Rule::Between(low, high) => {
                let actual = match measure(value) {
                    Measure::Number(n) => n,
                    Measure::Length(len) => len as f64,
                };
                check(
                    actual >= *low && actual <= *high,
                    format!(
                        "The {name} field must be between {} and {}.",
                        trim_float(*low),
                        trim_float(*high)
                    ),
                )
            }

            Rule::Email => check(
                text(value).is_some_and(|v| is_email(&v)),
                format!("The {name} field must be a valid email address."),
            ),
            Rule::Url => check(
                text(value).is_some_and(|v| is_url(&v)),
                format!("The {name} field must be a valid URL."),
            ),
            Rule::Slug => check(
                text(value).is_some_and(|v| {
                    !v.is_empty()
                        && v.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                }),
                format!("The {name} field must be a valid slug."),
            ),
            Rule::Uuid => check(
                text(value).is_some_and(|v| is_uuid(&v)),
                format!("The {name} field must be a valid UUID."),
            ),
            Rule::Alpha => check(
                text(value)
                    .is_some_and(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_alphabetic())),
                format!("The {name} field must only contain letters."),
            ),
            Rule::AlphaNumeric => check(
                text(value)
                    .is_some_and(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_alphanumeric())),
                format!("The {name} field must only contain letters and numbers."),
            ),
            Rule::AlphaDash => check(
                text(value).is_some_and(|v| {
                    !v.is_empty()
                        && v.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                }),
                format!(
                    "The {name} field must only contain letters, numbers, dashes and underscores."
                ),
            ),

            Rule::In(allowed) => check(
                text(value).is_some_and(|v| allowed.contains(&v)),
                format!("The selected {name} is invalid."),
            ),
            Rule::NotIn(denied) => check(
                text(value).is_some_and(|v| !denied.contains(&v)),
                format!("The selected {name} is invalid."),
            ),
            Rule::StartsWith(prefix) => check(
                text(value).is_some_and(|v| v.starts_with(prefix.as_str())),
                format!("The {name} field must start with {prefix}."),
            ),
            Rule::EndsWith(suffix) => check(
                text(value).is_some_and(|v| v.ends_with(suffix.as_str())),
                format!("The {name} field must end with {suffix}."),
            ),

            Rule::Date => check(
                text(value).is_some_and(|v| parse_date(&v).is_some()),
                format!("The {name} field must be a valid date."),
            ),
            Rule::After(bound) => check(
                text(value).and_then(|v| parse_date(&v)).is_some_and(|d| d > *bound),
                format!("The {name} field must be a date after {bound}."),
            ),
            Rule::Before(bound) => check(
                text(value).and_then(|v| parse_date(&v)).is_some_and(|d| d < *bound),
                format!("The {name} field must be a date before {bound}."),
            ),

            Rule::Confirmed => {
                let other = format!("{field}_confirmation");
                check(
                    crate::lookup(all, &other) == Some(value),
                    format!("The {name} field confirmation does not match."),
                )
            }
            Rule::Same(other) => check(
                crate::lookup(all, other) == Some(value),
                format!("The {name} field must match {}.", label(other)),
            ),
            Rule::Different(other) => check(
                crate::lookup(all, other) != Some(value),
                format!("The {name} field must be different from {}.", label(other)),
            ),

            Rule::Custom { message, check: predicate } => check(predicate(value), message.clone()),
        }
    }
}

fn check(passed: bool, message: String) -> Outcome {
    if passed {
        Outcome::Pass
    } else {
        Outcome::Fail(message)
    }
}

/// Whether a value counts as "filled" for [`Rule::Required`].
fn is_filled(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::String(s)) => !s.trim().is_empty(),
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::Object(map)) => !map.is_empty(),
        Some(_) => true,
    }
}

/// What [`Rule::Min`] and friends measure: a number's magnitude, or a string's
/// or array's length.
enum Measure {
    Number(f64),
    Length(usize),
}

fn measure(value: &Value) -> Measure {
    match value {
        Value::Number(n) => Measure::Number(n.as_f64().unwrap_or(0.0)),
        Value::String(s) => match s.parse::<f64>() {
            // A numeric string is measured as a number, so `min:18` on a form
            // field means "at least eighteen", not "at least two digits".
            Ok(n) => Measure::Number(n),
            Err(_) => Measure::Length(s.chars().count()),
        },
        Value::Array(items) => Measure::Length(items.len()),
        Value::Object(map) => Measure::Length(map.len()),
        Value::Bool(_) | Value::Null => Measure::Length(0),
    }
}

fn unit(value: &Value) -> &'static str {
    match value {
        Value::Array(_) | Value::Object(_) => "items",
        _ => "characters",
    }
}

fn trim_float(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

/// A field's human-readable name: `email_address` → `email address`.
fn label(field: &str) -> String {
    field.replace(['_', '.'], " ")
}

fn text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn as_number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn as_integer(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn as_bool(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(b) => Some(*b),
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "on" | "yes" => Some(true),
            "false" | "0" | "off" | "no" => Some(false),
            _ => None,
        },
        Value::Number(n) => n
            .as_i64()
            .map(|i| i == 0 || i == 1)
            .and_then(|ok| ok.then(|| n.as_i64().expect("just read") != 0)),
        _ => None,
    }
}

/// A pragmatic email check.
///
/// Deliberately **not** RFC 5322: that grammar accepts addresses no mail
/// provider will, and rejecting a valid-but-unusual address annoys a real user
/// far more than accepting an invalid one costs. The bar here is "one `@`, a
/// non-empty local part, and a domain with a dot" — after that, send a
/// confirmation email, which is the only real check.
fn is_email(value: &str) -> bool {
    let Some((local, domain)) = value.split_once('@') else {
        return false;
    };
    if local.is_empty() || domain.is_empty() || value.contains(' ') {
        return false;
    }
    if domain.contains('@') || domain.starts_with('.') || domain.ends_with('.') {
        return false;
    }
    match domain.split_once('.') {
        Some((head, tail)) => !head.is_empty() && !tail.is_empty() && !tail.contains(' '),
        None => false,
    }
}

fn is_url(value: &str) -> bool {
    let rest = value.strip_prefix("https://").or_else(|| value.strip_prefix("http://"));
    match rest {
        Some(rest) => !rest.is_empty() && !rest.starts_with('/') && !rest.contains(' '),
        None => false,
    }
}

fn is_uuid(value: &str) -> bool {
    let groups = [8, 4, 4, 4, 12];
    let mut parts = value.split('-');
    for expected in groups {
        let Some(part) = parts.next() else { return false };
        if part.len() != expected || !part.bytes().all(|b| b.is_ascii_hexdigit()) {
            return false;
        }
    }
    parts.next().is_none()
}

fn parse_date(value: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(value.trim(), "%Y-%m-%d").ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn run(rule: &Rule, value: Value) -> Outcome {
        let all = json!({});
        rule.check("field", Some(&value), &all)
    }

    fn passes(rule: &Rule, value: Value) -> bool {
        run(rule, value) == Outcome::Pass
    }

    fn message(rule: &Rule, field: &str, value: Value) -> String {
        match rule.check(field, Some(&value), &json!({})) {
            Outcome::Fail(message) => message,
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    #[test]
    fn required_rejects_absent_null_and_blank() {
        let all = json!({});
        assert!(matches!(Rule::Required.check("f", None, &all), Outcome::Fail(_)));
        assert!(matches!(Rule::Required.check("f", Some(&json!(null)), &all), Outcome::Fail(_)));
        assert!(matches!(Rule::Required.check("f", Some(&json!("")), &all), Outcome::Fail(_)));
        assert!(matches!(Rule::Required.check("f", Some(&json!("   ")), &all), Outcome::Fail(_)));
        assert!(matches!(Rule::Required.check("f", Some(&json!([])), &all), Outcome::Fail(_)));
        assert_eq!(Rule::Required.check("f", Some(&json!("x")), &all), Outcome::Pass);
        assert_eq!(Rule::Required.check("f", Some(&json!(0)), &all), Outcome::Pass);
        assert_eq!(Rule::Required.check("f", Some(&json!(false)), &all), Outcome::Pass);
    }

    #[test]
    fn present_only_asks_whether_the_key_exists() {
        let all = json!({});
        assert!(matches!(Rule::Present.check("f", None, &all), Outcome::Fail(_)));
        assert_eq!(Rule::Present.check("f", Some(&json!(null)), &all), Outcome::Pass);
        assert_eq!(Rule::Present.check("f", Some(&json!("")), &all), Outcome::Pass);
    }

    #[test]
    fn other_rules_skip_absent_and_null_values() {
        let all = json!({});
        assert_eq!(Rule::Email.check("f", None, &all), Outcome::Skip);
        assert_eq!(Rule::Email.check("f", Some(&json!(null)), &all), Outcome::Skip);
        assert_eq!(Rule::Integer.check("f", None, &all), Outcome::Skip);
    }

    #[test]
    fn type_rules() {
        assert!(passes(&Rule::String, json!("x")));
        assert!(!passes(&Rule::String, json!(1)));

        assert!(passes(&Rule::Integer, json!(5)));
        assert!(passes(&Rule::Integer, json!("5")), "a form sends numbers as strings");
        assert!(!passes(&Rule::Integer, json!("5.5")));
        assert!(!passes(&Rule::Integer, json!("abc")));

        assert!(passes(&Rule::Numeric, json!("5.5")));
        assert!(passes(&Rule::Numeric, json!(5)));
        assert!(!passes(&Rule::Numeric, json!("abc")));

        assert!(passes(&Rule::Boolean, json!(true)));
        assert!(passes(&Rule::Boolean, json!("on")));
        assert!(passes(&Rule::Boolean, json!("0")));
        assert!(!passes(&Rule::Boolean, json!("maybe")));

        assert!(passes(&Rule::Array, json!([1])));
        assert!(!passes(&Rule::Array, json!("x")));
    }

    #[test]
    fn size_rules_measure_numbers_by_value_and_text_by_length() {
        assert!(passes(&Rule::Min(3.0), json!("abc")));
        assert!(!passes(&Rule::Min(3.0), json!("ab")));

        // A numeric string is measured as a number: `min:18` on an age field.
        assert!(passes(&Rule::Min(18.0), json!("21")));
        assert!(!passes(&Rule::Min(18.0), json!("17")));

        assert!(passes(&Rule::Max(2.0), json!([1, 2])));
        assert!(!passes(&Rule::Max(2.0), json!([1, 2, 3])));

        assert!(passes(&Rule::Size(4.0), json!("abcd")));
        assert!(passes(&Rule::Between(1.0, 3.0), json!("ab")));
        assert!(!passes(&Rule::Between(1.0, 3.0), json!("abcd")));
    }

    #[test]
    fn size_messages_name_the_right_unit() {
        assert!(message(&Rule::Max(2.0), "tags", json!([1, 2, 3])).contains("2 items"));
        assert!(message(&Rule::Max(2.0), "name", json!("abc")).contains("2 characters"));
    }

    #[test]
    fn email_accepts_the_ordinary_and_rejects_the_obviously_broken() {
        for good in ["a@b.co", "first.last+tag@sub.example.com", "x@y.z"] {
            assert!(passes(&Rule::Email, json!(good)), "{good} should pass");
        }
        for bad in ["", "plain", "@b.co", "a@", "a@b", "a b@c.co", "a@b@c.co", "a@.co", "a@b."] {
            assert!(!passes(&Rule::Email, json!(bad)), "{bad} should fail");
        }
    }

    #[test]
    fn url_requires_a_scheme_and_a_host() {
        assert!(passes(&Rule::Url, json!("https://example.com")));
        assert!(passes(&Rule::Url, json!("http://example.com/a?b=1")));
        assert!(!passes(&Rule::Url, json!("example.com")));
        assert!(!passes(&Rule::Url, json!("https://")));
        assert!(!passes(&Rule::Url, json!("ftp://example.com")));
        assert!(!passes(&Rule::Url, json!("https:// example.com")));
    }

    #[test]
    fn character_class_rules() {
        assert!(passes(&Rule::Alpha, json!("abc")));
        assert!(!passes(&Rule::Alpha, json!("a1")));
        assert!(passes(&Rule::AlphaNumeric, json!("a1")));
        assert!(!passes(&Rule::AlphaNumeric, json!("a-1")));
        assert!(passes(&Rule::AlphaDash, json!("a-1_b")));
        assert!(!passes(&Rule::AlphaDash, json!("a b")));
        assert!(passes(&Rule::Slug, json!("hello-world")));
        assert!(!passes(&Rule::Slug, json!("hello world")));
        assert!(passes(&Rule::Uuid, json!("123e4567-e89b-12d3-a456-426614174000")));
        assert!(!passes(&Rule::Uuid, json!("nope")));
    }

    #[test]
    fn membership_rules() {
        let allowed = Rule::In(vec!["draft".into(), "live".into()]);
        assert!(passes(&allowed, json!("draft")));
        assert!(!passes(&allowed, json!("deleted")));

        let denied = Rule::NotIn(vec!["admin".into()]);
        assert!(passes(&denied, json!("user")));
        assert!(!passes(&denied, json!("admin")));

        assert!(passes(&Rule::StartsWith("pk_".into()), json!("pk_123")));
        assert!(!passes(&Rule::StartsWith("pk_".into()), json!("sk_123")));
        assert!(passes(&Rule::EndsWith(".png".into()), json!("a.png")));
    }

    #[test]
    fn date_rules() {
        assert!(passes(&Rule::Date, json!("2026-07-25")));
        assert!(!passes(&Rule::Date, json!("25/07/2026")));
        assert!(!passes(&Rule::Date, json!("2026-13-01")));

        let bound = NaiveDate::from_ymd_opt(2026, 1, 1).unwrap();
        assert!(passes(&Rule::After(bound), json!("2026-07-25")));
        assert!(!passes(&Rule::After(bound), json!("2025-12-31")));
        assert!(!passes(&Rule::After(bound), json!("2026-01-01")), "after is exclusive");
        assert!(passes(&Rule::Before(bound), json!("2025-12-31")));
    }

    #[test]
    fn cross_field_rules_read_the_whole_input() {
        let all = json!({ "password": "secret", "password_confirmation": "secret", "other": "x" });

        assert_eq!(Rule::Confirmed.check("password", Some(&json!("secret")), &all), Outcome::Pass);

        let mismatched = json!({ "password_confirmation": "different" });
        assert!(matches!(
            Rule::Confirmed.check("password", Some(&json!("secret")), &mismatched),
            Outcome::Fail(_)
        ));

        assert_eq!(
            Rule::Same("password".into()).check("f", Some(&json!("secret")), &all),
            Outcome::Pass
        );
        assert_eq!(
            Rule::Different("other".into()).check("f", Some(&json!("y")), &all),
            Outcome::Pass
        );
        assert!(matches!(
            Rule::Different("other".into()).check("f", Some(&json!("x")), &all),
            Outcome::Fail(_)
        ));
    }

    #[test]
    fn confirmed_fails_when_the_confirmation_is_absent() {
        let all = json!({ "password": "secret" });
        assert!(matches!(
            Rule::Confirmed.check("password", Some(&json!("secret")), &all),
            Outcome::Fail(_)
        ));
    }

    #[test]
    fn a_custom_rule_supplies_its_own_message() {
        let rule = Rule::Custom {
            message: "The code must start with X.".into(),
            check: Arc::new(|value| value.as_str().is_some_and(|s| s.starts_with('X'))),
        };
        assert!(passes(&rule, json!("X1")));
        assert_eq!(message(&rule, "code", json!("Y1")), "The code must start with X.");
    }

    #[test]
    fn messages_humanise_the_field_name() {
        assert_eq!(
            message(&Rule::Required, "email_address", json!(null)),
            "The email address field is required."
        );
    }
}
