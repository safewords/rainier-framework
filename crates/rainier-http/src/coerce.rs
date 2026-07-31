//! A `serde` deserialiser that coerces strings into scalars.
//!
//! Query strings, urlencoded forms and route parameters are **all strings** —
//! `?page=2` carries no clue that `2` is a number. Feeding that straight to
//! `serde_json::from_value` means `Query<Pagination>` with a `page: u32` field
//! fails on every request, which would make typed extractors useless for
//! exactly the inputs they are most wanted for.
//!
//! So this deserialiser sits in front: when the target type asks for a number,
//! a bool or a char and the value is a string, it parses. When the target asks
//! for a string and the value is a number or bool, it stringifies. Everything
//! else behaves as `serde_json` does.
//!
//! It deliberately does **not** treat `""` as absent. That conversion is
//! policy, not encoding — Rainier makes it an explicit middleware
//! (`ConvertEmptyStringsToNull`), so an application can
//! decide whether an empty text input means "null" or "empty string".

use std::fmt;

use serde::de::{
    self, DeserializeOwned, DeserializeSeed, EnumAccess, IntoDeserializer, MapAccess, SeqAccess,
    VariantAccess, Visitor,
};
use serde::forward_to_deserialize_any;
use serde_json::{Map, Value};

/// Deserialise `value` into `T`, coercing string scalars to the requested type.
pub fn from_value<T: DeserializeOwned>(value: &Value) -> Result<T, CoerceError> {
    T::deserialize(Coercing::new(value))
}

/// A deserialisation failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoerceError {
    message: String,
}

impl CoerceError {
    fn new(message: impl Into<String>) -> Self {
        Self { message: message.into() }
    }

    /// The failure message.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for CoerceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CoerceError {}

impl de::Error for CoerceError {
    fn custom<T: fmt::Display>(message: T) -> Self {
        Self::new(message.to_string())
    }
}

/// A `serde` deserialiser over a borrowed [`Value`].
pub struct Coercing<'de> {
    value: &'de Value,
}

impl<'de> Coercing<'de> {
    /// Wrap a value.
    pub fn new(value: &'de Value) -> Self {
        Self { value }
    }

    fn kind(&self) -> &'static str {
        match self.value {
            Value::Null => "null",
            Value::Bool(_) => "a boolean",
            Value::Number(_) => "a number",
            Value::String(_) => "a string",
            Value::Array(_) => "an array",
            Value::Object(_) => "an object",
        }
    }

    fn invalid(&self, wanted: &str) -> CoerceError {
        CoerceError::new(format!("expected {wanted}, found {}", self.kind()))
    }
}

/// Implements one numeric `deserialize_*`: parse from a string, narrow from a
/// JSON number, otherwise fail.
macro_rules! coerce_number {
    ($method:ident, $ty:ty, $visit:ident) => {
        fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, CoerceError> {
            let parsed: $ty = match self.value {
                Value::String(raw) => raw.trim().parse().map_err(|_| {
                    CoerceError::new(format!(
                        "`{raw}` is not a valid {}",
                        std::any::type_name::<$ty>()
                    ))
                })?,
                Value::Number(n) => n.to_string().parse().map_err(|_| {
                    CoerceError::new(format!(
                        "{n} does not fit in a {}",
                        std::any::type_name::<$ty>()
                    ))
                })?,
                // A bool coerces to 0/1, which is what `?flag=true` into a
                // numeric field should mean if anyone writes it.
                Value::Bool(b) => {
                    if *b {
                        1 as $ty
                    } else {
                        0 as $ty
                    }
                }
                _ => return Err(self.invalid("a number")),
            };
            visitor.$visit(parsed)
        }
    };
}

impl<'de> de::Deserializer<'de> for Coercing<'de> {
    type Error = CoerceError;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, CoerceError> {
        match self.value {
            Value::Null => visitor.visit_unit(),
            Value::Bool(b) => visitor.visit_bool(*b),
            Value::String(s) => visitor.visit_str(s),
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    visitor.visit_i64(i)
                } else if let Some(u) = n.as_u64() {
                    visitor.visit_u64(u)
                } else if let Some(f) = n.as_f64() {
                    visitor.visit_f64(f)
                } else {
                    Err(CoerceError::new("unrepresentable number"))
                }
            }
            Value::Array(items) => visitor.visit_seq(SeqReader { items, index: 0 }),
            Value::Object(map) => visitor.visit_map(MapReader {
                entries: map.iter().collect(),
                index: 0,
                value: None,
            }),
        }
    }

    fn deserialize_bool<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, CoerceError> {
        let parsed = match self.value {
            Value::Bool(b) => *b,
            // The set a checkbox or a query flag actually arrives as.
            Value::String(raw) => match raw.trim().to_ascii_lowercase().as_str() {
                "true" | "1" | "on" | "yes" => true,
                "false" | "0" | "off" | "no" => false,
                other => return Err(CoerceError::new(format!("`{other}` is not a valid boolean"))),
            },
            Value::Number(n) => n.as_i64().is_some_and(|i| i != 0),
            _ => return Err(self.invalid("a boolean")),
        };
        visitor.visit_bool(parsed)
    }

    coerce_number!(deserialize_i8, i8, visit_i8);
    coerce_number!(deserialize_i16, i16, visit_i16);
    coerce_number!(deserialize_i32, i32, visit_i32);
    coerce_number!(deserialize_i64, i64, visit_i64);
    coerce_number!(deserialize_u8, u8, visit_u8);
    coerce_number!(deserialize_u16, u16, visit_u16);
    coerce_number!(deserialize_u32, u32, visit_u32);
    coerce_number!(deserialize_u64, u64, visit_u64);
    coerce_number!(deserialize_f32, f32, visit_f32);
    coerce_number!(deserialize_f64, f64, visit_f64);

    fn deserialize_char<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, CoerceError> {
        match self.value {
            Value::String(s) if s.chars().count() == 1 => {
                visitor.visit_char(s.chars().next().expect("just counted one"))
            }
            Value::String(_) => Err(CoerceError::new("expected a single character")),
            _ => Err(self.invalid("a character")),
        }
    }

    fn deserialize_str<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, CoerceError> {
        self.deserialize_string(visitor)
    }

    fn deserialize_string<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, CoerceError> {
        match self.value {
            Value::String(s) => visitor.visit_str(s),
            // A JSON body may legitimately hold a number where the struct
            // wants a string (an id, a postcode). Stringify rather than fail.
            Value::Number(n) => visitor.visit_string(n.to_string()),
            Value::Bool(b) => visitor.visit_string(b.to_string()),
            _ => Err(self.invalid("a string")),
        }
    }

    fn deserialize_bytes<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, CoerceError> {
        match self.value {
            Value::String(s) => visitor.visit_bytes(s.as_bytes()),
            _ => Err(self.invalid("bytes")),
        }
    }

    fn deserialize_byte_buf<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, CoerceError> {
        self.deserialize_bytes(visitor)
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, CoerceError> {
        match self.value {
            Value::Null => visitor.visit_none(),
            _ => visitor.visit_some(self),
        }
    }

    fn deserialize_unit<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, CoerceError> {
        match self.value {
            Value::Null => visitor.visit_unit(),
            _ => Err(self.invalid("null")),
        }
    }

    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, CoerceError> {
        self.deserialize_unit(visitor)
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, CoerceError> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, CoerceError> {
        match self.value {
            Value::Array(items) => visitor.visit_seq(SeqReader { items, index: 0 }),
            // `?tag=x` where the field is `Vec<String>`: a single value is a
            // one-element list. Without this, whether `tags` parses would
            // depend on how many were submitted.
            Value::Null => visitor.visit_seq(SeqReader { items: &[], index: 0 }),
            single => visitor.visit_seq(SingleReader { value: Some(single) }),
        }
    }

    fn deserialize_tuple<V: Visitor<'de>>(
        self,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value, CoerceError> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value, CoerceError> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, CoerceError> {
        match self.value {
            Value::Object(map) => visitor.visit_map(MapReader {
                entries: map.iter().collect(),
                index: 0,
                value: None,
            }),
            _ => Err(self.invalid("an object")),
        }
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, CoerceError> {
        self.deserialize_map(visitor)
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, CoerceError> {
        match self.value {
            // `?status=active` — a unit variant by name.
            Value::String(name) => visitor.visit_enum(name.as_str().into_deserializer()),
            // `{"kind": {...}}` — an externally tagged variant.
            Value::Object(map) if map.len() == 1 => {
                let (name, value) = map.iter().next().expect("just checked len == 1");
                visitor.visit_enum(EnumReader { name, value })
            }
            _ => Err(self.invalid("an enum variant")),
        }
    }

    fn deserialize_identifier<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, CoerceError> {
        self.deserialize_string(visitor)
    }

    forward_to_deserialize_any! { ignored_any }
}

struct SeqReader<'de> {
    items: &'de [Value],
    index: usize,
}

impl<'de> SeqAccess<'de> for SeqReader<'de> {
    type Error = CoerceError;

    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, CoerceError> {
        let Some(value) = self.items.get(self.index) else {
            return Ok(None);
        };
        self.index += 1;
        seed.deserialize(Coercing::new(value)).map(Some)
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.items.len() - self.index)
    }
}

/// Presents one scalar as a one-element sequence.
struct SingleReader<'de> {
    value: Option<&'de Value>,
}

impl<'de> SeqAccess<'de> for SingleReader<'de> {
    type Error = CoerceError;

    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, CoerceError> {
        match self.value.take() {
            Some(value) => seed.deserialize(Coercing::new(value)).map(Some),
            None => Ok(None),
        }
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.value.is_some() as usize)
    }
}

struct MapReader<'de> {
    entries: Vec<(&'de String, &'de Value)>,
    index: usize,
    value: Option<&'de Value>,
}

impl<'de> MapAccess<'de> for MapReader<'de> {
    type Error = CoerceError;

    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, CoerceError> {
        let Some((key, value)) = self.entries.get(self.index) else {
            return Ok(None);
        };
        self.index += 1;
        self.value = Some(value);
        seed.deserialize(key.as_str().into_deserializer()).map(Some)
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(
        &mut self,
        seed: V,
    ) -> Result<V::Value, CoerceError> {
        let value =
            self.value.take().ok_or_else(|| CoerceError::new("value requested before its key"))?;
        seed.deserialize(Coercing::new(value))
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.entries.len() - self.index)
    }
}

struct EnumReader<'de> {
    name: &'de String,
    value: &'de Value,
}

impl<'de> EnumAccess<'de> for EnumReader<'de> {
    type Error = CoerceError;
    type Variant = Self;

    fn variant_seed<V: DeserializeSeed<'de>>(
        self,
        seed: V,
    ) -> Result<(V::Value, Self), CoerceError> {
        let name = seed.deserialize(self.name.as_str().into_deserializer())?;
        Ok((name, self))
    }
}

impl<'de> VariantAccess<'de> for EnumReader<'de> {
    type Error = CoerceError;

    fn unit_variant(self) -> Result<(), CoerceError> {
        Ok(())
    }

    fn newtype_variant_seed<T: DeserializeSeed<'de>>(
        self,
        seed: T,
    ) -> Result<T::Value, CoerceError> {
        seed.deserialize(Coercing::new(self.value))
    }

    fn tuple_variant<V: Visitor<'de>>(
        self,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value, CoerceError> {
        de::Deserializer::deserialize_seq(Coercing::new(self.value), visitor)
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, CoerceError> {
        de::Deserializer::deserialize_map(Coercing::new(self.value), visitor)
    }
}

/// Build a JSON object from string pairs — how route parameters reach the
/// deserialiser.
pub fn object_from_strings<'a>(pairs: impl IntoIterator<Item = (&'a String, &'a String)>) -> Value {
    let mut map = Map::new();
    for (key, value) in pairs {
        map.insert(key.clone(), Value::String(value.clone()));
    }
    Value::Object(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use serde_json::json;

    #[derive(Debug, PartialEq, Deserialize)]
    struct Pagination {
        page: u32,
        per_page: Option<u16>,
        sort: String,
    }

    #[test]
    fn coerces_strings_into_numbers() {
        let parsed: Pagination =
            from_value(&json!({ "page": "2", "per_page": "50", "sort": "name" })).unwrap();
        assert_eq!(parsed, Pagination { page: 2, per_page: Some(50), sort: "name".into() });
    }

    #[test]
    fn real_numbers_still_work() {
        let parsed: Pagination =
            from_value(&json!({ "page": 2, "per_page": 50, "sort": "name" })).unwrap();
        assert_eq!(parsed.page, 2);
    }

    #[test]
    fn numbers_coerce_into_strings() {
        #[derive(Deserialize)]
        struct Row {
            postcode: String,
        }
        let parsed: Row = from_value(&json!({ "postcode": 1234 })).unwrap();
        assert_eq!(parsed.postcode, "1234");
    }

    #[test]
    fn coerces_the_booleans_a_form_actually_sends() {
        #[derive(Deserialize)]
        struct Flags {
            a: bool,
            b: bool,
            c: bool,
            d: bool,
        }
        let parsed: Flags =
            from_value(&json!({ "a": "1", "b": "true", "c": "on", "d": "no" })).unwrap();
        assert!(parsed.a && parsed.b && parsed.c && !parsed.d);
    }

    #[test]
    fn a_missing_optional_is_none() {
        let parsed: Pagination = from_value(&json!({ "page": "1", "sort": "id" })).unwrap();
        assert_eq!(parsed.per_page, None);
    }

    #[test]
    fn an_explicit_null_optional_is_none() {
        let parsed: Pagination =
            from_value(&json!({ "page": "1", "per_page": null, "sort": "id" })).unwrap();
        assert_eq!(parsed.per_page, None);
    }

    #[test]
    fn an_empty_string_is_not_silently_treated_as_absent() {
        // Policy belongs to `ConvertEmptyStringsToNull`, not the encoding.
        let err = from_value::<Pagination>(&json!({ "page": "1", "per_page": "", "sort": "id" }))
            .unwrap_err();
        assert!(err.message().contains("not a valid"), "{}", err.message());
    }

    #[test]
    fn an_unparsable_number_names_the_value() {
        let err = from_value::<Pagination>(&json!({ "page": "abc", "sort": "id" })).unwrap_err();
        assert!(err.message().contains("`abc`"), "{}", err.message());
    }

    #[test]
    fn a_single_value_deserialises_as_a_one_element_list() {
        #[derive(Deserialize)]
        struct Tags {
            tag: Vec<String>,
        }
        let one: Tags = from_value(&json!({ "tag": "rust" })).unwrap();
        assert_eq!(one.tag, vec!["rust"]);

        let many: Tags = from_value(&json!({ "tag": ["rust", "web"] })).unwrap();
        assert_eq!(many.tag, vec!["rust", "web"]);
    }

    #[test]
    fn nested_structures_coerce_all_the_way_down() {
        #[derive(Debug, PartialEq, Deserialize)]
        struct Inner {
            n: i64,
        }
        #[derive(Debug, PartialEq, Deserialize)]
        struct Outer {
            inner: Inner,
            list: Vec<Inner>,
        }

        let parsed: Outer =
            from_value(&json!({ "inner": { "n": "1" }, "list": [{ "n": "2" }, { "n": "3" }] }))
                .unwrap();
        assert_eq!(parsed.inner.n, 1);
        assert_eq!(parsed.list, vec![Inner { n: 2 }, Inner { n: 3 }]);
    }

    #[test]
    fn unit_enum_variants_come_from_strings() {
        #[derive(Debug, PartialEq, Deserialize)]
        #[serde(rename_all = "lowercase")]
        enum Status {
            Active,
            Archived,
        }
        #[derive(Debug, PartialEq, Deserialize)]
        struct Filter {
            status: Status,
        }

        let parsed: Filter = from_value(&json!({ "status": "archived" })).unwrap();
        assert_eq!(parsed.status, Status::Archived);
    }

    #[test]
    fn deserialises_a_bare_scalar() {
        assert_eq!(from_value::<u64>(&json!("42")).unwrap(), 42);
        assert_eq!(from_value::<String>(&json!("x")).unwrap(), "x");
        assert!(from_value::<bool>(&json!("yes")).unwrap());
    }

    #[test]
    fn a_wrong_shape_reports_what_it_found() {
        let err = from_value::<Pagination>(&json!("nope")).unwrap_err();
        assert!(err.message().contains("found a string"), "{}", err.message());
    }

    #[test]
    fn out_of_range_numbers_are_rejected() {
        assert!(from_value::<u8>(&json!("300")).is_err());
        assert!(from_value::<i32>(&json!("-1")).is_ok());
        assert!(from_value::<u32>(&json!("-1")).is_err());
    }

    #[test]
    fn builds_an_object_from_string_pairs() {
        let id = ("id".to_string(), "7".to_string());
        let slug = ("slug".to_string(), "hello".to_string());
        let value = object_from_strings([(&id.0, &id.1), (&slug.0, &slug.1)]);
        assert_eq!(value, json!({ "id": "7", "slug": "hello" }));

        #[derive(Deserialize)]
        struct Params {
            id: u64,
            slug: String,
        }
        let parsed: Params = from_value(&value).unwrap();
        assert_eq!(parsed.id, 7);
        assert_eq!(parsed.slug, "hello");
    }
}
