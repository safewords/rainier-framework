//! Passkey credentials stored by web-auth/webauthn-lib — read into a
//! [`PasskeyCredential`], and written back.
//!
//! ```ignore
//! use rainier_compat::php::webauthn;
//!
//! // The JSON column the PHP application keeps per passkey.
//! let credential = webauthn::read_credential(&row.data)?;
//! let verified = relying_party.finish_authentication(&state, &response, &credential)?;
//!
//! // While the PHP side still signs people in too, give it the new counter
//! // in its own format — everything else in the row is left as it was.
//! let mut updated = credential.clone();
//! verified.apply_to(&mut updated);
//! row.data = webauthn::update_credential(&row.data, &updated)?;
//! ```
//!
//! # What the library stores
//!
//! A `PublicKeyCredentialSource`, normalised to JSON:
//!
//! | Field | Here |
//! |---|---|
//! | `publicKeyCredentialId` | [`id`](PasskeyCredential::id) |
//! | `credentialPublicKey` | [`public_key`](PasskeyCredential::public_key) — the COSE key, unchanged |
//! | `userHandle` | [`user_handle`](PasskeyCredential::user_handle) |
//! | `counter` | [`sign_count`](PasskeyCredential::sign_count) |
//! | `transports` | [`transports`](PasskeyCredential::transports) |
//! | `aaguid` | [`aaguid`](PasskeyCredential::aaguid), from its UUID form |
//! | `backupEligible`, `backupStatus` | the backup flags, `false` when absent |
//! | `attestationType` | [`attestation`](PasskeyCredential::attestation) |
//!
//! The byte fields are base64url without padding when the library writes
//! them, and its reader accepts standard base64 with padding as well — so
//! this does too, since rows written by older versions or by hand exist.
//!
//! The COSE key bytes are exactly what the authenticator produced, so a
//! passkey registered on the PHP side verifies here with no re-registration:
//! the user notices nothing.
//!
//! # Look up by the id inside the JSON
//!
//! Some PHP packages keep a `credential_id` column beside the JSON for
//! lookups, written by re-encoding the raw id bytes as if they were text.
//! That is lossy for ids that are not valid UTF-8 — most of them — so two
//! credentials can collide and none can be recovered from it. Index
//! [`PasskeyCredential::id_base64url`] instead, computed from the JSON.

use rainier_mfa::webauthn::{decode_base64url, PasskeyCredential};
use rainier_support::{Error, Result};
use serde_json::{Map, Value};

/// Read a stored credential.
///
/// # Errors
///
/// Not JSON, a required field missing or not base64, or a public key that is
/// not a valid ES256, EdDSA or RS256 COSE key.
pub fn read_credential(json: &str) -> Result<PasskeyCredential> {
    let value: Value = serde_json::from_str(json)
        .map_err(|e| invalid(&format!("the stored credential is not JSON: {e}")))?;
    read_credential_value(&value)
}

/// Read a stored credential that is already parsed.
///
/// # Errors
///
/// As [`read_credential`].
pub fn read_credential_value(value: &Value) -> Result<PasskeyCredential> {
    let object =
        value.as_object().ok_or_else(|| invalid("the stored credential is not an object"))?;

    let bytes = |field: &str| -> Result<Vec<u8>> {
        let text = object
            .get(field)
            .and_then(Value::as_str)
            .ok_or_else(|| invalid(&format!("`{field}` is missing")))?;
        decode_base64url(text).map_err(|_| invalid(&format!("`{field}` is not base64")))
    };

    let counter = match object.get("counter") {
        None | Some(Value::Null) => 0,
        Some(value) => value
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| invalid("`counter` is not a 32-bit count"))?,
    };

    let mut credential = PasskeyCredential::from_parts(
        bytes("publicKeyCredentialId")?,
        bytes("credentialPublicKey")?,
        counter,
        bytes("userHandle")?,
    )?;

    if let Some(transports) = object.get("transports").and_then(Value::as_array) {
        credential = credential.with_transports(
            transports.iter().filter_map(Value::as_str).map(str::to_string).collect(),
        );
    }

    let flag = |field: &str| object.get(field).and_then(Value::as_bool).unwrap_or(false);
    credential = credential.with_backup(flag("backupEligible"), flag("backupStatus"));

    if let Some(aaguid) = object.get("aaguid").and_then(Value::as_str) {
        credential = credential.with_aaguid(parse_uuid(aaguid)?);
    }

    let attestation =
        object.get("attestationType").and_then(Value::as_str).unwrap_or("none").to_string();
    Ok(credential.with_attestation(attestation))
}

/// Write what a Rainier sign-in changed — the counter and the backup flags —
/// into the stored JSON, leaving every other field as the PHP side wrote it.
///
/// Patching rather than regenerating is deliberate: the row carries fields
/// this crate has no reason to understand (`trustPath`, `otherUI`,
/// `uvInitialized`), and a PHP reader must keep finding them.
///
/// # Errors
///
/// If `original` is not a JSON object, or names a different credential.
pub fn update_credential(original: &str, credential: &PasskeyCredential) -> Result<String> {
    let value: Value = serde_json::from_str(original)
        .map_err(|e| invalid(&format!("the stored credential is not JSON: {e}")))?;
    let mut object: Map<String, Value> = match value {
        Value::Object(object) => object,
        _ => return Err(invalid("the stored credential is not an object")),
    };

    let stored_id = object
        .get("publicKeyCredentialId")
        .and_then(Value::as_str)
        .map(decode_base64url)
        .transpose()?;
    if stored_id.as_deref() != Some(credential.id.as_slice()) {
        return Err(invalid("that row holds a different credential"));
    }

    object.insert("counter".into(), Value::from(credential.sign_count));
    object.insert("backupEligible".into(), Value::Bool(credential.backup_eligible));
    object.insert("backupStatus".into(), Value::Bool(credential.backed_up));

    Ok(Value::Object(object).to_string())
}

/// `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx` to sixteen bytes.
fn parse_uuid(text: &str) -> Result<[u8; 16]> {
    let hex: String = text.chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 {
        return Err(invalid("`aaguid` is not a UUID"));
    }
    let mut bytes = [0u8; 16];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16)
            .map_err(|_| invalid("`aaguid` is not a UUID"))?;
    }
    Ok(bytes)
}

fn invalid(why: &str) -> Error {
    Error::bad_request(format!("unreadable webauthn-lib credential: {why}"))
}

#[cfg(test)]
mod tests {
    use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
    use base64::Engine as _;
    use rainier_mfa::webauthn::testing::{SoftwareAuthenticator, SoftwareKey};
    use rainier_mfa::webauthn::{
        AuthenticationResponse, PasskeyUser, RegistrationResponse, RelyingParty,
    };
    use serde_json::json;

    use super::*;

    /// A row in the shape the library normalises a credential source to.
    fn row(id: &[u8], key: &[u8], handle: &[u8], counter: u32) -> Value {
        json!({
            "publicKeyCredentialId": URL_SAFE_NO_PAD.encode(id),
            "type": "public-key",
            "transports": ["internal", "hybrid"],
            "attestationType": "none",
            "trustPath": { "type": "Webauthn\\TrustPath\\EmptyTrustPath" },
            "aaguid": "fbfc3007-154e-4ecc-8c0b-6e020557d7bd",
            "credentialPublicKey": URL_SAFE_NO_PAD.encode(key),
            "userHandle": URL_SAFE_NO_PAD.encode(handle),
            "counter": counter,
            "backupEligible": true,
            "backupStatus": true,
            "uvInitialized": true,
        })
    }

    #[test]
    fn every_field_is_read() {
        let key = SoftwareKey::es256().cose();
        let credential = read_credential(&row(b"cred-id", &key, b"42", 7).to_string()).unwrap();

        assert_eq!(credential.id, b"cred-id");
        assert_eq!(credential.public_key, key);
        assert_eq!(credential.user_handle, b"42");
        assert_eq!(credential.sign_count, 7);
        assert_eq!(credential.transports, vec!["internal", "hybrid"]);
        assert!(credential.backup_eligible && credential.backed_up);
        assert_eq!(credential.aaguid[..4], [0xfb, 0xfc, 0x30, 0x07]);
        assert_eq!(credential.attestation, "none");
    }

    #[test]
    fn standard_base64_with_padding_is_read_too() {
        let key = SoftwareKey::es256().cose();
        let id = [0xfbu8, 0xff, 0xfe, 0x01, 0x02];
        let mut value = row(&id, &key, b"42", 0);
        value["publicKeyCredentialId"] = json!(STANDARD.encode(id));
        value["credentialPublicKey"] = json!(STANDARD.encode(&key));

        let credential = read_credential_value(&value).unwrap();
        assert_eq!(credential.id, id);
        assert_eq!(credential.public_key, key);
    }

    #[test]
    fn optional_fields_default_and_required_ones_do_not() {
        let key = SoftwareKey::es256().cose();
        let minimal = json!({
            "publicKeyCredentialId": URL_SAFE_NO_PAD.encode(b"id"),
            "credentialPublicKey": URL_SAFE_NO_PAD.encode(&key),
            "userHandle": URL_SAFE_NO_PAD.encode(b"u"),
        });
        let credential = read_credential_value(&minimal).unwrap();
        assert_eq!(credential.sign_count, 0);
        assert!(!credential.backup_eligible);
        assert_eq!(credential.aaguid, [0; 16]);

        for missing in ["publicKeyCredentialId", "credentialPublicKey", "userHandle"] {
            let mut broken = minimal.clone();
            broken.as_object_mut().unwrap().remove(missing);
            assert!(read_credential_value(&broken).is_err(), "{missing}");
        }

        let mut bad_key = minimal.clone();
        bad_key["credentialPublicKey"] = json!(URL_SAFE_NO_PAD.encode([0xa0]));
        assert!(read_credential_value(&bad_key).is_err());

        let mut bad_uuid = minimal;
        bad_uuid["aaguid"] = json!("not-a-uuid");
        assert!(read_credential_value(&bad_uuid).is_err());
        assert!(read_credential("[]").is_err());
    }

    /// The point of the module: a passkey the PHP side registered signs in
    /// here, and the PHP side can read the counter this side wrote.
    #[test]
    fn a_passkey_registered_elsewhere_signs_in_here_and_the_row_stays_readable_there() {
        let rp = RelyingParty::new("example.com", "Example");
        let mut authenticator =
            SoftwareAuthenticator::es256("example.com", "https://example.com").counting_from(4);

        // Registered "elsewhere": the relying party is only used to obtain
        // the bytes an authenticator would have handed the PHP library.
        let (options, state) =
            rp.start_registration(&PasskeyUser::new(b"42".to_vec(), "ada", "Ada"), &[]);
        let created = rp
            .finish_registration(
                &state,
                &RegistrationResponse::from_json(&authenticator.register(&options)).unwrap(),
            )
            .unwrap();
        let stored = row(&created.id, &created.public_key, b"42", created.sign_count).to_string();

        let credential = read_credential(&stored).unwrap();
        let (options, state) = rp.start_authentication(&[]);
        let signed = authenticator.authenticate(&options);
        let verified = rp
            .finish_authentication(
                &state,
                &AuthenticationResponse::from_json(&signed).unwrap(),
                &credential,
            )
            .unwrap();
        assert_eq!(verified.sign_count, 6);

        let mut updated = credential.clone();
        verified.apply_to(&mut updated);
        let written: Value =
            serde_json::from_str(&update_credential(&stored, &updated).unwrap()).unwrap();
        assert_eq!(written["counter"], 6);
        assert_eq!(
            written["trustPath"]["type"], "Webauthn\\TrustPath\\EmptyTrustPath",
            "untouched"
        );
        assert_eq!(written["uvInitialized"], true, "untouched");
        assert_eq!(read_credential_value(&written).unwrap().sign_count, 6);
    }

    #[test]
    fn updating_refuses_a_row_for_another_credential() {
        let key = SoftwareKey::es256().cose();
        let stored = row(b"one", &key, b"u", 0).to_string();
        let other = read_credential(&row(b"two", &key, b"u", 0).to_string()).unwrap();
        assert!(update_credential(&stored, &other).is_err());
    }
}
