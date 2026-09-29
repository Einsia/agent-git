use super::*;
use crate::domain::{
    meta::{self, Meta},
    privacy::{PrivacyPolicy, ReplacementRule},
    privacy_envelope::{PrivacyEnvelope, ViewingRecipient},
    privacy_git::ProjectedHistory,
    storage, transcript,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};

pub(super) fn publication(
    repo: &Repo,
    branch: &str,
    destination: crate::hub::identity::RemoteIdentity,
    received: &std::path::Path,
    encryption_enabled: bool,
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
    meta::write(
        repo.root(),
        &Meta::new(session.clone(), "codex".into(), String::new()),
    )
    .unwrap();
    repo.git(&["add", "."]).unwrap();
    repo.git(&["commit", "-m", "Source session"]).unwrap();
    let source = repo.git(&["rev-parse", "HEAD"]).unwrap();
    if !encryption_enabled {
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
    let recipient = ViewingRecipient::from_base64(
        crate::domain::privacy_envelope::digest_bytes(public_key.as_bytes()).replace(':', "-"),
        &public_key,
    )
    .unwrap();
    let url = format!("{}/owner/project.git", destination.hub);
    let history = ProjectedHistory::prepare(repo, &[branch.into()], &recipient, &url).unwrap();
    let published = history.plan().heads()[0].oid().to_owned();
    let projected_session_id = storage::metadata_local(history.repo().root(), &published)
        .unwrap()
        .session;
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
    let receipt = PublicationReceipt {
        version: 1,
        mode: Default::default(),
        repository: "owner/project".into(),
        branch: branch.into(),
        source,
        published,
        projected_session_id: Some(projected_session_id),
        destination,
        url,
        policy_digest: Some(history.policy_digest().into()),
        recipient: Some(recipient.fingerprint().unwrap()),
    };
    drop(history);
    let next = format!("{raw}{}", raw.replace(private_text, "Later public turn"));
    let log = transcript::wrap_lines(&next, "codex", &session);
    storage::write_snapshot(repo.root(), &log, &log).unwrap();
    repo.git(&["add", "."]).unwrap();
    repo.git(&["commit", "-m", "Following session turn"])
        .unwrap();
    let advanced =
        ProjectedHistory::prepare(repo, &[branch.into()], &recipient, &receipt.url).unwrap();
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

pub(super) fn verify(remote: &Repo, delivery: &Delivery, encryption_enabled: bool) {
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
    if encryption_enabled {
        PrivacyEnvelope::parse(bytes.unwrap().as_bytes()).unwrap();
    } else {
        assert!(bytes.is_none());
    }
}
