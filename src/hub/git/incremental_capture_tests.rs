use super::*;
use crate::domain::secrets::{ScanLimits, Source as Carrier};
use crate::hub::git::{CompleteContentInspection, ContentInspection};

fn capture(repo: &Repo, roots: &[String], budget: u64) -> Result<CapturedPublication> {
    let started = std::time::Instant::now();
    let plan = PublicationPlan::freeze(repo, &["main".into()])?;
    let history_time = started.elapsed();
    let started = std::time::Instant::now();
    let source = Source::new(repo)?;
    let advertised = AdvertisedBaseline {
        url: "https://hub.invalid/alice/repo.git".into(),
        identity: RemoteIdentity::new(
            "https://hub.invalid",
            "00000000-0000-0000-0000-000000000001",
        )?,
        refs: crate::hub::git::RemoteRefs {
            refs: roots
                .iter()
                .enumerate()
                .map(|(index, oid)| (format!("refs/heads/branch-{index}"), oid.clone()))
                .collect(),
            ..Default::default()
        },
    };
    let git = GitContent::capture_advertised(&source, &plan, Some(advertised))?;
    let scope_time = started.elapsed();
    let isolated = Repo::at(git.directory.path()).exact_bare_root_inspection();
    let mut objects = std::collections::BTreeSet::new();
    for commit in &git.scope.commits {
        isolated.git_stream_split(
            &[
                "rev-list",
                "--no-walk",
                "--objects",
                "--no-object-names",
                commit,
            ],
            b'\n',
            |line| {
                let oid = std::str::from_utf8(line)?;
                if !git.scope.excluded.contains(oid) {
                    objects.insert(oid.to_owned());
                }
                Ok(())
            },
        )?;
    }
    objects.extend(git.scope.tags.iter().cloned());
    let bytes: u64 = git.lfs_inventory.iter().map(|pointer| pointer.size).sum();
    let started = std::time::Instant::now();
    let captured = CapturedPublication::stage(repo, &plan, budget, false, source, git)?;
    println!(
        "history={history_time:?} scope={scope_time:?} staging={:?} objects={} payload_bytes={bytes}",
        started.elapsed(),
        objects.len()
    );
    Ok(captured)
}

fn inspect(captured: CapturedPublication) -> CompleteContentInspection {
    let started = std::time::Instant::now();
    let complete = match captured.inspect(ScanLimits::DEFAULT) {
        ContentInspection::Complete(complete) => complete,
        ContentInspection::Blocked(blocked) => {
            panic!("selected content is incomplete: {:?}", blocked.reason())
        }
    };
    println!("content_scan={:?}", started.elapsed());
    complete
}

fn commit(repo: &Repo, message: &str) -> String {
    repo.add_all().unwrap();
    repo.commit(message).unwrap();
    repo.git(&["rev-parse", "HEAD"]).unwrap()
}

/// An old payload may disappear locally, but new pointer objects still require its bytes.
#[test]
fn incremental_scope_preserves_history_tags_and_payload_boundaries() {
    const CHILD: &str = "AGIT_INCREMENTAL_CAPTURE_TEST";
    if std::env::var_os(CHILD).is_none() {
        let home = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "hub::git::frozen::captured::incremental_tests::incremental_scope_preserves_history_tags_and_payload_boundaries", "--nocapture"])
            .env(CHILD, "1")
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .env("AGIT_HOME", home.path().join("agit"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", home.path().join("empty-config"))
            .output().unwrap();
        assert!(
            output.status.success(),
            "isolated scope fixture: {output:?}"
        );
        print!("{}", String::from_utf8(output.stdout).unwrap());
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let repo = Repo::init(directory.path()).unwrap();
    repo.git(&["symbolic-ref", "HEAD", "refs/heads/main"])
        .unwrap();
    repo.git(&["config", "user.name", "Incremental fixture"])
        .unwrap();
    repo.git(&["config", "user.email", "incremental@example.invalid"])
        .unwrap();
    repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
    repo.git(&["config", "tag.gpgsign", "false"]).unwrap();
    let payload = b"historical payload";
    use sha2::Digest;
    let pointer = Pointer {
        oid: format!("{:x}", sha2::Sha256::digest(payload)),
        size: payload.len() as u64,
    };
    let pointer_text = format!(
        "version {}\noid sha256:{}\nsize {}\n",
        crate::domain::lfs::VERSION,
        pointer.oid,
        pointer.size
    );
    std::fs::write(repo.root().join("payload"), &pointer_text).unwrap();
    let secret = "AKIA4X7QZ2M5RT6VW3JH";
    std::fs::write(repo.root().join("old-secret"), secret).unwrap();
    let base = commit(&repo, "Base");
    let cache = repo
        .root()
        .join(".git/lfs/objects")
        .join(&pointer.oid[..2])
        .join(&pointer.oid[2..4])
        .join(&pointer.oid);
    std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
    std::fs::write(&cache, payload).unwrap();
    let first = inspect(capture(&repo, &[], pointer.size).unwrap());
    assert!(first.has_findings());
    assert_eq!(first.captured().pointers(), std::slice::from_ref(&pointer));
    let unavailable = inspect(capture(&repo, &["a".repeat(40)], pointer.size).unwrap());
    assert!(unavailable.has_findings());
    std::fs::remove_file(&cache).unwrap();
    let roots = vec![base.clone()];
    let mut captured = capture(&repo, &roots, 0).unwrap();
    let identity = RemoteIdentity::new(
        "https://hub.invalid",
        "00000000-0000-0000-0000-000000000001",
    )
    .unwrap();
    let url = "https://hub.invalid/alice/repo.git";
    captured.git.baseline = Some((url.into(), identity.clone()));
    let repeat = inspect(captured);
    assert!(!repeat.has_findings());
    assert!(repeat.captured().pointers().is_empty());
    let other_identity =
        RemoteIdentity::new(&identity.hub, "00000000-0000-0000-0000-000000000002").unwrap();
    assert!(
        repeat
            .bind_destination(&repo, url, &other_identity)
            .is_err()
    );
    let mut captured = capture(&repo, &roots, 0).unwrap();
    captured.git.baseline = Some((url.into(), identity.clone()));
    assert!(
        inspect(captured)
            .bind_destination(&repo, "https://hub.invalid/alice/other.git", &identity)
            .is_err()
    );
    assert!(capture(&repo, &[], pointer.size).is_err());

    repo.git(&["checkout", "-b", "side"]).unwrap();
    std::fs::write(
        repo.root().join("removed-secret"),
        format!("new = {secret}"),
    )
    .unwrap();
    commit(&repo, "Add unpublished secret");
    repo.git(&["rm", "removed-secret"]).unwrap();
    commit(&repo, "Delete unpublished secret");
    repo.git(&["checkout", "main"]).unwrap();
    std::fs::write(repo.root().join("clean"), "clean content").unwrap();
    commit(&repo, "Independent branch update");
    repo.git(&[
        "merge",
        "--no-ff",
        "side",
        "-m",
        "Merge unpublished history",
    ])
    .unwrap();
    let tip = repo.git(&["rev-parse", "HEAD"]).unwrap();
    repo.git(&["update-ref", "refs/remotes/origin/main", &tip])
        .unwrap();
    let appended = inspect(capture(&repo, &roots, 0).unwrap());
    assert!(appended.report().scan().hits.iter().any(|hit| {
        hit.file
            .as_deref()
            .is_some_and(|file| file.contains("removed-secret"))
    }));
    assert!(appended.captured().pointers().is_empty());
    let roots = vec![tip];
    repo.git(&["tag", "-a", "new-tag", &base, "-m", secret])
        .unwrap();
    let tag = inspect(capture(&repo, &roots, 0).unwrap());
    assert!(
        tag.report()
            .scan()
            .hits
            .iter()
            .any(|hit| hit.source == Carrier::TagObject)
    );
    repo.git(&[
        "tag",
        "-f",
        "-a",
        "new-tag",
        &base,
        "-m",
        &format!("changed {secret}"),
    ])
    .unwrap();
    assert!(tag.verify_source(&repo).is_err());
    let changed = inspect(capture(&repo, &roots, 0).unwrap());
    assert!(
        changed
            .report()
            .scan()
            .hits
            .iter()
            .any(|hit| hit.source == Carrier::TagObject)
    );

    std::fs::write(
        repo.root().join("new-pointer"),
        pointer_text.replace("git-lfs.github.com", "hawser.github.com"),
    )
    .unwrap();
    commit(&repo, "New pointer for an existing payload");
    assert!(capture(&repo, &roots, pointer.size).is_err());
    std::fs::write(&cache, b"corrupt").unwrap();
    assert!(capture(&repo, &roots, pointer.size).is_err());
    std::fs::write(&cache, payload).unwrap();
    let new = inspect(capture(&repo, &roots, pointer.size).unwrap());
    assert_eq!(new.captured().pointers(), std::slice::from_ref(&pointer));
}
