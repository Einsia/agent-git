//! Scope and dual-repo discovery.
//!
//! The core of the PRD: the versioned objects are two git repos + a pairing.
//!   Environment = the user's existing code repository (left untouched)
//!   Agent Store = a standalone git repository holding AgentState, found by IDENTITY (`crate::agent`)
//!
//!   agit <git-args>        = agit -e <git-args>  → git operates on the Environment
//!   agit agent <git-args>  (alias: agit a)       → git operates on the Agent Store (isomorphic operation)
//!
//! The store is **not** a location this module decides. An agent is a memory that outlives any one
//! repo, so it lives at `$AGIT_HOME/agents/<aid>/` and is reached by resolving *which agent* — never
//! by walking to a path relative to the code repo.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Default: operates on the code repository. Must be a transparent git wrapper.
    Environment,
    /// `agit agent` / `agit a` (or the deprecated `-a`): operates on the Agent Store.
    Agent,
}

/// Where the WorkspaceRevision log lives. Deliberately placed outside both git worktrees, to avoid moving the agent ref when writing a pairing.
pub const WORKSPACE_DIR: &str = ".agit/workspace";

/// The working-tree root of the Environment (the code repository).
pub fn env_root() -> Result<PathBuf> {
    let out = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .context("failed to execute git")?;
    if !out.status.success() {
        bail!("The current directory is not inside a git repository. agit's Environment is your code repository.");
    }
    Ok(PathBuf::from(
        String::from_utf8_lossy(&out.stdout).trim(),
    ))
}

/// The working-tree root of the resolved agent's store: `$AGIT_HOME/agents/<aid>/`.
///
/// There is exactly ONE answer, and identity is what gives it (`crate::agent::resolve`: `--agent` →
/// `$AGIT_AGENT` → the active pointer → `.agit.toml [defaults]` → an actionable error).
///
/// The three rungs this replaced — `$AGIT_AGENT_DIR`, the `.agit/store` pointer file, and the nested
/// `<env>/.agit/agent` — all answered "where", and none of them answered "whose". A store found by
/// location is welded to one code repo, which is what made PRD #3 (an agent carrying its memory into
/// another repo) impossible. The pointer file failed the rule that keeps a local file honest — *its
/// absence must be fully recoverable from committed state* — because deleting it left nothing on earth
/// able to say where the store had gone.
pub fn agent_root() -> Result<PathBuf> {
    Ok(crate::agent::resolve(None)?.store)
}

pub fn workspace_dir() -> Result<PathBuf> {
    Ok(env_root()?.join(WORKSPACE_DIR))
}

/// agit's own home, for state that spans repos: `$AGIT_HOME` when set and non-empty, else `$HOME/.agit`.
///
/// The ONLY place that reads `$AGIT_HOME`. Tests point it (and `$HOME`) at a temp dir per invocation, so
/// a test run can never reach the developer's real stores.
pub fn agit_home() -> Result<PathBuf> {
    agit_home_from(
        std::env::var("AGIT_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
}

/// The resolution itself, taking the environment as arguments so it is testable without `set_var`
/// (which is process-global and races with parallel tests).
fn agit_home_from(agit_home: Option<&str>, home: Option<&str>) -> Result<PathBuf> {
    if let Some(h) = agit_home.map(str::trim).filter(|h| !h.is_empty()) {
        return Ok(PathBuf::from(h));
    }
    let home = home
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .context("could not read $HOME, and $AGIT_HOME is not set")?;
    Ok(PathBuf::from(home).join(".agit"))
}

/// The working-tree root of the repository for a given scope.
///
/// `Scope::Agent` still means "run git on the agent's store" — `agit a log`, `agit a commit` — it just
/// reaches it by identity now.
pub fn root_for(scope: Scope) -> Result<PathBuf> {
    match scope {
        Scope::Environment => env_root(),
        Scope::Agent => {
            let a = agent_root()?;
            if !a.join(".git").exists() {
                bail!(
                    "{} carries an agent's identity but is not a git repository.\n\
                     \x20      The store is damaged; if it has a remote, re-clone it with `agit a clone <url>`.",
                    a.display()
                );
            }
            Ok(a)
        }
    }
}

/// Run a single git command in the given repository's working tree, require success, and return stdout.
pub fn git_in(root: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .context("failed to execute git")?;
    if !out.status.success() {
        bail!(
            "git -C {} {} failed:\n{}",
            root.display(),
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

/// Failure-allowed variant: returns (exit code, stdout).
pub fn git_in_status(root: &Path, args: &[&str]) -> (i32, String) {
    match Command::new("git").arg("-C").arg(root).args(args).output() {
        Ok(out) => (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).trim_end().to_string(),
        ),
        Err(_) => (-1, String::new()),
    }
}

/// Inherited-stdio variant: git's progress, interactive credential prompts, and real stderr all go straight to the terminal.
/// Remote operations (clone/fetch) must use this -- capturing stdout would swallow git's error messages and also block credential input.
pub fn git_in_inherit(root: &Path, args: &[&str]) -> i32 {
    use std::process::Stdio;
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map(|s| s.code().unwrap_or(-1))
        .unwrap_or(-1)
}

// ─────────────── plausibly-here detection (stranded sessions started in the wrong dir) ───────────────

/// The git work-tree root of `dir`, or `None` when `dir` is not inside a git repository (or does not
/// exist locally — a recorded cwd from another machine simply answers `None`).
pub fn git_toplevel(dir: &Path) -> Option<PathBuf> {
    let (rc, out) = git_in_status(dir, &["rev-parse", "--show-toplevel"]);
    (rc == 0 && !out.trim().is_empty()).then(|| PathBuf::from(out.trim()))
}

fn origin_url(dir: &Path) -> Option<String> {
    let (rc, out) = git_in_status(dir, &["remote", "get-url", "origin"]);
    (rc == 0 && !out.trim().is_empty()).then(|| out.trim().to_string())
}

/// The repo's root commit(s). A repo with several roots (grafted histories) lists several; the LAST line
/// is used so two checkouts of the same history compare equal regardless of ordering quirks.
fn root_commit(dir: &Path) -> Option<String> {
    let (rc, out) = git_in_status(dir, &["rev-list", "--max-parents=0", "HEAD"]);
    if rc != 0 {
        return None;
    }
    out.lines().last().map(|l| l.trim().to_string()).filter(|s| !s.is_empty())
}

/// Do two working directories belong to the SAME repository? True when they share a work-tree root, an
/// origin url, or a root commit. Cross-machine paths that don't resolve locally answer `false` (git
/// can't identify them), which is the safe default — an unverifiable match must not warn.
pub fn same_repo(a: &Path, b: &Path) -> bool {
    let (ta, tb) = match (git_toplevel(a), git_toplevel(b)) {
        (Some(ta), Some(tb)) => (ta, tb),
        _ => return false,
    };
    if ta == tb {
        return true; // same repo, different subpath
    }
    if let (Some(ua), Some(ub)) = (origin_url(a), origin_url(b)) {
        if ua == ub {
            return true; // sibling clone: shared origin
        }
    }
    match (root_commit(a), root_commit(b)) {
        (Some(ra), Some(rb)) => ra == rb, // sibling clone: shared history root
        _ => false,
    }
}

/// Is a session recorded under `recorded` PLAUSIBLY meant for the repo rooted at `env`?
///   (a) `recorded` is a strict PARENT directory of `env` (ran in the monorepo/outer dir, meant a subdir), OR
///   (b)/(c) `recorded` resolves into the SAME repository as `env` (same work-tree, origin, or root commit).
///
/// `recorded == env` is NOT stranded — that session is OWNED, so it returns `false`. A genuinely
/// unrelated project (a different repo that is neither a parent nor a same-repo checkout) also returns
/// `false`: it was a correct, legitimate drop and must never warn.
pub fn plausibly_here(recorded: &Path, env: &Path) -> bool {
    if recorded == env {
        return false;
    }
    // (a) strict parent: component-wise prefix (equality already excluded above), so `/p/app` is a child
    // of `/p` but NOT of the unrelated `/pp`.
    if env.starts_with(recorded) {
        return true;
    }
    same_repo(recorded, env)
}

#[cfg(test)]
mod plausibly_here_tests {
    use super::plausibly_here;
    use std::path::Path;

    #[test]
    fn parent_is_plausible_equal_and_unrelated_are_not() {
        // (a) a strict parent of env is plausibly-meant-for-env (the wrong-directory case).
        assert!(plausibly_here(Path::new("/p"), Path::new("/p/app")));
        assert!(plausibly_here(Path::new("/home/you/proj"), Path::new("/home/you/proj/app")));
        // env itself is OWNED, not stranded.
        assert!(!plausibly_here(Path::new("/p/app"), Path::new("/p/app")));
        // a string-prefix that is not a path-component parent must NOT count (no false positive).
        assert!(!plausibly_here(Path::new("/pp"), Path::new("/p/app")));
        // an unrelated sibling that does not exist on disk (no git identity) must NOT warn.
        assert!(!plausibly_here(Path::new("/some/other/proj"), Path::new("/p/app")));
        // a CHILD of env is not a parent; without git it can't be proven same-repo, so it's not plausible here.
        assert!(!plausibly_here(Path::new("/p/app/sub"), Path::new("/p/app")));
    }
}

#[cfg(test)]
mod agit_home_tests {
    use super::agit_home_from;
    use std::path::PathBuf;

    #[test]
    fn agit_home_prefers_the_var_then_falls_back_to_home() {
        assert_eq!(
            agit_home_from(Some("/x/store"), Some("/home/dev")).unwrap(),
            PathBuf::from("/x/store"),
            "$AGIT_HOME wins when set"
        );
        assert_eq!(
            agit_home_from(None, Some("/home/dev")).unwrap(),
            PathBuf::from("/home/dev/.agit"),
            "unset → $HOME/.agit"
        );
        // An empty/blank var must not resolve to the *relative* `.agit`, which would silently plant a
        // store in whatever cwd agit happened to run from.
        for blank in ["", "   "] {
            assert_eq!(
                agit_home_from(Some(blank), Some("/home/dev")).unwrap(),
                PathBuf::from("/home/dev/.agit"),
                "blank $AGIT_HOME is treated as unset"
            );
        }
        assert!(
            agit_home_from(None, None).is_err(),
            "no $AGIT_HOME and no $HOME must fail loudly, not yield a relative path"
        );
    }
}
