//! Publication options constrain real receivers even when local Git defaults broaden a push.

use super::{RemoteIdentity, Repo, publication_push_args, push_tags};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const CHILD_ROOT: &str = "AGIT_PUSH_PUBLICATION_TEST_ROOT";
const COMPLETE: &str = "publication receiver fixture verified";

fn isolated(name: &str) -> Option<PathBuf> {
    if let Some(root) = std::env::var_os(CHILD_ROOT) {
        return Some(root.into());
    }
    let root = tempfile::tempdir().unwrap();
    for path in ["agit", "tmp", "templates"] {
        std::fs::create_dir(root.path().join(path)).unwrap();
    }
    let mut child = Command::new(std::env::current_exe().unwrap());
    child.env_clear();
    for key in ["PATH", "SystemRoot", "WINDIR", "ComSpec", "PATHEXT"] {
        if let Some(value) = std::env::var_os(key) {
            child.env(key, value);
        }
    }
    let output = child
        .args([
            "--exact",
            &format!("commands::push::publication_tests::{name}"),
            "--nocapture",
        ])
        .env(CHILD_ROOT, root.path())
        .env("HOME", root.path())
        .env("USERPROFILE", root.path())
        .env("AGIT_HOME", root.path().join("agit"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", root.path().join("empty-config"))
        .env("GIT_TEMPLATE_DIR", root.path().join("templates"))
        .env("GIT_ALLOW_PROTOCOL", "file")
        .env("GIT_AUTHOR_NAME", "Publication fixture")
        .env("GIT_AUTHOR_EMAIL", "publication@example.invalid")
        .env("GIT_COMMITTER_NAME", "Publication fixture")
        .env("GIT_COMMITTER_EMAIL", "publication@example.invalid")
        .env("TMP", root.path().join("tmp"))
        .env("TEMP", root.path().join("tmp"))
        .env("TMPDIR", root.path().join("tmp"))
        .current_dir(root.path())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "Isolated receiver fixture: {output:?}"
    );
    assert!(String::from_utf8(output.stdout).unwrap().contains(COMPLETE));
    None
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(output.status.success(), "Git fixture {args:?}: {output:?}");
    String::from_utf8(output.stdout)
        .unwrap()
        .trim_end()
        .to_owned()
}

fn repository(root: &Path, name: &str, bare: bool) -> PathBuf {
    let path = root.join(name);
    let mut args = vec!["init", "--quiet", "--initial-branch=main"];
    if bare {
        args.push("--bare");
    }
    args.push(path.to_str().unwrap());
    git(root, &args);
    git(&path, &["config", "commit.gpgsign", "false"]);
    git(&path, &["config", "tag.gpgsign", "false"]);
    path
}

fn refs(root: &Path) -> BTreeMap<String, String> {
    git(root, &["for-each-ref", "--format=%(refname) %(objectname)"])
        .lines()
        .map(|line| {
            let (name, oid) = line.split_once(' ').unwrap();
            (name.to_owned(), oid.to_owned())
        })
        .collect()
}

fn identity() -> RemoteIdentity {
    RemoteIdentity::new(
        "http://publication.example.invalid",
        "00000000-0000-0000-0000-000000000001",
    )
    .unwrap()
}

fn push_branch(repo: &Repo) {
    let selected = vec!["main".to_owned()];
    let args = publication_push_args(&selected, repo.ahead_behind().is_none());
    let outcome = crate::hub::git::push_for_remote(repo, &args, &identity(), false).unwrap();
    assert!(
        outcome.ok(),
        "Branch publication failed: {}",
        outcome.stderr
    );
}

fn publish_tag(repo: &Repo) {
    if let Err(outcome) = push_tags(repo, &["selected".to_owned()], &identity(), false) {
        panic!("Tag publication failed: {}", outcome.stderr);
    }
}

/// Explicit tag publication cannot acquire another tag through the user's follow-tags default.
#[test]
fn inherited_follow_tags_cannot_add_refs_to_branch_or_tag_publication() {
    let Some(root) = isolated("inherited_follow_tags_cannot_add_refs_to_branch_or_tag_publication")
    else {
        return;
    };
    let source = repository(&root, "source", false);
    git(
        &source,
        &["commit", "--allow-empty", "-qm", "selected history"],
    );
    git(&source, &["tag", "-a", "selected", "-m", "selected tag"]);
    git(
        &source,
        &["tag", "-a", "extra", "-m", "unselected tag message"],
    );
    git(&source, &["config", "push.followTags", "true"]);
    let head = git(&source, &["rev-parse", "HEAD"]);
    let tag = git(&source, &["rev-parse", "refs/tags/selected"]);
    let repo = Repo::at(&source);
    let branch_remote = repository(&root, "branch.git", true);
    repo.set_remote(branch_remote.to_str().unwrap()).unwrap();
    push_branch(&repo);
    assert_eq!(
        refs(&branch_remote),
        BTreeMap::from([("refs/heads/main".into(), head.clone())])
    );
    publish_tag(&repo);
    assert_eq!(
        refs(&branch_remote),
        BTreeMap::from([
            ("refs/heads/main".into(), head.clone()),
            ("refs/tags/selected".into(), tag.clone()),
        ])
    );

    let tag_remote = repository(&root, "tag.git", true);
    repo.set_remote(tag_remote.to_str().unwrap()).unwrap();
    publish_tag(&repo);
    assert_eq!(
        refs(&tag_remote),
        BTreeMap::from([("refs/tags/selected".into(), tag)])
    );

    let control = repository(&root, "control.git", true);
    repo.set_remote(control.to_str().unwrap()).unwrap();
    let outcome =
        crate::hub::git::run_for_remote(&repo, &["push", "origin", "main"], &identity()).unwrap();
    assert!(
        outcome.ok(),
        "Follow-tags control failed: {}",
        outcome.stderr
    );
    assert!(refs(&control).contains_key("refs/tags/extra"));
    assert_eq!(refs(&control).get("refs/heads/main"), Some(&head));
    println!("{COMPLETE}");
}

/// Submodule defaults cannot publish another repository or omit the selected superproject ref.
#[test]
fn inherited_submodule_recursion_cannot_change_publication_targets() {
    let Some(root) = isolated("inherited_submodule_recursion_cannot_change_publication_targets")
    else {
        return;
    };
    for mode in ["on-demand", "only"] {
        let lab = root.join(mode);
        std::fs::create_dir(&lab).unwrap();
        let sub_remote = repository(&lab, "sub.git", true);
        let sub_source = repository(&lab, "sub-source", false);
        git(
            &sub_source,
            &["commit", "--allow-empty", "-qm", "submodule base"],
        );
        git(&sub_source, &["push", sub_remote.to_str().unwrap(), "main"]);
        let sub_base = git(&sub_source, &["rev-parse", "HEAD"]);
        let source = repository(&lab, "source", false);
        git(
            &source,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "--quiet",
                sub_remote.to_str().unwrap(),
                "nested",
            ],
        );
        git(&source, &["commit", "-qm", "superproject base"]);
        let base = git(&source, &["rev-parse", "HEAD"]);
        let branch_remote = repository(&lab, "branch.git", true);
        let tag_remote = repository(&lab, "tag.git", true);
        let control = repository(&lab, "control.git", true);
        for receiver in [&branch_remote, &tag_remote, &control] {
            git(&source, &["push", receiver.to_str().unwrap(), "main"]);
        }
        let nested = source.join("nested");
        git(
            &nested,
            &[
                "commit",
                "--allow-empty",
                "-qm",
                "unpublished submodule work",
            ],
        );
        let sub_tip = git(&nested, &["rev-parse", "HEAD"]);
        git(&source, &["add", "nested"]);
        git(&source, &["commit", "-qm", "selected superproject history"]);
        git(
            &source,
            &["tag", "-a", "selected", "-m", "selected publication"],
        );
        git(&source, &["config", "push.recurseSubmodules", mode]);
        let tip = git(&source, &["rev-parse", "HEAD"]);
        let tag = git(&source, &["rev-parse", "refs/tags/selected"]);
        let repo = Repo::at(&source);
        repo.set_remote(tag_remote.to_str().unwrap()).unwrap();
        publish_tag(&repo);
        assert_eq!(
            refs(&tag_remote),
            BTreeMap::from([
                ("refs/heads/main".into(), base.clone()),
                ("refs/tags/selected".into(), tag),
            ])
        );
        assert_eq!(refs(&sub_remote).get("refs/heads/main"), Some(&sub_base));
        repo.set_remote(branch_remote.to_str().unwrap()).unwrap();
        push_branch(&repo);
        assert_eq!(refs(&branch_remote).get("refs/heads/main"), Some(&tip));
        assert_eq!(refs(&sub_remote).get("refs/heads/main"), Some(&sub_base));

        repo.set_remote(control.to_str().unwrap()).unwrap();
        git(&source, &["fetch", "--no-tags", "origin"]);
        let outcome =
            crate::hub::git::run_for_remote(&repo, &["push", "origin", "main"], &identity())
                .unwrap();
        assert!(
            outcome.ok(),
            "Submodule control {mode} failed: {}",
            outcome.stderr
        );
        assert_eq!(refs(&sub_remote).get("refs/heads/main"), Some(&sub_tip));
        let expected = if mode == "only" { &base } else { &tip };
        assert_eq!(refs(&control).get("refs/heads/main"), Some(expected));
    }
    println!("{COMPLETE}");
}

/// Explicit credential acceptance requires successful preparation and complete scan coverage.
#[test]
fn explicit_acceptance_requires_prepared_complete_scan() {
    use super::{ExitCode, Gate, finish_secret_scan, secrets};

    let Some(_root) = isolated("explicit_acceptance_requires_prepared_complete_scan") else {
        return;
    };
    let hits = || {
        vec![secrets::Hit {
            rule: "registered-secret".into(),
            file: Some("AGENTS.md".into()),
            source: secrets::Source::File,
            line: 1,
            redacted: "synthetic redacted finding".into(),
            fingerprint: 1,
        }]
    };
    for accepted in [false, true] {
        let clean = secrets::ScanReport {
            binary_carriers: 0,
            hits: Vec::new(),
            truncated: false,
            unscanned: Default::default(),
        };
        assert!(matches!(
            finish_secret_scan(Ok(clean), accepted).unwrap(),
            Gate::Pass
        ));
        for (error, expected) in [
            (
                anyhow::Error::from(secrets::ScanPreparationFailure::Configuration),
                ExitCode::Usage,
            ),
            (
                anyhow::Error::from(secrets::ScanPreparationFailure::LocalState),
                ExitCode::Precondition,
            ),
            (
                anyhow::anyhow!("synthetic preparation failure"),
                ExitCode::Failure,
            ),
        ] {
            assert!(
                matches!(finish_secret_scan(Err(error), accepted).unwrap(), Gate::Blocked(code) if code == expected)
            );
        }
        for unscanned in [
            secrets::Unscanned {
                over_budget: Some((2, 1)),
                ..Default::default()
            },
            secrets::Unscanned {
                oversized: vec![("aaaaaaaa".into(), 2)],
                ..Default::default()
            },
            secrets::Unscanned {
                oversized_files: vec![("AGENTS.md".into(), 2)],
                ..Default::default()
            },
        ] {
            for findings in [Vec::new(), hits()] {
                let report = secrets::ScanReport {
                    binary_carriers: 0,
                    hits: findings,
                    truncated: false,
                    unscanned: unscanned.clone(),
                };
                assert!(matches!(
                    finish_secret_scan(Ok(report), accepted).unwrap(),
                    Gate::Blocked(ExitCode::Policy)
                ));
            }
        }
        let report = secrets::ScanReport {
            binary_carriers: 0,
            hits: hits(),
            truncated: false,
            unscanned: Default::default(),
        };
        let verdict = finish_secret_scan(Ok(report), accepted).unwrap();
        assert_eq!(matches!(verdict, Gate::Pass), accepted);
    }
    println!("{COMPLETE}");
}
