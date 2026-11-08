//! The worktree invariant: every linked worktree of a local repository sits in that repository's
//! own place, `worktrees/<owner>/<name>/…`, and its `.git` points to that repository. A worktree
//! left in another repository's place names one repository while writing to another.

use std::path::{Path, PathBuf};
use std::process::Command;

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} in {}: {output:?}",
        dir.display()
    );
    String::from_utf8(output.stdout).unwrap()
}

/// Checks every repository under `home/repos`, and fails on none: at least one linked worktree
/// must be seen, so a fixture that lost its worktrees can't pass.
pub fn assert_each_names_its_repository(home: &Path) {
    let canonical = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let worktrees = canonical(&home.join("worktrees"));
    let mut seen = 0;
    for owner in std::fs::read_dir(home.join("repos")).unwrap().flatten() {
        for repository in std::fs::read_dir(owner.path()).unwrap().flatten() {
            let root = repository.path();
            if !root.join(".git").exists() {
                continue;
            }
            let place = worktrees
                .join(owner.file_name())
                .join(repository.file_name());
            let common = canonical(&root.join(".git"));
            for line in git(&root, &["worktree", "list", "--porcelain"])
                .lines()
                .filter_map(|line| line.strip_prefix("worktree "))
                .skip(1)
            {
                let path = canonical(&PathBuf::from(line));
                assert!(
                    path.starts_with(&place),
                    "{} sits outside {}",
                    path.display(),
                    place.display()
                );
                let points = PathBuf::from(
                    git(
                        &path,
                        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
                    )
                    .trim(),
                );
                assert_eq!(canonical(&points), common, "{}", path.display());
                seen += 1;
            }
        }
    }
    assert!(seen > 0, "no linked worktree was checked");
}
