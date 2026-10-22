//! RFC 4648 base32 — the alphabet every authenticator app reads a secret in.
//!
//! Encoding writes no padding, because an `otpauth://` URI with `=` in its
//! `secret` is one some apps refuse. Decoding is lenient in exactly the ways a
//! human copying a secret by hand is sloppy — lowercase, spaces, dashes,
//! trailing padding — and strict about everything else: a character outside
//! the alphabet is an error, never skipped, because a secret silently missing
//! a character produces codes that are wrong forever.

use rainier_support::{Error, Result};

const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// Encode `bytes` as unpadded, uppercase base32.
pub fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(5) * 8);
    let mut buffer: u64 = 0;
    let mut bits = 0u32;

    for &byte in bytes {
        buffer = (buffer << 8) | u64::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((buffer >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((buffer << (5 - bits)) & 31) as usize] as char);
    }

    out
}

/// Decode base32, ignoring case, whitespace, `-` and trailing `=`.
///
/// # Errors
///
/// A character outside the alphabet, or a length no encoder produces.
pub fn decode(text: &str) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 5 / 8);
    let mut buffer: u64 = 0;
    let mut bits = 0u32;
    let mut symbols = 0usize;

    for character in text.trim_end_matches('=').chars() {
        if character.is_whitespace() || character == '-' {
            continue;
        }
        let value = match character.to_ascii_uppercase() {
            c @ 'A'..='Z' => c as u64 - 'A' as u64,
            c @ '2'..='7' => c as u64 - '2' as u64 + 26,
            other => {
                return Err(Error::bad_request(format!("`{other}` is not a base32 character")))
            }
        };
        symbols += 1;
        buffer = (buffer << 5) | value;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }

    // 1, 3 and 6 trailing symbols cannot come from whole bytes.
    if matches!(symbols % 8, 1 | 3 | 6) {
        return Err(Error::bad_request("that is not a whole number of base32-encoded bytes"));
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc_4648_vectors() {
        for (plain, encoded) in [
            ("", ""),
            ("f", "MY"),
            ("fo", "MZXQ"),
            ("foo", "MZXW6"),
            ("foob", "MZXW6YQ"),
            ("fooba", "MZXW6YTB"),
            ("foobar", "MZXW6YTBOI"),
        ] {
            assert_eq!(encode(plain.as_bytes()), encoded);
            assert_eq!(decode(encoded).unwrap(), plain.as_bytes());
        }
    }

    #[test]
    fn decoding_forgives_what_a_human_does_to_a_secret() {
        assert_eq!(decode("mzxw 6ytb-oi======").unwrap(), b"foobar");
    }

    #[test]
    fn decoding_refuses_what_no_encoder_wrote() {
        assert!(decode("MZXW1").is_err(), "1 is not in the alphabet");
        assert!(decode("M").is_err(), "one symbol is not a byte");
        assert!(decode("MZX").is_err());
    }

    #[test]
    fn round_trips_arbitrary_bytes() {
        let bytes: Vec<u8> = (0..=255).collect();
        assert_eq!(decode(&encode(&bytes)).unwrap(), bytes);
    }
}
