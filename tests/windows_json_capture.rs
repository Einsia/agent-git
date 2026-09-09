//! Native redirected streams preserve the CLI envelope and command side effects.

#![cfg(all(windows, target_env = "msvc"))]

use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};
use windows_sys::Win32::System::Pipes::CreatePipe;

const HUB: &str = "http://127.0.0.1:1";
const SID: &str = "aaaaaaaa-0000-4000-8000-000000000001";

struct Lab {
    temporary: tempfile::TempDir,
    home: PathBuf,
    store: PathBuf,
    work: PathBuf,
}

fn ordinary_windows_path(path: &Path) -> PathBuf {
    let path = path.to_str().unwrap();
    if let Some(share) = path.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{share}"))
    } else {
        PathBuf::from(path.strip_prefix(r"\\?\").unwrap_or(path))
    }
}

impl Lab {
    fn new() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().join("home");
        let store = temporary.path().join("agit");
        // These path characters are fixture data that exercise native Unicode arguments.
        let work = temporary.path().join("work 'quoted' λ 🧪");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&work).unwrap();
        let work = ordinary_windows_path(&work.canonicalize().unwrap());
        Self {
            temporary,
            home,
            store,
            work,
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command
            .args(args)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("AGIT_HOME", &self.store)
            .env("AGIT_HUB_URL", HUB)
            .env("AGIT_SECRETS_KEYSTORE", "os")
            .env("GIT_CONFIG_GLOBAL", self.home.join("empty-gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("CI", "1")
            .env("NO_COLOR", "1")
            .current_dir(&self.work);
        for name in ["SystemRoot", "WINDIR", "TEMP", "TMP", "ComSpec"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        command
    }

    fn json(&self, args: &[&str]) -> (Output, Value) {
        let output = self.command(args).output().unwrap();
        let value = envelope(&output);
        (output, value)
    }

    fn persisted_config(&self) -> Value {
        serde_json::from_slice(&fs::read(self.store.join("config.json")).unwrap()).unwrap()
    }

    fn state(&self) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
        walkdir::WalkDir::new(self.temporary.path())
            .into_iter()
            .map(Result::unwrap)
            .map(|entry| {
                let contents = entry
                    .file_type()
                    .is_file()
                    .then(|| fs::read(entry.path()).unwrap());
                (entry.path().to_owned(), contents)
            })
            .collect()
    }

    fn save_credentials(&self, hub: &str, refresh_expires_at: &str) {
        agit::infra::credentials::save_at(
            &self.store.join("credentials").join(format!(
                "{}.json",
                agit::infra::config::hub_host_key(hub).unwrap()
            )),
            &agit::infra::credentials::HubCredential {
                username: "me".into(),
                email: None,
                hub: Some(hub.into()),
                access_token: "SYNTHETIC".into(),
                refresh_token: "SYNTHETIC".into(),
                access_expires_at: "2099-01-01T00:00:00Z".into(),
                refresh_expires_at: refresh_expires_at.into(),
            },
        )
        .unwrap();
    }

    fn import_session(&self) -> Vec<Value> {
        self.save_credentials(HUB, "2099-01-01T00:00:00Z");
        let project = self
            .home
            .join(".claude/projects")
            .join(agit::adapter::claude_code::slug_for(&self.work));
        fs::create_dir_all(&project).unwrap();
        // The transcript text is synthetic Unicode data, including embedded line endings.
        let records = vec![
            json!({"type":"user", "sessionId":SID, "cwd":self.work,
                "uuid":"synthetic-user", "message":{"role":"user", "content":"SYNTHETIC-QUESTION λ 🧪 文本"}}),
            json!({"type":"assistant", "sessionId":SID, "cwd":self.work,
                "uuid":"synthetic-assistant", "message":{"role":"assistant",
                    "content":"SYNTHETIC-ANSWER λ 🧪 文本\n".repeat(4096)}}),
        ];
        let native: String = records.iter().map(|record| format!("{record}\n")).collect();
        assert!(native.len() > 64 * 1024);
        fs::write(project.join(format!("{SID}.jsonl")), native).unwrap();
        for args in [
            vec!["--json", "init", "qa", "--no-bind"],
            vec![
                "--json",
                "import",
                SID,
                "--from",
                "claude-code",
                "--into",
                "me/qa@work",
            ],
        ] {
            let (output, value) = self.json(&args);
            assert!(output.status.success(), "{value}");
        }
        records
    }
}

impl Drop for Lab {
    fn drop(&mut self) {
        use agit::domain::secret_filter::KeyStore;

        if let Ok(bytes) = fs::read(self.store.join("secret-filter/vault.json"))
            && let Ok(value) = serde_json::from_slice::<Value>(&bytes)
            && let Some(id) = value["vault_id"].as_str()
        {
            let _ = agit::domain::secret_filter::OsKeyStore.delete(id);
        }
    }
}

fn envelope(output: &Output) -> Value {
    assert!(output.stderr.is_empty(), "{output:?}");
    let value: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!("stdout is not a complete JSON document: {error}: {output:?}")
    });
    assert_eq!(value["schema"], "cli-output");
    assert_eq!(value["exit_code"], output.status.code().unwrap());
    assert_eq!(value["ok"], output.status.success());
    value
}

#[test]
fn unicode_config_mutation_is_captured_and_survives_native_readback() {
    let lab = Lab::new();
    // The URL path is fixture data that exercises non-ASCII terminal output.
    let value = "http://127.0.0.1:1/配置/λ/🧪";
    let (output, document) = lab.json(&["--json", "config", "hub.url", value]);
    assert!(output.status.success(), "{document}");
    assert_eq!(document["schema_version"], 2);
    assert_eq!(document["fix"], json!([]));
    assert!(document.to_string().contains(value), "{document}");
    assert_eq!(lab.persisted_config()["hub.url"], value);

    let output = lab
        .command(&["config", "hub.url", "--json"])
        .env_remove("AGIT_HUB_URL")
        .output()
        .unwrap();
    let document = envelope(&output);
    assert!(output.status.success(), "{document}");
    assert_eq!(document["result"]["format"], "text");
    assert_eq!(document["result"]["lines"], json!([value]));
}

#[test]
fn unauthenticated_search_captures_diagnostics_and_typed_login_without_running_it() {
    let lab = Lab::new();
    let (output, local) = lab.json(&["--json", "search", "SYNTHETIC-QUERY"]);
    assert_eq!(output.status.code(), Some(5), "{local}");
    assert_eq!(local["fix"].as_array().unwrap().len(), 1);
    assert_eq!(
        local["fix"][0]["argv"],
        json!(["agit", "login", "--hub", HUB])
    );
    assert_eq!(local["fix"][0]["cwd"], lab.work.to_str().unwrap());
    assert_eq!(
        local["fix"][0]["env"],
        json!({"AGIT_HOME":lab.store, "AGIT_HUB_URL":HUB})
    );
    assert_eq!(local["fix"][0]["requires_interaction"], true);
    assert!(!lab.store.join("credentials").exists());

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let hub = format!("http://{}", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        for _ in 0..2 {
            let deadline = Instant::now() + Duration::from_secs(30);
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(connection) => break connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < deadline,
                            "search did not contact its fixture"
                        );
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Err(error) => panic!("cannot accept the fixture request: {error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(&stream);
            let mut request = String::new();
            loop {
                let mut line = String::new();
                assert_ne!(reader.read_line(&mut line).unwrap(), 0);
                if line == "\r\n" {
                    break;
                }
                request.push_str(&line);
                assert!(request.len() < 16 * 1024);
            }
            assert!(
                request.starts_with("GET /api/search/sessions?"),
                "{request}"
            );
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("authorization: bearer synthetic\r\n"),
                "{request}"
            );
            let body = json!({
                "error": "SYNTHETIC-AUTHORIZATION-REQUIRED",
                "kind": "unauthorized",
                "fix": [{"kind":"authenticate"}],
            })
            .to_string();
            write!(
                stream,
                "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        }
    });
    lab.save_credentials(&hub, "2000-01-01T00:00:00Z");
    let before = lab.state();
    let output = lab
        .command(&["--json", "search", "SYNTHETIC-QUERY"])
        .env("AGIT_HUB_URL", &hub)
        .output()
        .unwrap();
    let current = envelope(&output);
    assert_eq!(output.status.code(), Some(5), "{current}");
    assert_eq!(current["schema_version"], 2);
    assert_eq!(current["result"]["format"], "empty");
    assert!(
        !current["diagnostics"]["stderr"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(current["fix"].as_array().unwrap().len(), 1);
    assert_eq!(
        current["fix"][0]["argv"],
        json!(["agit", "login", "--hub", hub])
    );
    assert_eq!(current["fix"][0]["cwd"], lab.work.to_str().unwrap());
    assert_eq!(
        current["fix"][0]["env"],
        json!({"AGIT_HOME":lab.store, "AGIT_HUB_URL":hub})
    );
    assert_eq!(current["fix"][0]["requires_interaction"], true);
    assert_eq!(lab.state(), before);

    let output = lab
        .command(&["--json", "--json-version", "1", "search", "SYNTHETIC-QUERY"])
        .env("AGIT_HUB_URL", &hub)
        .output()
        .unwrap();
    let legacy = envelope(&output);
    assert_eq!(output.status.code(), Some(5), "{legacy}");
    assert_eq!(legacy["schema_version"], 1);
    assert!(legacy.get("fix").is_none());
    let mut same = current;
    same["schema_version"] = json!(1);
    same.as_object_mut().unwrap().remove("fix");
    assert_eq!(same, legacy);
    assert_eq!(legacy.as_object().unwrap().len(), 7);
    assert_eq!(lab.state(), before);
    server.join().unwrap();
}

#[test]
fn imported_raw_jsonl_is_drained_without_truncation_or_native_text_leaks() {
    let lab = Lab::new();
    let records = lab.import_session();
    for args in [
        vec!["--json", "show", "me/qa@work", "--raw"],
        vec!["--json", "show", "me/qa@work", "--raw", "--log-only"],
    ] {
        let (output, document) = lab.json(&args);
        assert!(output.status.success(), "{document}");
        assert_eq!(document["result"]["format"], "json_lines");
        assert_eq!(document["result"]["values"], json!(records));
        assert_eq!(document["fix"], json!([]));
    }
}

#[test]
fn parse_and_interactive_rejections_leave_local_state_untouched() {
    let lab = Lab::new();
    fs::create_dir_all(&lab.store).unwrap();
    fs::write(lab.store.join("synthetic-evidence"), "SYNTHETIC-UNCHANGED").unwrap();
    let before = lab.state();
    for (version, has_fix) in [("1", false), ("2", true)] {
        for (tail, expected_code) in [
            (vec!["--not-an-option"], 2),
            (vec!["search"], 2),
            (vec![], 2),
            (vec!["resume"], 8),
            (vec!["resume", "me/qa@work"], 8),
            (vec!["login"], 8),
        ] {
            let mut args = vec!["--json", "--json-version", version];
            args.extend(tail);
            let (output, document) = lab.json(&args);
            assert_eq!(output.status.code(), Some(expected_code), "{document}");
            assert_eq!(document["schema_version"], version.parse::<u32>().unwrap());
            assert_eq!(document.get("fix").is_some(), has_fix);
            assert_eq!(lab.state(), before, "rejection must precede store changes");
        }
    }
}

#[test]
fn startup_directory_failure_is_enveloped_before_config_mutation() {
    let lab = Lab::new();
    let before = lab.state();
    let missing = lab.work.join("missing-directory");
    let (output, document) = lab.json(&[
        "--json",
        "-C",
        missing.to_str().unwrap(),
        "config",
        "commit.auto",
        "false",
    ]);
    assert_eq!(output.status.code(), Some(2), "{document}");
    assert_eq!(document["result"]["format"], "empty");
    assert!(
        document["diagnostics"]["stderr"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["message"].as_str().unwrap().contains("cannot enter"))
    );
    assert_eq!(lab.state(), before);
}

#[test]
fn file_redirection_receives_the_envelope_after_the_command_mutates_config() {
    let lab = Lab::new();
    let destination = lab.work.join("captured-output.json");
    let output = lab
        .command(&["--json", "config", "commit.auto", "false"])
        .stdout(File::create(&destination).unwrap())
        .output()
        .unwrap();
    assert!(output.stdout.is_empty(), "{output:?}");
    let captured = Output {
        stdout: fs::read(destination).unwrap(),
        ..output
    };
    let document = envelope(&captured);
    assert!(captured.status.success(), "{document}");
    assert_eq!(document["command"], "config");
    assert_eq!(document["fix"], json!([]));
    assert_eq!(lab.persisted_config()["commit.auto"], "false");
}

fn closed_pipe_writer() -> OwnedHandle {
    let mut read = std::ptr::null_mut();
    let mut write = std::ptr::null_mut();
    assert_ne!(
        unsafe { CreatePipe(&mut read, &mut write, std::ptr::null(), 0) },
        0,
        "{}",
        std::io::Error::last_os_error()
    );
    let reader = unsafe { OwnedHandle::from_raw_handle(read) };
    let writer = unsafe { OwnedHandle::from_raw_handle(write) };
    // Closing the receiver before spawn makes the broken output boundary deterministic.
    drop(reader);
    writer
}

#[test]
fn a_closed_stderr_receiver_does_not_escape_the_stdout_envelope() {
    let lab = Lab::new();
    let output = lab
        .command(&["--json", "config", "commit.auto", "false"])
        .stderr(Stdio::from(closed_pipe_writer()))
        .output()
        .unwrap();
    let document = envelope(&output);
    assert!(output.status.success(), "{document}");
    assert_eq!(lab.persisted_config()["commit.auto"], "false");
    assert!(document.to_string().contains("commit.auto"));
}

#[test]
fn a_closed_stdout_receiver_does_not_hang_or_undo_the_command() {
    let lab = Lab::new();
    let mut child = lab
        .command(&["--json", "config", "commit.auto", "false"])
        .stdin(Stdio::null())
        .stdout(Stdio::from(closed_pipe_writer()))
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let output = child.wait_with_output().unwrap();
            panic!("the command did not exit after its stdout receiver closed: {output:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    assert_eq!(lab.persisted_config()["commit.auto"], "false");
}
