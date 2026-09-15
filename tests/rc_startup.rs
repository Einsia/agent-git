#![cfg(feature = "cli")]

use agit::protocol::{Frame, RcRegisterResult, method};
use futures_util::{SinkExt, StreamExt};
use std::io::{Read, Seek};
use std::path::Path;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

#[path = "support/startup_cache.rs"]
mod startup_cache;

#[cfg(windows)]
#[path = "../src/infra/windows_security.rs"]
#[allow(dead_code)]
mod security;

fn command(home: &Path, hub: &str) -> std::process::Command {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_agit"));
    command
        .current_dir(home)
        .env("AGIT_HOME", home)
        .env("AGIT_HUB_URL", hub)
        .env("NO_PROXY", "*")
        .env("AGIT_TELEMETRY", "off")
        .env_remove("AGIT_SESSION")
        .env_remove("AGIT_RC")
        .stdin(std::process::Stdio::null());
    command
}

fn pair(home: &Path, hub: &str) {
    let directory = home.join("rc/connections");
    #[cfg(windows)]
    {
        security::private_directory(home).unwrap();
        security::private_directory(&home.join("rc")).unwrap();
        security::private_directory(&directory).unwrap();
    }
    std::fs::create_dir_all(&directory).unwrap();
    startup_cache::seed(home);
    let key = agit::infra::config::hub_host_key(hub).unwrap();
    let path = directory.join(format!("{key}.json"));
    let body = serde_json::to_vec(&serde_json::json!({
        "connection_id": "startup-test", "token": "synthetic-startup-token", "hub": hub,
        "created_at": "2026-09-15T00:00:00Z"
    }))
    .unwrap();
    #[cfg(windows)]
    security::write_private_file(&path, &body).unwrap();
    #[cfg(not(windows))]
    std::fs::write(path, body).unwrap();
}

struct Cleanup<'a>(&'a Path, &'a str);

async fn captured_output(
    mut command: tokio::process::Command,
) -> std::io::Result<std::process::Output> {
    let mut stdout = tempfile::tempfile()?;
    let mut stderr = tempfile::tempfile()?;
    command
        .stdout(stdout.try_clone()?)
        .stderr(stderr.try_clone()?);
    let status = command.spawn()?.wait().await?;
    // Detached descendants can inherit output handles; readiness depends on the starter's exit.
    let read = |file: &mut std::fs::File| -> std::io::Result<Vec<u8>> {
        file.rewind()?;
        let mut bytes = Vec::new();
        file.take(1024 * 1024).read_to_end(&mut bytes)?;
        Ok(bytes)
    };
    Ok(std::process::Output {
        status,
        stdout: read(&mut stdout)?,
        stderr: read(&mut stderr)?,
    })
}

impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        let _ = command(self.0, self.1).args(["rc", "stop"]).output();
    }
}

#[tokio::test]
async fn detached_start_waits_for_registration_and_retains_private_diagnostics() {
    let temporary = tempfile::tempdir().unwrap();
    let home = temporary.path().join("home");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hub = format!("http://{}", listener.local_addr().unwrap());
    pair(home.as_path(), &hub);
    let _cleanup = Cleanup(home.as_path(), &hub);
    let mut start = tokio::process::Command::from(command(home.as_path(), &hub));
    start
        .args(["rc", "start", "--detach"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let output = captured_output(start);
    tokio::pin!(output);
    let (stream, _) = tokio::select! {
        result = &mut output => {
            let result = result.unwrap();
            panic!("startup exited before connecting: {}\n{}",
                String::from_utf8_lossy(&result.stdout), String::from_utf8_lossy(&result.stderr));
        }
        accepted = tokio::time::timeout(Duration::from_secs(15), listener.accept()) => {
            accepted.expect("daemon must reach the mock Hub").unwrap()
        }
    };
    let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
    let register = socket.next().await.unwrap().unwrap();
    let register = Frame::from_json(register.to_text().unwrap()).unwrap();
    assert_eq!(register.method(), method::RC_REGISTER);
    assert!(
        tokio::time::timeout(Duration::from_millis(300), &mut output)
            .await
            .is_err(),
        "spawning the daemon or accepting its socket cannot establish registration"
    );
    socket
        .send(Message::Text(
            Frame::response(
                register.id.unwrap(),
                RcRegisterResult {
                    connection_id: "startup-test".into(),
                    accepted_features: vec![],
                    workspaces: vec![],
                    persisted_seq: Default::default(),
                    server_time: "2026-09-15T00:00:00Z".into(),
                },
            )
            .to_json()
            .into(),
        ))
        .await
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(10), &mut output)
        .await
        .unwrap()
        .unwrap();
    let stdout = String::from_utf8(result.stdout).unwrap();
    assert!(
        result.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(stdout.contains("Hub registered; remote control ready"));
    assert!(stdout.contains(&format!("{hub}/workspaces")));
    let logs: Vec<_> = std::fs::read_dir(home.as_path().join("rc"))
        .unwrap()
        .map(Result::unwrap)
        .filter(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "log")
        })
        .collect();
    assert_eq!(logs.len(), 1);
    let path = logs[0].path();
    assert!(stdout.contains(path.to_str().unwrap()));
    let log = std::fs::read_to_string(&path).unwrap();
    assert!(log.contains("agitd: connected"), "{log}");
    assert!(!log.contains("synthetic-startup-token"));
    #[cfg(windows)]
    security::validate_path(&path, false, true).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[tokio::test]
async fn detached_start_reports_an_early_daemon_failure_with_its_log() {
    let temporary = tempfile::tempdir().unwrap();
    let home = temporary.path().join("home");
    let hub = "http://127.0.0.1:9";
    pair(home.as_path(), hub);
    let _cleanup = Cleanup(home.as_path(), hub);
    std::fs::write(
        home.as_path().join("rc/sessions.fail-closed.json"),
        b"invalid roster",
    )
    .unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(15),
        tokio::process::Command::from(command(home.as_path(), hub))
            .args(["rc", "start", "--detach"])
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    let stdout = String::from_utf8_lossy(&result.stdout);
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(!result.status.success(), "{stdout}");
    assert!(
        format!("{stdout}{stderr}").contains("daemon exited before Hub readiness"),
        "{stdout}\n{stderr}"
    );
    assert!(stdout.contains(".log"));
    assert!(!stdout.contains("remote control ready"));
    let log = std::fs::read_dir(home.as_path().join("rc"))
        .unwrap()
        .map(Result::unwrap)
        .find(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "log")
        })
        .unwrap()
        .path();
    assert!(
        std::fs::read_to_string(log)
            .unwrap()
            .contains("fail-closed roster snapshot")
    );
}

#[tokio::test]
async fn registration_rejection_is_logged_and_startup_wait_can_be_stopped() {
    let temporary = tempfile::tempdir().unwrap();
    let home = temporary.path().join("home");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hub = format!("http://{}", listener.local_addr().unwrap());
    pair(home.as_path(), &hub);
    let _cleanup = Cleanup(home.as_path(), &hub);
    let mut start = tokio::process::Command::from(command(home.as_path(), &hub));
    start
        .args(["rc", "start", "--detach"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let output = captured_output(start);
    tokio::pin!(output);
    let (stream, _) = tokio::select! {
        result = &mut output => {
            let result = result.unwrap();
            panic!("startup exited before connecting: {}\n{}",
                String::from_utf8_lossy(&result.stdout), String::from_utf8_lossy(&result.stderr));
        }
        accepted = tokio::time::timeout(Duration::from_secs(15), listener.accept()) => {
            accepted.expect("daemon must reach the mock Hub").unwrap()
        }
    };
    let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
    let register = socket.next().await.unwrap().unwrap();
    let register = Frame::from_json(register.to_text().unwrap()).unwrap();
    socket
        .send(Message::Text(
            serde_json::json!({
                "jsonrpc": "2.0", "id": register.id,
                "error": {"code": 401, "message": "synthetic registration refused"}
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(300), &mut output)
            .await
            .is_err(),
        "rejected registration cannot establish readiness"
    );
    let stop = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::from(command(home.as_path(), &hub))
            .args(["rc", "stop"])
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(stop.status.success());
    let result = tokio::time::timeout(Duration::from_secs(5), &mut output)
        .await
        .unwrap()
        .unwrap();
    assert!(!result.status.success());
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(
        text.contains("daemon exited before Hub readiness"),
        "{text}"
    );
    assert!(!text.contains("remote control ready"));
    let log = std::fs::read_dir(home.as_path().join("rc"))
        .unwrap()
        .map(Result::unwrap)
        .find(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "log")
        })
        .unwrap()
        .path();
    assert!(
        std::fs::read_to_string(log)
            .unwrap()
            .contains("synthetic registration refused")
    );
}
