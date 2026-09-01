#[cfg(unix)]
mod unix {
    use agit::domain::meta::{self, LayoutVersion, Meta};
    use agit::domain::repo::Repo;
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;
    use std::process::Command;

    fn repo_with_layout(path: &std::path::Path, layout: LayoutVersion) -> Repo {
        let repo = Repo::init(path).unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        let mut snapshot = Meta::new_file_line();
        snapshot.layout = layout;
        meta::write(repo.root(), &snapshot).unwrap();
        repo.add_all().unwrap();
        repo.commit(match layout {
            LayoutVersion::V0 => "v0",
            LayoutVersion::V1 => "v1",
        })
        .unwrap();
        repo
    }

    #[test]
    fn agit_status_survives_clean_readonly_and_denied_repos() {
        let fixture = tempfile::tempdir().unwrap();
        let home = fixture.path().join("home");
        let denied = repo_with_layout(&home.join("repos/owner/a-denied"), LayoutVersion::V0);
        let readonly = repo_with_layout(&home.join("repos/owner/b-readonly"), LayoutVersion::V1);
        let _other = repo_with_layout(&home.join("repos/owner/c-other"), LayoutVersion::V1);
        fs::create_dir_all(home.join("store")).unwrap();
        let denied_git = denied.root().join(".git");
        let git_dir = readonly.root().join(".git");
        fs::set_permissions(&denied_git, fs::Permissions::from_mode(0o555)).unwrap();
        fs::set_permissions(&git_dir, fs::Permissions::from_mode(0o555)).unwrap();

        let output = Command::new(env!("CARGO_BIN_EXE_agit"))
            .arg("status")
            .current_dir(fixture.path())
            .env("AGIT_HOME", &home)
            .output()
            .unwrap();

        fs::set_permissions(&denied_git, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(&git_dir, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            output.status.success(),
            "stdout={}\nstderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!git_dir.join("agit-layout-v1-spool.lock").exists());
        assert!(!git_dir.join("agit-checkout-transaction.lock").exists());
    }
}
