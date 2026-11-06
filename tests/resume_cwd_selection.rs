#[path = "support/lab_env.rs"]
mod lab_env;

use agit::domain::{meta, repo::Repo, storage, transcript};
use lab_env::LabEnv;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const TARGET: &str = "local/qa@session";

struct Lab {
    root: tempfile::TempDir,
    home: PathBuf,
    store: PathBuf,
    work: PathBuf,
}

fn success(output: Output) -> String {
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

impl Lab {
    fn new(unknown: bool) -> Self {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().canonicalize().unwrap();
        let home = directory.join("home");
        let store = directory.join("agit");
        let work = directory.join("work");
        for path in [&home, &work, &directory.join("git-template")] {
            fs::create_dir_all(path).unwrap();
        }
        let lab = Self {
            root,
            home,
            store,
            work,
        };
        lab.git(&lab.work, &["init", "--quiet", "--initial-branch=main"]);
        lab.git(&lab.work, &["config", "commit.gpgsign", "false"]);
        lab.git(
            &lab.work,
            &["commit", "--allow-empty", "-m", "Seed code checkout"],
        );
        let code_head = lab.git(&lab.work, &["rev-parse", "HEAD"]);
        success(lab.run(&["init", "qa", "--no-bind"]));
        let repo = Repo::open(lab.store.join("repos/local/qa")).unwrap();
        lab.git(repo.root(), &["switch", "-c", "session"]);
        let mut snapshot = meta::Meta::new(
            format!("agit-{}", "b".repeat(40)),
            "claude-code".into(),
            lab.work.to_string_lossy().into_owned(),
        );
        snapshot.kind = meta::Kind::Turn;
        snapshot.turn = Some(1);
        snapshot.cwd_state = Some(meta::CwdState {
            origin: None,
            head: Some(if unknown { code_head } else { "a".repeat(40) }),
            branch: Some("main".into()),
            worktree: meta::WorktreeStatus::Unknown,
            staged: 0,
            unstaged: 0,
            untracked: 0,
            conflicted: 0,
            status_digest: None,
        });
        let native = serde_json::json!({
            "type":"user", "sessionId":"synthetic-cwd-session",
            "message":{"role":"user","content":"SYNTHETIC-CWD-CONTEXT"}
        });
        let envelope =
            transcript::wrap_lines(&native.to_string(), "claude-code", &snapshot.session);
        storage::write_snapshot(repo.root(), &envelope, &envelope).unwrap();
        meta::write(repo.root(), &snapshot).unwrap();
        lab.git(repo.root(), &["add", "."]);
        lab.git(repo.root(), &["commit", "-m", "Seed saved cwd observation"]);
        success(
            lab.command(env!("CARGO_BIN_EXE_agit"))
                .env("AGIT_SESSION", TARGET)
                .arg("tag")
                .output()
                .unwrap(),
        );
        // An existing Store isolates native refusal from first-use storage initialization.
        fs::create_dir_all(lab.store.join("store")).unwrap();
        lab
    }

    fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = Command::new(program);
        command
            .env_clear_local()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("CLAUDE_CONFIG_DIR", self.home.join(".claude"))
            .env("CODEX_HOME", self.home.join(".codex"))
            .env("AGIT_HOME", &self.store)
            .env("AGIT_HUB_URL", "http://127.0.0.1:1")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", self.home.join("absent-gitconfig"))
            .env(
                "GIT_TEMPLATE_DIR",
                self.work.parent().unwrap().join("git-template"),
            )
            .env("GIT_AUTHOR_NAME", "Synthetic cwd author")
            .env("GIT_AUTHOR_EMAIL", "cwd@example.invalid")
            .env("GIT_COMMITTER_NAME", "Synthetic cwd author")
            .env("GIT_COMMITTER_EMAIL", "cwd@example.invalid")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("CI", "1")
            .env("AGIT_TUI", "0")
            .env("NO_COLOR", "1")
            .current_dir(&self.work)
            .stdin(Stdio::null());
        #[cfg(windows)]
        for key in ["SystemRoot", "WINDIR", "TEMP", "TMP", "ComSpec", "PATHEXT"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        command
    }

    fn git(&self, path: &Path, args: &[&str]) -> String {
        success(
            self.command("git")
                .arg("-C")
                .arg(path)
                .args(args)
                .output()
                .unwrap(),
        )
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(env!("CARGO_BIN_EXE_agit"))
            .args(args)
            .output()
            .unwrap()
    }

    /// Runs agit at a terminal, presses Enter once `prompt` shows, and returns the screen and
    /// whether the command succeeded. A command still waiting a minute later fails the test.
    #[cfg(all(unix, feature = "rc"))]
    fn at_terminal(&self, args: &[&str], prompt: &str) -> (String, bool) {
        use std::io::{Read, Write};
        use std::sync::{Arc, Mutex};
        use std::time::{Duration, Instant};
        let mut command = portable_pty::CommandBuilder::new(env!("CARGO_BIN_EXE_agit"));
        command.env_clear();
        for (name, value) in [
            ("PATH", std::env::var_os("PATH").unwrap_or_default()),
            ("HOME", self.home.clone().into_os_string()),
            (
                "CLAUDE_CONFIG_DIR",
                self.home.join(".claude").into_os_string(),
            ),
            ("CODEX_HOME", self.home.join(".codex").into_os_string()),
            ("AGIT_HOME", self.store.clone().into_os_string()),
            ("AGIT_HUB_URL", "http://127.0.0.1:1".into()),
            ("GIT_CONFIG_NOSYSTEM", "1".into()),
            (
                "GIT_CONFIG_GLOBAL",
                self.home.join("absent-gitconfig").into_os_string(),
            ),
            ("GIT_TERMINAL_PROMPT", "0".into()),
            ("AGIT_TUI", "0".into()),
            // The setup offer an unset computer gets at a terminal is a question of its own.
            ("AGIT_SKIP_SETUP", "1".into()),
            ("NO_COLOR", "1".into()),
            ("TERM", "xterm-256color".into()),
        ] {
            command.env(name, value);
        }
        command.cwd(&self.work);
        command.args(args);
        let pty = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize::default())
            .unwrap();
        let mut reader = pty.master.try_clone_reader().unwrap();
        let mut writer = pty.master.take_writer().unwrap();
        let screen = Arc::new(Mutex::new(Vec::new()));
        let shared = Arc::clone(&screen);
        let output = std::thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            while let Ok(read) = reader.read(&mut chunk) {
                if read == 0 {
                    break;
                }
                shared.lock().unwrap().extend_from_slice(&chunk[..read]);
            }
        });
        let mut child = pty.slave.spawn_command(command).unwrap();
        drop(pty.slave);
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut answered = false;
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if !answered && String::from_utf8_lossy(&screen.lock().unwrap()).contains(prompt) {
                writer.write_all(b"\r").unwrap();
                writer.flush().unwrap();
                answered = true;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                panic!(
                    "still waiting a minute later: {}",
                    String::from_utf8_lossy(&screen.lock().unwrap())
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        drop(writer);
        drop(pty.master);
        output.join().unwrap();
        let text = String::from_utf8_lossy(&screen.lock().unwrap()).into_owned();
        (text, status.success())
    }

    fn files(&self) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
        walkdir::WalkDir::new(self.root.path())
            .into_iter()
            .map(|entry| {
                let entry = entry.unwrap();
                assert!(!entry.file_type().is_symlink());
                (
                    entry
                        .path()
                        .strip_prefix(self.root.path())
                        .unwrap()
                        .to_owned(),
                    entry
                        .file_type()
                        .is_file()
                        .then(|| fs::read(entry.path()).unwrap()),
                )
            })
            .collect()
    }
}

#[test]
fn noninteractive_cwd_decisions_refuse_before_native_materialization() {
    for unknown in [false, true] {
        let lab = Lab::new(unknown);
        let repo = lab.store.join("repos/local/qa");
        let refs = lab.git(&repo, &["show-ref"]);
        let before = lab.files();
        for flags in [
            vec![],
            vec!["--quiet"],
            vec!["--json", "--json-version", "1"],
            vec!["--json", "--json-version", "2"],
        ] {
            let mut args = flags.clone();
            args.extend(["resume", TARGET, "--no-launch"]);
            let output = lab.run(&args);
            assert_eq!(output.status.code(), Some(8), "{output:?}");
            let text = if flags.contains(&"--json") {
                assert!(output.stderr.is_empty(), "{output:?}");
                let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
                assert_eq!(value["exit_code"], 8);
                assert_eq!(value["ok"], false);
                assert_eq!(value["result"]["format"], "empty");
                value["diagnostics"]["stderr"].to_string()
            } else {
                let expected = if flags.contains(&"--quiet") {
                    String::new()
                } else {
                    format!("target: {TARGET} (via explicit arguments)\n")
                };
                assert_eq!(
                    String::from_utf8_lossy(&output.stdout),
                    expected,
                    "{output:?}"
                );
                String::from_utf8(output.stderr).unwrap()
            };
            for candidate in [
                "continue anyway",
                "inject an environment notice",
                "cancel",
                "--yes",
            ] {
                assert!(text.contains(candidate), "{text}");
            }
            assert!(!text.contains("SYNTHETIC-CWD-CONTEXT"));
            assert_eq!(lab.git(&repo, &["show-ref"]), refs);
            assert_eq!(
                lab.files(),
                before,
                "a missing cwd choice changed local files"
            );
            assert!(!lab.home.join(".claude").exists());
        }
        let output = lab.run(&["resume", TARGET, "--no-launch", "--yes"]);
        assert_eq!(output.status.code(), Some(0), "{output:?}");
        assert_eq!(lab.git(&repo, &["show-ref"]), refs);
        assert!(lab.home.join(".claude").is_dir());
    }
}

/// Resuming in a changed cwd as another runtime is one decision. The question about the cwd shows
/// what the conversion keeps and loses, and Enter on its default, "continue anyway", resumes. An
/// implementation that asks "proceed?" again afterwards cancels the resume the person just chose
/// when they press Enter at its default No, and here waits for an answer that never comes.
#[cfg(all(unix, feature = "rc"))]
#[test]
fn a_changed_cwd_and_a_lossy_conversion_are_one_decision() {
    let lab = Lab::new(false);
    let (screen, succeeded) = lab.at_terminal(
        &["resume", TARGET, "--no-launch", "--as", "codex"],
        "choose how to resume",
    );
    assert!(succeeded, "{screen}");
    let choice = screen.find("choose how to resume").unwrap();
    let lost = screen.find("lost: encrypted reasoning").unwrap();
    assert!(lost < choice, "{screen}");
    assert!(!screen.contains("proceed?"), "{screen}");
    assert!(!screen.contains("cancelled"), "{screen}");
}
