//! Validation rules to JSON Schema — the part of the document nobody wants to
//! write twice.

use serde_json::{json, Map, Value};

use rainier_validation::{Rule, RuleSet};

/// The JSON Schema for a [`RuleSet`].
///
/// This is the reason generating a document is worth doing at all. A
/// hand-written OpenAPI file says what the endpoint accepted when somebody last
/// updated it; this one says what the validator will actually accept, because
/// it is derived from the same rules the validator runs.
///
/// ```
/// # use rainier_openapi::schema::schema_for;
/// # use rainier_validation::{field, Rule};
/// let schema = schema_for(&vec![
///     field("title", [Rule::Required, Rule::String, Rule::Between(3.0, 120.0)]),
/// ]);
///
/// assert_eq!(schema["required"][0], "title");
/// assert_eq!(schema["properties"]["title"]["minLength"], 3);
/// ```
pub fn schema_for(rules: &RuleSet) -> Value {
    let mut properties = Map::new();
    let mut required = Vec::new();

    for (name, field_rules) in rules {
        if field_rules.iter().any(|rule| matches!(rule, Rule::Required)) {
            required.push(Value::String((*name).to_string()));
        }
        properties.insert((*name).to_string(), property_for(field_rules));
    }

    let mut schema = json!({ "type": "object", "properties": properties });
    if !required.is_empty() {
        schema["required"] = Value::Array(required);
    }
    schema
}

/// One field's schema.
///
/// The type is decided first, because whether `Min(3)` means `minLength` or
/// `minimum` depends on it — the same rule means two different things and
/// getting it backwards produces a document that lies in a way clients act on.
fn property_for(rules: &[Rule]) -> Value {
    let kind = kind_of(rules);
    let mut property = Map::new();

    property.insert("type".into(), Value::String(kind.json_type().into()));
    if let Some(format) = kind.format() {
        property.insert("format".into(), Value::String(format.into()));
    }

    for rule in rules {
        match rule {
            Rule::Min(value) => bound(&mut property, kind, "min", *value),
            Rule::Max(value) => bound(&mut property, kind, "max", *value),
            Rule::Size(value) => {
                bound(&mut property, kind, "min", *value);
                bound(&mut property, kind, "max", *value);
            }
            Rule::Between(low, high) => {
                bound(&mut property, kind, "min", *low);
                bound(&mut property, kind, "max", *high);
            }
            Rule::In(values) => {
                property.insert(
                    "enum".into(),
                    Value::Array(values.iter().cloned().map(Value::String).collect()),
                );
            }
            Rule::StartsWith(prefix) => {
                property.insert("pattern".into(), json!(format!("^{}", escape(prefix))));
            }
            Rule::EndsWith(suffix) => {
                property.insert("pattern".into(), json!(format!("{}$", escape(suffix))));
            }
            Rule::Slug => {
                property.insert("pattern".into(), json!("^[A-Za-z0-9_-]+$"));
            }
            Rule::Alpha => {
                property.insert("pattern".into(), json!("^[A-Za-z]+$"));
            }
            Rule::AlphaNumeric => {
                property.insert("pattern".into(), json!("^[A-Za-z0-9]+$"));
            }
            Rule::AlphaDash => {
                property.insert("pattern".into(), json!("^[A-Za-z0-9_-]+$"));
            }
            // Everything else is either a type (already handled), a
            // cross-field rule that JSON Schema cannot express on one property,
            // or a predicate nothing can read.
            _ => {}
        }
    }

    Value::Object(property)
}

/// What a field holds, as far as the rules say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    String,
    Integer,
    Number,
    Boolean,
    Array,
    Email,
    Url,
    Uuid,
    Date,
}

impl Kind {
    fn json_type(self) -> &'static str {
        match self {
            Kind::Integer => "integer",
            Kind::Number => "number",
            Kind::Boolean => "boolean",
            Kind::Array => "array",
            _ => "string",
        }
    }

    /// The OpenAPI `format`, where there is a standard one.
    fn format(self) -> Option<&'static str> {
        match self {
            Kind::Email => Some("email"),
            Kind::Url => Some("uri"),
            Kind::Uuid => Some("uuid"),
            Kind::Date => Some("date"),
            _ => None,
        }
    }

    /// Whether a `Min`/`Max` counts characters or measures a value.
    fn measures_length(self) -> bool {
        !matches!(self, Kind::Integer | Kind::Number)
    }
}

/// The most specific type the rules imply.
///
/// Order matters: a field with `[String, Email]` is an email, and one with
/// `[Numeric, Integer]` is an integer. The specific rule wins, because it is
/// the one somebody added deliberately.
fn kind_of(rules: &[Rule]) -> Kind {
    let mut kind = Kind::String;

    for rule in rules {
        kind = match rule {
            Rule::Integer => Kind::Integer,
            Rule::Numeric if kind != Kind::Integer => Kind::Number,
            Rule::Boolean => Kind::Boolean,
            Rule::Array => Kind::Array,
            Rule::Email => Kind::Email,
            Rule::Url => Kind::Url,
            Rule::Uuid => Kind::Uuid,
            Rule::Date => Kind::Date,
            _ => kind,
        };
    }
    kind
}

/// `minLength` for text, `minimum` for a number, `minItems` for an array.
fn bound(property: &mut Map<String, Value>, kind: Kind, edge: &str, value: f64) {
    let key = match (kind, edge) {
        (Kind::Array, "min") => "minItems",
        (Kind::Array, "max") => "maxItems",
        (kind, "min") if kind.measures_length() => "minLength",
        (kind, "max") if kind.measures_length() => "maxLength",
        (_, "min") => "minimum",
        (_, _) => "maximum",
    };

    // A length is a count, and `"minLength": 3.0` is not valid JSON Schema.
    let value = if key.ends_with("Length") || key.ends_with("Items") {
        json!(value.max(0.0) as u64)
    } else {
        json!(value)
    };
    property.insert(key.into(), value);
}

/// Escape a literal for use inside a regular expression.
fn escape(literal: &str) -> String {
    literal
        .chars()
        .flat_map(|c| {
            let escaped = matches!(
                c,
                '.' | '^' | '$' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '\\'
            );
            escaped.then_some('\\').into_iter().chain(std::iter::once(c))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_validation::field;

    #[test]
    fn a_required_field_is_listed_as_required() {
        let schema = schema_for(&vec![
            field("title", [Rule::Required, Rule::String]),
            field("subtitle", [Rule::String]),
        ]);

        assert_eq!(schema["required"], json!(["title"]));
        assert!(schema["properties"]["subtitle"].is_object());
    }

    #[test]
    fn nothing_required_omits_the_key_rather_than_sending_an_empty_list() {
        // `"required": []` is invalid in JSON Schema draft 4 and pointless in
        // every later one.
        let schema = schema_for(&vec![field("optional", [Rule::String])]);

        assert!(schema.get("required").is_none(), "{schema}");
    }

    #[test]
    fn min_and_max_mean_length_for_text_and_value_for_a_number() {
        // The distinction that makes a generated document either useful or a
        // lie: the same rule means two different things.
        let text = schema_for(&vec![field("title", [Rule::String, Rule::Between(3.0, 120.0)])]);
        let number = schema_for(&vec![field("age", [Rule::Integer, Rule::Between(18.0, 120.0)])]);

        assert_eq!(text["properties"]["title"]["minLength"], 3);
        assert_eq!(text["properties"]["title"]["maxLength"], 120);
        assert!(text["properties"]["title"].get("minimum").is_none());

        assert_eq!(number["properties"]["age"]["minimum"], 18.0);
        assert_eq!(number["properties"]["age"]["maximum"], 120.0);
        assert!(number["properties"]["age"].get("minLength").is_none());
    }

    #[test]
    fn an_array_counts_its_items() {
        let schema = schema_for(&vec![field("tags", [Rule::Array, Rule::Max(5.0)])]);

        assert_eq!(schema["properties"]["tags"]["type"], "array");
        assert_eq!(schema["properties"]["tags"]["maxItems"], 5);
    }

    #[test]
    fn a_length_is_an_integer_not_a_float() {
        let schema = schema_for(&vec![field("title", [Rule::String, Rule::Min(3.0)])]);

        assert_eq!(schema["properties"]["title"]["minLength"].to_string(), "3");
    }

    #[test]
    fn the_specific_type_wins_over_the_general_one() {
        let email = schema_for(&vec![field("email", [Rule::String, Rule::Email])]);
        let integer = schema_for(&vec![field("n", [Rule::Numeric, Rule::Integer])]);

        assert_eq!(email["properties"]["email"]["format"], "email");
        assert_eq!(integer["properties"]["n"]["type"], "integer");
    }

    #[test]
    fn formats_come_through() {
        for (rule, format) in
            [(Rule::Email, "email"), (Rule::Url, "uri"), (Rule::Uuid, "uuid"), (Rule::Date, "date")]
        {
            let schema = schema_for(&vec![field("f", [rule])]);
            assert_eq!(schema["properties"]["f"]["format"], format);
        }
    }

    #[test]
    fn an_in_rule_becomes_an_enum() {
        let schema =
            schema_for(&vec![field("status", [Rule::In(vec!["draft".into(), "live".into()])])]);

        assert_eq!(schema["properties"]["status"]["enum"], json!(["draft", "live"]));
    }

    #[test]
    fn a_prefix_becomes_an_anchored_pattern_with_its_metacharacters_escaped() {
        // `starts_with("a.b")` must not match "axb".
        let schema = schema_for(&vec![field("key", [Rule::StartsWith("a.b".into())])]);

        assert_eq!(schema["properties"]["key"]["pattern"], r"^a\.b");
    }

    #[test]
    fn a_rule_no_schema_can_express_is_skipped_rather_than_guessed() {
        // `Confirmed` is a cross-field rule; there is no honest property-level
        // rendering, and inventing one would document something untrue.
        let schema = schema_for(&vec![field("password", [Rule::String, Rule::Confirmed])]);
        let property = &schema["properties"]["password"];

        assert_eq!(property["type"], "string");
        assert_eq!(property.as_object().unwrap().len(), 1, "{property}");
    }
}
