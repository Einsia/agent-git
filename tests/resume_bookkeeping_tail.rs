#![cfg(unix)]

//! `agit commit`, a runtime switch and a merge into the branch agree on what is unsettled.
//! Records a runtime writes after the last settled turn without starting a new one leave commit
//! nothing to settle, so the switch and the merge must proceed; a tail carrying a turn refuses
//! the switch until commit settles it. If they disagree, the refusal's own advice
//! (`agit commit`) cannot clear it.

use serde_json::json;
use std::{
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    process::{Command, Output},
};

const SID: &str = "aaaaaaaa-0000-4000-8000-000000000001";

/// A turn's stop and exit, in the order Claude Code writes them.
const EXIT: &[&str] = &[
    "stop_hook_summary",
    "turn_duration",
    "cost-state",
    "last-prompt",
    "cost-state",
    "file-history-snapshot",
];

/// A settled session left, reopened without a prompt and left again, record types in the order
/// a real Claude Code transcript has them.
const EXIT_AND_REOPEN: &[&str] = &[
    "cost-state",
    "last-prompt",
    "cost-state",
    "file-history-snapshot",
    "custom-title",
    "agent-name",
    "mode",
    "permission-mode",
    "atis-latch",
    "custom-title",
    "agent-name",
    "cost-state",
    "cost-state",
    "file-history-snapshot",
];

/// A turn up to its first reply record, record types in the order a real Claude Code transcript
/// has them.
const LAST_TURN: &[&str] = &[
    "api-request-blob",
    "api-request-blob",
    "api-request",
    "user",
    "last-prompt",
    "custom-title",
    "agent-name",
    "mode",
    "permission-mode",
    "atis-latch",
    "attachment",
    "assistant",
];

#[test]
fn commit_a_runtime_switch_and_a_merge_agree_on_what_is_unsettled() {
    for tail in [EXIT, EXIT_AND_REOPEN] {
        let lab = Lab::new();
        lab.append(&lab.tail(tail));
        let commit = lab.output(&["commit", SID]);
        let text = combined(&commit);
        assert!(commit.status.success(), "{tail:?}: {text}");
        assert!(text.contains("nothing new since"), "{tail:?}: {text}");
        let switched = lab.output(&["resume", "alice/qa@work", "--as", "codex", "--no-launch"]);
        let text = combined(&switched);
        assert!(switched.status.success(), "{tail:?}: {text}");
        assert!(
            text.contains("no new turn since the last settlement"),
            "{tail:?}: {text}"
        );
        assert!(
            !text.contains("already has unsettled content"),
            "{tail:?}: {text}"
        );
        assert_eq!(
            walk(&lab.home.join(".codex/sessions")).len(),
            1,
            "{tail:?}: {text}"
        );
    }

    for tail in [EXIT, EXIT_AND_REOPEN] {
        let lab = Lab::new();
        lab.append(&lab.tail(tail));
        let commit = lab.output(&["commit", SID]);
        let text = combined(&commit);
        assert!(text.contains("nothing new since"), "{tail:?}: {text}");
        let merged = lab.merge();
        let text = combined(&merged);
        assert!(merged.status.success(), "{tail:?}: {text}");
        assert!(
            !text.contains("unsettled or unverifiable"),
            "{tail:?}: {text}"
        );
        assert!(
            lab.repo().join(".git/AGIT_MERGE_TX").exists(),
            "{tail:?}: {text}"
        );
    }

    // Settlement can land a turn between its reply records. Commit offers no reply record written
    // after that until the next prompt and reports nothing new, so merge must not ask for a commit
    // over it either, whether the claim is the native transcript or a materialized one.
    for materialized in [false, true] {
        let lab = Lab::new();
        let (live, id) = if materialized {
            lab.run(&[
                "resume",
                "alice/qa@work",
                "--as",
                "claude-code",
                "--no-launch",
            ]);
            let live = walk(&lab.home.join(".claude/projects"))
                .into_iter()
                .find(|path| path != &lab.live)
                .expect("materialized transcript");
            let id = live.file_stem().unwrap().to_str().unwrap().to_owned();
            (live, id)
        } else {
            (lab.live.clone(), SID.to_owned())
        };
        append(&live, &lab.tail(LAST_TURN));
        let commit = lab.output(&["commit", &id]);
        let text = combined(&commit);
        assert!(commit.status.success(), "{materialized}: {text}");
        assert!(
            !text.contains("nothing new since"),
            "{materialized}: {text}"
        );
        append(&live, &(lab.tail(&["assistant"]) + &lab.tail(EXIT)));
        let commit = lab.output(&["commit", &id]);
        let text = combined(&commit);
        assert!(text.contains("nothing new since"), "{materialized}: {text}");
        let merged = lab.merge();
        let text = combined(&merged);
        assert!(merged.status.success(), "{materialized}: {text}");
        assert!(
            lab.repo().join(".git/AGIT_MERGE_TX").exists(),
            "{materialized}: {text}"
        );
    }

    let lab = Lab::new();
    lab.append(&(lab.turn(3) + &lab.tail(EXIT)));
    let refused = lab.output(&["resume", "alice/qa@work", "--as", "codex", "--no-launch"]);
    let text = combined(&refused);
    assert!(!refused.status.success(), "{text}");
    assert!(text.contains("already has unsettled content"), "{text}");
    assert!(text.contains(&format!("agit commit {SID}")), "{text}");
    assert!(walk(&lab.home.join(".codex/sessions")).is_empty(), "{text}");
    let commit = lab.output(&["commit", SID]);
    let text = combined(&commit);
    assert!(commit.status.success(), "{text}");
    assert!(!text.contains("nothing new since"), "{text}");
    let switched = lab.output(&["resume", "alice/qa@work", "--as", "codex", "--no-launch"]);
    let text = combined(&switched);
    assert!(switched.status.success(), "{text}");
    assert_eq!(walk(&lab.home.join(".codex/sessions")).len(), 1, "{text}");
}

fn combined(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn append(path: &Path, text: &str) {
    fs::OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(text.as_bytes())
        .unwrap();
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

    fn repo(&self) -> PathBuf {
        self.store.join("repos/alice/qa")
    }

    /// A manual merge into the session's branch from a branch at the same head.
    fn merge(&self) -> Output {
        let repo = self.repo();
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .unwrap();
            assert!(out.status.success(), "{args:?}: {}", combined(&out));
            String::from_utf8(out.stdout).unwrap().trim().to_owned()
        };
        let head = git(&["rev-parse", "refs/heads/work"]);
        git(&["branch", "source", &head]);
        self.output(&[
            "merge",
            "alice/qa@source",
            "--into",
            "alice/qa@work",
            "--manual",
        ])
    }

    fn run(&self, args: &[&str]) -> String {
        let out = self.output(args);
        assert!(out.status.success(), "{args:?}: {}", combined(&out));
        String::from_utf8(out.stdout).unwrap()
    }

    fn append(&self, text: &str) {
        append(&self.live, text);
    }

    /// Records Claude Code writes in the given order, by type (`system` records by subtype):
    /// fields as the runtime writes them, synthetic values. Request snapshots and attachments
    /// carry only their identity, since no settlement rule reads their payload.
    fn tail(&self, kinds: &[&str]) -> String {
        let system = |subtype: &str, extra: serde_json::Value| {
            let mut record = json!({"type":"system", "subtype":subtype, "sessionId":SID,
                "cwd":self.work, "uuid":"s", "parentUuid":"a", "timestamp":"2026-10-25T22:39:00.000Z",
                "version":"2.1.310", "isSidechain":false, "userType":"external",
                "entrypoint":"cli", "gitBranch":"main"});
            record
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            record
        };
        kinds
            .iter()
            .enumerate()
            .map(|(i, kind)| match *kind {
                "user" => json!({"type":kind, "sessionId":SID, "cwd":self.work, "uuid":"u-tail",
                    "message":{"role":"user", "content":"synthetic prompt"}}),
                "assistant" => json!({"type":kind, "sessionId":SID, "cwd":self.work,
                    "uuid":format!("a-tail-{i}"), "message":{"role":"assistant",
                    "content":[{"type":"text", "text":format!("synthetic answer {i}")}]}}),
                "api-request-blob" | "api-request" | "attachment" => {
                    json!({"type":kind, "sessionId":SID, "uuid":format!("r-tail-{i}")})
                }
                "stop_hook_summary" => system(kind, json!({"hookCount":1, "hookInfos":[],
                    "hookErrors":[], "hookAdditionalContext":[], "preventedContinuation":false,
                    "stopReason":"", "hasOutput":false, "level":"suggestion", "toolUseID":"t"})),
                "turn_duration" => system(kind, json!({"durationMs":1, "messageCount":2, "isMeta":false})),
                "cost-state" => json!({"type":kind, "sessionId":SID, "totalCostUSD":0.0,
                    "totalAPIDuration":0, "totalAPIDurationWithoutRetries":0, "totalToolDuration":0,
                    "totalDuration":0, "totalLinesAdded":0, "totalLinesRemoved":0, "startTime":0,
                    "hasUnknownModelCost":false, "modelUsage":{}}),
                "last-prompt" => json!({"type":kind, "sessionId":SID, "lastPrompt":"synthetic prompt", "leafUuid":"a"}),
                "file-history-snapshot" => json!({"type":kind, "messageId":"m-tail",
                    "snapshot":{"messageId":"m-tail", "trackedFileBackups":{}, "timestamp":"2026-10-25T22:39:00.000Z"},
                    "isSnapshotUpdate":true}),
                "custom-title" => json!({"type":kind, "sessionId":SID, "customTitle":"synthetic title"}),
                "agent-name" => json!({"type":kind, "sessionId":SID, "agentName":"synthetic name"}),
                "mode" => json!({"type":kind, "sessionId":SID, "mode":"normal"}),
                "permission-mode" => json!({"type":kind, "sessionId":SID, "permissionMode":"default"}),
                "atis-latch" => json!({"type":kind, "sessionId":SID, "atis":"synthetic"}),
                other => panic!("no fixture for {other}"),
            })
            .map(|record| format!("{record}\n"))
            .collect()
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
