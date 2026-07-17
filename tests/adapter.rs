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
        // `--agent` is required non-interactively: an agent is named for what it knows, so agit will
        // not invent a label from the directory.
        assert_eq!(r.agit(&["init", "--agent", "adapter-test"]).0, 0);
        r
    }
    fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Every process this suite spawns goes through here. agit resolves ~/.claude, ~/.codex and its own
    /// home from the environment, so an un-isolated test reads — and can write — the developer's real
    /// session stores. Per-invocation env only: `std::env::set_var` is process-global and would race
    /// across parallel tests.
    fn cmd(&self, program: &str) -> Command {
        let mut c = Command::new(program);
        c.current_dir(self.path())
            .env("HOME", self.path())
            .env("AGIT_HOME", self.path().join("agit-home"));
        c
    }
    fn sh(&self, c: &str) -> String {
        let o = self.cmd("sh").arg("-c").arg(c).output().unwrap();
        String::from_utf8_lossy(&o.stdout).to_string()
    }
    fn write(&self, rel: &str, content: &str) {
        let p = self.path().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }
    fn agit(&self, args: &[&str]) -> (i32, String, String) {
        let o = self.cmd(BIN).args(args).output().unwrap();
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
/// (matching 0 rollouts) rather than reporting "not implemented". `Repo::cmd` points HOME at the
/// isolated temp dir, which has no .codex/sessions → 0 rollouts, and never the machine's real ~/.codex.
#[test]
fn codex_snap_is_implemented() {
    let r = Repo::new();
    let (code, out, err) = r.agit(&["-a", "snap", "--from", "codex"]);
    assert_eq!(code, 0, "codex snap should work now: {err}");
    assert!(out.contains("codex") && out.contains("matched 0 rollouts"), "should mirror codex (0 rollouts): {out}");
}
