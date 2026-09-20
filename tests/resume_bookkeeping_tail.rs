#![cfg(unix)]

//! Runtime bookkeeping written after the last settled turn does not count as unsettled work.
//! A runtime switch materializes the settled branch and leaves such records behind; a tail that
//! carries a turn still refuses, because materializing would drop that turn.

use serde_json::json;
use std::{
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    process::{Command, Output},
};

const SID: &str = "aaaaaaaa-0000-4000-8000-000000000001";

#[test]
fn a_bookkeeping_tail_does_not_block_a_runtime_switch_but_a_turn_does() {
    let lab = Lab::new();
    lab.append(
        &(json!({"type":"file-history-snapshot", "messageId":"m-tail", "sessionId":SID,
            "snapshot":{"trackedFileBackups":{}}, "isSnapshotUpdate":false})
        .to_string()
            + "\n"),
    );
    let switched = lab.output(&["resume", "alice/qa@work", "--as", "codex", "--no-launch"]);
    let text = combined(&switched);
    assert!(switched.status.success(), "{text}");
    assert!(
        text.contains("no new turn since the last settlement"),
        "{text}"
    );
    assert!(!text.contains("already has unsettled content"), "{text}");
    assert_eq!(walk(&lab.home.join(".codex/sessions")).len(), 1, "{text}");

    let lab = Lab::new();
    lab.append(&lab.turn(3));
    let refused = lab.output(&["resume", "alice/qa@work", "--as", "codex", "--no-launch"]);
    let text = combined(&refused);
    assert!(!refused.status.success(), "{text}");
    assert!(text.contains("already has unsettled content"), "{text}");
    assert!(walk(&lab.home.join(".codex/sessions")).is_empty(), "{text}");
}

fn combined(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(walk(&path));
        } else if path.extension().is_some_and(|ext| ext == "jsonl") {
            out.push(path);
        }
    }
    out
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
        assert!(out.status.success(), "{args:?}: {}", combined(&out));
        String::from_utf8(out.stdout).unwrap()
    }

    fn append(&self, text: &str) {
        fs::OpenOptions::new()
            .append(true)
            .open(&self.live)
            .unwrap()
            .write_all(text.as_bytes())
            .unwrap();
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
