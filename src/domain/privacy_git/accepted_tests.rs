use super::*;
use crate::domain::{storage, transcript};
use crate::hub::{git::RemoteRefs, identity::RemoteIdentity};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use crypto_box::SecretKey;

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

/// Identity mappings retain public Git bytes without requiring a separate projection cache.
#[test]
fn a_new_fork_reuses_accepted_public_ancestors_without_their_projection_cache() {
    use super::sessions::SessionPublicationPlan;
    use crate::domain::session_privacy::{AccountIdentity, SessionPrivacy, SessionPrivacyConfig};
    use crate::hub::privacy::sessions::SessionPrivacyResponse;
    let temp = tempfile::tempdir().unwrap();
    let source = Repo::init(&temp.path().join("collaborator")).unwrap();
    source
        .git(&["config", "user.name", "Collaborator"])
        .unwrap();
    source
        .git(&["config", "user.email", "collaborator@example.invalid"])
        .unwrap();
    source.git(&["config", "commit.gpgsign", "false"]).unwrap();
    source.git(&["checkout", "-b", "teammate"]).unwrap();
    let original_meta = Meta::new(
        format!("agit-{}", "a".repeat(40)),
        "codex".into(),
        String::new(),
    );
    meta::write(source.root(), &original_meta).unwrap();
    storage::write_snapshot(source.root(), "", "").unwrap();
    source.add_all().unwrap();
    source.commit("Published teammate snapshot").unwrap();
    let base = source.git(&["rev-parse", "HEAD"]).unwrap();
    let identity = RemoteIdentity::new(
        "https://hub.invalid",
        "00000000-0000-0000-0000-000000000001",
    )
    .unwrap();
    let mapping = accepted::Mapping {
        source: base.clone(),
        public: base.clone(),
        source_parents: vec![],
        public_parents: vec![],
        store: "c".repeat(64),
        source_session: original_meta.session.clone(),
        public_session: original_meta.session,
        policy: PrivacyPolicy::default().digest().unwrap(),
        recipient: None,
        plain: false,
    };
    accepted::Ledger::open(&source, &identity)
        .unwrap()
        .prepare(&BTreeMap::from([(
            "teammate".into(),
            accepted::Candidate {
                source_head: base.clone(),
                public_head: base.clone(),
                mappings: BTreeMap::from([(base.clone(), mapping)]),
            },
        )]))
        .unwrap();
    source.git(&["checkout", "-b", "fork"]).unwrap();
    let mut metadata = Meta::new(
        format!("agit-{}", "b".repeat(40)),
        "claude-code".into(),
        String::new(),
    );
    metadata.session_privacy = Some(
        SessionPrivacy::new(Some(
            AccountIdentity::new(&identity.hub, "collaborator").unwrap(),
        ))
        .unwrap(),
    );
    meta::write(source.root(), &metadata).unwrap();
    source.add_all().unwrap();
    source.commit("Cross-runtime fork").unwrap();
    let head = source.git(&["rev-parse", "HEAD"]).unwrap();
    let selected = SessionPublicationPlan {
        identity: identity.clone(),
        remote: RemoteRefs {
            heads: vec![base.clone()],
            refs: BTreeMap::from([("refs/heads/teammate".into(), base.clone())]),
            ..Default::default()
        },
        configs: BTreeMap::from([(
            metadata.session.clone(),
            SessionPrivacyResponse {
                config: SessionPrivacyConfig {
                    repository_id: identity.agent_id,
                    session_id: metadata.session,
                    privacy: metadata.session_privacy.unwrap(),
                    revision: 1,
                    created_at: "2026-01-01T00:00:00Z".into(),
                },
                owner_key: None,
            },
        )]),
    };
    accepted::Ledger::open(&source, &selected.identity)
        .unwrap()
        .reconcile(&selected.remote)
        .unwrap();
    assert_eq!(
        publication_envelope(&source, Some("teammate"), &base, &selected.identity).unwrap(),
        (base.clone(), None)
    );
    let mut published = ProjectedHistory::prepare_sessions_with_redactor(
        &source,
        &["fork".into()],
        &selected,
        &[],
        &Redactor::new(Default::default()),
    )
    .unwrap();
    assert_eq!(published.plan.heads()[0].oid(), head);
    assert_eq!(
        published
            .repo
            .git(&["rev-parse", &format!("{head}^")])
            .unwrap(),
        base
    );
    published.prepare_acceptance().unwrap();
}

/// A collaborator's new tip requires synchronization, including when its objects are not local.
#[test]
fn remote_advances_report_pull_without_discarding_local_commits() {
    let temp = tempfile::tempdir().unwrap();
    let source = Repo::init(&temp.path().join("source")).unwrap();
    source.git(&["config", "user.name", "Writer"]).unwrap();
    source
        .git(&["config", "user.email", "writer@example.invalid"])
        .unwrap();
    source.git(&["config", "commit.gpgsign", "false"]).unwrap();
    source.git(&["checkout", "-b", "wiki"]).unwrap();
    meta::write(source.root(), &Meta::new_file_line()).unwrap();
    source.add_all().unwrap();
    source.commit("Base").unwrap();
    let base = source.git(&["rev-parse", "HEAD"]).unwrap();
    source.git(&["checkout", "-b", "peer"]).unwrap();
    fs::write(source.root().join("peer.md"), "Peer page\n").unwrap();
    source.add_all().unwrap();
    source.commit("Peer page").unwrap();
    let peer = source.git(&["rev-parse", "HEAD"]).unwrap();
    source.git(&["checkout", "wiki"]).unwrap();
    let identity = RemoteIdentity::new(
        "https://hub.invalid",
        "00000000-0000-0000-0000-000000000001",
    )
    .unwrap();
    let ledger = accepted::Ledger::open(&source, &identity).unwrap();
    let check = |remote_head: &str| {
        let plan = PublicationPlan::freeze_selected(&source, &["wiki".into()]).unwrap();
        sessions::require_remote_ancestry(
            &source,
            &plan,
            &ledger,
            &RemoteRefs {
                refs: BTreeMap::from([("refs/heads/wiki".into(), remote_head.into())]),
                heads: vec![remote_head.into()],
                ..Default::default()
            },
            "team/wiki",
        )
    };
    assert!(check(&base).is_ok());
    let error = check(&peer).unwrap_err().to_string();
    assert!(error.contains("ahead of this checkout"), "{error}");
    fs::write(source.root().join("local.md"), "Local page\n").unwrap();
    source.add_all().unwrap();
    source.commit("Local page").unwrap();
    let local = source.git(&["rev-parse", "HEAD"]).unwrap();
    for (tip, diagnosis) in [(&peer, "has diverged"), (&"f".repeat(40), "has changed")] {
        let error = check(tip).unwrap_err().to_string();
        assert!(error.contains(diagnosis), "{error}");
        assert!(error.contains("agit pull team/wiki@wiki"), "{error}");
        assert!(
            error.contains("local unpublished commits are kept"),
            "{error}"
        );
        assert!(!error.contains(accepted::RECOVERY), "{error}");
    }
    assert_eq!(source.git(&["rev-parse", "wiki"]).unwrap(), local);
    assert_eq!(
        source.git(&["show", "wiki:local.md"]).unwrap(),
        "Local page"
    );
    // A checkout that has never published this branch has no private mapping state to restore.
    meta::write(
        source.root(),
        &Meta::new(
            "agit-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            "codex".into(),
            String::new(),
        ),
    )
    .unwrap();
    source.add_all().unwrap();
    source.commit("Session").unwrap();
    let error = check(&peer).unwrap_err().to_string();
    assert!(error.contains("agit pull team/wiki@wiki"), "{error}");
    assert!(!error.contains(accepted::RECOVERY), "{error}");
}

/// Mixed admission preserves accepted bytes and scopes each new ciphertext to its own session.
#[test]
fn session_owner_publication_retains_plain_ancestry_and_immutable_candidates() {
    use super::sessions::SessionPublicationPlan;
    use crate::domain::session_privacy::{
        AccountIdentity, OwnerResolution, PublishedSnapshot, SessionPrivacy, SessionPrivacyConfig,
        SessionScheme,
    };
    use crate::hub::privacy::sessions::{AccountSessionKey, SessionPrivacyResponse};

    let temp = tempfile::tempdir().unwrap();
    let source = Repo::init(&temp.path().join("source")).unwrap();
    source
        .git(&["config", "user.name", "Original Writer"])
        .unwrap();
    source
        .git(&["config", "user.email", "original@example.invalid"])
        .unwrap();
    source.git(&["config", "commit.gpgsign", "false"]).unwrap();
    let identity = RemoteIdentity::new(
        "https://hub.invalid",
        "00000000-0000-0000-0000-000000000001",
    )
    .unwrap();
    let logical = |letter: char| format!("agit-{}", letter.to_string().repeat(40));
    let account = |id: &str| AccountIdentity::new(&identity.hub, id).unwrap();
    let save = |metadata: &Meta, content: &str| {
        let mut text = transcript::wrap_lines(
            &format!(
                "{}\n",
                json!({"type":"user","message":{"role":"user","content":content}})
            ),
            "claude-code",
            &metadata.session,
        );
        if metadata.session_privacy.is_some() {
            text += &transcript::wrap_lines(
                &format!(
                    "{}\n{}\n",
                    json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"shell","name":"Bash","input":{"command":"private tool input"}}]}}),
                    json!({"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"shell","content":"private tool output"}]}}),
                ),
                "claude-code",
                &metadata.session,
            );
        }
        storage::write_snapshot(source.root(), &text, &text).unwrap();
        meta::write(source.root(), metadata).unwrap();
        source.add_all().unwrap();
        let saved = crate::infra::git_runtime::command()
            .arg("-C")
            .arg(source.root())
            .args(["commit", "-m", "Source snapshot"])
            .env("GIT_AUTHOR_DATE", "2020-01-01T00:00:00Z")
            .env("GIT_COMMITTER_DATE", "2020-01-01T00:00:00Z")
            .output()
            .unwrap();
        assert!(saved.status.success(), "{saved:?}");
        source.git(&["rev-parse", "HEAD"]).unwrap()
    };
    source.git(&["checkout", "-b", "legacy"]).unwrap();
    let legacy = Meta::new(logical('a'), "claude-code".into(), String::new());
    fs::write(source.root().join("README.md"), "Accepted plain file\n").unwrap();
    let base = save(&legacy, "Accepted plain conversation");
    let mut selected = SessionPublicationPlan {
        identity: identity.clone(),
        remote: RemoteRefs {
            heads: vec![base.clone()],
            refs: BTreeMap::from([("refs/heads/legacy".into(), base.clone())]),
            ..Default::default()
        },
        configs: BTreeMap::new(),
    };
    let make_config = |metadata: &Meta, key: Option<AccountSessionKey>| SessionPrivacyResponse {
        config: SessionPrivacyConfig {
            repository_id: identity.agent_id.clone(),
            session_id: metadata.session.clone(),
            privacy: metadata.session_privacy.clone().unwrap_or(SessionPrivacy {
                scheme: SessionScheme::Plain,
                hub: None,
                creator: None,
                owner: None,
                owner_source: OwnerResolution::Unresolved,
            }),
            revision: 1,
            created_at: "2026-01-01T00:00:00Z".into(),
        },
        owner_key: key,
    };
    selected
        .configs
        .insert(legacy.session.clone(), make_config(&legacy, None));
    use sha2::Digest;
    let payload = b"Plain file payload\n";
    let lfs_oid = hex::encode(sha2::Sha256::digest(payload));
    let lfs_path = source
        .common_dir()
        .unwrap()
        .join("lfs/objects")
        .join(&lfs_oid[..2])
        .join(&lfs_oid[2..4])
        .join(&lfs_oid);
    fs::create_dir_all(lfs_path.parent().unwrap()).unwrap();
    fs::write(&lfs_path, payload).unwrap();
    fs::write(source.root().join("binary.dat"), b"\0binary bytes").unwrap();
    fs::write(
        source.root().join("plain-lfs.bin"),
        format!(
            "version https://git-lfs.github.com/spec/v1\noid sha256:{lfs_oid}\nsize {}\n",
            payload.len()
        ),
    )
    .unwrap();
    fs::write(
        source.root().join("README.md"),
        "Accepted plain file\nAppended plain line\n",
    )
    .unwrap();
    fs::write(source.root().join("first-copy.txt"), "Shared plain text\n").unwrap();
    fs::write(source.root().join("second-copy.txt"), "Shared plain text\n").unwrap();
    let plain = save(&legacy, "New plain conversation");
    source.git(&["checkout", "-b", "work", &base]).unwrap();
    let mut unclaimed = Meta::new_session_line("claude-code".into(), String::new());
    unclaimed.committer_account = Some(account("owner-alice"));
    storage::write_snapshot(source.root(), "", "").unwrap();
    meta::write(source.root(), &unclaimed).unwrap();
    source.add_all().unwrap();
    source.commit("Unclaimed session birth").unwrap();
    let birth = source.git(&["rev-parse", "HEAD"]).unwrap();
    let mut owned = Meta::new(logical('b'), "claude-code".into(), String::new());
    owned.session_privacy = Some(SessionPrivacy {
        scheme: crate::domain::session_privacy::SessionScheme::SessionOwnerV2,
        ..SessionPrivacy::new(Some(account("owner-alice"))).unwrap()
    });
    owned.committer_account = Some(account("writer-bob"));
    fs::write(
        source.root().join("private-original.txt"),
        "Unpublished original file\n",
    )
    .unwrap();
    // Activity keeps conversation text while discarding standalone publication notices.
    let original = save(
        &owned,
        "[Content omitted by privacy policy]\n[Image omitted by policy]\n[Attachment body omitted; metadata retained: {}]\nPrivate session Alice",
    );
    source.git(&["branch", "alias", &original]).unwrap();
    let key_a = SecretKey::from([41; 32]);
    let session_key = |id: &str, key: &SecretKey, version| AccountSessionKey {
        account_id: id.into(),
        key_version: version,
        public_key_algorithm: "x25519".into(),
        public_key: STANDARD.encode(key.public_key().as_bytes()),
    };
    selected.configs.insert(
        owned.session.clone(),
        make_config(&owned, Some(session_key("owner-alice", &key_a, 1))),
    );
    source.git(&["checkout", "-b", "other", &base]).unwrap();
    let mut other = Meta::new(logical('c'), "claude-code".into(), String::new());
    other.session_privacy = Some(SessionPrivacy {
        scheme: crate::domain::session_privacy::SessionScheme::SessionOwnerV2,
        ..SessionPrivacy::new(Some(account("owner-carol"))).unwrap()
    });
    other.committer_account = Some(account("owner-carol"));
    let other_source = save(&other, "Private session Carol");
    let key_c = SecretKey::from([42; 32]);
    selected.configs.insert(
        other.session.clone(),
        make_config(&other, Some(session_key("owner-carol", &key_c, 1))),
    );
    let branches = vec![
        "work".into(),
        "legacy".into(),
        "other".into(),
        "alias".into(),
    ];
    let redactor = Redactor::new(Default::default());
    let prepare = |branches: &[String], selected: &SessionPublicationPlan| {
        ProjectedHistory::prepare_sessions_with_redactor(
            &source,
            branches,
            selected,
            &[],
            &redactor,
        )
    };
    source
        .git(&["checkout", "-b", "ambiguous", &birth])
        .unwrap();
    let mut conflicting = other.clone();
    conflicting.session = logical('d');
    save(&conflicting, "Another claim of the same empty birth");
    selected.configs.insert(
        conflicting.session.clone(),
        make_config(&conflicting, Some(session_key("owner-carol", &key_c, 1))),
    );
    let mut ambiguous = branches.clone();
    ambiguous.push("ambiguous".into());
    let error = prepare(&ambiguous, &selected).err().unwrap();
    assert!(
        format!("{error:#}").contains("unambiguous descendant"),
        "{error:#}"
    );
    let first = prepare(&branches, &selected).unwrap();
    first.session_manifest().validate().unwrap();
    assert!(
        first.session_manifest().snapshots.iter().any(
            |entry| matches!(entry, PublishedSnapshot::Plain { commit, .. } if commit == &plain)
        )
    );
    assert_eq!(
        first.repo.git(&["cat-file", "commit", &base]).unwrap(),
        source.git(&["cat-file", "commit", &base]).unwrap()
    );
    assert_eq!(
        first.repo.git(&["rev-parse", "refs/heads/legacy"]).unwrap(),
        plain
    );
    let published_a = first.repo.git(&["rev-parse", "refs/heads/work"]).unwrap();
    assert_eq!(
        first.repo.git(&["rev-parse", "refs/heads/alias"]).unwrap(),
        published_a
    );
    let published_c = first.repo.git(&["rev-parse", "refs/heads/other"]).unwrap();
    let envelope = |repo: &Repo, commit: &str| {
        PrivacyEnvelope::parse(
            repo.show_result(commit, "privacy/envelope.json")
                .unwrap()
                .unwrap()
                .as_bytes(),
        )
        .unwrap()
    };
    crate::domain::activity::tests::assert_encrypted_publication_activity(
        first.repo(),
        first.session_manifest(),
    );
    let sealed_a = envelope(&first.repo, &published_a);
    let sealed_c = envelope(&first.repo, &published_c);
    let public_session = sealed_a
        .session_binding
        .as_ref()
        .unwrap()
        .published_session_id
        .clone();
    assert_eq!(
        sealed_a.open_layer(&key_a).unwrap().metadata["session_privacy"],
        serde_json::to_value(&owned.session_privacy).unwrap()
    );
    assert!(sealed_a.open_layer(&key_c).is_err());
    assert!(sealed_c.open_layer(&key_c).is_ok());
    assert_eq!(
        sealed_a.session_binding.as_ref().unwrap().committer,
        crate::domain::session_privacy::CommitterAttribution::Account {
            account_id: "writer-bob".into()
        }
    );
    for original in [&original, &other_source, &birth] {
        assert!(first.repo.git(&["cat-file", "-e", original]).is_err());
    }
    assert!(
        first
            .repo
            .show_result(&published_a, "private-original.txt")
            .unwrap()
            .is_none()
    );
    assert!(
        first
            .repo
            .show_result(&published_a, "README.md")
            .unwrap()
            .is_none()
    );
    assert_eq!(
        first
            .repo
            .git(&["rev-parse", &format!("{published_a}^^")])
            .unwrap(),
        base
    );
    first.public_digest().unwrap();
    let captured =
        crate::hub::git::CapturedPublication::capture_projected(&first, 1024 * 1024, &source)
            .unwrap();
    assert_eq!(captured.pointers().len(), 1);
    assert_eq!(captured.pointers()[0].oid, lfs_oid);
    let mut retained = Vec::new();
    use std::io::Read;
    captured
        .open_payload(&captured.pointers()[0])
        .unwrap()
        .read_to_end(&mut retained)
        .unwrap();
    assert_eq!(retained, payload);
    fs::write(&lfs_path, b"Unreviewed live cache replacement").unwrap();
    let preview = first
        .write_preview(
            &json!({"repository_id":identity.agent_id}),
            &source,
            &captured,
        )
        .unwrap();
    let preview: serde_json::Value = serde_json::from_slice(&fs::read(preview).unwrap()).unwrap();
    let files = preview["files"].as_array().unwrap();
    let lfs = files
        .iter()
        .find(|file| file["path"] == "plain-lfs.bin")
        .unwrap();
    assert_eq!(lfs["text"], std::str::from_utf8(payload).unwrap());
    assert_eq!(lfs["lfs"]["oid"], lfs_oid);
    assert_eq!(lfs["commit"], plain);
    let binary = files
        .iter()
        .find(|file| file["path"] == "binary.dat")
        .unwrap();
    assert_eq!(binary["binary_bytes"], b"\0binary bytes".len());
    assert!(binary.get("text").is_none());
    // An append-only file shows the bytes it adds to its accepted version, and a blob that two
    // paths introduce is shown once; a preview that dropped either path would hide a publication.
    let file = |path: &str| files.iter().find(|file| file["path"] == path).unwrap();
    assert_eq!(file("README.md")["text"], "Appended plain line\n");
    assert!(file("README.md")["appended_to"].is_string());
    let copies = [file("first-copy.txt"), file("second-copy.txt")];
    assert_eq!(copies[0]["blob"], copies[1]["blob"]);
    assert_eq!(
        copies
            .iter()
            .filter(|file| file["text"] == "Shared plain text\n")
            .count(),
        1
    );
    assert_eq!(
        copies
            .iter()
            .filter(|file| file["repeated"] == true)
            .count(),
        1
    );
    assert!(
        !preview
            .to_string()
            .contains("Unreviewed live cache replacement")
    );
    assert!(!preview.to_string().contains("Unpublished original file"));
    fs::write(&lfs_path, payload).unwrap();
    let first_plan = first.plan.clone();
    let store = first.repo.root().to_owned();
    drop(first);

    let removed = selected.configs.remove(&owned.session).unwrap();
    assert!(
        prepare(&branches, &selected)
            .err()
            .unwrap()
            .to_string()
            .contains("registered privacy configuration")
    );
    selected.configs.insert(owned.session.clone(), removed);
    assert_eq!(
        Repo::at(&store)
            .git(&["rev-parse", "refs/heads/work"])
            .unwrap(),
        published_a
    );
    let rotated = SecretKey::from([43; 32]);
    selected.configs.get_mut(&owned.session).unwrap().owner_key =
        Some(session_key("owner-alice", &rotated, 2));
    let mut retry = prepare(&branches, &selected).unwrap();
    assert_eq!(retry.plan, first_plan);
    assert_eq!(retry.preparations, 0);
    retry.prepare_acceptance().unwrap();
    selected.remote = remote(&retry);
    let receipt = retry
        .session_receipts("team/app", &identity, "https://hub.invalid/team/app.git")
        .unwrap()
        .into_iter()
        .find(|receipt| receipt.branch == "work")
        .unwrap();
    drop(retry);
    assert_eq!(
        receipt.mode,
        crate::domain::privacy_receipt::PublicationMode::SessionOwnerV2
    );
    receipt.save(&source).unwrap();
    assert_eq!(
        crate::domain::privacy_receipt::PublicationReceipt::load(&source, "work")
            .unwrap()
            .as_ref(),
        Some(&receipt)
    );
    assert_eq!(
        serde_json::to_value(&receipt).unwrap()["mode"],
        "session_owner_v2"
    );
    assert_eq!(receipt.session_id(&source).unwrap(), public_session);
    assert!(receipt.ancestors(&source).unwrap().contains(&base));
    let mut altered = receipt.clone();
    altered.session_snapshot.as_mut().unwrap().envelope_digest =
        digest_json(&json!("different envelope")).unwrap();
    assert!(altered.session_id(&source).is_err());
    altered = receipt.clone();
    altered.source = "f".repeat(40);
    assert!(altered.validate().is_err());
    source.git(&["checkout", "work"]).unwrap();
    save(&owned, "Private continuation by Bob");
    let mut ledger = accepted::Ledger::open(&source, &identity).unwrap();
    ledger.reconcile(&selected.remote).unwrap();
    let plan = PublicationPlan::freeze_selected(&source, &branches).unwrap();
    sessions::require_remote_ancestry(&source, &plan, &ledger, &selected.remote, "team/app")
        .unwrap();
    let mut advanced = selected.remote.clone();
    advanced
        .refs
        .insert("refs/heads/work".into(), "f".repeat(40));
    let error = sessions::require_remote_ancestry(&source, &plan, &ledger, &advanced, "team/app")
        .unwrap_err();
    assert!(
        error.to_string().contains("agit pull team/app@work"),
        "{error:#}"
    );
    let mut next = prepare(&branches, &selected).unwrap();
    let second_public = next.repo.git(&["rev-parse", "refs/heads/work"]).unwrap();
    assert_eq!(
        next.repo
            .git(&["rev-parse", &format!("{second_public}^")])
            .unwrap(),
        published_a
    );
    let sealed = envelope(&next.repo, &second_public);
    assert_eq!(
        sealed
            .session_binding
            .as_ref()
            .unwrap()
            .owner_key
            .key_version,
        2
    );
    assert_eq!(
        sealed
            .session_binding
            .as_ref()
            .unwrap()
            .published_session_id,
        public_session
    );
    assert!(sealed.open_layer(&rotated).is_ok());
    assert!(
        envelope(&next.repo, &published_a)
            .open_layer(&key_a)
            .is_ok()
    );
    next.prepare_acceptance().unwrap();
    selected.remote = remote(&next);
    drop(next);
    assert_eq!(receipt.session_id(&source).unwrap(), public_session);
    assert_eq!(receipt.published, published_a);
    source.git(&["branch", "-m", "work", "renamed"]).unwrap();
    let renamed = vec!["renamed".into()];
    let same = prepare(&renamed, &selected).unwrap();
    assert_eq!(
        same.repo.git(&["rev-parse", "refs/heads/renamed"]).unwrap(),
        second_public
    );
    drop(same);
    fs::remove_dir_all(store.join(".git/session-candidates")).unwrap();
    save(&owned, "Continuation after local candidate cache loss");
    let restored = prepare(&renamed, &selected).unwrap();
    let third = restored
        .repo
        .git(&["rev-parse", "refs/heads/renamed"])
        .unwrap();
    assert_eq!(
        envelope(&restored.repo, &third)
            .session_binding
            .unwrap()
            .published_session_id,
        public_session
    );
    drop(restored);
    source.git(&["checkout", "-b", "plain-child"]).unwrap();
    save(
        &legacy,
        "Plain child cannot expose encrypted original ancestry",
    );
    assert!(
        prepare(&["plain-child".into()], &selected)
            .err()
            .unwrap()
            .to_string()
            .contains("unaccepted original ancestry")
    );
}

/// Preview caches can be regenerated, but accepted anonymous ancestors and their receipts are immutable.
#[test]
fn metadata_upgrade_refreshes_previews_and_preserves_accepted_anonymous_history() {
    use super::sessions::SessionPublicationPlan;
    use crate::domain::session_privacy::{AccountIdentity, SessionPrivacy, SessionPrivacyConfig};
    use crate::hub::privacy::sessions::{AccountSessionKey, SessionPrivacyResponse};

    let temp = tempfile::tempdir().unwrap();
    let source = Repo::init(&temp.path().join("source")).unwrap();
    source
        .git(&["config", "user.name", "Original Writer"])
        .unwrap();
    source
        .git(&["config", "user.email", "writer@example.invalid"])
        .unwrap();
    source.git(&["checkout", "-b", "work"]).unwrap();
    let identity = RemoteIdentity::new(
        "https://hub.invalid",
        "00000000-0000-0000-0000-000000000001",
    )
    .unwrap();
    let account = AccountIdentity::new(&identity.hub, "owner").unwrap();
    let mut metadata = Meta::new(
        format!("agit-{}", "a".repeat(40)),
        "claude-code".into(),
        String::new(),
    );
    metadata.session_privacy = Some(SessionPrivacy {
        scheme: crate::domain::session_privacy::SessionScheme::SessionOwnerV2,
        ..SessionPrivacy::new(Some(account.clone())).unwrap()
    });
    metadata.committer_account = Some(account);
    let save = |content: &str| {
        let text = transcript::wrap_lines(
            &format!(
                "{}\n",
                json!({"type":"user","message":{"role":"user","content":content}})
            ),
            "claude-code",
            &metadata.session,
        );
        storage::write_snapshot(source.root(), &text, &text).unwrap();
        meta::write(source.root(), &metadata).unwrap();
        source.add_all().unwrap();
        source.commit(content).unwrap();
        source.git(&["rev-parse", "HEAD"]).unwrap()
    };
    let original = save("Original session snapshot");
    let key = SecretKey::from([45; 32]);
    let mut selected = SessionPublicationPlan {
        identity: identity.clone(),
        remote: RemoteRefs::default(),
        configs: BTreeMap::from([(
            metadata.session.clone(),
            SessionPrivacyResponse {
                config: SessionPrivacyConfig {
                    repository_id: identity.agent_id.clone(),
                    session_id: metadata.session.clone(),
                    privacy: metadata.session_privacy.clone().unwrap(),
                    revision: 1,
                    created_at: "2026-01-01T00:00:00Z".into(),
                },
                owner_key: Some(AccountSessionKey {
                    account_id: "owner".into(),
                    key_version: 1,
                    public_key_algorithm: "x25519".into(),
                    public_key: STANDARD.encode(key.public_key().as_bytes()),
                }),
            },
        )]),
    };
    let prepare = |selected: &SessionPublicationPlan| {
        ProjectedHistory::prepare_sessions_with_redactor(
            &source,
            &["work".into()],
            selected,
            &[],
            &Redactor::new(Default::default()),
        )
        .unwrap()
    };
    let first = prepare(&selected);
    let head = first.plan.heads()[0].oid();
    let tree = first
        .repo
        .git(&["rev-parse", &format!("{head}^{{tree}}")])
        .unwrap();
    let anonymous_bytes = format!("tree {tree}\n{LEGACY_COMMIT_SUFFIX}");
    let anonymous = write_object(&first.repo, "commit", anonymous_bytes.as_bytes()).unwrap();
    let scope =
        digest_json(&json!({"version":2, "kind":"session-owner", "identity":identity})).unwrap();
    let cache_path = first.repo.root().join(".git/session-candidates");
    let mut cache: Cache =
        serde_json::from_slice(&state::load(&cache_path, &scope).unwrap().unwrap()).unwrap();
    cache.version = 2;
    for commit in cache.commits.values_mut() {
        *commit = anonymous.clone();
    }
    state::save(&cache_path, &scope, &serde_json::to_vec(&cache).unwrap()).unwrap();
    let mut candidate = first.candidates["work"].clone();
    candidate.public_head = anonymous.clone();
    candidate.mappings.get_mut(&original).unwrap().public = anonymous.clone();
    let mut receipt = first
        .session_receipts("owner/app", &identity, "https://hub.invalid/owner/app.git")
        .unwrap()
        .pop()
        .unwrap();
    receipt.published = anonymous.clone();
    receipt.session_snapshot.as_mut().unwrap().published_commit = anonymous.clone();
    let public_session = receipt.projected_session_id.clone().unwrap();
    drop(first);

    let refreshed = prepare(&selected);
    let refreshed_head = refreshed.plan.heads()[0].oid();
    assert_ne!(refreshed_head, anonymous);
    let attribution = "--format=%an%n%ae%n%aI%n%cn%n%ce%n%cI%n%B";
    assert_eq!(
        refreshed
            .repo
            .git_bytes_result(&["show", "--no-patch", attribution, refreshed_head])
            .unwrap(),
        source
            .git_bytes_result(&["show", "--no-patch", attribution, &original])
            .unwrap()
    );
    let refreshed_receipt = refreshed
        .session_receipts("owner/app", &identity, "https://hub.invalid/owner/app.git")
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(
        refreshed_receipt.projected_session_id.as_deref(),
        Some(public_session.as_str())
    );
    drop(refreshed);

    accepted::Ledger::open(&source, &identity)
        .unwrap()
        .prepare(&BTreeMap::from([("work".into(), candidate)]))
        .unwrap();
    selected.remote = RemoteRefs {
        heads: vec![anonymous.clone()],
        refs: BTreeMap::from([("refs/heads/work".into(), anonymous.clone())]),
        ..Default::default()
    };
    let retained = prepare(&selected);
    assert_eq!(retained.plan.heads()[0].oid(), anonymous);
    assert!(retained.session_manifest().snapshots.is_empty());
    crate::domain::activity::tests::assert_unattributed_projection(retained.repo(), &anonymous);
    drop(retained);
    assert_eq!(receipt.session_id(&source).unwrap(), public_session);

    source.git(&["config", "user.name", "Next Writer"]).unwrap();
    let continued = save("Continue accepted anonymous history");
    let next = prepare(&selected);
    let head = next.plan.heads()[0].oid();
    assert_eq!(
        next.repo.git(&["rev-parse", &format!("{head}^")]).unwrap(),
        anonymous
    );
    assert_eq!(
        next.repo
            .git_bytes_result(&["show", "--no-patch", attribution, head])
            .unwrap(),
        source
            .git_bytes_result(&["show", "--no-patch", attribution, &continued])
            .unwrap()
    );
    assert_eq!(
        next.repo
            .git_bytes_result(&["cat-file", "commit", &anonymous])
            .unwrap(),
        anonymous_bytes.as_bytes()
    );
}

/// The original publisher retains opaque accepted ciphertext without a legacy decoder or key.
#[test]
fn legacy_accepted_mapping_preserves_public_objects_when_original_history_continues() {
    use super::sessions::SessionPublicationPlan;
    use crate::domain::session_privacy::{AccountIdentity, SessionPrivacy, SessionPrivacyConfig};
    use crate::hub::privacy::sessions::{AccountSessionKey, SessionPrivacyResponse};

    let temp = tempfile::tempdir().unwrap();
    let source = Repo::init(&temp.path().join("source")).unwrap();
    source
        .git(&["config", "user.name", "Original Writer"])
        .unwrap();
    source
        .git(&["config", "user.email", "writer@example.invalid"])
        .unwrap();
    source.git(&["config", "commit.gpgsign", "false"]).unwrap();
    source.git(&["checkout", "-b", "work"]).unwrap();
    let metadata = Meta::new(
        format!("agit-{}", "a".repeat(40)),
        "claude-code".into(),
        String::new(),
    );
    let save = |content: &str| {
        let text = transcript::wrap_lines(
            &format!(
                "{}\n",
                json!({"type":"user","message":{"role":"user","content":content}})
            ),
            "claude-code",
            &metadata.session,
        );
        storage::write_snapshot(source.root(), &text, &text).unwrap();
        meta::write(source.root(), &metadata).unwrap();
        source.add_all().unwrap();
        source.commit("Original snapshot").unwrap();
        source.git(&["rev-parse", "HEAD"]).unwrap()
    };
    let original = save("Private original conversation");
    let identity = RemoteIdentity::new(
        "https://hub.invalid",
        "00000000-0000-0000-0000-000000000001",
    )
    .unwrap();
    let store = "c".repeat(64);
    let parent = source
        .common_dir()
        .unwrap()
        .join("agit/privacy-publication");
    fs::create_dir_all(&parent).unwrap();
    let retained_root = parent.join(&store);
    initialize_with_format(&retained_root, 40).unwrap();
    let retained = Repo::at(retained_root).local_objects_only();
    let legacy = include_bytes!("../../../tests/fixtures/privacy-accepted-v1-envelope.json");
    assert!(PrivacyEnvelope::parse(legacy).is_err());
    let public_metadata = Meta::new(
        format!("agit-{}", "b".repeat(40)),
        "claude-code".into(),
        String::new(),
    );
    let (_, metadata_text) =
        crate::domain::privacy_metadata::git_metadata(&public_metadata, None).unwrap();
    let mut files = storage::snapshot_files("", "").unwrap();
    files.insert(meta::FILE.into(), metadata_text.into_bytes());
    files.insert("privacy/envelope.json".into(), legacy.to_vec());
    let tree = write_tree(&retained, files).unwrap();
    let public_bytes = format!("tree {tree}\n{LEGACY_COMMIT_SUFFIX}");
    let public = write_object(&retained, "commit", public_bytes.as_bytes()).unwrap();
    let policy = serde_json::from_slice::<serde_json::Value>(legacy).unwrap()["policy_digest"]
        .as_str()
        .unwrap()
        .to_owned();
    let mapping = accepted::Mapping {
        plain: false,
        source: original.clone(),
        public: public.clone(),
        source_parents: vec![],
        public_parents: vec![],
        store,
        source_session: metadata.session.clone(),
        public_session: public_metadata.session,
        policy,
        recipient: Some(crate::domain::privacy_envelope::digest_bytes(
            b"legacy recipient",
        )),
    };
    let mut ledger = accepted::Ledger::open(&source, &identity).unwrap();
    ledger
        .prepare(&BTreeMap::from([(
            "work".into(),
            accepted::Candidate {
                source_head: original.clone(),
                public_head: public.clone(),
                mappings: BTreeMap::from([(original.clone(), mapping)]),
            },
        )]))
        .unwrap();
    let mut selected = SessionPublicationPlan {
        identity: identity.clone(),
        remote: RemoteRefs {
            heads: vec![public.clone()],
            refs: BTreeMap::from([("refs/heads/work".into(), public.clone())]),
            ..Default::default()
        },
        configs: BTreeMap::new(),
    };
    let prepare = |selected: &SessionPublicationPlan| {
        ProjectedHistory::prepare_sessions_with_redactor(
            &source,
            &["work".into()],
            selected,
            &[],
            &Redactor::new(Default::default()),
        )
    };
    let same = prepare(&selected).unwrap();
    assert_eq!(same.plan.heads()[0].oid(), public);
    assert!(same.session_manifest().snapshots.is_empty());
    assert_eq!(
        same.session_manifest().preserved_commits,
        std::slice::from_ref(&public)
    );
    assert!(
        same.session_receipts("team/app", &identity, "https://hub.invalid/team/app.git")
            .unwrap()
            .is_empty()
    );
    assert!(
        published::retain_accepted(&retained, same.repo(), &public, &["f".repeat(40)]).is_err()
    );
    drop(same);

    let unavailable = retained.root().with_extension("unavailable");
    fs::rename(retained.root(), &unavailable).unwrap();
    initialize_with_format(retained.root(), 40).unwrap();
    let error = prepare(&selected).err().unwrap();
    let missing = error.downcast_ref::<MissingAcceptedObjects>().unwrap();
    assert_eq!(missing.branch, "work");
    assert_eq!(missing.published, public);
    crate::commands::plumbing::import_commit_graph(&source, &Repo::at(&unavailable), &public)
        .unwrap();
    let fetched = prepare(&selected).unwrap();
    assert_eq!(fetched.plan.heads()[0].oid(), public);
    assert_eq!(
        read(fetched.repo(), &["cat-file", "commit", &public], MAX_COMMIT).unwrap(),
        public_bytes.as_bytes()
    );
    drop(fetched);
    fs::remove_dir_all(retained.root()).unwrap();
    fs::rename(&unavailable, retained.root()).unwrap();

    let key = SecretKey::from([44; 32]);
    selected.configs.insert(
        metadata.session.clone(),
        SessionPrivacyResponse {
            config: SessionPrivacyConfig {
                repository_id: identity.agent_id.clone(),
                session_id: metadata.session.clone(),
                privacy: SessionPrivacy {
                    scheme: crate::domain::session_privacy::SessionScheme::SessionOwnerV2,
                    ..SessionPrivacy::new(Some(
                        AccountIdentity::new(&identity.hub, "owner").unwrap(),
                    ))
                    .unwrap()
                },
                revision: 1,
                created_at: "2026-01-01T00:00:00Z".into(),
            },
            owner_key: Some(AccountSessionKey {
                account_id: "owner".into(),
                key_version: 1,
                public_key_algorithm: "x25519".into(),
                public_key: STANDARD.encode(key.public_key().as_bytes()),
            }),
        },
    );
    let continuation = save("Private continuation under session ownership");
    let next = prepare(&selected).unwrap();
    let head = next.plan.heads()[0].oid();
    assert_eq!(
        next.repo.git(&["rev-parse", &format!("{head}^")]).unwrap(),
        public
    );
    assert_eq!(
        next.repo
            .git_bytes_result(&["cat-file", "commit", &public])
            .unwrap(),
        public_bytes.as_bytes()
    );
    assert_eq!(
        next.repo
            .git_bytes_result(&[
                "cat-file",
                "blob",
                &format!("{public}:privacy/envelope.json")
            ])
            .unwrap(),
        legacy
    );
    assert!(next.repo.git(&["cat-file", "-e", &original]).is_err());
    let receipts = next
        .session_receipts("team/app", &identity, "https://hub.invalid/team/app.git")
        .unwrap();
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].source, continuation);
    assert!(next.session_manifest().preserved_commits.contains(&public));
    crate::hub::git::CapturedPublication::capture_projected(&next, 1024 * 1024, &source).unwrap();
}
