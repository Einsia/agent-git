use agit::domain::{meta, repo::Repo, storage, transcript};
use std::{fs, process::Command};

#[test]
fn sharing_follows_the_selected_branch_instead_of_repo_recency() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("agit");
    let work = tmp.path().join("work");
    fs::create_dir_all(&work).unwrap();
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
    let repo = Repo::init(&home.join("repos/me/paper")).unwrap();
    repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
    meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
    repo.add_all().unwrap();
    repo.commit("file line").unwrap();
    for (branch, digit) in [("selected", 'a'), ("other", 'b')] {
        repo.git(&["branch", branch, "main"]).unwrap();
        let tree = agit::commands::worktree::checkout(&repo, branch).unwrap();
        let id = format!("agit-{}", digit.to_string().repeat(40));
        let raw = "{\"type\":\"user\",\"sessionId\":\"fixture\",\"message\":{\"role\":\"user\",\"content\":\"A harmless fixture conversation\"}}\n";
        let envelope = transcript::wrap_lines(raw, "claude-code", &id);
        storage::write_snapshot(tree.root(), &envelope, &envelope).unwrap();
        meta::write(
            tree.root(),
            &meta::Meta::new(id, "claude-code".into(), work.display().to_string()),
        )
        .unwrap();
        tree.add_all().unwrap();
        tree.commit("settled fixture").unwrap();
    }

    let selected = repo.git(&["rev-parse", "selected"]).unwrap();
    for target in [None, Some("@"), Some("me/paper@selected")] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command
            .args(["share", "--public"])
            .current_dir(&work)
            .env("AGIT_HOME", &home)
            .env("AGIT_HUB_URL", hub)
            .env("AGIT_SESSION", "me/paper@selected")
            .env("AGIT_TUI", "0")
            .env_remove("AGIT_YES");
        for (variable, _) in agit::infra::runtime_session::ENV_SESSIONS {
            command.env_remove(variable);
        }
        if let Some(target) = target {
            command.arg(target);
        }
        let output = command.output().unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(8), "{target:?}: {stderr}");
        assert!(
            stderr.contains(&format!(
                "share for VIEW of me/paper@{} requires",
                &selected[..12]
            )),
            "{target:?}: {stderr}"
        );
    }
}
