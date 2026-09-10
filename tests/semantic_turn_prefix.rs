//! Semantic reconnaissance reads frozen LOG content without assigning Git ancestry.

use agit::domain::{mergetx, meta, repo::Repo, storage, transcript};
use serde_json::{Value, json};
use std::{collections::BTreeMap, fs, path::Path, process::Command};

struct Lab(tempfile::TempDir);

impl Lab {
    fn new() -> Self {
        Self(tempfile::tempdir().unwrap())
    }

    fn command(&self, executable: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = Command::new(executable);
        command
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.0.path())
            .env("AGIT_HOME", self.0.path())
            .env("AGIT_HUB_URL", "http://127.0.0.1:1")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", self.0.path().join("empty-gitconfig"))
            .env("GIT_AUTHOR_NAME", "Synthetic Author")
            .env("GIT_AUTHOR_EMAIL", "fixture@example.test")
            .env("GIT_COMMITTER_NAME", "Synthetic Author")
            .env("GIT_COMMITTER_EMAIL", "fixture@example.test")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("CI", "1")
            .env("NO_COLOR", "1")
            .current_dir(self.0.path());
        #[cfg(windows)]
        {
            for name in ["SystemRoot", "WINDIR", "TEMP", "TMP", "ComSpec"] {
                if let Some(value) = std::env::var_os(name) {
                    command.env(name, value);
                }
            }
            command.env("USERPROFILE", self.0.path());
        }
        command
    }

    fn git(&self, repo: &Repo, args: &[&str]) -> String {
        success(
            self.command("git")
                .current_dir(repo.root())
                .args(args)
                .output()
                .unwrap(),
        )
        .trim()
        .to_string()
    }

    fn repo(&self, name: &str) -> Repo {
        self.make_repo(name, true)
    }

    fn make_repo(&self, name: &str, declared: bool) -> Repo {
        let root = self.0.path().join("repos/alice").join(name);
        fs::create_dir_all(&root).unwrap();
        let repo = Repo::at(root);
        self.git(&repo, &["init", "-b", "main"]);
        if declared {
            meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
        }
        fs::write(repo.root().join("AGENTS.md"), name).unwrap();
        self.commit(&repo, name);
        self.git(&repo, &["checkout", "-b", "topic"]);
        repo
    }

    fn commit(&self, repo: &Repo, message: &str) -> String {
        self.git(repo, &["add", "-A"]);
        self.git(
            repo,
            &["-c", "commit.gpgsign=false", "commit", "-m", message],
        );
        self.git(repo, &["rev-parse", "HEAD"])
    }

    fn save(&self, repo: &Repo, log: &str, view: &str, runtime: &str, claim: char) {
        storage::write_snapshot(repo.root(), log, view).unwrap();
        let mut metadata = meta::Meta::new(identity(claim), runtime.into(), "/fixture".into());
        metadata.turn = Some(1);
        meta::write(repo.root(), &metadata).unwrap();
        self.commit(repo, "save synthetic transcript");
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        self.command(env!("CARGO_BIN_EXE_agit"))
            .args(args)
            .output()
            .unwrap()
    }

    fn diff(&self, dots: &str) -> String {
        success(self.run(&[
            "diff",
            &format!("alice/left@topic{dots}alice/right@topic"),
            "--turns",
        ]))
    }
}

fn identity(claim: char) -> String {
    format!("agit-{}", claim.to_string().repeat(40))
}

fn envelope(runtime: &str, claim: char, value: Value) -> String {
    transcript::wrap_lines(&value.to_string(), runtime, &identity(claim))
}

fn codex(role: &str, text: &str, claim: char) -> String {
    envelope(
        "codex",
        claim,
        json!({"type":"response_item", "timestamp":format!("{claim}-time"), "payload":{
            "type":"message", "role":role, "content":[{"type":if role == "user" {"input_text"} else {"output_text"}, "text":text}]
        }}),
    )
}

fn claude(role: &str, text: &str, claim: char) -> String {
    envelope(
        "claude-code",
        claim,
        json!({"type":role, "sessionId":format!("native-{claim}"), "timestamp":format!("{claim}-time"), "cwd":format!("/machine-{claim}"), "message":{"role":role,"content":text}}),
    )
}

fn success(output: std::process::Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn bytes_under(root: &Path) -> BTreeMap<std::path::PathBuf, Vec<u8>> {
    walkdir::WalkDir::new(root)
        .into_iter()
        .map(Result::unwrap)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| {
            (
                entry.path().strip_prefix(root).unwrap().to_owned(),
                fs::read(entry.path()).unwrap(),
            )
        })
        .collect()
}

#[test]
fn unrelated_cross_runtime_logs_report_prefix_and_suffixes_without_changing_sources() {
    let lab = Lab::new();
    let left = lab.repo("left");
    let right = lab.repo("right");
    let left_log = [
        codex("user", "shared", 'a'),
        codex("assistant", "answer", 'a'),
        codex("user", "left", 'a'),
    ]
    .concat();
    let right_log = [
        claude("user", "shared", 'b'),
        claude("assistant", "answer", 'b'),
        claude("user", "right", 'b'),
        claude("user", "next", 'b'),
    ]
    .concat();
    lab.save(&left, &left_log, &codex("user", "left", 'a'), "codex", 'a');
    lab.save(
        &right,
        &right_log,
        &claude("user", "next", 'b'),
        "claude-code",
        'b',
    );
    success(lab.run(&["diff", "alice/left@topic..topic", "--files"]));
    let before_left = bytes_under(left.root());
    let before_right = bytes_under(right.root());
    for dots in ["..", "..."] {
        let text = lab.diff(dots);
        assert!(
            text.contains("semantic prefix  1 normalized LOG turns"),
            "{text}"
        );
        assert!(
            text.contains("A semantic suffix  +1 turns    B semantic suffix  +2 turns"),
            "{text}"
        );
        assert!(
            text.contains("not complete transcript equality or Git ancestry"),
            "{text}"
        );
        assert!(!text.contains("fork point"), "{text}");
    }
    let text = success(lab.run(&[
        "merge",
        "alice/right@topic",
        "--into",
        "alice/left@topic",
        "--dry-run",
    ]));
    assert!(text.contains("fork point  unavailable"), "{text}");
    assert!(
        text.contains(
            "this side semantic suffix  +1 turns    source side semantic suffix  +2 turns"
        ),
        "{text}"
    );
    assert_eq!(bytes_under(left.root()), before_left);
    assert_eq!(bytes_under(right.root()), before_right);
    assert!(mergetx::read(left.root()).unwrap().is_none());
}

#[test]
fn projected_matches_keep_exclusions_visible_and_never_enter_transaction_ancestry() {
    let lab = Lab::new();
    let left = lab.repo("left");
    let right = lab.repo("right");
    for (repo, claim, output) in [(&left, 'a', "left result"), (&right, 'b', "right result")] {
        let log = [
            codex("user", "same prompt", claim),
            envelope("codex", claim, json!({"type":"response_item", "payload":{"type":"function_call", "call_id":"call", "name":"shell", "arguments":"{}"}})),
            envelope("codex", claim, json!({"type":"response_item", "payload":{"type":"function_call_output", "call_id":"call", "output":output}})),
        ].concat();
        lab.save(repo, &log, &log, "codex", claim);
    }
    let text = lab.diff("...");
    assert!(
        text.contains("semantic prefix  1 normalized LOG turns"),
        "{text}"
    );
    assert!(
        text.contains("A semantic suffix  +0 turns    B semantic suffix  +0 turns"),
        "{text}"
    );
    assert!(
        text.contains("excludes metadata, tool results, compaction and unmodeled content"),
        "{text}"
    );
    let original = lab.git(&left, &["rev-parse", "refs/heads/topic"]);
    let source = lab.git(&right, &["rev-parse", "refs/heads/topic"]);
    success(lab.run(&[
        "merge",
        "alice/right@topic",
        "--into",
        "alice/left@topic",
        "--manual",
    ]));
    let tx = mergetx::read(left.root()).unwrap().unwrap();
    assert!(tx.base.is_empty());
    assert_eq!(tx.target_head, original);
    assert_eq!(lab.git(&left, &["rev-parse", "refs/heads/topic"]), original);
    success(lab.run(&[
        "merge",
        "--into",
        "alice/left@topic",
        "summary",
        "-m",
        "Retain both recorded outcomes",
    ]));
    success(lab.run(&["merge", "--continue", "--into", "alice/left@topic"]));
    let merged = lab.git(&left, &["rev-parse", "refs/heads/topic"]);
    assert_eq!(
        lab.git(&left, &["show", "-s", "--format=%P", &merged]),
        format!("{original} {source}")
    );
    let log = storage::materialize_at(left.root(), &merged, meta::LOG_FILE).unwrap();
    assert!(log.contains("left result") && log.contains("right result"));
    assert!(mergetx::read(left.root()).unwrap().is_none());
}

#[test]
fn matching_later_turns_do_not_extend_a_diverged_prefix_and_control_text_is_not_structure() {
    let lab = Lab::new();
    let left = lab.repo("left");
    let right = lab.repo("right");
    let a = [
        codex("user", "x\x1ea\x1fy", 'a'),
        codex("user", "matching later", 'a'),
    ]
    .concat();
    let b = [
        codex("user", "x", 'b'),
        codex("assistant", "y", 'b'),
        codex("user", "matching later", 'b'),
    ]
    .concat();
    lab.save(&left, &a, &a, "codex", 'a');
    lab.save(&right, &b, &b, "codex", 'b');
    let text = lab.diff("...");
    assert!(
        text.contains("semantic prefix  0 normalized LOG turns"),
        "{text}"
    );
    assert!(
        text.contains("A semantic suffix  +2 turns    B semantic suffix  +2 turns"),
        "{text}"
    );
    assert!(!text.contains("semantic hash"));
}

#[test]
fn empty_and_preamble_only_endpoints_do_not_claim_equal_histories() {
    for preamble in [false, true] {
        let lab = Lab::new();
        let left = lab.repo("left");
        let right = lab.repo("right");
        if preamble {
            for (repo, claim) in [(&left, 'a'), (&right, 'b')] {
                let log = codex("assistant", "preamble", claim);
                lab.save(repo, &log, &log, "codex", claim);
            }
        }
        let text = lab.diff("...");
        assert!(
            text.contains("semantic prefix unavailable (an endpoint has no comparable user turns)"),
            "{text}"
        );
        assert!(!text.contains("semantic suffix"));
    }
}

#[test]
fn mixed_sessions_keep_native_identity_groups_and_saved_event_order() {
    let lab = Lab::new();
    let left = lab.repo("left");
    let right = lab.repo("right");
    let message = |role| json!({"kind":"message", "id":"same-native-id", "data":{"role":role}});
    let part = |text| json!({"kind":"part", "message_id":"same-native-id", "data":{"type":"text", "text":text}});
    let mixed = [
        envelope("opencode", 'a', message("user")),
        envelope("opencode", 'b', message("assistant")),
        claude("user", "opening prompt", 'c'),
        envelope("opencode", 'a', part("next prompt")),
        codex("assistant", "first reply", 'd'),
        envelope("opencode", 'b', part("second reply")),
    ]
    .concat();
    let native = [
        codex("user", "opening prompt", 'e'),
        codex("user", "next prompt", 'e'),
        codex("assistant", "first reply", 'e'),
        codex("assistant", "second reply", 'e'),
    ]
    .concat();
    lab.save(&left, &mixed, &mixed, "opencode", 'a');
    lab.save(&right, &native, &native, "codex", 'e');
    let text = lab.diff("...");
    assert!(
        text.contains("semantic prefix  2 normalized LOG turns"),
        "{text}"
    );
    assert!(
        text.contains("A semantic suffix  +0 turns    B semantic suffix  +0 turns"),
        "{text}"
    );
}

#[test]
fn semantic_report_preserves_explicit_context_and_json_contract_versions() {
    let lab = Lab::new();
    let left = lab.repo("left");
    let right = lab.repo("right");
    for (repo, claim) in [(&left, 'a'), (&right, 'b')] {
        let log = codex("user", "same", claim);
        lab.save(repo, &log, &log, "codex", claim);
    }
    for version in [1, 2] {
        let output = lab
            .command(env!("CARGO_BIN_EXE_agit"))
            .env("AGIT_SESSION", "alice/right@topic")
            .args([
                "--json",
                "--json-version",
                &version.to_string(),
                "diff",
                "alice/left@topic...@",
                "--turns",
            ])
            .output()
            .unwrap();
        let document: Value = serde_json::from_str(&success(output)).unwrap();
        assert_eq!(document["schema"], "cli-output");
        assert_eq!(document["schema_version"], version);
        assert_eq!(document["result"]["format"], "text");
        assert_eq!(document["result"]["kind"], "diff");
        assert_eq!(document["exit_code"], 0);
        assert!(
            document["result"]["lines"]
                .as_array()
                .unwrap()
                .iter()
                .any(|line| line == "semantic prefix  1 normalized LOG turns")
        );
        assert_eq!(document.get("fix").is_some(), version == 2);
        assert_eq!(
            document.as_object().unwrap().len(),
            if version == 2 { 8 } else { 7 }
        );
    }
}

#[test]
fn unsupported_sources_and_damaged_logs_fail_before_reporting_a_semantic_match() {
    for damage in [
        "unknown-source",
        "missing-log",
        "bad-sequence",
        "bad-object",
        "missing-meta",
        "bad-meta",
    ] {
        let lab = Lab::new();
        let left = lab.repo("left");
        let right = lab.repo("right");
        let log = codex("user", "same", 'a');
        lab.save(&left, &log, &log, "codex", 'a');
        let log = if damage == "unknown-source" {
            envelope(
                "future-runtime",
                'b',
                json!({"type":"future", "text":"same"}),
            )
        } else {
            codex("user", "same", 'b')
        };
        lab.save(&right, &log, &log, "codex", 'b');
        success(lab.run(&["diff", "alice/left@topic..topic", "--files"]));
        match damage {
            "missing-meta" => fs::remove_file(right.root().join(meta::FILE)).unwrap(),
            "bad-meta" => fs::write(right.root().join(meta::FILE), "{").unwrap(),
            "missing-log" => fs::remove_file(right.root().join(meta::LOG_FILE)).unwrap(),
            "bad-sequence" => {
                fs::write(right.root().join(meta::LOG_FILE), "not an event id\n").unwrap()
            }
            "bad-object" => {
                let path = walkdir::WalkDir::new(right.root().join("events"))
                    .into_iter()
                    .map(Result::unwrap)
                    .find(|entry| entry.file_type().is_file())
                    .unwrap()
                    .into_path();
                fs::write(path, log.replace("same", "different")).unwrap();
            }
            _ => {}
        }
        if damage != "unknown-source" {
            lab.commit(&right, "record synthetic corruption");
        }
        let before = bytes_under(right.root());
        let output = lab.run(&["diff", "alice/left@topic...alice/right@topic", "--turns"]);
        assert!(!output.status.success(), "{damage}: {output:?}");
        assert!(output.stdout.is_empty(), "{damage}: {output:?}");
        assert_eq!(bytes_under(right.root()), before);
    }
}

#[test]
fn ordinary_git_histories_keep_graph_comparison_without_inventing_semantic_turns() {
    let lab = Lab::new();
    lab.make_repo("left", false);
    lab.make_repo("right", false);
    for dots in ["..", "..."] {
        let text = lab.diff(dots);
        assert!(text.contains("base"), "{text}");
        assert!(text.contains("B side    +1 turns"), "{text}");
        assert!(
            text.contains(
                "semantic prefix unavailable (an endpoint has no AgentGit session declaration)"
            ),
            "{text}"
        );
        assert!(!text.contains("semantic suffix"));
    }
}
