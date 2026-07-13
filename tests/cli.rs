//! v3 端到端:双库模型 + scope 路由 + WorkspaceRevision 配对 + session dump 的密钥防线。
//! fact/evidence 那一套已废弃移除。

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
        assert_eq!(r.agit(&["init"]).0, 0, "init 应成功");
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

// ─────────────────────────── init / 存储模型 ───────────────────────────

#[test]
fn init_creates_agent_store_for_sessions() {
    let r = Repo::new();
    assert!(r.agent().join(".git").exists(), "Agent Store 应是独立 git 仓库");
    assert!(r.agent().join("sessions").exists(), "应有 sessions/ 骨架");
    // 不再有 fact 那套
    assert!(!r.agent().join("state/facts").exists(), "state/facts 应已移除");
    assert!(r.git_env(&["config", "--get", "merge.agit.driver"]).is_empty(), "不再注册 fact merge driver");
    // .agit/ 被代码仓库忽略
    assert!(r.sh("git check-ignore .agit; echo $?").contains('0'));
    // 密钥 hook 装好
    assert!(r.agent().join(".git/hooks/pre-commit").exists());
    assert!(r.agent().join(".git/hooks/pre-push").exists());
}

#[test]
fn init_is_idempotent() {
    let r = Repo::new();
    let head1 = r.git_agent(&["rev-parse", "HEAD"]);
    assert_eq!(r.agit(&["init"]).0, 0);
    assert_eq!(r.git_agent(&["rev-parse", "HEAD"]), head1, "重跑 init 不新增提交");
}

// ─────────────────────── scope 路由（关键歧义）───────────────────────

#[test]
fn default_scope_is_transparent_git_on_code_repo() {
    let r = Repo::new();
    let (code, out, _) = r.agit(&["status", "--short"]);
    assert_eq!(code, 0);
    assert!(!out.contains(".agit"), "agit status 不该暴露 .agit/：\n{out}");
}

#[test]
fn agit_dash_a_targets_agent_store() {
    let r = Repo::new();
    r.write(".agit/agent/notes.md", "hi\n");
    assert_eq!(r.agit(&["-a", "add", "-A"]).0, 0);
    assert_eq!(r.agit(&["-a", "commit", "-m", "agent scope"]).0, 0);
    assert_eq!(r.git_agent(&["log", "-1", "--format=%s"]), "agent scope");
    assert_eq!(r.git_env(&["log", "-1", "--format=%s"]), "seed", "代码仓库不该多提交");
}

/// PRD 点名的歧义:`agit commit -a` 里的 -a 是 git 参数,不是 scope 开关。
#[test]
fn commit_dash_a_is_git_flag_not_scope() {
    let r = Repo::new();
    let agent_before = r.git_agent(&["rev-list", "--count", "HEAD"]);
    r.write("app.ts", "export const x = 2;\n");
    let (code, _, err) = r.agit(&["commit", "-a", "-m", "code via -a"]);
    assert_eq!(code, 0, "commit -a 应作用在代码仓库: {err}");
    assert_eq!(r.git_env(&["log", "-1", "--format=%s"]), "code via -a");
    assert_eq!(r.git_agent(&["rev-list", "--count", "HEAD"]), agent_before, "不该碰 Agent Store");
}

// ─────────────────────── WorkspaceRevision 配对 ───────────────────────

#[test]
fn agent_commit_generates_workspace_revision() {
    let r = Repo::new();
    r.write(".agit/agent/notes.md", "x\n");
    r.agit(&["-a", "add", "-A"]);
    r.agit(&["-a", "commit", "-m", "c"]);
    let head = r.path().join(".agit/workspace/HEAD.json");
    assert!(head.exists(), "agent commit 后应生成 WorkspaceRevision");
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

// ─────────────────── session dump 的密钥防线 ───────────────────

#[test]
fn secret_in_session_blocked_by_precommit() {
    let r = Repo::new();
    // 模拟 sync 后的 session dump,里面带一个真密钥
    r.write(
        ".agit/agent/sessions/claude-code/s.jsonl",
        "{\"type\":\"user\",\"message\":{\"content\":\"key AKIAIOSFODNN7EXAMPLE\"}}\n",
    );
    r.agit(&["-a", "add", "-A"]);
    let (code, _, err) = r.agit(&["-a", "commit", "-m", "leak"]);
    assert_ne!(code, 0, "含密钥的 session 提交应被拦");
    assert!(err.contains("疑似密钥") || err.contains("aws"), "{err}");
}

#[test]
fn scan_covers_sessions_but_ignores_uuid_noise() {
    let r = Repo::new();
    // 高熵 UUID/requestId 不该误报;真 AWS key 该报
    r.write(
        ".agit/agent/sessions/claude-code/s.jsonl",
        "{\"uuid\":\"7c48816b-6fa5-42f7-9fff-bbeea20ff632\",\"requestId\":\"req_a8Xk92mFqLp3\"}\n\
         {\"content\":\"AKIAIOSFODNN7EXAMPLE\"}\n",
    );
    let (code, _, err) = r.agit(&["-a", "scan"]);
    assert_ne!(code, 0, "真密钥应报");
    assert!(err.contains("aws-access-key-id"), "{err}");
    assert!(!err.contains("high-entropy"), "session 里的 UUID/requestId 不该被熵检测误报:\n{err}");
}

// ─────────────────────── 透传保真 ───────────────────────

#[test]
fn passthrough_propagates_git_exit_code() {
    let r = Repo::new();
    let (code, _, _) = r.agit(&["rev-parse", "does-not-exist"]);
    assert_ne!(code, 0);
    assert_ne!(code, 2, "透传应传播 git 的退出码");
}
