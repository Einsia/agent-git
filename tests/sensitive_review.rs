use agit::domain::{link, meta, repo::Repo, storage, store::Store, transcript};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const SLUG: &str = "local/sensitive-review";
const CLAIM: &str = "agit-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

struct Fixture {
    temporary: tempfile::TempDir,
    home: PathBuf,
    repo: Repo,
    layout: meta::LayoutVersion,
}

impl Fixture {
    fn new() -> Self {
        Self::with_layout(meta::LayoutVersion::V1)
    }

    fn with_layout(layout: meta::LayoutVersion) -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().join("home");
        let repo = Repo::init(&home.join("repos").join(SLUG)).unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        let mut snapshot = meta::Meta::new_file_line();
        snapshot.layout = layout;
        meta::write(repo.root(), &snapshot).unwrap();
        repo.add_all().unwrap();
        repo.commit("initialize synthetic file line").unwrap();
        repo.git(&["switch", "-c", "selected"]).unwrap();
        let fixture = Self {
            temporary,
            home,
            repo,
            layout,
        };
        fixture.record("SELECTED-SYNTHETIC-EVIDENCE", 1);
        fixture
    }

    fn record(&self, text: &str, turn: u32) {
        let head = self.head();
        let prefix =
            storage::materialize_at(self.repo.root(), &head, meta::LOG_FILE).unwrap_or_default();
        let raw = serde_json::json!({"type": "user", "message": {"role": "user", "content": text}})
            .to_string();
        let log = format!(
            "{prefix}{}",
            transcript::wrap_lines(&raw, "claude-code", CLAIM)
        );
        let mut snapshot = meta::Meta::new(CLAIM.into(), "claude-code".into(), "/synthetic".into());
        snapshot.kind = meta::Kind::Turn;
        snapshot.turn = Some(turn);
        snapshot.layout = self.layout;
        meta::write(self.repo.root(), &snapshot).unwrap();
        match self.layout {
            meta::LayoutVersion::V0 => {
                std::fs::write(self.repo.root().join(meta::LEGACY_LOG_FILE), &log).unwrap();
                std::fs::write(self.repo.root().join(meta::LEGACY_VIEW_FILE), &log).unwrap();
            }
            meta::LayoutVersion::V1 => {
                storage::write_snapshot(self.repo.root(), &log, &log).unwrap();
            }
        }
        self.repo.add_all().unwrap();
        self.repo.commit("record synthetic evidence").unwrap();
    }

    fn head(&self) -> String {
        self.repo.git(&["rev-parse", "HEAD"]).unwrap().trim().into()
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
        command.env_clear();
        for key in [
            "PATH",
            "SystemRoot",
            "WINDIR",
            "TEMP",
            "TMP",
            "ComSpec",
            "PATHEXT",
        ] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        command
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("AGIT_HOME", &self.home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("CI", "1")
            .current_dir(self.temporary.path())
            .args(args);
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn legacy() -> Self {
        let fixture = Self::with_layout(meta::LayoutVersion::V0);
        fixture.repo.git(&["branch", "unselected"]).unwrap();
        let other = Repo::init(&fixture.home.join("repos/local/other")).unwrap();
        other.git(&["config", "commit.gpgsign", "false"]).unwrap();
        let mut snapshot = meta::Meta::new_file_line();
        snapshot.layout = meta::LayoutVersion::V0;
        meta::write(other.root(), &snapshot).unwrap();
        other.add_all().unwrap();
        other
            .commit("initialize another legacy repository")
            .unwrap();
        let native = fixture.home.join(".claude/projects/synthetic");
        std::fs::create_dir_all(&native).unwrap();
        std::fs::write(
            native.join(format!("{CLAIM}.jsonl")),
            format!(
                "{}\n",
                serde_json::json!({
                    "type": "user", "sessionId": CLAIM,
                    "cwd": fixture.temporary.path(),
                    "message": {"role": "user", "content": "synthetic native evidence"}
                })
            ),
        )
        .unwrap();
        let mut claim = link::Link::new("claude-code", CLAIM, Some(fixture.temporary.path()));
        claim.owner = Some("local".into());
        claim.agent = Some("sensitive-review".into());
        claim.branch = Some("selected".into());
        link::write(&Store::at(fixture.home.join("store")), &claim).unwrap();
        assert!(!fixture.home.join("layout-v1.complete").exists());
        fixture
    }

    fn pending(&self, relative: &str) -> PathBuf {
        let directory = self.home.join("layout-v1-recovery");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(self.home.join("layout-v1.lock"), b"").unwrap();
        let path = directory.join(format!("pending-review-{}-0", std::process::id()));
        std::fs::write(&path, format!("{relative}\n")).unwrap();
        path
    }

    fn files(&self) -> BTreeMap<PathBuf, FileImage> {
        let mut images = BTreeMap::new();
        let mut pending = vec![self.home.clone()];
        while let Some(path) = pending.pop() {
            let metadata = std::fs::symlink_metadata(&path).unwrap();
            let image = if metadata.file_type().is_symlink() {
                FileImage::Symlink(std::fs::read_link(&path).unwrap())
            } else if metadata.is_dir() {
                for entry in std::fs::read_dir(&path).unwrap() {
                    pending.push(entry.unwrap().path());
                }
                FileImage::Directory
            } else {
                assert!(metadata.is_file(), "fixture contains a special file");
                FileImage::File(std::fs::read(&path).unwrap())
            };
            images.insert(path.strip_prefix(&self.home).unwrap().to_owned(), image);
        }
        images
    }
}

#[derive(Debug, PartialEq, Eq)]
enum FileImage {
    Directory,
    File(Vec<u8>),
    Symlink(PathBuf),
}

#[test]
fn a_changed_review_head_cannot_mutate_the_view_or_ref() {
    let fixture = Fixture::new();
    let reviewed = fixture.head();
    fixture.record("UNREVIEWED-SYNTHETIC-EVIDENCE", 2);
    let current = fixture.head();
    let pair = storage::materialize_pair_at(fixture.repo.root(), &current).unwrap();
    let files = fixture.repo.git(&["status", "--porcelain"]).unwrap();
    let output = fixture.run(&[
        "revert",
        "local/sensitive-review@selected#1.1",
        "--into",
        "local/sensitive-review@selected",
        "--expected-head",
        &reviewed,
    ]);
    assert_eq!(
        output.status.code(),
        Some(4),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.head(), current);
    assert_eq!(
        storage::materialize_pair_at(fixture.repo.root(), &current).unwrap(),
        pair
    );
    assert_eq!(fixture.repo.git(&["status", "--porcelain"]).unwrap(), files);
}

#[test]
fn a_matching_review_head_removes_only_the_view_occurrence_and_retains_evidence() {
    let fixture = Fixture::new();
    let reviewed = fixture.head();
    let original = storage::materialize_at(fixture.repo.root(), &reviewed, meta::LOG_FILE).unwrap();
    let output = fixture.run(&[
        "revert",
        "local/sensitive-review@selected#1.1",
        "--into",
        "local/sensitive-review@selected",
        "--expected-head",
        &reviewed,
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let (log, view) = storage::materialize_pair_at(fixture.repo.root(), &fixture.head()).unwrap();
    assert!(log.starts_with(&original));
    assert!(!view.contains("SELECTED-SYNTHETIC-EVIDENCE"));
    assert_ne!(fixture.head(), reviewed);
}

#[test]
fn an_invalid_guard_is_rejected_before_repository_lookup() {
    let fixture = Fixture::new();
    for expected in ["HEAD", "--all", "", "a\nsecret"] {
        let output = fixture.run(&[
            "revert",
            "missing/repo@branch#1.1",
            "--expected-head",
            expected,
        ]);
        assert_eq!(output.status.code(), Some(2));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("secret"));
    }
}

#[cfg(unix)]
fn fake_runtime(response: &serde_json::Value) -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let executable = directory.path().join("claude");
    std::fs::write(directory.path().join("response.json"), response.to_string()).unwrap();
    std::fs::write(&executable, r#"#!/bin/sh
if [ "$2" = "--help" ]; then
  printf '%s\n' '--print --safe-mode --settings --tools --disable-slash-commands --strict-mcp-config --mcp-config --no-chrome --no-session-persistence --permission-mode --permission-prompts --output-format --json-schema'
  exit 0
fi
[ -z "${AGIT_SESSION+x}" ] || exit 42
printf '%s\n' "$PWD" > "$CLAUDE_CONFIG_DIR/cwd"
printf '%s\n' "$AGIT_HOME" > "$CLAUDE_CONFIG_DIR/agit-home"
/bin/cat > "$CLAUDE_CONFIG_DIR/input"
/bin/cat "$CLAUDE_CONFIG_DIR/response.json"
"#).unwrap();
    std::fs::set_permissions(executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    directory
}

#[cfg(windows)]
fn fake_runtime(response: &serde_json::Value) -> tempfile::TempDir {
    static PROGRAM: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    let program = PROGRAM.get_or_init(|| {
        let build = tempfile::tempdir().unwrap();
        let executable = build.path().join("synthetic_sensitive_reviewer.exe");
        let output = Command::new("rustc")
            .args([
                "--edition=2024",
                "--crate-name",
                "synthetic_sensitive_reviewer",
                "-Dwarnings",
            ])
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sensitive_reviewer.rs"))
            .arg("-o")
            .arg(&executable)
            .current_dir(build.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "native fixture compilation failed: {output:?}"
        );
        std::fs::read(&executable).unwrap()
    });
    let directory = tempfile::Builder::new()
        .prefix("sensitive reviewer native ")
        .tempdir()
        .unwrap();
    std::fs::write(directory.path().join("claude.exe"), program).unwrap();
    std::fs::write(directory.path().join("response.json"), response.to_string()).unwrap();
    directory
}

#[cfg(any(unix, windows))]
fn scan_with_runtime(fixture: &Fixture, runtime: &tempfile::TempDir, extra: &[&str]) -> Output {
    let mut paths = vec![runtime.path().to_path_buf()];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    fixture
        .command(&[
            "scan",
            "local/sensitive-review@selected",
            "--sensitive",
            "--json",
        ])
        .args(extra)
        .env("PATH", std::env::join_paths(paths).unwrap())
        .env("CLAUDE_CONFIG_DIR", runtime.path())
        .env("AGIT_SESSION", "unrelated/repo@other")
        .output()
        .unwrap()
}

#[cfg(any(unix, windows))]
#[test]
fn sensitive_review_uses_only_committed_selected_view_and_returns_guarded_local_remedies() {
    let fixture = Fixture::new();
    let reviewed = fixture.head();
    fixture.repo.git(&["switch", "-c", "unselected"]).unwrap();
    fixture.record("UNSELECTED-SYNTHETIC-EVIDENCE", 2);
    fixture.repo.git(&["switch", "selected"]).unwrap();
    std::fs::write(
        fixture.repo.root().join("UNSETTLED.txt"),
        "UNCOMMITTED-SYNTHETIC-EVIDENCE",
    )
    .unwrap();
    let original = storage::materialize_pair_at(fixture.repo.root(), &reviewed).unwrap();
    let runtime = fake_runtime(&serde_json::json!({
        "type": "result", "subtype": "success", "is_error": false,
        "structured_output": {"assessments": [{"scope": 0, "locator": "@#1.1", "category": "sensitive-information"}]}
    }));
    let output = scan_with_runtime(&fixture, &runtime, &[]);
    assert_eq!(output.status.code(), Some(7), "{output:?}");
    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(envelope["schema_version"], 2);
    let report = &envelope["result"]["value"];
    assert_eq!(report["complete"], true);
    assert_eq!(report["scopes"][0]["events_reviewed"], 1);
    assert_eq!(report["findings"][0]["snapshot"], reviewed);
    assert_eq!(
        report["findings"][0]["remedies"][1]["argv"],
        serde_json::json!([
            "agit",
            "revert",
            "local/sensitive-review@selected#1.1",
            "--into",
            "local/sensitive-review@selected",
            "--expected-head",
            reviewed
        ])
    );
    let input = std::fs::read_to_string(runtime.path().join("input")).unwrap();
    assert!(input.contains("SELECTED-SYNTHETIC-EVIDENCE"));
    assert!(!input.contains("UNSELECTED-SYNTHETIC-EVIDENCE"));
    assert!(!input.contains("UNCOMMITTED-SYNTHETIC-EVIDENCE"));
    #[cfg(windows)]
    {
        let schema: serde_json::Value =
            serde_json::from_slice(&std::fs::read(runtime.path().join("schema")).unwrap()).unwrap();
        assert_eq!(schema["properties"]["assessments"]["type"], "array");
        assert_eq!(schema["additionalProperties"], false);
    }
    assert_eq!(fixture.head(), reviewed);
    assert_eq!(
        storage::materialize_pair_at(fixture.repo.root(), &reviewed).unwrap(),
        original
    );
    let child_home = std::fs::read_to_string(runtime.path().join("agit-home")).unwrap();
    assert_ne!(child_home.trim(), fixture.home.to_str().unwrap());
    assert!(!std::path::Path::new(child_home.trim()).exists());
    let legacy = scan_with_runtime(&fixture, &runtime, &["--json-version", "1"]);
    let envelope: serde_json::Value = serde_json::from_slice(&legacy.stdout).unwrap();
    assert_eq!(envelope["schema_version"], 1);
    assert_eq!(envelope["result"]["value"]["complete"], true);
}

/// Completing shallow history must not change the meaning of an already issued remedy.
#[cfg(any(unix, windows))]
#[test]
fn a_shallow_scan_refuses_before_review_and_unshallowing_keeps_the_guarded_locator_correct() {
    let fixture = Fixture::new();
    fixture.record("SECOND-SYNTHETIC-EVIDENCE", 2);
    let reviewed = fixture.head();
    let original = storage::materialize_pair_at(fixture.repo.root(), &reviewed).unwrap();
    let source = fixture.temporary.path().join("full-source");
    std::fs::rename(fixture.repo.root(), &source).unwrap();
    let clone = Command::new("git")
        .args(["clone", "--no-local", "--depth=1", "--branch", "selected"])
        .arg(&source)
        .arg(fixture.repo.root())
        .output()
        .unwrap();
    assert!(clone.status.success(), "{clone:?}");
    fixture
        .repo
        .git(&["config", "commit.gpgsign", "false"])
        .unwrap();
    assert_eq!(fixture.head(), reviewed);
    assert_eq!(
        fixture
            .repo
            .git(&["rev-parse", "--is-shallow-repository"])
            .unwrap(),
        "true"
    );
    let runtime = fake_runtime(&serde_json::json!({
        "type": "result", "subtype": "success", "is_error": false,
        "structured_output": {"assessments": [
            {"scope": 0, "locator": "@#1.1", "category": "none"},
            {"scope": 0, "locator": "@#2.1", "category": "sensitive-information"}
        ]}
    }));
    let before = fixture.files();
    let output = scan_with_runtime(&fixture, &runtime, &[]);
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(envelope["result"]["value"]["complete"], false);
    assert_eq!(
        envelope["result"]["value"]["findings"],
        serde_json::json!([])
    );
    assert!(!runtime.path().join("input").exists());
    assert!(!runtime.path().join("cwd").exists());
    assert_eq!(fixture.files(), before);

    fixture
        .repo
        .git(&["fetch", "--unshallow", "origin"])
        .unwrap();
    assert_eq!(fixture.head(), reviewed);
    assert_eq!(
        fixture
            .repo
            .git(&["rev-parse", "--is-shallow-repository"])
            .unwrap(),
        "false"
    );
    let output = scan_with_runtime(&fixture, &runtime, &[]);
    assert_eq!(output.status.code(), Some(7), "{output:?}");
    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let report = &envelope["result"]["value"];
    assert_eq!(report["complete"], true);
    assert_eq!(report["scopes"][0]["events_reviewed"], 2);
    assert_eq!(report["findings"][0]["snapshot"], reviewed);
    let argv = &report["findings"][0]["remedies"][1]["argv"];
    assert_eq!(
        argv,
        &serde_json::json!([
            "agit",
            "revert",
            "local/sensitive-review@selected#2.1",
            "--into",
            "local/sensitive-review@selected",
            "--expected-head",
            reviewed
        ])
    );
    let args: Vec<_> = argv
        .as_array()
        .unwrap()
        .iter()
        .skip(1)
        .map(|arg| arg.as_str().unwrap())
        .collect();
    let shallow = fixture.repo.git_path("shallow").unwrap();
    std::fs::write(&shallow, format!("{reviewed}\n")).unwrap();
    let before = fixture.files();
    let rejected = fixture.run(&args);
    assert_eq!(rejected.status.code(), Some(4), "{rejected:?}");
    assert_eq!(fixture.files(), before);
    assert_eq!(fixture.head(), reviewed);
    fixture
        .repo
        .git(&["fetch", "--unshallow", "origin"])
        .unwrap();
    let grafts = fixture.repo.git_path("info/grafts").unwrap();
    std::fs::create_dir_all(grafts.parent().unwrap()).unwrap();
    std::fs::write(&grafts, format!("{reviewed}\n")).unwrap();
    let before = fixture.files();
    let rejected = fixture.run(&args);
    assert_eq!(rejected.status.code(), Some(4), "{rejected:?}");
    assert_eq!(fixture.files(), before);
    std::fs::remove_file(grafts).unwrap();
    assert_eq!(fixture.head(), reviewed);
    let reverted = fixture.run(&args);
    assert!(reverted.status.success(), "{reverted:?}");
    assert_eq!(
        fixture
            .repo
            .git(&["show", "-s", "--format=%P", "HEAD"])
            .unwrap(),
        reviewed
    );
    let (log, view) = storage::materialize_pair_at(fixture.repo.root(), &fixture.head()).unwrap();
    assert!(log.starts_with(&original.0));
    assert!(view.contains("SELECTED-SYNTHETIC-EVIDENCE"));
    assert!(!view.contains("SECOND-SYNTHETIC-EVIDENCE"));
    assert_eq!(
        storage::materialize_pair_at(fixture.repo.root(), &reviewed).unwrap(),
        original
    );
}

#[test]
fn a_guarded_revert_checks_the_frozen_history_of_another_source_branch() {
    let fixture = Fixture::new();
    fixture.repo.git(&["switch", "-c", "source"]).unwrap();
    std::fs::write(fixture.repo.root().join("source-only.txt"), b"synthetic\n").unwrap();
    fixture.record("SECOND-SYNTHETIC-EVIDENCE", 2);
    let source = fixture.head();
    fixture.repo.git(&["switch", "selected"]).unwrap();
    fixture.record("SECOND-SYNTHETIC-EVIDENCE", 2);
    let reviewed = fixture.head();
    assert_ne!(source, reviewed);
    let args = [
        "revert",
        "local/sensitive-review@source#2.1",
        "--into",
        "local/sensitive-review@selected",
        "--expected-head",
        &reviewed,
    ];
    for relative in ["shallow", "info/grafts"] {
        let path = fixture.repo.git_path(relative).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("{source}\n")).unwrap();
        let before = fixture.files();
        let output = fixture.run(&args);
        assert_eq!(output.status.code(), Some(4), "{output:?}");
        assert_eq!(fixture.files(), before);
        std::fs::remove_file(path).unwrap();
    }
    let original = storage::materialize_pair_at(fixture.repo.root(), &reviewed).unwrap();
    let output = fixture.run(&args);
    assert!(output.status.success(), "{output:?}");
    let (log, view) = storage::materialize_pair_at(fixture.repo.root(), &fixture.head()).unwrap();
    assert!(log.starts_with(&original.0));
    assert!(view.contains("SELECTED-SYNTHETIC-EVIDENCE"));
    assert!(!view.contains("SECOND-SYNTHETIC-EVIDENCE"));
    assert_eq!(
        fixture
            .repo
            .git(&["rev-parse", "refs/heads/source"])
            .unwrap(),
        source
    );
}

#[cfg(any(unix, windows))]
#[test]
fn missing_or_hostile_model_coverage_is_incomplete_and_does_not_echo_payloads() {
    let fixture = Fixture::new();
    let original = fixture.head();
    for report in [
        serde_json::json!({"assessments": []}),
        serde_json::json!({"assessments": [{"scope": 0, "locator": "@#9.1", "category": "none"}]}),
        serde_json::json!({"assessments": [{"scope": 0, "locator": "@#1.1", "category": "none", "command": "PRIVATE-HOSTILE-COMMAND"}]}),
    ] {
        let runtime = fake_runtime(&serde_json::json!({
            "type": "result", "subtype": "success", "is_error": false, "structured_output": report,
        }));
        let output = scan_with_runtime(&fixture, &runtime, &[]);
        assert_eq!(output.status.code(), Some(4), "{output:?}");
        let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(envelope["result"]["value"]["complete"], false);
        assert_eq!(
            envelope["result"]["value"]["scopes"][0]["events_reviewed"],
            0
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains("PRIVATE-HOSTILE-COMMAND"));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("PRIVATE-HOSTILE-COMMAND"));
        assert_eq!(fixture.head(), original);
    }
}

#[cfg(any(unix, windows))]
#[test]
fn unsupported_runtime_never_falls_back_to_an_installed_reviewer() {
    let fixture = Fixture::new();
    let configured = fixture.run(&["config", "runtime.default", "codex"]);
    assert!(configured.status.success(), "{configured:?}");
    let runtime = fake_runtime(&serde_json::json!({
        "type": "result", "subtype": "success", "is_error": false,
        "structured_output": {"assessments": [{"scope": 0, "locator": "@#1.1", "category": "none"}]},
    }));
    let output = scan_with_runtime(&fixture, &runtime, &[]);
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(envelope["result"]["value"]["complete"], false);
    assert_eq!(
        envelope["result"]["value"]["scopes"][0]["events_reviewed"],
        0
    );
    assert!(!runtime.path().join("input").exists());
    assert!(!runtime.path().join("cwd").exists());
}

#[test]
fn a_missing_sensitive_ref_keeps_the_reference_exit_code() {
    let fixture = Fixture::new();
    let output = fixture.run(&[
        "scan",
        "local/sensitive-review@missing",
        "--sensitive",
        "--json",
    ]);
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(envelope["result"]["value"]["complete"], false);
}

#[cfg(any(unix, windows))]
#[test]
fn ambiguous_sensitive_refs_keep_candidates_without_review_or_storage_changes() {
    for layout in [meta::LayoutVersion::V0, meta::LayoutVersion::V1] {
        let fixture = Fixture::with_layout(layout);
        let reviewed = fixture.head();
        fixture
            .repo
            .git(&["tag", "selected", "refs/heads/selected"])
            .unwrap();
        let runtime = fake_runtime(&serde_json::json!({
            "type": "result", "subtype": "success", "is_error": false,
            "structured_output": {"assessments": [{"scope": 0, "locator": "@#1.1", "category": "none"}]}
        }));
        let mut paths = vec![runtime.path().to_path_buf()];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        let run = |target: &str, format: &[&str]| {
            fixture
                .command(&["scan", target, "--sensitive"])
                .args(format)
                .env("PATH", std::env::join_paths(&paths).unwrap())
                .env("CLAUDE_CONFIG_DIR", runtime.path())
                .output()
                .unwrap()
        };
        let before = fixture.files();
        for format in [
            vec![],
            vec!["--quiet"],
            vec!["--json", "--json-version", "1"],
            vec!["--json", "--json-version", "2"],
        ] {
            let output = run("local/sensitive-review@selected", &format);
            assert_eq!(output.status.code(), Some(8), "{output:?}");
            let diagnostic = if format.contains(&"--json") {
                assert!(output.stderr.is_empty(), "{output:?}");
                let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
                assert_eq!(envelope["exit_code"], 8);
                assert_eq!(envelope["ok"], false);
                assert_eq!(envelope["result"]["value"]["complete"], false);
                assert_eq!(envelope["result"]["value"]["scopes"], serde_json::json!([]));
                assert_eq!(
                    envelope["result"]["value"]["findings"],
                    serde_json::json!([])
                );
                if format.last() == Some(&"1") {
                    assert!(envelope.get("fix").is_none());
                } else {
                    assert_eq!(envelope["fix"], serde_json::json!([]));
                }
                envelope["diagnostics"]["stderr"].to_string()
            } else {
                assert!(output.stdout.is_empty(), "{output:?}");
                String::from_utf8(output.stderr).unwrap()
            };
            for candidate in ["ambiguous", "branch selected", "tag selected"] {
                assert!(diagnostic.contains(candidate), "{diagnostic}");
            }
            assert!(!diagnostic.contains("SELECTED-SYNTHETIC-EVIDENCE"));
            assert!(!runtime.path().join("input").exists());
            assert!(!runtime.path().join("cwd").exists());
            assert_eq!(fixture.files(), before, "reference refusal changed storage");
            assert_eq!(fixture.head(), reviewed);
        }
        let explicit = format!("local/sensitive-review@{reviewed}");
        let output = run(&explicit, &["--json"]);
        assert_eq!(output.status.code(), Some(0), "{output:?}");
        let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(envelope["result"]["value"]["complete"], true);
        assert_eq!(
            envelope["result"]["value"]["scopes"][0]["snapshot"],
            reviewed
        );
        assert!(runtime.path().join("input").exists());
        assert_eq!(fixture.files(), before, "explicit review changed storage");
    }
}

/// Unsupported inspection still reads legacy evidence without migrating any repository.
#[test]
fn a_v0_sensitive_refusal_preserves_all_files_without_a_completed_migration() {
    for incomplete_marker in [false, true] {
        let fixture = Fixture::legacy();
        std::fs::write(
            fixture.home.join("config.json"),
            r#"{"runtime.default":"codex"}"#,
        )
        .unwrap();
        if incomplete_marker {
            std::fs::write(fixture.home.join("layout-v1.complete"), b"incomplete\n").unwrap();
        }
        fixture.pending("local/other");
        let before = fixture.files();
        for format in [
            vec![],
            vec!["--json", "--json-version", "1"],
            vec!["--json"],
        ] {
            let output = fixture
                .command(&["scan", "local/sensitive-review@selected", "--sensitive"])
                .args(format)
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(4), "{output:?}");
            let diagnostic = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                diagnostic.contains("configured runtime does not provide"),
                "{output:?}"
            );
            assert_eq!(fixture.files(), before, "inspection changed legacy storage");
        }
    }
}

/// A review guard is checked against its original legacy head before any migration can move it.
#[test]
fn v0_stale_and_malformed_guards_preserve_native_claims_and_git_files() {
    let fixture = Fixture::legacy();
    std::fs::write(fixture.home.join("layout-v1.complete"), b"incomplete\n").unwrap();
    let stale = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    assert_ne!(fixture.head(), stale);
    let before = fixture.files();
    for (expected, code, diagnostic) in [
        (stale, 4, "target changed after it was reviewed"),
        ("HEAD", 2, "requires a full commit SHA"),
    ] {
        for format in [
            vec![],
            vec!["--json", "--json-version", "1"],
            vec!["--json"],
        ] {
            let output = fixture
                .command(&[
                    "revert",
                    "local/sensitive-review@selected#1.1",
                    "--into",
                    "local/sensitive-review@selected",
                    "--expected-head",
                    expected,
                ])
                .args(format)
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(code), "{output:?}");
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(text.contains(diagnostic), "{output:?}");
            assert_eq!(fixture.files(), before, "a refused guard changed storage");
        }
    }
}

/// A guarded edit cannot validate one repository and publish through another Git route.
#[test]
fn guarded_reverts_refuse_inherited_git_routing_before_touching_any_repository() {
    for legacy in [false, true] {
        let fixture = if legacy {
            Fixture::legacy()
        } else {
            Fixture::new()
        };
        let reviewed = fixture.head();
        let foreign_path = fixture.home.join("repos/local/foreign");
        fixture
            .repo
            .git(&[
                "clone",
                "--no-local",
                "--branch",
                "selected",
                fixture.repo.root().to_str().unwrap(),
                foreign_path.to_str().unwrap(),
            ])
            .unwrap();
        let foreign = Repo::open(&foreign_path).unwrap();
        assert_eq!(
            foreign.git(&["rev-parse", "refs/heads/selected"]).unwrap(),
            reviewed
        );
        let foreign_git = foreign_path.join(".git").display().to_string();
        let foreign_work = foreign_path.display().to_string();
        let before = fixture.files();
        let args = [
            "revert",
            "local/sensitive-review@selected#1.1",
            "--into",
            "local/sensitive-review@selected",
            "--expected-head",
            &reviewed,
        ];
        for (key, value) in [
            ("GIT_DIR", foreign_git.as_str()),
            ("GIT_DIR", ""),
            ("GIT_WORK_TREE", foreign_work.as_str()),
            ("GIT_COMMON_DIR", foreign_git.as_str()),
            ("GIT_INDEX_FILE", "synthetic-index"),
            ("GIT_OBJECT_DIRECTORY", "synthetic-objects"),
            ("GIT_ALTERNATE_OBJECT_DIRECTORIES", "synthetic-alternates"),
            ("GIT_NAMESPACE", "synthetic-namespace"),
            ("GIT_CONFIG_COUNT", "1"),
            (
                "GIT_CONFIG_PARAMETERS",
                "'core.worktree=synthetic-worktree'",
            ),
            ("GIT_REPLACE_REF_BASE", "refs/synthetic-replace/"),
            ("GIT_SHALLOW_FILE", "synthetic-shallow"),
            ("GIT_GRAFT_FILE", "synthetic-grafts"),
            ("GIT_PREFIX", "synthetic-prefix/"),
        ] {
            for format in [
                vec![],
                vec!["--quiet"],
                vec!["--json", "--json-version", "1"],
                vec!["--json"],
            ] {
                let json = format.contains(&"--json");
                let mut command = fixture.command(&args);
                command.args(format).env(key, value);
                if key == "GIT_CONFIG_COUNT" {
                    command
                        .env("GIT_CONFIG_KEY_0", "core.worktree")
                        .env("GIT_CONFIG_VALUE_0", &foreign_path);
                }
                let output = command.output().unwrap();
                assert_eq!(output.status.code(), Some(4), "{key}: {output:?}");
                let diagnostic = if json {
                    assert!(output.stderr.is_empty(), "{key}: {output:?}");
                    let envelope: serde_json::Value =
                        serde_json::from_slice(&output.stdout).unwrap();
                    assert_eq!(envelope["exit_code"], 4);
                    assert_eq!(envelope["ok"], false);
                    envelope.to_string()
                } else {
                    assert!(output.stdout.is_empty(), "{key}: {output:?}");
                    String::from_utf8(output.stderr).unwrap()
                };
                assert!(diagnostic.contains(&format!("unset {key}")), "{diagnostic}");
                assert!(!diagnostic.contains(&foreign_git), "{diagnostic}");
                assert_eq!(fixture.files(), before, "{key}: rejection changed files");
            }
        }

        let output = fixture.run(&args);
        assert!(output.status.success(), "{output:?}");
        let landed = fixture.head();
        assert_ne!(landed, reviewed);
        assert_eq!(
            fixture
                .repo
                .git(&["show", "-s", "--format=%P", &landed])
                .unwrap(),
            reviewed
        );
        let (_, view) = storage::materialize_pair_at(fixture.repo.root(), &landed).unwrap();
        assert!(!view.contains("SELECTED-SYNTHETIC-EVIDENCE"));
        assert_eq!(
            foreign.git(&["rev-parse", "refs/heads/selected"]).unwrap(),
            reviewed
        );
        let protected = |files: BTreeMap<PathBuf, FileImage>| {
            files
                .into_iter()
                .filter(|(path, _)| !path.starts_with(Path::new("repos").join(SLUG)))
                .collect::<BTreeMap<_, _>>()
        };
        let mut expected = protected(before);
        assert_eq!(
            expected.insert(PathBuf::from("layout-v1.lock"), FileImage::File(Vec::new())),
            None
        );
        assert_eq!(
            expected.insert(PathBuf::from("layout-v1-recovery"), FileImage::Directory),
            None
        );
        assert_eq!(protected(fixture.files()), expected);
    }
}

/// Recovery in the selected scope cannot be consumed by inspection or a guarded edit.
#[test]
fn v0_review_refuses_selected_or_unknown_recovery_without_consuming_it() {
    for recovery in [SLUG, "local/missing", "../outside", "checkout"] {
        let fixture = Fixture::legacy();
        let head = fixture.head();
        if recovery == "checkout" {
            std::fs::write(
                fixture
                    .repo
                    .git_path("agit-checkout-transaction.json")
                    .unwrap(),
                b"pending synthetic checkout",
            )
            .unwrap();
        } else {
            fixture.pending(recovery);
        }
        let before = fixture.files();
        for args in [
            vec!["scan", "local/sensitive-review@selected", "--sensitive"],
            vec![
                "revert",
                "local/sensitive-review@selected#1.1",
                "--expected-head",
                &head,
            ],
        ] {
            let output = fixture.run(&args);
            assert_eq!(output.status.code(), Some(4), "{recovery}: {output:?}");
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("settled local storage"),
                "{output:?}"
            );
            assert_eq!(fixture.files(), before, "recovery evidence was changed");
        }
    }
}

/// A matching guard does not authorize replacing the user's prospective V1 namespace.
#[test]
fn v0_guarded_revert_refuses_user_namespace_collisions_without_writes() {
    for mode in ["untracked", "tracked", "ignored"] {
        let fixture = Fixture::legacy();
        std::fs::write(
            fixture.repo.root().join(meta::LOG_FILE),
            b"user-owned data\n",
        )
        .unwrap();
        if mode == "tracked" {
            let mut snapshot = meta::read(fixture.repo.root()).unwrap();
            snapshot.kind = meta::Kind::File;
            snapshot.turn = None;
            meta::write(fixture.repo.root(), &snapshot).unwrap();
            fixture.repo.add_all().unwrap();
            fixture
                .repo
                .commit("record a user-owned root file")
                .unwrap();
        } else if mode == "ignored" {
            std::fs::write(fixture.repo.git_path("info/exclude").unwrap(), b"/LOG\n").unwrap();
        }
        let head = fixture.head();
        let before = fixture.files();
        let output = fixture.run(&[
            "revert",
            "local/sensitive-review@selected#1.1",
            "--expected-head",
            &head,
        ]);
        assert!(!output.status.success(), "{mode}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("cannot upgrade"),
            "{mode}: {output:?}"
        );
        assert_eq!(
            fixture.files(),
            before,
            "a namespace refusal changed storage"
        );
    }
}

/// Only the reviewed target crosses layouts; another scope's recovery remains untouched.
#[test]
fn v0_matching_guard_lands_a_direct_view_child_without_global_migration() {
    let fixture = Fixture::legacy();
    fixture.pending("local/other");
    std::fs::write(fixture.home.join("layout-v1.complete"), b"incomplete\n").unwrap();
    let reviewed = fixture.head();
    let original = storage::materialize_pair_at(fixture.repo.root(), &reviewed).unwrap();
    let refs = || -> BTreeMap<String, String> {
        fixture
            .repo
            .git(&["for-each-ref", "--format=%(refname) %(objectname)"])
            .unwrap()
            .lines()
            .map(|line| {
                let (name, oid) = line.split_once(' ').unwrap();
                (name.to_owned(), oid.to_owned())
            })
            .collect()
    };
    let mut expected_refs = refs();
    let protected = || {
        fixture
            .files()
            .into_iter()
            .filter(|(path, _)| !path.starts_with(Path::new("repos").join(SLUG)))
            .collect::<BTreeMap<_, _>>()
    };
    let before = protected();
    let output = fixture.run(&[
        "revert",
        "local/sensitive-review@selected#1.1",
        "--expected-head",
        &reviewed,
    ]);
    assert!(output.status.success(), "{output:?}");
    let landed = fixture.head();
    assert_ne!(landed, reviewed);
    assert_eq!(
        fixture
            .repo
            .git(&["show", "-s", "--format=%P", &landed])
            .unwrap(),
        reviewed
    );
    expected_refs.insert("refs/heads/selected".into(), landed.clone());
    assert_eq!(refs(), expected_refs);
    let snapshot = meta::read_at_ref(&fixture.repo, &landed).unwrap();
    assert_eq!(snapshot.kind, meta::Kind::View);
    assert_eq!(snapshot.layout, meta::LayoutVersion::V1);
    assert_eq!(
        meta::read_at_ref(&fixture.repo, "refs/heads/unselected")
            .unwrap()
            .layout,
        meta::LayoutVersion::V0
    );
    let (log, view) = storage::materialize_pair_at(fixture.repo.root(), &landed).unwrap();
    assert!(log.starts_with(&original.0));
    assert!(!view.contains("SELECTED-SYNTHETIC-EVIDENCE"));
    assert_eq!(
        storage::materialize_pair_at(fixture.repo.root(), &reviewed).unwrap(),
        original
    );
    assert_eq!(
        protected(),
        before,
        "another scope or startup marker changed"
    );
    assert!(!fixture.repo.root().join(meta::LEGACY_LOG_FILE).exists());
    assert!(fixture.repo.root().join(meta::LOG_FILE).is_file());
}

/// A successful native review is also an inspection when its selected snapshot uses V0.
#[cfg(any(unix, windows))]
#[test]
fn a_controlled_v0_sensitive_review_preserves_the_complete_local_file_inventory() {
    let fixture = Fixture::legacy();
    fixture.pending("local/other");
    let reviewed = fixture.head();
    let before = fixture.files();
    let runtime = fake_runtime(&serde_json::json!({
        "type": "result", "subtype": "success", "is_error": false,
        "structured_output": {"assessments": [{"scope": 0, "locator": "@#1.1", "category": "none"}]}
    }));
    let output = scan_with_runtime(&fixture, &runtime, &[]);
    assert!(output.status.success(), "{output:?}");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["result"]["value"]["complete"], true);
    assert_eq!(report["result"]["value"]["scopes"][0]["snapshot"], reviewed);
    assert_eq!(report["result"]["value"]["scopes"][0]["events_reviewed"], 1);
    assert!(
        std::fs::read_to_string(runtime.path().join("input"))
            .unwrap()
            .contains("SELECTED-SYNTHETIC-EVIDENCE")
    );
    assert_eq!(fixture.files(), before);
}
