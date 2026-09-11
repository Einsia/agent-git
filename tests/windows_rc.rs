#![cfg(windows)]

use std::io::{Read, Seek, Write};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use agit::rc::control::{self, Reply, Request};

#[path = "../src/infra/windows_security.rs"]
#[allow(dead_code)]
mod security;

fn command(home: &Path, hub: &str, args: &[&str]) -> Output {
    command_input(home, hub, args, None)
}

fn command_input(home: &Path, hub: &str, args: &[&str], input: Option<&str>) -> Output {
    eprintln!("native CLI starting: {args:?}");
    let mut stdout = tempfile::tempfile().unwrap();
    let mut stderr = tempfile::tempfile().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_agit"))
        .args(args)
        .env("AGIT_HOME", home)
        .env("AGIT_HUB_URL", hub)
        .env("CI", "1")
        .env("AGIT_SECRETS_KEYSTORE", "os")
        .env_remove("AGIT_SESSION")
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(stdout.try_clone().unwrap())
        .stderr(stderr.try_clone().unwrap())
        .spawn()
        .unwrap();
    if let Some(input) = input {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            timed_out = true;
            let _ = child.kill();
            break child.wait().unwrap();
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    // Descendants may retain output handles after the command exits; file reads do not wait for EOF.
    let read_output = |file: &mut std::fs::File| {
        file.rewind().unwrap();
        let mut bytes = Vec::new();
        file.take(1024 * 1024).read_to_end(&mut bytes).unwrap();
        bytes
    };
    let output = Output {
        status,
        stdout: read_output(&mut stdout),
        stderr: read_output(&mut stderr),
    };
    assert!(
        !timed_out,
        "command {args:?} timed out: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    eprintln!("native CLI finished: {args:?} ({})", output.status);
    output
}

fn success(home: &Path, hub: &str, args: &[&str]) -> Output {
    let output = command(home, hub, args);
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

struct FakeHub {
    url: String,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

struct DaemonCleanup<'a> {
    home: &'a Path,
    hub: &'a str,
}

impl Drop for DaemonCleanup<'_> {
    fn drop(&mut self) {
        let _ = command(self.home, self.hub, &["rc", "stop"]);
    }
}

struct VaultCleanup<'a>(&'a Path);

impl Drop for VaultCleanup<'_> {
    fn drop(&mut self) {
        use agit::domain::secret_filter::KeyStore;
        let path = self.0.join("secret-filter/vault.json");
        if let Ok(body) = std::fs::read(path)
            && let Ok(value) = serde_json::from_slice::<serde_json::Value>(&body)
            && let Some(id) = value["vault_id"].as_str()
        {
            let _ = agit::domain::secret_filter::OsKeyStore.delete(id);
        }
    }
}

fn secret_commands_reload_live_matcher(home: &Path) {
    use agit::domain::secret_filter::MatcherHandle;
    let matcher = MatcherHandle::load_default().unwrap();
    let live = matcher.clone();
    let listener = control::listen().unwrap();
    let worker = std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stop = false;
            control::serve_one(&mut stream.unwrap(), |request| match request {
                Request::Status => Reply::Status(control::Status {
                    pid: std::process::id(),
                    ..Default::default()
                }),
                Request::ReloadSecrets => match live.reload_default() {
                    Ok(status) => Reply::SecretsReloaded {
                        generation: status.generation,
                        rules: status.rules,
                    },
                    Err(error) => Reply::Error {
                        message: error.to_string(),
                    },
                },
                Request::Stop => {
                    stop = true;
                    Reply::Stopping
                }
            })
            .unwrap();
            if stop {
                break;
            }
        }
    });
    let added = command_input(
        home,
        "http://127.0.0.1:9",
        &["secrets", "add", "native-fixture", "--stdin"],
        Some("windows-fixture-secret\n"),
    );
    let after_add = matcher.snapshot();
    let removed = command(
        home,
        "http://127.0.0.1:9",
        &["secrets", "remove", "native-fixture", "--yes"],
    );
    let after_remove = matcher.snapshot();
    assert!(matches!(
        control::ask(&Request::Stop).unwrap(),
        Reply::Stopping
    ));
    worker.join().unwrap();
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );
    assert!(
        removed.status.success(),
        "{}",
        String::from_utf8_lossy(&removed.stderr)
    );
    assert_eq!(after_add.find("windows-fixture-secret").len(), 1);
    assert!(after_remove.find("windows-fixture-secret").is_empty());
    assert!(after_remove.generation() > after_add.generation());
}

impl FakeHub {
    fn start() -> Self {
        Self::with_pair_status("200 OK")
    }

    fn with_pair_status(pair_status: &'static str) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let worker = std::thread::spawn(move || {
            while !stopping.load(Ordering::SeqCst) {
                let (mut stream, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    Err(error) => panic!("fake Hub accept: {error}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = Vec::new();
                let mut byte = [0];
                while request.len() < 65536 && !request.ends_with(b"\r\n\r\n") {
                    if stream.read_exact(&mut byte).is_err() {
                        break;
                    }
                    request.push(byte[0]);
                }
                let request = String::from_utf8_lossy(&request);
                let paired = request.starts_with("POST /api/rc/connections ");
                let length = request
                    .lines()
                    .find_map(|line| {
                        let (key, value) = line.split_once(':')?;
                        key.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                if length > 65536 {
                    continue;
                }
                let mut body = vec![0; length];
                if stream.read_exact(&mut body).is_err() {
                    continue;
                }
                if paired {
                    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
                    assert!(body["platform"].as_str().unwrap().starts_with("windows"));
                    assert!(!body["machine_fingerprint"].as_str().unwrap().is_empty());
                }
                let (status, body) = if paired && pair_status == "200 OK" {
                    (
                        pair_status,
                        r#"{"connection_id":"windows-fixture","token":"synthetic-rc-fixture"}"#,
                    )
                } else if paired {
                    (
                        pair_status,
                        r#"{"error":"synthetic HTTP 401 wording","kind":"unauthorized","fix":[{"kind":"authenticate"}]}"#,
                    )
                } else {
                    ("503 Service Unavailable", "{}")
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        Self {
            url,
            stop,
            worker: Some(worker),
        }
    }
}

impl Drop for FakeHub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.worker.take().unwrap().join().unwrap();
    }
}

fn restricted_open(path: &Path, access: u32) -> std::io::Result<security::Handle> {
    use windows_sys::Win32::Security::{
        CreateRestrictedToken, CreateWellKnownSid, DISABLE_MAX_PRIVILEGE, ImpersonateLoggedOnUser,
        RevertToSelf, SID_AND_ATTRIBUTES, TOKEN_DUPLICATE, TOKEN_IMPERSONATE, TOKEN_QUERY,
        WinWorldSid,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, OPEN_EXISTING, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    let mut raw = std::ptr::null_mut();
    assert_ne!(
        unsafe {
            OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_QUERY | TOKEN_DUPLICATE | TOKEN_IMPERSONATE,
                &mut raw,
            )
        },
        0
    );
    let token = security::Handle::new(raw).unwrap();
    let mut world = [0usize; 16];
    let mut size = std::mem::size_of_val(&world) as u32;
    assert_ne!(
        unsafe {
            CreateWellKnownSid(
                WinWorldSid,
                std::ptr::null_mut(),
                world.as_mut_ptr().cast(),
                &mut size,
            )
        },
        0
    );
    let restricted = SID_AND_ATTRIBUTES {
        Sid: world.as_mut_ptr().cast(),
        Attributes: 0,
    };
    let mut raw = std::ptr::null_mut();
    assert_ne!(
        unsafe {
            CreateRestrictedToken(
                token.0,
                DISABLE_MAX_PRIVILEGE,
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                1,
                &restricted,
                &mut raw,
            )
        },
        0
    );
    let restricted = security::Handle::new(raw).unwrap();
    let name = security::wide(path).unwrap();
    assert_ne!(unsafe { ImpersonateLoggedOnUser(restricted.0) }, 0);
    let result = security::Handle::new(unsafe {
        CreateFileW(
            name.as_ptr(),
            access,
            0,
            std::ptr::null(),
            OPEN_EXISTING,
            SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
            std::ptr::null_mut(),
        )
    });
    let reverted = unsafe { RevertToSelf() };
    assert_ne!(reverted, 0, "test must restore its thread token");
    result
}

fn restricted_client_is_denied(path: &Path) {
    use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, GENERIC_READ, GENERIC_WRITE};
    assert_eq!(
        restricted_open(path, GENERIC_READ | GENERIC_WRITE)
            .err()
            .unwrap()
            .raw_os_error(),
        Some(ERROR_ACCESS_DENIED as i32)
    );
}

fn hub_credentials_are_private_under_inherited_public_read(home: &Path) {
    use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, GENERIC_READ};
    let directory = home.join("public-read-credentials");
    security::private_directory(&directory).unwrap();
    let acl = Command::new("icacls")
        .arg(&directory)
        .args(["/grant", "*S-1-1-0:(OI)(CI)R"])
        .output()
        .unwrap();
    assert!(acl.status.success());
    let legacy = directory.join("legacy.json");
    std::fs::write(&legacy, b"synthetic legacy credential").unwrap();
    assert!(
        restricted_open(&legacy, GENERIC_READ).is_ok(),
        "the inherited-read fixture must expose an ordinary file write"
    );
    let path = directory.join("private.json");
    let mut credential = agit::infra::credentials::HubCredential {
        username: "windows-fixture".into(),
        email: None,
        hub: Some("http://127.0.0.1:9".into()),
        access_token: "synthetic-access-fixture".into(),
        refresh_token: "synthetic-refresh-fixture".into(),
        access_expires_at: "2099-01-01T00:00:00Z".into(),
        refresh_expires_at: "2099-01-01T00:00:00Z".into(),
    };
    for username in ["before-refresh", "after-refresh"] {
        credential.username = username.into();
        agit::infra::credentials::save_at(&path, &credential).unwrap();
        assert!(agit::infra::credentials::load_at(&path).unwrap().username == username);
        security::validate_path(&path, false, true).unwrap();
        assert_eq!(
            restricted_open(&path, GENERIC_READ)
                .err()
                .unwrap()
                .raw_os_error(),
            Some(ERROR_ACCESS_DENIED as i32)
        );
    }
    assert!(agit::infra::credentials::save_at(&legacy, &credential).is_err());
    assert!(std::fs::read(&legacy).unwrap() == b"synthetic legacy credential");
    assert!(restricted_open(&legacy, GENERIC_READ).is_ok());
    assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 2);
}

#[test]
fn native_pipe_permissions_and_daemon_lifecycle() {
    let temporary = tempfile::tempdir().unwrap();
    let home = temporary.path().join("home");
    security::private_directory(&home).unwrap();
    let _vault_cleanup = VaultCleanup(&home);
    unsafe {
        std::env::set_var("AGIT_HOME", &home);
        std::env::set_var("AGIT_SECRETS_KEYSTORE", "os");
    }
    hub_credentials_are_private_under_inherited_public_read(&home);
    let rc = agit::rc::rc_dir().unwrap();
    let path = control::socket_path().unwrap();
    assert_eq!(path, control::socket_path_for(&rc.join(".")).unwrap());
    let other = home.join("other");
    security::private_directory(&other).unwrap();
    assert_ne!(path, control::socket_path_for(&other).unwrap());

    let listener = control::listen().unwrap();
    assert!(
        control::listen().is_err(),
        "an existing pipe owner must not be replaced"
    );
    restricted_client_is_denied(&path);
    let worker = std::thread::spawn(move || {
        for stream in listener.incoming().take(2) {
            control::serve_one(&mut stream.unwrap(), |request| match request {
                Request::Status => Reply::Status(control::Status {
                    pid: std::process::id(),
                    ..Default::default()
                }),
                _ => Reply::Stopping,
            })
            .unwrap();
        }
    });
    assert!(matches!(control::presence(), control::Presence::Running(_)));
    assert!(matches!(
        control::ask(&Request::Stop).unwrap(),
        Reply::Stopping
    ));
    worker.join().unwrap();
    assert_eq!(control::presence(), control::Presence::Absent);

    let listener = control::listen().unwrap();
    let stalled = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let started = Instant::now();
    let worker = std::thread::spawn(move || {
        let mut incoming = listener.incoming();
        let result = control::serve_one(&mut incoming.next().unwrap().unwrap(), |_| {
            panic!("incomplete frames must not dispatch")
        });
        assert!(result.is_err());
        control::serve_one(&mut incoming.next().unwrap().unwrap(), |_| Reply::Stopping).unwrap();
    });
    std::thread::sleep(Duration::from_secs(6));
    drop(stalled);
    assert!(matches!(
        control::ask(&Request::Stop).unwrap(),
        Reply::Stopping
    ));
    worker.join().unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(12),
        "a stalled peer must not wedge later control requests"
    );

    secret_commands_reload_live_matcher(&home);

    let refused = FakeHub::with_pair_status("503 Service Unavailable");
    agit::infra::credentials::save(
        &refused.url,
        &agit::infra::credentials::HubCredential {
            username: "windows-fixture".into(),
            email: None,
            hub: Some(refused.url.clone()),
            access_token: "synthetic-user-fixture".into(),
            refresh_token: "synthetic-refresh-fixture".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_expires_at: "2099-01-01T00:00:00Z".into(),
        },
    )
    .unwrap();
    let credential_path = agit::infra::config::credentials_path(&refused.url).unwrap();
    let credential_before = std::fs::read(&credential_path).unwrap();
    let fingerprint_before = agit::rc::identity::identity().unwrap().machine_fingerprint;
    for flags in [
        vec![],
        vec!["--quiet"],
        vec!["--json", "--json-version", "1"],
        vec!["--json", "--json-version", "2"],
    ] {
        for operation in [vec!["rc", "pair"], vec!["rc", "start", "--detach"]] {
            let mut args = flags.clone();
            args.extend(operation);
            let output = command(&home, &refused.url, &args);
            assert_eq!(output.status.code(), Some(6), "{args:?}: {output:?}");
            if flags.contains(&"--json") {
                assert!(output.stderr.is_empty(), "{output:?}");
                let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
                assert_eq!(value["command"], "rc");
                assert_eq!(value["exit_code"], 6);
                assert_eq!(value["ok"], false);
                assert_eq!(
                    value["schema_version"],
                    flags.last().unwrap().parse::<u32>().unwrap()
                );
                if flags.last() == Some(&"1") {
                    assert!(value.get("fix").is_none());
                } else {
                    assert_eq!(value["fix"], serde_json::json!([]));
                }
            } else {
                assert!(
                    String::from_utf8_lossy(&output.stderr).contains("synthetic HTTP 401 wording")
                );
                assert!(output.stdout.is_empty(), "{output:?}");
            }
            assert_eq!(std::fs::read(&credential_path).unwrap(), credential_before);
            assert_eq!(
                agit::rc::identity::identity().unwrap().machine_fingerprint,
                fingerprint_before
            );
            assert!(
                agit::rc::identity::connection(&refused.url)
                    .unwrap()
                    .is_none()
            );
            assert_eq!(control::presence(), control::Presence::Absent);
            for secret in ["synthetic-user-fixture", "synthetic-refresh-fixture"] {
                assert!(!String::from_utf8_lossy(&output.stdout).contains(secret));
                assert!(!String::from_utf8_lossy(&output.stderr).contains(secret));
            }
        }
    }
    drop(refused);

    let hub = FakeHub::start();
    agit::infra::credentials::save(
        &hub.url,
        &agit::infra::credentials::HubCredential {
            username: "windows-fixture".into(),
            email: None,
            hub: Some(hub.url.clone()),
            access_token: "synthetic-user-fixture".into(),
            refresh_token: "synthetic-refresh-fixture".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_expires_at: "2099-01-01T00:00:00Z".into(),
        },
    )
    .unwrap();
    success(&home, &hub.url, &["rc", "pair"]);
    let _cleanup = DaemonCleanup {
        home: &home,
        hub: &hub.url,
    };
    let fingerprint = agit::rc::identity::identity().unwrap().machine_fingerprint;
    for _ in 0..2 {
        success(&home, &hub.url, &["rc", "start", "--detach"]);
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if matches!(control::presence(), control::Presence::Running(_)) {
                break;
            }
            assert!(Instant::now() < deadline, "detached daemon did not start");
            std::thread::sleep(Duration::from_millis(50));
        }
        success(&home, &hub.url, &["rc", "status"]);
        assert!(
            !command(&home, &hub.url, &["rc", "start", "--detach"])
                .status
                .success()
        );
        assert!(matches!(
            control::ask(&Request::ReloadSecrets).unwrap(),
            Reply::SecretsReloaded { .. }
        ));
        success(&home, &hub.url, &["rc", "stop"]);
        let deadline = Instant::now() + Duration::from_secs(15);
        while control::presence() != control::Presence::Absent {
            assert!(Instant::now() < deadline, "daemon pipe survived shutdown");
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(!rc.join("agitd.pid").exists());
        assert_eq!(
            fingerprint,
            agit::rc::identity::identity().unwrap().machine_fingerprint
        );
    }
    assert!(!command(&home, &hub.url, &["rc", "status"]).status.success());
    security::validate_path(&rc.join("identity.json"), false, true).unwrap();
    assert!(security::require_process_user(std::process::id(), "S-1-5-21-0-0-0-9999").is_err());

    let hostile = temporary.path().join("hostile");
    security::private_directory(&hostile).unwrap();
    let hostile_rc = hostile.join("rc");
    security::private_directory(&hostile_rc).unwrap();
    let acl = Command::new("icacls")
        .arg(&hostile_rc)
        .args(["/grant", "*S-1-1-0:(OI)(CI)F"])
        .output()
        .unwrap();
    assert!(acl.status.success());
    assert!(security::validate_path(&hostile_rc, true, true).is_err());
    assert!(
        !command(&hostile, &hub.url, &["rc", "start", "--detach"])
            .status
            .success()
    );
    assert!(!hostile_rc.join("identity.json").exists());

    let inheritable_home = temporary.path().join("inheritable-home");
    security::private_directory(&inheritable_home).unwrap();
    let inheritable_rc = inheritable_home.join("rc");
    security::private_directory(&inheritable_rc).unwrap();
    let acl = Command::new("icacls")
        .arg(&inheritable_rc)
        .args(["/grant", "*S-1-1-0:(OI)(IO)F"])
        .output()
        .unwrap();
    assert!(acl.status.success());
    assert!(
        security::validate_path(&inheritable_rc, true, true).is_err(),
        "private RC files must not inherit access for other users"
    );
    assert!(
        !command(&inheritable_home, &hub.url, &["rc", "start", "--detach"])
            .status
            .success()
    );
    assert!(!inheritable_rc.join("identity.json").exists());

    let shared_parent = temporary.path().join("shared-parent");
    security::private_directory(&shared_parent).unwrap();
    let nested_home = shared_parent.join("private-home");
    security::private_directory(&nested_home).unwrap();
    let acl = Command::new("icacls")
        .arg(&shared_parent)
        .args(["/grant", "*S-1-1-0:(DC)"])
        .output()
        .unwrap();
    assert!(acl.status.success());
    assert!(
        security::validate_path(&nested_home, true, true).is_err(),
        "a private leaf cannot override destructive access to its ancestors"
    );
    assert!(
        !command(&nested_home, &hub.url, &["rc", "start", "--detach"])
            .status
            .success()
    );
    assert!(!nested_home.join("rc").exists());
}
