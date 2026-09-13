//! Skill installation reports each runtime as a bundle while preserving actionable failures.

use std::path::Path;
use std::process::{Command, Output, Stdio};

fn setup(root: &Path) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
    command.env_clear();
    for name in ["SystemRoot", "WINDIR", "TEMP", "TMP", "ComSpec"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
        .args(["setup", "--skill", "--runtime", "codex"])
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", root)
        .env("USERPROFILE", root)
        .env("CODEX_HOME", root.join("codex"))
        .env("AGIT_HOME", root.join("agit"))
        .env("AGIT_HUB_URL", "http://127.0.0.1:1")
        .env("NO_COLOR", "1")
        .current_dir(root)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn summary(output: Output, status: &str) {
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains(&format!("skill codex {status} →")),
        "{stdout}"
    );
    assert_eq!(
        stdout
            .lines()
            .filter(|line| line.contains("skill codex"))
            .count(),
        1,
        "{stdout}"
    );
    assert!(!stdout.contains("reference "), "{stdout}");
    assert!(!stdout.contains("entrypoint"), "{stdout}");
    assert!(!stdout.contains("version"), "{stdout}");
    assert!(stdout.contains("All set"), "{stdout}");
}

#[test]
fn install_repeat_and_stale_cleanup_use_one_runtime_summary() {
    let root = tempfile::tempdir().unwrap();
    summary(setup(root.path()), "updated");
    let skill = root.path().join("codex/skills/agit");
    assert_eq!(
        std::fs::read_to_string(skill.join("SKILL.md")).unwrap(),
        include_str!("../src/commands/setup_skill.md")
    );
    summary(setup(root.path()), "is up to date");
    let refs = skill.join("references/commands");
    std::fs::write(refs.join("retired-command.md"), "Retired command.").unwrap();
    std::fs::write(refs.join("user.txt"), "User notes.").unwrap();
    summary(setup(root.path()), "updated");
    assert!(!refs.join("retired-command.md").exists());
    assert_eq!(
        std::fs::read_to_string(refs.join("user.txt")).unwrap(),
        "User notes."
    );
    summary(setup(root.path()), "is up to date");
}

#[test]
fn failed_file_keeps_its_path_and_does_not_report_bundle_success() {
    let root = tempfile::tempdir().unwrap();
    summary(setup(root.path()), "updated");
    let entrypoint = root.path().join("codex/skills/agit/SKILL.md");
    std::fs::remove_file(&entrypoint).unwrap();
    std::fs::create_dir(&entrypoint).unwrap();
    let output = setup(root.path());
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("skill codex entrypoint write failed"),
        "{stderr}"
    );
    assert!(
        stderr.contains(&entrypoint.display().to_string()),
        "{stderr}"
    );
    assert!(stderr.contains("Setup incomplete"), "{stderr}");
    assert!(!stdout.contains("All set"), "{stdout}");
    assert!(!stdout.contains("skill codex"), "{stdout}");
}
