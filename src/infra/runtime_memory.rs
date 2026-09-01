//! Where each runtime keeps "this project's memory".
//!
//! agit does not hold memory for the runtime; it touches that directory at two moments: before
//! launch it merges the branch's memory in, and at settlement it collects the directory's changes
//! into the branch (see [`crate::commands::memory`]). So this module answers one question — given
//! a runtime and a working directory, where its project memory directory is.
//!
//! * Claude Code: `<config>/projects/<encoded project root>/memory/`, where `<config>` is
//!   `CLAUDE_CONFIG_DIR` or `~/.claude`; `autoMemoryDirectory` in settings overrides the whole
//!   location. The project root is the main checkout of the git repo holding cwd (linked
//!   worktrees share one memory), or cwd itself when it is outside a repo; the encoding replaces
//!   every non-alphanumeric character in the path with `-`.
//! * Codex: memory is global and generated in the background, with no per-project directory — the
//!   answer here is None, and Codex memory is collected through another path.
//! * OpenCode / Cursor: no memory persisted to disk.

use std::path::{Path, PathBuf};

/// A runtime's project memory directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeMemory {
    pub runtime: &'static str,
    pub dir: PathBuf,
}

/// The runtime's memory directory under this working directory; None when the runtime persists no
/// per-project memory.
pub fn locate(runtime: &str, cwd: &Path) -> Option<RuntimeMemory> {
    match runtime {
        "claude-code" => {
            let config = claude_config_dir()?;
            let settings = [
                config.join("settings.json"),
                cwd.join(".claude/settings.json"),
                cwd.join(".claude/settings.local.json"),
            ];
            let dir = claude_memory_dir(&config, cwd, &settings);
            Some(RuntimeMemory {
                runtime: "claude-code",
                dir,
            })
        }
        _ => None,
    }
}

fn claude_config_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR")
        && !dir.trim().is_empty()
    {
        return Some(PathBuf::from(dir.trim()));
    }
    home().map(|h| h.join(".claude"))
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Claude Code's memory directory: `autoMemoryDirectory` wins, otherwise the encoded project root.
pub fn claude_memory_dir(config: &Path, cwd: &Path, settings: &[PathBuf]) -> PathBuf {
    for file in settings {
        if let Some(dir) = auto_memory_directory(file) {
            return dir;
        }
    }
    config
        .join("projects")
        .join(encode_project(&project_root(cwd)))
        .join("memory")
}

fn auto_memory_directory(settings: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(settings).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let raw = value.get("autoMemoryDirectory")?.as_str()?.trim();
    if raw.is_empty() {
        return None;
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        return home().map(|h| h.join(rest));
    }
    Some(PathBuf::from(raw))
}

/// Memory belongs to a project root: the main checkout of the git repo holding cwd, or cwd itself
/// when it is outside a repo.
fn project_root(cwd: &Path) -> PathBuf {
    let common = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--git-common-dir"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let root = match common {
        Some(dir) => {
            let dir = PathBuf::from(dir);
            let dir = if dir.is_absolute() {
                dir
            } else {
                cwd.join(dir)
            };
            dir.parent().map(Path::to_path_buf).unwrap_or(dir)
        }
        None => cwd.to_path_buf(),
    };
    root.canonicalize().unwrap_or(root)
}

/// Claude Code's project directory name: each non-alphanumeric character in the path becomes `-`.
pub fn encode_project(root: &Path) -> String {
    root.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_project_path_becomes_a_dash_separated_name() {
        assert_eq!(
            encode_project(Path::new("/Users/me/Code/agent-git")),
            "-Users-me-Code-agent-git"
        );
        assert_eq!(
            encode_project(Path::new("/tmp/x/-Users-me")),
            "-tmp-x--Users-me",
            "a dash in the path stays a dash, so the encoding is not reversible"
        );
        assert_eq!(encode_project(Path::new("/a/b.c_d")), "-a-b-c-d");
    }

    #[test]
    fn auto_memory_directory_in_settings_wins() {
        let d = tempfile::tempdir().unwrap();
        let settings = d.path().join("settings.json");
        std::fs::write(&settings, r#"{"autoMemoryDirectory": "/elsewhere/mem"}"#).unwrap();
        let dir = claude_memory_dir(d.path(), d.path(), &[settings]);
        assert_eq!(dir, PathBuf::from("/elsewhere/mem"));
    }

    #[test]
    fn without_settings_the_dir_is_under_projects() {
        let d = tempfile::tempdir().unwrap();
        let cwd = d.path().join("proj");
        std::fs::create_dir_all(&cwd).unwrap();
        let dir = claude_memory_dir(d.path(), &cwd, &[d.path().join("absent.json")]);
        let expected = d
            .path()
            .join("projects")
            .join(encode_project(&cwd.canonicalize().unwrap()))
            .join("memory");
        assert_eq!(dir, expected);
    }
}
