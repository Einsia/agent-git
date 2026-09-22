#![cfg(feature = "cli")]

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
        .env("CI", "1")
        .env(
            "AGIT_SECRETS_KEYSTORE",
            if cfg!(windows) { "os" } else { "file" },
        )
        .env_remove("AGIT_SESSION")
        .env_remove("AGIT_RC")
        .stdin(Stdio::null());
    command
}

struct Stop<'a>(&'a Path);
impl Drop for Stop<'_> {
    fn drop(&mut self) {
        let _ = command(self.0).args(["rc", "stop"]).output();
        #[cfg(windows)]
        {
            use agit::domain::secret_filter::KeyStore;
            if let Ok(bytes) = std::fs::read(self.0.join("secret-filter/vault.json"))
                && let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes)
                && let Some(id) = value["vault_id"].as_str()
            {
                let _ = agit::domain::secret_filter::OsKeyStore.delete(id);
            }
        }
    }
}

#[cfg(unix)]
struct OwnedChild(std::process::Child);

#[cfg(unix)]
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(unix)]
#[tokio::test]
async fn stopped_legacy_socket_requires_explicit_recovery_before_restart() {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::time::{Duration, Instant};

    let directory = tempfile::tempdir().unwrap();
    let home = &directory.path().join("legacy home");
    startup_cache::seed(home);
    let rc = home.join("desktop-rc");
    std::fs::create_dir_all(&rc).unwrap();
    let socket = agit::rc::control::socket_path_for(&rc);
    let rpc = socket.with_extension("rpc");
    drop(UnixListener::bind(&socket).unwrap());
    drop(UnixListener::bind(&rpc).unwrap());
    let inode = std::fs::metadata(&socket).unwrap().ino();
    let pid = std::process::id().to_string();
    std::fs::write(rc.join("agitd.pid"), &pid).unwrap();
    let project = home.join("project");
    std::fs::create_dir(&project).unwrap();
    let bindings = serde_json::to_vec(&serde_json::json!({
        "workspaces": {"local-owner": {"fixture": project}}
    }))
    .unwrap();
    std::fs::write(rc.join("workspaces.json"), &bindings).unwrap();
    let other_rc = home.join("rc");
    std::fs::create_dir(&other_rc).unwrap();
    let other_socket = agit::rc::control::socket_path_for(&other_rc);
    let other_listener = UnixListener::bind(&other_socket).unwrap();
    let other_inode = std::fs::metadata(&other_socket).unwrap().ino();

    for args in [
        ["rc", "local", "start"].as_slice(),
        ["rc", "local", "start", "--detach"].as_slice(),
        ["rc", "local", "bridge", "--ensure"].as_slice(),
        ["rc", "local", "restart", "--if-idle"].as_slice(),
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
        assert!(
            diagnostic.contains("recover --confirm-stopped"),
            "{diagnostic}"
        );
        assert!(diagnostic.contains("AGIT_HOME="), "{diagnostic}");
        assert_eq!(std::fs::metadata(&socket).unwrap().ino(), inode);
        assert_eq!(std::fs::read_to_string(rc.join("agitd.pid")).unwrap(), pid);
    }
    let lock = rc.join("agitd.lock");
    let lock_inode = std::fs::metadata(&lock).unwrap().ino();
    assert!(std::fs::read(&lock).unwrap().is_empty());
    let unconfirmed = command(home)
        .args(["rc", "local", "recover"])
        .output()
        .unwrap();
    assert!(!unconfirmed.status.success());
    assert_eq!(std::fs::metadata(&socket).unwrap().ino(), inode);

    let recovered = command(home)
        .args(["rc", "local", "recover", "--confirm-stopped"])
        .output()
        .unwrap();
    assert!(recovered.status.success(), "{recovered:?}");
    let recovered: serde_json::Value = serde_json::from_slice(&recovered.stdout).unwrap();
    assert_eq!(recovered["status"], "recovered");
    assert_eq!(recovered["socket_removed"], true);
    assert!(!socket.exists());
    assert!(rpc.exists());
    assert_eq!(std::fs::metadata(&lock).unwrap().ino(), lock_inode);
    assert!(std::fs::read(&lock).unwrap().is_empty());
    assert_eq!(std::fs::read_to_string(rc.join("agitd.pid")).unwrap(), pid);
    assert_eq!(std::fs::read(rc.join("workspaces.json")).unwrap(), bindings);
    assert_eq!(std::fs::metadata(&other_socket).unwrap().ino(), other_inode);

    let _cleanup = Stop(home);
    let started = tokio::time::timeout(
        Duration::from_secs(25),
        tokio::process::Command::from(command(home))
            .args(["rc", "local", "start", "--detach"])
            .output(),
    )
    .await
    .expect("startup after recovery must be bounded")
    .unwrap();
    assert!(started.status.success(), "{started:?}");
    let mut stream = UnixStream::connect(&rpc).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"workspace.list\",\"params\":{}}\n")
        .unwrap();
    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response).unwrap();
    let response: serde_json::Value = serde_json::from_str(&response).unwrap();
    assert_eq!(
        response["result"]["workspaces"][0]["projects"][0]["project_id"],
        "fixture"
    );
    assert_eq!(std::fs::read(rc.join("workspaces.json")).unwrap(), bindings);
    assert_eq!(std::fs::metadata(&lock).unwrap().ino(), lock_inode);
    assert!(!std::fs::read(&lock).unwrap().is_empty());
    assert_eq!(std::fs::metadata(&other_socket).unwrap().ino(), other_inode);
    let live_inode = std::fs::metadata(&socket).unwrap().ino();
    let live_recovery = command(home)
        .args(["rc", "local", "recover", "--confirm-stopped"])
        .output()
        .unwrap();
    assert!(!live_recovery.status.success());
    assert!(
        String::from_utf8_lossy(&live_recovery.stderr)
            .contains("cannot acquire daemon ownership lock")
    );
    assert_eq!(std::fs::metadata(&socket).unwrap().ino(), live_inode);

    assert!(
        command(home)
            .args(["rc", "local", "stop"])
            .output()
            .unwrap()
            .status
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    while agit::rc::control::presence_in(&rc) != agit::rc::control::Presence::Absent {
        assert!(Instant::now() < deadline, "recovered daemon did not stop");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    drop(other_listener);
    for path in [socket, rpc, other_socket] {
        std::fs::remove_file(path).unwrap();
    }
}

/// Kernel ownership must recover after a crash even when the diagnostic PID names a live,
/// unrelated process. Synthesizing that reuse avoids depending on the host PID allocator.
#[cfg(unix)]
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

#[cfg(unix)]
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
    assert_eq!(
        agit::rc::control::presence_in(&home.join("desktop-rc")),
        agit::rc::control::Presence::Absent,
        "a completed stop must release the daemon's lifetime ownership"
    );
    let restarted = command(home)
        .args(["rc", "local", "start", "--detach"])
        .output()
        .unwrap();
    assert!(restarted.status.success(), "{restarted:?}");
    assert_ne!(
        before["identity"]["instance_id"],
        status()["identity"]["instance_id"]
    );
    assert!(
        !command(home)
            .args(["rc", "pair"])
            .output()
            .unwrap()
            .status
            .success()
    );
}

#[tokio::test]
async fn bridge_waits_for_another_process_to_release_the_lifecycle_lock() {
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::time::timeout;

    let directory = tempfile::tempdir().unwrap();
    let home = directory.path().join("home");
    startup_cache::seed(&home);
    let _cleanup = Stop(&home);
    let started = command(&home)
        .args(["rc", "local", "start", "--detach"])
        .output()
        .unwrap();
    assert!(started.status.success(), "{started:?}");
    let status = command(&home)
        .args(["rc", "local", "status"])
        .output()
        .unwrap();
    assert!(status.status.success(), "{status:?}");
    let before: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();

    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(home.join("desktop-rc/lifecycle.lock"))
        .unwrap();
    fs2::FileExt::lock_exclusive(&lock).unwrap();
    let mut bridge = tokio::process::Command::from(command(&home))
        .args(["rc", "local", "bridge", "--ensure"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    bridge
        .stdin
        .as_mut()
        .unwrap()
        .write_all(
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"machine.describe\",\"params\":{}}\n",
        )
        .await
        .unwrap();
    let mut replies = BufReader::new(bridge.stdout.take().unwrap()).lines();
    assert!(
        timeout(Duration::from_secs(1), replies.next_line())
            .await
            .is_err(),
        "a contended bridge must wait without replying or closing stdout"
    );
    assert!(
        bridge.try_wait().unwrap().is_none(),
        "lock contention must not terminate the bridge"
    );

    fs2::FileExt::unlock(&lock).unwrap();
    let reply = timeout(Duration::from_secs(20), replies.next_line())
        .await
        .expect("bridge must attach after the lifecycle lock is released")
        .unwrap()
        .expect("bridge must deliver the pending RPC reply");
    let reply: serde_json::Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(reply["id"], 1);
    assert!(reply["result"]["instance_id"].is_string(), "{reply}");
    assert_eq!(
        reply["result"]["instance_id"], before["identity"]["instance_id"],
        "the waiting bridge must reuse the compatible daemon"
    );
    bridge.kill().await.unwrap();
}
