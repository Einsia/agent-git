#![cfg(unix)]

//! Resume judges a native claim by hydrated content. A repository record registered after a
//! branch was settled can match text inside that settled prefix; re-projecting the live
//! transcript with the current dictionary would then differ from the committed envelopes even
//! though the runtime never rewrote a byte, and the branch would refuse every resume.

use serde_json::json;
use std::{
    fs,
    io::Write as _,
    path::PathBuf,
    process::{Command, Output, Stdio},
};

const SID: &str = "aaaaaaaa-0000-4000-8000-000000000001";
const LATER: &str = "bbbbbbbb-0000-4000-8000-000000000002";

#[test]
fn resume_keeps_a_settled_prefix_resumable_after_later_repository_records() {
    let lab = Lab::new();
    // The value of this record already sits inside the settled prefix of `work`.
    lab.register_secret("later-rule", "synthetic prompt");
    let later = lab.live.parent().unwrap().join(format!("{LATER}.jsonl"));
    fs::write(&later, lab.turn(1).replace(SID, LATER)).unwrap();
    lab.run(&[
        "import",
        LATER,
        "--into",
        "alice/qa@after-mapping",
        "--independent",
    ]);
    let dictionary = lab.repo().join(".git/agit/secret-dictionary/vault.json");
    let dictionary_before = fs::read(&dictionary).unwrap();
    let live_before = fs::read(&lab.live).unwrap();

    let untouched = lab.run(&["resume", "alice/qa@work", "--no-launch"]);
    assert!(
        untouched.contains("reusing the local native session (zero-copy)"),
        "{untouched}"
    );

    // Content appended after settlement is unsettled work, not a rewrite: a runtime switch
    // must ask for settlement instead of refusing the branch outright.
    fs::OpenOptions::new()
        .append(true)
        .open(&lab.live)
        .unwrap()
        .write_all(lab.turn(3).as_bytes())
        .unwrap();
    let switched = lab.output(&["resume", "alice/qa@work", "--as", "codex", "--no-launch"]);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&switched.stdout),
        String::from_utf8_lossy(&switched.stderr)
    );
    assert!(!switched.status.success(), "{text}");
    assert!(text.contains("already has unsettled content"), "{text}");
    assert!(
        !text.contains("rewritten inside its recorded baseline"),
        "{text}"
    );

    let appended = lab.run(&["resume", "alice/qa@work", "--no-launch"]);
    assert!(
        appended.contains("reusing the local native session (zero-copy)"),
        "{appended}"
    );

    // A comparison reads the dictionary and the transcript; it persists neither.
    assert_eq!(fs::read(&dictionary).unwrap(), dictionary_before);
    let mut expected_live = live_before;
    expected_live.extend_from_slice(lab.turn(3).as_bytes());
    assert_eq!(fs::read(&lab.live).unwrap(), expected_live);
}

struct Lab {
    _dir: tempfile::TempDir,
    home: PathBuf,
    store: PathBuf,
    work: PathBuf,
    live: PathBuf,
}

impl Lab {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let store = dir.path().join("agit");
        let work = dir.path().join("work");
        let project = home.join(".claude/projects/fixture");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&work).unwrap();
        fs::create_dir_all(store.join("credentials")).unwrap();
        fs::write(
            store.join("credentials/127.0.0.1_1.json"),
            json!({
                "username": "alice", "email": null,
                "hub": "http://127.0.0.1:1", "access_token": "synthetic",
                "access_expires_at": "2099-01-01T00:00:00Z",
                "refresh_token": "synthetic", "refresh_expires_at": "2099-01-01T00:00:00Z"
            })
            .to_string(),
        )
        .unwrap();
        let lab = Self {
            live: project.join(format!("{SID}.jsonl")),
            _dir: dir,
            home,
            store,
            work,
        };
        lab.run(&["config", "secrets.keystore", "file"]);
        lab.run(&["init", "qa"]);
        fs::write(&lab.live, format!("{}{}", lab.turn(1), lab.turn(2))).unwrap();
        lab.run(&["import", SID, "--into", "alice/qa@work", "--independent"]);
        lab
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agit"));
        cmd.env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("HOME", &self.home)
            .env("AGIT_HOME", &self.store)
            .env("AGIT_HUB_URL", "http://127.0.0.1:1")
            .env("CI", "1")
            .env("AGIT_YES", "1")
            .env("NO_COLOR", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .current_dir(&self.work);
        cmd
    }

    fn output(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }

    fn run(&self, args: &[&str]) -> String {
        let out = self.output(args);
        assert!(
            out.status.success(),
            "{args:?}: {}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn register_secret(&self, name: &str, value: &str) {
        let mut child = self
            .command()
            .args(["secrets", "add", name, "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(value.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn repo(&self) -> PathBuf {
        self.store.join("repos/alice/qa")
    }

    fn turn(&self, n: usize) -> String {
        [
            json!({"type":"user", "sessionId":SID, "cwd":self.work,
                "uuid":format!("u{n}"), "message":{"role":"user", "content":format!("synthetic prompt {n}")}}),
            json!({"type":"assistant", "sessionId":SID, "cwd":self.work,
                "uuid":format!("a{n}"), "message":{"role":"assistant", "content":[{"type":"text", "text":format!("synthetic answer {n}")}]}}),
        ]
        .iter()
        .map(|v| format!("{v}\n"))
        .collect()
    }
}
