use super::*;
use crate::domain::{
    secret_filter::{Matcher, MatcherHandle, RepositoryDictionary},
    storage, transcript,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use crypto_box::SecretKey;

/// Source identity alone cannot authorize reuse after policy, secret rules, dictionary or path
/// permissions change. New secret rules may share a generation with the previous rule set.
#[test]
fn incremental_reuse_rechecks_mutable_dependencies_and_authenticates_its_index() {
    let temp = tempfile::tempdir().unwrap();
    let source = Repo::init(&temp.path().join("source")).unwrap();
    source
        .git(&["config", "user.name", "Privacy fixture"])
        .unwrap();
    source
        .git(&["config", "user.email", "privacy@example.invalid"])
        .unwrap();
    source.git(&["checkout", "-b", "work"]).unwrap();
    let workspace = source.root().canonicalize().unwrap();
    fs::create_dir(workspace.join("src")).unwrap();
    let path = workspace.join("src/visible.rs");
    fs::write(&path, "FILE_BODY_NOT_COLLECTED").unwrap();
    let session = format!("agit-{}", "b".repeat(40));
    let raw = [
        json!({"type":"user","message":{"role":"user","content":"Public request"}}),
        json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"read","name":"Read","input":{"file_path":path}}]}}),
        json!({"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"read","content":"SYNTHETIC_TOOL_BODY"}]}}),
    ].into_iter().map(|value| format!("{value}\n")).collect::<String>();
    let log = transcript::wrap_lines(&raw, "claude-code", &session);
    storage::write_snapshot(source.root(), &log, &log).unwrap();
    meta::write(
        source.root(),
        &Meta::new(
            session,
            "claude-code".into(),
            workspace.display().to_string(),
        ),
    )
    .unwrap();
    source.git(&["add", "."]).unwrap();
    source.git(&["commit", "-m", "Private source"]).unwrap();
    let mut policy = PrivacyPolicy {
        workspace: Some(workspace.clone()),
        ..Default::default()
    };
    policy.save(&source).unwrap();
    let key = SecretKey::from([24; 32]);
    let recipient = ViewingRecipient::from_base64(
        "viewer".into(),
        &STANDARD.encode(key.public_key().as_bytes()),
    )
    .unwrap();
    let registered = MatcherHandle::new(Matcher::for_test(&[("one", "Unrelated value")]));
    let redactor = Redactor::with_registered(Default::default(), registered.clone());
    let prepare = || {
        ProjectedHistory::prepare_with_redactor(
            &source,
            &["work".into()],
            &recipient,
            "https://hub.invalid/alice/cache",
            &redactor,
        )
    };
    let started = std::time::Instant::now();
    let initial = prepare().unwrap();
    println!("initial projection preparation={:?}", started.elapsed());
    assert_eq!(initial.preparations, 1);
    let cache_root = initial.repo.root().to_path_buf();
    let published = initial.plan.heads()[0].oid().to_owned();
    let original = source.git(&["rev-parse", "HEAD"]).unwrap();
    let scope = crate::domain::repo::publication::InspectionScope::incremental(
        &initial.repo.clone().exact_root_inspection(),
        &initial.plan,
        [original].into_iter(),
    )
    .unwrap();
    assert_eq!(scope.commits, initial.plan.commit_objects());
    let scope = crate::domain::repo::publication::InspectionScope::incremental(
        &initial.repo.clone().exact_root_inspection(),
        &initial.plan,
        [published.clone()].into_iter(),
    )
    .unwrap();
    assert!(scope.commits.is_empty());
    drop(initial);
    let started = std::time::Instant::now();
    let reused = prepare().unwrap();
    println!("reused projection preparation={:?}", started.elapsed());
    assert_eq!(reused.preparations, 0);
    assert_eq!(reused.plan.heads()[0].oid(), published);
    drop(reused);

    fs::write(&path, "CHANGED_FILE_BODY_NOT_COLLECTED").unwrap();
    assert_eq!(
        prepare().unwrap().preparations,
        0,
        "captured session output does not depend on current file bodies"
    );
    #[cfg(unix)]
    {
        fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(temp.path(), &path).unwrap();
        let denied = prepare().unwrap();
        assert_eq!(denied.preparations, 1);
        assert!(
            !denied
                .inspection_views
                .values()
                .any(|text| text.contains("SYNTHETIC_TOOL_BODY"))
        );
        drop(denied);
        fs::remove_file(&path).unwrap();
        fs::write(&path, "RESTORED_FILE_BODY_NOT_COLLECTED").unwrap();
        assert_eq!(prepare().unwrap().preparations, 1);
    }

    let new_rules = Matcher::for_test(&[("two", "Public request")]);
    assert_eq!(new_rules.generation(), registered.snapshot().generation());
    registered.replace(new_rules);
    let changed = prepare().unwrap();
    assert_eq!(changed.preparations, 1);
    let scope = crate::domain::repo::publication::InspectionScope::incremental(
        &changed.repo.clone().exact_root_inspection(),
        &changed.plan,
        [published].into_iter(),
    )
    .unwrap();
    assert_eq!(scope.commits, changed.plan.commit_objects());
    assert!(
        !changed
            .inspection_views
            .values()
            .any(|text| text.contains("Public request"))
    );
    drop(changed);
    assert_eq!(prepare().unwrap().preparations, 0);

    RepositoryDictionary::open(source.root())
        .unwrap()
        .import_publication_values(&BTreeSet::from(["new dictionary value".into()]))
        .unwrap();
    assert_eq!(prepare().unwrap().preparations, 1);
    assert_eq!(prepare().unwrap().preparations, 0);
    policy.exclude.push("src/**".into());
    policy.save(&source).unwrap();
    let denied = prepare().unwrap();
    assert_eq!(denied.preparations, 1);
    assert!(
        !denied
            .inspection_views
            .values()
            .any(|text| text.contains("SYNTHETIC_TOOL_BODY"))
    );
    drop(denied);

    let previous = prepare().unwrap();
    let published = previous.plan.heads()[0].oid().to_owned();
    drop(previous);
    let rotated_key = SecretKey::from([25; 32]);
    let rotated_recipient = ViewingRecipient::from_base64(
        "rotated-viewer".into(),
        &STANDARD.encode(rotated_key.public_key().as_bytes()),
    )
    .unwrap();
    let rotated = ProjectedHistory::prepare_with_redactor(
        &source,
        &["work".into()],
        &rotated_recipient,
        "https://hub.invalid/alice/cache",
        &redactor,
    )
    .unwrap();
    let scope = crate::domain::repo::publication::InspectionScope::incremental(
        &rotated.repo.clone().exact_root_inspection(),
        &rotated.plan,
        [published].into_iter(),
    )
    .unwrap();
    assert_eq!(scope.commits, rotated.plan.commit_objects());
    drop(rotated);
    drop(prepare().unwrap());

    let cache = cache_root.join(".git/privacy-preparation/index.enc");
    let mut bytes = fs::read(&cache).unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains(workspace.to_str().unwrap()));
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    fs::write(&cache, bytes).unwrap();
    let error = prepare()
        .err()
        .expect("an edited preparation record cannot authorize reuse");
    assert!(
        error.to_string().contains("authentication failed"),
        "{error:#}"
    );
}
