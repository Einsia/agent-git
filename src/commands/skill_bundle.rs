//! AgentGit Skill resources bundled at compile time.
//!
//! `setup_skill.md` is the single entry-point source. The per-command documents are the
//! `references/commands/` resources of every runtime, and the input to doctor's completeness
//! check. Listing the resource names explicitly is deliberate: Rust's `include_str!` has no
//! directory glob, so the compiler errors the moment a file is deleted or renamed instead of
//! letting the installer silently ship one sub-skill short.

use crate::infra::config;

pub const VERSION_FILE: &str = "VERSION";
pub const REFERENCES_DIR: &str = "references/commands";

/// Markers delimiting an inline Skill section already present in a file; `--skill` writes a
/// native Skill directory, and `--agents-md` reuses this same pair for its short integration
/// section.
pub const BEGIN_MARKER: &str = "<!-- agit:begin -->";
pub const END_MARKER: &str = "<!-- agit:end -->";

/// The standalone skill section in a Cursor project's AGENTS.md.
///
/// A Cursor project can also carry the short integration section `--agents-md` writes, so the
/// two cannot share one marker pair — one update would replace the other section.
pub const CURSOR_BEGIN_MARKER: &str = "<!-- agit:skill-begin -->";
pub const CURSOR_END_MARKER: &str = "<!-- agit:skill-end -->";

macro_rules! subskills {
    ($($name:literal),+ $(,)?) => {
        &[
            $(($name, include_str!(concat!("subskills/", $name, ".md"))),)+
        ]
    };
}

/// The sub-skill of every top-level command. This list must match the `Commands` enum one for
/// one.
pub const SUBSKILLS: &[(&str, &str)] = subskills![
    "branch",
    "cherry-pick",
    "clone",
    "commit",
    "config",
    "diff",
    "distill",
    "doctor",
    "export",
    "fetch",
    "fork",
    "hooks",
    "import",
    "init",
    "log",
    "login",
    "logout",
    "mcp",
    "memory",
    "merge",
    "new",
    "pr",
    "pull",
    "push",
    "rc",
    "repo",
    "resume",
    "revert",
    "run",
    "scan",
    "search",
    "secrets",
    "setup",
    "share",
    "show",
    "status",
    "switch",
    "tag",
    "upgrade",
    "view",
    "whoami",
];

pub fn entrypoint() -> &'static str {
    include_str!("setup_skill.md")
}

pub fn version() -> &'static str {
    config::BUILD_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_subskill_has_frontmatter_and_unique_name() {
        let mut names = std::collections::BTreeSet::new();
        for (name, body) in SUBSKILLS {
            assert!(names.insert(name), "duplicate subskill {name}");
            assert!(body.starts_with("---\n"), "{name} has no frontmatter");
            assert!(body.contains("\nname:"), "{name} has no name");
            assert!(body.contains("\ndescription:"), "{name} has no description");
        }
    }

    #[test]
    fn subskill_manifest_covers_every_top_level_command() {
        // Use clap's generated command definition as the source of truth.  Comparing
        // both complete sets catches both a stale resource entry and a newly added
        // top-level command that forgot to ship a sub-skill.
        let commands = crate::commands::cli_def()
            .get_subcommands()
            .map(|command| command.get_name().to_owned())
            .collect::<std::collections::BTreeSet<_>>();
        let subskills = SUBSKILLS
            .iter()
            .map(|(name, _)| (*name).to_owned())
            .collect::<std::collections::BTreeSet<_>>();

        assert_eq!(
            subskills, commands,
            "sub-skill manifest and top-level Commands are out of sync"
        );
    }
}
