use super::*;
use crate::domain::{
    meta,
    privacy_envelope::ViewingRecipient,
    privacy_key::{KeyRecord, recipient_id},
    privacy_layer::PrivateLayer,
    repo::Repo,
    storage, transcript,
};
use serde_json::json;
use std::{
    cell::RefCell,
    collections::BTreeMap,
    io::{BufRead, BufReader, Write},
    net::TcpListener,
    process::Command,
    time::{Duration, Instant},
};

#[derive(Default)]
struct MemoryStore(RefCell<BTreeMap<String, Vec<u8>>>);
impl CredentialStore for MemoryStore {
    fn get(&self, name: &str) -> anyhow::Result<Option<Zeroizing<Vec<u8>>>> {
        Ok(self.0.borrow().get(name).cloned().map(Zeroizing::new))
    }
    fn set(&self, name: &str, bytes: &[u8]) -> anyhow::Result<()> {
        self.0.borrow_mut().insert(name.into(), bytes.to_vec());
        Ok(())
    }
    fn delete(&self, name: &str) -> anyhow::Result<()> {
        self.0.borrow_mut().remove(name);
        Ok(())
    }
}

#[test]
fn saved_unlock_rechecks_remote_scope_before_recovery() {
    const CHILD: &str = "AGIT_TEST_SAVED_UNLOCK_ROOT";
    let root = match std::env::var_os(CHILD) {
        Some(root) => std::path::PathBuf::from(root),
        None => {
            let root = tempfile::tempdir().unwrap();
            let mut child = Command::new(std::env::current_exe().unwrap());
            child.env_clear();
            for name in ["PATH", "SystemRoot", "TEMP", "TMP"] {
                if let Some(value) = std::env::var_os(name) {
                    child.env(name, value);
                }
            }
            let result = child.args(["--exact", "commands::privacy::unlock::tests::saved_unlock_rechecks_remote_scope_before_recovery", "--nocapture"])
                .env(CHILD, root.path()).env("AGIT_HOME", root.path().join("agit"))
                .env("HOME", root.path()).env("USERPROFILE", root.path())
                .env("AGIT_SECRETS_KEYSTORE", "file").env("AGIT_USE_SYSTEM_GIT", "1")
                .env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_CONFIG_GLOBAL", root.path().join("no-config"))
                .env("GIT_AUTHOR_NAME", "Unlock fixture").env("GIT_AUTHOR_EMAIL", "unlock@example.invalid")
                .env("GIT_COMMITTER_NAME", "Unlock fixture").env("GIT_COMMITTER_EMAIL", "unlock@example.invalid")
                .current_dir(root.path()).output().unwrap();
            assert!(result.status.success(), "{result:?}");
            assert!(String::from_utf8_lossy(&result.stdout).contains("saved unlock verified"));
            return;
        }
    };
    let repo = Repo::init(&root.join("agit/repos/alice/app")).unwrap();
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/privacy-web-key.json"
    ))
    .unwrap();
    let record: KeyRecord = serde_json::from_value(fixture["record"].clone()).unwrap();
    let public = record.key.public_key.clone();
    let session = format!("agit-{}", "b".repeat(40));
    let log = transcript::wrap_lines(
        "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"private remembered session\"}}\n",
        "claude-code",
        &format!("agit-{}", "a".repeat(40)),
    );
    let layer = PrivateLayer::new(
        &log,
        &log,
        serde_json::to_value(meta::Meta::new(
            format!("agit-{}", "a".repeat(40)),
            "claude-code".into(),
            "/source/workspace".into(),
        ))
        .unwrap(),
        BTreeMap::new(),
    )
    .unwrap();
    let envelope = PrivacyEnvelope::seal_layer(
        digest_bytes(b"policy"),
        digest_bytes(b"snapshot"),
        json!({"metadata":{"session":session}}),
        &layer,
        &ViewingRecipient::from_base64(recipient_id(&public), &public).unwrap(),
        Vec::new(),
    )
    .unwrap();
    std::fs::create_dir_all(repo.root().join("privacy")).unwrap();
    std::fs::write(
        repo.root().join("privacy/envelope.json"),
        serde_json::to_vec(&envelope).unwrap(),
    )
    .unwrap();
    repo.add_all().unwrap();
    repo.commit("Encrypted fixture").unwrap();
    let first = repo.git(&["rev-parse", "HEAD"]).unwrap();
    repo.git(&[
        "commit",
        "--allow-empty",
        "-m",
        "Another encrypted snapshot",
    ])
    .unwrap();
    let second = repo.git(&["rev-parse", "HEAD"]).unwrap();
    repo.git(&["commit", "--allow-empty", "-m", "Reused repository fixture"])
        .unwrap();
    let third = repo.git(&["rev-parse", "HEAD"]).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let hub = format!("http://{}", listener.local_addr().unwrap());
    let identity =
        crate::hub::identity::RemoteIdentity::new(&hub, &uuid::Uuid::from_u128(1).to_string())
            .unwrap();
    crate::hub::identity::pin(&repo, &identity).unwrap();
    repo.set_remote(&format!("{hub}/alice/app.git")).unwrap();
    let scope = Scope {
        hub: hub.clone(),
        recipient: record.recipient.clone(),
        agent_id: identity.agent_id.clone(),
        viewing_public_key: public,
    };
    let store = MemoryStore::default();
    let commits = [
        first.clone(),
        first.clone(),
        second.clone(),
        third.clone(),
        third.clone(),
    ];
    let server = std::thread::spawn(move || {
        let started = Instant::now();
        for (index, commit) in commits.iter().enumerate() {
            let mut stream = loop {
                assert!(
                    started.elapsed() < Duration::from_secs(90),
                    "missing scope check"
                );
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5))
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(&mut stream);
            let mut request = String::new();
            reader.read_line(&mut request).unwrap();
            let slug = if index < 2 {
                "alice/app"
            } else {
                "alice/renamed"
            };
            assert!(
                request.starts_with(&format!(
                    "GET /api/agents/{slug}/privacy/keys/{}?ref={commit} ",
                    record.recipient
                )),
                "{request}"
            );
            loop {
                let mut header = String::new();
                assert!(reader.read_line(&mut header).unwrap() > 0);
                if header == "\r\n" {
                    break;
                }
                assert!(!header.to_ascii_lowercase().starts_with("authorization:"));
            }
            let (status, response) = if index == 3 {
                (
                    404,
                    json!({"kind":"not_found","error":"private repository"}),
                )
            } else {
                (
                    200,
                    json!({"agent_id":uuid::Uuid::from_u128(if index < 4 { 1 } else { 2 }).to_string(), "commit":commit,"session_id":session,"key":record}),
                )
            };
            let response = response.to_string();
            write!(stream, "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).unwrap();
        }
    });
    let cache = repo.common_dir().unwrap().join("agit/privacy-recovery");
    assert!(
        run_with_store(
            &format!("alice/app@{first}"),
            Some(&root),
            Some(1),
            false,
            &store,
            || Ok(Zeroizing::new("wrong".into()))
        )
        .is_err()
    );
    assert!(!cache.join(&first).exists());
    assert!(store.0.borrow().is_empty());
    run_with_store(
        &format!("alice/app@{first}"),
        Some(&root),
        Some(1),
        false,
        &store,
        || Ok(Zeroizing::new(fixture["password"].as_str().unwrap().into())),
    )
    .unwrap();
    assert!(privacy_credentials::recall(&store, &scope, Utc::now()).is_ok());
    assert_eq!(
        storage::materialize_worktree(&cache.join(&first), meta::LOG_FILE).unwrap(),
        log
    );
    repo.set_remote(&format!("{hub}/alice/renamed.git"))
        .unwrap();
    run_with_store(
        &format!("alice/app@{second}"),
        Some(&root),
        None,
        true,
        &store,
        || panic!("saved key must not prompt"),
    )
    .unwrap();
    assert_eq!(
        storage::materialize_worktree(&cache.join(&second), meta::LOG_FILE).unwrap(),
        log
    );
    for _ in 0..2 {
        assert!(
            run_with_store(
                &format!("alice/app@{third}"),
                Some(&root),
                None,
                true,
                &store,
                || panic!("reader refusal must not prompt")
            )
            .is_err()
        );
    }
    assert!(!cache.join(&third).exists());
    server.join().unwrap();
    privacy_credentials::forget(&store, &identity.hub, &identity.agent_id).unwrap();
    assert!(store.0.borrow().is_empty());
    println!("saved unlock verified");
}
