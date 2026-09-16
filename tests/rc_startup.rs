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
