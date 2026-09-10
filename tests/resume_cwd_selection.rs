use agit::domain::{meta, repo::Repo, storage, transcript};
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
            .env_clear()
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
