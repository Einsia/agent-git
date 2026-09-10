//! Acquiring a repository does not make an absent selected branch or version a generic failure.

use agit::domain::{meta, repo::Repo};
use agit::hub::identity::{self, RemoteIdentity};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

const AGENT_ID: &str = "aaaaaaaa-0000-4000-8000-000000000135";

struct Hub {
    base: String,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<(usize, usize)>>,
    git_status: Option<u16>,
}

impl Hub {
    fn new(source: &Path, status: u16) -> Self {
        Self::with_git_status(source, status, None)
    }

    fn with_git_status(source: &Path, status: u16, git_status: Option<u16>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        // Successful acquisition uses an owned source; HTTP rejections still invoke real Git.
        let clone_url = git_status.map_or_else(
            || json!(source),
            |_| json!(format!("{base}/alice/paper.git")),
        );
        let response = if status == 200 {
            json!({"agent_id":AGENT_ID,"owner":"alice","name":"paper",
                "visibility":"public","clone_url":clone_url})
        } else {
            json!({"error":"synthetic authentication refusal; HTTP 401"})
        }
        .to_string();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let worker = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(30);
            let mut count = 0;
            let mut git_count = 0;
            while !stopped.load(Ordering::Acquire) {
                assert!(
                    Instant::now() < deadline,
                    "metadata fixture deadline elapsed"
                );
                let (mut stream, _) = match listener.accept() {
                    Ok(stream) => stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("metadata fixture accept failed: {error}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut headers = Vec::new();
                while !headers.ends_with(b"\r\n\r\n") {
                    assert!(
                        headers.len() < 8192,
                        "metadata headers exceed fixture budget"
                    );
                    let mut byte = [0];
                    stream.read_exact(&mut byte).unwrap();
                    headers.push(byte[0]);
                }
                let headers = String::from_utf8(headers).unwrap();
                assert!(!headers.to_ascii_lowercase().contains("authorization:"));
                if headers.starts_with("GET /api/agents/alice/paper HTTP/1.1\r\n") {
                    count += 1;
                    assert_eq!(count, 1, "the metadata request was replayed");
                    if status != 0 {
                        write!(stream, "HTTP/1.1 {status} Synthetic\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).unwrap();
                    }
                } else {
                    assert!(
                        headers.starts_with(
                            "GET /alice/paper.git/info/refs?service=git-upload-pack HTTP/1.1\r\n"
                        ),
                        "{headers}"
                    );
                    assert!(
                        headers
                            .lines()
                            .any(|line| line.eq_ignore_ascii_case(&format!(
                                "X-AgentGit-Expected-Agent-Id: {AGENT_ID}"
                            )))
                    );
                    git_count += 1;
                    assert_eq!(git_count, 1, "the Git request was replayed");
                    let status = git_status.expect("unexpected Git HTTP request");
                    write!(stream, "HTTP/1.1 {status} Synthetic\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                }
            }
            (count, git_count)
        });
        Self {
            base,
            stop,
            worker: Some(worker),
            git_status,
        }
    }

    fn finish(&mut self) {
        self.finish_counts((1, usize::from(self.git_status.is_some())));
    }

    fn finish_counts(&mut self, counts: (usize, usize)) {
        self.stop.store(true, Ordering::Release);
        assert_eq!(self.worker.take().unwrap().join().unwrap(), counts);
    }
}

impl Drop for Hub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct Lab {
    root: tempfile::TempDir,
    source: PathBuf,
    home: PathBuf,
    work: PathBuf,
    hub: Hub,
    head: String,
    version: String,
}

impl Lab {
    fn new(status: u16) -> Self {
        Self::with_git_status(status, None)
    }

    fn with_git_status(status: u16, git_status: Option<u16>) -> Self {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let home = root.path().join("agit");
        let work = root.path().join("work");
        for path in [
            &source,
            &work,
            &root.path().join("templates"),
            &root.path().join("tmp"),
        ] {
            fs::create_dir_all(path).unwrap();
        }
        let hub = match git_status {
            Some(status) => Hub::with_git_status(&source, 200, Some(status)),
            None => Hub::new(&source, status),
        };
        let mut lab = Self {
            root,
            source,
            home,
            work,
            hub,
            head: String::new(),
            version: String::new(),
        };
        lab.git(&lab.source, &["init", "--quiet", "--initial-branch=main"]);
        meta::write(&lab.source, &meta::Meta::new_file_line()).unwrap();
        fs::write(
            lab.source.join("AGENTS.md"),
            "Synthetic shared-file source.\n",
        )
        .unwrap();
        lab.git(&lab.source, &["add", "-A"]);
        lab.git(
            &lab.source,
            &["commit", "--quiet", "-m", "synthetic file line"],
        );
        lab.head = lab.git(&lab.source, &["rev-parse", "HEAD"]);
        lab.version = meta::id_from_sha(&lab.head);
        lab.git(&lab.source, &["tag", &lab.version]);
        lab
    }

    fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut cmd = Command::new(program);
        cmd.env_clear();
        for key in ["PATH", "SystemRoot", "WINDIR", "ComSpec", "PATHEXT"] {
            if let Some(value) = std::env::var_os(key) {
                cmd.env(key, value);
            }
        }
        cmd.env("HOME", self.root.path())
            .env("USERPROFILE", self.root.path())
            .env("AGIT_HOME", &self.home)
            .env("AGIT_HUB_URL", &self.hub.base)
            .env("TMP", self.root.path().join("tmp"))
            .env("TEMP", self.root.path().join("tmp"))
            .env("TMPDIR", self.root.path().join("tmp"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", self.root.path().join("empty-config"))
            .env("GIT_TEMPLATE_DIR", self.root.path().join("templates"))
            .env("GIT_AUTHOR_NAME", "Clone category fixture")
            .env("GIT_AUTHOR_EMAIL", "clone@example.invalid")
            .env("GIT_COMMITTER_NAME", "Clone category fixture")
            .env("GIT_COMMITTER_EMAIL", "clone@example.invalid")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("CI", "1")
            .env("NO_COLOR", "1")
            .env("AGIT_TUI", "0")
            .current_dir(&self.work)
            .stdin(Stdio::null());
        cmd
    }

    fn git(&self, dir: &Path, args: &[&str]) -> String {
        let out = self
            .command("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    fn clone_ref(&self, selected: &str, mode: &str) -> Output {
        self.clone_command(selected, mode).output().unwrap()
    }

    fn clone_command(&self, selected: &str, mode: &str) -> Command {
        let mut cmd = self.command(env!("CARGO_BIN_EXE_agit"));
        if mode == "quiet" {
            cmd.arg("--quiet");
        }
        if let Some(version) = mode.strip_prefix("json") {
            cmd.args(["--json", "--json-version", version]);
        }
        cmd.args(["clone", &format!("alice/paper@{selected}"), "--no-bind"]);
        cmd
    }

    fn destination(&self) -> PathBuf {
        self.home.join("repos/alice/paper")
    }
}

fn inventory(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    walkdir::WalkDir::new(root)
        .into_iter()
        .map(|entry| {
            let entry = entry.unwrap();
            assert!(!entry.file_type().is_symlink());
            let data = entry
                .file_type()
                .is_file()
                .then(|| fs::read(entry.path()).unwrap());
            (entry.path().strip_prefix(root).unwrap().to_owned(), data)
        })
        .collect()
}

fn assert_output(output: &Output, mode: &str, code: i32, diagnostic: Option<&str>) {
    assert_eq!(output.status.code(), Some(code), "{mode}: {output:?}");
    let text = if let Some(version) = mode.strip_prefix("json") {
        assert!(output.stderr.is_empty(), "{output:?}");
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["schema_version"], version.parse::<u32>().unwrap());
        assert_eq!(value["command"], "clone");
        assert_eq!(value["exit_code"], code);
        assert_eq!(value["ok"], code == 0);
        if version == "1" {
            assert!(value.get("fix").is_none());
        } else {
            assert_eq!(value["fix"], json!([]));
        }
        value["diagnostics"]["stderr"].to_string()
    } else {
        String::from_utf8(output.stderr.clone()).unwrap()
    };
    if let Some(diagnostic) = diagnostic {
        assert!(text.contains(diagnostic), "{mode}: {text}");
    }
}

#[test]
fn missing_selected_clone_reference_keeps_the_acquired_repository_and_returns_ref() {
    for mode in ["human", "quiet", "json1", "json2"] {
        for selected in ["absent".to_owned(), format!("agit-{}", "b".repeat(40))] {
            let mut lab = Lab::new(200);
            let before = inventory(&lab.source);
            let output = lab.clone_ref(&selected, mode);
            assert_output(
                &output,
                mode,
                3,
                Some(if selected == "absent" {
                    "has no branch"
                } else {
                    "has no version"
                }),
            );
            lab.hub.finish();
            assert_eq!(inventory(&lab.source), before);
            let dest = lab.destination();
            assert_eq!(lab.git(&dest, &["rev-parse", "HEAD"]), lab.head);
            assert_eq!(lab.git(&dest, &["symbolic-ref", "HEAD"]), "refs/heads/main");
            assert_eq!(
                lab.git(
                    &dest,
                    &["for-each-ref", "--format=%(refname)", "refs/heads"]
                ),
                "refs/heads/main"
            );
            assert_eq!(lab.git(&dest, &["tag", "--list"]), lab.version);
            assert_eq!(
                identity::read(&Repo::at(&dest)).unwrap(),
                Some(RemoteIdentity::new(&lab.hub.base, AGENT_ID).unwrap())
            );
            assert!(!lab.home.join("workspaces").exists());
            assert!(!lab.home.join("store").exists());
        }
    }
}

#[test]
fn exact_clone_targets_and_authentication_keep_their_own_categories() {
    for mode in ["human", "quiet", "json1", "json2"] {
        for version in [false, true] {
            let mut lab = Lab::new(200);
            let selected = if version {
                lab.version.clone()
            } else {
                "main".into()
            };
            assert_output(&lab.clone_ref(&selected, mode), mode, 0, None);
            lab.hub.finish();
            assert_eq!(
                lab.git(&lab.destination(), &["rev-parse", "HEAD"]),
                lab.head
            );
            assert!(!lab.home.join("workspaces").exists());
        }
        let mut lab = Lab::new(401);
        assert_output(
            &lab.clone_ref("absent", mode),
            mode,
            5,
            Some("synthetic authentication refusal"),
        );
        lab.hub.finish();
        assert!(!lab.destination().exists());
    }
}

#[test]
fn clone_client_failures_use_transport_types_and_do_not_replay_or_create_identity() {
    for mode in ["human", "quiet", "json1", "json2"] {
        for (status, code) in [(401, 5), (403, 6), (404, 6), (500, 6), (0, 6)] {
            let mut lab = Lab::new(status);
            let primed = lab
                .command(env!("CARGO_BIN_EXE_agit"))
                .args(["config", "hub.url"])
                .output()
                .unwrap();
            assert!(primed.status.success(), "{primed:?}");
            let before = inventory(lab.root.path());
            assert_output(
                &lab.clone_ref("main", mode),
                mode,
                code,
                Some("remote request failed"),
            );
            lab.hub.finish();
            assert_eq!(inventory(lab.root.path()), before);
            assert!(!lab.destination().exists());
        }

        let mut lab = Lab::new(200);
        let primed = lab
            .command(env!("CARGO_BIN_EXE_agit"))
            .args(["config", "hub.url"])
            .output()
            .unwrap();
        assert!(primed.status.success(), "{primed:?}");
        let before = inventory(lab.root.path());
        let output = lab
            .clone_command("main", mode)
            .arg("--mine")
            .output()
            .unwrap();
        assert_output(&output, mode, 5, Some("not signed in"));
        lab.hub.finish_counts((0, 0));
        assert_eq!(inventory(lab.root.path()), before);
    }
}

#[test]
fn actual_git_http_failures_preserve_authentication_and_the_fixed_repository_identity() {
    for mode in ["human", "quiet", "json1", "json2"] {
        for (status, code) in [(401, 5), (403, 5), (404, 6), (500, 6)] {
            let mut lab = Lab::with_git_status(200, Some(status));
            let before = inventory(&lab.source);
            assert_output(
                &lab.clone_ref("main", mode),
                mode,
                code,
                Some("clone failed"),
            );
            lab.hub.finish();
            assert_eq!(inventory(&lab.source), before);
            assert!(!lab.destination().join(".git").exists());
            assert!(!lab.home.join("workspaces").exists());
            assert!(!lab.home.join("store").exists());
        }
    }
}

/// Invalid local request configuration cannot initiate metadata or Git transport.
#[test]
fn clone_configuration_refusals_preserve_usage_without_sending_requests() {
    for mode in ["human", "quiet", "json1", "json2"] {
        for broken in ["credentials", "hub"] {
            let mut lab = Lab::new(200);
            let primed = lab
                .command(env!("CARGO_BIN_EXE_agit"))
                .args(["config", "hub.url"])
                .output()
                .unwrap();
            assert!(primed.status.success(), "{primed:?}");
            let mut command = lab.clone_command("main", mode);
            if broken == "credentials" {
                let key = agit::infra::hub_authority::HubAuthority::parse(&lab.hub.base)
                    .unwrap()
                    .storage_key();
                let dir = lab.home.join("credentials");
                fs::create_dir_all(&dir).unwrap();
                fs::write(dir.join(format!("{key}.json")), b"{ invalid JSON").unwrap();
            } else {
                command.env("AGIT_HUB_URL", format!("{}#invalid", lab.hub.base));
            }
            let before = inventory(lab.root.path());
            let output = command.output().unwrap();
            assert_output(&output, mode, 2, Some("invalid Hub request configuration"));
            lab.hub.finish_counts((0, 0));
            assert_eq!(inventory(lab.root.path()), before, "{mode}/{broken}");
        }
    }
}

/// An occupied checkout destination fails before Git transport and preserves its contents.
#[test]
fn occupied_clone_destinations_are_preconditions_without_git_requests() {
    for mode in ["human", "quiet", "json1", "json2"] {
        for occupied in ["directory", "file"] {
            let mut lab = Lab::with_git_status(200, Some(500));
            let primed = lab
                .command(env!("CARGO_BIN_EXE_agit"))
                .args(["config", "hub.url"])
                .output()
                .unwrap();
            assert!(primed.status.success(), "{primed:?}");
            let dest = lab.destination();
            if occupied == "directory" {
                fs::create_dir_all(&dest).unwrap();
                fs::write(dest.join("keep.txt"), b"Owned destination contents").unwrap();
            } else {
                fs::create_dir_all(dest.parent().unwrap()).unwrap();
                fs::write(&dest, b"Owned destination file").unwrap();
            }
            let source = inventory(&lab.source);
            let destination = if dest.is_dir() {
                Some(inventory(&dest))
            } else {
                None
            };
            let output = lab.clone_ref("main", mode);
            assert_output(&output, mode, 4, Some("clone destination"));
            lab.hub.finish_counts((1, 0));
            assert_eq!(inventory(&lab.source), source);
            if let Some(before) = destination {
                assert_eq!(inventory(&dest), before);
            } else {
                assert_eq!(fs::read(&dest).unwrap(), b"Owned destination file");
            }
            assert!(!dest.join(".git").exists());
            assert!(!lab.home.join("workspaces").exists());
            assert!(!lab.home.join("store").exists());
        }
    }
}
