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

    #[test]
    fn rc_land_migrates_remote_history_after_startup_completion() {
        use std::io::{Read as _, Write as _};

        let fixture = tempfile::tempdir().unwrap();
        let home = fixture.path().join("home");
        let repo = repo_with_layout(&home.join("repos/owner/history"), LayoutVersion::V1);
        let current = repo.git(&["rev-parse", "HEAD"]).unwrap();
        let mut legacy = Meta::new_session_line("codex".into(), "/project".into());
        legacy.layout = LayoutVersion::V0;
        meta::write(repo.root(), &legacy).unwrap();
        fs::write(repo.root().join("LOG.jsonl"), []).unwrap();
        fs::write(repo.root().join("VIEW.jsonl"), []).unwrap();
        repo.add_all().unwrap();
        repo.commit("legacy remote session").unwrap();
        let remote = repo.git(&["rev-parse", "HEAD"]).unwrap();
        repo.git(&["update-ref", "refs/remotes/origin/session", &remote])
            .unwrap();
        repo.git(&["reset", "--hard", &current]).unwrap();

        let startup = Command::new(env!("CARGO_BIN_EXE_agit"))
            .args(["config", "hub.url"])
            .env("AGIT_HOME", &home)
            .output()
            .unwrap();
        assert!(startup.status.success());
        assert!(home.join("layout-v1.complete").is_file());

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let agent_id = "00000000-0000-0000-0000-000000000001";
        agit::hub::identity::pin(
            &repo,
            &agit::hub::identity::RemoteIdentity::new(&base, agent_id).unwrap(),
        )
        .unwrap();
        let hub = std::thread::spawn(move || {
            listener.set_nonblocking(true).unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            let mut socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "RC did not contact the hub"
                        );
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(error) => panic!("cannot accept the RC request: {error}"),
                }
            };
            socket.set_nonblocking(false).unwrap();
            socket
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 4096];
            while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let size = socket.read(&mut chunk).unwrap();
                if size == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..size]);
            }
            let request = String::from_utf8(request).unwrap();
            assert!(request.starts_with("GET /api/agents/owner/history "));
            let body = serde_json::json!({
                "agent_id": agent_id,
                "owner": "owner",
                "name": "history",
                "clone_url": "unused",
                "visibility": "public"
            })
            .to_string();
            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        });
        let output = Command::new(env!("CARGO_BIN_EXE_agit"))
            .args(agit::commands::rc::land_argv(
                "owner/history",
                agent_id,
                "session",
                "codex",
                "test-rc-session",
                fixture.path().to_str().unwrap(),
            ))
            .current_dir(fixture.path())
            .env("AGIT_HOME", &home)
            .env("AGIT_HUB_URL", &base)
            .env_remove("AGIT_SESSION")
            .env_remove("AGIT_EXPECTED_AGENT_ID")
            .output()
            .unwrap();
        hub.join().unwrap();
        assert!(
            output.status.success(),
            "stdout={}\nstderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            meta::read_at_ref(&repo, "refs/heads/session")
                .unwrap()
                .layout,
            LayoutVersion::V1
        );
        assert_eq!(
            meta::read_at_ref(&repo, "refs/remotes/origin/session")
                .unwrap()
                .layout,
            LayoutVersion::V0
        );
        assert!(
            fs::read_dir(home.join("layout-v1-recovery"))
                .unwrap()
                .next()
                .is_none()
        );
    }
}
