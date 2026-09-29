use super::*;
use crate::domain::{
    privacy_key::recipient_id, privacy_receipt::PublicationReceipt, storage, transcript,
};
use crate::hub::{git::RemoteRefs, identity::RemoteIdentity};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use crypto_box::SecretKey;

fn recipient(seed: u8) -> (SecretKey, ViewingRecipient) {
    let key = SecretKey::from([seed; 32]);
    let public = STANDARD.encode(key.public_key().as_bytes());
    let recipient = ViewingRecipient::from_base64(recipient_id(&public), &public).unwrap();
    (key, recipient)
}

fn append(repo: &Repo, n: usize) -> String {
    let session = format!("agit-{}", "b".repeat(40));
    let raw = format!(
        "{}\n",
        json!({"type":"user","message":{"role":"user","content":format!("Synthetic turn {n}")}})
    );
    let log = transcript::wrap_lines(&raw, "claude-code", &session);
    storage::write_snapshot(repo.root(), &log, &log).unwrap();
    meta::write(
        repo.root(),
        &Meta::new(session, "claude-code".into(), String::new()),
    )
    .unwrap();
    repo.add_all().unwrap();
    repo.commit(&format!("Source {n}")).unwrap();
    repo.git(&["rev-parse", "HEAD"]).unwrap()
}

fn remote(history: &ProjectedHistory) -> RemoteRefs {
    RemoteRefs {
        heads: history
            .plan
            .heads()
            .iter()
            .map(|head| head.oid().to_owned())
            .collect(),
        tags: history
            .plan
            .tags()
            .iter()
            .map(|tag| (tag.name().to_owned(), tag.oid().to_owned()))
            .collect(),
        refs: history
            .plan
            .heads()
            .iter()
            .chain(history.plan.tags())
            .map(|reference| (reference.name().to_owned(), reference.oid().to_owned()))
            .collect(),
    }
}

#[test]
fn accepted_ancestry_survives_rewrap_rotation_cache_rebuild_and_lost_response() {
    let temp = tempfile::tempdir().unwrap();
    let source = Repo::init(&temp.path().join("source")).unwrap();
    source.git(&["config", "commit.gpgsign", "false"]).unwrap();
    source.git(&["checkout", "-b", "work"]).unwrap();
    let first_source = append(&source, 1);
    PrivacyPolicy::default().save(&source).unwrap();
    let identity = RemoteIdentity::new(
        "https://hub.invalid",
        "00000000-0000-0000-0000-000000000001",
    )
    .unwrap();
    let url = "https://hub.invalid/alice/app.git";
    crate::hub::identity::pin(&source, &identity).unwrap();
    source.set_remote(url).unwrap();
    #[cfg(feature = "cli")]
    let reuse = |point: &str| {
        crate::commands::run::reuse_receipt(
            &source,
            &identity.hub,
            ("alice", "app"),
            None,
            point,
            crate::hub::reuse::ReuseMode::Continue,
        )
    };
    let redactor = Redactor::new(Default::default());
    let (old_key, old) = recipient(20);
    let (new_key, new) = recipient(21);
    let prepare = |repo: &Repo, recipient: &ViewingRecipient, remote: &RemoteRefs, url: &str| {
        ProjectedHistory::prepare_with_rules(
            repo,
            &["work".into()],
            recipient,
            url,
            &redactor,
            &[],
            Some((&identity, remote)),
        )
        .unwrap()
    };
    let mut first = prepare(&source, &old, &RemoteRefs::default(), url);
    let first_public = first.plan.heads()[0].oid().to_owned();
    let public_session = storage::metadata_local(first.repo.root(), &first_public)
        .unwrap()
        .session;
    let first_cache = first.repo.root().to_owned();
    let first_remote = remote(&first);
    let receipt = PublicationReceipt {
        version: 1,
        mode: Default::default(),
        repository: "alice/app".into(),
        branch: "work".into(),
        source: first_source.clone(),
        published: first_public.clone(),
        projected_session_id: Some(public_session.clone()),
        destination: identity.clone(),
        url: url.into(),
        policy_digest: Some(first.policy_digest.clone()),
        recipient: Some(old.fingerprint().unwrap()),
    };
    first.prepare_acceptance().unwrap();
    drop(first);
    let ledger = accepted::Ledger::open(&source, &identity).unwrap();
    assert!(
        ledger.get("work", &first_source).is_none(),
        "prepared objects are not accepted"
    );
    assert_eq!(
        accepted_session_identity(&source, None, &first_source, &identity).unwrap(),
        None,
        "a prepared projection cannot authorize a reuse notification"
    );
    #[cfg(feature = "cli")]
    assert_eq!(reuse(&first_source), None);
    // A matching authenticated advertisement reconciles a response lost after receive.
    let same = prepare(&source, &old, &first_remote, url);
    assert_eq!(same.plan.heads()[0].oid(), first_public);
    assert_eq!(same.preparations, 0);
    drop(same);
    assert_ne!(public_session, format!("agit-{}", "b".repeat(40)));
    assert_ne!(first_public, first_source);
    assert_eq!(
        accepted_session_identity(&source, None, &first_source, &identity).unwrap(),
        Some((public_session.clone(), first_public.clone()))
    );
    #[cfg(feature = "cli")]
    assert_eq!(
        reuse(&first_source),
        crate::hub::reuse::SessionReuse::new(
            "alice",
            "app",
            &public_session,
            &first_public,
            crate::hub::reuse::ReuseMode::Continue,
        )
    );
    let other_identity =
        RemoteIdentity::new(&identity.hub, "00000000-0000-0000-0000-000000000002").unwrap();
    assert_eq!(
        accepted_session_identity(&source, None, &first_source, &other_identity).unwrap(),
        None,
        "publication mappings cannot cross repository identities"
    );
    let (selected, envelope) = accepted_envelope(&source, None, &first_source, &identity).unwrap();
    assert_eq!(selected, first_public);
    assert!(envelope.open_layer(&old_key).is_ok());
    assert_eq!(
        receipt_session_id(&source, &receipt).unwrap(),
        public_session
    );
    fs::remove_dir_all(first_cache.join(".git/privacy-preparation")).unwrap();
    let rotated = prepare(&source, &new, &first_remote, url);
    assert_eq!(rotated.plan.heads()[0].oid(), first_public);
    assert_eq!(remote(&rotated).tags, first_remote.tags);
    assert_eq!(rotated.preparations, 0);
    assert_eq!(
        rotated.receipt_binding("work").unwrap().unwrap().1,
        receipt.recipient.as_deref().unwrap()
    );
    drop(rotated);
    assert_eq!(
        receipt_session_id(&source, &receipt).unwrap(),
        public_session
    );
    let second_source = append(&source, 2);
    #[cfg(feature = "cli")]
    assert_eq!(
        reuse(&second_source),
        None,
        "unpublished turns remain private"
    );
    let mut appended = prepare(&source, &new, &first_remote, url);
    assert_eq!(appended.preparations, 1);
    let second_public = appended.plan.heads()[0].oid().to_owned();
    assert_eq!(
        appended
            .repo
            .git(&["rev-parse", &format!("{second_public}^")])
            .unwrap(),
        first_public
    );
    assert_eq!(
        storage::metadata_local(appended.repo.root(), &second_public)
            .unwrap()
            .session,
        public_session
    );
    let envelope = PrivacyEnvelope::parse(
        appended
            .repo
            .show_result(&second_public, "privacy/envelope.json")
            .unwrap()
            .unwrap()
            .as_bytes(),
    )
    .unwrap();
    assert_eq!(envelope.private_payload.wrapped_keys[0].recipient, new.id());
    assert!(envelope.open_layer(&new_key).is_ok());
    assert!(envelope.open_layer(&old_key).is_err());
    let after = remote(&appended);
    for (tag, oid) in &first_remote.tags {
        assert_eq!(after.tags.get(tag), Some(oid));
    }
    appended.prepare_acceptance().unwrap();
    drop(appended);
    let repeated = prepare(&source, &new, &after, url);
    assert_eq!(repeated.plan.heads()[0].oid(), second_public);
    assert_eq!(repeated.preparations, 0);
    drop(repeated);
    assert!(
        accepted::Ledger::open(&source, &identity)
            .unwrap()
            .get("work", &second_source)
            .is_some()
    );
    let renamed = prepare(
        &source,
        &new,
        &after,
        "https://hub.invalid/alice/renamed.git",
    );
    assert_eq!(renamed.plan.heads()[0].oid(), second_public);
    assert_eq!(renamed.preparations, 0);
    drop(renamed);
    let another =
        RemoteIdentity::new(&identity.hub, "00000000-0000-0000-0000-000000000002").unwrap();
    assert!(
        ProjectedHistory::prepare_with_rules(
            &source,
            &["work".into()],
            &new,
            url,
            &redactor,
            &[],
            Some((&another, &after))
        )
        .is_err()
    );
    let mut policy = PrivacyPolicy::default();
    policy.exclude.push("private/**".into());
    policy.save(&source).unwrap();
    let changed_policy = prepare(&source, &new, &after, url);
    assert!(changed_policy.verify_accepted_policy().is_err());
    drop(changed_policy);
    let ledger_root = source.common_dir().unwrap().join("agit/privacy-accepted");
    for entry in fs::read_dir(ledger_root).unwrap() {
        let path = entry.unwrap().path().join("index.enc");
        if path.is_file() {
            fs::write(path, b"invalid authenticated mapping").unwrap();
        }
    }
    assert!(
        ProjectedHistory::prepare_with_rules(
            &source,
            &["work".into()],
            &new,
            url,
            &redactor,
            &[],
            Some((&identity, &after))
        )
        .is_err()
    );
}

#[test]
fn interrupted_batch_needs_remote_reconciliation_before_acceptance() {
    use crate::hub::git::{
        Outcome, PublicationAttempt, PublicationPhase, PublicationReport, PublicationStatus,
        PublishedRef,
    };
    let temp = tempfile::tempdir().unwrap();
    let source = Repo::init(&temp.path().join("source")).unwrap();
    source.git(&["config", "commit.gpgsign", "false"]).unwrap();
    source.git(&["checkout", "-b", "work"]).unwrap();
    let first = append(&source, 1);
    let second = append(&source, 2);
    let identity = RemoteIdentity::new(
        "https://hub.invalid",
        "00000000-0000-0000-0000-000000000001",
    )
    .unwrap();
    let (_, key) = recipient(25);
    let mut history = ProjectedHistory::prepare_with_rules(
        &source,
        &["work".into()],
        &key,
        "https://hub.invalid/alice/app.git",
        &Redactor::new(Default::default()),
        &[],
        Some((&identity, &RemoteRefs::default())),
    )
    .unwrap();
    history.prepare_acceptance().unwrap();
    let partial = PublicationReport {
        lfs: None,
        tags: None,
        error: Some("lost response".into()),
        heads: Some(PublicationPhase {
            error: None,
            unattempted: vec![],
            attempts: vec![PublicationAttempt {
                batch: 0,
                outcome: Outcome {
                    code: 1,
                    stderr: "lost response".into(),
                },
                complete: false,
                error: None,
                refs: vec![PublishedRef {
                    reference: history.plan.heads()[0].clone(),
                    status: PublicationStatus::Updated,
                    detail: None,
                }],
            }],
        }),
    };
    history.confirm_acceptance(&partial).unwrap();
    assert!(
        history
            .accepted_ledger
            .as_ref()
            .unwrap()
            .get("work", &second)
            .is_none()
    );
    let candidate = &history.candidates["work"];
    let mut observed = RemoteRefs::default();
    observed.refs.insert(
        "refs/heads/work".into(),
        candidate.mappings[&first].public.clone(),
    );
    history
        .accepted_ledger
        .as_mut()
        .unwrap()
        .reconcile(&observed)
        .unwrap();
    assert!(
        history
            .accepted_ledger
            .as_ref()
            .unwrap()
            .get("work", &first)
            .is_some()
    );
    assert!(
        history
            .accepted_ledger
            .as_ref()
            .unwrap()
            .get("work", &second)
            .is_none()
    );
    let observed = remote(&history);
    history
        .accepted_ledger
        .as_mut()
        .unwrap()
        .reconcile(&observed)
        .unwrap();
    assert!(
        history
            .accepted_ledger
            .as_ref()
            .unwrap()
            .get("work", &second)
            .is_some()
    );
}

#[test]
fn fork_keeps_accepted_parent_and_its_own_session_after_rotation() {
    let temp = tempfile::tempdir().unwrap();
    let source = Repo::init(&temp.path().join("source")).unwrap();
    source.git(&["config", "commit.gpgsign", "false"]).unwrap();
    source.git(&["checkout", "-b", "work"]).unwrap();
    let original = append(&source, 1);
    let mut policy = PrivacyPolicy::default();
    policy.branches.insert(
        "restricted".into(),
        super::super::privacy::BranchRestriction {
            exclude: vec!["**".into()],
            memory_exclude: vec![],
        },
    );
    policy.save(&source).unwrap();
    let identity = RemoteIdentity::new(
        "https://hub.invalid",
        "00000000-0000-0000-0000-000000000001",
    )
    .unwrap();
    let url = "https://hub.invalid/alice/app.git";
    let redactor = Redactor::new(Default::default());
    let (old_key, old) = recipient(20);
    let (new_key, new) = recipient(21);
    let mut first = ProjectedHistory::prepare_with_rules(
        &source,
        &["work".into()],
        &old,
        url,
        &redactor,
        &[],
        Some((&identity, &RemoteRefs::default())),
    )
    .unwrap();
    let first_public = first.plan.heads()[0].oid().to_owned();
    let first_remote = remote(&first);
    first.prepare_acceptance().unwrap();
    drop(first);
    let same = ProjectedHistory::prepare_with_rules(
        &source,
        &["work".into()],
        &old,
        url,
        &redactor,
        &[],
        Some((&identity, &first_remote)),
    )
    .unwrap();
    drop(same);
    let base = crate::commands::fork::ForkBase {
        repo: source.clone(),
        slug: "alice/app".into(),
        resolved: crate::domain::refs::Resolved {
            branch: Some("work".into()),
            sha: original.clone(),
            turn: None,
            event_index: None,
            range: None,
            path: None,
        },
    };
    crate::commands::fork::fork_branch(&base, "alice/app@work", "continuation")
        .unwrap()
        .unwrap();
    let next = ProjectedHistory::prepare_with_rules(
        &source,
        &["continuation".into()],
        &new,
        url,
        &redactor,
        &[],
        Some((&identity, &first_remote)),
    )
    .unwrap();
    let next_public = next.plan.heads()[0].oid();
    let parent = next
        .repo
        .git(&["rev-parse", &format!("{next_public}^")])
        .unwrap();
    assert_eq!(
        parent, first_public,
        "Forking an already published source must retain its accepted public parent, including across key rotation"
    );
    assert_eq!(next.preparations, 1);
    let original_session = storage::metadata_local(next.repo.root(), &first_public)
        .unwrap()
        .session;
    let fork_session = storage::metadata_local(next.repo.root(), next_public)
        .unwrap()
        .session;
    assert_ne!(original_session, fork_session);
    let envelope = PrivacyEnvelope::parse(
        next.repo
            .show_result(next_public, "privacy/envelope.json")
            .unwrap()
            .unwrap()
            .as_bytes(),
    )
    .unwrap();
    assert!(envelope.open_layer(&new_key).is_ok());
    assert!(envelope.open_layer(&old_key).is_err());
    let inherited = PrivacyEnvelope::parse(
        next.repo
            .show_result(&first_public, "privacy/envelope.json")
            .unwrap()
            .unwrap()
            .as_bytes(),
    )
    .unwrap();
    assert!(inherited.open_layer(&old_key).is_ok());
    drop(next);
    crate::commands::fork::fork_branch(&base, "alice/app@work", "restricted")
        .unwrap()
        .unwrap();
    let error = ProjectedHistory::prepare_with_rules(
        &source,
        &["restricted".into()],
        &new,
        url,
        &redactor,
        &[],
        Some((&identity, &first_remote)),
    )
    .err()
    .unwrap();
    assert!(
        error
            .to_string()
            .contains("incompatible branch privacy restrictions"),
        "{error:#}"
    );
}

#[test]
fn fork_after_clone_continuation_keeps_original_public_policy() {
    let temp = tempfile::tempdir().unwrap();
    let source = Repo::init(&temp.path().join("source")).unwrap();
    source.git(&["config", "commit.gpgsign", "false"]).unwrap();
    source.git(&["checkout", "-b", "work"]).unwrap();
    append(&source, 1);
    let original_policy = PrivacyPolicy::default();
    original_policy.save(&source).unwrap();
    let identity = RemoteIdentity::new(
        "https://hub.invalid",
        "00000000-0000-0000-0000-000000000001",
    )
    .unwrap();
    let url = "https://hub.invalid/alice/app.git";
    let redactor = Redactor::new(Default::default());
    let (_, old) = recipient(20);
    let (new_key, new) = recipient(21);
    let prepare = |repo: &Repo, branch: &str, key: &ViewingRecipient, remote: &RemoteRefs| {
        ProjectedHistory::prepare_with_rules(
            repo,
            &[branch.into()],
            key,
            url,
            &redactor,
            &[],
            Some((&identity, remote)),
        )
        .unwrap()
    };
    let first = prepare(&source, "work", &old, &RemoteRefs::default());
    let first_public = first.plan.heads()[0].oid().to_owned();
    let first_remote = remote(&first);
    let cloned = Repo::init(&temp.path().join("clone")).unwrap();
    cloned
        .git(&[
            "fetch",
            first.repo.root().to_str().unwrap(),
            "refs/heads/work:refs/heads/work",
        ])
        .unwrap();
    cloned.git(&["checkout", "work"]).unwrap();
    cloned.git(&["config", "commit.gpgsign", "false"]).unwrap();
    let mut local_policy = original_policy.clone();
    local_policy.exclude.push("private/**".into());
    local_policy.save(&cloned).unwrap();
    let continuation = append(&cloned, 2);
    let mut continued = prepare(&cloned, "work", &new, &first_remote);
    continued.verify_accepted_policy().unwrap();
    let accepted_parent = continued.plan.heads()[0].oid().to_owned();
    let current_remote = remote(&continued);
    continued.prepare_acceptance().unwrap();
    drop(continued);
    let reconciled = prepare(&cloned, "work", &new, &current_remote);
    reconciled.verify_accepted_policy().unwrap();
    drop(reconciled);
    let base = crate::commands::fork::ForkBase {
        repo: cloned.clone(),
        slug: "alice/app".into(),
        resolved: crate::domain::refs::Resolved {
            branch: Some("work".into()),
            sha: continuation.clone(),
            turn: None,
            event_index: None,
            range: None,
            path: None,
        },
    };
    crate::commands::fork::fork_branch(&base, "alice/app@work", "continuation")
        .unwrap()
        .unwrap();
    let forked = prepare(&cloned, "continuation", &new, &current_remote);
    forked.verify_accepted_policy().unwrap();
    let fork_tip = forked.plan.heads()[0].oid();
    assert_eq!(
        forked
            .repo
            .git(&["rev-parse", &format!("{fork_tip}^")])
            .unwrap(),
        accepted_parent
    );
    assert_eq!(
        forked
            .repo
            .git(&["rev-parse", &format!("{fork_tip}^^")])
            .unwrap(),
        first_public
    );
    assert_eq!(forked.preparations, 1);
    assert_ne!(
        storage::metadata_local(forked.repo.root(), fork_tip)
            .unwrap()
            .session,
        storage::metadata_local(forked.repo.root(), &accepted_parent)
            .unwrap()
            .session
    );
    let envelope = PrivacyEnvelope::parse(
        forked
            .repo
            .show_result(fork_tip, "privacy/envelope.json")
            .unwrap()
            .unwrap()
            .as_bytes(),
    )
    .unwrap();
    assert_eq!(envelope.policy_digest, local_policy.digest().unwrap());
    assert_eq!(envelope.private_payload.wrapped_keys[0].recipient, new.id());
    assert!(envelope.open_layer(&new_key).is_ok());
    let ledger = accepted::Ledger::open(&cloned, &identity).unwrap();
    let inherited = ledger.get("work", &first_public).unwrap();
    assert_eq!(inherited.source, inherited.public);
    assert_eq!(inherited.policy, original_policy.digest().unwrap());
    let mut changed_policy = local_policy.clone();
    changed_policy.exclude.push("confidential/**".into());
    assert!(
        ledger
            .inherited("continuation", &continuation, &changed_policy)
            .is_err()
    );
    let mut restricted_policy = local_policy;
    restricted_policy.branches.insert(
        "continuation".into(),
        super::super::privacy::BranchRestriction {
            exclude: vec!["**".into()],
            memory_exclude: vec![],
        },
    );
    assert!(
        ledger
            .inherited("continuation", &first_public, &restricted_policy)
            .is_err()
    );
}

#[test]
fn fork_refuses_ambiguous_accepted_ancestors() {
    let temp = tempfile::tempdir().unwrap();
    let source = Repo::init(&temp.path().join("source")).unwrap();
    source.git(&["config", "commit.gpgsign", "false"]).unwrap();
    source.git(&["checkout", "-b", "work"]).unwrap();
    let original = append(&source, 1);
    source.git(&["branch", "other", &original]).unwrap();
    PrivacyPolicy::default().save(&source).unwrap();
    let identity = RemoteIdentity::new(
        "https://hub.invalid",
        "00000000-0000-0000-0000-000000000001",
    )
    .unwrap();
    let (_, key) = recipient(20);
    let redactor = Redactor::new(Default::default());
    let mut history = ProjectedHistory::prepare_with_rules(
        &source,
        &["work".into(), "other".into()],
        &key,
        "https://hub.invalid/alice/app.git",
        &redactor,
        &[],
        Some((&identity, &RemoteRefs::default())),
    )
    .unwrap();
    let accepted = remote(&history);
    assert_ne!(
        accepted.refs["refs/heads/work"],
        accepted.refs["refs/heads/other"]
    );
    history.prepare_acceptance().unwrap();
    drop(history);
    let mut ledger = accepted::Ledger::open(&source, &identity).unwrap();
    ledger.reconcile(&accepted).unwrap();
    assert!(
        ledger
            .inherited("work", &original, &PrivacyPolicy::default())
            .is_ok()
    );
    let error = ledger
        .inherited("continuation", &original, &PrivacyPolicy::default())
        .err()
        .unwrap();
    assert!(
        error
            .to_string()
            .contains("ambiguous accepted publications"),
        "{error:#}"
    );
    let base = crate::commands::fork::ForkBase {
        repo: source.clone(),
        slug: "alice/app".into(),
        resolved: crate::domain::refs::Resolved {
            branch: Some("work".into()),
            sha: original,
            turn: None,
            event_index: None,
            range: None,
            path: None,
        },
    };
    crate::commands::fork::fork_branch(&base, "alice/app@work", "continuation")
        .unwrap()
        .unwrap();
    assert!(
        ProjectedHistory::prepare_with_rules(
            &source,
            &["continuation".into()],
            &key,
            "https://hub.invalid/alice/app.git",
            &redactor,
            &[],
            Some((&identity, &accepted))
        )
        .err()
        .unwrap()
        .to_string()
        .contains("ambiguous accepted publications")
    );
}

#[test]
fn cloned_accepted_history_does_not_need_retired_key_lookup() {
    let temp = tempfile::tempdir().unwrap();
    let source = Repo::init(&temp.path().join("source")).unwrap();
    source.git(&["config", "commit.gpgsign", "false"]).unwrap();
    source.git(&["checkout", "-b", "work"]).unwrap();
    append(&source, 1);
    PrivacyPolicy::default().save(&source).unwrap();
    let identity = RemoteIdentity::new(
        "https://hub.invalid",
        "00000000-0000-0000-0000-000000000001",
    )
    .unwrap();
    let url = "https://hub.invalid/alice/app.git";
    let redactor = Redactor::new(Default::default());
    let (old_key, old) = recipient(20);
    let (new_key, new) = recipient(21);
    let first = ProjectedHistory::prepare_with_rules(
        &source,
        &["work".into()],
        &old,
        url,
        &redactor,
        &[],
        Some((&identity, &RemoteRefs::default())),
    )
    .unwrap();
    let first_public = first.plan.heads()[0].oid().to_owned();
    let first_remote = remote(&first);
    let original_envelope = first
        .repo
        .show_result(&first_public, "privacy/envelope.json")
        .unwrap()
        .unwrap();
    let cloned = Repo::init(&temp.path().join("clone")).unwrap();
    cloned
        .git(&[
            "fetch",
            first.repo.root().to_str().unwrap(),
            "refs/heads/work:refs/heads/work",
        ])
        .unwrap();
    cloned.git(&["checkout", "work"]).unwrap();
    PrivacyPolicy::default().save(&cloned).unwrap();
    cloned.git(&["config", "commit.gpgsign", "false"]).unwrap();
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let reader_hub = format!("http://{}", listener.local_addr().unwrap());
    let reader_identity = RemoteIdentity::new(&reader_hub, &identity.agent_id).unwrap();
    let reader_url = format!("{reader_hub}/alice/app.git");
    let server = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    let mut bytes = [0u8; 8192];
                    stream
                        .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                        .unwrap();
                    let count = stream.read(&mut bytes).unwrap();
                    let request = String::from_utf8_lossy(&bytes[..count]).into_owned();
                    let body =
                        r#"{"kind":"not_found","error":"publication viewing key not found"}"#;
                    write!(stream, "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
                    return Some(request);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() >= deadline {
                        return None;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(error) => panic!("{error}"),
            }
        }
    });
    let result = ProjectedHistory::prepare_with_rules(
        &cloned,
        &["work".into()],
        &new,
        &reader_url,
        &redactor,
        &[],
        Some((&reader_identity, &first_remote)),
    );
    let mut prepared = result.unwrap();
    assert_eq!(prepared.plan.heads()[0].oid(), first_public);
    assert!(prepared.receipt_binding("work").unwrap().is_none());
    assert_eq!(prepared.preparations, 0);
    prepared.prepare_acceptance().unwrap();
    drop(prepared);
    let new_source = append(&cloned, 2);
    let mut continued = ProjectedHistory::prepare_with_rules(
        &cloned,
        &["work".into()],
        &new,
        &reader_url,
        &redactor,
        &[],
        Some((&reader_identity, &first_remote)),
    )
    .unwrap();
    assert_eq!(continued.preparations, 1);
    let head = continued.plan.heads()[0].oid().to_owned();
    assert_eq!(
        continued
            .repo
            .git(&["rev-parse", &format!("{head}^")])
            .unwrap(),
        first_public
    );
    assert_eq!(
        continued
            .repo
            .show_result(&first_public, "privacy/envelope.json")
            .unwrap()
            .unwrap(),
        original_envelope
    );
    assert_eq!(
        continued.receipt_binding("work").unwrap().unwrap().1,
        new.fingerprint().unwrap()
    );
    let envelope = PrivacyEnvelope::parse(
        continued
            .repo
            .show_result(&head, "privacy/envelope.json")
            .unwrap()
            .unwrap()
            .as_bytes(),
    )
    .unwrap();
    assert!(envelope.open_layer(&new_key).is_ok());
    assert!(envelope.open_layer(&old_key).is_err());
    let observed = remote(&continued);
    continued.prepare_acceptance().unwrap();
    drop(continued);
    let repeated = ProjectedHistory::prepare_with_rules(
        &cloned,
        &["work".into()],
        &new,
        &reader_url,
        &redactor,
        &[],
        Some((&reader_identity, &observed)),
    )
    .unwrap();
    assert_eq!(repeated.plan.heads()[0].oid(), head);
    assert_eq!(repeated.preparations, 0);
    assert!(
        accepted::Ledger::open(&cloned, &reader_identity)
            .unwrap()
            .get("work", &new_source)
            .is_some()
    );
    assert!(
        server.join().unwrap().is_none(),
        "publishing must not request reader-key material"
    );
}
