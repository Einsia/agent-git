use super::*;
use crate::domain::privacy_key::KeyRecord;
use serde_json::Value;

#[test]
fn noninteractive_configuration_refuses_without_a_password() {
    let error = ensure_interactive(false).unwrap_err();
    assert!(error.is::<InteractionRequired>());
    assert!(ensure_interactive(true).is_ok());
}

#[test]
fn configuration_checks_state_before_password_entry() {
    let empty = RepositoryKeyConfig {
        agent_id: "test".into(),
        config_version: 9,
        current_recipient: None,
        keys: vec![],
    };
    assert!(
        prepare(&empty, Operation::Rewrap, |_, _| panic!(
            "must validate current configuration first"
        ))
        .is_err()
    );
    assert!(
        prepare(&empty, Operation::Rotate, |_, _| panic!(
            "must validate current configuration first"
        ))
        .is_err()
    );
}

#[test]
fn password_change_rewraps_the_fetched_key_and_rotation_generates_another() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/privacy-web-key.json"
    ))
    .unwrap();
    let record: KeyRecord = serde_json::from_value(fixture["record"].clone()).unwrap();
    let current = RepositoryKeyConfig {
        agent_id: "test".into(),
        config_version: 9,
        current_recipient: Some(record.recipient.clone()),
        keys: vec![record.clone()],
    };
    let (changed, recipient) = prepare(&current, Operation::Rewrap, |_, confirm| {
        Ok(Zeroizing::new(
            if confirm {
                "synthetic-new-password"
            } else {
                fixture["password"].as_str().unwrap()
            }
            .into(),
        ))
    })
    .unwrap();
    assert_eq!(recipient.as_ref(), Some(&record.recipient));
    assert_eq!(changed.public_key, record.key.public_key);
    assert!(changed != record.key);
    let (rotated, recipient) = prepare(&current, Operation::Rotate, |_, confirm| {
        assert!(confirm);
        Ok(Zeroizing::new("synthetic-rotation-password".into()))
    })
    .unwrap();
    assert!(recipient.is_none());
    assert_ne!(rotated.public_key, record.key.public_key);
    assert_eq!(current.config_version, 9);
    assert!(
        prepare(&current, Operation::Initialize, |_, _| panic!(
            "must validate configuration first"
        ))
        .is_err()
    );
}

#[test]
fn browser_identity_binding_retains_the_destination_and_refuses_unrelated_origins() {
    let root = tempfile::tempdir().unwrap();
    let repo = Repo::init(root.path()).unwrap();
    let expected = RemoteIdentity::new(
        "https://hub.example",
        "ef306cd5-e4d3-4a3c-a8ce-e3c2f65b48d4",
    )
    .unwrap();
    repo.set_remote("https://other.example/alice/app.git")
        .unwrap();
    assert!(pin_browser_identity(&repo, "alice/app", &expected).is_err());
    assert!(identity::read(&repo).unwrap().is_none());
    repo.set_remote("https://hub.example/alice/app.git")
        .unwrap();
    pin_browser_identity(&repo, "alice/app", &expected).unwrap();
    assert_eq!(identity::read(&repo).unwrap().as_ref(), Some(&expected));
    let before = std::fs::read(repo.root().join(".git/config")).unwrap();
    pin_browser_identity(&repo, "alice/app", &expected).unwrap();
    let replacement = RemoteIdentity::new(
        "https://hub.example",
        "a31b3a75-8245-4ae5-83a7-52a612012c90",
    )
    .unwrap();
    assert!(pin_browser_identity(&repo, "alice/app", &replacement).is_err());
    assert_eq!(
        std::fs::read(repo.root().join(".git/config")).unwrap(),
        before
    );
}
