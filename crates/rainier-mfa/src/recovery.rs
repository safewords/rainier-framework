//! Single-use recovery codes — the way back in when the phone is gone.
//!
//! ```
//! use rainier_mfa::recovery;
//!
//! // Shown to the user once, at enrolment; only the hashes are stored.
//! let codes = recovery::generate(8);
//! let mut stored: Vec<String> = codes.iter().map(|c| recovery::hash(c)).collect();
//!
//! assert!(recovery::consume(&mut stored, &codes[0]));
//! assert!(!recovery::consume(&mut stored, &codes[0]), "single use");
//! assert_eq!(stored.len(), 7);
//! ```
//!
//! # Stored as hashes
//!
//! A recovery code is a password that bypasses the second factor, so a table
//! of them in plaintext is a table of second-factor bypasses for whoever reads
//! a backup. Unlike a password it has ~119 bits of entropy, so a plain SHA-256
//! is enough — there is no dictionary to slow down, and a slow hash would only
//! make every login with a code slower.
//!
//! [`consume_plain`] exists for codes an earlier system stored readable (or
//! encrypted, decrypted by the caller): use it to accept them while
//! re-storing them hashed, not to keep storing them that way.
//!
//! # The format
//!
//! Two groups of ten alphanumerics, `abcDEF1234-XYZ9876qwe`: long enough to
//! need no throttle to be unguessable, short enough to read off paper. Codes
//! are compared exactly — they are mixed case — after trimming the whitespace
//! a paste brings with it.

use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

const ALPHABET: &[u8; 62] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

/// `count` fresh codes.
pub fn generate(count: usize) -> Vec<String> {
    (0..count).map(|_| format!("{}-{}", group(), group())).collect()
}

fn group() -> String {
    // Rejection sampling over bytes, so no character is likelier than another:
    // 256 is not a multiple of 62, and `byte % 62` would favour the first 8.
    let mut out = String::with_capacity(10);
    while out.len() < 10 {
        for byte in crate::random_bytes(16) {
            if byte < 248 && out.len() < 10 {
                out.push(ALPHABET[(byte % 62) as usize] as char);
            }
        }
    }
    out
}

/// The form to store: lowercase hex SHA-256 of the trimmed code.
pub fn hash(code: &str) -> String {
    Sha256::digest(code.trim().as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// Spend `submitted` against stored hashes, removing it on a match.
///
/// Every stored hash is compared, in constant time, whether or not an earlier
/// one matched — so the timing does not say how far down the list a code is.
pub fn consume(stored_hashes: &mut Vec<String>, submitted: &str) -> bool {
    let candidate = hash(submitted);
    spend(stored_hashes, candidate.as_bytes(), |stored| stored.as_bytes().to_vec())
}

/// Spend `submitted` against codes stored as they were issued.
///
/// For migrating from a system that kept them readable; see the module
/// documentation.
pub fn consume_plain(stored_codes: &mut Vec<String>, submitted: &str) -> bool {
    let candidate = submitted.trim().to_string();
    spend(stored_codes, candidate.as_bytes(), |stored| stored.trim().as_bytes().to_vec())
}

fn spend(stored: &mut Vec<String>, candidate: &[u8], form: impl Fn(&str) -> Vec<u8>) -> bool {
    if candidate.is_empty() {
        return false;
    }

    let mut found = None;
    for (index, entry) in stored.iter().enumerate() {
        let equal: bool = form(entry).ct_eq(candidate).into();
        if equal && found.is_none() {
            found = Some(index);
        }
    }

    match found {
        Some(index) => {
            stored.remove(index);
            true
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_have_the_documented_shape_and_do_not_repeat() {
        let codes = generate(50);
        assert_eq!(codes.len(), 50);
        for code in &codes {
            let (left, right) = code.split_once('-').unwrap();
            assert_eq!(left.len(), 10);
            assert_eq!(right.len(), 10);
            assert!(left.chars().chain(right.chars()).all(|c| c.is_ascii_alphanumeric()));
        }
        let unique: std::collections::BTreeSet<_> = codes.iter().collect();
        assert_eq!(unique.len(), codes.len());
    }

    #[test]
    fn a_code_is_spent_once_and_only_its_own_hash_is_removed() {
        let codes = generate(3);
        let mut stored: Vec<String> = codes.iter().map(|c| hash(c)).collect();

        assert!(consume(&mut stored, &format!("  {}\n", codes[1])), "pasted with whitespace");
        assert_eq!(stored, vec![hash(&codes[0]), hash(&codes[2])]);
        assert!(!consume(&mut stored, &codes[1]));
    }

    #[test]
    fn a_wrong_or_empty_code_spends_nothing() {
        let mut stored: Vec<String> = generate(2).iter().map(|c| hash(c)).collect();
        assert!(!consume(&mut stored, "AAAAAAAAAA-AAAAAAAAAA"));
        assert!(!consume(&mut stored, ""));
        assert!(!consume_plain(&mut vec![String::new()], "  "));
        assert_eq!(stored.len(), 2);
    }

    #[test]
    fn codes_are_case_sensitive() {
        let mut stored = vec![hash("abcDEF1234-XYZ9876qwe")];
        assert!(!consume(&mut stored, "ABCDEF1234-XYZ9876QWE"));
        assert!(consume(&mut stored, "abcDEF1234-XYZ9876qwe"));
    }

    #[test]
    fn plaintext_codes_from_an_earlier_system_can_be_spent() {
        let mut stored = vec!["one-code".to_string(), "two-code".to_string()];
        assert!(consume_plain(&mut stored, "two-code"));
        assert_eq!(stored, vec!["one-code".to_string()]);
    }

    #[test]
    fn the_hash_is_hex_sha256() {
        assert_eq!(hash("abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }
}
