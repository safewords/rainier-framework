//! The CBOR a WebAuthn authenticator speaks — a small, strict [`decode`]r.
//!
//! An attestation object and a COSE key use a sliver of CBOR (RFC 8949):
//! unsigned and negative integers, byte and text strings, arrays, maps,
//! booleans and null, all with definite lengths. CTAP2 requires authenticators
//! to emit exactly that, canonically. So this reads that sliver and **refuses
//! the rest** — floats, tags, indefinite lengths, duplicate map keys — rather
//! than carrying a general-purpose decoder whose every extra feature is
//! surface for input the server never needed to accept.
//!
//! It is also bounded: nesting is limited, and no length is believed until
//! the bytes behind it are known to exist, so a five-byte input claiming a
//! four-gigabyte string is an error and not an allocation.
//!
//! [`encode`] writes the same subset canonically. The server never needs it —
//! it exists for [`SoftwareAuthenticator`](crate::webauthn::testing::SoftwareAuthenticator),
//! which has to produce what a real authenticator would.

use rainier_support::{Error, Result};

/// How deep arrays and maps may nest. A COSE key is depth 1, an attestation
/// object with a `packed` statement depth 3.
const MAX_DEPTH: usize = 16;

/// The most entries one array or map may claim.
const MAX_ITEMS: u64 = 4096;

/// A decoded value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// A non-negative integer.
    Unsigned(u64),
    /// A negative integer, `-1 - n` for the encoded `n`.
    Negative(i128),
    /// A byte string.
    Bytes(Vec<u8>),
    /// A UTF-8 text string.
    Text(String),
    /// An array.
    Array(Vec<Value>),
    /// A map, in encoded order. Keys are unique — [`decode`] refuses
    /// duplicates.
    Map(Vec<(Value, Value)>),
    /// `true` or `false`.
    Bool(bool),
    /// `null`.
    Null,
}

impl Value {
    /// This value as an integer, if it is one.
    pub fn as_int(&self) -> Option<i128> {
        match self {
            Self::Unsigned(n) => Some(i128::from(*n)),
            Self::Negative(n) => Some(*n),
            _ => None,
        }
    }

    /// This value as bytes, if it is a byte string.
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Bytes(bytes) => Some(bytes),
            _ => None,
        }
    }

    /// This value as text, if it is a text string.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(text) => Some(text),
            _ => None,
        }
    }

    /// The entries, if this is a map.
    pub fn as_map(&self) -> Option<&[(Value, Value)]> {
        match self {
            Self::Map(entries) => Some(entries),
            _ => None,
        }
    }

    /// The value under an integer key — how COSE keys are addressed.
    pub fn get_int(&self, key: i128) -> Option<&Value> {
        self.as_map()?.iter().find(|(k, _)| k.as_int() == Some(key)).map(|(_, v)| v)
    }

    /// The value under a text key — how an attestation object is addressed.
    pub fn get_text(&self, key: &str) -> Option<&Value> {
        self.as_map()?.iter().find(|(k, _)| k.as_text() == Some(key)).map(|(_, v)| v)
    }
}

/// Decode exactly one value occupying all of `bytes`.
///
/// # Errors
///
/// Malformed or unsupported CBOR, or bytes left over after the value.
pub fn decode(bytes: &[u8]) -> Result<Value> {
    let (value, used) = decode_prefix(bytes)?;
    if used != bytes.len() {
        return Err(invalid("trailing bytes after the value"));
    }
    Ok(value)
}

/// Decode one value from the front of `bytes`, returning it and how many
/// bytes it occupied — for a COSE key embedded in authenticator data, where
/// extensions may follow it.
///
/// # Errors
///
/// Malformed or unsupported CBOR.
pub fn decode_prefix(bytes: &[u8]) -> Result<(Value, usize)> {
    let mut reader = Reader { bytes, at: 0 };
    let value = reader.value(0)?;
    Ok((value, reader.at))
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn take(&mut self, len: usize) -> Result<&[u8]> {
        let end = self.at.checked_add(len).filter(|end| *end <= self.bytes.len());
        let end = end.ok_or_else(|| invalid("the input ends inside a value"))?;
        let slice = &self.bytes[self.at..end];
        self.at = end;
        Ok(slice)
    }

    fn argument(&mut self, info: u8) -> Result<u64> {
        let value = match info {
            0..=23 => return Ok(u64::from(info)),
            24 => u64::from(self.take(1)?[0]),
            25 => u64::from(u16::from_be_bytes(self.take(2)?.try_into().expect("two bytes"))),
            26 => u64::from(u32::from_be_bytes(self.take(4)?.try_into().expect("four bytes"))),
            27 => u64::from_be_bytes(self.take(8)?.try_into().expect("eight bytes")),
            31 => return Err(invalid("indefinite lengths are not accepted")),
            _ => return Err(invalid("a reserved length encoding")),
        };
        Ok(value)
    }

    fn length(&mut self, info: u8) -> Result<usize> {
        let length = self.argument(info)?;
        // Not believed until the bytes exist: a claim longer than what is
        // left is malformed, whatever it says.
        if length > (self.bytes.len() - self.at) as u64 {
            return Err(invalid("a length longer than the input"));
        }
        Ok(length as usize)
    }

    fn count(&mut self, info: u8) -> Result<usize> {
        let count = self.argument(info)?;
        if count > MAX_ITEMS || count > (self.bytes.len() - self.at) as u64 {
            return Err(invalid("a container claiming more entries than it can hold"));
        }
        Ok(count as usize)
    }

    fn value(&mut self, depth: usize) -> Result<Value> {
        if depth > MAX_DEPTH {
            return Err(invalid("nested too deeply"));
        }

        let initial = self.take(1)?[0];
        let (major, info) = (initial >> 5, initial & 0x1f);

        Ok(match major {
            0 => Value::Unsigned(self.argument(info)?),
            1 => Value::Negative(-1 - i128::from(self.argument(info)?)),
            2 => {
                let len = self.length(info)?;
                Value::Bytes(self.take(len)?.to_vec())
            }
            3 => {
                let len = self.length(info)?;
                let text = std::str::from_utf8(self.take(len)?)
                    .map_err(|_| invalid("a text string that is not UTF-8"))?;
                Value::Text(text.to_string())
            }
            4 => {
                let count = self.count(info)?;
                let mut items = Vec::with_capacity(count);
                for _ in 0..count {
                    items.push(self.value(depth + 1)?);
                }
                Value::Array(items)
            }
            5 => {
                let count = self.count(info)?;
                let mut entries: Vec<(Value, Value)> = Vec::with_capacity(count);
                for _ in 0..count {
                    let key = self.value(depth + 1)?;
                    if entries.iter().any(|(existing, _)| *existing == key) {
                        return Err(invalid("a map with a duplicate key"));
                    }
                    let value = self.value(depth + 1)?;
                    entries.push((key, value));
                }
                Value::Map(entries)
            }
            6 => return Err(invalid("tags are not accepted")),
            _ => match info {
                20 => Value::Bool(false),
                21 => Value::Bool(true),
                22 => Value::Null,
                _ => return Err(invalid("floats and simple values are not accepted")),
            },
        })
    }
}

/// Encode `value` canonically — shortest lengths, entries in the order given.
pub fn encode(value: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    write(&mut out, value);
    out
}

fn head(out: &mut Vec<u8>, major: u8, argument: u64) {
    let major = major << 5;
    match argument {
        0..=23 => out.push(major | argument as u8),
        24..=0xff => out.extend([major | 24, argument as u8]),
        0x100..=0xffff => {
            out.push(major | 25);
            out.extend((argument as u16).to_be_bytes());
        }
        0x1_0000..=0xffff_ffff => {
            out.push(major | 26);
            out.extend((argument as u32).to_be_bytes());
        }
        _ => {
            out.push(major | 27);
            out.extend(argument.to_be_bytes());
        }
    }
}

fn write(out: &mut Vec<u8>, value: &Value) {
    match value {
        Value::Unsigned(n) => head(out, 0, *n),
        Value::Negative(n) => head(out, 1, (-1 - n) as u64),
        Value::Bytes(bytes) => {
            head(out, 2, bytes.len() as u64);
            out.extend(bytes);
        }
        Value::Text(text) => {
            head(out, 3, text.len() as u64);
            out.extend(text.as_bytes());
        }
        Value::Array(items) => {
            head(out, 4, items.len() as u64);
            for item in items {
                write(out, item);
            }
        }
        Value::Map(entries) => {
            head(out, 5, entries.len() as u64);
            for (key, value) in entries {
                write(out, key);
                write(out, value);
            }
        }
        Value::Bool(false) => out.push(0xf4),
        Value::Bool(true) => out.push(0xf5),
        Value::Null => out.push(0xf6),
    }
}

fn invalid(why: &str) -> Error {
    Error::bad_request(format!("invalid CBOR: {why}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn rfc_8949_appendix_a_examples_in_the_subset() {
        assert_eq!(decode(&hex("00")).unwrap(), Value::Unsigned(0));
        assert_eq!(decode(&hex("1864")).unwrap(), Value::Unsigned(100));
        assert_eq!(decode(&hex("1a000f4240")).unwrap(), Value::Unsigned(1_000_000));
        assert_eq!(decode(&hex("1bffffffffffffffff")).unwrap(), Value::Unsigned(u64::MAX));
        assert_eq!(decode(&hex("20")).unwrap(), Value::Negative(-1));
        assert_eq!(decode(&hex("3863")).unwrap(), Value::Negative(-100));
        assert_eq!(
            decode(&hex("3bffffffffffffffff")).unwrap(),
            Value::Negative(-18446744073709551616)
        );
        assert_eq!(decode(&hex("4401020304")).unwrap(), Value::Bytes(vec![1, 2, 3, 4]));
        assert_eq!(decode(&hex("6449455446")).unwrap(), Value::Text("IETF".into()));
        assert_eq!(decode(&hex("f4")).unwrap(), Value::Bool(false));
        assert_eq!(decode(&hex("f6")).unwrap(), Value::Null);
        assert_eq!(
            decode(&hex("a201020304")).unwrap(),
            Value::Map(vec![
                (Value::Unsigned(1), Value::Unsigned(2)),
                (Value::Unsigned(3), Value::Unsigned(4)),
            ])
        );
        assert_eq!(
            decode(&hex("8301820203820405")).unwrap(),
            Value::Array(vec![
                Value::Unsigned(1),
                Value::Array(vec![Value::Unsigned(2), Value::Unsigned(3)]),
                Value::Array(vec![Value::Unsigned(4), Value::Unsigned(5)]),
            ])
        );
    }

    #[test]
    fn what_is_outside_the_subset_is_refused() {
        for (bytes, why) in [
            ("f93c00", "a half float"),
            ("c11a514b67b0", "a tag"),
            ("5f42010243030405ff", "an indefinite byte string"),
            ("9fff", "an indefinite array"),
            ("a201020103", "a duplicate key"),
            ("1c", "a reserved length"),
        ] {
            assert!(decode(&hex(bytes)).is_err(), "{why} should be refused");
        }
    }

    #[test]
    fn lengths_are_not_believed_beyond_the_input() {
        assert!(decode(&hex("5affffffff00")).is_err(), "a 4 GiB string in six bytes");
        assert!(decode(&hex("9affffffff")).is_err(), "four billion entries in five bytes");
        assert!(decode(&hex("62ff")).is_err());
        assert!(decode(&hex("62c328")).is_err(), "not UTF-8");
    }

    #[test]
    fn nesting_is_bounded() {
        let deep = vec![0x81u8; MAX_DEPTH + 2].into_iter().chain([0x00]).collect::<Vec<_>>();
        assert!(decode(&deep).is_err());
    }

    #[test]
    fn a_prefix_reports_how_much_it_used_and_a_whole_decode_refuses_leftovers() {
        let (value, used) = decode_prefix(&hex("0102")).unwrap();
        assert_eq!((value, used), (Value::Unsigned(1), 1));
        assert!(decode(&hex("0102")).is_err());
    }

    #[test]
    fn encoding_round_trips_canonically() {
        let value = Value::Map(vec![
            (Value::Unsigned(1), Value::Unsigned(2)),
            (Value::Unsigned(3), Value::Negative(-7)),
            (Value::Negative(-1), Value::Unsigned(1)),
            (Value::Negative(-2), Value::Bytes(vec![0xab; 32])),
            (Value::Text("fmt".into()), Value::Text("none".into())),
            (Value::Text("big".into()), Value::Unsigned(70_000)),
            (Value::Text("list".into()), Value::Array(vec![Value::Bool(true), Value::Null])),
        ]);
        let bytes = encode(&value);
        assert_eq!(decode(&bytes).unwrap(), value);
        assert_eq!(&encode(&Value::Unsigned(100)), &hex("1864"));
        assert_eq!(&encode(&Value::Negative(-100)), &hex("3863"));
    }

    #[test]
    fn keyed_lookups() {
        let map = Value::Map(vec![
            (Value::Negative(-1), Value::Unsigned(1)),
            (Value::Text("fmt".into()), Value::Text("none".into())),
        ]);
        assert_eq!(map.get_int(-1), Some(&Value::Unsigned(1)));
        assert_eq!(map.get_text("fmt").and_then(Value::as_text), Some("none"));
        assert_eq!(map.get_int(3), None);
    }
}
