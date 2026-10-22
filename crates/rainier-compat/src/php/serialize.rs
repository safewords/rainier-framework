//! PHP's `serialize()` format — [`unserialize`] and [`serialize`] over a
//! [`Value`] tree.
//!
//! ```
//! use rainier_compat::php::serialize::{serialize, unserialize, Value};
//!
//! let value = unserialize(br#"a:2:{s:4:"name";s:3:"Ada";s:3:"age";i:36;}"#).unwrap();
//! assert_eq!(value.get("name").and_then(Value::as_str), Some("Ada"));
//! assert_eq!(value.get("age").and_then(Value::as_int), Some(36));
//!
//! // Byte for byte back again.
//! assert_eq!(serialize(&value), br#"a:2:{s:4:"name";s:3:"Ada";s:3:"age";i:36;}"#);
//! ```
//!
//! # Why a value tree and not serde
//!
//! A PHP payload is not a schema. A session holds whatever every request
//! that ever touched it decided to put there — arrays keyed by integers *and*
//! strings in one map, objects of classes this process has never heard of,
//! references between slots. A serde `Deserialize` target would have to
//! anticipate all of it, and the first unexpected shape would lose the whole
//! payload. So the codec reads **everything** into [`Value`], losslessly, and
//! the caller picks out what it understands. [`Value::to_json`] is there for
//! the common case where the interesting part is plain data.
//!
//! # It never instantiates anything
//!
//! PHP's own `unserialize` constructs objects and runs their magic methods,
//! which is why feeding it untrusted input is a well-known remote code
//! execution. Here an object is a class **name** and a list of properties —
//! data, nothing more — so a hostile payload can at worst be refused.
//!
//! # Bounded
//!
//! Every length and count is checked against the bytes actually left before
//! anything is allocated, and nesting is capped ([`Limits`]). A payload that
//! claims a four-gigabyte string, or a billion elements, or ten thousand
//! levels of arrays, is an error rather than an allocation or a stack
//! overflow.
//!
//! # Floats
//!
//! Read in every form the format allows (`1.5`, `1.0E+25`, `INF`, `-INF`,
//! `NAN`). Written as the shortest decimal that round-trips, which PHP parses
//! to the same double — though for very large or very small magnitudes PHP
//! itself would have chosen an exponent where this writes the digits out.
//! Equal values, not always equal bytes; nothing compares serialized floats.

use std::fmt;

use rainier_support::{Error, Result};
use serde_json::{Map as JsonMap, Number, Value as Json};

/// One PHP value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// `N;`
    Null,
    /// `b:0;` / `b:1;`
    Bool(bool),
    /// `i:42;`
    Int(i64),
    /// `d:0.5;`
    Float(f64),
    /// `s:5:"bytes";` — **bytes**, because a PHP string is one. Most are UTF-8;
    /// [`as_str`](Value::as_str) says so when it is.
    String(Vec<u8>),
    /// `a:n:{…}` — an ordered map whose keys are integers or strings, in the
    /// order PHP wrote them.
    Array(Vec<(Key, Value)>),
    /// `O:len:"Class":n:{…}` — a class name and its properties. Nothing is
    /// constructed.
    Object(Object),
    /// `C:len:"Class":len:{…}` — a class that implements its own
    /// serialization. The payload is kept verbatim, because only that class
    /// knows what it means.
    Custom {
        /// The class.
        class: String,
        /// Whatever its `serialize()` method produced.
        data: Vec<u8>,
    },
    /// `E:len:"Class:Case";` — a backed or pure enum case.
    Enum {
        /// The enum.
        class: String,
        /// The case.
        case: String,
    },
    /// `r:n;` (a copy of slot *n*) or `R:n;` (an alias of slot *n*), where
    /// slots are numbered from 1 in the order values appear. Kept as written:
    /// resolving it would change what a round trip produces.
    Reference {
        /// The slot it points at.
        slot: usize,
        /// `R:` — a PHP `&` alias — rather than `r:`.
        alias: bool,
    },
}

/// An array key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Key {
    /// `i:0;`
    Int(i64),
    /// `s:3:"key";`
    Str(Vec<u8>),
}

impl Key {
    /// A string key.
    pub fn str(key: impl AsRef<[u8]>) -> Self {
        Key::Str(key.as_ref().to_vec())
    }

    /// Whether this key is `name`, compared the way PHP would look it up — so
    /// the string `"7"` and the integer `7` are the same key.
    pub fn matches(&self, name: &str) -> bool {
        match self {
            Key::Str(bytes) => bytes == name.as_bytes(),
            Key::Int(n) => name.parse::<i64>().is_ok_and(|parsed| parsed == *n),
        }
    }

    /// The key as text — an integer key printed, a string key if it is UTF-8.
    pub fn to_text(&self) -> Option<String> {
        match self {
            Key::Int(n) => Some(n.to_string()),
            Key::Str(bytes) => String::from_utf8(bytes.clone()).ok(),
        }
    }
}

/// An object: its class and its properties.
#[derive(Debug, Clone, PartialEq)]
pub struct Object {
    /// The class name, as written.
    pub class: String,
    /// Properties in order. Names are kept **mangled** — see [`Property`].
    pub properties: Vec<(Property, Value)>,
}

impl Object {
    /// A property by its plain name, whatever its visibility.
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.properties.iter().find(|(property, _)| property.name() == name).map(|(_, v)| v)
    }
}

/// A property name, as PHP mangles it by visibility.
///
/// PHP writes a protected property as `"\0*\0name"` and a private one as
/// `"\0Class\0name"`. The raw form is kept so the object round-trips; the
/// accessors take the mangling apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Property(pub Vec<u8>);

/// Who can see a property.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Visibility {
    /// No mangling.
    Public,
    /// `\0*\0`.
    Protected,
    /// `\0Class\0` — private to that class.
    Private(String),
}

impl Property {
    /// A public property.
    pub fn public(name: &str) -> Self {
        Self(name.as_bytes().to_vec())
    }

    /// A protected property.
    pub fn protected(name: &str) -> Self {
        Self([b"\0*\0".as_slice(), name.as_bytes()].concat())
    }

    /// A property private to `class`.
    pub fn private(class: &str, name: &str) -> Self {
        Self([b"\0", class.as_bytes(), b"\0", name.as_bytes()].concat())
    }

    /// The unmangled name.
    pub fn name(&self) -> String {
        let (_, name) = self.split();
        String::from_utf8_lossy(name).into_owned()
    }

    /// Its visibility.
    pub fn visibility(&self) -> Visibility {
        let (scope, _) = self.split();
        match scope {
            None => Visibility::Public,
            Some(b"*") => Visibility::Protected,
            Some(class) => Visibility::Private(String::from_utf8_lossy(class).into_owned()),
        }
    }

    fn split(&self) -> (Option<&[u8]>, &[u8]) {
        let raw = self.0.as_slice();
        if raw.first() == Some(&0) {
            if let Some(end) = raw[1..].iter().position(|byte| *byte == 0) {
                return (Some(&raw[1..=end]), &raw[end + 2..]);
            }
        }
        (None, raw)
    }
}

impl Value {
    /// A string value from text.
    pub fn string(text: impl AsRef<[u8]>) -> Self {
        Value::String(text.as_ref().to_vec())
    }

    /// The string, if this is one and it is UTF-8.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(bytes) => std::str::from_utf8(bytes).ok(),
            _ => None,
        }
    }

    /// The raw bytes of a string.
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::String(bytes) => Some(bytes),
            _ => None,
        }
    }

    /// The integer, if this is one.
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(n) => Some(*n),
            _ => None,
        }
    }

    /// The boolean, if this is one.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// The array's entries, if this is one.
    pub fn as_array(&self) -> Option<&[(Key, Value)]> {
        match self {
            Value::Array(entries) => Some(entries),
            _ => None,
        }
    }

    /// The object, if this is one.
    pub fn as_object(&self) -> Option<&Object> {
        match self {
            Value::Object(object) => Some(object),
            _ => None,
        }
    }

    /// An array element or object property by name — `None` for anything
    /// else.
    pub fn get(&self, name: &str) -> Option<&Value> {
        match self {
            Value::Array(entries) => {
                entries.iter().find(|(key, _)| key.matches(name)).map(|(_, value)| value)
            }
            Value::Object(object) => object.get(name),
            _ => None,
        }
    }

    /// Follow a dotted path — `"_flash.old"` — through nested arrays and
    /// objects.
    pub fn path(&self, path: &str) -> Option<&Value> {
        path.split('.').try_fold(self, |value, segment| value.get(segment))
    }

    /// Whether this is `null`.
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Convert to JSON, **only** when nothing would be lost.
    ///
    /// `None` for an object, a custom-serialized class, an enum, a reference,
    /// a non-UTF-8 string, a non-finite float, or an array containing any of
    /// those. An array whose keys are exactly `0..n` in order becomes a JSON
    /// array — which is what PHP's `json_encode` does — and any other array
    /// an object with its keys as text.
    pub fn to_json(&self) -> Option<Json> {
        Some(match self {
            Value::Null => Json::Null,
            Value::Bool(b) => Json::Bool(*b),
            Value::Int(n) => Json::Number((*n).into()),
            Value::Float(f) => Json::Number(Number::from_f64(*f)?),
            Value::String(bytes) => Json::String(String::from_utf8(bytes.clone()).ok()?),
            Value::Array(entries) => {
                let is_list =
                    entries.iter().enumerate().all(|(i, (key, _))| *key == Key::Int(i as i64));
                if is_list {
                    Json::Array(
                        entries.iter().map(|(_, value)| value.to_json()).collect::<Option<_>>()?,
                    )
                } else {
                    let mut map = JsonMap::new();
                    for (key, value) in entries {
                        map.insert(key.to_text()?, value.to_json()?);
                    }
                    Json::Object(map)
                }
            }
            Value::Object(_)
            | Value::Custom { .. }
            | Value::Enum { .. }
            | Value::Reference { .. } => return None,
        })
    }

    /// The PHP value JSON describes — a JSON array becomes a list keyed
    /// `0..n`, an object an array keyed by its member names, a number an `i:`
    /// when it is an integer and a `d:` otherwise.
    pub fn from_json(json: &Json) -> Self {
        match json {
            Json::Null => Value::Null,
            Json::Bool(b) => Value::Bool(*b),
            Json::Number(n) => match n.as_i64() {
                Some(i) => Value::Int(i),
                None => Value::Float(n.as_f64().unwrap_or(0.0)),
            },
            Json::String(s) => Value::string(s),
            Json::Array(items) => Value::Array(
                items
                    .iter()
                    .enumerate()
                    .map(|(i, item)| (Key::Int(i as i64), Value::from_json(item)))
                    .collect(),
            ),
            Json::Object(members) => Value::Array(
                members
                    .iter()
                    .map(|(name, value)| (array_key(name), Value::from_json(value)))
                    .collect(),
            ),
        }
    }
}

/// The key PHP would store `name` under: a canonical decimal integer string
/// becomes an integer key, exactly as `$array["7"]` is `$array[7]`.
pub fn array_key(name: &str) -> Key {
    let canonical = name.parse::<i64>().ok().filter(|n| n.to_string() == name);
    match canonical {
        Some(n) => Key::Int(n),
        None => Key::str(name),
    }
}

/// How much a payload may ask for.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Nesting depth of arrays and objects. 64 by default — deeper than any
    /// real session, shallow enough that the recursion cannot exhaust a
    /// stack.
    pub max_depth: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self { max_depth: 64 }
    }
}

/// Read one value with the default [`Limits`]. Trailing bytes are an error.
pub fn unserialize(input: &[u8]) -> Result<Value> {
    unserialize_with(input, Limits::default())
}

/// Read one value with explicit limits.
pub fn unserialize_with(input: &[u8], limits: Limits) -> Result<Value> {
    let mut parser = Parser { input, at: 0, limits };
    let value = parser.value(0)?;
    if parser.at != input.len() {
        return Err(parser.error("trailing bytes after the value"));
    }
    Ok(value)
}

/// Write a value in PHP's format.
pub fn serialize(value: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    write(value, &mut out);
    out
}

fn write(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Null => out.extend_from_slice(b"N;"),
        Value::Bool(b) => out.extend_from_slice(if *b { b"b:1;" } else { b"b:0;" }),
        Value::Int(n) => out.extend_from_slice(format!("i:{n};").as_bytes()),
        Value::Float(f) => out.extend_from_slice(format!("d:{};", float(*f)).as_bytes()),
        Value::String(bytes) => write_string(bytes, out),
        Value::Array(entries) => {
            out.extend_from_slice(format!("a:{}:{{", entries.len()).as_bytes());
            for (key, value) in entries {
                write_key(key, out);
                write(value, out);
            }
            out.push(b'}');
        }
        Value::Object(object) => {
            out.extend_from_slice(
                format!(
                    "O:{}:\"{}\":{}:{{",
                    object.class.len(),
                    object.class,
                    object.properties.len()
                )
                .as_bytes(),
            );
            for (property, value) in &object.properties {
                write_string(&property.0, out);
                write(value, out);
            }
            out.push(b'}');
        }
        Value::Custom { class, data } => {
            out.extend_from_slice(
                format!("C:{}:\"{}\":{}:{{", class.len(), class, data.len()).as_bytes(),
            );
            out.extend_from_slice(data);
            out.push(b'}');
        }
        Value::Enum { class, case } => {
            let name = format!("{class}:{case}");
            out.extend_from_slice(format!("E:{}:\"{}\";", name.len(), name).as_bytes());
        }
        Value::Reference { slot, alias } => {
            out.extend_from_slice(format!("{}:{slot};", if *alias { 'R' } else { 'r' }).as_bytes())
        }
    }
}

fn write_string(bytes: &[u8], out: &mut Vec<u8>) {
    out.extend_from_slice(format!("s:{}:\"", bytes.len()).as_bytes());
    out.extend_from_slice(bytes);
    out.extend_from_slice(b"\";");
}

fn write_key(key: &Key, out: &mut Vec<u8>) {
    match key {
        Key::Int(n) => out.extend_from_slice(format!("i:{n};").as_bytes()),
        Key::Str(bytes) => write_string(bytes, out),
    }
}

/// A float the way PHP's parser reads it back.
fn float(f: f64) -> String {
    if f.is_nan() {
        "NAN".into()
    } else if f.is_infinite() {
        if f > 0.0 {
            "INF".into()
        } else {
            "-INF".into()
        }
    } else {
        // Rust's `Display` is the shortest string that round-trips.
        format!("{f}")
    }
}

struct Parser<'a> {
    input: &'a [u8],
    at: usize,
    limits: Limits,
}

impl Parser<'_> {
    fn error(&self, what: &str) -> Error {
        Error::bad_request(format!("not a PHP-serialized value: {what} at byte {}", self.at))
    }

    fn remaining(&self) -> usize {
        self.input.len() - self.at
    }

    fn peek(&self) -> Option<u8> {
        self.input.get(self.at).copied()
    }

    fn expect(&mut self, byte: u8) -> Result<()> {
        if self.peek() == Some(byte) {
            self.at += 1;
            Ok(())
        } else {
            Err(self.error(&format!("expected `{}`", byte as char)))
        }
    }

    /// Bytes up to (not including) `end`, consuming `end`.
    fn until(&mut self, end: u8) -> Result<&[u8]> {
        let start = self.at;
        let offset = self.input[start..]
            .iter()
            .position(|byte| *byte == end)
            .ok_or_else(|| self.error(&format!("unterminated, expected `{}`", end as char)))?;
        self.at = start + offset + 1;
        Ok(&self.input[start..start + offset])
    }

    fn int_until(&mut self, end: u8) -> Result<i64> {
        let digits = self.until(end)?;
        parse_int(digits).ok_or_else(|| self.error("bad integer"))
    }

    /// A length or count — non-negative, and no larger than what is left.
    fn length_until(&mut self, end: u8) -> Result<usize> {
        let n = self.int_until(end)?;
        let n = usize::try_from(n).map_err(|_| self.error("negative length"))?;
        if n > self.remaining() {
            return Err(self.error("a length longer than the payload"));
        }
        Ok(n)
    }

    fn take(&mut self, n: usize) -> Result<&[u8]> {
        if n > self.remaining() {
            return Err(self.error("truncated"));
        }
        let bytes = &self.input[self.at..self.at + n];
        self.at += n;
        Ok(bytes)
    }

    /// `len:"bytes"` — the quoted form a string, class name and enum share.
    fn quoted(&mut self) -> Result<Vec<u8>> {
        let len = self.length_until(b':')?;
        self.expect(b'"')?;
        let bytes = self.take(len)?.to_vec();
        self.expect(b'"')?;
        Ok(bytes)
    }

    fn name(&mut self) -> Result<String> {
        let bytes = self.quoted()?;
        String::from_utf8(bytes).map_err(|_| self.error("a class name that is not UTF-8"))
    }

    fn value(&mut self, depth: usize) -> Result<Value> {
        let tag = self.peek().ok_or_else(|| self.error("empty"))?;
        self.at += 1;

        if tag == b'N' {
            self.expect(b';')?;
            return Ok(Value::Null);
        }
        self.expect(b':')?;

        match tag {
            b'b' => match self.until(b';')? {
                b"0" => Ok(Value::Bool(false)),
                b"1" => Ok(Value::Bool(true)),
                _ => Err(self.error("bad boolean")),
            },
            b'i' => Ok(Value::Int(self.int_until(b';')?)),
            b'd' => {
                let text = self.until(b';')?;
                parse_float(text).map(Value::Float).ok_or_else(|| self.error("bad float"))
            }
            b's' => {
                let bytes = self.quoted()?;
                self.expect(b';')?;
                Ok(Value::String(bytes))
            }
            b'a' => {
                self.deeper(depth)?;
                let count = self.length_until(b':')?;
                self.expect(b'{')?;
                let mut entries = Vec::with_capacity(count.min(self.remaining() / 4));
                for _ in 0..count {
                    let key = self.key()?;
                    let value = self.value(depth + 1)?;
                    entries.push((key, value));
                }
                self.expect(b'}')?;
                Ok(Value::Array(entries))
            }
            b'O' => {
                self.deeper(depth)?;
                let class = self.name()?;
                self.expect(b':')?;
                let count = self.length_until(b':')?;
                self.expect(b'{')?;
                let mut properties = Vec::with_capacity(count.min(self.remaining() / 4));
                for _ in 0..count {
                    let name = match self.key()? {
                        Key::Str(bytes) => bytes,
                        // PHP accepts an integer-named property and makes it
                        // a string; so does this.
                        Key::Int(n) => n.to_string().into_bytes(),
                    };
                    let value = self.value(depth + 1)?;
                    properties.push((Property(name), value));
                }
                self.expect(b'}')?;
                Ok(Value::Object(Object { class, properties }))
            }
            b'C' => {
                let class = self.name()?;
                self.expect(b':')?;
                let len = self.length_until(b':')?;
                self.expect(b'{')?;
                let data = self.take(len)?.to_vec();
                self.expect(b'}')?;
                Ok(Value::Custom { class, data })
            }
            b'E' => {
                let name = self.name()?;
                self.expect(b';')?;
                let (class, case) =
                    name.split_once(':').ok_or_else(|| self.error("an enum without a case"))?;
                Ok(Value::Enum { class: class.into(), case: case.into() })
            }
            b'r' | b'R' => {
                let slot = self.int_until(b';')?;
                let slot = usize::try_from(slot)
                    .ok()
                    .filter(|slot| *slot > 0)
                    .ok_or_else(|| self.error("bad reference"))?;
                Ok(Value::Reference { slot, alias: tag == b'R' })
            }
            _ => Err(self.error(&format!("unknown type `{}`", tag as char))),
        }
    }

    /// An array key — only `i:` or `s:`, read directly so a key can never
    /// recurse into a container.
    fn key(&mut self) -> Result<Key> {
        match self.peek() {
            Some(b'i') => {
                self.at += 1;
                self.expect(b':')?;
                Ok(Key::Int(self.int_until(b';')?))
            }
            Some(b's') => {
                self.at += 1;
                self.expect(b':')?;
                let bytes = self.quoted()?;
                self.expect(b';')?;
                Ok(Key::Str(bytes))
            }
            _ => Err(self.error("an array key that is not an integer or a string")),
        }
    }

    fn deeper(&self, depth: usize) -> Result<()> {
        if depth >= self.limits.max_depth {
            return Err(self.error("nested too deeply"));
        }
        Ok(())
    }
}

fn parse_int(digits: &[u8]) -> Option<i64> {
    let text = std::str::from_utf8(digits).ok()?;
    let unsigned = text.strip_prefix(['+', '-']).unwrap_or(text);
    if unsigned.is_empty() || !unsigned.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.strip_prefix('+').unwrap_or(text).parse().ok()
}

fn parse_float(text: &[u8]) -> Option<f64> {
    let text = std::str::from_utf8(text).ok()?;
    match text {
        "INF" => return Some(f64::INFINITY),
        "-INF" => return Some(f64::NEG_INFINITY),
        "NAN" => return Some(f64::NAN),
        _ => {}
    }
    // Only the forms PHP writes — digits, one point, one exponent — so Rust's
    // looser `inf`/`nan` spellings are not accepted through the back door.
    let ok = !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'+' | b'-' | b'.' | b'e' | b'E'));
    if !ok {
        return None;
    }
    text.parse().ok()
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Key::Int(n) => write!(f, "{n}"),
            Key::Str(bytes) => write!(f, "{}", String::from_utf8_lossy(bytes)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(bytes: &[u8]) -> Value {
        let value = unserialize(bytes)
            .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(bytes)));
        assert_eq!(
            serialize(&value),
            bytes,
            "not byte-identical: {}",
            String::from_utf8_lossy(bytes)
        );
        value
    }

    #[test]
    fn scalars_read_and_write_back_byte_for_byte() {
        assert_eq!(round_trip(b"N;"), Value::Null);
        assert_eq!(round_trip(b"b:1;"), Value::Bool(true));
        assert_eq!(round_trip(b"b:0;"), Value::Bool(false));
        assert_eq!(round_trip(b"i:-42;"), Value::Int(-42));
        assert_eq!(round_trip(b"i:9223372036854775807;"), Value::Int(i64::MAX));
        assert_eq!(round_trip(b"d:0.5;"), Value::Float(0.5));
        assert_eq!(round_trip(b"s:0:\"\";"), Value::string(""));
    }

    #[test]
    fn a_string_length_is_in_bytes_and_may_contain_quotes_and_semicolons() {
        let value = round_trip("s:7:\"é\";\"x;\";".as_bytes());
        assert_eq!(value.as_str(), Some("é\";\"x;"));

        // Not UTF-8: kept, just not offered as text.
        let value = round_trip(b"s:2:\"\xff\xfe\";");
        assert_eq!(value.as_bytes(), Some(&b"\xff\xfe"[..]));
        assert_eq!(value.as_str(), None);
    }

    #[test]
    fn floats_in_every_form_php_writes() {
        assert_eq!(unserialize(b"d:1.0E+25;").unwrap(), Value::Float(1.0e25));
        assert_eq!(unserialize(b"d:-1.5E-7;").unwrap(), Value::Float(-1.5e-7));
        assert_eq!(unserialize(b"d:INF;").unwrap(), Value::Float(f64::INFINITY));
        assert_eq!(unserialize(b"d:-INF;").unwrap(), Value::Float(f64::NEG_INFINITY));
        assert!(matches!(unserialize(b"d:NAN;").unwrap(), Value::Float(f) if f.is_nan()));
        assert!(unserialize(b"d:inf;").is_err(), "Rust's spelling is not PHP's");

        // Written as something that reads back to the same double.
        for f in [0.1, 1.0, -0.0, 1e25, 1e-7, f64::MAX, f64::MIN_POSITIVE, 123456.789] {
            let back = unserialize(&serialize(&Value::Float(f))).unwrap();
            assert_eq!(back, Value::Float(f), "{f}");
        }
    }

    #[test]
    fn a_mixed_array_keeps_its_order_and_its_key_types() {
        let value = round_trip(br#"a:3:{i:0;s:1:"a";s:3:"key";i:1;i:7;a:0:{}}"#);
        let entries = value.as_array().unwrap();
        assert_eq!(entries[0].0, Key::Int(0));
        assert_eq!(entries[1].0, Key::str("key"));
        assert_eq!(value.get("7"), Some(&Value::Array(vec![])), "\"7\" finds the integer key 7");
    }

    #[test]
    fn objects_keep_their_class_and_mangled_property_names() {
        let bytes = b"O:4:\"User\":3:{s:4:\"name\";s:3:\"Ada\";s:6:\"\0*\0age\";i:36;s:10:\"\0User\0pass\";s:6:\"secret\";}";
        let value = round_trip(bytes);
        let object = value.as_object().unwrap();

        assert_eq!(object.class, "User");
        assert_eq!(object.properties[0].0.visibility(), Visibility::Public);
        assert_eq!(object.properties[1].0.visibility(), Visibility::Protected);
        assert_eq!(object.properties[1].0.name(), "age");
        assert_eq!(object.properties[2].0.visibility(), Visibility::Private("User".into()));
        assert_eq!(object.properties[2].0.name(), "pass");
        assert_eq!(value.get("age").and_then(Value::as_int), Some(36));

        assert_eq!(Property::protected("age"), object.properties[1].0);
        assert_eq!(Property::private("User", "pass"), object.properties[2].0);
    }

    #[test]
    fn custom_serialized_classes_enums_and_references_round_trip() {
        let custom = round_trip(br#"C:11:"ArrayObject":21:{x:i:0;a:0:{};m:a:0:{}}"#);
        assert!(
            matches!(&custom, Value::Custom { class, data } if class == "ArrayObject" && data.len() == 21)
        );

        let case = round_trip(br#"E:11:"Suit:Hearts";"#);
        assert_eq!(case, Value::Enum { class: "Suit".into(), case: "Hearts".into() });

        let refs = round_trip(br#"a:2:{i:0;s:1:"x";i:1;R:2;}"#);
        assert_eq!(refs.get("1"), Some(&Value::Reference { slot: 2, alias: true }));
        round_trip(br#"a:1:{i:0;r:1;}"#);
    }

    #[test]
    fn nothing_is_lost_converting_plain_data_to_json_and_back() {
        let value = unserialize(
            br#"a:3:{s:5:"items";a:2:{i:0;i:1;i:1;d:2.5;}s:4:"name";s:3:"Ada";s:4:"none";N;}"#,
        )
        .unwrap();
        let json = value.to_json().unwrap();
        assert_eq!(json, serde_json::json!({ "items": [1, 2.5], "name": "Ada", "none": null }));
        assert_eq!(Value::from_json(&json), value);

        // A sparse integer-keyed array is not a list.
        let sparse = unserialize(br#"a:1:{i:3;b:1;}"#).unwrap();
        assert_eq!(sparse.to_json().unwrap(), serde_json::json!({ "3": true }));

        // An object has no lossless JSON form.
        assert!(unserialize(br#"O:8:"stdClass":0:{}"#).unwrap().to_json().is_none());
    }

    #[test]
    fn a_numeric_string_key_becomes_an_integer_key_as_php_does() {
        assert_eq!(array_key("7"), Key::Int(7));
        assert_eq!(array_key("07"), Key::str("07"));
        assert_eq!(array_key("-3"), Key::Int(-3));
        assert_eq!(array_key("x"), Key::str("x"));
    }

    #[test]
    fn hostile_payloads_are_refused_without_allocating() {
        // A string that claims four gigabytes.
        assert!(unserialize(b"s:4294967296:\"x\";").is_err());
        // A billion elements.
        assert!(unserialize(b"a:1000000000:{}").is_err());
        // Negative lengths.
        assert!(unserialize(b"s:-1:\"\";").is_err());
        assert!(unserialize(b"a:-1:{}").is_err());
        // Ten thousand levels of nesting.
        let deep = "a:1:{i:0;".repeat(10_000) + "N;" + &"}".repeat(10_000);
        assert!(unserialize(deep.as_bytes()).is_err());
        // An array as a key.
        assert!(unserialize(b"a:1:{a:0:{}i:1;}").is_err());
        // A reference to slot zero.
        assert!(unserialize(b"r:0;").is_err());
        // Truncations and trailing garbage.
        for bad in
            [&b"s:5:\"abc\";"[..], b"i:1", b"a:1:{i:0;", b"N;N;", b"", b"x:1;", b"i:1x;", b"i:;"]
        {
            assert!(unserialize(bad).is_err(), "{}", String::from_utf8_lossy(bad));
        }
    }

    #[test]
    fn the_depth_limit_is_adjustable_and_exact() {
        let three = b"a:1:{i:0;a:1:{i:0;a:0:{}}}";
        assert!(unserialize_with(three, Limits { max_depth: 3 }).is_ok());
        assert!(unserialize_with(three, Limits { max_depth: 2 }).is_err());
    }

    /// A small deterministic generator, so the property test needs no
    /// dependency and fails the same way every time.
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            self.0 >> 33
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    fn arbitrary(rng: &mut Lcg, depth: usize) -> Value {
        let choice = if depth > 3 { rng.below(5) } else { rng.below(9) };
        match choice {
            0 => Value::Null,
            1 => Value::Bool(rng.below(2) == 1),
            2 => Value::Int(rng.next() as i64 - (1 << 30)),
            3 => Value::Float((rng.next() as f64) / 7.0 - 1e6),
            4 => Value::String((0..rng.below(12)).map(|_| rng.below(256) as u8).collect()),
            5 | 6 => Value::Array(
                (0..rng.below(5))
                    .map(|i| {
                        let key = if rng.below(2) == 0 {
                            Key::Int(i as i64)
                        } else {
                            Key::Str(format!("k{}\"; {}", i, rng.below(100)).into_bytes())
                        };
                        (key, arbitrary(rng, depth + 1))
                    })
                    .collect(),
            ),
            7 => Value::Object(Object {
                class: format!("App\\Model{}", rng.below(10)),
                properties: (0..rng.below(4))
                    .map(|i| {
                        let name = format!("p{i}");
                        let property = match rng.below(3) {
                            0 => Property::public(&name),
                            1 => Property::protected(&name),
                            _ => Property::private("App\\Secret", &name),
                        };
                        (property, arbitrary(rng, depth + 1))
                    })
                    .collect(),
            }),
            _ => Value::Enum { class: "Status".into(), case: format!("Case{}", rng.below(4)) },
        }
    }

    #[test]
    fn arbitrary_values_round_trip() {
        let mut rng = Lcg(0x5eed);
        for _ in 0..2_000 {
            let value = arbitrary(&mut rng, 0);
            let bytes = serialize(&value);
            let back = unserialize(&bytes)
                .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&bytes)));
            assert_eq!(back, value);
            assert_eq!(serialize(&back), bytes);
        }
    }

    #[test]
    fn arbitrary_truncations_are_errors_not_panics() {
        let mut rng = Lcg(42);
        for _ in 0..300 {
            let bytes = serialize(&arbitrary(&mut rng, 0));
            for cut in 0..bytes.len() {
                let _ = unserialize(&bytes[..cut]);
            }
            // And single-byte corruptions.
            for at in 0..bytes.len() {
                let mut corrupt = bytes.clone();
                corrupt[at] ^= 0x5a;
                let _ = unserialize(&corrupt);
            }
        }
    }
}
