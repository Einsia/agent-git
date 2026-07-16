//! v3 end-to-end: dual-repo model + scope routing + WorkspaceRevision pairing + secret defense for session dumps.
//! The fact/evidence approach has been deprecated and removed.

use std::path::{Path, PathBuf};
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_agit");

struct Repo {
    dir: tempfile::TempDir,
}

impl Repo {
    fn new() -> Repo {
        let dir = tempfile::tempdir().unwrap();
        let r = Repo { dir };
        r.sh("git init -q -b main .");
        r.sh("git config user.name dev && git config user.email d@x.com");
        r.sh("git config commit.gpgsign false");
        r.write("app.ts", "export const x = 1;\n");
        r.sh("git add -A && git commit -qm seed");
        assert_eq!(r.agit(&["init"]).0, 0, "init should succeed");
        r
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }
    fn agent(&self) -> PathBuf {
        self.path().join(".agit/agent")
    }
    fn sh(&self, cmd: &str) -> String {
        let o = Command::new("sh").arg("-c").arg(cmd).current_dir(self.path()).output().unwrap();
        String::from_utf8_lossy(&o.stdout).to_string()
    }
    fn write(&self, rel: &str, content: &str) {
        let p = self.path().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }
    fn agit(&self, args: &[&str]) -> (i32, String, String) {
        let o = Command::new(BIN).args(args).current_dir(self.path()).output().unwrap();
        (
            o.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&o.stdout).to_string(),
            String::from_utf8_lossy(&o.stderr).to_string(),
        )
    }
    fn agit_env(&self, envs: &[(&str, &str)], args: &[&str]) -> (i32, String, String) {
        let mut c = Command::new(BIN);
        c.args(args).current_dir(self.path());
        for (k, v) in envs {
            c.env(k, v);
        }
        let o = c.output().unwrap();
        (
            o.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&o.stdout).to_string(),
            String::from_utf8_lossy(&o.stderr).to_string(),
        )
    }
    fn git_env(&self, args: &[&str]) -> String {
        let mut a = vec!["-C", self.path().to_str().unwrap()];
        a.extend_from_slice(args);
        String::from_utf8_lossy(&Command::new("git").args(&a).output().unwrap().stdout).trim().to_string()
    }
    fn git_agent(&self, args: &[&str]) -> String {
        let ap = self.agent();
        let mut a = vec!["-C", ap.to_str().unwrap()];
        a.extend_from_slice(args);
        String::from_utf8_lossy(&Command::new("git").args(&a).output().unwrap().stdout).trim().to_string()
    }
}

// ─────────────────────────── init / storage model ───────────────────────────

#[test]
fn init_creates_agent_store_for_sessions() {
    let r = Repo::new();
    assert!(r.agent().join(".git").exists(), "Agent Store should be a standalone git repo");
    assert!(r.agent().join("sessions").exists(), "should have the sessions/ skeleton");
    // no more fact machinery
    assert!(!r.agent().join("state/facts").exists(), "state/facts should have been removed");
    assert!(r.git_env(&["config", "--get", "merge.agit.driver"]).is_empty(), "the fact merge driver is no longer registered");
    // .agit/ is ignored by the code repo
    assert!(r.sh("git check-ignore .agit; echo $?").contains('0'));
    // secret hooks are installed
    assert!(r.agent().join(".git/hooks/pre-commit").exists());
    assert!(r.agent().join(".git/hooks/pre-push").exists());
}

#[test]
fn init_is_idempotent() {
    let r = Repo::new();
    let head1 = r.git_agent(&["rev-parse", "HEAD"]);
    assert_eq!(r.agit(&["init"]).0, 0);
    assert_eq!(r.git_agent(&["rev-parse", "HEAD"]), head1, "re-running init adds no new commit");
}

// ─────────────────────── scope routing (key ambiguity) ───────────────────────

#[test]
fn default_scope_is_transparent_git_on_code_repo() {
    let r = Repo::new();
    let (code, out, _) = r.agit(&["status", "--short"]);
    assert_eq!(code, 0);
    assert!(!out.contains(".agit"), "agit status should not expose .agit/:\n{out}");
}

#[test]
fn agit_dash_a_targets_agent_store() {
    let r = Repo::new();
    r.write(".agit/agent/notes.md", "hi\n");
    assert_eq!(r.agit(&["-a", "add", "-A"]).0, 0);
    assert_eq!(r.agit(&["-a", "commit", "-m", "agent scope"]).0, 0);
    assert_eq!(r.git_agent(&["log", "-1", "--format=%s"]), "agent scope");
    assert_eq!(r.git_env(&["log", "-1", "--format=%s"]), "seed", "the code repo should not gain an extra commit");
}

/// Ambiguity called out by the PRD: the -a in `agit commit -a` is a git flag, not a scope switch.
#[test]
fn commit_dash_a_is_git_flag_not_scope() {
    let r = Repo::new();
    let agent_before = r.git_agent(&["rev-list", "--count", "HEAD"]);
    r.write("app.ts", "export const x = 2;\n");
    let (code, _, err) = r.agit(&["commit", "-a", "-m", "code via -a"]);
    assert_eq!(code, 0, "commit -a should act on the code repo: {err}");
    assert_eq!(r.git_env(&["log", "-1", "--format=%s"]), "code via -a");
    assert_eq!(r.git_agent(&["rev-list", "--count", "HEAD"]), agent_before, "should not touch the Agent Store");
}

// ─────────────────────── WorkspaceRevision pairing ───────────────────────

#[test]
fn agent_commit_generates_workspace_revision() {
    let r = Repo::new();
    r.write(".agit/agent/notes.md", "x\n");
    r.agit(&["-a", "add", "-A"]);
    r.agit(&["-a", "commit", "-m", "c"]);
    let head = r.path().join(".agit/workspace/HEAD.json");
    assert!(head.exists(), "an agent commit should generate a WorkspaceRevision");
    let json = std::fs::read_to_string(&head).unwrap();
    assert!(json.contains("agent_rev") && json.contains("head_commit") && json.contains("stash_tree"));
    assert!(json.contains(&r.git_agent(&["rev-parse", "HEAD"])));
}

#[test]
fn env_commit_also_pairs() {
    let r = Repo::new();
    r.write("app.ts", "export const x = 3;\n");
    r.agit(&["commit", "-am", "code moved"]);
    let log = r.path().join(".agit/workspace/log.jsonl");
    assert!(log.exists());
    assert!(std::fs::read_to_string(&log).unwrap().contains("env:commit"));
}

#[test]
fn environment_state_captures_dirty_worktree() {
    let r = Repo::new();
    r.write("scratch.txt", "未跟踪\n");
    r.write(".agit/agent/notes.md", "x\n");
    r.agit(&["-a", "add", "-A"]);
    r.agit(&["-a", "commit", "-m", "pair while dirty"]);
    let json = std::fs::read_to_string(r.path().join(".agit/workspace/HEAD.json")).unwrap();
    assert!(json.contains("\"dirty\": true"), "{json}");
}

// ─────────────────── secret defense for session dumps ───────────────────

#[test]
fn secret_in_session_blocked_by_precommit() {
    let r = Repo::new();
    // simulate a post-sync session dump that carries a real secret
    r.write(
        ".agit/agent/sessions/claude-code/s.jsonl",
        "{\"type\":\"user\",\"message\":{\"content\":\"key AKIAIOSFODNN7EXAMPLE\"}}\n",
    );
    r.agit(&["-a", "add", "-A"]);
    let (code, _, err) = r.agit(&["-a", "commit", "-m", "leak"]);
    assert_ne!(code, 0, "committing a session that contains a secret should be blocked");
    assert!(err.contains("suspected secrets") || err.contains("aws"), "{err}");
}

#[test]
fn scan_covers_sessions_but_ignores_uuid_noise() {
    let r = Repo::new();
    // a high-entropy UUID/requestId should not false-positive; a real AWS key should be reported
    r.write(
        ".agit/agent/sessions/claude-code/s.jsonl",
        "{\"uuid\":\"7c48816b-6fa5-42f7-9fff-bbeea20ff632\",\"requestId\":\"req_a8Xk92mFqLp3\"}\n\
         {\"content\":\"AKIAIOSFODNN7EXAMPLE\"}\n",
    );
    let (code, _, err) = r.agit(&["-a", "scan"]);
    assert_ne!(code, 0, "a real secret should be reported");
    assert!(err.contains("aws-access-key-id"), "{err}");
    assert!(!err.contains("high-entropy"), "a UUID/requestId inside a session should not be false-flagged by entropy detection:\n{err}");
}

/// Regression: pre-commit must scan **the blob in the index**, not the working tree.
/// Stage a version that carries a secret, then revert the working tree to a clean version (without re-staging); the commit must still be blocked --
/// otherwise the secret lands in the repo while the hook reads the clean working tree and lets it through (the old behavior).
#[test]
fn staged_secret_blocked_even_after_worktree_cleaned() {
    let r = Repo::new();
    let p = ".agit/agent/sessions/claude-code/s.jsonl";
    r.write(p, "{\"content\":\"key AKIAIOSFODNN7EXAMPLE\"}\n"); // the secret version
    r.agit(&["-a", "add", "-A"]); // stage the secret blob
    r.write(p, "{\"content\":\"clean\"}\n"); // revert the working tree to clean, without re-staging
    let (code, _, err) = r.agit(&["-a", "commit", "-m", "sneaky"]);
    assert_ne!(code, 0, "a staged secret should be blocked even if the working tree is already clean: {err}");
    assert!(err.contains("suspected secrets") || err.contains("aws"), "{err}");
    // and the clean working-tree version should have no hits at all (proving we scan the index, not the disk)
    assert!(!err.contains("clean"));
}


/// Regression: codex sync filters by session_meta.cwd -- it syncs only this project's rollouts,
/// and never pulls in another project's sessions (the privacy bottom line).
#[test]
fn codex_sync_only_pulls_matching_project() {
    let r = Repo::new();
    let top = r.sh("git rev-parse --show-toplevel").trim().to_string();
    let home = r.path().join("fakehome");
    let day = home.join(".codex/sessions/2026/07/15");
    std::fs::create_dir_all(&day).unwrap();
    // this project's rollout (cwd == repo root)
    std::fs::write(
        day.join("rollout-2026-07-15T00-00-00-aaaa-mine.jsonl"),
        format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"mineid\",\"cwd\":\"{top}\",\"git\":{{\"branch\":\"main\"}}}}}}\n\
             {{\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"MINE work\"}}}}\n"
        ),
    )
    .unwrap();
    // another project's rollout (different cwd) -- should not be synced
    std::fs::write(
        day.join("rollout-2026-07-15T01-00-00-bbbb-other.jsonl"),
        "{\"type\":\"session_meta\",\"payload\":{\"id\":\"otherid\",\"cwd\":\"/some/other/proj\",\"git\":{\"branch\":\"x\"}}}\n\
         {\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"OTHER secret\"}}\n",
    )
    .unwrap();
    // fork/resume: the 1st session_meta is this project, the 2nd embeds a parent session from **another project**.
    // The whole file must be skipped -- otherwise the parent session's content leaks into this project's store and gets pushed to collaborators.
    std::fs::write(
        day.join("rollout-2026-07-15T02-00-00-cccc-fork.jsonl"),
        format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"forkid\",\"cwd\":\"{top}\",\"git\":{{\"branch\":\"main\"}}}}}}\n\
             {{\"type\":\"session_meta\",\"payload\":{{\"id\":\"parentid\",\"cwd\":\"/some/other/proj\",\"git\":{{\"branch\":\"x\"}}}}}}\n\
             {{\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"PARENT leaked secret\"}}}}\n"
        ),
    )
    .unwrap();

    let (code, out, err) = r.agit_env(&[("HOME", home.to_str().unwrap())], &["-a", "snap", "--from", "codex"]);
    assert_eq!(code, 0, "codex sync should succeed: {err}");
    assert!(out.contains("matched 1 rollouts"), "should match only this project's 1 rollout (the fork must be skipped):\n{out}");

    let cdir = r.agent().join("sessions/codex");
    assert!(cdir.join("mineid.jsonl").exists(), "this project's session should be written to disk");
    assert!(!cdir.join("otherid.jsonl").exists(), "another project's session should never be synced");
    assert!(!cdir.join("forkid.jsonl").exists(), "a fork that contains a foreign-project session should not be synced at all");
    // double insurance: another project's content should not appear anywhere in the codex directory
    let mut all = String::new();
    for e in std::fs::read_dir(&cdir).unwrap() {
        all.push_str(&std::fs::read_to_string(e.unwrap().path()).unwrap());
    }
    assert!(all.contains("MINE work"));
    assert!(!all.contains("OTHER secret"), "another project's content leaked");
    assert!(!all.contains("PARENT leaked secret"), "the parent-project session inside the fork leaked");
}

// ─────────────────────── passthrough fidelity ───────────────────────

#[test]
fn passthrough_propagates_git_exit_code() {
    let r = Repo::new();
    let (code, _, _) = r.agit(&["rev-parse", "does-not-exist"]);
    assert_ne!(code, 0);
    assert_ne!(code, 2, "passthrough should propagate git's exit code");
}
