use super::*;
use crate::domain::{
    meta::{self, Meta},
    privacy::{PrivacyPolicy, ReplacementRule},
    privacy_envelope::PrivacyEnvelope,
    privacy_git::{ProjectedHistory, sessions::SessionPublicationPlan},
    session_privacy::{AccountIdentity, SessionPrivacy, SessionPrivacyConfig},
    storage, transcript,
};
use crate::hub::privacy::sessions::{AccountSessionKey, SessionPrivacyResponse};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use std::collections::BTreeMap;

pub(super) fn publication(
    repo: &Repo,
    branch: &str,
    destination: crate::hub::identity::RemoteIdentity,
    received: &std::path::Path,
    session_encrypted: bool,
) -> (PublicationReceipt, Repo) {
    repo.git(&["config", "user.name", "Fixture"]).unwrap();
    repo.git(&["config", "user.email", "fixture@example.invalid"])
        .unwrap();
    repo.git(&["checkout", "-b", branch]).unwrap();
    let private_text = "Unshared session note";
    let raw = format!(
        "{}\n",
        json!({"type":"response_item", "payload":{
            "type":"message", "role":"user", "content":[{"type":"input_text", "text":private_text}]
        }})
    );
    let session = format!("agit-{}", "a".repeat(40));
    let log = transcript::wrap_lines(&raw, "codex", &session);
    storage::write_snapshot(repo.root(), &log, &log).unwrap();
    let mut metadata = Meta::new(session.clone(), "codex".into(), String::new());
    if session_encrypted {
        let account = AccountIdentity::new(&destination.hub, "account-1").unwrap();
        metadata.session_privacy = Some(SessionPrivacy {
            scheme: crate::domain::session_privacy::SessionScheme::SessionOwnerV2,
            ..SessionPrivacy::new(Some(account.clone())).unwrap()
        });
        metadata.committer_account = Some(account);
    }
    meta::write(repo.root(), &metadata).unwrap();
    repo.git(&["add", "."]).unwrap();
    repo.git(&["commit", "-m", "Source session"]).unwrap();
    let source = repo.git(&["rev-parse", "HEAD"]).unwrap();
    if !session_encrypted {
        std::fs::create_dir_all(received).unwrap();
        let remote = Repo::at(received);
        remote.git(&["init", "--bare"]).unwrap();
        repo.git(&[
            "push",
            received.to_str().unwrap(),
            &format!("{source}:refs/heads/{branch}"),
        ])
        .unwrap();
        let receipt = PublicationReceipt {
            session_snapshot: None,
            version: 2,
            mode: crate::domain::privacy_receipt::PublicationMode::Ordinary,
            repository: "owner/project".into(),
            branch: branch.into(),
            source: source.clone(),
            published: source,
            projected_session_id: None,
            url: format!("{}/owner/project.git", destination.hub),
            destination,
            policy_digest: None,
            recipient: None,
        };
        let log = transcript::wrap_lines(
            &format!(
                "{raw}{}",
                raw.replace(private_text, "Following ordinary turn")
            ),
            "codex",
            &session,
        );
        storage::write_snapshot(repo.root(), &log, &log).unwrap();
        repo.add_all().unwrap();
        repo.commit("Following ordinary session turn").unwrap();
        repo.git(&[
            "push",
            received.to_str().unwrap(),
            &format!("HEAD:refs/heads/{branch}"),
        ])
        .unwrap();
        return (receipt, remote);
    }
    PrivacyPolicy {
        workspace: Some(repo.root().canonicalize().unwrap()),
        replacements: vec![ReplacementRule {
            pattern: private_text.into(),
            replacement: "REDACTED".into(),
            regex: false,
        }],
        ..Default::default()
    }
    .save(repo)
    .unwrap();
    let key = crypto_box::SecretKey::from([23; 32]);
    let public_key = STANDARD.encode(key.public_key().as_bytes());
    let mut selected = SessionPublicationPlan {
        sources: None,
        asking_again: Default::default(),
        fork_sources: Default::default(),
        pinned_source: None,
        delivery: None,
        identity: destination.clone(),
        remote: Default::default(),
        configs: BTreeMap::from([(
            session.clone(),
            SessionPrivacyResponse {
                config: SessionPrivacyConfig {
                    repository_id: destination.agent_id.clone(),
                    session_id: session.clone(),
                    privacy: metadata.session_privacy.unwrap(),
                    revision: 1,
                    created_at: "2026-01-01T00:00:00Z".into(),
                },
                owner_key: Some(AccountSessionKey {
                    account_id: "account-1".into(),
                    key_version: 1,
                    public_key_algorithm: "x25519".into(),
                    public_key,
                }),
            },
        )]),
    };
    let url = format!("{}/owner/project.git", destination.hub);
    let mut history =
        ProjectedHistory::prepare_sessions(repo, &[branch.into()], &selected, &[]).unwrap();
    history.prepare_acceptance().unwrap();
    let published = history.plan().heads()[0].oid().to_owned();
    std::fs::create_dir_all(received).unwrap();
    let remote = Repo::at(received);
    remote.git(&["init", "--bare"]).unwrap();
    push(&history, &remote, branch);
    let bytes = remote
        .show_result(&published, "privacy/envelope.json")
        .unwrap()
        .unwrap();
    let envelope = PrivacyEnvelope::parse(bytes.as_bytes()).unwrap();
    assert!(
        !envelope
            .public_projection
            .to_string()
            .contains(private_text)
    );
    assert!(envelope.public_projection.to_string().contains("REDACTED"));
    assert!(
        envelope
            .open_layer(&key)
            .unwrap()
            .session_bytes()
            .unwrap()
            .0
            .contains(private_text)
    );
    assert!(remote.git(&["cat-file", "-e", &source]).is_err());
    let receipt = history
        .session_receipts("owner/project", &destination, &url)
        .unwrap()
        .pop()
        .unwrap();
    let reference = format!("refs/heads/{branch}");
    let received_head = remote.git(&["rev-parse", &reference]).unwrap();
    selected.remote = crate::hub::git::RemoteRefs {
        heads: vec![received_head.clone()],
        refs: BTreeMap::from([(reference, received_head)]),
        ..Default::default()
    };
    drop(history);
    let next = format!("{raw}{}", raw.replace(private_text, "Later public turn"));
    let log = transcript::wrap_lines(&next, "codex", &session);
    storage::write_snapshot(repo.root(), &log, &log).unwrap();
    repo.git(&["add", "."]).unwrap();
    repo.git(&["commit", "-m", "Following session turn"])
        .unwrap();
    let mut advanced =
        ProjectedHistory::prepare_sessions(repo, &[branch.into()], &selected, &[]).unwrap();
    advanced.prepare_acceptance().unwrap();
    assert_ne!(advanced.plan().heads()[0].oid(), receipt.published);
    push(&advanced, &remote, branch);
    (receipt, remote)
}

fn push(history: &ProjectedHistory, remote: &Repo, branch: &str) {
    let published = history.plan().heads()[0].oid();
    Repo::at(history.repo().root())
        .git(&[
            "push",
            remote.root().to_str().unwrap(),
            &format!("{published}:refs/heads/{branch}"),
        ])
        .unwrap();
}

pub(super) fn verify(remote: &Repo, delivery: &Delivery, session_encrypted: bool) {
    let notification = &delivery.notification;
    remote
        .git(&[
            "merge-base",
            "--is-ancestor",
            &notification.public_commit,
            &format!("refs/heads/{}", notification.branch),
        ])
        .unwrap();
    let metadata = storage::metadata_local(remote.root(), &notification.public_commit).unwrap();
    assert_eq!(metadata.session, notification.projected_session_id);
    let bytes = remote
        .show_result(&notification.public_commit, "privacy/envelope.json")
        .unwrap();
    if session_encrypted {
        assert_eq!(
            PrivacyEnvelope::parse(bytes.unwrap().as_bytes())
                .unwrap()
                .format_version,
            2
        );
    } else {
        assert!(bytes.is_none());
    }
}
