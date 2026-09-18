#![cfg(all(feature = "cli", unix))]

use std::path::Path;
use std::process::{Command, Stdio};

#[path = "support/startup_cache.rs"]
mod startup_cache;

fn command(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
    command
        .current_dir(home)
        .env("AGIT_HOME", home)
        .env("AGIT_HUB_URL", "http://127.0.0.1:9")
        .env("AGIT_TELEMETRY", "off")
        .env_remove("AGIT_SESSION")
        .env_remove("AGIT_RC")
        .stdin(Stdio::null());
    command
}

struct Stop<'a>(&'a Path);
impl Drop for Stop<'_> {
    fn drop(&mut self) {
        let _ = command(self.0).args(["rc", "stop"]).output();
    }
}

struct OwnedChild(std::process::Child);

impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn legacy_socket_uncertainty_is_shared_by_startup_and_status() {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::net::UnixListener;

    let directory = tempfile::tempdir().unwrap();
    let home = directory.path();
    startup_cache::seed(home);
    let rc = home.join("desktop-rc");
    std::fs::create_dir_all(&rc).unwrap();
    let socket = agit::rc::control::socket_path_for(&rc);
    drop(UnixListener::bind(&socket).unwrap());
    let inode = std::fs::metadata(&socket).unwrap().ino();
    let pid = std::process::id().to_string();
    std::fs::write(rc.join("agitd.pid"), &pid).unwrap();

    for args in [
        ["rc", "local", "start", "--detach"].as_slice(),
        ["rc", "local", "status"].as_slice(),
        ["rc", "status"].as_slice(),
    ] {
        let output = command(home).args(args).output().unwrap();
        assert!(!output.status.success());
        let diagnostic = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            diagnostic.contains("cannot establish ownership"),
            "{diagnostic}"
        );
        assert!(!diagnostic.contains("no daemon is running"), "{diagnostic}");
        assert_eq!(std::fs::metadata(&socket).unwrap().ino(), inode);
        assert_eq!(std::fs::read_to_string(rc.join("agitd.pid")).unwrap(), pid);
    }
    std::fs::remove_file(socket).unwrap();
}

/// Kernel ownership must recover after a crash even when the diagnostic PID names a live,
/// unrelated process. Synthesizing that reuse avoids depending on the host PID allocator.
#[tokio::test]
async fn crashed_owner_recovers_with_a_reused_live_pid() {
    use agit::rc::control::{Presence, presence_in, socket_path_for};
    use std::time::{Duration, Instant};

    let directory = tempfile::tempdir().unwrap();
    let home = directory.path();
    startup_cache::seed(home);
    let rc = home.join("desktop-rc");
    let mut original = OwnedChild(
        command(home)
            .args(["rc", "local", "start"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if presence_in(&rc) == Presence::Running(original.0.id()) {
            break;
        }
        assert!(
            original.0.try_wait().unwrap().is_none(),
            "owner exited during startup"
        );
        assert!(
            Instant::now() < deadline,
            "owner did not publish control status"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    original.0.kill().unwrap();
    original.0.wait().unwrap();

    let mut unrelated = OwnedChild(Command::new("sleep").arg("60").spawn().unwrap());
    std::fs::write(rc.join("agitd.pid"), unrelated.0.id().to_string()).unwrap();
    assert!(socket_path_for(&rc).exists());
    assert_eq!(presence_in(&rc), Presence::Absent);
    let _cleanup = Stop(home);
    let start = tokio::time::timeout(
        Duration::from_secs(20),
        tokio::process::Command::from(command(home))
            .args(["rc", "local", "start", "--detach", "--json"])
            .output(),
    )
    .await
    .expect("recovery must be bounded")
    .unwrap();
    assert!(
        start.status.success(),
        "{}",
        String::from_utf8_lossy(&start.stderr)
    );
    let status = command(home)
        .args(["rc", "local", "status"])
        .output()
        .unwrap();
    assert!(status.status.success());
    let status: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    let pid = status["pid"].as_u64().unwrap() as u32;
    assert_ne!(pid, unrelated.0.id());
    assert_eq!(presence_in(&rc), Presence::Running(pid));
    assert!(
        unrelated.0.try_wait().unwrap().is_none(),
        "recovery must not signal the reused PID"
    );

    let stop = command(home).args(["rc", "stop"]).output().unwrap();
    assert!(stop.status.success());
    let deadline = Instant::now() + Duration::from_secs(15);
    while presence_in(&rc) != Presence::Absent {
        assert!(
            Instant::now() < deadline,
            "stopped owner retained ownership"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(unrelated.0.try_wait().unwrap().is_none());
    // Short-path sockets live outside the disposable home; remove only this fixture's paths.
    let socket = socket_path_for(&rc);
    std::fs::remove_file(socket.with_extension("rpc")).unwrap();
    std::fs::remove_file(socket).unwrap();
}

#[tokio::test]
async fn explicit_start_enables_inbound_without_pairing_and_reuses_the_owner_daemon() {
    let directory = tempfile::tempdir().unwrap();
    let home = directory.path();
    startup_cache::seed(home);
    let _cleanup = Stop(home);
    let local = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::process::Command::from(command(home))
            .args(["rc", "local", "start", "--detach", "--json"])
            .output(),
    )
    .await
    .expect("detached startup must release the caller's pipes")
    .unwrap();
    assert!(
        local.status.success(),
        "{}",
        String::from_utf8_lossy(&local.stderr)
    );
    let status = || {
        let result = command(home)
            .args(["rc", "local", "status"])
            .output()
            .unwrap();
        assert!(result.status.success());
        serde_json::from_slice::<serde_json::Value>(&result.stdout).unwrap()
    };
    let before = status();
    let pending = || {
        std::fs::read_dir(home.join("desktop-rc"))
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("cloud-inbound-")
            })
    };
    assert!(!pending(), "outbound-only startup must not enable inbound");
    let start = command(home)
        .args(["rc", "start", "--detach"])
        .output()
        .unwrap();
    assert!(
        start.status.success(),
        "{}",
        String::from_utf8_lossy(&start.stderr)
    );
    assert!(String::from_utf8_lossy(&start.stdout).contains("enabled for your account"));
    assert!(
        pending(),
        "unavailable Cloud registration must remain retryable"
    );
    assert_eq!(
        before["pid"],
        status()["pid"],
        "explicit startup must reuse the owner daemon"
    );
    let stopped = command(home).args(["rc", "stop"]).output().unwrap();
    assert!(stopped.status.success());
    assert!(
        !command(home)
            .args(["rc", "pair"])
            .output()
            .unwrap()
            .status
            .success()
    );
}
