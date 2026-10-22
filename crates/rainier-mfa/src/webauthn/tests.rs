use serde_json::{json, Value as Json};

use super::testing::{Attestation, SoftwareAuthenticator, SoftwareKey};
use super::*;

const RP: &str = "example.com";
const ORIGIN: &str = "https://example.com";

fn rp() -> RelyingParty {
    RelyingParty::new(RP, "Example")
}

fn user() -> PasskeyUser {
    PasskeyUser::new(b"user-handle-1".to_vec(), "ada@example.com", "Ada")
}

fn register(rp: &RelyingParty, authenticator: &mut SoftwareAuthenticator) -> PasskeyCredential {
    let (options, state) = rp.start_registration(&user(), &[]);
    let created = authenticator.register(&options);
    rp.finish_registration(&state, &RegistrationResponse::from_json(&created).unwrap()).unwrap()
}

fn sign_in(
    rp: &RelyingParty,
    authenticator: &mut SoftwareAuthenticator,
    credential: &PasskeyCredential,
    allow: &[PasskeyCredential],
) -> Result<VerifiedAssertion> {
    let (options, state) = rp.start_authentication(allow);
    let signed = authenticator.authenticate(&options);
    rp.finish_authentication(&state, &AuthenticationResponse::from_json(&signed)?, credential)
}

/// Re-encode one field of a response.
fn bend(response: &mut Json, path: &[&str], value: Json) {
    let mut node = response;
    for key in &path[..path.len() - 1] {
        node = &mut node[*key];
    }
    node[path[path.len() - 1]] = value;
}

fn client_data(response: &Json) -> Json {
    let bytes = decode(response["response"]["clientDataJSON"].as_str().unwrap(), "x").unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn with_client_data(response: &mut Json, data: &Json) {
    let encoded = B64URL.encode(serde_json::to_vec(data).unwrap());
    bend(response, &["response", "clientDataJSON"], json!(encoded));
}

// --- the whole ceremony, every algorithm ---------------------------------------

#[test]
fn every_algorithm_registers_and_signs_in() {
    for key in [SoftwareKey::es256(), SoftwareKey::ed25519(), SoftwareKey::rs256()] {
        let algorithm = key.algorithm();
        let rp = rp();
        let mut authenticator = SoftwareAuthenticator::new(key, RP, ORIGIN);

        let mut credential = register(&rp, &mut authenticator);
        assert_eq!(credential.id, authenticator.credential_id());
        assert_eq!(credential.user_handle, b"user-handle-1");
        assert_eq!(credential.key().unwrap().algorithm(), algorithm);
        assert_eq!(credential.attestation, "none");
        assert!(credential.backup_eligible && credential.backed_up);
        assert_eq!(credential.transports, vec!["internal", "hybrid"]);

        let verified = sign_in(&rp, &mut authenticator, &credential, &[]).unwrap();
        assert!(verified.user_verified);
        verified.apply_to(&mut credential);
    }
}

#[test]
fn registration_options_are_the_json_the_browser_parses() {
    let existing = PasskeyCredential::from_parts(
        vec![9; 16],
        SoftwareKey::es256().cose(),
        0,
        b"user-handle-1".to_vec(),
    )
    .unwrap()
    .with_transports(vec!["usb".into()]);

    let (options, state) = rp().start_registration(&user(), std::slice::from_ref(&existing));

    assert_eq!(options["rp"], json!({ "id": RP, "name": "Example" }));
    assert_eq!(options["user"]["id"], B64URL.encode(b"user-handle-1"));
    assert_eq!(options["user"]["name"], "ada@example.com");
    assert_eq!(decode(options["challenge"].as_str().unwrap(), "c").unwrap().len(), 32);
    let algs: Vec<i64> = options["pubKeyCredParams"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["alg"].as_i64().unwrap())
        .collect();
    assert_eq!(algs, vec![-7, -8, -257]);
    assert_eq!(options["excludeCredentials"][0]["id"], existing.id_base64url());
    assert_eq!(options["excludeCredentials"][0]["transports"], json!(["usb"]));
    assert_eq!(options["authenticatorSelection"]["userVerification"], "required");
    assert_eq!(options["attestation"], "none");
    assert_eq!(state.user_handle(), b"user-handle-1");
}

#[test]
fn state_survives_a_round_trip_through_a_session() {
    let (_, state) = rp().start_registration(&user(), &[]);
    let stored = serde_json::to_string(&state).unwrap();
    assert_eq!(serde_json::from_str::<RegistrationState>(&stored).unwrap(), state);

    let (_, state) = rp().start_authentication(&[]);
    let stored = serde_json::to_string(&state).unwrap();
    assert_eq!(serde_json::from_str::<AuthenticationState>(&stored).unwrap(), state);
}

#[test]
fn a_stored_credential_round_trips_through_json() {
    let mut authenticator = SoftwareAuthenticator::es256(RP, ORIGIN);
    let credential = register(&rp(), &mut authenticator);
    let json = serde_json::to_value(&credential).unwrap();
    assert_eq!(json["id"], credential.id_base64url());
    assert_eq!(serde_json::from_value::<PasskeyCredential>(json).unwrap(), credential);
}

// --- registration refusals -------------------------------------------------------

#[test]
fn registration_refuses_a_foreign_origin_a_wrong_challenge_and_the_wrong_ceremony() {
    let rp = rp();

    let mut phishing = SoftwareAuthenticator::es256(RP, "https://examp1e.com");
    let (options, state) = rp.start_registration(&user(), &[]);
    let created = phishing.register(&options);
    assert!(rp
        .finish_registration(&state, &RegistrationResponse::from_json(&created).unwrap())
        .is_err());

    let mut authenticator = SoftwareAuthenticator::es256(RP, ORIGIN);
    let (options, state) = rp.start_registration(&user(), &[]);
    let (other_options, _) = rp.start_registration(&user(), &[]);
    let _ = options;
    let created = authenticator.register(&other_options);
    let err = rp.finish_registration(&state, &RegistrationResponse::from_json(&created).unwrap());
    assert!(err.unwrap_err().message().contains("challenge"));

    let (options, state) = rp.start_registration(&user(), &[]);
    let mut created = authenticator.register(&options);
    let mut data = client_data(&created);
    data["type"] = json!("webauthn.get");
    with_client_data(&mut created, &data);
    assert!(rp
        .finish_registration(&state, &RegistrationResponse::from_json(&created).unwrap())
        .is_err());

    let (options, state) = rp.start_registration(&user(), &[]);
    let mut created = authenticator.register(&options);
    let mut data = client_data(&created);
    data["crossOrigin"] = json!(true);
    with_client_data(&mut created, &data);
    let response = RegistrationResponse::from_json(&created).unwrap();
    assert!(rp.finish_registration(&state, &response).is_err());
    assert!(rp.clone().allow_cross_origin(true).finish_registration(&state, &response).is_ok());
}

#[test]
fn registration_refuses_a_passkey_for_another_site() {
    let rp = rp();
    let mut authenticator = SoftwareAuthenticator::es256("evil.example", ORIGIN);
    let (options, state) = rp.start_registration(&user(), &[]);
    let created = authenticator.register(&options);
    let err = rp.finish_registration(&state, &RegistrationResponse::from_json(&created).unwrap());
    assert!(err.unwrap_err().message().contains("different site"));
}

#[test]
fn user_verification_is_required_unless_relaxed() {
    let mut authenticator = SoftwareAuthenticator::es256(RP, ORIGIN).verifying_user(false);
    let (options, state) = rp().start_registration(&user(), &[]);
    let created = authenticator.register(&options);
    let response = RegistrationResponse::from_json(&created).unwrap();
    assert!(rp().finish_registration(&state, &response).is_err());

    let relaxed = rp().user_verification(UserVerification::Preferred);
    let (options, state) = relaxed.start_registration(&user(), &[]);
    let created = authenticator.register(&options);
    assert!(relaxed
        .finish_registration(&state, &RegistrationResponse::from_json(&created).unwrap())
        .is_ok());
}

#[test]
fn a_mismatched_id_is_refused() {
    let mut authenticator = SoftwareAuthenticator::es256(RP, ORIGIN);
    let (options, state) = rp().start_registration(&user(), &[]);
    let mut created = authenticator.register(&options);
    let other = B64URL.encode([7u8; 16]);
    bend(&mut created, &["id"], json!(other));
    bend(&mut created, &["rawId"], json!(other));
    let err = rp().finish_registration(&state, &RegistrationResponse::from_json(&created).unwrap());
    assert!(err.unwrap_err().message().contains("credential id"));
}

#[test]
fn an_expired_challenge_is_refused() {
    let mut authenticator = SoftwareAuthenticator::es256(RP, ORIGIN);
    let (options, mut state) = rp().start_registration(&user(), &[]);
    state.expires_at = now() - 1;
    let created = authenticator.register(&options);
    let err = rp().finish_registration(&state, &RegistrationResponse::from_json(&created).unwrap());
    assert!(err.unwrap_err().message().contains("expired"));
}

#[test]
fn attestation_formats_follow_the_policy() {
    let mut packed = SoftwareAuthenticator::es256(RP, ORIGIN).attesting(Attestation::PackedSelf);
    assert_eq!(register(&rp(), &mut packed).attestation, "packed");

    let mut u2f = SoftwareAuthenticator::es256(RP, ORIGIN).attesting(Attestation::Unverifiable);
    assert_eq!(register(&rp(), &mut u2f).attestation, "unverified:fido-u2f");

    let strict = rp().attestation(AttestationPolicy::Strict);
    let (options, state) = strict.start_registration(&user(), &[]);
    let created = u2f.register(&options);
    assert!(strict
        .finish_registration(&state, &RegistrationResponse::from_json(&created).unwrap())
        .is_err());
}

#[test]
fn a_bent_attestation_object_is_refused() {
    let mut authenticator = SoftwareAuthenticator::es256(RP, ORIGIN);
    let (options, state) = rp().start_registration(&user(), &[]);
    let mut created = authenticator.register(&options);
    let mut object =
        decode(created["response"]["attestationObject"].as_str().unwrap(), "x").unwrap();
    let last = object.len() - 1;
    object[last] ^= 0xff;
    bend(&mut created, &["response", "attestationObject"], json!(B64URL.encode(&object)));
    assert!(rp()
        .finish_registration(&state, &RegistrationResponse::from_json(&created).unwrap())
        .is_err());
}

// --- sign-in refusals ------------------------------------------------------------

#[test]
fn a_tampered_signature_or_signed_data_is_refused() {
    let rp = rp();
    let mut authenticator = SoftwareAuthenticator::es256(RP, ORIGIN);
    let credential = register(&rp, &mut authenticator);

    let (options, state) = rp.start_authentication(&[]);
    let mut signed = authenticator.authenticate(&options);
    let mut data = client_data(&signed);
    data["origin"] = json!("https://example.com");
    data["extra"] = json!("changed after signing");
    with_client_data(&mut signed, &data);
    let err = rp.finish_authentication(
        &state,
        &AuthenticationResponse::from_json(&signed).unwrap(),
        &credential,
    );
    assert!(err.is_err());
}

#[test]
fn a_signature_from_another_authenticator_is_refused() {
    let rp = rp();
    let mut mine = SoftwareAuthenticator::es256(RP, ORIGIN);
    let credential = register(&rp, &mut mine);

    let mut theirs = SoftwareAuthenticator::es256(RP, ORIGIN);
    let _ = register(&rp, &mut theirs);
    let (options, state) = rp.start_authentication(&[]);
    let mut signed = theirs.authenticate(&options);
    // Claim to be my credential.
    bend(&mut signed, &["id"], json!(credential.id_base64url()));
    bend(&mut signed, &["rawId"], json!(credential.id_base64url()));
    let response = AuthenticationResponse::from_json(&signed).unwrap();
    assert!(rp.finish_authentication(&state, &response, &credential).is_err());
}

#[test]
fn a_credential_the_challenge_did_not_allow_is_refused() {
    let rp = rp();
    let mut authenticator = SoftwareAuthenticator::es256(RP, ORIGIN);
    let credential = register(&rp, &mut authenticator);
    let other =
        PasskeyCredential::from_parts(vec![1; 16], SoftwareKey::es256().cose(), 0, b"u".to_vec())
            .unwrap();

    let err = sign_in(&rp, &mut authenticator, &credential, &[other]).unwrap_err();
    assert!(err.message().contains("not asked for"));
    assert!(
        sign_in(&rp, &mut authenticator, &credential, std::slice::from_ref(&credential)).is_ok()
    );
}

#[test]
fn a_user_handle_for_someone_else_is_refused() {
    let rp = rp();
    let mut authenticator = SoftwareAuthenticator::es256(RP, ORIGIN);
    let mut credential = register(&rp, &mut authenticator);
    credential.user_handle = b"another-user".to_vec();
    let err = sign_in(&rp, &mut authenticator, &credential, &[]).unwrap_err();
    assert!(err.message().contains("different user"));
}

#[test]
fn a_discoverable_sign_in_without_a_user_handle_is_refused() {
    let rp = rp();
    let mut authenticator = SoftwareAuthenticator::es256(RP, ORIGIN);
    let credential = register(&rp, &mut authenticator);

    let (options, state) = rp.start_authentication(&[]);
    let mut signed = authenticator.authenticate(&options);
    bend(&mut signed, &["response", "userHandle"], Json::Null);
    let response = AuthenticationResponse::from_json(&signed).unwrap();
    assert!(rp.finish_authentication(&state, &response, &credential).is_err());
}

#[test]
fn a_counter_that_does_not_advance_means_a_clone() {
    let rp = rp();
    let mut authenticator = SoftwareAuthenticator::es256(RP, ORIGIN).counting_from(10);
    let mut credential = register(&rp, &mut authenticator);
    assert_eq!(credential.sign_count, 11);

    let verified = sign_in(&rp, &mut authenticator, &credential, &[]).unwrap();
    assert_eq!(verified.sign_count, 12);
    verified.apply_to(&mut credential);

    authenticator.set_counter(5);
    let err = sign_in(&rp, &mut authenticator, &credential, &[]).unwrap_err();
    assert!(err.message().contains("cloned"));
}

#[test]
fn a_synced_passkey_reporting_zero_forever_is_fine() {
    let rp = rp();
    let mut authenticator = SoftwareAuthenticator::es256(RP, ORIGIN);
    let credential = register(&rp, &mut authenticator);
    assert_eq!(credential.sign_count, 0);
    for _ in 0..3 {
        assert_eq!(sign_in(&rp, &mut authenticator, &credential, &[]).unwrap().sign_count, 0);
    }
}

#[test]
fn backed_up_without_being_eligible_is_malformed() {
    let rp = rp();
    let mut authenticator = SoftwareAuthenticator::es256(RP, ORIGIN).backup_flags(false, true);
    let (options, state) = rp.start_registration(&user(), &[]);
    let created = authenticator.register(&options);
    assert!(rp
        .finish_registration(&state, &RegistrationResponse::from_json(&created).unwrap())
        .is_err());
}

#[test]
fn from_parts_refuses_what_could_never_verify() {
    assert!(PasskeyCredential::from_parts(vec![], SoftwareKey::es256().cose(), 0, vec![]).is_err());
    assert!(PasskeyCredential::from_parts(vec![1; 1024], SoftwareKey::es256().cose(), 0, vec![])
        .is_err());
    assert!(PasskeyCredential::from_parts(vec![1], vec![0xa0], 0, vec![]).is_err());
}

// --- published vectors (py_webauthn's test suite, real browsers) ----------------

/// A registration Chrome produced with a security key: `none` attestation,
/// counter 23, RP `localhost`.
#[test]
fn a_real_none_attestation_registers() {
    let created = json!({
        "id": "9y1xA8Tmg1FEmT-c7_fvWZ_uoTuoih3OvR45_oAK-cwHWhAbXrl2q62iLVTjiyEZ7O7n-CROOY494k7Q3xrs_w",
        "rawId": "9y1xA8Tmg1FEmT-c7_fvWZ_uoTuoih3OvR45_oAK-cwHWhAbXrl2q62iLVTjiyEZ7O7n-CROOY494k7Q3xrs_w",
        "response": {
            "attestationObject": "o2NmbXRkbm9uZWdhdHRTdG10oGhhdXRoRGF0YVjESZYN5YgOjGh0NBcPZHZgW4_krrmihjLHmVzzuoMdl2NFAAAAFwAAAAAAAAAAAAAAAAAAAAAAQPctcQPE5oNRRJk_nO_371mf7qE7qIodzr0eOf6ACvnMB1oQG165dqutoi1U44shGezu5_gkTjmOPeJO0N8a7P-lAQIDJiABIVggSFbUJF-42Ug3pdM8rDRFu_N5oiVEysPDB6n66r_7dZAiWCDUVnB39FlGypL-qAoIO9xWHtJygo2jfDmHl-_eKFRLDA",
            "clientDataJSON": "eyJ0eXBlIjoid2ViYXV0aG4uY3JlYXRlIiwiY2hhbGxlbmdlIjoiVHdON240V1R5R0tMYzRaWS1xR3NGcUtuSE00bmdscXN5VjBJQ0psTjJUTzlYaVJ5RnRya2FEd1V2c3FsLWdrTEpYUDZmbkYxTWxyWjUzTW00UjdDdnciLCJvcmlnaW4iOiJodHRwOi8vbG9jYWxob3N0OjUwMDAiLCJjcm9zc09yaWdpbiI6ZmFsc2V9"
        },
        "type": "public-key",
        "clientExtensionResults": {},
        "transports": ["nfc", "usb"]
    });
    let state: RegistrationState = serde_json::from_value(json!({
        "challenge": "TwN7n4WTyGKLc4ZY-qGsFqKnHM4nglqsyV0ICJlN2TO9XiRyFtrkaDwUvsql-gkLJXP6fnF1MlrZ53Mm4R7Cvw",
        "user_handle": "dXNlcg",
        "user_verification": "required",
        "expires_at": u64::MAX,
    }))
    .unwrap();
    let rp = RelyingParty::new("localhost", "py_webauthn").origins(["http://localhost:5000"]);

    let credential = rp
        .finish_registration(&state, &RegistrationResponse::from_json(&created).unwrap())
        .unwrap();
    assert_eq!(credential.sign_count, 23);
    assert_eq!(credential.aaguid, [0; 16]);
    assert!(!credential.backed_up);
    assert_eq!(
        credential.public_key,
        decode("pQECAyYgASFYIEhW1CRfuNlIN6XTPKw0RbvzeaIlRMrDwwep-uq_-3WQIlgg1FZwd_RZRsqS_qgKCDvcVh7ScoKNo3w5h5fv3ihUSww", "k").unwrap()
    );
}

/// An ES256 sign-in with user presence only, counter 77 → 78.
#[test]
fn a_real_es256_assertion_verifies() {
    let signed = json!({
        "id": "EDx9FfAbp4obx6oll2oC4-CZuDidRVV4gZhxC529ytlnqHyqCStDUwfNdm1SNHAe3X5KvueWQdAX3x9R1a2b9Q",
        "rawId": "EDx9FfAbp4obx6oll2oC4-CZuDidRVV4gZhxC529ytlnqHyqCStDUwfNdm1SNHAe3X5KvueWQdAX3x9R1a2b9Q",
        "response": {
            "authenticatorData": "SZYN5YgOjGh0NBcPZHZgW4_krrmihjLHmVzzuoMdl2MBAAAATg",
            "clientDataJSON": "eyJjaGFsbGVuZ2UiOiJ4aTMwR1BHQUZZUnhWRHBZMXNNMTBEYUx6VlFHNjZudi1fN1JVYXpIMHZJMll2RzhMWWdERW52TjVmWlpOVnV2RUR1TWk5dGUzVkxxYjQyTjBma0xHQSIsImNsaWVudEV4dGVuc2lvbnMiOnt9LCJoYXNoQWxnb3JpdGhtIjoiU0hBLTI1NiIsIm9yaWdpbiI6Imh0dHA6Ly9sb2NhbGhvc3Q6NTAwMCIsInR5cGUiOiJ3ZWJhdXRobi5nZXQifQ",
            "signature": "MEUCIGisVZOBapCWbnJJvjelIzwpixxIwkjCCb5aCHafQu68AiEA88v-2pJNNApPFwAKFiNuf82-2hBxYW5kGwVweeoxCwo"
        },
        "type": "public-key",
        "clientExtensionResults": {}
    });
    let id =
        "EDx9FfAbp4obx6oll2oC4-CZuDidRVV4gZhxC529ytlnqHyqCStDUwfNdm1SNHAe3X5KvueWQdAX3x9R1a2b9Q";
    let credential = PasskeyCredential::from_parts(
        decode(id, "id").unwrap(),
        decode("pQECAyYgASFYIIeDTe-gN8A-zQclHoRnGFWN8ehM1b7yAsa8I8KIvmplIlgg4nFGT5px8o6gpPZZhO01wdy9crDSA_Ngtkx0vGpvPHI", "k").unwrap(),
        77,
        b"irrelevant".to_vec(),
    )
    .unwrap();
    let state: AuthenticationState = serde_json::from_value(json!({
        "challenge": "xi30GPGAFYRxVDpY1sM10DaLzVQG66nv-_7RUazH0vI2YvG8LYgDEnvN5fZZNVuvEDuMi9te3VLqb42N0fkLGA",
        "allowed": [id],
        "user_verification": "discouraged",
        "expires_at": u64::MAX,
    }))
    .unwrap();
    let rp = RelyingParty::new("localhost", "py_webauthn")
        .origins(["http://localhost:5000"])
        .user_verification(UserVerification::Discouraged);

    let response = AuthenticationResponse::from_json(&signed).unwrap();
    let verified = rp.finish_authentication(&state, &response, &credential).unwrap();
    assert_eq!(verified.sign_count, 78);
    assert!(!verified.user_verified);

    // Required verification refuses the same assertion.
    let strict = rp.clone().user_verification(UserVerification::Required);
    let mut state_required = state.clone();
    state_required.user_verification = UserVerification::Required;
    assert!(strict.finish_authentication(&state_required, &response, &credential).is_err());

    // And a replayed counter is a clone.
    let mut seen = credential.clone();
    seen.sign_count = 78;
    assert!(rp.finish_authentication(&state, &response, &seen).is_err());
}

/// An RS256 sign-in (Windows Hello), with a user handle.
#[test]
fn a_real_rs256_assertion_verifies() {
    let signed = json!({
        "id": "ZoIKP1JQvKdrYj1bTUPJ2eTUsbLeFkv-X5xJQNr4k6s",
        "rawId": "ZoIKP1JQvKdrYj1bTUPJ2eTUsbLeFkv-X5xJQNr4k6s",
        "response": {
            "authenticatorData": "SZYN5YgOjGh0NBcPZHZgW4_krrmihjLHmVzzuoMdl2MFAAAAAQ",
            "clientDataJSON": "eyJ0eXBlIjoid2ViYXV0aG4uZ2V0IiwiY2hhbGxlbmdlIjoiaVBtQWkxUHAxWEw2b0FncTNQV1p0WlBuWmExekZVRG9HYmFRMF9LdlZHMWxGMnMzUnRfM280dVN6Y2N5MHRtY1RJcFRUVDRCVTFULUk0bWFhdm5kalEiLCJvcmlnaW4iOiJodHRwOi8vbG9jYWxob3N0OjUwMDAiLCJjcm9zc09yaWdpbiI6ZmFsc2V9",
            "signature": "iOHKX3erU5_OYP_r_9HLZ-CexCE4bQRrxM8WmuoKTDdhAnZSeTP0sjECjvjfeS8MJzN1ArmvV0H0C3yy_FdRFfcpUPZzdZ7bBcmPh1XPdxRwY747OrIzcTLTFQUPdn1U-izCZtP_78VGw9pCpdMsv4CUzZdJbEcRtQuRS03qUjqDaovoJhOqEBmxJn9Wu8tBi_Qx7A33RbYjlfyLm_EDqimzDZhyietyop6XUcpKarKqVH0M6mMrM5zTjp8xf3W7odFCadXEJg-ERZqFM0-9Uup6kJNLbr6C5J4NDYmSm3HCSA6lp2iEiMPKU8Ii7QZ61kybXLxsX4w4Dm3fOLjmDw",
            "userHandle": "T1RWa1l6VXdPRFV0WW1NNVlTMDBOVEkxTFRnd056Z3RabVZpWVdZNFpEVm1ZMk5p"
        },
        "type": "public-key",
        "clientExtensionResults": {}
    });
    let credential = PasskeyCredential::from_parts(
        decode("ZoIKP1JQvKdrYj1bTUPJ2eTUsbLeFkv-X5xJQNr4k6s", "id").unwrap(),
        decode("pAEDAzkBACBZAQDfV20epzvQP-HtcdDpX-cGzdOxy73WQEvsU7Dnr9UWJophEfpngouvgnRLXaEUn_d8HGkp_HIx8rrpkx4BVs6X_B6ZjhLlezjIdJbLbVeb92BaEsmNn1HW2N9Xj2QM8cH-yx28_vCjf82ahQ9gyAr552Bn96G22n8jqFRQKdVpO-f-bvpvaP3IQ9F5LCX7CUaxptgbog1SFO6FI6ob5SlVVB00lVXsaYg8cIDZxCkkENkGiFPgwEaZ7995SCbiyCpUJbMqToLMgojPkAhWeyktu7TlK6UBWdJMHc3FPAIs0lH_2_2hKS-mGI1uZAFVAfW1X-mzKL0czUm2P1UlUox7IUMBAAE", "k").unwrap(),
        0,
        decode("T1RWa1l6VXdPRFV0WW1NNVlTMDBOVEkxTFRnd056Z3RabVZpWVdZNFpEVm1ZMk5p", "h").unwrap(),
    )
    .unwrap();
    let state: AuthenticationState = serde_json::from_value(json!({
        "challenge": "iPmAi1Pp1XL6oAgq3PWZtZPnZa1zFUDoGbaQ0_KvVG1lF2s3Rt_3o4uSzccy0tmcTIpTTT4BU1T-I4maavndjQ",
        "user_verification": "required",
        "expires_at": u64::MAX,
    }))
    .unwrap();
    let rp = RelyingParty::new("localhost", "py_webauthn").origins(["http://localhost:5000"]);

    let response = AuthenticationResponse::from_json(&signed).unwrap();
    let verified = rp.finish_authentication(&state, &response, &credential).unwrap();
    assert_eq!(verified.sign_count, 1);
    assert!(verified.user_verified);
    assert_eq!(credential.key().unwrap().algorithm(), CoseAlgorithm::Rs256);
}
