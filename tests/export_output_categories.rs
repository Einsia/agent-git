//! A valid export with an unavailable output path is a local precondition failure.

use agit::domain::{meta, storage, transcript};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

struct Lab {
    root: tempfile::TempDir,
    home: PathBuf,
    work: PathBuf,
    repo: PathBuf,
    hub: TcpListener,
    raw: String,
    head: String,
}

impl Lab {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("agit");
        let work = root.path().join("work");
        let repo = home.join("repos/local/qa");
        for path in [
            &repo,
            &work,
            &root.path().join("templates"),
            &root.path().join("tmp"),
        ] {
            fs::create_dir_all(path).unwrap();
        }
        let hub = TcpListener::bind("127.0.0.1:0").unwrap();
        hub.set_nonblocking(true).unwrap();
        let raw = format!(
            "{}\n",
            json!({"type":"response_item","payload":{
                "type":"message","role":"user","content":[{
                    "type":"input_text","text":"SYNTHETIC-EXPORT-CONTENT"
                }]
            }})
        );
        let mut lab = Self {
            root,
            home,
            work,
            repo,
            hub,
            raw,
            head: String::new(),
        };
        lab.git(&["init", "--quiet", "--initial-branch=main"]);
        let metadata =
            meta::Meta::new(meta::mint_session_id(), "codex".into(), "/synthetic".into());
        meta::write(&lab.repo, &metadata).unwrap();
        let wrapped = transcript::wrap_lines(&lab.raw, "codex", &metadata.session);
        storage::write_snapshot(&lab.repo, &wrapped, &wrapped).unwrap();
        lab.git(&["add", "-A"]);
        lab.git(&["commit", "--quiet", "-m", "synthetic export source"]);
        lab.head = lab.git(&["rev-parse", "HEAD"]);
        // Finish startup before refusal snapshots, and prove the selected source is readable.
        let output = lab.export("human", "jsonl", Path::new("-"));
        assert_eq!(output.status.code(), Some(0), "{output:?}");
        assert_eq!(output.stdout, lab.raw.as_bytes());
        lab.assert_no_network();
        lab
    }

    fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = Command::new(program);
        command.env_clear();
        for key in ["PATH", "SystemRoot", "WINDIR", "ComSpec", "PATHEXT"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        command
            .env("HOME", self.root.path())
            .env("USERPROFILE", self.root.path())
            .env("AGIT_HOME", &self.home)
            .env(
                "AGIT_HUB_URL",
                format!("http://{}", self.hub.local_addr().unwrap()),
            )
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", self.root.path().join("empty-config"))
            .env("GIT_TEMPLATE_DIR", self.root.path().join("templates"))
            .env("GIT_AUTHOR_NAME", "Export category fixture")
            .env("GIT_AUTHOR_EMAIL", "export@example.invalid")
            .env("GIT_COMMITTER_NAME", "Export category fixture")
            .env("GIT_COMMITTER_EMAIL", "export@example.invalid")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("TMP", self.root.path().join("tmp"))
            .env("TEMP", self.root.path().join("tmp"))
            .env("TMPDIR", self.root.path().join("tmp"))
            .env("CI", "1")
            .env("NO_COLOR", "1")
            .env("AGIT_TUI", "0")
            .current_dir(&self.work)
            .stdin(Stdio::null());
        command
    }

    fn git(&self, args: &[&str]) -> String {
        let output = self
            .command("git")
            .arg("-C")
            .arg(&self.repo)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{args:?}: {output:?}");
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn export(&self, mode: &str, format: &str, path: &Path) -> Output {
        self.export_command(mode, format, path).output().unwrap()
    }

    fn export_command(&self, mode: &str, format: &str, path: &Path) -> Command {
        let mut command = self.command(env!("CARGO_BIN_EXE_agit"));
        if mode == "quiet" {
            command.arg("--quiet");
        }
        if let Some(version) = mode.strip_prefix("json") {
            command.args(["--json", "--json-version", version]);
        }
        command
            .args([
                "export",
                &format!("local/qa@{}", self.head),
                "--format",
                format,
                "-o",
            ])
            .arg(path);
        command
    }

    fn state(&self) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
        walkdir::WalkDir::new(self.root.path())
            .into_iter()
            .map(|entry| {
                let entry = entry.unwrap();
                assert!(!entry.file_type().is_symlink());
                let data = entry
                    .file_type()
                    .is_file()
                    .then(|| fs::read(entry.path()).unwrap());
                (
                    entry
                        .path()
                        .strip_prefix(self.root.path())
                        .unwrap()
                        .to_owned(),
                    data,
                )
            })
            .collect()
    }

    fn assert_no_network(&self) {
        match self.hub.accept() {
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            other => panic!("export attempted a network connection: {other:?}"),
        }
    }
}

fn assert_output(output: &Output, mode: &str, code: i32, message: Option<&str>) {
    assert_eq!(output.status.code(), Some(code), "{mode}: {output:?}");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("SYNTHETIC-EXPORT-CONTENT"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("SYNTHETIC-EXPORT-CONTENT"));
    if let Some(version) = mode.strip_prefix("json") {
        assert!(output.stderr.is_empty(), "{output:?}");
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["schema"], "cli-output");
        assert_eq!(value["schema_version"], version.parse::<u32>().unwrap());
        assert_eq!(value["command"], "export");
        assert_eq!(value["exit_code"], code);
        assert_eq!(value["ok"], code == 0);
        if version == "1" {
            assert!(value.get("fix").is_none());
        } else {
            assert_eq!(value["fix"], json!([]));
        }
        if let Some(message) = message {
            assert_eq!(value["result"]["format"], "empty");
            assert!(
                value["diagnostics"]["stderr"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|row| row["level"] == "error"
                        && row["message"].as_str().unwrap().contains(message))
            );
        }
    } else if let Some(message) = message {
        assert!(output.stdout.is_empty(), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains(message));
    }
}

#[test]
fn unavailable_export_paths_are_preconditions_without_mutating_the_source() {
    for mode in ["human", "quiet", "json1", "json2"] {
        let lab = Lab::new();
        let occupied = lab.work.join("occupied");
        fs::create_dir(&occupied).unwrap();
        fs::write(occupied.join("sentinel"), "owned sentinel\n").unwrap();
        let missing = lab.work.join("absent-parent/output.jsonl");
        let refs = lab.git(&["show-ref"]);
        let before = lab.state();
        for path in [&occupied, &missing] {
            assert_output(
                &lab.export(mode, "jsonl", path),
                mode,
                4,
                Some("cannot write export"),
            );
            assert_eq!(lab.state(), before, "{mode}: {}", path.display());
            assert_eq!(lab.git(&["rev-parse", "HEAD"]), lab.head);
            assert_eq!(lab.git(&["show-ref"]), refs);
            lab.assert_no_network();
        }
        let output = lab.work.join("accepted.jsonl");
        assert_output(
            &lab.export(mode, "unsupported", &output),
            mode,
            2,
            Some("unknown format"),
        );
        assert_eq!(lab.state(), before);
        assert_output(&lab.export(mode, "jsonl", &output), mode, 0, None);
        assert_eq!(fs::read(&output).unwrap(), lab.raw.as_bytes());
        let mut after = lab.state();
        assert_eq!(
            after.remove(&PathBuf::from("work/accepted.jsonl")),
            Some(Some(lab.raw.as_bytes().to_vec()))
        );
        assert_eq!(
            after, before,
            "only the selected output file may be created"
        );
        assert_eq!(lab.git(&["rev-parse", "HEAD"]), lab.head);
        assert_eq!(lab.git(&["show-ref"]), refs);
        lab.assert_no_network();
    }
}

#[test]
fn stdout_exports_deliver_the_selected_content_without_changing_owned_state() {
    for mode in ["human", "quiet", "json1", "json2"] {
        let lab = Lab::new();
        let before = lab.state();
        let output = lab.export(mode, "jsonl", Path::new("-"));
        assert_eq!(output.status.code(), Some(0), "{output:?}");
        assert!(output.stderr.is_empty(), "{output:?}");
        if let Some(version) = mode.strip_prefix("json") {
            let value: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(value["schema_version"], version.parse::<u32>().unwrap());
            assert_eq!(value["command"], "export");
            assert_eq!(value["exit_code"], 0);
            assert_eq!(value["ok"], true);
            assert_eq!(
                value["result"]["value"],
                serde_json::from_str::<Value>(&lab.raw).unwrap()
            );
        } else {
            assert_eq!(output.stdout, lab.raw.as_bytes());
        }
        assert_eq!(lab.state(), before);
        lab.assert_no_network();
    }
}

#[cfg(target_os = "linux")]
#[test]
fn full_output_device_refuses_export_and_the_final_json_envelope() {
    for mode in ["human", "quiet", "json1", "json2"] {
        let lab = Lab::new();
        let before = lab.state();
        let full = fs::OpenOptions::new()
            .write(true)
            .open("/dev/full")
            .unwrap();
        let output = lab
            .export_command(mode, "jsonl", Path::new("-"))
            .stdout(Stdio::from(full))
            .stderr(Stdio::piped())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(4), "{mode}: {output:?}");
        assert!(output.stdout.is_empty());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("SYNTHETIC-EXPORT-CONTENT"));
        if !mode.starts_with("json") {
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("cannot write export to stdout")
            );
        }
        assert_eq!(lab.state(), before);
        lab.assert_no_network();
    }
}

#[cfg(unix)]
#[test]
fn closed_output_pipe_keeps_the_unix_sigpipe_contract() {
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::os::unix::process::ExitStatusExt;
    for mode in ["human", "quiet", "json1", "json2"] {
        let lab = Lab::new();
        let before = lab.state();
        let mut descriptors = [-1; 2];
        assert_eq!(unsafe { libc::pipe(descriptors.as_mut_ptr()) }, 0);
        let reader = unsafe { OwnedFd::from_raw_fd(descriptors[0]) };
        let writer = unsafe { OwnedFd::from_raw_fd(descriptors[1]) };
        drop(reader);
        let output = lab
            .export_command(mode, "jsonl", Path::new("-"))
            .stdout(Stdio::from(writer))
            .stderr(Stdio::piped())
            .output()
            .unwrap();
        assert_eq!(
            output.status.signal(),
            Some(libc::SIGPIPE),
            "{mode}: {output:?}"
        );
        assert!(output.stdout.is_empty());
        assert_eq!(lab.state(), before);
        lab.assert_no_network();
    }
}

#[cfg(all(windows, target_env = "msvc"))]
#[test]
fn closed_output_pipe_refuses_export_and_the_final_json_envelope_on_windows() {
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    use windows_sys::Win32::System::Pipes::CreatePipe;
    for mode in ["human", "quiet", "json1", "json2"] {
        let lab = Lab::new();
        let before = lab.state();
        let mut read = std::ptr::null_mut();
        let mut write = std::ptr::null_mut();
        assert_ne!(
            unsafe { CreatePipe(&mut read, &mut write, std::ptr::null(), 0) },
            0
        );
        let reader = unsafe { OwnedHandle::from_raw_handle(read) };
        let writer = unsafe { OwnedHandle::from_raw_handle(write) };
        drop(reader);
        let output = lab
            .export_command(mode, "jsonl", Path::new("-"))
            .stdout(Stdio::from(writer))
            .stderr(Stdio::piped())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(4), "{mode}: {output:?}");
        assert!(output.stdout.is_empty());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("SYNTHETIC-EXPORT-CONTENT"));
        if !mode.starts_with("json") {
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("cannot write export to stdout")
            );
        }
        assert_eq!(lab.state(), before);
        lab.assert_no_network();
    }
}
