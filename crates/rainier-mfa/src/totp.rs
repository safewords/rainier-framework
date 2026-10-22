//! Time-based one-time passwords — [`Totp`] and its [`Secret`].
//!
//! ```
//! use rainier_mfa::totp::{Secret, Totp};
//!
//! let totp = Totp::new().issuer("Acme");
//! let secret = Secret::generate();
//!
//! // Shown once, as a QR code, when the user turns two-factor on:
//! let uri = totp.provisioning_uri(&secret, "ada@example.com");
//! assert!(uri.starts_with("otpauth://totp/Acme:ada%40example.com?secret="));
//!
//! // Then, at every login — `last_used_step` is the value the previous
//! // success returned, persisted beside the secret:
//! let code = totp.code_at(&secret, 1_700_000_000);
//! let step = totp.verify_at(&secret, &code, 1_700_000_000, None).unwrap();
//! assert!(totp.verify_at(&secret, &code, 1_700_000_000, Some(step)).is_none(), "spent");
//! ```
//!
//! # Replay is the part most implementations forget
//!
//! A TOTP code is valid for its whole 30-second step — and, with the usual
//! one-step window, for ninety seconds. Without more, a code read over a
//! shoulder or lifted from a phishing page can be replayed for that long.
//! RFC 6238 §5.2 says a verifier **must not** accept the same code twice, so
//! [`verify`](Totp::verify) takes the last step that was accepted and refuses
//! anything at or before it. That is google2fa's `verifyKeyNewer`.
//!
//! The persisted step closes replay across requests that run one after the
//! other. Two requests racing with the same code both read the same old step;
//! closing that needs an atomic claim on the step (the accounts scaffolding
//! uses the cache's `add`).
//!
//! # Compatible with what the apps compute
//!
//! HMAC-SHA1, six digits, thirty seconds is what every authenticator app
//! assumes when the URI says nothing else, and what a secret written by
//! `pragmarx/google2fa` was enrolled with. The other algorithms and lengths
//! are here because RFC 6238 defines them, and some hardware tokens use them;
//! several popular apps silently ignore them, so do not change them for an
//! existing enrolment.

use std::time::{SystemTime, UNIX_EPOCH};

use hmac::{Hmac, KeyInit, Mac};
use rainier_support::{Error, Result};
use subtle::ConstantTimeEq;

use crate::base32;

/// The HMAC under the code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Algorithm {
    /// HMAC-SHA1 — the default, and the only one every app supports.
    #[default]
    Sha1,
    /// HMAC-SHA256.
    Sha256,
    /// HMAC-SHA512.
    Sha512,
}

impl Algorithm {
    /// The name an `otpauth://` URI uses.
    pub fn name(self) -> &'static str {
        match self {
            Self::Sha1 => "SHA1",
            Self::Sha256 => "SHA256",
            Self::Sha512 => "SHA512",
        }
    }

    fn mac(self, key: &[u8], message: &[u8]) -> Vec<u8> {
        macro_rules! run {
            ($digest:ty) => {{
                let mut mac =
                    <Hmac<$digest>>::new_from_slice(key).expect("HMAC accepts a key of any length");
                mac.update(message);
                mac.finalize().into_bytes().to_vec()
            }};
        }
        match self {
            Self::Sha1 => run!(sha1::Sha1),
            Self::Sha256 => run!(sha2::Sha256),
            Self::Sha512 => run!(sha2::Sha512),
        }
    }
}

/// A shared secret.
///
/// The bytes, not the base32 text — so two spellings of one secret (`abcd`
/// and `ABCD`, with or without spaces) are the same secret. Stays out of
/// `Debug` output, because a secret in a log line is a second factor in a log
/// line.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(Vec<u8>);

impl Secret {
    /// 160 random bits — RFC 4226's recommended length, and what google2fa's
    /// `generateSecretKey(32)` produces (32 base32 characters).
    pub fn generate() -> Self {
        Self(crate::random_bytes(20))
    }

    /// From raw bytes.
    pub fn from_bytes(bytes: impl Into<Vec<u8>>) -> Self {
        Self(bytes.into())
    }

    /// From the base32 text an app, a QR code or a PHP application's
    /// `otp_secret` column holds.
    ///
    /// # Errors
    ///
    /// Not base32, or empty.
    pub fn from_base32(text: &str) -> Result<Self> {
        let bytes = base32::decode(text)?;
        if bytes.is_empty() {
            return Err(Error::bad_request("a TOTP secret cannot be empty"));
        }
        Ok(Self(bytes))
    }

    /// The base32 form — what to store, and what a user types if they cannot
    /// scan.
    pub fn to_base32(&self) -> String {
        base32::encode(&self.0)
    }

    /// The raw bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret([redacted])")
    }
}

/// An HOTP code (RFC 4226) — the counter-based primitive TOTP is built on.
pub fn hotp(algorithm: Algorithm, key: &[u8], counter: u64, digits: u32) -> String {
    let mac = algorithm.mac(key, &counter.to_be_bytes());

    // Dynamic truncation, RFC 4226 §5.3.
    let offset = (mac[mac.len() - 1] & 0x0f) as usize;
    let binary = (u32::from(mac[offset]) & 0x7f) << 24
        | u32::from(mac[offset + 1]) << 16
        | u32::from(mac[offset + 2]) << 8
        | u32::from(mac[offset + 3]);

    let modulus = 10u32.pow(digits);
    format!("{:0width$}", binary % modulus, width = digits as usize)
}

/// TOTP settings: algorithm, length, step and tolerance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Totp {
    algorithm: Algorithm,
    digits: u32,
    period: u64,
    window: u64,
    issuer: Option<String>,
}

impl Default for Totp {
    fn default() -> Self {
        Self::new()
    }
}

impl Totp {
    /// SHA1, six digits, thirty-second steps, one step of tolerance either
    /// side — what the apps assume.
    pub fn new() -> Self {
        Self { algorithm: Algorithm::Sha1, digits: 6, period: 30, window: 1, issuer: None }
    }

    /// The HMAC algorithm.
    #[must_use]
    pub fn algorithm(mut self, algorithm: Algorithm) -> Self {
        self.algorithm = algorithm;
        self
    }

    /// Code length, 6 to 8.
    ///
    /// # Panics
    ///
    /// Outside 6..=8 — a programming error, caught at boot.
    #[must_use]
    pub fn digits(mut self, digits: u32) -> Self {
        assert!((6..=8).contains(&digits), "a TOTP code is 6 to 8 digits, not {digits}");
        self.digits = digits;
        self
    }

    /// Step length in seconds.
    ///
    /// # Panics
    ///
    /// On zero.
    #[must_use]
    pub fn period(mut self, seconds: u64) -> Self {
        assert!(seconds > 0, "a TOTP step cannot be zero seconds");
        self.period = seconds;
        self
    }

    /// Steps of clock drift tolerated either side. `1` (the default) accepts
    /// the previous, current and next code; every extra step widens a
    /// guesser's odds by two codes.
    #[must_use]
    pub fn window(mut self, steps: u64) -> Self {
        self.window = steps;
        self
    }

    /// The name an authenticator app files the entry under.
    #[must_use]
    pub fn issuer(mut self, issuer: impl Into<String>) -> Self {
        self.issuer = Some(issuer.into());
        self
    }

    /// The configured issuer.
    pub fn issuer_name(&self) -> Option<&str> {
        self.issuer.as_deref()
    }

    /// The step a Unix time falls in.
    pub fn step_at(&self, unix: u64) -> u64 {
        unix / self.period
    }

    /// The code for a Unix time.
    pub fn code_at(&self, secret: &Secret, unix: u64) -> String {
        hotp(self.algorithm, secret.as_bytes(), self.step_at(unix), self.digits)
    }

    /// The code right now.
    pub fn current_code(&self, secret: &Secret) -> String {
        self.code_at(secret, now())
    }

    /// Check `code` now — see [`verify_at`](Self::verify_at).
    pub fn verify(&self, secret: &Secret, code: &str, last_used_step: Option<u64>) -> Option<u64> {
        self.verify_at(secret, code, now(), last_used_step)
    }

    /// Check `code` at `unix`, refusing any step at or before
    /// `last_used_step`.
    ///
    /// Returns the step it matched — **persist it** as the next call's
    /// `last_used_step`, or the code can be replayed. `None` for a wrong
    /// code, a spent one, or one that is not `digits` digits.
    ///
    /// Every step in the window is computed and compared in constant time,
    /// whatever matches first, so the answer's timing says nothing about
    /// which step (if any) a guess was close to.
    pub fn verify_at(
        &self,
        secret: &Secret,
        code: &str,
        unix: u64,
        last_used_step: Option<u64>,
    ) -> Option<u64> {
        let code: String = code.chars().filter(|c| !c.is_whitespace()).collect();
        if code.len() != self.digits as usize || !code.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }

        let current = self.step_at(unix);
        let first = current.saturating_sub(self.window);
        let last = current.saturating_add(self.window);

        let mut matched = None;
        for step in first..=last {
            let expected = hotp(self.algorithm, secret.as_bytes(), step, self.digits);
            let equal: bool = expected.as_bytes().ct_eq(code.as_bytes()).into();
            let fresh = last_used_step.is_none_or(|used| step > used);
            if equal && fresh {
                matched = Some(step);
            }
        }
        matched
    }

    /// The `otpauth://` URI an authenticator app enrols from — render it as a
    /// QR code.
    ///
    /// The label is `Issuer:account` and the issuer is repeated as a
    /// parameter, which is the form both old and new apps read. Algorithm,
    /// digits and period are written only when they differ from the
    /// defaults, since some apps refuse a URI naming a parameter they do not
    /// support even when its value is the default.
    pub fn provisioning_uri(&self, secret: &Secret, account: &str) -> String {
        let label = match &self.issuer {
            Some(issuer) => format!("{}:{}", escape(issuer), escape(account)),
            None => escape(account),
        };

        let mut uri = format!("otpauth://totp/{label}?secret={}", secret.to_base32());
        if let Some(issuer) = &self.issuer {
            uri.push_str(&format!("&issuer={}", escape(issuer)));
        }
        if self.algorithm != Algorithm::Sha1 {
            uri.push_str(&format!("&algorithm={}", self.algorithm.name()));
        }
        if self.digits != 6 {
            uri.push_str(&format!("&digits={}", self.digits));
        }
        if self.period != 30 {
            uri.push_str(&format!("&period={}", self.period));
        }
        uri
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Percent-encoding for a URI component: everything but the unreserved set,
/// so a `:` or `&` in an issuer or an address cannot change the URI's shape.
fn escape(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA1_KEY: &[u8] = b"12345678901234567890";
    const SHA256_KEY: &[u8] = b"12345678901234567890123456789012";
    const SHA512_KEY: &[u8] = b"1234567890123456789012345678901234567890123456789012345678901234";

    #[test]
    fn rfc_4226_hotp_vectors() {
        let expected = [
            "755224", "287082", "359152", "969429", "338314", "254676", "287922", "162583",
            "399871", "520489",
        ];
        for (counter, code) in expected.iter().enumerate() {
            assert_eq!(hotp(Algorithm::Sha1, SHA1_KEY, counter as u64, 6), *code);
        }
    }

    #[test]
    fn rfc_6238_appendix_b_vectors() {
        let times =
            [59u64, 1_111_111_109, 1_111_111_111, 1_234_567_890, 2_000_000_000, 20_000_000_000];
        let table: [(Algorithm, &[u8], [&str; 6]); 3] = [
            (
                Algorithm::Sha1,
                SHA1_KEY,
                ["94287082", "07081804", "14050471", "89005924", "69279037", "65353130"],
            ),
            (
                Algorithm::Sha256,
                SHA256_KEY,
                ["46119246", "68084774", "67062674", "91819424", "90698825", "77737706"],
            ),
            (
                Algorithm::Sha512,
                SHA512_KEY,
                ["90693936", "25091201", "99943326", "93441116", "38618901", "47863826"],
            ),
        ];

        for (algorithm, key, codes) in table {
            let totp = Totp::new().algorithm(algorithm).digits(8);
            let secret = Secret::from_bytes(key);
            for (time, code) in times.iter().zip(codes) {
                assert_eq!(totp.code_at(&secret, *time), code, "{algorithm:?} at {time}");
            }
        }
    }

    #[test]
    fn the_window_tolerates_one_step_of_drift_and_no_more() {
        let totp = Totp::new();
        let secret = Secret::generate();
        let t = 1_700_000_000;

        for drift in [-30i64, 0, 30] {
            let code = totp.code_at(&secret, (t as i64 + drift) as u64);
            assert!(totp.verify_at(&secret, &code, t, None).is_some(), "drift {drift}");
        }
        for drift in [-60i64, 60] {
            let code = totp.code_at(&secret, (t as i64 + drift) as u64);
            assert!(totp.verify_at(&secret, &code, t, None).is_none(), "drift {drift}");
        }
    }

    #[test]
    fn a_spent_step_and_every_step_before_it_is_refused() {
        let totp = Totp::new();
        let secret = Secret::generate();
        let t = 1_700_000_000;

        let current = totp.code_at(&secret, t);
        let step = totp.verify_at(&secret, &current, t, None).expect("fresh");
        assert_eq!(step, totp.step_at(t));
        assert!(totp.verify_at(&secret, &current, t, Some(step)).is_none(), "replayed");

        let previous = totp.code_at(&secret, t - 30);
        assert!(totp.verify_at(&secret, &previous, t, Some(step)).is_none(), "older than spent");

        let next = totp.code_at(&secret, t + 30);
        assert_eq!(totp.verify_at(&secret, &next, t, Some(step)), Some(step + 1));
    }

    #[test]
    fn malformed_codes_are_refused_before_any_comparison() {
        let totp = Totp::new();
        let secret = Secret::generate();
        let t = 1_700_000_000;
        let code = totp.code_at(&secret, t);

        assert!(totp
            .verify_at(&secret, &format!("{} {}", &code[..3], &code[3..]), t, None)
            .is_some());
        assert!(totp.verify_at(&secret, &code[..5], t, None).is_none());
        assert!(totp.verify_at(&secret, "12345a", t, None).is_none());
        assert!(totp.verify_at(&secret, "", t, None).is_none());
    }

    #[test]
    fn a_google2fa_secret_reads_back_as_the_same_bytes() {
        // google2fa's generateSecretKey(32): 32 base32 characters, 160 bits.
        let secret = Secret::from_base32("JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP").unwrap();
        assert_eq!(secret.as_bytes().len(), 20);
        assert_eq!(secret.to_base32(), "JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP");
        assert_eq!(Secret::from_base32("jbsw y3dp ehpk 3pxp jbsw y3dp ehpk 3pxp").unwrap(), secret);
        assert_eq!(Secret::generate().to_base32().len(), 32);
    }

    #[test]
    fn the_provisioning_uri_escapes_its_label_and_omits_defaults() {
        let secret = Secret::from_base32("JBSWY3DPEHPK3PXP").unwrap();

        let plain = Totp::new().issuer("Acme & Co").provisioning_uri(&secret, "ada:x@example.com");
        assert_eq!(
            plain,
            "otpauth://totp/Acme%20%26%20Co:ada%3Ax%40example.com?secret=JBSWY3DPEHPK3PXP\
             &issuer=Acme%20%26%20Co"
        );

        let custom = Totp::new().algorithm(Algorithm::Sha256).digits(8).period(60);
        assert_eq!(
            custom.provisioning_uri(&secret, "ada"),
            "otpauth://totp/ada?secret=JBSWY3DPEHPK3PXP&algorithm=SHA256&digits=8&period=60"
        );
    }

    #[test]
    fn the_secret_stays_out_of_debug_output() {
        let secret = Secret::from_base32("JBSWY3DPEHPK3PXP").unwrap();
        assert!(!format!("{secret:?}").contains("JBSW"));
        assert!(!format!("{:?}", secret.as_bytes()).is_empty());
    }
}
