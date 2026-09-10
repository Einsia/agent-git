//! Capturing standard streams preserves command arguments, stdin, and execution count.

use serde_json::Value;
use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

struct Lab {
    _temp: tempfile::TempDir,
    home: PathBuf,
    store: PathBuf,
    cwd: PathBuf,
}

impl Lab {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let store = temp.path().join("agit");
        let cwd = temp.path().join("work");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&cwd).unwrap();
        fs::write(home.join("gitconfig"), "").unwrap();
        Self {
            _temp: temp,
            home,
            store,
            cwd,
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command
            .args(args)
            .current_dir(&self.cwd)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("AGIT_HOME", &self.store)
            .env("AGIT_HUB_URL", "http://127.0.0.1:1")
            .env("AGIT_YES", "1")
            .env("NO_COLOR", "1")
            .env("GIT_CONFIG_GLOBAL", self.home.join("gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1");
        if let Some(system_root) = std::env::var_os("SystemRoot") {
            command.env("SystemRoot", system_root);
        }
        command
    }

    fn run(&self, args: &[&str]) -> Value {
        document(&self.command(args).output().unwrap())
    }
}

fn document(output: &Output) -> Value {
    assert!(
        output.stderr.is_empty(),
        "diagnostics escaped the envelope: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout must contain exactly one document: {error}: {}",
            String::from_utf8_lossy(&output.stdout)
        )
    });
    assert_eq!(document["schema"], "cli-output");
    assert_eq!(document["exit_code"], output.status.code().unwrap());
    assert_eq!(document["ok"], output.status.success());
    document
}

#[test]
fn structured_status_applies_relative_directory_once() {
    let lab = Lab::new();
    fs::create_dir(lab.cwd.join("nested")).unwrap();
    let doc = lab.run(&["-C", "nested", "--json", "status"]);
    assert_eq!(doc["ok"], true);
    assert_eq!(doc["result"]["format"], "json");
    let expected = lab.cwd.join("nested").canonicalize().unwrap();
    let actual = PathBuf::from(doc["result"]["value"]["cwd"].as_str().unwrap());
    assert_eq!(actual.canonicalize().unwrap(), expected);
    assert!(!lab.store.exists());
}

#[test]
fn verbose_structured_output_is_complete_beyond_pipe_capacity() {
    let lab = Lab::new();
    let links = lab.store.join("store").join("codex");
    fs::create_dir_all(&links).unwrap();
    let branch = "topic/".to_owned() + &"long-name-".repeat(80);
    for index in 0..150 {
        fs::write(
            links.join(format!("session-{index}.json")),
            serde_json::json!({
                "owner": "alice", "agent": "demo", "branch": branch, "cwd": lab.cwd,
            })
            .to_string(),
        )
        .unwrap();
    }
    let output = lab
        .command(&["--json", "status", "--limit", "150"])
        .output()
        .unwrap();
    assert!(output.stdout.len() > 256 * 1024);
    let doc = document(&output);
    assert_eq!(doc["ok"], true);
    assert_eq!(
        doc["result"]["value"]["sessions"]["items"]
            .as_array()
            .unwrap()
            .len(),
        150
    );
    assert_eq!(
        doc["result"]["value"]["sessions"]["items"][149]["branch"],
        branch
    );
}

#[test]
fn initialization_executes_once_and_command_failures_keep_diagnostics() {
    let lab = Lab::new();
    let created = lab.run(&["--json", "init", "demo"]);
    assert_eq!(created["ok"], true, "{created}");
    let repeated = lab.run(&["--json", "init", "demo"]);
    assert_eq!(repeated["ok"], false);
    assert!(
        repeated["diagnostics"]["stderr"]
            .to_string()
            .contains("already exists")
    );
}

#[test]
fn literal_json_argument_is_not_removed_or_promoted_to_a_flag() {
    let lab = Lab::new();
    let doc = lab.run(&["--json", "config", "runtime.default", "--", "--json"]);
    assert_eq!(doc["ok"], false);
    assert!(doc["diagnostics"]["stderr"].to_string().contains("--json"));
    let native = lab
        .command(&["config", "runtime.default", "--", "--json"])
        .output()
        .unwrap();
    assert!(!native.status.success());
    assert!(!String::from_utf8_lossy(&native.stdout).contains("cli-output"));
    assert!(String::from_utf8_lossy(&native.stderr).contains("--json"));
}

#[test]
fn launch_and_parse_rejections_precede_storage_preparation() {
    let lab = Lab::new();
    for args in [
        vec!["--json", "resume", "local/demo@work"],
        vec!["--json", "login"],
        vec!["--json", "rc", "start"],
        vec!["--json", "status", "--unknown-option"],
    ] {
        let doc = lab.run(&args);
        assert_eq!(doc["ok"], false);
        assert!(!lab.store.exists());
    }
}

#[test]
fn stdin_reaches_token_login_without_secret_output() {
    let lab = Lab::new();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let hub = format!("http://{}", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "login never reached the hub"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(error) => panic!("accept failed: {error}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut input = Vec::new();
        loop {
            let mut chunk = [0; 4096];
            let count = stream.read(&mut chunk).unwrap();
            assert_ne!(count, 0);
            input.extend_from_slice(&chunk[..count]);
            if let Some(end) = input.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&input[..end]);
                let length: usize = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .map(|n| n.parse().unwrap())
                    })
                    .unwrap();
                if input.len() >= end + 4 + length {
                    let body: Value =
                        serde_json::from_slice(&input[end + 4..end + 4 + length]).unwrap();
                    assert_eq!(body["token"], "fake-pat-for-capture-test");
                    break;
                }
            }
        }
        let body = serde_json::json!({
            "username": "alice", "email": null,
            "access_token": "fake-access", "refresh_token": "fake-refresh",
            "access_expires_at": "2099-01-01T00:00:00Z",
            "refresh_expires_at": "2099-01-01T00:00:00Z"
        })
        .to_string();
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
    });
    let mut child = lab
        .command(&["--json", "login", "--with-token"])
        .env("AGIT_HUB_URL", hub)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"fake-pat-for-capture-test\n")
        .unwrap();
    let doc = document(&child.wait_with_output().unwrap());
    server.join().unwrap();
    assert_eq!(doc["ok"], true, "{doc}");
    assert!(!doc.to_string().contains("fake-pat"));
    assert!(!doc.to_string().contains("fake-access"));
    assert!(!doc.to_string().contains("fake-refresh"));

    let empty = lab.run(&["--json", "login", "--with-token"]);
    assert_eq!(empty["ok"], false);
    assert!(
        empty["diagnostics"]["stderr"]
            .to_string()
            .contains("stdin was empty")
    );
}
