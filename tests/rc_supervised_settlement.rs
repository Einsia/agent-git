#[cfg(unix)]
mod unix {
    use std::path::Path;
    use std::process::Command;

    fn base_command(root: &Path, args: &[&str]) -> Command {
        let cwd = root.join("workspace");
        std::fs::create_dir_all(&cwd).unwrap();
        let home = root.join("agit-home");
        std::fs::create_dir_all(&home).unwrap();
        // `main` takes this migration lock before dispatching any subcommand.
        // Seed the empty lock so the assertions below measure the requested
        // command, not that process-wide startup preflight.
        std::fs::write(home.join("layout-v1.lock"), []).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command
            .args(args)
            .current_dir(&cwd)
            .env("AGIT_HOME", home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0");
        command
    }

    fn run_args(root: &Path, args: &[&str], supervised: bool) -> std::process::Output {
        let mut command = base_command(root, args);
        if supervised {
            command.env("AGIT_RC_SUPERVISED_HOOK", "1");
        }
        command.output().unwrap()
    }

    fn run(root: &Path, command: &str) -> std::process::Output {
        run_args(root, &[command], true)
    }

    /// Exercise the real CLI entry points in isolated child processes. Neither
    /// command may reach login, context resolution, a local repository, or the
    /// network while it carries the harness marker.
    #[test]
    fn a_supervised_harness_cannot_commit_or_push_outside_the_live_lease() {
        for command in ["commit", "push"] {
            let root = tempfile::tempdir().unwrap();
            let output = run(root.path(), command);
            assert!(
                !output.status.success(),
                "`agit {command}` unexpectedly wrote"
            );
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                text.contains("owned by the RC supervisor"),
                "`agit {command}` reached a different path: {text}"
            );
            assert!(
                !root.path().join("agit-home/repos").exists()
                    && !root.path().join("agit-home/store").exists(),
                "`agit {command}` touched repository state before refusing"
            );
        }
    }

    /// The private supervisor entry point keeps failures observable. In
    /// particular, disabling automatic settlement must not look like a
    /// successful hook no-op that the supervisor could follow with a push.
    #[test]
    fn strict_supervisor_commit_fails_when_auto_settlement_is_disabled() {
        let root = tempfile::tempdir().unwrap();
        assert!(
            run_args(root.path(), &["config", "commit.auto", "false"], false)
                .status
                .success()
        );

        let output = run_args(root.path(), &["commit", "--from-supervisor"], false);
        assert!(!output.status.success());
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            text.contains("automatic settlement is disabled"),
            "strict settlement reached a later path: {text}"
        );
        assert!(
            !root.path().join("agit-home/repos").exists()
                && !root.path().join("agit-home/store").exists(),
            "a disabled strict settlement touched repository state"
        );
    }

    /// Exercise the complete private settlement contract, not merely the
    /// predicate that consumes its stdout. The supervisor relies on this exact
    /// child command to create a real turn commit and atomically report the
    /// resulting SHA through `AGIT_RC_SUPERVISOR_COMMIT_RESULT`.
    #[test]
    fn strict_supervisor_commit_writes_the_real_new_head_to_its_result_file() {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("workspace");
        let agit_home = root.path().join("agit-home");
        let runtime_home = root.path().join("runtime-home");
        let repo = agit_home.join("repos/alice/photo");
        let session_id = "8d5c2a51-8af0-4e5f-9f9e-731acee20c17";
        let agent_id = "6f35cd22-85ae-4ed5-a106-2d9069d6b346";
        let branch = "s/integration";
        let hub = "https://hub.test";
        let result_path = root.path().join("settled-sha");

        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .args(["--no-replace-objects", "-C"])
                .arg(&repo)
                .args(args)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_TERMINAL_PROMPT", "0")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap().trim().to_string()
        };
        git(&["init", "-q"]);
        git(&["config", "commit.gpgsign", "false"]);
        let remote_identity = serde_json::json!({
            "hub": hub,
            "agent_id": agent_id,
        })
        .to_string();
        git(&["config", "agit.remoteIdentity", &remote_identity]);

        let credentials = serde_json::json!({
            "username": "alice",
            "email": "alice@example.test",
            "access_token": "test-access-token",
            "access_expires_at": "2100-01-01T00:00:00Z",
            "refresh_token": "test-refresh-token",
            "refresh_expires_at": "2100-01-01T00:00:00Z",
        });
        let credentials_path = agit_home.join("credentials/hub.test.json");
        std::fs::create_dir_all(credentials_path.parent().unwrap()).unwrap();
        std::fs::write(
            &credentials_path,
            format!("{}\n", serde_json::to_string_pretty(&credentials).unwrap()),
        )
        .unwrap();

        let link_path = agit_home
            .join("store/claude-code")
            .join(format!("{session_id}.json"));
        std::fs::create_dir_all(link_path.parent().unwrap()).unwrap();
        let link = serde_json::json!({
            "cwd": cwd,
            "agent": "photo",
            "branch": branch,
        });
        std::fs::write(
            &link_path,
            format!("{}\n", serde_json::to_string_pretty(&link).unwrap()),
        )
        .unwrap();

        let transcript_dir = runtime_home
            .join(".claude/projects")
            .join(agit::adapter::claude_code::slug_for(&cwd));
        std::fs::create_dir_all(&transcript_dir).unwrap();
        let user = serde_json::json!({
            "type": "user",
            "sessionId": session_id,
            "uuid": "cb71e9d8-a3d2-4c70-930d-98def38d9f54",
            "cwd": cwd,
            "message": {"role": "user", "content": "settle this turn"},
        });
        let assistant = serde_json::json!({
            "type": "assistant",
            "sessionId": session_id,
            "uuid": "88bfaf5e-4107-4cf2-9038-a2a891d171f1",
            "parentUuid": "cb71e9d8-a3d2-4c70-930d-98def38d9f54",
            "cwd": cwd,
            "message": {
                "role": "assistant",
                "content": [{"type": "text", "text": "done"}],
            },
        });
        std::fs::write(
            transcript_dir.join(format!("{session_id}.jsonl")),
            format!("{}\n{}\n", user, assistant),
        )
        .unwrap();

        let output = base_command(root.path(), &["commit", "--from-supervisor"])
            .env("HOME", &runtime_home)
            .env("AGIT_HUB_URL", hub)
            .env("AGIT_SESSION", format!("alice/photo@{branch}"))
            .env("AGIT_EXPECTED_AGENT_ID", agent_id)
            .env("AGIT_RC_SUPERVISOR_COMMIT_RESULT", &result_path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "strict settlement failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        let reported = std::fs::read_to_string(&result_path)
            .expect("the strict child must write its result")
            .trim()
            .to_string();
        let head = git(&["rev-parse", "HEAD"]);
        assert_eq!(reported, head, "the side channel must name the real HEAD");
        assert_eq!(reported.len(), 40, "the result must be a full commit SHA");
        assert_eq!(git(&["rev-list", "--count", "HEAD"]), "1");
        assert_eq!(git(&["branch", "--show-current"]), branch);
    }
}
