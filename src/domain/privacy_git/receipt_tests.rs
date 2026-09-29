use super::*;
use crate::domain::{
    privacy_receipt::{
        PublicationReceipt, SupervisorPushRequest,
        outbox::{Capture, Entry},
    },
    storage, transcript,
};
use agit_peer::{access::Principal, publication::Executor};
use base64::{Engine as _, engine::general_purpose::STANDARD};

/// Reclamation keeps pending work, distinct coverage and the current source's completion evidence.
#[test]
fn receipt_reclamation_requires_both_raw_histories_and_preserves_retry_evidence() {
    use agit_peer::publication::Receipt;
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
    let scope =
        digest_json(&json!({"version":VERSION, "destination":url, "recipient":"recipient"}))
            .unwrap();
    let cache_root = parent.join(scope.trim_start_matches("sha256:"));
    initialize(&cache_root).unwrap();
    let cache = Repo::at(cache_root).local_objects_only();
    let tree = write_object(&cache, "tree", b"").unwrap();
    let unrelated = write_object(
        &cache,
        "commit",
        format!("{}\n", commit_body(&tree, &[])).as_bytes(),
    )
    .unwrap();
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
        let public = write_object(
            &cache,
            "commit",
            commit_body(&tree, &previous_public).as_bytes(),
        )
        .unwrap();
        previous_public = vec![public.clone()];
        let mut request = SupervisorPushRequest {
            version: 1,
            request_id: uuid::Uuid::new_v4().to_string(),
            notification_id: None,
            repository: "owner/project".into(),
            branch: "work".into(),
            source: source_id,
            destination: crate::hub::identity::RemoteIdentity::new(
                "https://hub.invalid",
                "00000000-0000-0000-0000-000000000001",
            )
            .unwrap(),
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
            version: 1,
            mode: Default::default(),
            repository: request.repository.clone(),
            branch: request.branch.clone(),
            source: request.source.clone(),
            published: if index == 3 {
                unrelated.clone()
            } else {
                public
            },
            projected_session_id: Some(format!("agit-{}", "a".repeat(40))),
            destination: request.destination.clone(),
            url: url.into(),
            policy_digest: Some("policy".into()),
            recipient: Some("recipient".into()),
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

/// Receipt recovery uses its immutable generated snapshot even when the branch names another session.
#[test]
fn receipt_recovery_revalidates_the_saved_snapshot_without_replacing_evidence() {
    let temp = tempfile::tempdir().unwrap();
    let source = Repo::init(&temp.path().join("source")).unwrap();
    source.git(&["config", "user.name", "Fixture"]).unwrap();
    source
        .git(&["config", "user.email", "fixture@example.invalid"])
        .unwrap();
    source.git(&["checkout", "-b", "work"]).unwrap();
    let commit_session = |session: &str| {
        let raw = format!(
            "{}\n",
            json!({"type":"user","message":{"role":"user","content":"Public request"}})
        );
        let log = transcript::wrap_lines(&raw, "claude-code", session);
        storage::write_snapshot(source.root(), &log, &log).unwrap();
        meta::write(
            source.root(),
            &Meta::new(session.into(), "claude-code".into(), String::new()),
        )
        .unwrap();
        source.git(&["add", "."]).unwrap();
        source.git(&["commit", "-m", "Source session"]).unwrap();
        source.git(&["rev-parse", "HEAD"]).unwrap()
    };
    let native = format!("agit-{}", "a".repeat(40));
    let first_source = commit_session(&native);
    PrivacyPolicy {
        workspace: Some(source.root().canonicalize().unwrap()),
        ..Default::default()
    }
    .save(&source)
    .unwrap();
    let key = crypto_box::SecretKey::from([23; 32]);
    let recipient = ViewingRecipient::from_base64(
        "viewer".into(),
        &STANDARD.encode(key.public_key().as_bytes()),
    )
    .unwrap();
    let url = "https://hub.invalid/owner/project.git";
    let redactor = Redactor::new(Default::default());
    let prepare = || {
        ProjectedHistory::prepare_with_redactor(
            &source,
            &["work".into()],
            &recipient,
            url,
            &redactor,
        )
        .unwrap()
    };
    let history = prepare();
    let published = history.plan().heads()[0].oid().to_owned();
    let expected = storage::metadata_local(history.repo().root(), &published)
        .unwrap()
        .session;
    let cache = history.repo().clone();
    let receipt = PublicationReceipt {
        version: 1,
        mode: Default::default(),
        repository: "owner/project".into(),
        branch: "work".into(),
        source: first_source.clone(),
        published,
        projected_session_id: None,
        destination: crate::hub::identity::RemoteIdentity::new(
            "https://hub.invalid",
            "00000000-0000-0000-0000-000000000001",
        )
        .unwrap(),
        url: url.into(),
        policy_digest: Some(history.policy_digest().into()),
        recipient: Some(recipient.fingerprint().unwrap()),
    };
    drop(history);
    let second_source = commit_session(&format!("agit-{}", "b".repeat(40)));
    let advanced = prepare();
    let latest = storage::metadata_local(advanced.repo().root(), advanced.plan().heads()[0].oid())
        .unwrap()
        .session;
    assert_ne!(expected, latest);
    assert_ne!(expected, native);
    drop(advanced);
    let mut request = SupervisorPushRequest {
        version: 1,
        request_id: uuid::Uuid::new_v4().to_string(),
        repository: receipt.repository.clone(),
        branch: receipt.branch.clone(),
        source: first_source,
        destination: receipt.destination.clone(),
        notification_id: None,
    };
    let capture = Capture {
        session_id: "logical".into(),
        native_session_id: "native".into(),
        runtime: "claude-code".into(),
        generation: 1,
        incarnation: None,
        through_seq: None,
    };
    let original = Entry::begin(&source, &mut request, capture.clone()).unwrap();
    Entry::prepare(&source, &request, &receipt).unwrap();
    let executor = Executor {
        owner: Principal {
            issuer: "https://hub.invalid".into(),
            account_id: "owner".into(),
        },
        device_id: "executor".into(),
        credential_epoch: 1,
    };
    let notification = Entry::bind_notification(&source, &request, executor.clone()).unwrap();
    assert_eq!(notification.projected_session_id, expected);
    assert_eq!(notification.public_commit, receipt.published);
    assert_eq!(notification.notification_id, original.notification_id);
    assert_eq!(notification.capture, capture);
    let saved = Entry::load(&source, &request).unwrap().unwrap();
    assert_eq!(saved.prepared.unwrap().projected_session_id, Some(expected));
    assert!(saved.publication.is_none() && saved.acknowledged.is_none());
    assert_eq!(
        Entry::bind_notification(&source, &request, executor.clone()).unwrap(),
        notification
    );

    let mut missing = receipt.clone();
    missing.source = second_source;
    missing.published = "f".repeat(40);
    let mut request = SupervisorPushRequest {
        source: missing.source.clone(),
        notification_id: None,
        ..request
    };
    Entry::begin(&source, &mut request, capture).unwrap();
    Entry::prepare(&source, &request, &missing).unwrap();
    let before = serde_json::to_value(Entry::load(&source, &request).unwrap()).unwrap();
    assert!(Entry::bind_notification(&source, &request, executor).is_err());
    assert_eq!(
        serde_json::to_value(Entry::load(&source, &request).unwrap()).unwrap(),
        before
    );

    let mut wrong_scope = receipt.clone();
    wrong_scope.recipient = Some("another recipient".into());
    assert!(receipt_session_id(&source, &wrong_scope).is_err());
    let mut wrong_policy = receipt.clone();
    wrong_policy.policy_digest = Some("another policy".into());
    assert!(receipt_session_id(&source, &wrong_policy).is_err());

    cache.git(&["read-tree", &receipt.published]).unwrap();
    fs::write(cache.root().join("undeclared.txt"), "UNDECLARED_CONTENT").unwrap();
    cache.git(&["add", "undeclared.txt"]).unwrap();
    let tree = cache.git(&["write-tree"]).unwrap();
    let parents = commit_parents(
        &read(
            &cache,
            &["cat-file", "commit", &receipt.published],
            MAX_COMMIT,
        )
        .unwrap(),
    )
    .unwrap();
    let mut altered = receipt;
    altered.published =
        write_object(&cache, "commit", commit_body(&tree, &parents).as_bytes()).unwrap();
    assert!(
        receipt_session_id(&source, &altered).is_err(),
        "an envelope carrier with undeclared files must not supply a recovered identity"
    );
}
