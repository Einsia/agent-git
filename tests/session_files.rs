use agit::domain::{
    meta::{self, Meta},
    repo::Repo,
};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

struct Lab {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    workspace: PathBuf,
    primary: Repo,
}

impl Lab {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let workspace = tmp.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let primary = Repo::init(&home.join("repos/me/files")).unwrap();
        primary.git(&["config", "commit.gpgsign", "false"]).unwrap();
        primary
            .set_remote("https://files.test/me/files.git")
            .unwrap();
        meta::write(primary.root(), &Meta::new_file_line()).unwrap();
        agit::domain::storage::ensure_attributes(primary.root()).unwrap();
        fs::write(primary.root().join("README.md"), "# files\n").unwrap();
        primary.add_all().unwrap();
        primary.commit("initialize").unwrap();
        primary.git(&["branch", "first"]).unwrap();
        primary.git(&["branch", "second"]).unwrap();
        Self {
            _tmp: tmp,
            home,
            workspace,
            primary,
        }
    }
    fn call(&self, branch: &str, args: &[&str]) -> std::process::Output {
        self.call_at(branch, args, &self.workspace)
    }
    fn call_at(&self, branch: &str, args: &[&str], cwd: &Path) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_agit"))
            .args(args)
            .current_dir(cwd)
            .env("AGIT_HOME", &self.home)
            .env("AGIT_SESSION", format!("me/files@{branch}"))
            .env("AGIT_HUB_URL", "http://127.0.0.1:9")
            .env("GIT_AUTHOR_NAME", "Fixture")
            .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
            .env("GIT_COMMITTER_NAME", "Fixture")
            .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
            .output()
            .unwrap()
    }
    fn ok(&self, branch: &str, args: &[&str]) -> String {
        let out = self.call(branch, args);
        assert!(
            out.status.success(),
            "{args:?}: {}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }
    fn checkout(&self, branch: &str) -> Repo {
        Repo::at(PathBuf::from(self.ok(branch, &["file", "cwd"])))
    }
}

#[test]
fn file_diffs_preserve_patch_bytes_and_apply_with_trailing_whitespace() {
    let lab = Lab::new();
    let repo = lab.checkout("first");
    let path = repo.root().join("README.md");
    fs::write(&path, b"old report\n").unwrap();
    lab.ok("first", &["file", "add", path.to_str().unwrap()]);
    lab.ok("first", &["file", "commit", "-m", "save baseline"]);
    for staged in [false, true] {
        fs::write(&path, b"new report  \n").unwrap();
        let args = if staged {
            lab.ok("first", &["file", "add", path.to_str().unwrap()]);
            vec!["file", "diff", "--staged"]
        } else {
            vec!["file", "diff"]
        };
        let output = lab.call("first", &args);
        assert!(output.status.success());
        let mut git_args = vec!["diff", "--no-ext-diff", "--no-textconv"];
        if staged {
            git_args.push("--cached");
        }
        git_args.push("--");
        assert_eq!(output.stdout, repo.git_bytes_result(&git_args).unwrap());
        assert!(output.stdout.ends_with(b"+new report  \n"));
        let patch = lab.workspace.join("report.patch");
        fs::write(&patch, &output.stdout).unwrap();
        fs::write(&path, b"old report\n").unwrap();
        repo.git(&["apply", "--check", patch.to_str().unwrap()])
            .unwrap();
        repo.git(&["apply", patch.to_str().unwrap()]).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new report  \n");
    }
}

#[test]
fn permalinks_follow_the_selected_origin_instead_of_the_global_hub() {
    let lab = Lab::new();
    lab.primary
        .set_remote("https://staging.test/hub/me/files.git")
        .unwrap();
    agit::hub::identity::pin(
        &lab.primary,
        &agit::hub::identity::RemoteIdentity::new(
            "https://old.test",
            "00000000-0000-0000-0000-000000000001",
        )
        .unwrap(),
    )
    .unwrap();
    let url = lab.ok("first", &["file", "link", "README.md"]);
    assert!(
        url.starts_with("https://staging.test/hub/@me/files?"),
        "{url}"
    );
    for origin in [
        "https://staging.test/hub/another/files.git",
        "https://staging.test/hub/me/another.git",
        "https://staging.test/hub/../me/files.git",
        "https://staging.test/hub/%2e%2e/me/files.git",
        "https://staging.test/hub/me/files.git?other=1",
    ] {
        lab.primary.set_remote(origin).unwrap();
        assert!(
            !lab.call("first", &["file", "link", "README.md"])
                .status
                .success(),
            "{origin}"
        );
    }
    lab.primary.git(&["remote", "remove", "origin"]).unwrap();
    assert!(
        !lab.call("first", &["file", "link", "README.md"])
            .status
            .success()
    );
}

#[test]
fn worktree_subdirectories_and_native_path_separators_select_the_same_file() {
    let lab = Lab::new();
    let repo = lab.checkout("first");
    let artifacts = repo.root().join("artifacts");
    fs::create_dir_all(&artifacts).unwrap();
    fs::write(artifacts.join("report.pdf"), b"first").unwrap();
    let result = lab.call_at("first", &["file", "add", "./report.pdf"], &artifacts);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        repo.git_bytes_result(&["show", ":artifacts/report.pdf"])
            .unwrap(),
        b"first"
    );

    fs::write(artifacts.join("report.pdf"), b"second").unwrap();
    let source = Path::new("artifacts").join("report.pdf");
    let result = lab.call_at(
        "first",
        &["file", "add", source.to_str().unwrap()],
        repo.root(),
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        repo.git_bytes_result(&["show", ":artifacts/report.pdf"])
            .unwrap(),
        b"second"
    );
}

#[test]
fn file_commands_preserve_branch_indexes_and_snapshot_add_time_bytes() {
    let lab = Lab::new();
    let a = lab.checkout("first");
    let b = lab.checkout("second");
    assert_ne!(a.root(), b.root());
    assert_eq!(lab.primary.current_branch().as_deref(), Some("main"));
    let expected_a = lab.ok("second", &["file", "cwd", "--into", "me/files@first"]);
    assert_eq!(Path::new(&expected_a), a.root());

    fs::write(lab.workspace.join("report.pdf"), b"%PDF-1.7\n\0first").unwrap();
    let head = a.git(&["rev-parse", "HEAD"]).unwrap();
    lab.ok("first", &["file", "add", "report.pdf"]);
    assert_eq!(a.git(&["rev-parse", "HEAD"]).unwrap(), head);
    fs::write(a.root().join("artifacts/report.pdf"), b"later draft").unwrap();
    fs::write(a.root().join("private-note.md"), "unselected").unwrap();
    fs::write(b.root().join("other.md"), "second branch").unwrap();
    b.git(&["add", "other.md"]).unwrap();
    let second_index = b.git_bytes_result(&["ls-files", "--stage", "-z"]).unwrap();
    lab.ok(
        "first",
        &["file", "commit", "-m", "publish selected artifact"],
    );
    assert_eq!(
        a.git_bytes_result(&["show", "HEAD:artifacts/report.pdf"])
            .unwrap(),
        b"%PDF-1.7\n\0first"
    );
    assert_eq!(
        fs::read(a.root().join("artifacts/report.pdf")).unwrap(),
        b"later draft"
    );
    assert!(!lab.ok("first", &["file", "list"]).contains("private-note"));
    assert_eq!(
        b.git_bytes_result(&["ls-files", "--stage", "-z"]).unwrap(),
        second_index
    );
    assert_eq!(b.git(&["rev-parse", "HEAD"]).unwrap(), head);
    lab.ok(
        "first",
        &[
            "file",
            "get",
            "artifacts/report.pdf",
            "--output",
            "download.pdf",
        ],
    );
    assert_eq!(
        fs::read(lab.workspace.join("download.pdf")).unwrap(),
        b"%PDF-1.7\n\0first"
    );
    let url = lab.ok("first", &["file", "link", "artifacts/report.pdf"]);
    assert!(url.contains(&format!("ref={}", a.git(&["rev-parse", "HEAD"]).unwrap())));
    assert!(url.ends_with("file=artifacts/report.pdf"));
}

#[test]
fn rename_removal_and_unstage_are_explicit() {
    let lab = Lab::new();
    let repo = lab.checkout("first");
    fs::write(lab.workspace.join("source.md"), "artifact").unwrap();
    lab.ok(
        "first",
        &["file", "add", "source.md", "--to", "artifacts/old.md"],
    );
    lab.ok(
        "first",
        &["file", "restore", "--staged", "artifacts/old.md"],
    );
    assert!(repo.root().join("artifacts/old.md").is_file());
    assert!(
        repo.git(&["diff", "--cached", "--name-only"])
            .unwrap()
            .is_empty()
    );
    lab.ok(
        "first",
        &["file", "add", "source.md", "--to", "artifacts/old.md"],
    );
    lab.ok("first", &["file", "commit", "-m", "add"]);
    lab.ok(
        "first",
        &["file", "mv", "artifacts/old.md", "artifacts/new.md"],
    );
    assert!(repo.show("HEAD", "artifacts/old.md").is_some());
    lab.ok("first", &["file", "commit", "-m", "rename"]);
    assert!(repo.show("HEAD", "artifacts/old.md").is_none());
    lab.ok("first", &["file", "rm", "--cached", "artifacts/new.md"]);
    assert!(repo.root().join("artifacts/new.md").is_file());
    lab.ok("first", &["file", "commit", "-m", "remove"]);
    assert!(repo.show("HEAD", "artifacts/new.md").is_none());
}

#[test]
fn paths_cannot_reach_storage_or_escape_the_file_worktree() {
    let lab = Lab::new();
    let repo = lab.checkout("first");
    fs::write(lab.workspace.join("source.md"), "artifact").unwrap();
    for dest in [
        "../escape",
        "session/meta.json",
        "events/payload",
        ".git/config",
        "artifacts/../../escape",
        "artifacts/../LOG",
    ] {
        let before = repo
            .git_bytes_result(&["ls-files", "--stage", "-z"])
            .unwrap();
        assert!(
            !lab.call("first", &["file", "add", "source.md", "--to", dest])
                .status
                .success(),
            "accepted {dest}"
        );
        assert_eq!(
            repo.git_bytes_result(&["ls-files", "--stage", "-z"])
                .unwrap(),
            before
        );
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&lab.workspace, repo.root().join("outside")).unwrap();
        assert!(
            !lab.call(
                "first",
                &["file", "add", "source.md", "--to", "outside/copied.md"]
            )
            .status
            .success()
        );
        assert!(!lab.workspace.join("copied.md").exists());
    }
}

#[test]
fn filesystem_aliases_cannot_modify_storage_or_git_administration() {
    let lab = Lab::new();
    let repo = lab.checkout("first");
    fs::write(lab.workspace.join("source.md"), "replacement").unwrap();
    fs::write(repo.root().join("LOG"), "history").unwrap();
    fs::write(repo.root().join("VIEW"), "view").unwrap();
    fs::create_dir_all(repo.root().join("events")).unwrap();
    fs::write(repo.root().join("events/payload"), "event").unwrap();
    let protected = [
        repo.root().join("session/meta.json"),
        repo.root().join("LOG"),
        repo.root().join("VIEW"),
        repo.root().join("events/payload"),
        repo.root().join(".git"),
        lab.primary.root().join(".git/config"),
    ];
    let snapshots: Vec<_> = protected
        .iter()
        .map(|path| fs::read(path).unwrap())
        .collect();
    let index = repo
        .git_bytes_result(&["ls-files", "--stage", "-z"])
        .unwrap();
    for alias in [
        "Session/meta.json",
        "SESSION/meta.json",
        "log",
        "view",
        "Events/payload",
        "session./meta.json",
        "session /meta.json",
        "LOG.",
        "LOG ",
        "LOG:stream",
        ".git.",
        ".git ",
        ".GiT/config",
        "artifacts/.git./config",
    ] {
        assert!(
            !lab.call("first", &["file", "add", "source.md", "--to", alias])
                .status
                .success(),
            "{alias}"
        );
        assert!(
            !lab.call("first", &["file", "rm", alias]).status.success(),
            "{alias}"
        );
        assert!(
            !lab.call("first", &["file", "mv", "README.md", alias])
                .status
                .success(),
            "{alias}"
        );
        assert!(
            !lab.call("first", &["file", "restore", "--staged", alias])
                .status
                .success(),
            "{alias}"
        );
        for (path, before) in protected.iter().zip(&snapshots) {
            assert_eq!(
                &fs::read(path).unwrap(),
                before,
                "{alias}: {}",
                path.display()
            );
        }
        assert_eq!(
            repo.git_bytes_result(&["ls-files", "--stage", "-z"])
                .unwrap(),
            index
        );
    }
}

#[test]
fn adding_the_worktree_stages_deletions_without_managed_paths() {
    let lab = Lab::new();
    let repo = lab.checkout("first");
    fs::remove_file(repo.root().join("README.md")).unwrap();
    fs::write(repo.root().join("report.md"), "report").unwrap();
    let root = repo.root().to_str().unwrap();
    lab.ok("first", &["-C", root, "file", "add", "."]);
    let staged = repo.git(&["diff", "--cached", "--name-only"]).unwrap();
    assert_eq!(staged, "README.md\nreport.md");
    lab.ok("first", &["file", "commit", "-m", "replace README"]);
    assert!(repo.show("HEAD", "README.md").is_none());
    assert_eq!(repo.show("HEAD", "report.md").as_deref(), Some("report"));
}
