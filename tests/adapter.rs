//! Adapter 测试:Claude Code session 解析(reconcile 的 brief 靠它),Codex 是显式桩。

use std::path::Path;
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
        r.sh("git config user.name d && git config user.email d@x && git config commit.gpgsign false");
        r.write("app.ts", "x\n");
        r.sh("git add -A && git commit -qm seed");
        assert_eq!(r.agit(&["init"]).0, 0);
        r
    }
    fn path(&self) -> &Path {
        self.dir.path()
    }
    fn sh(&self, c: &str) -> String {
        String::from_utf8_lossy(&Command::new("sh").arg("-c").arg(c).current_dir(self.path()).output().unwrap().stdout).to_string()
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
}

#[test]
fn adapter_list_shows_both_runtimes() {
    let r = Repo::new();
    let (code, out, _) = r.agit(&["adapter"]);
    assert_eq!(code, 0);
    assert!(out.contains("claude-code") && out.contains("codex"));
}

/// Codex 走 sync 时显式报未实现,而不是静默。
#[test]
fn codex_sync_reports_not_implemented() {
    let r = Repo::new();
    let (code, _, err) = r.agit(&["-a", "sync", "--from", "codex"]);
    assert_ne!(code, 0, "codex 应显式报未接");
    assert!(err.contains("还没接") || err.contains("codex"), "{err}");
}
