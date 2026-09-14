//! Runtime names and aliases share the adapter construction inventory.

use super::Adapter;
use anyhow::Context;
use std::path::PathBuf;

pub struct RuntimeRegistration {
    pub id: &'static str,
    pub label: &'static str,
    pub skill_dir: Option<fn() -> crate::Result<PathBuf>>,
    pub aliases: &'static [&'static str],
    pub create: fn() -> Box<dyn Adapter>,
}

macro_rules! runtimes {
    ($(($id:literal, $label:literal, [$($alias:literal),*], $adapter:path, $skill:expr)),* $(,)?) => {
        pub const RUNTIMES: &[&str] = &[$($id),*];
        pub const REGISTRY: &[RuntimeRegistration] = &[
            $(RuntimeRegistration { id: $id, label: $label, skill_dir: $skill, aliases: &[$($alias),*], create: || Box::new($adapter) }),*
        ];
    };
}

fn user_skill(relative: &str) -> crate::Result<PathBuf> {
    Ok(crate::infra::config::user_home()
        .context("the user home is not set")?
        .join(relative))
}

runtimes! {
    ("claude-code", "Claude Code", ["claude", "cc"], super::claude_code::ClaudeCode, Some(|| user_skill(".claude/skills/agit"))),
    ("codex", "Codex", ["cx", "chatgpt-desktop", "chatgpt", "chatgpt-app", "codex-app"], super::codex::Codex, Some(|| Ok(super::codex::codex_home()?.join("skills/agit")))),
    ("cursor", "Cursor", [], super::cursor::Cursor, Some(|| user_skill(".cursor/skills/agit"))),
    ("claude-desktop", "Claude Desktop", ["claude-app"], super::claude_desktop::ClaudeDesktop, None),
    ("opencode", "OpenCode", [], super::opencode::OpenCode, Some(|| user_skill(".config/opencode/skills/agit"))),
    ("hermes", "Hermes", ["hermes-agent"], super::hermes::Hermes, Some(|| Ok(super::hermes::home()?.join("skills/agit")))),
    ("openclaw", "OpenClaw", [], super::openclaw::OpenClaw, Some(|| Ok(super::openclaw::home()?.join("skills/agit")))),
    ("workbuddy", "WorkBuddy", ["workbuddy-ai"], super::workbuddy::WorkBuddy, Some(|| Ok(super::workbuddy::home()?.join("skills/agit")))),
}

pub fn registration(runtime: &str) -> Option<&'static RuntimeRegistration> {
    let id = super::normalize(runtime).ok()?;
    REGISTRY.iter().find(|entry| entry.id == id)
}

pub fn setup_runtimes() -> impl Iterator<Item = &'static str> {
    REGISTRY
        .iter()
        .filter(|entry| entry.skill_dir.is_some())
        .map(|entry| entry.id)
}

pub fn runtime_label(runtime: &str) -> &'static str {
    registration(runtime)
        .map(|entry| entry.label)
        .unwrap_or("agent")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_registered_name_resolves_to_its_own_adapter() {
        let mut names = std::collections::HashSet::new();
        for r in REGISTRY {
            assert_eq!((r.create)().id(), r.id);
            for name in std::iter::once(&r.id).chain(r.aliases) {
                assert!(names.insert(*name), "duplicate runtime name: {name}");
                assert_eq!(super::super::get(name).unwrap().id(), r.id);
            }
        }
    }

    #[test]
    fn read_only_targets_do_not_launch_an_unrelated_runtime() {
        let cwd = std::path::Path::new("/workspace");
        assert!(
            super::super::get("cursor")
                .unwrap()
                .start_command(cwd)
                .is_none()
        );
        assert!(super::super::get("unknown-runtime").is_err());
        assert_eq!(
            super::super::get("cx")
                .unwrap()
                .start_command(cwd)
                .as_deref(),
            Some("codex")
        );
    }

    #[cfg(unix)]
    #[test]
    fn native_launch_arguments_survive_shell_parsing() {
        let id = "session'$(false)";
        let prompt = "It's a literal $HOME\n`false`";
        for runtime in ["claude-code", "codex"] {
            let command = super::super::get(runtime)
                .unwrap()
                .resume_command(
                    id,
                    std::path::Path::new("/workspace with spaces"),
                    Some(prompt),
                    None,
                )
                .unwrap();
            let result = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!("set -- {command}; printf '%s\\0' \"$@\""))
                .output()
                .unwrap();
            assert!(result.status.success());
            let args: Vec<_> = result.stdout.split(|byte| *byte == 0).collect();
            assert!(args.contains(&id.as_bytes()));
            assert!(args.contains(&prompt.as_bytes()));
        }
    }
}
