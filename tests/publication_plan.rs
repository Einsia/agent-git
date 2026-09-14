//! Frozen publication metadata feeds the real scanner without substituting live refs.

#![cfg(feature = "cli")]

use agit::domain::repo::Repo;
use agit::domain::repo::publication::PublicationPlan;
use agit::domain::secrets::{self, ScanPlan, Source};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const CHILD_HOME: &str = "AGIT_PUBLICATION_PLAN_TEST_HOME";
const AWS: &str = "AKIA4X7QZ2M5RT6VW3JH";

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(output.status.success(), "Git fixture failed: {args:?}");
    String::from_utf8(output.stdout)
        .unwrap()
        .trim_end()
        .to_owned()
}

fn inventory(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    walkdir::WalkDir::new(root)
        .into_iter()
        .map(Result::unwrap)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| {
            (
                entry.path().strip_prefix(root).unwrap().to_owned(),
                std::fs::read(entry.path()).unwrap(),
            )
        })
        .collect()
}

/// Moving refs to clean history cannot make the frozen scanner omit retained disclosures.
#[test]
fn frozen_plan_drives_scanner_after_live_refs_move() {
    let Some(home) = std::env::var_os(CHILD_HOME) else {
        let home = tempfile::tempdir().unwrap();
        for path in ["agit", "tmp", "templates"] {
            std::fs::create_dir(home.path().join(path)).unwrap();
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
                "frozen_plan_drives_scanner_after_live_refs_move",
                "--nocapture",
            ])
            .env(CHILD_HOME, home.path())
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .env("AGIT_HOME", home.path().join("agit"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", home.path().join("empty-config"))
            .env("GIT_TEMPLATE_DIR", home.path().join("templates"))
            .env("TMP", home.path().join("tmp"))
            .env("TEMP", home.path().join("tmp"))
            .env("TMPDIR", home.path().join("tmp"))
            .current_dir(home.path())
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "Isolated library fixture: {output:?}"
        );
        assert!(
            String::from_utf8(output.stdout)
                .unwrap()
                .contains("publication planner scanner fixture verified")
        );
        return;
    };
    let root = PathBuf::from(home).join("repo");
    std::fs::create_dir(&root).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    for (key, value) in [
        ("user.name", "Publication fixture"),
        ("user.email", "publication@example.invalid"),
        ("commit.gpgsign", "false"),
        ("tag.gpgsign", "false"),
    ] {
        git(&root, &["config", key, value]);
    }
    git(&root, &["commit", "--allow-empty", "-qm", "clean base"]);
    let base = git(&root, &["rev-parse", "HEAD"]);
    let repo = Repo::at(&root);
    // The scanner's local lock carriers exist before the publication observation begins.
    assert!(
        secrets::scan_agent_repo(&repo, &ScanPlan::full())
            .unwrap()
            .hits
            .is_empty()
    );
    git(&root, &["checkout", "-qb", "selected"]);
    std::fs::write(root.join("payload.txt"), format!("key = {AWS}\n")).unwrap();
    git(&root, &["add", "payload.txt"]);
    git(
        &root,
        &["commit", "-qm", &format!("commit disclosure {AWS}")],
    );
    let selected = git(&root, &["rev-parse", "HEAD"]);
    git(
        &root,
        &[
            "tag",
            "-a",
            "inner",
            "-m",
            &format!("inner disclosure {AWS}"),
        ],
    );
    let inner = git(&root, &["rev-parse", "refs/tags/inner"]);
    git(
        &root,
        &[
            "-c",
            "advice.nestedTag=false",
            "tag",
            "-a",
            "outer",
            "inner",
            "-m",
            &format!("outer disclosure {AWS}"),
        ],
    );
    let outer = git(&root, &["rev-parse", "refs/tags/outer"]);
    git(&root, &["tag", "-d", "inner"]);
    git(&root, &["checkout", "-qb", "unselected", &base]);
    git(
        &root,
        &["commit", "--allow-empty", "-qm", "unselected history"],
    );
    let unselected = git(&root, &["rev-parse", "HEAD"]);
    git(&root, &["tag", "unselected-version"]);
    git(&root, &["checkout", "-q", "main"]);
    let branches = vec!["selected".to_owned()];
    let before = inventory(&root);
    let plan = PublicationPlan::freeze(&repo, &branches).unwrap();
    assert_eq!(
        plan.heads()
            .iter()
            .map(|reference| reference.name())
            .collect::<Vec<_>>(),
        ["refs/heads/main", "refs/heads/selected"]
    );
    assert!(plan.commit_objects().contains(&base));
    assert!(plan.commit_objects().contains(&selected));
    assert!(!plan.commit_objects().contains(&unselected));
    assert_eq!(plan.tags().len(), 1);
    assert_eq!(plan.tags()[0].name(), "refs/tags/outer");
    assert_eq!(plan.tags()[0].oid(), outer);
    assert!(plan.tag_objects().contains(&inner));
    assert!(plan.tag_objects().contains(&outer));
    plan.verify(&repo, &branches).unwrap();
    assert_eq!(inventory(&root), before);

    git(
        &root,
        &["update-ref", "refs/heads/selected", &base, &selected],
    );
    git(&root, &["update-ref", "refs/tags/outer", &base, &outer]);
    let before = inventory(&root);
    assert!(plan.verify(&repo, &branches).is_err());
    assert!(
        secrets::scan_agent_repo(&repo, &ScanPlan::full())
            .unwrap()
            .hits
            .is_empty()
    );
    let report =
        secrets::scan_agent_repo_frozen(&repo, plan.commit_objects(), plan.tag_objects()).unwrap();
    assert!(report.unscanned.is_empty());
    for source in [Source::CommitObject, Source::BlobObject, Source::TagObject] {
        assert!(
            report.hits.iter().any(|hit| hit.source == source),
            "Missing immutable carrier: {source:?}"
        );
    }
    for oid in [&inner, &outer] {
        let label = format!("tag object {}", &oid[..8]);
        assert!(
            report.hits.iter().any(|hit| hit.source == Source::TagObject
                && hit.file.as_deref() == Some(label.as_str()))
        );
    }
    assert_eq!(
        plan.heads()[1].refspec(),
        format!("{selected}:refs/heads/selected")
    );
    assert_eq!(plan.tags()[0].refspec(), format!("{outer}:refs/tags/outer"));
    assert_eq!(inventory(&root), before);
    println!("publication planner scanner fixture verified");
}
