use std::path::Path;
use std::process::{Command, Output};

fn config(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_agit"))
        .arg("config")
        .args(args)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", root)
        .env("USERPROFILE", root)
        .env("AGIT_HOME", root.join("agit"))
        .env("CI", "1")
        .env("AGIT_TUI", "0")
        .env("NO_COLOR", "1")
        .current_dir(root)
        .output()
        .unwrap()
}

fn success(root: &Path, args: &[&str]) -> String {
    let output = config(root, args);
    assert!(
        output.status.success(),
        "config failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn automatic_settlement_display_distinguishes_default_and_stored_values() {
    let root = tempfile::tempdir().unwrap();
    assert_eq!(success(root.path(), &["commit.auto"]).trim(), "(unset)");
    let before = std::fs::read(root.path().join("agit/config.json")).ok();
    assert!(
        success(root.path(), &["--list"])
            .lines()
            .any(|line| line == "commit.auto = true (default)")
    );
    assert_eq!(
        std::fs::read(root.path().join("agit/config.json")).ok(),
        before,
        "listing a default must not persist it"
    );

    for value in ["false", "true"] {
        success(root.path(), &["commit.auto", value]);
        assert_eq!(success(root.path(), &["commit.auto"]).trim(), value);
        let expected = format!("commit.auto = {value}");
        assert!(
            success(root.path(), &["--list"])
                .lines()
                .any(|line| line == expected)
        );
    }

    success(root.path(), &["--unset", "commit.auto"]);
    assert_eq!(success(root.path(), &["commit.auto"]).trim(), "(unset)");
    assert!(
        success(root.path(), &["--list"])
            .lines()
            .any(|line| line == "commit.auto = true (default)")
    );
}
