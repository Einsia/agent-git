//! Skill refresh preserves user instructions and migrates only existing integrations.

use std::path::Path;
use std::process::{Command, Output, Stdio};

const BLOCK: &str = "<!-- agit:skill-begin -->\n<!-- agit:skill-version:old -->\nOld skill\n<!-- agit:skill-end -->\n";

fn setup(root: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
    command.env_clear();
    for name in ["SystemRoot", "WINDIR", "TEMP", "TMP", "ComSpec", "PATH"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    std::fs::create_dir_all(root.join("project")).unwrap();
    command
        .args(["--no-tui", "setup", "--skill"])
        .args(args)
        .env("HOME", root)
        .env("USERPROFILE", root)
        .env("CODEX_HOME", root.join("custom-codex"))
        .env("AGIT_HOME", root.join("agit"))
        .env("AGIT_HUB_URL", "http://127.0.0.1:1")
        .env("NO_COLOR", "1")
        .current_dir(root.join("project"))
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn install_stub(root: &Path) -> std::path::PathBuf {
    let skill = root.join("custom-codex/skills/agit");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(skill.join("SKILL.md"), "Old native skill").unwrap();
    skill
}

#[test]
fn runtime_filtered_refresh_preserves_other_runtime_legacy_until_its_bundle_is_installed() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let skill = install_stub(root);
    let original = format!("# User instructions\n{BLOCK}Keep this.\n");
    std::fs::write(root.join("AGENTS.md"), &original).unwrap();
    std::fs::create_dir(root.join("project")).unwrap();
    std::fs::write(root.join("project/AGENTS.md"), "Project instructions\n").unwrap();
    std::fs::write(root.join("custom-codex/hooks.json"), "User hooks\n").unwrap();
    std::fs::write(root.join("custom-codex/config.toml"), "# User config\n").unwrap();
    let output = setup(root, &["--installed-only", "--runtime", "codex"]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        std::fs::read_to_string(root.join("AGENTS.md")).unwrap(),
        original
    );
    assert!(!root.join(".cursor/skills/agit").exists());
    let output = setup(root, &["--installed-only"]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        std::fs::read_to_string(root.join("AGENTS.md")).unwrap(),
        "# User instructions\nKeep this.\n"
    );
    assert_eq!(
        std::fs::read_to_string(skill.join("SKILL.md")).unwrap(),
        include_str!("../src/commands/setup_skill.md")
    );
    assert!(skill.join("references/commands/commit.md").is_file());
    assert_eq!(
        std::fs::read_to_string(root.join(".cursor/skills/agit/SKILL.md")).unwrap(),
        include_str!("../src/commands/setup_skill.md")
    );
    for (path, content) in [
        ("project/AGENTS.md", "Project instructions\n"),
        ("custom-codex/hooks.json", "User hooks\n"),
        ("custom-codex/config.toml", "# User config\n"),
    ] {
        assert_eq!(std::fs::read_to_string(root.join(path)).unwrap(), content);
    }
    let backups: Vec<_> = std::fs::read_dir(root)
        .unwrap()
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("AGENTS.md.agit-backup-")
        })
        .collect();
    assert_eq!(backups.len(), 1);
    assert_eq!(
        std::fs::read_to_string(backups[0].path()).unwrap(),
        original
    );
    let output = setup(root, &["--installed-only"]);
    assert!(output.status.success(), "{output:?}");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("removed"));
    for runtime in [".claude", ".config/opencode"] {
        assert!(!root.join(runtime).exists(), "{runtime}");
    }
}

#[test]
fn refresh_without_integrations_does_not_install_any() {
    let root = tempfile::tempdir().unwrap();
    let output = setup(root.path(), &["--installed-only"]);
    assert!(output.status.success(), "{output:?}");
    for runtime in ["custom-codex", ".claude", ".cursor", ".config/opencode"] {
        assert!(!root.path().join(runtime).exists(), "{runtime}");
    }
    assert!(!root.path().join("project/AGENTS.md").exists());
}

#[test]
fn refresh_migrates_a_home_legacy_install_without_a_native_bundle() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("AGENTS.md"), BLOCK).unwrap();
    let output = setup(root.path(), &["--installed-only"]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        std::fs::read_to_string(root.path().join("AGENTS.md")).unwrap(),
        ""
    );
    assert!(root.path().join(".cursor/skills/agit/SKILL.md").is_file());
    assert!(!root.path().join("custom-codex").exists());
}

#[test]
fn failed_native_refresh_preserves_the_home_manual() {
    let root = tempfile::tempdir().unwrap();
    let skill = root.path().join(".cursor/skills/agit");
    std::fs::create_dir_all(skill.join("SKILL.md")).unwrap();
    std::fs::write(root.path().join("AGENTS.md"), BLOCK).unwrap();
    let output = setup(root.path(), &["--installed-only", "--runtime", "cursor"]);
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    assert_eq!(
        std::fs::read_to_string(root.path().join("AGENTS.md")).unwrap(),
        BLOCK
    );
}

#[test]
fn legacy_codex_global_manual_migrates_with_a_custom_codex_home() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join(".codex")).unwrap();
    let manual = BLOCK
        .replace("agit:skill-begin", "agit:begin")
        .replace("agit:skill-end", "agit:end");
    std::fs::write(
        root.path().join(".codex/AGENTS.md"),
        format!("User rules\n{manual}"),
    )
    .unwrap();
    let output = setup(root.path(), &["--installed-only"]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        std::fs::read_to_string(root.path().join(".codex/AGENTS.md")).unwrap(),
        "User rules\n"
    );
    assert!(
        root.path()
            .join("custom-codex/skills/agit/SKILL.md")
            .is_file()
    );
}

/// An agit section in a home-level instruction file reaches every conversation under the home
/// folder, also ones unrelated to AgentGit, so a skill refresh removes it from each such file and
/// keeps the user's own text and a backup. A refresh that left one would keep agit in every
/// Codex and Claude Code prompt.
#[test]
fn a_skill_refresh_removes_agit_sections_from_home_level_instruction_files() {
    let root = tempfile::tempdir().unwrap();
    let section =
        "<!-- agit:begin -->\n## Session version control (agit)\nrules\n<!-- agit:end -->\n";
    let files = ["AGENTS.md", "CLAUDE.md", "custom-codex/AGENTS.md"];
    for file in files {
        let path = root.path().join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("User rules\n{section}")).unwrap();
    }
    let output = setup(root.path(), &["--installed-only"]);
    assert!(output.status.success(), "{output:?}");
    for file in files {
        let path = root.path().join(file);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "User rules\n",
            "{file}"
        );
        let backups = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter(|entry| {
                entry
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains(".agit-backup-")
            })
            .count();
        assert!(backups >= 1, "{file}");
    }
}

/// The project section is written only on request and only at a Git repository's top level: a
/// plain setup writes no AGENTS.md wherever it runs, and `--agents-md` outside a repository
/// writes nothing and says why. Writing it into the folder setup happened to run in left a stray
/// AGENTS.md in unrelated projects and the home folder (CLI #198, #211).
#[test]
fn the_agents_md_section_is_written_only_at_a_repository_top_level_on_request() {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    let refused = setup(root.path(), &["--agents-md"]);
    assert_eq!(refused.status.code(), Some(4), "{refused:?}");
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("Git repository's top level"),
        "{refused:?}"
    );
    assert!(!project.join("AGENTS.md").exists());

    let git = Command::new("git")
        .args(["init", "-q"])
        .current_dir(&project)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(git.status.success());
    let mut plain = Command::new(env!("CARGO_BIN_EXE_agit"));
    plain.env_clear();
    for name in ["SystemRoot", "WINDIR", "TEMP", "TMP", "ComSpec", "PATH"] {
        if let Some(value) = std::env::var_os(name) {
            plain.env(name, value);
        }
    }
    let plain = plain
        .args(["--no-tui", "setup", "--yes"])
        .env("HOME", root.path())
        .env("USERPROFILE", root.path())
        .env("CODEX_HOME", root.path().join("custom-codex"))
        .env("AGIT_HOME", root.path().join("agit"))
        .env("AGIT_HUB_URL", "http://127.0.0.1:1")
        .env("AGIT_TELEMETRY_DISABLED", "1")
        .env("NO_COLOR", "1")
        .current_dir(&project)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!project.join("AGENTS.md").exists(), "{plain:?}");
    assert!(!root.path().join("AGENTS.md").exists(), "{plain:?}");

    let written = setup(root.path(), &["--agents-md"]);
    assert!(written.status.success(), "{written:?}");
    let text = std::fs::read_to_string(project.join("AGENTS.md")).unwrap();
    assert!(text.contains("Session version control (agit)"), "{text}");
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&written.stdout),
        String::from_utf8_lossy(&written.stderr)
    );
    assert!(
        said.contains("tells coding agents in this project"),
        "{said}"
    );
}
