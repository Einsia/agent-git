use super::*;

fn fixture() -> (KeyRecord, Zeroizing<String>, Zeroizing<[u8; 32]>) {
    let value: serde_json::Value =
        serde_json::from_str(include_str!("../../../tests/fixtures/privacy-web-key.json")).unwrap();
    (
        serde_json::from_value(value["record"].clone()).unwrap(),
        Zeroizing::new(value["password"].as_str().unwrap().to_owned()),
        Zeroizing::new(
            decode_exact::<32>(
                value["private_key"].as_str().unwrap(),
                "fixture private key",
            )
            .unwrap(),
        ),
    )
}

#[test]
fn web_unicode_password_record_opens_and_rewrap_keeps_identity() {
    let (record, password, expected) = fixture();
    let key = record.unlock(&password).unwrap();
    assert_eq!(key, expected);
    let next_password = Zeroizing::new("synthetic-next-password".to_owned());
    let cli: serde_json::Value =
        serde_json::from_str(include_str!("../../../tests/fixtures/privacy-cli-key.json")).unwrap();
    let expected_input: KeyInput = serde_json::from_value(cli["key"].clone()).unwrap();
    let nonce =
        decode_exact::<24>(&expected_input.encrypted_private_key.nonce, "fixture nonce").unwrap();
    let input =
        KeyInput::wrap_with_params(&key, &next_password, expected_input.kdf, nonce).unwrap();
    assert_eq!(input.public_key, record.key.public_key);
    assert_eq!(input.unlock(&next_password).unwrap(), expected);
    assert!(input.unlock(&password).is_err());
    assert_eq!(serde_json::to_value(input).unwrap(), cli["key"]);
}

#[test]
fn wrong_password_tampering_and_wrong_public_binding_never_return_a_key() {
    let (mut record, password, _) = fixture();
    // A small KDF cost keeps failure checks focused on authentication and key binding.
    record.key.kdf.opslimit = 1;
    record.key.kdf.memlimit = 8 * 1024 * 1024;
    let key = Zeroizing::new([0x42; 32]);
    record.key = KeyInput::wrap_with_params(&key, &password, record.key.kdf, [1; 24]).unwrap();
    record.recipient = recipient_id(&record.key.public_key);
    assert!(
        record
            .unlock(&Zeroizing::new(password.trim().to_owned()))
            .is_err()
    );
    assert!(record.unlock(&Zeroizing::new("wrong".into())).is_err());
    let original = record.clone();
    let mut ciphertext = decode_exact::<48>(
        &record.key.encrypted_private_key.ciphertext,
        "fixture ciphertext",
    )
    .unwrap();
    ciphertext[0] ^= 1;
    record.key.encrypted_private_key.ciphertext = STANDARD.encode(ciphertext);
    assert!(record.unlock(&password).is_err());
    record = original.clone();
    record.key.public_key = STANDARD.encode(x25519_dalek::x25519(
        [0x43; 32],
        x25519_dalek::X25519_BASEPOINT_BYTES,
    ));
    record.recipient = recipient_id(&record.key.public_key);
    assert!(
        record
            .unlock(&password)
            .unwrap_err()
            .to_string()
            .contains("does not match")
    );
    record = original;
    record.recipient = recipient_id(&STANDARD.encode([0x44; 32]));
    assert!(record.validate().is_err());
}

#[test]
fn malformed_records_are_rejected_before_derivation() {
    let (record, _, _) = fixture();
    let value = serde_json::to_value(record).unwrap();
    for (field, replacement) in [
        ("/kdf/algorithm", serde_json::json!("argon2i")),
        ("/kdf/opslimit", serde_json::json!(0)),
        ("/kdf/opslimit", serde_json::json!(11)),
        ("/kdf/memlimit", serde_json::json!(8192)),
        ("/kdf/memlimit", serde_json::json!(u64::MAX)),
        ("/kdf/salt", serde_json::json!(STANDARD.encode([0; 32]))),
        ("/encrypted_private_key/version", serde_json::json!(1)),
        (
            "/encrypted_private_key/algorithm",
            serde_json::json!("xchacha20-poly1305"),
        ),
        (
            "/encrypted_private_key/ciphertext",
            serde_json::json!("A".repeat(16384)),
        ),
        (
            "/encrypted_private_key/nonce",
            serde_json::json!(STANDARD.encode([0; 12])),
        ),
        ("/public_key_algorithm", serde_json::json!("ed25519")),
        ("/public_key", serde_json::json!(STANDARD.encode([0; 32]))),
        (
            "/public_key",
            serde_json::json!(" AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="),
        ),
    ] {
        let mut changed = value.clone();
        *changed.pointer_mut(field).unwrap() = replacement;
        let record: KeyRecord = serde_json::from_value(changed).unwrap();
        assert!(record.validate().is_err(), "accepted invalid {field}");
    }
}

#[test]
fn generation_uses_web_defaults_and_fresh_identity() {
    let password = Zeroizing::new("synthetic-generation-password".to_owned());
    let generated = KeyInput::generate(&password).unwrap();
    assert_eq!(generated.kdf.opslimit, 3);
    assert_eq!(generated.kdf.memlimit, 268435456);
    assert_eq!(generated.encrypted_private_key.version, 2);
    assert_eq!(
        generated.encrypted_private_key.algorithm,
        "xsalsa20-poly1305"
    );
    let key = generated.unlock(&password).unwrap();
    let (fixture, _, _) = fixture();
    assert_ne!(generated.public_key, fixture.key.public_key);
    assert_eq!(
        STANDARD.encode(x25519_dalek::x25519(
            *key,
            x25519_dalek::X25519_BASEPOINT_BYTES
        )),
        generated.public_key
    );
    assert!(KeyInput::generate(&Zeroizing::new(String::new())).is_err());
}

#[test]
fn shared_backend_web_password_record_matches_cli_key_bytes() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/privacy-password-key.json"
    ))
    .unwrap();
    let record: KeyRecord = serde_json::from_value(fixture["record"].clone()).unwrap();
    let private = record
        .unlock(&Zeroizing::new(
            fixture["test_only_password"].as_str().unwrap().into(),
        ))
        .unwrap();
    assert_eq!(
        STANDARD.encode(private.as_ref()),
        fixture["test_only_private_key"]
    );
}
