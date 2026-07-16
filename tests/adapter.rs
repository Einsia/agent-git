//! Adapter tests: session parsing for Claude Code / Codex (reconcile's brief relies on it).

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

/// Codex snap is wired up: even when this project has no sessions in Codex, it returns normally
/// (matching 0 rollouts) rather than reporting "not implemented". HOME points at an empty dir to
/// stay hermetic and not depend on the machine's real ~/.codex.
#[test]
fn codex_snap_is_implemented() {
    let r = Repo::new();
    let o = Command::new(BIN)
        .args(["-a", "snap", "--from", "codex"])
        .current_dir(r.path())
        .env("HOME", r.path()) // no .codex/sessions → matches 0 rollouts
        .output()
        .unwrap();
    let code = o.status.code().unwrap_or(-1);
    let out = String::from_utf8_lossy(&o.stdout);
    assert_eq!(code, 0, "codex snap should work now: {}", String::from_utf8_lossy(&o.stderr));
    assert!(out.contains("codex") && out.contains("matched 0 rollouts"), "should mirror codex (0 rollouts): {out}");
}
