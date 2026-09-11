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
    "file",
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
    "open",
    "pr",
    "pull",
    "push",
    "rc",
    "repo",
    "resume",
    "revert",
    "scan",
    "search",
    "secrets",
    "setup",
    "share",
    "show",
    "status",
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
    use clap::Parser as _;

    fn command_examples(body: &str) -> Vec<(usize, Vec<String>)> {
        let mut examples = Vec::new();
        let mut bash = false;
        let mut synopsis = false;
        let mut pending = String::new();
        let mut start = 0;
        for (line_index, line) in body.lines().enumerate() {
            let line = line.trim();
            if line.starts_with("## ") {
                synopsis = line == "## Synopsis";
            }
            if line.starts_with("```") {
                bash = line == "```bash";
                continue;
            }
            if !bash || synopsis || (pending.is_empty() && !line.starts_with("agit ")) {
                continue;
            }
            if pending.is_empty() {
                start = line_index + 1;
            }
            if let Some(part) = line.strip_suffix('\\') {
                pending.push_str(part);
                pending.push(' ');
                continue;
            }
            pending.push_str(line);
            let words = shlex::split(&pending).expect("command example must have balanced quoting");
            examples.push((
                start,
                words
                    .into_iter()
                    .take_while(|word| !matches!(word.as_str(), ">" | ">>" | "<" | "|" | "&&"))
                    .collect(),
            ));
            pending.clear();
        }
        assert!(pending.is_empty(), "unfinished command example");
        examples
    }

    #[test]
    fn documented_shell_examples_parse_without_running_commands() {
        let mut commands = std::collections::BTreeSet::new();
        for (name, body) in
            std::iter::once(("SKILL", entrypoint())).chain(SUBSKILLS.iter().copied())
        {
            for (line, words) in command_examples(body) {
                let cli = crate::commands::Cli::try_parse_from(&words)
                    .unwrap_or_else(|error| panic!("{name}:{line}: {words:?}\n{error}"));
                if let Some(command) = cli.command {
                    commands.insert(crate::commands::command_name(&command));
                }
            }
        }
        for command in [
            "import", "commit", "search", "new", "resume", "open", "merge",
        ] {
            assert!(
                commands.contains(command),
                "no checked example for {command}"
            );
        }
    }

    #[test]
    fn project_injection_examples_parse_without_running_commands() {
        let section = include_str!("setup_agents_section.md");
        for example in section.split('`').skip(1).step_by(2) {
            if example.starts_with("agit ") {
                let words = shlex::split(example).unwrap();
                crate::commands::Cli::try_parse_from(&words)
                    .unwrap_or_else(|error| panic!("{example}: {error}"));
            }
        }
    }

    #[test]
    fn descriptions_quote_yaml_mapping_delimiters() {
        for (name, body) in
            std::iter::once(("SKILL", entrypoint())).chain(SUBSKILLS.iter().copied())
        {
            let frontmatter = body
                .strip_prefix("---\n")
                .unwrap()
                .split("\n---")
                .next()
                .unwrap();
            let description = frontmatter
                .lines()
                .find_map(|line| line.strip_prefix("description: "))
                .unwrap_or_else(|| panic!("{name}: description must be a single scalar"));
            if description.starts_with('"') {
                serde_json::from_str::<String>(description).unwrap_or_else(|error| {
                    panic!("{name}: malformed quoted description: {error}")
                });
            } else {
                assert!(
                    !description.contains(": "),
                    "{name}: quote the YAML mapping delimiter"
                );
            }
        }
    }

    #[test]
    fn example_reader_preserves_ref_selectors_and_quoted_values() {
        let body = "## Synopsis\n```bash\nagit commit [options]\n```\n\
                    ## Examples\n```bash\n  agit merge --into me/repo@review \\\n                    summary -m \"Keep #3 and its evidence\" # explanation\n\
                    agit view me/repo@topic#3 --json > /tmp/view.json\n```";
        let examples = command_examples(body);
        assert_eq!(
            examples.iter().map(|(_, words)| words).collect::<Vec<_>>(),
            [
                &[
                    "agit",
                    "merge",
                    "--into",
                    "me/repo@review",
                    "summary",
                    "-m",
                    "Keep #3 and its evidence"
                ][..],
                &["agit", "view", "me/repo@topic#3", "--json"][..],
            ]
        );
    }

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
