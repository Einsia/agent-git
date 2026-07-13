//! Adapter 测试：Claude Code session → AgentState 的确定性抽取，Codex 接口 seam。

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
        r.sh("git config user.name dev && git config user.email d@x.com && git config commit.gpgsign false");
        // 一个会被合成 session「读过」的文件
        r.write(
            "models/user.ts",
            "export interface User {\n  id: number;\n  user_id: string;\n}\n",
        );
        r.sh("git add -A && git commit -qm seed");
        assert_eq!(r.agit(&["init"]).0, 0);
        r
    }
    fn path(&self) -> &Path {
        self.dir.path()
    }
    fn sh(&self, cmd: &str) -> String {
        String::from_utf8_lossy(
            &Command::new("sh")
                .arg("-c")
                .arg(cmd)
                .current_dir(self.path())
                .output()
                .unwrap()
                .stdout,
        )
        .to_string()
    }
    fn write(&self, rel: &str, content: &str) {
        let p = self.path().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }
    fn read_state(&self, rel: &str) -> String {
        std::fs::read_to_string(self.path().join(".agit/agent/state").join(rel)).unwrap_or_default()
    }
    fn agit(&self, args: &[&str]) -> (i32, String, String) {
        let o = Command::new(BIN).args(args).current_dir(self.path()).output().unwrap();
        (
            o.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&o.stdout).to_string(),
            String::from_utf8_lossy(&o.stderr).to_string(),
        )
    }
}

/// 造一份最小的 Claude Code 风格 session：prompt + Read(带行范围) + Bash + Write。
fn synthetic_session(dir: &Path, canary: &Path) -> PathBuf {
    let lines = [
        r#"{"type":"mode","sessionId":"s1"}"#.to_string(),
        r#"{"type":"system","cwd":"CWD","gitBranch":"main","isMeta":true,"message":{"role":"user","content":"<caveat>ignore me</caveat>"}}"#.to_string(),
        r#"{"type":"user","cwd":"CWD","gitBranch":"main","uuid":"u1","message":{"role":"user","content":"查清 user 字段名"}}"#.to_string(),
        format!(
            r#"{{"type":"assistant","uuid":"a1","message":{{"role":"assistant","content":[{{"type":"text","text":"我来读一下 models/user.ts。"}},{{"type":"tool_use","id":"t1","name":"Read","input":{{"file_path":"CWD/models/user.ts","offset":3,"limit":1}}}}]}}}}"#
        ),
        format!(
            r#"{{"type":"assistant","uuid":"a2","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"t2","name":"Bash","input":{{"command":"touch {}"}}}},{{"type":"tool_use","id":"t3","name":"Write","input":{{"file_path":"CWD/notes.md","content":"x"}}}}]}}}}"#,
            canary.display()
        ),
    ];
    let cwd = dir.to_string_lossy();
    let body = lines.join("\n").replace("CWD", &cwd);
    let p = dir.join("session.jsonl");
    std::fs::write(&p, body).unwrap();
    p
}

#[test]
fn import_extracts_agentstate_from_claude_session() {
    let r = Repo::new();
    let canary = r.path().join("PWNED");
    let sess = synthetic_session(r.path(), &canary);

    let (code, out, err) = r.agit(&["-a", "import", sess.to_str().unwrap()]);
    assert_eq!(code, 0, "import 应成功: {err}");
    assert!(out.contains("1 条 prompt") || out.contains("目标"), "{out}");

    // 目标来自真实 prompt，caveat 被剔除
    let goals = r.read_state("goals.md");
    assert!(goals.contains("查清 user 字段名"), "{goals}");
    assert!(!goals.contains("ignore me"), "caveat 不该进目标");

    // Read 的文件被对齐到当前基线，带上了摘要（#...）
    let pool = r.read_state("_evidence_pool.md");
    assert!(
        pool.contains("file:models/user.ts:3 #"),
        "Read 应产出对齐基线、带摘要的 file 证据:\n{pool}"
    );

    // Bash 命令被记录，但 **绝不执行**
    assert!(pool.contains("cmd:touch"), "命令应入池:\n{pool}");
    assert!(!canary.exists(), "import 绝不能执行 session 里的命令");

    // Write 成了 artifact
    let art = r.read_state("artifacts.md");
    assert!(art.contains("notes.md"), "{art}");

    // 溯源：记录了 session id 与环境基线
    let sj = r.read_state("_session.json");
    assert!(sj.contains("\"session_id\": \"session\"") || sj.contains("session"));
    assert!(sj.contains("head_commit"), "应记录 EnvironmentRevision 基线");
}

#[test]
fn imported_state_commits_into_agent_store() {
    let r = Repo::new();
    let sess = synthetic_session(r.path(), &r.path().join("PWNED"));
    r.agit(&["-a", "import", sess.to_str().unwrap()]);

    assert_eq!(r.agit(&["-a", "add", "-A"]).0, 0);
    assert_eq!(r.agit(&["-a", "commit", "-m", "import context"]).0, 0);
    let log = r.sh("git -C .agit/agent log -1 --format=%s");
    assert!(log.contains("import context"), "{log}");
}

#[test]
fn codex_adapter_reports_not_implemented() {
    let r = Repo::new();
    let (code, _, err) = r.agit(&["-a", "import", "--from", "codex"]);
    assert_ne!(code, 0, "codex 应显式报未实现，而不是静默");
    assert!(err.contains("尚未实现") || err.contains("未实现"), "{err}");
}

#[test]
fn adapter_list_shows_both_runtimes() {
    let r = Repo::new();
    let (code, out, _) = r.agit(&["adapter"]);
    assert_eq!(code, 0);
    assert!(out.contains("claude-code"));
    assert!(out.contains("codex"));
}

#[test]
fn export_produces_portable_digest() {
    let r = Repo::new();
    let sess = synthetic_session(r.path(), &r.path().join("PWNED"));
    r.agit(&["-a", "import", sess.to_str().unwrap()]);
    // 手写一条 fact，让 digest 里有内容
    r.write(
        ".agit/agent/state/facts/api__user__id.md",
        "---\nsubject: api/user/id\n---\n字段叫 user_id。\n",
    );
    let out = r.path().join("ctx-digest.md");
    let (code, _, err) = r.agit(&["-a", "export", out.to_str().unwrap()]);
    assert_eq!(code, 0, "{err}");
    let digest = std::fs::read_to_string(&out).unwrap();
    assert!(digest.contains("目标"), "digest 应含目标");
    assert!(digest.contains("字段叫 user_id"), "digest 应含 fact:\n{digest}");
}
