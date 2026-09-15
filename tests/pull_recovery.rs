//! Recovery text retains the selected repository and survives native shell argument parsing.

use agit::domain::meta;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

struct Hub {
    url: String,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<usize>>,
}

impl Hub {
    fn new(source: &Path) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let body = serde_json::json!({
            "agent_id":"aaaaaaaa-0000-4000-8000-000000000090",
            "owner":"alice", "name":"notes", "visibility":"public", "clone_url":source,
        })
        .to_string();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let worker = std::thread::spawn(move || {
            let mut requests = 0;
            while !stopped.load(Ordering::Acquire) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("metadata accept failed: {error}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(3)))
                    .unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    assert!(request.len() < 8192, "metadata request too large");
                    let mut byte = [0];
                    stream.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                let request = String::from_utf8(request).unwrap();
                if request.starts_with("GET /api/cli/version ") {
                    write!(
                        stream,
                        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .unwrap();
                    continue;
                }
                assert!(request.starts_with("GET /api/agents/alice/notes HTTP/1.1\r\n"));
                requests += 1;
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
            requests
        });
        Self {
            url,
            stop,
            worker: Some(worker),
        }
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
    home: PathBuf,
    work: PathBuf,
    source: PathBuf,
    hub: Hub,
}

impl Lab {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("agit");
        let work = root.path().join("work");
        let source = root.path().join("source");
        fs::create_dir_all(&work).unwrap();
        Self {
            hub: Hub::new(&source),
            root,
            home,
            work,
            source,
        }
    }

    fn command(&self, program: impl AsRef<std::ffi::OsStr>, session: Option<&str>) -> Command {
        let mut command = Command::new(program);
        command.env_clear();
        for key in [
            "PATH",
            "SystemRoot",
            "WINDIR",
            "ComSpec",
            "PATHEXT",
            "TEMP",
            "TMP",
        ] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        command
            .current_dir(&self.work)
            .env("HOME", self.root.path())
            .env("USERPROFILE", self.root.path())
            .env("AGIT_HOME", &self.home)
            .env("AGIT_HUB_URL", &self.hub.url)
            .env("GIT_CONFIG_GLOBAL", self.root.path().join("empty-config"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Recovery fixture")
            .env("GIT_AUTHOR_EMAIL", "recovery@example.invalid")
            .env("GIT_COMMITTER_NAME", "Recovery fixture")
            .env("GIT_COMMITTER_EMAIL", "recovery@example.invalid")
            .env("CI", "1")
            .env("NO_COLOR", "1")
            .env("AGIT_TUI", "0")
            .stdin(Stdio::null());
        if let Some(session) = session {
            command.env("AGIT_SESSION", session);
        }
        command
    }

    fn git(&self, repo: &Path, args: &[&str]) -> String {
        let output = self
            .command("git", None)
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}: {output:?}");
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn shell(&self, text: &str, session: Option<&str>) -> Output {
        #[cfg(windows)]
        let mut command = {
            let mut command = self.command("powershell.exe", session);
            command.args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &format!("{text}; exit $LASTEXITCODE"),
            ]);
            command
        };
        #[cfg(not(windows))]
        let mut command = {
            let mut command = self.command("sh", session);
            command.args(["-c", text]);
            command
        };
        let mut paths = vec![
            Path::new(env!("CARGO_BIN_EXE_agit"))
                .parent()
                .unwrap()
                .to_path_buf(),
        ];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        command
            .env("PATH", std::env::join_paths(paths).unwrap())
            .output()
            .unwrap()
    }
}

#[test]
fn divergence_hints_keep_the_explicit_repo_without_or_with_foreign_process_identity() {
    for branch in ["topic", "topic;literal'quoted‘’‚‛"] {
        for session in [None, Some("bob/other@topic")] {
            let mut lab = Lab::new();
            fs::create_dir_all(&lab.source).unwrap();
            lab.git(&lab.source, &["init", "--quiet", "--initial-branch=main"]);
            let snapshot = meta::Meta::new(
                "agit-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                "codex".into(),
                lab.work.to_string_lossy().into_owned(),
            );
            meta::write(&lab.source, &snapshot).unwrap();
            fs::write(lab.source.join(meta::LOG_FILE), "").unwrap();
            fs::write(lab.source.join(meta::VIEW_FILE), "").unwrap();
            fs::write(lab.source.join("AGENTS.md"), "Shared context.\n").unwrap();
            lab.git(&lab.source, &["add", "-A"]);
            lab.git(&lab.source, &["commit", "-qm", "shared base"]);
            let base = lab.git(&lab.source, &["rev-parse", "HEAD"]);
            lab.git(&lab.source, &["checkout", "-qb", branch]);
            lab.git(
                &lab.source,
                &["commit", "--allow-empty", "-qm", "remote advance"],
            );
            let remote = lab.git(&lab.source, &["rev-parse", "HEAD"]);
            let selected = lab.home.join("repos/alice/notes");
            fs::create_dir_all(selected.parent().unwrap()).unwrap();
            let output = lab
                .command("git", None)
                .args(["clone", "--quiet"])
                .arg(&lab.source)
                .arg(&selected)
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            lab.git(&selected, &["reset", "--hard", &base]);
            lab.git(
                &selected,
                &["commit", "--allow-empty", "-qm", "local advance"],
            );
            let local = lab.git(&selected, &["rev-parse", "HEAD"]);
            let foreign = lab.home.join("repos/bob/other");
            fs::create_dir_all(&foreign).unwrap();
            lab.git(&foreign, &["init", "--quiet", "--initial-branch=topic"]);
            fs::write(foreign.join("untouched"), "foreign identity\n").unwrap();
            lab.git(&foreign, &["add", "-A"]);
            lab.git(&foreign, &["commit", "-qm", "foreign base"]);
            let foreign_refs = lab.git(&foreign, &["show-ref"]);
            let output = lab
                .command(env!("CARGO_BIN_EXE_agit"), session)
                .args(["pull", "alice/notes", "-b", branch])
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(4), "{output:?}");
            let diagnostic = String::from_utf8_lossy(&output.stderr);
            let recovery: Vec<_> = diagnostic
                .lines()
                .filter(|line| line.contains("reconcile:") || line.contains("keep both:"))
                .map(|line| line.split('`').nth(1).unwrap())
                .collect();
            assert_eq!(recovery.len(), 2, "{diagnostic}");
            let merge = lab.shell(&format!("{} --dry-run", recovery[0]), session);
            assert!(merge.status.success(), "{}: {merge:?}", recovery[0]);
            let report = String::from_utf8_lossy(&merge.stdout);
            assert!(report.contains(&base[..9]), "{report}");
            let fork = recovery[1].strip_suffix(" --resume").unwrap();
            let fork_output = lab.shell(fork, session);
            assert!(fork_output.status.success(), "{fork}: {fork_output:?}");
            assert_eq!(
                lab.git(
                    &selected,
                    &["rev-parse", &format!("refs/heads/{branch}-remote^")]
                ),
                remote
            );
            assert_eq!(
                lab.git(&selected, &["rev-parse", &format!("refs/heads/{branch}")]),
                local
            );
            assert_eq!(lab.git(&foreign, &["show-ref"]), foreign_refs);
            assert!(!lab.home.join("store/codex").exists());
            lab.hub.stop.store(true, Ordering::Release);
            assert_eq!(lab.hub.worker.take().unwrap().join().unwrap(), 1);
        }
    }
}
