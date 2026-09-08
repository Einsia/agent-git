//! Stored credentials must remain bound to their recorded authority through every CLI path.

use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

const TOKEN: &str = "SYNTHETIC-SOURCE-ACCESS-DO-NOT-USE";

#[derive(Clone, Debug)]
struct Request {
    target: String,
    authorization: Option<String>,
}

struct FakeHub {
    base: String,
    requests: Arc<Mutex<Vec<Request>>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl FakeHub {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let seen = requests.clone();
        let stopped = stop.clone();
        let worker = std::thread::spawn(move || {
            while !stopped.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((socket, _)) => serve(socket, &seen),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("synthetic hub cannot accept: {error}"),
                }
            }
        });
        Self {
            base,
            requests,
            stop,
            worker: Some(worker),
        }
    }

    fn uppercase(&self) -> String {
        self.base.replacen("http://", "HTTP://", 1)
    }

    fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for FakeHub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
            && !std::thread::panicking()
        {
            panic!("synthetic hub worker failed");
        }
    }
}

fn serve(mut socket: TcpStream, requests: &Mutex<Vec<Request>>) {
    socket.set_nonblocking(false).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    socket
        .set_write_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut bytes = Vec::new();
    let mut buffer = [0; 4096];
    let (header_end, body_len) = loop {
        let count = socket.read(&mut buffer).unwrap();
        if count == 0 {
            return;
        }
        bytes.extend_from_slice(&buffer[..count]);
        assert!(
            bytes.len() <= 64 * 1024,
            "synthetic request exceeds its bound"
        );
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            let header = String::from_utf8_lossy(&bytes[..end]);
            let length = header
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .map(|(_, value)| value.trim().parse::<usize>().unwrap())
                .unwrap_or(0);
            break (end, length);
        }
    };
    assert!(
        body_len <= 64 * 1024,
        "synthetic request body exceeds its bound"
    );
    while bytes.len() < header_end + 4 + body_len {
        let count = socket.read(&mut buffer).unwrap();
        if count == 0 {
            return;
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    let header = String::from_utf8_lossy(&bytes[..header_end]);
    let target = header
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .to_owned();
    let authorization = header
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
        .map(|(_, value)| value.trim().to_owned());
    requests.lock().unwrap().push(Request {
        target: target.clone(),
        authorization,
    });
    let body = if target.ends_with("/api/auth/me") {
        r#"{"username":"fixture-user","email":null}"#
    } else {
        "{}"
    };
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = socket.write_all(response.as_bytes());
}

struct Lab {
    _root: tempfile::TempDir,
    user_home: PathBuf,
    agit_home: PathBuf,
    work: PathBuf,
    git_config: PathBuf,
}

impl Lab {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let user_home = root.path().join("user");
        let agit_home = root.path().join("agit");
        let work = root.path().join("work");
        for path in [&user_home, &agit_home, &work] {
            fs::create_dir_all(path).unwrap();
        }
        fs::create_dir_all(agit_home.join("credentials")).unwrap();
        fs::write(agit_home.join("layout-v1.complete"), b"1\n").unwrap();
        let git_config = root.path().join("gitconfig");
        fs::write(&git_config, b"[commit]\n\tgpgsign = false\n").unwrap();
        Self {
            _root: root,
            user_home,
            agit_home,
            work,
            git_config,
        }
    }

    fn command(&self, hub: &str, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command
            .args(args)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.user_home)
            .env("USERPROFILE", &self.user_home)
            .env("AGIT_HOME", &self.agit_home)
            .env("AGIT_HUB_URL", hub)
            .env("AGIT_SECRETS_KEYSTORE", "file")
            .env("AGIT_TUI", "0")
            .env("CI", "1")
            .env("NO_COLOR", "1")
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env("no_proxy", "127.0.0.1,localhost")
            .env("GIT_CONFIG_GLOBAL", &self.git_config)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ALLOW_PROTOCOL", "")
            .current_dir(&self.work);
        if let Some(system_root) = std::env::var_os("SystemRoot") {
            command.env("SystemRoot", system_root);
        }
        command
    }

    fn run(&self, hub: &str, args: &[&str]) -> Output {
        self.command(hub, args).output().unwrap()
    }

    fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.agit_home.join("credentials").join(name);
        fs::write(&path, bytes).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        path
    }

    fn credentials(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        fs::read_dir(self.agit_home.join("credentials"))
            .unwrap()
            .map(|entry| {
                let path = entry.unwrap().path();
                (path.file_name().unwrap().into(), fs::read(path).unwrap())
            })
            .collect()
    }
}

fn credential(hub: Option<&str>) -> Value {
    json!({
        "username": "fixture-user",
        "email": null,
        "hub": hub,
        "access_token": TOKEN,
        "refresh_token": "SYNTHETIC-SOURCE-REFRESH-DO-NOT-USE",
        "access_expires_at": "2099-01-01T00:00:00Z",
        "refresh_expires_at": "2099-02-01T00:00:00Z"
    })
}

fn legacy_filename(hub: &str) -> String {
    let value = hub.trim().trim_end_matches('/');
    let authority = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))
        .unwrap_or(value)
        .split('/')
        .next()
        .unwrap();
    let key: String = authority
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '.' {
                ch
            } else {
                '_'
            }
        })
        .collect();
    format!("{key}.json")
}

fn assert_auth_refused(output: &Output, source: &FakeHub, destination: &FakeHub) {
    let requests = destination.requests();
    assert!(
        requests.is_empty(),
        "a rejected authority received requests: {requests:?}"
    );
    assert!(
        source.requests().is_empty(),
        "an unrelated source was contacted"
    );
    assert_eq!(output.status.code(), Some(5), "{output:?}");
}

/// A colliding legacy filename cannot authorize a different explicit port.
#[test]
fn uppercase_hub_selection_never_sends_another_authoritys_legacy_token() {
    let source = FakeHub::new();
    let destination = FakeHub::new();
    let lab = Lab::new();
    lab.write(
        "HTTP_.json",
        &serde_json::to_vec(&credential(Some(&source.uppercase()))).unwrap(),
    );
    let before = lab.credentials();
    let control = lab.run(&destination.base, &["whoami", "--check"]);
    assert_auth_refused(&control, &source, &destination);
    let output = lab.run(&destination.uppercase(), &["whoami", "--check"]);
    assert_auth_refused(&output, &source, &destination);
    assert_eq!(
        lab.credentials(),
        before,
        "reading must preserve legacy files"
    );
}

/// Matching authority metadata permits legacy reuse without rewriting its stored bytes.
#[test]
fn legacy_metadata_allows_same_authority_spelling_and_mount_changes() {
    for uppercase in [false, true] {
        let hub = FakeHub::new();
        let lab = Lab::new();
        let stored = format!("{}/saved", hub.base);
        let requested = format!(
            "{}/active",
            if uppercase {
                hub.uppercase()
            } else {
                hub.base.clone()
            }
        );
        let bytes = serde_json::to_vec_pretty(&credential(Some(&stored))).unwrap();
        let path = lab.write(&legacy_filename(&stored), &bytes);
        let before = lab.credentials();
        let output = lab.run(&requested, &["whoami", "--check"]);
        assert!(output.status.success(), "{output:?}");
        let requests = hub.requests();
        assert_eq!(requests.len(), 1, "{requests:?}");
        assert_eq!(requests[0].target, "/active/api/auth/me");
        assert_eq!(
            requests[0].authorization.as_deref(),
            Some(format!("Bearer {TOKEN}").as_str())
        );
        assert_eq!(fs::read(path).unwrap(), bytes);
        assert_eq!(
            lab.credentials(),
            before,
            "reading must not migrate legacy files"
        );
    }
}

/// Missing, mismatched, or malformed binding evidence cannot supply authenticated requests.
#[test]
fn unusable_legacy_hub_metadata_fails_closed_without_changing_files() {
    let source = FakeHub::new();
    let destination = FakeHub::new();
    let mut omitted = credential(None);
    omitted.as_object_mut().unwrap().remove("hub");
    for bytes in [
        serde_json::to_vec(&credential(None)).unwrap(),
        serde_json::to_vec(&omitted).unwrap(),
        serde_json::to_vec(&credential(Some(&source.base))).unwrap(),
        serde_json::to_vec(&credential(Some("not-a-hub"))).unwrap(),
        b"{ malformed synthetic credential".to_vec(),
    ] {
        let lab = Lab::new();
        lab.write(&legacy_filename(&destination.base), &bytes);
        let before = lab.credentials();
        let output = lab.run(&destination.base, &["whoami", "--check"]);
        assert_auth_refused(&output, &source, &destination);
        assert_eq!(lab.credentials(), before);
    }
}

/// Global logout revokes at the recorded hub even when the selected hub shares its legacy name.
#[test]
fn logout_all_never_routes_a_legacy_token_by_the_current_filename() {
    let source = FakeHub::new();
    let destination = FakeHub::new();
    let lab = Lab::new();
    let stored = format!("{}/saved", source.uppercase());
    lab.write(
        "HTTP_.json",
        &serde_json::to_vec(&credential(Some(&stored))).unwrap(),
    );
    let output = lab.run(&destination.uppercase(), &["logout", "--all"]);
    let foreign = destination.requests();
    assert!(
        foreign.is_empty(),
        "logout sent a foreign token: {foreign:?}"
    );
    assert!(output.status.success(), "{output:?}");
    let requests = source.requests();
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_eq!(requests[0].target, "/saved/api/auth/logout");
    assert_eq!(
        requests[0].authorization.as_deref(),
        Some(format!("Bearer {TOKEN}").as_str())
    );
    assert!(
        lab.credentials().is_empty(),
        "explicit global logout must clear local credentials"
    );
}

/// Global cleanup of unbound files cannot infer a revoke destination from the selected hub.
#[test]
fn logout_all_clears_unbound_legacy_files_without_sending_their_tokens() {
    let hub = FakeHub::new();
    for bytes in [
        serde_json::to_vec(&credential(None)).unwrap(),
        serde_json::to_vec(&credential(Some("not-a-hub"))).unwrap(),
        b"{ malformed synthetic credential".to_vec(),
    ] {
        let lab = Lab::new();
        lab.write("HTTP_.json", &bytes);
        let output = lab.run(&hub.uppercase(), &["logout", "--all"]);
        let requests = hub.requests();
        assert!(
            requests.is_empty(),
            "an unbound token was sent: {requests:?}"
        );
        assert!(output.status.success(), "{output:?}");
        assert!(lab.credentials().is_empty());
    }
}

#[test]
fn invalid_current_slot_cannot_resurrect_a_valid_legacy_token() {
    let hub = FakeHub::new();
    let lab = Lab::new();
    lab.write(
        &legacy_filename(&hub.base),
        &serde_json::to_vec(&credential(Some(&hub.base))).unwrap(),
    );
    lab.write(
        &format!(
            "{}.json",
            agit::infra::config::hub_host_key(&hub.base).unwrap()
        ),
        b"invalid current record",
    );
    let before = lab.credentials();
    let output = lab.run(&hub.base, &["whoami", "--check"]);
    assert_eq!(output.status.code(), Some(5), "{output:?}");
    assert!(hub.requests().is_empty());
    assert_eq!(lab.credentials(), before);
}

#[test]
fn legacy_case_preservation_reuses_only_the_same_filesystem_record() {
    let hub = FakeHub::new();
    let lab = Lab::new();
    let stored = hub.base.replacen("http://", "HTTP://", 1);
    let path = lab.write(
        "HTTP_.json",
        &serde_json::to_vec(&credential(Some(&stored))).unwrap(),
    );
    let alias = lab.agit_home.join("credentials/http_.json");
    if !alias.exists() {
        return;
    }
    let changed = stored.replacen("HTTP://", "HtTp://", 1);
    fs::write(
        &path,
        serde_json::to_vec(&credential(Some(&changed))).unwrap(),
    )
    .unwrap();
    let before = lab.credentials();
    let output = lab.run(&hub.base, &["whoami", "--check"]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(hub.requests().len(), 1);
    assert_eq!(lab.credentials(), before);
}

#[cfg(unix)]
#[test]
fn unreadable_credentials_cannot_report_a_successful_global_logout() {
    use std::os::unix::fs::PermissionsExt;
    let lab = Lab::new();
    let hub = "http://127.0.0.1:1";
    let path = lab.write(
        &legacy_filename(hub),
        &serde_json::to_vec(&credential(Some(hub))).unwrap(),
    );
    let directory = path.parent().unwrap();
    fs::set_permissions(directory, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read_dir(directory).is_ok() {
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
        return;
    }
    let output = lab.run(hub, &["logout", "--all"]);
    fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(!output.status.success(), "{output:?}");
    assert!(path.exists());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("no saved credentials"));
}

#[test]
fn invalid_login_destinations_are_rejected_without_echoing_secrets() {
    let lab = Lab::new();
    for hub in [
        "http://private-user:private-password@127.0.0.1:1",
        "http://127.0.0.1:1?private-query",
        "http://127.0.0.1:1#private-fragment",
    ] {
        let output = lab.run(hub, &["login", "--with-token"]);
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        assert!(!String::from_utf8_lossy(&output.stdout).contains("private-"));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("private-"));
        assert!(lab.credentials().is_empty());
    }
}

#[test]
fn canonical_case_aliases_are_revoked_and_removed_consistently() {
    for all in [false, true] {
        let hub = FakeHub::new();
        let lab = Lab::new();
        let key = agit::infra::config::hub_host_key(&hub.base).unwrap();
        let path = lab.write(
            &format!("{}.JSON", key.to_ascii_uppercase()),
            &serde_json::to_vec(&credential(Some(&hub.base))).unwrap(),
        );
        let canonical = path.with_file_name(format!("{key}.json"));
        if !canonical.exists() {
            continue;
        }
        assert!(lab.run(&hub.base, &["whoami"]).status.success());
        let output = lab.run(
            &hub.base,
            if all {
                &["logout", "--all"]
            } else {
                &["logout"]
            },
        );
        assert!(output.status.success(), "{output:?}");
        assert!(!path.exists());
        let requests = hub.requests();
        assert_eq!(requests.len(), 1, "{requests:?}");
        assert_eq!(requests[0].target, "/api/auth/logout");
        assert_eq!(lab.run(&hub.base, &["whoami"]).status.code(), Some(5));
    }
}

#[test]
fn whoami_only_contacts_the_hub_for_explicit_identity_verification() {
    let hub = FakeHub::new();
    let canary = Lab::new();
    let output = canary
        .command(&hub.base, &["config", "--list"])
        .env_remove("CI")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    if agit::infra::config::is_production_release() {
        assert!(
            hub.requests()
                .iter()
                .any(|request| request.target == "/api/cli/version")
        );
    }
    hub.requests.lock().unwrap().clear();

    let lab = Lab::new();
    lab.write(
        &legacy_filename(&hub.base),
        &serde_json::to_vec(&credential(Some(&hub.base))).unwrap(),
    );
    let before = lab.credentials();
    let local = lab
        .command(&hub.base, &["whoami"])
        .env_remove("CI")
        .output()
        .unwrap();
    assert!(local.status.success(), "{local:?}");
    assert!(
        hub.requests().is_empty(),
        "local identity inspection must not contact the Hub"
    );
    assert_eq!(lab.credentials(), before);

    let checked = lab
        .command(&hub.base, &["whoami", "--check"])
        .env_remove("CI")
        .output()
        .unwrap();
    assert!(checked.status.success(), "{checked:?}");
    let requests = hub.requests();
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_eq!(requests[0].target, "/api/auth/me");
    assert_eq!(
        requests[0].authorization.as_deref(),
        Some(format!("Bearer {TOKEN}").as_str())
    );
}

#[test]
#[cfg(windows)]
fn copied_auth_hint_preserves_native_shims_exit_and_environment() {
    let lab = Lab::new();
    let root = tempfile::tempdir().unwrap();
    let recorder = root.path().join("record.ps1");
    std::fs::write(
        &recorder,
        "$record = @{ hub=$env:AGIT_HUB_URL; arguments=@($args) }; [IO.File]::WriteAllText((Join-Path $PSScriptRoot 'record.json'), ($record | ConvertTo-Json -Compress))",
    ).unwrap();
    std::fs::write(
        root.path().join("agit.cmd"),
        "@echo off\r\npowershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File \"%~dp0record.ps1\" %*\r\nexit /b 17\r\n",
    ).unwrap();
    let restored = root.path().join("restored.json");
    // PowerShell delimiter fixtures remain literal through native command resolution.
    for hub in [
        "https://example.invalid/team&qa;Write-Output/it's/$HOME",
        "https://example.invalid/a\"b",
        "https://example.invalid/‘’‚‛/$HOME",
    ] {
        let output = lab.run(hub, &["share", "list"]);
        assert_eq!(output.status.code(), Some(5), "{output:?}");
        let diagnostics = String::from_utf8(output.stderr).unwrap();
        let hint = diagnostics
            .lines()
            .find(|line| line.contains("log in from PowerShell with `"))
            .unwrap();
        assert!(hint.contains("from PowerShell"));
        let copied = hint
            .split_once("with `")
            .unwrap()
            .1
            .strip_suffix('`')
            .unwrap();
        for prior in ["$null", "''", "'https://prior.invalid'"] {
            let script = format!(
                "$env:AGIT_HUB_URL={prior}; $before=$env:AGIT_HUB_URL; {copied}; $record=@{{ before=$before; after=$env:AGIT_HUB_URL; exit_code=$LASTEXITCODE }}; [IO.File]::WriteAllText($env:AGIT_FIXTURE_RESTORED_PATH, ($record | ConvertTo-Json -Compress))",
            );
            let output = std::process::Command::new("powershell.exe")
                .args(["-NoProfile", "-NonInteractive", "-Command", &script])
                .env("AGIT_FIXTURE_RESTORED_PATH", &restored)
                .env("PATHEXT", ".COM;.EXE;.BAT;.CMD")
                .env(
                    "PATH",
                    format!(
                        "{};{}",
                        root.path().display(),
                        std::env::var("PATH").unwrap_or_default()
                    ),
                )
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            assert!(output.stderr.is_empty(), "{output:?}");
            let actual: serde_json::Value =
                serde_json::from_slice(&std::fs::read(root.path().join("record.json")).unwrap())
                    .unwrap();
            assert_eq!(actual, serde_json::json!({"hub":hub,"arguments":["login"]}));
            let actual: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&restored).unwrap()).unwrap();
            assert_eq!(actual["before"], actual["after"]);
            assert_eq!(actual["exit_code"], 17);
            assert!(lab.credentials().is_empty());
        }
    }
}
