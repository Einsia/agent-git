//! Reverse lookup ambiguity requires an explicit choice before any checkout or binding.

use agit::domain::meta;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

const AGENT_ID: &str = "aaaaaaaa-0000-4000-8000-000000000001";

struct Hub {
    base: String,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Hub {
    fn new(count: usize, lookup_status: u16) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let url = base.clone();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&requests);
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            while !stopped.load(Ordering::Relaxed) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                        continue;
                    }
                    Err(e) => panic!("local hub accept: {e}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .unwrap();
                let mut line = String::new();
                let mut reader = BufReader::new(&mut stream);
                reader.read_line(&mut line).unwrap();
                let path = line.split_whitespace().nth(1).unwrap().to_string();
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).unwrap() == 0 || header == "\r\n" {
                        break;
                    }
                }
                seen.lock().unwrap().push(path.clone());
                let agent = |owner: &str| json!({"agent_id":AGENT_ID,"owner":owner,"name":"paper","visibility":"public","clone_url":format!("{url}/{owner}/paper.git")});
                let (status, body) = if path.starts_with("/api/agents/for-repo?") {
                    let body = if lookup_status == 200 {
                        Value::Array(
                            ["alice", "org"]
                                .into_iter()
                                .take(count)
                                .map(agent)
                                .collect(),
                        )
                    } else {
                        json!({"error":"synthetic lookup failure"})
                    };
                    (lookup_status, body)
                } else if path == "/api/agents/alice/paper" {
                    (200, agent("alice"))
                } else {
                    (404, json!({"error":"synthetic unavailable fetch"}))
                };
                let body = body.to_string();
                write!(stream,"HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
            }
        });
        Self {
            base,
            requests,
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Hub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.take().unwrap().join().unwrap();
    }
}

struct Lab {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    work: PathBuf,
    hub: Hub,
}

impl Lab {
    fn new(count: usize, lookup_status: u16, origin: bool) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("agit");
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        let lab = Self {
            _tmp: tmp,
            home,
            work,
            hub: Hub::new(count, lookup_status),
        };
        lab.git(&lab.work, &["init", "--quiet"]);
        if origin {
            lab.git(
                &lab.work,
                &[
                    "remote",
                    "add",
                    "origin",
                    "https://example.invalid/org/project.git",
                ],
            );
        }
        lab
    }

    fn command(&self, program: &str) -> Command {
        let mut command = Command::new(program);
        command.env_clear();
        for key in ["PATH", "SystemRoot", "TEMP", "TMP"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        command
            .current_dir(&self.work)
            .env("HOME", self._tmp.path())
            .env("USERPROFILE", self._tmp.path())
            .env("AGIT_HOME", &self.home)
            .env("AGIT_HUB_URL", &self.hub.base)
            .env("CI", "1")
            .env("AGIT_TUI", "0")
            .env("NO_COLOR", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0");
        command
    }

    fn git(&self, directory: &Path, args: &[&str]) -> Vec<u8> {
        assert!(directory.starts_with(self._tmp.path()));
        let out = self
            .command("git")
            .current_dir(directory)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        out.stdout
    }

    #[cfg(unix)]
    fn run_bounded(&self, mut command: Command) -> Output {
        use std::io::{Read, Seek};
        use std::os::unix::process::CommandExt;
        use std::process::Stdio;
        use std::time::{Duration, Instant};

        let mut stdout = tempfile::tempfile_in(self._tmp.path()).unwrap();
        let mut stderr = tempfile::tempfile_in(self._tmp.path()).unwrap();
        let mut child = command
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(stdout.try_clone().unwrap())
            .stderr(stderr.try_clone().unwrap())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                result => {
                    // The child owns its process group, so cleanup includes its Git subprocesses.
                    unsafe {
                        libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
                    }
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("clone fixture subprocess did not complete: {result:?}");
                }
            }
        };
        let read = |file: &mut std::fs::File| {
            file.rewind().unwrap();
            let mut bytes = Vec::new();
            file.take(1024 * 1024).read_to_end(&mut bytes).unwrap();
            bytes
        };
        Output {
            status,
            stdout: read(&mut stdout),
            stderr: read(&mut stderr),
        }
    }

    fn clone(&self, args: &[&str]) -> Output {
        self.command(env!("CARGO_BIN_EXE_agit"))
            .arg("clone")
            .args(args)
            .output()
            .unwrap()
    }

    fn assert_no_target(&self) {
        for path in [
            "repos/alice/paper",
            "repos/org/paper",
            "workspaces",
            "store",
        ] {
            assert!(
                !self.home.join(path).exists(),
                "unexpected target state: {path}"
            );
        }
        let requests = self.hub.requests.lock().unwrap();
        assert!(
            requests
                .iter()
                .all(|path| path.starts_with("/api/agents/for-repo?")),
            "{requests:?}"
        );
    }
}

#[test]
fn ambiguous_clone_is_interactive_refusal_with_stderr_candidates() {
    for args in [vec![], vec!["--yes"]] {
        let lab = Lab::new(2, 200, true);
        let before = std::fs::read(lab.work.join(".git/config")).unwrap();
        let out = lab.clone(&args);
        assert_eq!(
            out.status.code(),
            Some(8),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            out.stdout.is_empty(),
            "{}",
            String::from_utf8_lossy(&out.stdout)
        );
        let errors = String::from_utf8_lossy(&out.stderr);
        for expected in ["alice/paper", "org/paper", "agit clone <owner>/<agent>"] {
            assert!(errors.contains(expected), "{errors}");
        }
        assert_eq!(std::fs::read(lab.work.join(".git/config")).unwrap(), before);
        lab.assert_no_target();
    }
}

/// Cancellation requires the rendered choice to be refused without selecting or binding a target.
#[cfg(all(unix, feature = "rc"))]
#[test]
fn ambiguous_clone_tty_cancellation_preserves_usage_exit() {
    use std::io::Read;
    use std::time::{Duration, Instant};

    for (key_name, key) in [("Esc", b'\x1b'), ("q", b'q')] {
        let lab = Lab::new(2, 200, true);
        let git_config = std::fs::read(lab.work.join(".git/config")).unwrap();
        let agit_config = std::fs::read(lab.home.join("config.json")).ok();
        let template = lab.command(env!("CARGO_BIN_EXE_agit"));
        let mut command = portable_pty::CommandBuilder::new(env!("CARGO_BIN_EXE_agit"));
        command.arg("clone");
        command.cwd(&lab.work);
        command.env_clear();
        for (name, value) in template.get_envs() {
            if let Some(value) = value {
                command.env(name, value);
            }
        }
        command.env("TERM", "xterm-256color");
        let pty = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize::default())
            .unwrap();
        let mut reader = pty.master.try_clone_reader().unwrap();
        let mut writer = pty.master.take_writer().unwrap();
        let mut child = pty.slave.spawn_command(command).unwrap();
        drop(pty.slave);
        let (send, chunks) = std::sync::mpsc::channel();
        let output_reader = std::thread::spawn(move || {
            let mut buffer = [0; 4096];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => return Ok(()),
                    Ok(size) => {
                        if send.send(buffer[..size].to_vec()).is_err() {
                            return Ok(());
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) if error.raw_os_error() == Some(libc::EIO) => return Ok(()),
                    Err(error) => return Err(error),
                }
            }
        });
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut output = Vec::new();
        let mut sent_key = false;
        let status = loop {
            output.extend(chunks.try_iter().take(64).flatten());
            if output.len() > 1024 * 1024 {
                break Err(format!("excessive PTY output while waiting for {key_name}"));
            }
            let rendered = String::from_utf8_lossy(&output);
            if !sent_key
                && [
                    "several agents worked here — copy which?",
                    "alice/paper",
                    "org/paper",
                ]
                .into_iter()
                .all(|text| rendered.contains(text))
            {
                if let Err(error) = writer.write_all(&[key]).and_then(|()| writer.flush()) {
                    break Err(format!("could not send {key_name}: {error}"));
                }
                sent_key = true;
            }
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) => {}
                Err(error) => break Err(format!("could not poll child: {error}")),
            }
            if Instant::now() >= deadline {
                break Err(format!("timed out waiting for {key_name} cancellation"));
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        if status.is_err() {
            // The PTY child owns its session, so cleanup closes inherited slave handles too.
            if let Some(pid) = child.process_id() {
                unsafe {
                    libc::killpg(pid as libc::pid_t, libc::SIGKILL);
                }
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        drop(writer);
        drop(pty.master);
        let reader_result = output_reader.join();
        output.extend(chunks.try_iter().flatten());
        let output = String::from_utf8_lossy(&output);
        assert!(reader_result.unwrap().is_ok(), "{key_name}: {output}");
        let status = status.unwrap_or_else(|error| panic!("{error}: {output}"));
        assert!(
            sent_key,
            "{key_name} was not sent after the choices: {output}"
        );
        assert_eq!(status.exit_code(), 2, "{key_name}: {output}");
        assert!(
            output.contains("selection cancelled; no agent was selected."),
            "{key_name}: {output}"
        );
        assert!(
            !output.contains("nothing interactive"),
            "{key_name}: {output}"
        );
        assert_eq!(
            std::fs::read(lab.work.join(".git/config")).unwrap(),
            git_config
        );
        assert_eq!(
            std::fs::read(lab.home.join("config.json")).ok(),
            agit_config
        );
        lab.assert_no_target();
    }
}

#[test]
#[cfg(unix)]
fn ambiguous_clone_json_has_no_selected_result() {
    let lab = Lab::new(2, 200, true);
    let out = lab.clone(&["--json"]);
    let body: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(8), "{body}");
    assert_eq!(body["exit_code"], 8);
    assert_eq!(body["ok"], false);
    assert_eq!(body["result"]["format"], "empty");
    assert!(out.stderr.is_empty());
    let diagnostics = body["diagnostics"]["stderr"].to_string();
    for expected in ["alice/paper", "org/paper"] {
        assert!(diagnostics.contains(expected), "{diagnostics}");
    }
    lab.assert_no_target();
}

#[test]
fn missing_lookup_preconditions_keep_their_exit_classification() {
    for (origin, count, status, code, message) in [
        (false, 2, 200, 2, "no origin"),
        (true, 0, 200, 2, "no agent has worked"),
        (true, 2, 500, 6, "reverse lookup failed"),
    ] {
        let lab = Lab::new(count, status, origin);
        let out = lab.clone(&[]);
        assert_eq!(out.status.code(), Some(code));
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(text.contains(message), "{text}");
        lab.assert_no_target();
    }
}

#[test]
fn unique_clone_candidate_still_opens_its_existing_checkout() {
    let lab = Lab::new(1, 200, true);
    let repo = lab.home.join("repos/alice/paper");
    std::fs::create_dir_all(&repo).unwrap();
    lab.git(&repo, &["init", "--quiet"]);
    lab.git(&repo, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    lab.git(&repo, &["config", "--local", "user.name", "Clone Fixture"]);
    lab.git(
        &repo,
        &["config", "--local", "user.email", "clone@example.invalid"],
    );
    lab.git(&repo, &["config", "--local", "commit.gpgsign", "false"]);
    meta::write(&repo, &meta::Meta::new_file_line()).unwrap();
    lab.git(&repo, &["add", "-A"]);
    lab.git(&repo, &["commit", "--quiet", "-m", "synthetic file line"]);
    let pin = json!({"hub":lab.hub.base,"agent_id":AGENT_ID}).to_string();
    lab.git(&repo, &["config", "--local", "agit.remoteIdentity", &pin]);
    lab.git(
        &repo,
        &[
            "remote",
            "add",
            "origin",
            &format!("{}/alice/paper.git", lab.hub.base),
        ],
    );
    let before = lab.git(&repo, &["rev-parse", "refs/heads/main"]);
    let out = lab.clone(&["--no-bind"]);
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(lab.git(&repo, &["rev-parse", "refs/heads/main"]), before);
    assert!(!lab.home.join("repos/org/paper").exists());
    assert!(!lab.home.join("workspaces").exists());
    assert!(
        lab.hub
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|path| path == "/api/agents/alice/paper")
    );
    println!("\nCLONE_FIXTURE_COMPLETED");
}

#[test]
#[cfg(unix)]
fn clone_fixture_ignores_polluted_global_hooks() {
    use std::os::unix::fs::PermissionsExt;

    let lab = Lab::new(0, 200, false);
    let hooks = lab._tmp.path().join("hooks");
    let marker = hooks.join("invoked");
    let global = lab._tmp.path().join("polluted.gitconfig");
    std::fs::create_dir(&hooks).unwrap();
    let hook = hooks.join("pre-commit");
    std::fs::write(
        &hook,
        "#!/bin/sh\nprintf 'rejecting hook ran\\n' > \"${0%/*}/invoked\"\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut config = lab.command("git");
    config
        .args(["config", "--file"])
        .arg(&global)
        .arg("core.hooksPath")
        .arg(&hooks);
    let out = lab.run_bounded(config);
    assert!(out.status.success(), "{out:?}");

    lab.git(
        &lab.work,
        &["config", "--local", "user.name", "Hook Canary"],
    );
    lab.git(
        &lab.work,
        &["config", "--local", "user.email", "canary@example.invalid"],
    );
    std::fs::write(lab.work.join("canary.txt"), "synthetic hook canary\n").unwrap();
    lab.git(&lab.work, &["add", "canary.txt"]);
    let mut canary = lab.command("git");
    canary
        .args(["commit", "--quiet", "-m", "synthetic hook canary"])
        .env("GIT_CONFIG_GLOBAL", &global);
    let out = lab.run_bounded(canary);
    assert!(!out.status.success(), "{out:?}");
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap(),
        "rejecting hook ran\n"
    );
    std::fs::remove_file(&marker).unwrap();

    let mut probe = lab.command(std::env::current_exe().unwrap().to_str().unwrap());
    probe
        .args([
            "--exact",
            "unique_clone_candidate_still_opens_its_existing_checkout",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("GIT_CONFIG_GLOBAL", &global);
    let out = lab.run_bounded(probe);
    assert!(out.status.success(), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .any(|line| line == "CLONE_FIXTURE_COMPLETED"),
        "the unique-candidate fixture did not complete: {out:?}"
    );
    assert!(!marker.exists(), "the isolated fixture ran the global hook");
}
