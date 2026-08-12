//! Signed browser uploads: the S3 POST-policy form.
//!
//! A client never gets credentials. It gets a **policy document** saying
//! exactly what it may upload, and a signature proving the application
//! authorised that document. The bucket enforces it — so the limits here are
//! not advisory, and a client that exceeds them is refused by object storage
//! rather than by the application.
//!
//! # Why POST rather than a presigned PUT
//!
//! A presigned PUT signs a URL and nothing else: whoever holds it can upload
//! any bytes, of any size, with any content type, until it expires. A POST
//! policy signs *conditions* — this key, under this size, of this type — which
//! is what lets an untrusted browser upload directly to a bucket without
//! becoming a way to fill it.
//!
//! # Hand-rolled, deliberately
//!
//! The AWS Rust SDK has no first-class POST-policy support — it signs requests
//! this process makes, and this is a form somebody else will submit. So it is
//! SigV4 by hand: a scope, a chained key derivation, and one HMAC over the
//! base64 policy.
//!
//! Short, but every step is load-bearing, and a mistake surfaces as an opaque
//! `403 SignatureDoesNotMatch` in the browser naming no field. That is why the
//! derivation is pinned against AWS's published vector below rather than only
//! checked for running — an almost-right chain still produces 32 plausible
//! bytes.
//!
//! # Cloudflare R2 does not implement this at all
//!
//! Verified against a live bucket on 2026-08-12:
//!
//! ```text
//! 501 NotImplemented
//! Presigned post requests are not yet implemented
//! ```
//!
//! The signature is correct; R2 simply does not serve POST policies. So on R2
//! use [`Filesystem::temporary_upload_url`](crate::Filesystem::temporary_upload_url),
//! which is a presigned PUT and which R2 does implement — accepting that it
//! signs a URL rather than conditions, and compensating with a short expiry, a
//! key the client did not choose, and a size checked after the fact.
//!
//! This module stays for real S3 and for anything else that implements the
//! POST form. It is left here rather than deleted because the next person will
//! reach for POST policies for exactly the right reasons, and should find this
//! note instead of spending a day on a 501.
//!
//! # R2 is stricter than S3 in one way that matters
//!
//! R2 requires the region in the credential scope to be `auto`. Signing with
//! `us-east-1` — which S3 accepts as an alias for the default region — fails
//! against R2, and the error says nothing about regions.

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// How long a form is good for.
///
/// An hour: long enough for a large file on a poor connection, short enough
/// that a leaked form is not a standing grant.
pub const TTL_SECONDS: i64 = 3600;

/// What a client is allowed to upload.
#[derive(Debug, Clone)]
pub struct UploadConditions {
    /// The exact object key. Not a prefix: a prefix condition would let a
    /// client choose its own key within it and overwrite another upload.
    pub key: String,
    /// The bucket the form posts to.
    pub bucket: String,
    /// The largest acceptable body, in bytes.
    ///
    /// Signed as a range condition, so the bucket rejects an oversized upload
    /// rather than accepting it and leaving us to notice later.
    pub max_bytes: u64,
    /// The content type the client declared.
    pub content_type: String,
}

/// A signed form: the fields a client must POST, and where.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SignedUploadForm {
    /// The endpoint to POST to.
    pub url: String,
    /// Form fields, in the order they must be sent — the file part goes last.
    pub fields: Vec<(String, String)>,
}

/// Credentials and endpoint for signing.
#[derive(Debug, Clone)]
pub struct SigningContext {
    /// The access key id.
    pub access_key_id: String,
    /// The secret. Never logged, never returned.
    pub secret_access_key: String,
    /// `auto` for R2. See the module docs — S3 tolerates other values here and
    /// R2 does not.
    pub region: String,
    /// The S3 endpoint, e.g. `https://<account>.r2.cloudflarestorage.com`.
    pub endpoint: String,
}

/// Build the signed POST form.
///
/// `now` is taken rather than read so the signature is reproducible in a test;
/// production passes `Utc::now()`.
pub fn sign(
    ctx: &SigningContext,
    conditions: &UploadConditions,
    now: chrono::DateTime<chrono::Utc>,
) -> SignedUploadForm {
    let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
    let date_stamp = now.format("%Y%m%d").to_string();
    let expiration =
        (now + chrono::Duration::seconds(TTL_SECONDS)).format("%Y-%m-%dT%H:%M:%SZ").to_string();

    let credential = format!("{}/{}/{}/s3/aws4_request", ctx.access_key_id, date_stamp, ctx.region);

    // Every condition here is enforced by the bucket. `content-length-range`
    // is the one that matters most: without it a signed form is an unbounded
    // write, and the first thing anyone does with one is find out how
    // unbounded.
    let policy = serde_json::json!({
        "expiration": expiration,
        "conditions": [
            { "bucket": conditions.bucket },
            // Exact key, not `starts-with`. A prefix lets the client pick a
            // name inside it and overwrite somebody else's object.
            { "key": conditions.key },
            { "Content-Type": conditions.content_type },
            { "x-amz-algorithm": "AWS4-HMAC-SHA256" },
            { "x-amz-credential": credential },
            { "x-amz-date": amz_date },
            ["content-length-range", 1, conditions.max_bytes],
        ]
    });

    let encoded = base64_encode(policy.to_string().as_bytes());
    let signature = hex(&hmac(&signing_key(ctx, &date_stamp), encoded.as_bytes()));

    SignedUploadForm {
        // Path-style. R2 does not serve these buckets at a virtual host, and a
        // virtual-host URL fails DNS rather than returning anything that names
        // the cause.
        url: format!("{}/{}", ctx.endpoint.trim_end_matches('/'), conditions.bucket),
        fields: vec![
            ("key".into(), conditions.key.clone()),
            ("Content-Type".into(), conditions.content_type.clone()),
            ("x-amz-algorithm".into(), "AWS4-HMAC-SHA256".into()),
            ("x-amz-credential".into(), credential),
            ("x-amz-date".into(), amz_date),
            ("policy".into(), encoded),
            ("x-amz-signature".into(), signature),
        ],
    }
}

/// The SigV4 signing key: four chained HMACs down to the service scope.
///
/// Chained rather than concatenated, and each step keys the next — getting the
/// order wrong still produces 32 plausible bytes, which is why this is pinned
/// to AWS's published vector in the tests.
fn signing_key(ctx: &SigningContext, date_stamp: &str) -> Vec<u8> {
    let k_date = hmac(format!("AWS4{}", ctx.secret_access_key).as_bytes(), date_stamp.as_bytes());
    let k_region = hmac(&k_date, ctx.region.as_bytes());
    let k_service = hmac(&k_region, b"s3");
    hmac(&k_service, b"aws4_request")
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Standard base64, which is what the policy field takes.
fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);

        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { TABLE[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[n as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> SigningContext {
        SigningContext {
            access_key_id: "AKIAIOSFODNN7EXAMPLE".into(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
            region: "auto".into(),
            endpoint: "https://account.r2.cloudflarestorage.com".into(),
        }
    }

    fn conditions() -> UploadConditions {
        UploadConditions {
            key: "abc123.png".into(),
            bucket: "ingestion".into(),
            max_bytes: 10 * 1024 * 1024,
            content_type: "image/png".into(),
        }
    }

    fn at(s: &str) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&chrono::Utc)
    }

    #[test]
    fn the_signing_key_matches_aws_published_vector() {
        // From AWS's SigV4 documentation. The four HMACs chain, and getting
        // the order wrong still yields 32 plausible bytes — so without a known
        // answer the only signal is a 403 with no detail.
        let ctx = SigningContext {
            secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
            region: "us-east-1".into(),
            ..ctx()
        };

        assert_eq!(
            hex(&signing_key(&ctx, "20130524")),
            "dbb893acc010964918f1fd433add87c70e8b0db6be30c1fbeafefa5ec6ba8378"
        );
    }

    #[test]
    fn base64_matches_known_values_including_padding() {
        // Hand-rolled, so the padding cases are worth pinning: a policy that
        // is not a multiple of three bytes is the common case, not the edge.
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn the_form_carries_every_field_the_bucket_requires() {
        let form = sign(&ctx(), &conditions(), at("2026-08-12T09:00:00Z"));
        let names: Vec<&str> = form.fields.iter().map(|(k, _)| k.as_str()).collect();

        for required in [
            "key",
            "Content-Type",
            "x-amz-algorithm",
            "x-amz-credential",
            "x-amz-date",
            "policy",
            "x-amz-signature",
        ] {
            assert!(names.contains(&required), "{required} is missing from {names:?}");
        }
    }

    #[test]
    fn the_policy_bounds_the_body_size() {
        // Without content-length-range a signed form is an unbounded write,
        // and the first thing anyone does with one is find out how unbounded.
        let form = sign(&ctx(), &conditions(), at("2026-08-12T09:00:00Z"));
        let policy = form.fields.iter().find(|(k, _)| k == "policy").unwrap().1.clone();
        let decoded = decode_for_test(&policy);

        assert!(decoded.contains("content-length-range"), "{decoded}");
        assert!(decoded.contains("10485760"), "{decoded}");
    }

    #[test]
    fn the_key_is_exact_rather_than_a_prefix() {
        // A `starts-with` condition would let the client choose its own name
        // inside the prefix and overwrite another upload's object.
        let form = sign(&ctx(), &conditions(), at("2026-08-12T09:00:00Z"));
        let policy = form.fields.iter().find(|(k, _)| k == "policy").unwrap().1.clone();
        let decoded = decode_for_test(&policy);

        assert!(!decoded.contains("starts-with"), "{decoded}");
        assert!(decoded.contains("abc123.png"), "{decoded}");
    }

    #[test]
    fn the_credential_scope_uses_the_configured_region() {
        // R2 requires `auto`. Signing with us-east-1 — which S3 accepts as an
        // alias for its default — fails against R2, and the error says nothing
        // about regions.
        let form = sign(&ctx(), &conditions(), at("2026-08-12T09:00:00Z"));
        let credential = &form.fields.iter().find(|(k, _)| k == "x-amz-credential").unwrap().1;

        assert!(credential.contains("/auto/s3/aws4_request"), "{credential}");
        assert!(credential.starts_with("AKIAIOSFODNN7EXAMPLE/20260812/"), "{credential}");
    }

    #[test]
    fn the_url_is_path_style() {
        // R2 does not serve these buckets at a virtual host, and a
        // virtual-host URL fails DNS rather than returning anything that names
        // the cause.
        let form = sign(&ctx(), &conditions(), at("2026-08-12T09:00:00Z"));

        assert_eq!(form.url, "https://account.r2.cloudflarestorage.com/ingestion");
    }

    #[test]
    fn the_signature_changes_with_the_key_being_signed_for() {
        // The signature covers the policy, and the policy names the key — so
        // a form cannot be replayed for a different object.
        let a = sign(&ctx(), &conditions(), at("2026-08-12T09:00:00Z"));
        let b = sign(
            &ctx(),
            &UploadConditions { key: "other.png".into(), ..conditions() },
            at("2026-08-12T09:00:00Z"),
        );

        let sig = |f: &SignedUploadForm| {
            f.fields.iter().find(|(k, _)| k == "x-amz-signature").unwrap().1.clone()
        };
        assert_ne!(sig(&a), sig(&b));
    }

    #[test]
    fn the_secret_never_appears_in_the_form() {
        // The whole point: the client gets a signature, never a credential.
        let form = sign(&ctx(), &conditions(), at("2026-08-12T09:00:00Z"));
        let rendered = serde_json::to_string(&form).unwrap();

        assert!(!rendered.contains("wJalrXUtnFEMI"), "the secret leaked into the form");
    }

    /// Minimal base64 decode, for asserting on the policy this module encodes.
    fn decode_for_test(input: &str) -> String {
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut bits = Vec::new();
        for c in input.bytes().filter(|c| *c != b'=') {
            let v = TABLE.iter().position(|t| *t == c).expect("valid base64") as u32;
            bits.push(v);
        }
        let mut out = Vec::new();
        for chunk in bits.chunks(4) {
            let mut n = 0u32;
            for (i, v) in chunk.iter().enumerate() {
                n |= v << (18 - 6 * i);
            }
            let bytes = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
            out.extend_from_slice(&bytes[..chunk.len() - 1]);
        }
        String::from_utf8(out).expect("the policy is utf-8")
    }
}
