//! Turning query strings and form bodies into a uniform JSON tree.
//!
//! Everything downstream of the request — `input()`, validation rules,
//! `Form<T>` deserialisation — reads one shape: a [`serde_json::Value`]. A JSON
//! body already is one; a query string or a urlencoded form has to be lifted
//! into one, and that lift is where PHP-style bracket notation is honoured:
//!
//! | Query | Value |
//! |---|---|
//! | `a=1&b=2` | `{"a": "1", "b": "2"}` |
//! | `tags[]=x&tags[]=y` | `{"tags": ["x", "y"]}` |
//! | `user[name]=ada` | `{"user": {"name": "ada"}}` |
//! | `a=1&a=2` | `{"a": ["1", "2"]}` |
//!
//! Values stay **strings**. A form has no types — `?active=1` could be the
//! number one or the string "1" — so guessing here would make
//! `Form<T>`/validation behaviour depend on what the value happened to look
//! like. Coercion is the validator's job, where the expected type is known.

use serde_json::{Map, Value};

/// Parse `application/x-www-form-urlencoded` bytes (a query string or a form
/// body) into a JSON object.
pub fn parse_urlencoded(raw: &[u8]) -> Value {
    let mut root = Value::Object(Map::new());
    for (key, value) in form_urlencoded::parse(raw) {
        insert(&mut root, &key, Value::String(value.into_owned()));
    }
    root
}

/// Insert `value` into `target` at a possibly-bracketed `key`.
pub(crate) fn insert(target: &mut Value, key: &str, value: Value) {
    let segments = parse_segments(key);
    if !segments.is_empty() {
        insert_segments(target, &segments, value);
    }
}

/// Walk `segments`, creating containers of whatever kind the *next* segment
/// needs, and place `value` at the end.
///
/// Recursion rather than a cursor loop, because the container kind at each
/// level is only known from the segment after it — `a[b]` needs `a` to be an
/// object, `a[]` needs it to be an array — and a loop would have to look ahead
/// and re-descend to fix up what it had already created.
fn insert_segments(target: &mut Value, segments: &[Segment], value: Value) {
    match segments {
        [] => *target = value,

        // Final named key: set it, or fold a repeat into an array.
        [Segment::Key(name)] => {
            let map = as_object(target);
            match map.get_mut(name) {
                // A repeated plain key (`a=1&a=2`) becomes an array, as every
                // server-side form parser has done for decades.
                Some(Value::Array(items)) => items.push(value),
                Some(existing) => {
                    let previous = std::mem::replace(existing, Value::Null);
                    *existing = Value::Array(vec![previous, value]);
                }
                None => {
                    map.insert(name.clone(), value);
                }
            }
        }

        // Final `[]`: append.
        [Segment::Append] => as_array(target).push(value),

        [Segment::Key(name), rest @ ..] => {
            let child = as_object(target).entry(name.clone()).or_insert(Value::Null);
            insert_segments(child, rest, value);
        }

        // `a[][b]=1` — each `[]` starts a fresh element.
        [Segment::Append, rest @ ..] => {
            let items = as_array(target);
            items.push(Value::Null);
            let last = items.last_mut().expect("just pushed");
            insert_segments(last, rest, value);
        }
    }
}

/// Coerce `target` into an object, replacing whatever was there.
///
/// A conflict (`a=1&a[b]=2`) can only be resolved by picking one shape; the
/// later, more specific key wins, which is the same call PHP makes.
fn as_object(target: &mut Value) -> &mut Map<String, Value> {
    if !target.is_object() {
        *target = Value::Object(Map::new());
    }
    target.as_object_mut().expect("just ensured object")
}

/// Coerce `target` into an array, replacing whatever was there.
fn as_array(target: &mut Value) -> &mut Vec<Value> {
    if !target.is_array() {
        *target = Value::Array(Vec::new());
    }
    target.as_array_mut().expect("just ensured array")
}

/// One step of a bracketed key.
#[derive(Debug, Clone, PartialEq)]
enum Segment {
    /// A named key.
    Key(String),
    /// An empty `[]`, meaning "append".
    Append,
}

/// `user[address][city]` → `[Key("user"), Key("address"), Key("city")]`;
/// `tags[]` → `[Key("tags"), Append]`.
fn parse_segments(key: &str) -> Vec<Segment> {
    let Some(open) = key.find('[') else {
        return vec![Segment::Key(key.to_string())];
    };

    let mut segments = vec![Segment::Key(key[..open].to_string())];
    let mut rest = &key[open..];

    while let Some(stripped) = rest.strip_prefix('[') {
        let Some(close) = stripped.find(']') else {
            // Unbalanced — treat the remainder as a literal key rather than
            // silently discarding it.
            segments.push(Segment::Key(stripped.to_string()));
            break;
        };
        let name = &stripped[..close];
        segments.push(if name.is_empty() {
            Segment::Append
        } else {
            Segment::Key(name.to_string())
        });
        rest = &stripped[close + 1..];
    }

    segments
}

/// Read a dotted path out of a JSON tree. `items.0.name` indexes arrays.
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

/// Render a JSON value as the string an `input()` caller expects.
///
/// Scalars stringify; containers do not (a caller asking for a string does not
/// want `[object Object]`), so they read as `None`.
pub fn scalar_to_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Null | Value::Array(_) | Value::Object(_) => None,
    }
}

/// Merge `overlay` over `base`, recursing into objects.
pub fn merge(base: Value, overlay: Value) -> Value {
    match (base, overlay) {
        (Value::Object(mut base), Value::Object(overlay)) => {
            for (key, value) in overlay {
                let merged = match base.remove(&key) {
                    Some(existing) => merge(existing, value),
                    None => value,
                };
                base.insert(key, merged);
            }
            Value::Object(base)
        }
        (_, overlay) => overlay,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(query: &str) -> Value {
        parse_urlencoded(query.as_bytes())
    }

    #[test]
    fn parses_flat_pairs() {
        assert_eq!(parse("a=1&b=2"), json!({ "a": "1", "b": "2" }));
    }

    #[test]
    fn values_stay_strings() {
        // Not `1` — a form carries no types, so coercion belongs to whoever
        // knows the target type.
        assert_eq!(parse("n=1&t=true"), json!({ "n": "1", "t": "true" }));
    }

    #[test]
    fn percent_and_plus_decoding() {
        assert_eq!(parse("q=hello+world"), json!({ "q": "hello world" }));
        assert_eq!(parse("q=a%26b"), json!({ "q": "a&b" }));
    }

    #[test]
    fn empty_input_is_an_empty_object() {
        assert_eq!(parse(""), json!({}));
    }

    #[test]
    fn bracket_append_builds_an_array() {
        assert_eq!(parse("tags[]=x&tags[]=y"), json!({ "tags": ["x", "y"] }));
    }

    #[test]
    fn a_single_bracket_append_is_still_an_array() {
        assert_eq!(parse("tags[]=x"), json!({ "tags": ["x"] }));
    }

    #[test]
    fn a_repeated_plain_key_collapses_to_an_array() {
        assert_eq!(parse("a=1&a=2&a=3"), json!({ "a": ["1", "2", "3"] }));
    }

    #[test]
    fn bracket_keys_build_nested_objects() {
        assert_eq!(parse("user[name]=ada"), json!({ "user": { "name": "ada" } }));
        assert_eq!(
            parse("user[address][city]=paris"),
            json!({ "user": { "address": { "city": "paris" } } })
        );
    }

    #[test]
    fn nested_objects_and_scalars_coexist() {
        assert_eq!(
            parse("user[name]=ada&user[age]=36&active=1"),
            json!({ "user": { "name": "ada", "age": "36" }, "active": "1" })
        );
    }

    #[test]
    fn an_unbalanced_bracket_keeps_the_value() {
        let parsed = parse("a[b=1");
        assert!(parsed.get("a").is_some(), "{parsed}");
    }

    #[test]
    fn repeated_appends_accumulate_into_one_array() {
        // Regression guard: creating the parent container must not discard the
        // array built by the previous value for the same key.
        assert_eq!(parse("t[]=a&t[]=b&t[]=c"), json!({ "t": ["a", "b", "c"] }));
        assert_eq!(parse("u[tags][]=x&u[tags][]=y"), json!({ "u": { "tags": ["x", "y"] } }));
    }

    #[test]
    fn an_append_of_objects_starts_a_new_element_each_time() {
        assert_eq!(
            parse("rows[][name]=a&rows[][name]=b"),
            json!({ "rows": [{ "name": "a" }, { "name": "b" }] })
        );
    }

    #[test]
    fn a_more_specific_later_key_wins_a_shape_conflict() {
        assert_eq!(parse("a=1&a[b]=2"), json!({ "a": { "b": "2" } }));
    }

    #[test]
    fn dotted_lookup_walks_objects_and_arrays() {
        let value = json!({ "user": { "roles": ["admin", "editor"] } });
        assert_eq!(lookup(&value, "user.roles.0"), Some(&json!("admin")));
        assert_eq!(lookup(&value, "user.roles.9"), None);
        assert_eq!(lookup(&value, "user.missing"), None);
    }

    #[test]
    fn only_scalars_stringify() {
        assert_eq!(scalar_to_string(&json!("x")), Some("x".to_string()));
        assert_eq!(scalar_to_string(&json!(3)), Some("3".to_string()));
        assert_eq!(scalar_to_string(&json!(true)), Some("true".to_string()));
        assert_eq!(scalar_to_string(&json!(null)), None);
        assert_eq!(scalar_to_string(&json!({ "a": 1 })), None);
        assert_eq!(scalar_to_string(&json!([1])), None);
    }

    #[test]
    fn merge_is_recursive_over_objects() {
        let merged =
            merge(json!({ "a": { "x": 1, "y": 2 }, "b": 1 }), json!({ "a": { "y": 3 }, "c": 4 }));
        assert_eq!(merged, json!({ "a": { "x": 1, "y": 3 }, "b": 1, "c": 4 }));
    }
}
