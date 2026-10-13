use super::*;
use crate::domain::privacy_receipt::{
    PublicationReceipt, SupervisorPushRequest,
    outbox::{Capture, Entry},
};
use agit_peer::{access::Principal, publication::Executor};

/// Reclamation keeps pending work, distinct coverage and the current source's completion evidence.
#[test]
fn receipt_reclamation_requires_both_raw_histories_and_preserves_retry_evidence() {
    use agit_peer::publication::Receipt;
    const CHILD: &str = "AGIT_RECEIPT_RECLAMATION_FIXTURE";
    if std::env::var_os(CHILD).is_none() {
        // Concurrent test forks can temporarily inherit a publication lock before exec closes it.
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "domain::privacy_git::receipt_tests::receipt_reclamation_requires_both_raw_histories_and_preserves_retry_evidence",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "isolated receipt reclamation failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let source = Repo::init(&directory.path().join("source")).unwrap();
    source.git(&["config", "user.name", "Fixture"]).unwrap();
    source
        .git(&["config", "user.email", "fixture@example.invalid"])
        .unwrap();
    source.git(&["checkout", "-b", "work"]).unwrap();
    let parent = source
        .common_dir()
        .unwrap()
        .join("agit/privacy-publication");
    fs::create_dir_all(&parent).unwrap();
    fs::File::create(parent.join("lock")).unwrap();
    let url = "https://hub.invalid/owner/project.git";
    let identity = crate::hub::identity::RemoteIdentity::new(
        "https://hub.invalid",
        "00000000-0000-0000-0000-000000000001",
    )
    .unwrap();
    let store = "c".repeat(64);
    let cache_root = parent.join(&store);
    initialize_with_format(&cache_root, 40).unwrap();
    let cache = Repo::at(cache_root).local_objects_only();
    let policy = crate::domain::privacy_envelope::digest_bytes(b"policy");
    let owner_key = crate::domain::session_privacy::OwnerKeyBinding {
        account_id: "owner".into(),
        key_version: 1,
    };
    let key = crypto_box::SecretKey::from([41; 32]);
    use base64::Engine as _;
    let recipient = crate::domain::privacy_envelope::ViewingRecipient::from_base64(
        owner_key.recipient_id().unwrap(),
        &base64::engine::general_purpose::STANDARD.encode(key.public_key().as_bytes()),
    )
    .unwrap();
    let prepare = |source: &str, parents: &[String]| {
        let binding = crate::domain::session_privacy::SessionSnapshotBinding {
            repository_id: identity.agent_id.clone(),
            session_id: format!("agit-{}", "b".repeat(40)),
            published_session_id: format!("agit-{}", "a".repeat(40)),
            owner_key: owner_key.clone(),
            committer: crate::domain::session_privacy::CommitterAttribution::Account {
                account_id: "writer".into(),
            },
            source_commit: source.into(),
        };
        let envelope = PrivacyEnvelope::seal_session(
            policy.clone(),
            json!({"purpose":"receipt ancestry fixture"}),
            b"original",
            &recipient,
            binding.clone(),
        )
        .unwrap();
        let bytes = serde_json::to_vec(&envelope).unwrap();
        let tree = write_tree(
            &cache,
            BTreeMap::from([("privacy/envelope.json".into(), bytes.clone())]),
        )
        .unwrap();
        let public =
            write_object(&cache, "commit", commit_body(&tree, parents).as_bytes()).unwrap();
        crate::domain::session_privacy::AcceptedSnapshot {
            version: crate::domain::session_privacy::SESSION_ENVELOPE_VERSION,
            binding,
            published_commit: public,
            envelope_digest: crate::domain::privacy_envelope::digest_bytes(&bytes),
        }
    };
    let mut ledger = accepted::Ledger::open(&source, &identity).unwrap();
    let mut mappings = BTreeMap::new();
    let mut previous_source = Vec::new();
    let mut unrelated = String::new();
    let executor = Executor {
        owner: Principal {
            issuer: "https://hub.invalid".into(),
            account_id: "owner".into(),
        },
        device_id: "executor".into(),
        credential_epoch: 1,
    };
    let capture = Capture {
        session_id: "logical".into(),
        native_session_id: "native".into(),
        runtime: "codex".into(),
        incarnation: Some("daemon".into()),
        generation: 1,
        through_seq: Some(10),
    };
    let mut previous_public = vec![];
    let mut requests = vec![];
    for index in 0..6 {
        source
            .git(&["commit", "--allow-empty", "-m", &format!("Source {index}")])
            .unwrap();
        let source_id = source.git(&["rev-parse", "HEAD"]).unwrap();
        let canonical = prepare(&source_id, &previous_public);
        let snapshot = if index == 3 {
            prepare(&source_id, &previous_public)
        } else {
            canonical.clone()
        };
        if index == 3 {
            unrelated = snapshot.published_commit.clone();
        }
        let mapping = accepted::Mapping {
            source: source_id.clone(),
            public: canonical.published_commit.clone(),
            source_parents: previous_source.clone(),
            public_parents: previous_public.clone(),
            store: store.clone(),
            source_session: snapshot.binding.session_id.clone(),
            public_session: snapshot.binding.published_session_id.clone(),
            policy: policy.clone(),
            recipient: Some(owner_key.fingerprint().unwrap()),
        };
        mappings.insert(source_id.clone(), mapping);
        let mut candidate_mappings = mappings.clone();
        candidate_mappings.get_mut(&source_id).unwrap().public = snapshot.published_commit.clone();
        ledger
            .prepare(&BTreeMap::from([(
                "work".into(),
                accepted::Candidate {
                    source_head: source_id.clone(),
                    public_head: snapshot.published_commit.clone(),
                    mappings: candidate_mappings,
                },
            )]))
            .unwrap();
        previous_source = vec![source_id.clone()];
        previous_public = vec![canonical.published_commit.clone()];
        let mut request = SupervisorPushRequest {
            version: 1,
            request_id: uuid::Uuid::new_v4().to_string(),
            notification_id: None,
            repository: "owner/project".into(),
            branch: "work".into(),
            source: source_id,
            destination: identity.clone(),
        };
        let mut captured = capture.clone();
        if index == 1 {
            captured.generation += 1;
        }
        if index == 2 {
            captured.through_seq = Some(100);
        }
        Entry::begin(&source, &mut request, captured).unwrap();
        let publication = PublicationReceipt {
            version: 3,
            mode: crate::domain::privacy_receipt::PublicationMode::SessionOwnerV2,
            repository: request.repository.clone(),
            branch: request.branch.clone(),
            source: request.source.clone(),
            published: snapshot.published_commit.clone(),
            projected_session_id: Some(snapshot.binding.published_session_id.clone()),
            destination: request.destination.clone(),
            url: url.into(),
            policy_digest: Some(policy.clone()),
            recipient: Some(owner_key.fingerprint().unwrap()),
            session_snapshot: Some(snapshot),
        };
        Entry::prepare(&source, &request, &publication).unwrap();
        let notification = Entry::bind_notification(&source, &request, executor.clone()).unwrap();
        if index != 4 {
            Entry::acknowledge(
                &source,
                &request,
                &Receipt {
                    version: 1,
                    receipt_id: format!("receipt-{index}"),
                    notification_id: notification.notification_id.clone(),
                    binding_digest: notification.digest().unwrap(),
                    repository_id: notification.repository_id,
                    public_commit: notification.public_commit,
                },
            )
            .unwrap();
        }
        requests.push(request);
    }
    let latest = requests.last().unwrap();
    source
        .git(&["update-ref", "refs/heads/work", &requests[0].source])
        .unwrap();
    assert_eq!(Entry::reclaim_acknowledged(&source, latest).unwrap(), 0);
    assert!(
        Entry::load(&source, &requests[0])
            .unwrap()
            .unwrap()
            .acknowledged
            .is_some()
    );
    source
        .git(&["update-ref", "refs/heads/work", &latest.source])
        .unwrap();

    // Topology overlays must neither hide covered ancestors nor invent coverage for a fork.
    cache
        .git(&["replace", "--graft", &previous_public[0], &unrelated])
        .unwrap();
    fs::write(
        cache.root().join(".git/shallow"),
        format!("{}\n", previous_public[0]),
    )
    .unwrap();
    assert_eq!(Entry::reclaim_acknowledged(&source, latest).unwrap(), 1);
    assert!(Entry::load(&source, &requests[0]).unwrap().is_none());
    for request in &requests[1..] {
        assert!(Entry::load(&source, request).unwrap().is_some());
    }
    assert!(
        Entry::load(&source, &requests[4])
            .unwrap()
            .unwrap()
            .acknowledged
            .is_none()
    );
    let mut unchanged = latest.clone();
    unchanged.notification_id = None;
    let retained = Entry::begin(&source, &mut unchanged, capture).unwrap();
    assert_eq!(
        retained.notification_id,
        latest.notification_id.clone().unwrap()
    );
    assert!(retained.publication.is_some() && retained.acknowledged.is_some());
    assert_eq!(Entry::reclaim_acknowledged(&source, latest).unwrap(), 0);
    assert_eq!(
        Entry::records(&source, "work", "native", "codex")
            .unwrap()
            .len(),
        5
    );
}
