use agit::domain::{link, meta, repo::Repo, storage, store::Store, transcript};
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const SID: &str = "00000000-0000-4000-8000-000000000001";

struct Lab {
    temp: tempfile::TempDir,
    repo: Repo,
    native: PathBuf,
    claim: link::Link,
    prefix: String,
}

fn message(role: &str, text: &str) -> String {
    format!(
        "{}\n",
        json!({"type":"response_item","payload":{
            "type":"message","role":role,"content":[{"type":"input_text","text":text}]
        }})
    )
}

fn isolated(program: impl AsRef<std::ffi::OsStr>, root: &Path) -> Command {
    let mut command = Command::new(program);
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
        .env("HOME", root)
        .env("USERPROFILE", root)
        .env("AGIT_HOME", root.join("agit"))
        .env("CODEX_HOME", root.join("codex"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("AGIT_HUB_URL", "http://127.0.0.1:1")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", root.join("empty-config"))
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .env("NO_COLOR", "1")
        .env("AGIT_TUI", "0")
        .current_dir(root);
    command
}

fn success(output: Output) -> String {
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

impl Lab {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::write(root.join("empty-config"), "").unwrap();
        fs::create_dir(root.join("empty-hooks")).unwrap();
        let repo = Repo::at(root.join("agit/repos/alice/notes"));
        fs::create_dir_all(repo.root()).unwrap();
        let mut init = isolated("git", root);
        success(
            init.args(["-C", repo.root().to_str().unwrap(), "init", "-b", "main"])
                .output()
                .unwrap(),
        );
        let native = root
            .join("codex/sessions/2026/09/09")
            .join(format!("rollout-2026-09-09T00-00-00-{SID}.jsonl"));
        fs::create_dir_all(native.parent().unwrap()).unwrap();
        let prefix = format!(
            "{}\n{}{}",
            json!({"type":"session_meta","payload":{"id":SID}}),
            message("user", "settled question"),
            message("assistant", "settled answer")
        );
        fs::write(
            &native,
            format!(
                "{prefix}{}{}",
                message("user", "pending question"),
                message("assistant", "pending answer")
            ),
        )
        .unwrap();
        let mut claim = link::Link::new("codex", SID, None);
        claim.owner = Some("alice".into());
        claim.agent = Some("notes".into());
        claim.branch = Some("topic".into());
        link::write(&Store::at(root.join("agit/store")), &claim).unwrap();
        let lab = Self {
            temp,
            repo,
            native,
            claim,
            prefix,
        };
        lab.git(&[
            "config",
            "core.hooksPath",
            lab.temp.path().join("empty-hooks").to_str().unwrap(),
        ]);
        lab.git(&["config", "commit.gpgsign", "false"]);
        meta::write(lab.repo.root(), &meta::Meta::new_file_line()).unwrap();
        fs::write(lab.repo.root().join("AGENTS.md"), "shared baseline\n").unwrap();
        lab.commit("initial shared state");
        lab.git(&["checkout", "-b", "topic"]);
        let session = format!("agit-{}", "a".repeat(40));
        let log = transcript::wrap_lines(&lab.prefix, "codex", &session);
        storage::write_snapshot(lab.repo.root(), &log, &log).unwrap();
        let mut snapshot = meta::Meta::new(session, "codex".into(), "/fixture".into());
        snapshot.turn = Some(1);
        meta::write(lab.repo.root(), &snapshot).unwrap();
        lab.commit("settled native prefix");
        lab.git(&["checkout", "-b", "other", "main"]);
        fs::write(lab.repo.root().join("AGENTS.md"), "shared pending change\n").unwrap();
        lab
    }

    fn git(&self, args: &[&str]) -> String {
        let mut command = isolated("git", self.temp.path());
        success(
            command
                .arg("-C")
                .arg(self.repo.root())
                .args(args)
                .output()
                .unwrap(),
        )
    }

    fn commit(&self, message: &str) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-m", message]);
    }

    fn command(&self) -> Command {
        let mut command = isolated(env!("CARGO_BIN_EXE_agit"), self.temp.path());
        command.env("AGIT_SESSION", "alice/notes@topic");
        command
    }

    fn save_claim(&self, claim: &link::Link) {
        link::write(&Store::at(self.temp.path().join("agit/store")), claim).unwrap();
    }
}

fn inventory(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
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
fn omitted_range_uses_the_selected_branch_native_evidence_and_preserves_the_working_patch() {
    let lab = Lab::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let hub = format!("http://{}", listener.local_addr().unwrap());
    let before = inventory(lab.temp.path());
    let output = lab
        .command()
        .env("AGIT_HUB_URL", hub)
        .arg("diff")
        .output()
        .unwrap();
    let text = success(output);
    assert!(text.contains("+shared pending change"), "{text}");
    assert!(
        text.contains("+shared pending change\npending alice/notes@topic:"),
        "{text}"
    );
    assert!(text.contains("pending alice/notes@topic: 2 events, 1 user turns with pending activity (1 newly started), 0 ToolUse calls"), "{text}");
    assert_eq!(
        inventory(lab.temp.path()),
        before,
        "inspection changed local state"
    );
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[test]
fn matching_native_snapshot_is_distinct_from_missing_unreadable_and_ambiguous_evidence() {
    let lab = Lab::new();
    fs::write(&lab.native, &lab.prefix).unwrap();
    assert!(
        success(lab.command().arg("diff").output().unwrap())
            .contains("no unsettled content in the verified native snapshot")
    );
    for state in ["truncated", "malformed", "foreign", "absent"] {
        match state {
            "truncated" => fs::write(&lab.native, "{}\n").unwrap(),
            "malformed" => fs::write(&lab.native, format!("{}malformed\n", lab.prefix)).unwrap(),
            "foreign" => fs::write(
                &lab.native,
                lab.prefix
                    .replace(SID, "00000000-0000-4000-8000-000000000002"),
            )
            .unwrap(),
            _ => fs::remove_file(&lab.native).unwrap(),
        }
        let before = inventory(lab.temp.path());
        let output = lab.command().arg("diff").output().unwrap();
        assert_eq!(output.status.code(), Some(4), "{state}: {output:?}");
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(text.contains("unavailable"), "{state}: {text}");
        assert!(!text.contains("no unsettled"), "{state}: {text}");
        assert_eq!(inventory(lab.temp.path()), before);
    }
    fs::write(&lab.native, &lab.prefix).unwrap();
    let mut duplicate = lab.claim.clone();
    duplicate.session_id = "00000000-0000-4000-8000-000000000002".into();
    lab.save_claim(&duplicate);
    let output = lab.command().arg("diff").output().unwrap();
    assert_eq!(output.status.code(), Some(4));
    assert!(String::from_utf8_lossy(&output.stdout).contains("exactly one"));
}

#[test]
fn safe_native_carrier_reads_keep_the_unfinished_tail_visible() {
    let lab = Lab::new();
    fs::write(
        &lab.native,
        format!(
            "{}{}{{\"type\":",
            lab.prefix,
            message("user", "pending prompt")
        ),
    )
    .unwrap();
    let before = inventory(lab.temp.path());
    let text = success(lab.command().arg("diff").output().unwrap());
    assert!(
        text.contains("1 events, 1 user turns with pending activity (1 newly started)"),
        "{text}"
    );
    assert!(
        text.contains("an incomplete trailing record is still being written"),
        "{text}"
    );
    assert!(!text.contains("no unsettled content"), "{text}");
    assert_eq!(inventory(lab.temp.path()), before);
}

#[test]
fn codex_inspection_does_not_open_the_native_wal_index_or_pick_duplicate_rollouts() {
    let lab = Lab::new();
    let database = lab.temp.path().join("codex/state_5.sqlite");
    let connection = rusqlite::Connection::open(&database).unwrap();
    connection
        .pragma_update(None, "journal_mode", "WAL")
        .unwrap();
    connection
        .execute("CREATE TABLE threads(id TEXT)", [])
        .unwrap();
    drop(connection);
    let before = inventory(lab.temp.path());
    success(lab.command().arg("diff").output().unwrap());
    assert_eq!(inventory(lab.temp.path()), before);
    fs::copy(
        &lab.native,
        lab.native
            .with_file_name(format!("rollout-2026-09-09T01-00-00-{SID}.jsonl")),
    )
    .unwrap();
    let output = lab.command().arg("diff").output().unwrap();
    assert_eq!(output.status.code(), Some(4));
    assert!(String::from_utf8_lossy(&output.stdout).contains("multiple native transcript"));
}

#[test]
fn materialized_view_counts_only_appended_content_and_requires_its_original_branch_tip() {
    use sha2::{Digest, Sha256};
    let lab = Lab::new();
    lab.git(&["checkout", "topic"]);
    let mut snapshot = meta::read_at_ref_result(&lab.repo, "topic")
        .unwrap()
        .unwrap();
    let discarded = format!(
        "{}{}",
        message("user", "removed from view"),
        message("assistant", "archive only")
    );
    let log = transcript::wrap_lines(
        &format!("{}{discarded}", lab.prefix),
        "codex",
        &snapshot.session,
    );
    let view = transcript::wrap_lines(&lab.prefix, "codex", &snapshot.session);
    storage::write_snapshot(lab.repo.root(), &log, &view).unwrap();
    snapshot.turn = Some(2);
    meta::write(lab.repo.root(), &snapshot).unwrap();
    lab.commit("projection fixture");
    let tip = lab.git(&["rev-parse", "topic"]).trim().to_owned();
    let mut claim = lab.claim.clone();
    claim.materialized_from = Some(tip);
    claim.baseline_bytes = Some(lab.prefix.len() as u64);
    claim.baseline_hash = Some(hex::encode(Sha256::digest(lab.prefix.as_bytes())));
    lab.save_claim(&claim);
    let before = inventory(lab.temp.path());
    let result = success(lab.command().arg("diff").output().unwrap());
    assert!(result.contains("pending alice/notes@topic: 2 events, 1 user turns with pending activity (1 newly started), 0 ToolUse calls"), "{result}");
    assert_eq!(inventory(lab.temp.path()), before);
    lab.git(&[
        "commit",
        "--allow-empty",
        "-m",
        "branch advanced independently",
    ]);
    let before = inventory(lab.temp.path());
    let output = lab.command().arg("diff").output().unwrap();
    assert_eq!(output.status.code(), Some(4));
    assert!(String::from_utf8_lossy(&output.stdout).contains("selected branch has advanced"));
    assert_eq!(inventory(lab.temp.path()), before);
}

#[test]
fn pending_inspection_requires_explicit_context_and_an_available_native_source() {
    let lab = Lab::new();
    let before = inventory(lab.temp.path());
    let output = lab
        .command()
        .env_remove("AGIT_SESSION")
        .current_dir(lab.repo.root())
        .arg("diff")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3));
    assert_eq!(inventory(lab.temp.path()), before);
    fs::remove_file(
        lab.temp
            .path()
            .join("agit/store/codex")
            .join(format!("{SID}.json")),
    )
    .unwrap();
    let mut claim = lab.claim.clone();
    claim.source = "opencode".into();
    lab.save_claim(&claim);
    let before = inventory(lab.temp.path());
    let output = lab.command().arg("diff").output().unwrap();
    assert_eq!(output.status.code(), Some(4));
    assert!(String::from_utf8_lossy(&output.stdout).contains("unavailable"));
    assert_eq!(inventory(lab.temp.path()), before);
}

#[cfg(unix)]
#[test]
fn git_callbacks_cannot_run_during_pending_inspection() {
    let lab = Lab::new();
    fs::write(
        lab.repo.root().join(".git/callback.sh"),
        "#!/bin/sh\nprintf called > \"$CALLBACK_MARKER\"\ncat\n",
    )
    .unwrap();
    let marker = lab.temp.path().join("callback-ran");
    lab.git(&["config", "core.fsmonitor", "sh .git/callback.sh"]);
    let before = inventory(lab.temp.path());
    success(
        lab.command()
            .env("CALLBACK_MARKER", &marker)
            .arg("diff")
            .output()
            .unwrap(),
    );
    assert_eq!(inventory(lab.temp.path()), before);
    assert!(!marker.exists());
    lab.git(&["config", "filter.canary.clean", "sh .git/callback.sh"]);
    fs::write(
        lab.repo.root().join(".gitattributes"),
        "AGENTS.md filter=canary\n",
    )
    .unwrap();
    let before = inventory(lab.temp.path());
    let output = lab
        .command()
        .env("CALLBACK_MARKER", &marker)
        .arg("diff")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(4));
    assert!(String::from_utf8_lossy(&output.stderr).contains("clean/process filters"));
    assert!(!marker.exists());
    assert_eq!(inventory(lab.temp.path()), before);
}

#[test]
fn partial_clone_configuration_is_refused_before_missing_objects_can_be_fetched() {
    let lab = Lab::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let remote = format!("http://{}/objects.git", listener.local_addr().unwrap());
    lab.git(&["config", "remote.origin.url", &remote]);
    lab.git(&["config", "remote.origin.promisor", "true"]);
    let object = lab.git(&["rev-parse", "topic:LOG"]).trim().to_owned();
    fs::remove_file(
        lab.repo
            .root()
            .join(".git/objects")
            .join(&object[..2])
            .join(&object[2..]),
    )
    .unwrap();
    let before = inventory(lab.temp.path());
    let output = lab.command().arg("diff").output().unwrap();
    assert_eq!(output.status.code(), Some(4));
    assert!(String::from_utf8_lossy(&output.stderr).contains("local objects"));
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert_eq!(inventory(lab.temp.path()), before);
}

#[test]
fn native_file_discovery_refuses_duplicate_claude_and_cursor_identities() {
    for runtime in ["claude-code", "claude-desktop", "cursor"] {
        let lab = Lab::new();
        lab.git(&["checkout", "topic"]);
        let mut snapshot = meta::read_at_ref_result(&lab.repo, "topic")
            .unwrap()
            .unwrap();
        snapshot.runtime = runtime.into();
        meta::write(lab.repo.root(), &snapshot).unwrap();
        let native = if runtime == "cursor" {
            format!(
                "{}\n",
                json!({"role":"user","content":[{"type":"text","text":"settled question"}]})
            )
        } else {
            format!(
                "{}\n",
                json!({"type":"user","sessionId":SID,"message":{"role":"user","content":"settled question"}})
            )
        };
        let log = transcript::wrap_lines(&native, runtime, &snapshot.session);
        storage::write_snapshot(lab.repo.root(), &log, &log).unwrap();
        lab.commit("native fixture history");
        let mut claim = lab.claim.clone();
        claim.source = runtime.into();
        claim.cwd = Some("/missing-project".into());
        fs::remove_file(
            lab.temp
                .path()
                .join("agit/store/codex")
                .join(format!("{SID}.json")),
        )
        .unwrap();
        lab.save_claim(&claim);
        for project in ["first", "second"] {
            let path = if runtime == "cursor" {
                lab.temp
                    .path()
                    .join(".cursor/projects")
                    .join(project)
                    .join("agent-transcripts")
                    .join(SID)
                    .join(format!("{SID}.jsonl"))
            } else {
                lab.temp
                    .path()
                    .join(".claude/projects")
                    .join(project)
                    .join(format!("{SID}.jsonl"))
            };
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, &native).unwrap();
        }
        let before = inventory(lab.temp.path());
        let output = lab.command().arg("diff").output().unwrap();
        assert_eq!(output.status.code(), Some(4), "{runtime}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("multiple native transcript"),
            "{runtime}: {output:?}"
        );
        assert_eq!(inventory(lab.temp.path()), before);
    }
}

#[cfg(unix)]
#[test]
fn symlinked_native_carriers_are_not_hidden_by_a_stale_regular_copy() {
    let lab = Lab::new();
    fs::write(&lab.native, &lab.prefix).unwrap();
    let active = lab.temp.path().join("active-transcript.jsonl");
    fs::write(
        &active,
        format!("{}{}", lab.prefix, message("user", "unsaved")),
    )
    .unwrap();
    let linked = lab
        .native
        .with_file_name(format!("rollout-2026-09-09T01-00-00-{SID}.jsonl"));
    std::os::unix::fs::symlink(active, &linked).unwrap();
    let before = inventory(lab.temp.path());
    let output = lab.command().arg("diff").output().unwrap();
    assert_eq!(output.status.code(), Some(4));
    assert!(String::from_utf8_lossy(&output.stdout).contains("symlink"));
    assert_eq!(inventory(lab.temp.path()), before);
    assert!(
        fs::symlink_metadata(linked)
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[cfg(unix)]
#[test]
fn linked_checkout_configuration_is_checked_before_legacy_recovery_reads() {
    let lab = Lab::new();
    let linked = lab.temp.path().join("linked-checkout");
    lab.git(&[
        "worktree",
        "add",
        "-b",
        "linked",
        linked.to_str().unwrap(),
        "main",
    ]);
    lab.git(&["config", "extensions.worktreeConfig", "true"]);
    let git = |args: &[&str]| {
        success(
            isolated("git", lab.temp.path())
                .arg("-C")
                .arg(&linked)
                .args(args)
                .output()
                .unwrap(),
        )
    };
    let mut snapshot = meta::Meta::new_file_line();
    snapshot.layout = meta::LayoutVersion::V0;
    meta::write(&linked, &snapshot).unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-m", "legacy fixture"]);
    let old = git(&["rev-parse", "HEAD"]).trim().to_owned();
    snapshot.layout = meta::LayoutVersion::V1;
    meta::write(&linked, &snapshot).unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-m", meta::STORAGE_MIGRATION_MESSAGE]);
    git(&["update-ref", "refs/agit/layout-v0/linked", &old]);
    fs::write(
        linked.join("callback.sh"),
        "#!/bin/sh\nprintf called > \"$CALLBACK_MARKER\"\ncat\n",
    )
    .unwrap();
    git(&[
        "config",
        "--worktree",
        "filter.canary.clean",
        "sh callback.sh",
    ]);
    fs::write(
        linked.join(".gitattributes"),
        "session/meta.json filter=canary\n",
    )
    .unwrap();
    fs::write(
        linked.join(meta::FILE),
        format!("{}\n", fs::read_to_string(linked.join(meta::FILE)).unwrap()),
    )
    .unwrap();
    let marker = lab.temp.path().join("linked-callback-ran");
    let before = inventory(lab.temp.path());
    let output = lab
        .command()
        .env("CALLBACK_MARKER", &marker)
        .arg("diff")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(4));
    assert!(String::from_utf8_lossy(&output.stderr).contains("clean/process filters"));
    assert!(!marker.exists());
    assert_eq!(inventory(lab.temp.path()), before);
}

#[cfg(unix)]
#[test]
fn nested_gitlink_filters_cannot_escape_readonly_preflight() {
    let lab = Lab::new();
    let nested = lab.repo.root().join("memory/nested");
    fs::create_dir_all(&nested).unwrap();
    let git = |args: &[&str]| {
        success(
            isolated("git", lab.temp.path())
                .arg("-C")
                .arg(&nested)
                .args(args)
                .output()
                .unwrap(),
        )
    };
    git(&["init", "-b", "main"]);
    git(&[
        "config",
        "core.hooksPath",
        lab.temp.path().join("empty-hooks").to_str().unwrap(),
    ]);
    git(&["config", "commit.gpgsign", "false"]);
    fs::write(nested.join("content.txt"), "shared submodule baseline\n").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-m", "submodule fixture"]);
    let tip = git(&["rev-parse", "HEAD"]).trim().to_owned();
    lab.git(&[
        "update-index",
        "--add",
        "--cacheinfo",
        &format!("160000,{tip},memory/nested"),
    ]);
    fs::write(
        nested.join(".git/callback.sh"),
        "#!/bin/sh\nprintf called > \"$CALLBACK_MARKER\"\ncat\n",
    )
    .unwrap();
    git(&["config", "filter.canary.clean", "sh .git/callback.sh"]);
    fs::write(nested.join(".gitattributes"), "content.txt filter=canary\n").unwrap();
    fs::write(nested.join("content.txt"), "pending submodule change\n").unwrap();
    let marker = lab.temp.path().join("submodule-callback-ran");
    let before = inventory(lab.temp.path());
    let output = lab
        .command()
        .env("CALLBACK_MARKER", &marker)
        .arg("diff")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(4));
    assert!(String::from_utf8_lossy(&output.stderr).contains("clean/process filters"));
    assert!(!marker.exists());
    assert_eq!(inventory(lab.temp.path()), before);
}

#[test]
fn git_environment_cannot_redirect_pending_evidence_or_the_working_index() {
    let lab = Lab::new();
    for name in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_NAMESPACE",
        "GIT_SHALLOW_FILE",
    ] {
        let before = inventory(lab.temp.path());
        let output = lab
            .command()
            .env(name, lab.repo.root())
            .arg("diff")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(4), "{name}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("repository redirection"),
            "{name}: {output:?}"
        );
        assert_eq!(inventory(lab.temp.path()), before);
    }
}

#[test]
fn both_json_envelope_versions_keep_the_existing_text_result_contract() {
    let lab = Lab::new();
    for version in ["1", "2"] {
        let before = inventory(lab.temp.path());
        let output = lab
            .command()
            .args(["--json", "--json-version", version, "diff"])
            .output()
            .unwrap();
        let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(document["schema_version"], version.parse::<u32>().unwrap());
        assert_eq!(document["result"]["format"], "text");
        assert_eq!(document["exit_code"], 0);
        assert!(
            document["result"]["lines"]
                .as_array()
                .unwrap()
                .iter()
                .any(|line| line
                    .as_str()
                    .is_some_and(|line| line.contains("pending alice/notes@topic: 2 events")))
        );
        assert_eq!(inventory(lab.temp.path()), before);
    }
}
