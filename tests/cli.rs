//! v2 端到端测试：两库模型 + scope 路由 + WorkspaceRevision 配对。
//!
//! merge driver 的裁决测试在 tests/merge.rs（那套领域逻辑跨 v1→v2 不变）。

use std::path::{Path, PathBuf};
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_agit");

struct Repo {
    dir: tempfile::TempDir,
}

impl Repo {
    /// 一个代码仓库 + 已 `agit init` 的 Agent Store。
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
        let o = Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .current_dir(self.path())
            .output()
            .unwrap();
        String::from_utf8_lossy(&o.stdout).to_string()
    }
    fn write(&self, rel: &str, content: &str) {
        let p = self.path().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }
    /// (退出码, stdout, stderr)
    fn agit(&self, args: &[&str]) -> (i32, String, String) {
        let o = Command::new(BIN)
            .args(args)
            .current_dir(self.path())
            .output()
            .unwrap();
        (
            o.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&o.stdout).to_string(),
            String::from_utf8_lossy(&o.stderr).to_string(),
        )
    }
    fn git_env(&self, args: &[&str]) -> String {
        let mut a = vec!["-C", self.path().to_str().unwrap()];
        a.extend_from_slice(args);
        String::from_utf8_lossy(&Command::new("git").args(&a).output().unwrap().stdout)
            .trim()
            .to_string()
    }
    fn git_agent(&self, args: &[&str]) -> String {
        let ap = self.agent();
        let mut a = vec!["-C", ap.to_str().unwrap()];
        a.extend_from_slice(args);
        String::from_utf8_lossy(&Command::new("git").args(&a).output().unwrap().stdout)
            .trim()
            .to_string()
    }
}

// ─────────────────────────── init / 存储模型 ───────────────────────────

#[test]
fn init_creates_two_stores() {
    let r = Repo::new();
    assert!(r.agent().join(".git").exists(), "Agent Store 应是独立 git 仓库");
    assert!(r.agent().join("state/facts").exists(), "应有 state/facts 骨架");
    // .agit/ 被代码仓库 gitignore
    let ignored = r.sh("git check-ignore .agit; echo $?");
    assert!(ignored.contains('0'), ".agit/ 应被代码仓库忽略");
    // merge driver 注册在 Agent Store 上，不是代码仓库
    assert!(r.git_agent(&["config", "--get", "merge.agit.driver"]).contains("merge-file"));
    assert!(r.git_env(&["config", "--get", "merge.agit.driver"]).is_empty());
}

#[test]
fn init_is_idempotent() {
    let r = Repo::new();
    let head1 = r.git_agent(&["rev-parse", "HEAD"]);
    assert_eq!(r.agit(&["init"]).0, 0);
    assert_eq!(r.git_agent(&["rev-parse", "HEAD"]), head1, "重跑 init 不应新增提交");
}

// ─────────────────────── scope 路由（最关键的歧义）───────────────────────

#[test]
fn default_scope_is_transparent_git_on_code_repo() {
    let r = Repo::new();
    // agit status 就是代码仓库的 status
    let (code, out, _) = r.agit(&["status", "--short"]);
    assert_eq!(code, 0);
    // .agit/ 被忽略，不该出现在 untracked 里
    assert!(!out.contains(".agit"), "agit status 不该暴露 .agit/：\n{out}");
}

#[test]
fn agit_dash_a_commit_targets_agent_store() {
    let r = Repo::new();
    r.write(".agit/agent/state/goals.md", "# 目标\n上线退款重构\n");
    assert_eq!(r.agit(&["-a", "add", "-A"]).0, 0);
    assert_eq!(r.agit(&["-a", "commit", "-m", "agent scope"]).0, 0);
    assert_eq!(r.git_agent(&["log", "-1", "--format=%s"]), "agent scope");
    // 代码仓库不该多出提交
    assert_eq!(r.git_env(&["log", "-1", "--format=%s"]), "seed");
}

/// PRD 专门点名的歧义：`agit commit -a` 里的 -a 是 git 的参数，不是 scope 开关。
#[test]
fn commit_dash_a_is_git_flag_not_scope() {
    let r = Repo::new();
    let agent_before = r.git_agent(&["rev-list", "--count", "HEAD"]);

    // 改一个已跟踪文件（不 stage），然后 `agit commit -a` 应作用在代码仓库
    r.write("app.ts", "export const x = 2;\n");
    let (code, _, err) = r.agit(&["commit", "-a", "-m", "code via -a"]);
    assert_eq!(code, 0, "commit -a 应成功作用在代码仓库: {err}");
    assert_eq!(r.git_env(&["log", "-1", "--format=%s"]), "code via -a");

    // Agent Store 一个提交都不该多
    assert_eq!(
        r.git_agent(&["rev-list", "--count", "HEAD"]),
        agent_before,
        "commit -a 不该碰 Agent Store"
    );
}

// ─────────────────────── WorkspaceRevision 配对 ───────────────────────

#[test]
fn commit_generates_workspace_revision() {
    let r = Repo::new();
    r.write(".agit/agent/state/goals.md", "改一下\n");
    r.agit(&["-a", "add", "-A"]);
    r.agit(&["-a", "commit", "-m", "agent change"]);

    let head = r.path().join(".agit/workspace/HEAD.json");
    assert!(head.exists(), "agent commit 后应生成 WorkspaceRevision");
    let json = std::fs::read_to_string(&head).unwrap();
    assert!(json.contains("agent_rev"), "应记录 AgentRevision");
    assert!(json.contains("head_commit"), "应记录 EnvironmentRevision 的 HEAD");
    assert!(json.contains("stash_tree"), "EnvironmentState 应含覆盖工作树的 stash");
    assert!(json.contains("\"trigger\""), "应记录触发来源");

    // 配对里的 agent_rev 应等于 Agent Store 的真实 HEAD
    let agent_head = r.git_agent(&["rev-parse", "HEAD"]);
    assert!(json.contains(&agent_head), "配对的 agent_rev 应指向真实提交");
}

#[test]
fn env_commit_also_pairs() {
    let r = Repo::new();
    r.write("app.ts", "export const x = 3;\n");
    r.agit(&["commit", "-am", "code moved"]);

    let log = r.path().join(".agit/workspace/log.jsonl");
    assert!(log.exists());
    let content = std::fs::read_to_string(&log).unwrap();
    assert!(content.contains("env:commit"), "代码提交也应生成配对:\n{content}");
}

#[test]
fn environment_state_captures_dirty_worktree() {
    let r = Repo::new();
    // 留一个未提交、未跟踪的改动
    r.write("scratch.txt", "未跟踪的工作树内容\n");
    r.write(".agit/agent/state/goals.md", "x\n");
    r.agit(&["-a", "add", "-A"]);
    r.agit(&["-a", "commit", "-m", "pair while dirty"]);

    let json = std::fs::read_to_string(r.path().join(".agit/workspace/HEAD.json")).unwrap();
    assert!(json.contains("\"dirty\": true"), "工作树脏时应记 dirty=true:\n{json}");
}

// ─────────────────────────── 密钥防线 ───────────────────────────

#[test]
fn secret_blocked_by_agent_precommit_hook() {
    let r = Repo::new();
    r.write(
        ".agit/agent/state/facts/leak.md",
        "---\nsubject: leak\n---\nAWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE\n",
    );
    r.agit(&["-a", "add", "-A"]);
    let (code, _, err) = r.agit(&["-a", "commit", "-m", "leak"]);
    assert_ne!(code, 0, "含密钥的 agent 提交应被 pre-commit 拦下");
    assert!(err.contains("疑似密钥") || err.contains("aws"), "{err}");
    // 提交没发生
    assert_eq!(r.git_agent(&["log", "-1", "--format=%s"]).contains("leak"), false);
}

#[test]
fn agent_scan_finds_secret() {
    let r = Repo::new();
    r.write(
        ".agit/agent/state/facts/note.md",
        "连接串 postgresql://u:hunter2ButLonger@db.internal:5432/app\n",
    );
    let (code, _, err) = r.agit(&["-a", "scan"]);
    assert_ne!(code, 0);
    assert!(err.contains("connection-string"), "{err}");
}

// ─────────────────────── 透传保真：退出码 ───────────────────────

#[test]
fn passthrough_propagates_git_exit_code() {
    let r = Repo::new();
    // 一条注定失败的 git 命令，退出码应原样透出（不是 agit 的 2）
    let (code, _, _) = r.agit(&["rev-parse", "does-not-exist"]);
    assert_ne!(code, 0);
    assert_ne!(code, 2, "透传应传播 git 的退出码，而不是 agit 的错误码");
}
