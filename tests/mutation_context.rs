use agit::domain::{link, meta, repo::Repo, storage, store::Store, transcript};
use sha2::{Digest, Sha256};
use std::{fs, path::PathBuf, process::Command};

struct Lab {
    _temp: tempfile::TempDir,
    home: PathBuf,
    work: PathBuf,
    repo: Repo,
}

impl Lab {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("agit");
        let work = temp.path().join("work");
        fs::create_dir_all(&work).unwrap();
        let work = work.canonicalize().unwrap();
        let hub = "http://127.0.0.1:1";
        agit::infra::credentials::save_at(
            &home.join("credentials").join(format!(
                "{}.json",
                agit::infra::config::hub_host_key(hub).unwrap()
            )),
            &agit::infra::credentials::HubCredential {
                username: "me".into(),
                email: None,
                hub: Some(hub.into()),
                access_token: "fixture".into(),
                access_expires_at: "2099-01-01T00:00:00Z".into(),
                refresh_token: "fixture".into(),
                refresh_expires_at: "2099-01-01T00:00:00Z".into(),
            },
        )
        .unwrap();
        let repo = Repo::init(&home.join("repos/me/fixture")).unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
        storage::ensure_attributes(repo.root()).unwrap();
        repo.add_all().unwrap();
        repo.commit("shared file line").unwrap();
        repo.git(&["branch", "work", "main"]).unwrap();
        let tree = agit::commands::worktree::checkout(&repo, "work").unwrap();
        let id = format!("agit-{}", "a".repeat(40));
        let raw = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"fixture conversation\"}}\n";
        let envelope = transcript::wrap_lines(raw, "claude-code", &id);
        storage::write_snapshot(tree.root(), &envelope, &envelope).unwrap();
        meta::write(
            tree.root(),
            &meta::Meta::new(id, "claude-code".into(), work.display().to_string()),
        )
        .unwrap();
        tree.add_all().unwrap();
        tree.commit("saved conversation").unwrap();
        Self {
            _temp: temp,
            home,
            work,
            repo,
        }
    }

    fn pin(&self) {
        let digest = hex::encode(Sha256::digest(self.work.to_string_lossy().as_bytes()));
        fs::create_dir_all(self.home.join("workspaces")).unwrap();
        fs::write(
            self.home
                .join("workspaces")
                .join(format!("{}.json", &digest[..16])),
            serde_json::to_vec(&serde_json::json!({
                "dir": self.work, "repo": "me/fixture", "pinned": "main"
            }))
            .unwrap(),
        )
        .unwrap();
    }

    fn adopt(&self, id: &str, branch: &str) {
        let mut record = link::Link::new("claude-code", id, Some(&self.work));
        record.owner = Some("me".into());
        record.agent = Some("fixture".into());
        record.branch = Some(branch.into());
        link::write(&Store::at(self.home.join("store")), &record).unwrap();
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command
            .args(args)
            .current_dir(&self.work)
            .env("AGIT_HOME", &self.home)
            .env("AGIT_HUB_URL", "http://127.0.0.1:1")
            .env("AGIT_TUI", "0")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("AGIT_SESSION")
            .env_remove("AGIT_YES");
        for (variable, _) in agit::infra::runtime_session::ENV_SESSIONS {
            command.env_remove(variable);
        }
        command
    }
}

#[test]
fn directory_pins_cannot_choose_daily_write_targets() {
    let lab = Lab::new();
    lab.pin();
    fs::write(lab.repo.root().join("README.md"), "unsaved user file\n").unwrap();
    let before = lab.repo.git(&["rev-parse", "main"]).unwrap();
    for args in [
        vec!["push", "--dry-run"],
        vec!["commit", "-m", "must not commit"],
        vec!["share", "--public"],
    ] {
        let output = lab.command(&args).output().unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{args:?}: {stderr}");
        assert!(stderr.contains("AGIT_SESSION"), "{args:?}: {stderr}");
        assert_eq!(lab.repo.git(&["rev-parse", "main"]).unwrap(), before);
    }
    let explicit = lab
        .command(&[
            "commit",
            "me/fixture@main",
            "-m",
            "save explicit file line",
            "--",
            "README.md",
        ])
        .output()
        .unwrap();
    assert!(
        explicit.status.success(),
        "{}",
        String::from_utf8_lossy(&explicit.stderr)
    );
    assert_ne!(lab.repo.git(&["rev-parse", "main"]).unwrap(), before);
}

#[test]
fn bare_push_cannot_publish_the_only_repo_from_an_unrelated_directory() {
    let lab = Lab::new();
    let output = lab.command(&["push", "--dry-run"]).output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(
        stderr.contains("no explicit publish repository"),
        "{stderr}"
    );
}

#[test]
fn adoption_cannot_replace_explicit_process_identity() {
    let lab = Lab::new();
    lab.pin();
    lab.adopt("fixture-one", "work");
    for harness in [false, true] {
        let mut command = lab.command(&["share", "--public"]);
        if harness {
            command.env("CLAUDE_CODE_SESSION_ID", "fixture-one");
        }
        let output = command.output().unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{stderr}");
        assert!(stderr.contains("requires a valid AGIT_SESSION"), "{stderr}");
    }
    let head = lab.repo.git(&["rev-parse", "work"]).unwrap();
    let mut selected = lab.command(&["share", "--public"]);
    selected.env("AGIT_SESSION", "me/fixture@work");
    selected.env("CLAUDE_CODE_SESSION_ID", "fixture-one");
    let output = selected.output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(8), "{stderr}");
    assert!(
        stderr.contains(&format!(
            "share for VIEW of me/fixture@{} requires",
            &head[..12]
        )),
        "{stderr}"
    );

    selected.env("AGIT_SESSION", "me/fixture@main");
    let output = selected.output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("refusing the stale target"), "{stderr}");
    assert!(!stderr.contains("share for"), "{stderr}");
}
