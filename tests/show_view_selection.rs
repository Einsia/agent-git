#![cfg(unix)]

use serde_json::json;
use std::{fs, path::PathBuf, process::Command};

const SID: &str = "aaaaaaaa-0000-4000-8000-000000000001";

#[test]
fn raw_missing_native_selection_refuses_without_prose_as_evidence() {
    let lab = Lab::new();
    let empty_store = lab.home.join("empty-agit");
    for json in [false, true] {
        let mut command = lab.command();
        command
            .env("AGIT_HOME", &empty_store)
            .env_remove("AGIT_SESSION");
        if json {
            command.arg("--json");
        }
        let output = command.args(["show", SID, "--raw"]).output().unwrap();
        assert_eq!(output.status.code(), Some(3), "{output:?}");
        if json {
            let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(value["ok"], false);
            assert_eq!(value["exit_code"], 3);
            assert!(output.stderr.is_empty());
        } else {
            assert!(output.stdout.is_empty(), "{output:?}");
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("no adopted local link"),
                "{output:?}"
            );
        }
    }
}

struct Lab {
    _root: tempfile::TempDir,
    home: PathBuf,
    store: PathBuf,
    work: PathBuf,
}

impl Lab {
    fn new() -> Self {
        Self::with_records(Vec::new())
    }

    fn with_records(extra: Vec<serde_json::Value>) -> Self {
        let root = tempfile::tempdir().unwrap();
        let lab = Self {
            home: root.path().join("home"),
            store: root.path().join("agit"),
            work: root.path().join("work"),
            _root: root,
        };
        fs::create_dir_all(&lab.work).unwrap();
        let project = lab.home.join(".claude/projects/synthetic");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(lab.store.join("credentials")).unwrap();
        fs::write(
            lab.store.join("credentials/127.0.0.1_1.json"),
            json!({"username":"me", "hub":"http://127.0.0.1:1",
                "access_token":"synthetic", "refresh_token":"synthetic",
                "access_expires_at":"2099-01-01T00:00:00Z",
                "refresh_expires_at":"2099-01-01T00:00:00Z"})
            .to_string(),
        )
        .unwrap();
        let mut records = Vec::new();
        for (index, text) in ["VISIBLE-FIRST", "HIDDEN-TURN", "VISIBLE-LAST"]
            .iter()
            .enumerate()
        {
            records.push(json!({"type":"user", "sessionId":SID, "cwd":lab.work,
                "uuid":format!("user-{index}"), "message":{"role":"user", "content":text}}));
            records.push(json!({"type":"assistant", "sessionId":SID, "cwd":lab.work,
                "uuid":format!("answer-{index}"), "message":{"role":"assistant", "content":[{"type":"text", "text":format!("answer {index}")}]}}));
        }
        records.extend(extra);
        fs::write(
            project.join(format!("{SID}.jsonl")),
            records
                .iter()
                .map(|record| format!("{record}\n"))
                .collect::<String>(),
        )
        .unwrap();
        lab.ok(&["init", "qa"]);
        lab.ok(&["import", SID, "--into", "me/qa@work", "--independent"]);
        lab.ok(&["revert", "me/qa@work#2"]);
        lab
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agit"));
        cmd.env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("HOME", &self.home)
            .env("AGIT_HOME", &self.store)
            .env("AGIT_HUB_URL", "http://127.0.0.1:1")
            .env("AGIT_SESSION", "me/qa@work")
            .env("AGIT_SECRETS_KEYSTORE", "file")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("CI", "1")
            .env("AGIT_YES", "1")
            .env("NO_COLOR", "1")
            .current_dir(&self.work);
        cmd
    }

    fn ok(&self, args: &[&str]) -> String {
        let output = self.command().args(args).output().unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }
}

/// Repository selections obey VIEW while an explicit native selector retains its live evidence.
#[test]
fn repository_show_never_widens_a_reverted_view_to_the_log() {
    let lab = Lab::new();
    for args in [
        vec!["show"],
        vec!["show", "work", "--agent", "me/qa"],
        vec!["show", "me/qa@work"],
    ] {
        let text = lab.ok(&args);
        assert!(
            text.contains("VISIBLE-FIRST") && text.contains("VISIBLE-LAST"),
            "{args:?}: {text}"
        );
        assert!(!text.contains("HIDDEN-TURN"), "{args:?}: {text}");
    }
    assert!(lab.ok(&["show", SID]).contains("HIDDEN-TURN"));
    assert!(lab.ok(&["show", "me/qa@work#2"]).contains("HIDDEN-TURN"));
}

/// An unreadable saved VIEW is refused even when the full LOG remains readable.
#[test]
fn repository_show_refuses_a_corrupt_view() {
    let lab = Lab::new();
    let repo = agit::domain::repo::Repo::open(lab.store.join("repos/me/qa")).unwrap();
    let worktree = repo
        .worktrees()
        .unwrap()
        .into_iter()
        .find(|tree| tree.branch.as_deref() == Some("work"))
        .unwrap();
    let branch = agit::domain::repo::Repo::open(&worktree.path).unwrap();
    fs::write(
        branch.root().join(agit::domain::meta::VIEW_FILE),
        format!("{}\n", "0".repeat(40)),
    )
    .unwrap();
    branch.add_all().unwrap();
    branch
        .git(&[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "unreadable selected view",
        ])
        .unwrap();
    for args in [vec!["show"], vec!["show", "work", "--agent", "me/qa"]] {
        let output = lab.command().args(&args).output().unwrap();
        assert!(!output.status.success(), "{args:?} accepted a corrupt VIEW");
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(
            !text.contains("VISIBLE-FIRST") && !text.contains("HIDDEN-TURN"),
            "{args:?}: {text}"
        );
    }
    assert!(lab.ok(&["show", SID]).contains("HIDDEN-TURN"));
}

/// A turnless session still requires a readable VIEW; only a valid empty VIEW is empty evidence.
#[test]
fn a_turnless_session_does_not_hide_view_corruption() {
    let lab = Lab::new();
    lab.ok(&["new", "me/qa", "-b", "empty", "--no-launch"]);
    lab.ok(&["show", "me/qa@empty"]);
    let repo = agit::domain::repo::Repo::open(lab.store.join("repos/me/qa")).unwrap();
    let tree = repo
        .worktrees()
        .unwrap()
        .into_iter()
        .find(|tree| tree.branch.as_deref() == Some("empty"))
        .unwrap();
    let branch = agit::domain::repo::Repo::open(tree.path).unwrap();
    assert!(
        agit::domain::meta::resolve(branch.root())
            .unwrap()
            .turn
            .is_none()
    );
    fs::write(
        branch.root().join(agit::domain::meta::VIEW_FILE),
        format!("{}\n", "0".repeat(40)),
    )
    .unwrap();
    branch.add_all().unwrap();
    branch
        .git(&[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "unreachable view",
        ])
        .unwrap();
    for args in [
        vec!["show", "me/qa@empty"],
        vec!["show", "empty"],
        vec!["show", "empty", "--agent", "me/qa"],
        vec!["--json", "show", "me/qa@empty"],
    ] {
        let output = lab.command().args(&args).output().unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!output.status.success(), "{args:?}: {text}");
        assert!(text.contains("not reachable from LOG"), "{args:?}: {text}");
        assert!(!text.contains("no turns settled yet"), "{args:?}: {text}");
        if args[0] == "--json" {
            let payload: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(payload["ok"], false);
        }
    }
}

/// Unsupported ranges cannot be mistaken for the cumulative VIEW at their upper bound.
#[test]
fn show_refuses_ranges_instead_of_rendering_a_different_scope() {
    let lab = Lab::new();
    for args in [
        vec!["show", "me/qa@work#2..2"],
        vec!["show", "me/qa@work#3..3"],
        vec!["--json", "show", "me/qa@work#2..3"],
    ] {
        let output = lab.command().args(&args).output().unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.status.code(), Some(2), "{args:?}: {text}");
        assert!(text.contains("does not support turn ranges"), "{text}");
        assert!(
            !text.contains("VISIBLE-FIRST") && !text.contains("HIDDEN-TURN"),
            "{text}"
        );
    }
    assert!(lab.ok(&["show", "me/qa@work#2"]).contains("HIDDEN-TURN"));
}

/// Full history requires an explicit mode, and a turn selector keeps its own evidence boundary.
#[test]
fn log_only_exposes_hidden_history_without_widening_a_turn() {
    let lab = Lab::new();
    for mut args in [
        vec!["show"],
        vec!["show", "@"],
        vec!["show", "work"],
        vec!["show", "work", "--agent", "me/qa"],
        vec!["show", "me/qa@work"],
    ] {
        let view = lab.ok(&args);
        assert!(!view.contains("HIDDEN-TURN"), "{args:?}: {view}");
        args.push("--log-only");
        let log = lab.ok(&args);
        assert!(log.contains("repository LOG"), "{args:?}: {log}");
        for prompt in ["VISIBLE-FIRST", "HIDDEN-TURN", "VISIBLE-LAST"] {
            assert!(log.contains(prompt), "{args:?}: {log}");
        }
    }
    let turn = lab.ok(&["show", "me/qa@work#2", "--log-only"]);
    assert!(turn.contains("HIDDEN-TURN"));
    assert!(!turn.contains("VISIBLE-FIRST") && !turn.contains("VISIBLE-LAST"));
    let native = lab.ok(&["show", SID, "--log-only"]);
    assert!(native.contains("live transcript") && native.contains("HIDDEN-TURN"));
    for target in [
        "me/qa@main",
        "me/qa@work#2.1",
        "me/qa@main:AGENTS.md",
        "me/qa@work#1..2",
    ] {
        let output = lab
            .command()
            .args(["show", target, "--log-only"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{target}: {output:?}");
        assert!(!String::from_utf8_lossy(&output.stdout).contains("VISIBLE-FIRST"));
    }
    let file = lab.ok(&["show", "me/qa@main:AGENTS.md"]);
    let repo = agit::domain::repo::Repo::open(lab.store.join("repos/me/qa")).unwrap();
    assert_eq!(
        file,
        repo.show_raw_result("main", "AGENTS.md").unwrap().unwrap()
    );
}

/// LOG selection validates its own objects without requiring a valid VIEW or substituting one.
#[test]
fn log_only_reads_an_intact_log_beside_a_corrupt_view_and_refuses_missing_log() {
    let lab = Lab::new();
    let repo = agit::domain::repo::Repo::open(lab.store.join("repos/me/qa")).unwrap();
    let tree = repo
        .worktrees()
        .unwrap()
        .into_iter()
        .find(|tree| tree.branch.as_deref() == Some("work"))
        .unwrap();
    let branch = agit::domain::repo::Repo::open(tree.path).unwrap();
    fs::write(branch.root().join("VIEW"), format!("{}\n", "0".repeat(40))).unwrap();
    branch.add_all().unwrap();
    branch
        .git(&[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "synthetic invalid view",
        ])
        .unwrap();
    for mut args in [
        vec!["show"],
        vec!["show", "work", "--agent", "me/qa"],
        vec!["show", "me/qa@work"],
    ] {
        let view = lab.command().args(&args).output().unwrap();
        assert!(!view.status.success(), "{args:?}: {view:?}");
        args.push("--log-only");
        assert!(lab.ok(&args).contains("HIDDEN-TURN"));
    }
    fs::remove_file(branch.root().join("LOG")).unwrap();
    branch.add_all().unwrap();
    branch
        .git(&[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "synthetic missing log",
        ])
        .unwrap();
    for args in [
        vec!["show", "--log-only"],
        vec!["show", "work", "--agent", "me/qa", "--log-only"],
        vec!["show", "me/qa@work", "--log-only"],
    ] {
        let output = lab.command().args(&args).output().unwrap();
        assert!(!output.status.success(), "{args:?}: {output:?}");
        assert!(!String::from_utf8_lossy(&output.stdout).contains("HIDDEN-TURN"));
    }
}

/// Current-context LOG reads stay on their local branch despite other ref namespaces.
#[test]
fn log_only_keeps_at_on_the_exact_local_branch() {
    let lab = Lab::new();
    let repo = agit::domain::repo::Repo::open(lab.store.join("repos/me/qa")).unwrap();
    repo.git(&["tag", "work", "main"]).unwrap();
    let tip = repo.git(&["rev-parse", "refs/heads/work"]).unwrap();
    repo.git(&["branch", &tip, "main"]).unwrap();
    assert!(lab.ok(&["show", "@", "--log-only"]).contains("HIDDEN-TURN"));
    repo.git(&["tag", "missing", "refs/heads/work"]).unwrap();
    repo.git(&[
        "update-ref",
        "refs/remotes/origin/missing",
        "refs/heads/work",
    ])
    .unwrap();
    for target in [None, Some("@")] {
        let mut cmd = lab.command();
        cmd.env("AGIT_SESSION", "me/qa@missing").arg("show");
        if let Some(target) = target {
            cmd.arg(target);
        }
        let output = cmd.arg("--log-only").output().unwrap();
        assert!(!output.status.success(), "{target:?}: {output:?}");
        assert!(!String::from_utf8_lossy(&output.stdout).contains("HIDDEN-TURN"));
    }
}

/// Raw rendering retains native values, including opaque records and long tool results, in order.
#[test]
fn raw_preserves_native_values_and_selected_evidence_without_truncation() {
    let lab = Lab::with_records(vec![
        json!({"type":"assistant", "sessionId":SID, "uuid":"raw-tool",
            "message":{"role":"assistant", "content":[{"type":"tool_use", "id":"raw-call", "name":"Bash", "input":{"command":"true"}}]}}),
        json!({"type":"user", "sessionId":SID, "uuid":"raw-result",
            "message":{"role":"user", "content":[{"type":"tool_result", "tool_use_id":"raw-call", "content":"LONG-RESULT-".repeat(500)}]}}),
        json!({"type":"assistant", "sessionId":SID, "uuid":"raw-end",
            "message":{"role":"assistant", "content":"RAW-END"}}),
        json!({"type":"system", "subtype":"synthetic-opaque", "sessionId":SID,
            "uuid":"raw-opaque", "payload":{"vendor_field":["preserve", true, null]}}),
    ]);
    let source = lab
        .home
        .join(format!(".claude/projects/synthetic/{SID}.jsonl"));
    let native = fs::read_to_string(&source).unwrap();
    let records: Vec<serde_json::Value> = native
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let repo = agit::domain::repo::Repo::open(lab.store.join("repos/me/qa")).unwrap();
    let frozen = repo.git(&["rev-parse", "refs/heads/work"]).unwrap();

    let mut stored = records.clone();
    stored.push(json!({"type":"system", "subtype":"agit:__revert__", "source":"me/qa@work#2"}));
    let visible: Vec<_> = stored
        .iter()
        .filter(|record| record["uuid"] != "user-1" && record["uuid"] != "answer-1")
        .cloned()
        .collect();
    let values = |text: &str| -> Vec<serde_json::Value> {
        text.lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    };
    for mut args in [
        vec!["show", "--raw"],
        vec!["show", "@", "--raw"],
        vec!["show", "work", "--raw"],
        vec!["show", "work", "--agent", "me/qa", "--raw"],
        vec!["show", "me/qa@work", "--raw"],
    ] {
        assert_eq!(values(&lab.ok(&args)), visible, "{args:?}");
        args.push("--log-only");
        assert_eq!(values(&lab.ok(&args)), stored, "{args:?}");
    }
    assert_eq!(lab.ok(&["show", SID, "--raw"]), native);
    let old_target = format!("me/qa@{frozen}");
    assert_eq!(values(&lab.ok(&["show", &old_target, "--raw"])), visible);
    assert_eq!(
        values(&lab.ok(&["show", "me/qa@work#2", "--raw"])),
        records[2..4]
    );
    assert_eq!(
        values(&lab.ok(&["show", "me/qa@work#2", "--log-only", "--raw"])),
        records[2..4]
    );
    assert_eq!(
        values(&lab.ok(&["show", "me/qa@work#2.1", "--raw"])),
        records[2..3]
    );
    assert_eq!(
        lab.ok(&["show", "me/qa@main:AGENTS.md", "--raw"]),
        lab.ok(&["show", "me/qa@main:AGENTS.md"])
    );
    let wrapped: serde_json::Value =
        serde_json::from_str(&lab.ok(&["--json", "show", "me/qa@work", "--raw"])).unwrap();
    assert_eq!(wrapped["ok"], true);
    assert_eq!(wrapped["result"]["format"], "json_lines");
    assert_eq!(wrapped["result"]["values"], json!(visible));
    for args in [
        vec!["show", "@", "--raw", "--max-chars", "5"],
        vec!["show", "@", "--raw", "--max-chars", "2000"],
        vec!["show", "me/qa@main", "--raw"],
    ] {
        let output = lab.command().args(&args).output().unwrap();
        assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
        assert!(!String::from_utf8_lossy(&output.stdout).contains("VISIBLE-FIRST"));
    }
    let tree = repo
        .worktrees()
        .unwrap()
        .into_iter()
        .find(|tree| tree.branch.as_deref() == Some("work"))
        .unwrap();
    let branch = agit::domain::repo::Repo::open(tree.path).unwrap();
    fs::write(branch.root().join("VIEW"), format!("{}\n", "0".repeat(40))).unwrap();
    branch.git(&["read-tree", "HEAD"]).unwrap();
    branch.git(&["add", "VIEW"]).unwrap();
    branch
        .git(&[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "synthetic unreadable view",
        ])
        .unwrap();
    assert!(
        !lab.command()
            .args(["show", "@", "--raw"])
            .output()
            .unwrap()
            .status
            .success()
    );
    assert_eq!(values(&lab.ok(&["show", &old_target, "--raw"])), visible);
    assert_eq!(
        values(&lab.ok(&["show", "@", "--raw", "--log-only"])),
        stored
    );
}

/// Native source groups retain interleaved records without exposing evidence outside the VIEW.
#[test]
fn mixed_saved_sources_render_in_order_and_raw_retains_native_values() {
    use agit::domain::{meta, repo::Repo, storage, transcript};
    let lab = Lab::new();
    let repo = Repo::open(lab.store.join("repos/me/qa")).unwrap();
    let tree = repo
        .worktrees()
        .unwrap()
        .into_iter()
        .find(|tree| tree.branch.as_deref() == Some("work"))
        .unwrap();
    let branch = Repo::open(&tree.path).unwrap();
    branch.git(&["read-tree", "HEAD"]).unwrap();
    let prior = branch.show_result("HEAD", meta::LOG_FILE).unwrap().unwrap();
    let records = [
        (
            "claude-code",
            'a',
            json!({"type":"user", "message":{"role":"user", "content":"MIXED-CLAUDE"}}),
        ),
        (
            "opencode",
            'b',
            json!({"kind":"message", "id":"same", "data":{"role":"user"}}),
        ),
        (
            "opencode",
            'c',
            json!({"kind":"message", "id":"same", "data":{"role":"assistant"}}),
        ),
        (
            "claude-code",
            'a',
            json!({"type":"assistant", "message":{"role":"assistant", "content":[{"type":"tool_use", "id":"call", "name":"Bash", "input":{"command":"true"}}]}}),
        ),
        (
            "codex",
            'd',
            json!({"type":"response_item", "payload":{"type":"message", "role":"user", "content":[{"type":"input_text", "text":"MIXED-CODEX"}]}}),
        ),
        (
            "opencode",
            'b',
            json!({"kind":"part", "message_id":"same", "data":{"type":"text", "text":"MIXED-OPEN-USER"}}),
        ),
        (
            "codex",
            'd',
            json!({"type":"response_item", "payload":{"type":"function_call", "call_id":"call", "name":"exec_command", "arguments":"{}"}}),
        ),
        (
            "claude-code",
            'a',
            json!({"type":"user", "message":{"role":"user", "content":[{"type":"tool_result", "tool_use_id":"call", "content":"MIXED-CLAUDE-RESULT"}]}}),
        ),
        (
            "opencode",
            'c',
            json!({"kind":"part", "message_id":"same", "data":{"type":"text", "text":"MIXED-OPEN-ASSISTANT"}}),
        ),
        (
            "codex",
            'd',
            json!({"type":"response_item", "payload":{"type":"function_call_output", "call_id":"call", "output":"MIXED-CODEX-RESULT"}}),
        ),
        (
            "codex",
            'e',
            json!({"type":"user", "agit":"merge_summary", "message":{"role":"user", "content":"MIXED-SUMMARY"}}),
        ),
    ];
    let selected: String = records
        .iter()
        .map(|(source, claim, native)| {
            transcript::wrap_lines(
                &native.to_string(),
                source,
                &format!("agit-{}", claim.to_string().repeat(40)),
            )
        })
        .collect();
    storage::write_snapshot(branch.root(), &format!("{prior}{selected}"), &selected).unwrap();
    let mut snapshot = meta::read_at_ref_result(&branch, "HEAD").unwrap().unwrap();
    snapshot.turn = Some(4);
    snapshot.kind = meta::Kind::Turn;
    meta::write(branch.root(), &snapshot).unwrap();
    branch.add_all().unwrap();
    branch
        .git(&[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "mixed native evidence",
        ])
        .unwrap();
    for args in [
        vec!["show"],
        vec!["show", "@"],
        vec!["show", "work", "--agent", "me/qa"],
        vec!["show", "me/qa@work"],
        vec!["show", "me/qa@work#4"],
        vec!["show", "me/qa@work", "--log-only"],
    ] {
        let text = lab.ok(&args);
        let ordered = [
            "MIXED-CLAUDE",
            "call: Bash",
            "MIXED-CODEX",
            "MIXED-OPEN-USER",
            "call: exec_command",
            "MIXED-CLAUDE-RESULT",
            "MIXED-OPEN-ASSISTANT",
            "MIXED-CODEX-RESULT",
            "MIXED-SUMMARY",
        ];
        let mut rest = text.as_str();
        for marker in ordered {
            let at = rest
                .find(marker)
                .unwrap_or_else(|| panic!("{args:?} missing or reordered {marker}: {text}"));
            rest = &rest[at + marker.len()..];
        }
        assert_eq!(
            text.contains("HIDDEN-TURN"),
            args.contains(&"--log-only"),
            "{args:?}: {text}"
        );
        assert!(!text.contains("_object_hash"), "{args:?}: {text}");
    }
    let raw = lab.ok(&["show", "@", "--raw"]);
    let actual: Vec<serde_json::Value> = raw
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        actual,
        records
            .iter()
            .map(|(_, _, value)| value.clone())
            .collect::<Vec<_>>()
    );
    let parsed = transcript::display::parse(&selected).unwrap();
    let roles: Vec<_> = parsed
        .events
        .iter()
        .filter_map(|event| {
            event
                .text
                .as_deref()
                .filter(|text| text.starts_with("MIXED-OPEN"))
                .map(|text| (text, event.kind))
        })
        .collect();
    assert_eq!(
        roles,
        [
            ("MIXED-OPEN-USER", agit::adapter::EventKind::UserPrompt),
            (
                "MIXED-OPEN-ASSISTANT",
                agit::adapter::EventKind::AssistantReply
            )
        ]
    );

    let unknown = transcript::wrap_lines(
        &records[0].2.to_string(),
        "future-runtime",
        &snapshot.session,
    );
    storage::write_snapshot(
        branch.root(),
        &format!("{prior}{selected}{unknown}"),
        &unknown,
    )
    .unwrap();
    branch.add_all().unwrap();
    branch
        .git(&[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "unknown native source",
        ])
        .unwrap();
    let output = lab.command().args(["show", "me/qa@work"]).output().unwrap();
    assert!(
        !output.status.success(),
        "unknown source must not be guessed"
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&lab.ok(&["show", "me/qa@work", "--raw"]))
            .unwrap(),
        records[0].2
    );
}
