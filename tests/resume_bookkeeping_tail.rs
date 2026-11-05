#![cfg(unix)]

//! `agit commit`, a runtime switch and a merge into the branch agree on what is unsettled.
//! Records a runtime writes after the last settled turn without starting a new one leave commit
//! nothing to settle, so the switch and the merge must proceed; a tail carrying a turn refuses
//! the switch until commit settles it. If they disagree, the refusal's own advice
//! (`agit commit`) cannot clear it.

#[path = "support/lab_env.rs"]
mod lab_env;

use lab_env::LabEnv;
use serde_json::json;
use std::{
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    process::{Command, Output},
};

const SID: &str = "aaaaaaaa-0000-4000-8000-000000000001";
const CODEX_SID: &str = "aaaaaaaa-0000-4000-8000-0000000c0de5";

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

    // A writer that records stop reasons settles a reply written after a settled turn, woken by a
    // task notification with no new prompt, as a further commit of that turn. Until then that
    // reply is unsettled work, not bookkeeping, whether the claim is the native transcript or a
    // materialized one: the switch refuses until it is committed, and the merge settles it onto
    // the branch before it opens. Read as bookkeeping, both would leave the reply behind.
    for materialized in [false, true] {
        for merging in [false, true] {
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
            append(&live, &(lab.tail(&["user", "reply"]) + &lab.tail(EXIT)));
            let commit = lab.output(&["commit", &id]);
            let text = combined(&commit);
            assert!(commit.status.success(), "{materialized}: {text}");
            assert!(
                !text.contains("nothing new since"),
                "{materialized}: {text}"
            );
            append(
                &live,
                &(lab.tail(&["notification", "reply"]) + &lab.tail(EXIT)),
            );
            if merging {
                let merged = lab.merge();
                let text = combined(&merged);
                assert!(merged.status.success(), "{materialized}: {text}");
                let landed = Command::new("git")
                    .arg("-C")
                    .arg(lab.repo())
                    .args(["rev-list", "--count", "refs/heads/source..refs/heads/work"])
                    .env("GIT_CONFIG_GLOBAL", "/dev/null")
                    .env("GIT_CONFIG_NOSYSTEM", "1")
                    .output()
                    .unwrap();
                assert_eq!(
                    String::from_utf8_lossy(&landed.stdout).trim(),
                    "1",
                    "{materialized}: {text}"
                );
                let log = lab.run(&["log", "alice/qa@work"]);
                assert!(
                    log.lines().any(|line| line.trim_start().starts_with('+')),
                    "{materialized}: {log}"
                );
                continue;
            }
            let switched = lab.output(&["resume", "alice/qa@work", "--as", "codex", "--no-launch"]);
            let text = combined(&switched);
            assert!(!switched.status.success(), "{materialized}: {text}");
            assert!(
                text.contains("already has unsettled content"),
                "{materialized}: {text}"
            );
            let commit = lab.output(&["commit", &id]);
            let text = combined(&commit);
            assert!(commit.status.success(), "{materialized}: {text}");
            assert!(
                !text.contains("nothing new since"),
                "{materialized}: {text}"
            );
            let switched = lab.output(&["resume", "alice/qa@work", "--as", "codex", "--no-launch"]);
            let text = combined(&switched);
            assert!(switched.status.success(), "{materialized}: {text}");
        }
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

/// Claude Code 2.1.311 through 2.1.314 append these records at a turn's end and at /exit. In
/// 2.1.311 (the version the first capture's records name): an attachment with the request
/// snapshot, two copies of the last prompt, mode, permission mode, the latch, the stop-hook
/// summary, the turn duration, two cost states and a file-history snapshot. In 2.1.314: the
/// stop-hook summary, the turn duration, a copy of the last prompt and two cost states. Commit finds nothing new in them, so a merge and a runtime
/// switch proceed. A turn after them still refuses the switch, a copy of a prompt never settled
/// refuses it, and a record of a type this build does not know refuses both by its type alone,
/// with the command that fixes it.
#[test]
fn claude_code_2_1_311_to_2_1_314_exit_records_leave_a_merge_free() {
    for (version, capture, records) in [
        (
            "2.1.311",
            include_str!("fixtures/cc-2.1.311-trailing-records.jsonl"),
            11,
        ),
        (
            "2.1.314",
            include_str!("fixtures/cc-2.1.314-trailing-records.jsonl"),
            5,
        ),
    ] {
        exit_records_leave_a_merge_free(version, capture, records);
    }
}

fn exit_records_leave_a_merge_free(version: &str, capture: &str, records: usize) {
    let exit = |lab: &Lab, last_prompt: &str| -> String {
        capture
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                let mut record: serde_json::Value = serde_json::from_str(line).unwrap();
                let fields = record.as_object_mut().unwrap();
                for key in ["sessionId", "session_id"] {
                    if fields.contains_key(key) {
                        fields.insert(key.into(), json!(SID));
                    }
                }
                if fields.contains_key("cwd") {
                    fields.insert("cwd".into(), json!(lab.work));
                }
                if fields.contains_key("lastPrompt") {
                    fields.insert("lastPrompt".into(), json!(last_prompt));
                }
                format!("{record}\n")
            })
            .collect()
    };
    assert_eq!(
        exit(&Lab::new(), "").lines().count(),
        records,
        "{version}: the capture's trailing records"
    );

    let lab = Lab::new();
    lab.append(&exit(&lab, "synthetic prompt 2"));
    let commit = lab.output(&["commit", SID]);
    let text = combined(&commit);
    assert!(text.contains("nothing new since"), "{version}: {text}");
    let merged = lab.merge();
    let text = combined(&merged);
    assert!(merged.status.success(), "{version}: {text}");
    assert!(
        lab.repo().join(".git/AGIT_MERGE_TX").exists(),
        "{version}: {text}"
    );

    let lab = Lab::new();
    lab.append(&exit(&lab, "synthetic prompt 2"));
    let switched = lab.output(&["resume", "alice/qa@work", "--as", "codex", "--no-launch"]);
    let text = combined(&switched);
    assert!(switched.status.success(), "{version}: {text}");
    assert!(
        text.contains("no new turn since the last settlement"),
        "{version}: {text}"
    );

    // A turn after the exit records is unsettled work.
    let lab = Lab::new();
    lab.append(&(exit(&lab, "synthetic prompt 2") + &lab.turn(3)));
    let refused = lab.output(&["resume", "alice/qa@work", "--as", "codex", "--no-launch"]);
    let text = combined(&refused);
    assert!(!refused.status.success(), "{version}: {text}");
    assert!(
        text.contains("already has unsettled content"),
        "{version}: {text}"
    );

    // A copy of a prompt no settled turn holds is not left behind.
    let lab = Lab::new();
    lab.append(&exit(&lab, "a prompt that was never settled"));
    let refused = lab.output(&["resume", "alice/qa@work", "--as", "codex", "--no-launch"]);
    let text = combined(&refused);
    assert!(!refused.status.success(), "{version}: {text}");
    assert!(
        text.contains("already has unsettled content"),
        "{version}: {text}"
    );

    // A type this build does not know is refused by its name, cut short, and nothing else of it
    // is shown.
    const CANARY: &str = "CANARY-UNKNOWN-RECORD-BODY-c41f";
    let unknown = format!(
        "{}\n",
        json!({"type":"a-record-from-a-later-claude-code-version-x", "sessionId":SID, "body":CANARY})
    );
    let lab = Lab::new();
    lab.append(&(exit(&lab, "synthetic prompt 2") + &unknown));
    let merged = lab.merge();
    let text = combined(&merged);
    assert!(!merged.status.success(), "{version}: {text}");
    for expected in [
        "record of type `a-record-from-a-later-claude-code-versio`",
        upgrade_step(),
    ] {
        assert!(text.contains(expected), "{version}: {expected:?} in {text}");
    }
    assert!(
        !text.contains("unsettled or unverifiable"),
        "{version}: {text}"
    );
    assert!(!text.contains(CANARY), "{version}: {text}");
    let switched = lab.output(&["resume", "alice/qa@work", "--as", "codex", "--no-launch"]);
    let text = combined(&switched);
    assert!(!switched.status.success(), "{version}: {text}");
    assert!(
        text.contains("record of type `a-record-from-a-later-claude-code-versio`"),
        "{version}: {text}"
    );
    assert!(!text.contains(CANARY), "{version}: {text}");
}

/// The start of a Codex turn after the last settled one, as Codex 0.166 writes it before the
/// prompt (applied settings, the turn's start, its context), leaves `agit commit` nothing to
/// settle, so a merge into the branch proceeds. An event of a payload type this build has not
/// seen is refused by that name, says nothing changed, and names an upgrade step this install
/// can take, showing nothing else of the record. A classification that took the events
/// settlement reads for unknown ones would refuse the first merge as `event_msg`; one that let
/// an unseen event pass would leave a turn this build cannot read behind.
#[test]
fn a_started_codex_turn_leaves_a_merge_free_and_a_new_event_is_named() {
    let lab = Lab::codex();
    lab.append(&lab.codex_turn_start(3));
    let commit = lab.output(&["commit", "alice/qa@work"]);
    let text = combined(&commit);
    assert!(text.contains("nothing new since"), "{text}");
    let merged = lab.merge();
    let text = combined(&merged);
    assert!(merged.status.success(), "{text}");
    assert!(lab.repo().join(".git/AGIT_MERGE_TX").exists(), "{text}");

    const CANARY: &str = "CANARY-LATER-CODEX-EVENT-BODY-7d20";
    let lab = Lab::codex();
    let unknown = json!({"timestamp":"2026-11-05T06:34:16.000Z", "type":"event_msg",
        "payload":{"type":"a_later_codex_event", "body":CANARY}});
    lab.append(&format!("{}{unknown}\n", lab.codex_turn_start(3)));
    let merged = lab.merge();
    let text = combined(&merged);
    assert!(!merged.status.success(), "{text}");
    for expected in [
        "record of type `event_msg/a_later_codex_event`",
        "nothing was changed and its turns stay saved on this computer",
        upgrade_step(),
    ] {
        assert!(text.contains(expected), "{expected:?} in {text}");
    }
    assert!(!text.contains(CANARY), "{text}");
}

/// What a refusal a newer agit resolves asks first, in this build's channel: only a production
/// build upgrades itself.
fn upgrade_step() -> &'static str {
    if agit::infra::config::is_production_release() {
        "run `agit upgrade`, then retry"
    } else {
        "install a newer agit from the GitLab pipeline that built this one"
    }
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

    /// A Codex session of two settled turns, in the record shape Codex 0.166 writes.
    fn codex() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let store = dir.path().join("agit");
        let work = dir.path().join("work");
        let sessions = home.join(".codex/sessions/2026/11/05");
        fs::create_dir_all(&sessions).unwrap();
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
            live: sessions.join(format!("rollout-2026-11-05T00-00-00-{CODEX_SID}.jsonl")),
            _dir: dir,
            home,
            store,
            work,
        };
        lab.run(&["config", "secrets.keystore", "file"]);
        lab.run(&["init", "qa"]);
        let header = json!({"timestamp":"2026-11-05T06:34:16.000Z", "type":"session_meta",
            "payload":{"id":CODEX_SID, "cwd":lab.work, "timestamp":"2026-11-05T06:34:16.000Z",
                "cli_version":"0.166.0"}});
        fs::write(
            &lab.live,
            format!("{header}\n{}{}", lab.codex_turn(1), lab.codex_turn(2)),
        )
        .unwrap();
        lab.run(&[
            "import",
            CODEX_SID,
            "--from",
            "codex",
            "--into",
            "alice/qa@work",
            "--independent",
        ]);
        lab
    }

    /// What Codex writes as a turn starts, before its prompt: applied settings, the turn's
    /// start and its context.
    fn codex_turn_start(&self, n: usize) -> String {
        let at = "2026-11-05T06:34:16.000Z";
        [
            json!({"timestamp":at, "type":"event_msg", "payload":{"type":"thread_settings_applied",
                "thread_id":CODEX_SID, "thread_settings":{}}}),
            json!({"timestamp":at, "type":"event_msg", "payload":{"type":"task_started",
                "turn_id":format!("t{n}"), "root_turn_id":format!("t{n}"), "started_at":0,
                "model_context_window":1, "collaboration_mode_kind":"default", "turn_attribution":{}}}),
            json!({"timestamp":at, "type":"turn_context", "payload":{"cwd":self.work, "model":"m"}}),
        ]
        .iter()
        .map(|record| format!("{record}\n"))
        .collect()
    }

    /// A whole Codex turn, record kinds in the order Codex 0.166 writes them, synthetic values.
    fn codex_turn(&self, n: usize) -> String {
        let at = "2026-11-05T06:34:16.000Z";
        let rest = [
            json!({"timestamp":at, "type":"response_item", "payload":{"type":"message", "role":"user",
                "content":[{"type":"input_text", "text":format!("synthetic prompt {n}")}]}}),
            json!({"timestamp":at, "type":"event_msg", "payload":{"type":"item_completed",
                "thread_id":CODEX_SID, "turn_id":format!("t{n}"), "item":{"type":"user_message"},
                "started_at_ms":0, "completed_at_ms":0}}),
            json!({"timestamp":at, "type":"event_msg", "payload":{"type":"item_completed",
                "thread_id":CODEX_SID, "turn_id":format!("t{n}"), "item":{"type":"agent_message"},
                "started_at_ms":0, "completed_at_ms":0}}),
            json!({"timestamp":at, "type":"response_item", "payload":{"type":"message", "role":"assistant",
                "content":[{"type":"output_text", "text":format!("synthetic answer {n}")}]}}),
            json!({"timestamp":at, "type":"token_usage_record", "payload":{}}),
            json!({"timestamp":at, "type":"event_msg", "payload":{"type":"token_count",
                "info":null, "rate_limits":null}}),
            json!({"timestamp":at, "type":"event_msg", "payload":{"type":"task_complete",
                "turn_id":format!("t{n}"), "root_turn_id":format!("t{n}"),
                "last_agent_message":format!("synthetic answer {n}"), "started_at":0,
                "completed_at":0, "duration_ms":1, "time_to_first_token_ms":1}}),
        ];
        let rest: String = rest.iter().map(|record| format!("{record}\n")).collect();
        format!("{}{rest}", self.codex_turn_start(n))
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agit"));
        cmd.env_clear_local()
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
                // A reply from a writer that records why each message stopped.
                "reply" => json!({"type":"assistant", "sessionId":SID, "cwd":self.work,
                    "uuid":format!("a-reply-{i}"), "message":{"role":"assistant", "stop_reason":"end_turn",
                    "content":[{"type":"text", "text":format!("synthetic reply {i}")}]}}),
                // A background task's completion waking the agent without a prompt.
                "notification" => json!({"type":"user", "sessionId":SID, "cwd":self.work,
                    "uuid":format!("n-tail-{i}"), "promptSource":"system", "message":{"role":"user",
                    "content":"<task-notification>\n<task-id>b1</task-id>\n<status>completed</status>\n</task-notification>"}}),
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
                // A copy of the last settled prompt, as Claude Code writes it.
                "last-prompt" => json!({"type":kind, "sessionId":SID, "lastPrompt":"synthetic prompt 2", "leafUuid":"a"}),
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
