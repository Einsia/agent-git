//! Shell completion stdout stays executable when setup also reports integration results.

use std::path::Path;
use std::process::{Command, Output, Stdio};

fn command(root: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
    command.env_clear();
    for name in ["SystemRoot", "WINDIR", "TEMP", "TMP", "ComSpec"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
        .args(args)
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", root)
        .env("USERPROFILE", root)
        .env("CODEX_HOME", root.join("codex"))
        .env("AGIT_HOME", root.join("agit"))
        .env("AGIT_HUB_URL", "http://127.0.0.1:1")
        .env("NO_COLOR", "1")
        .env("CI", "1")
        .current_dir(root)
        .stdin(Stdio::null());
    command
}

fn success(output: Output) -> Output {
    assert!(output.status.success(), "{output:?}");
    output
}

#[test]
fn ordinary_and_mixed_setup_emit_the_same_script_as_quiet_completion_only() {
    for shell in ["bash", "zsh", "fish"] {
        let root = tempfile::tempdir().unwrap();
        let baseline = success(
            command(root.path(), &["--quiet", "setup", "--completions", shell])
                .output()
                .unwrap(),
        );
        assert!(!baseline.stdout.is_empty());
        assert!(baseline.stderr.is_empty());
        for args in [
            vec!["setup", "--completions", shell],
            vec![
                "setup",
                "--skill",
                "--agents-md",
                "--runtime",
                "codex",
                "--completions",
                shell,
            ],
            vec![
                "setup",
                "--skill",
                "--runtime",
                "cc",
                "--completions",
                shell,
            ],
            vec![
                "setup",
                "--hooks",
                "--runtime",
                "openclaw",
                "--completions",
                shell,
            ],
        ] {
            let output = success(command(root.path(), &args).output().unwrap());
            assert_eq!(output.stdout, baseline.stdout, "{args:?}");
            assert!(String::from_utf8_lossy(&output.stderr).contains("All set"));
        }
        let mut output = command(
            root.path(),
            &["setup", "--agents-md", "--completions", shell],
        );
        let agents = root.path().join("AGENTS.md");
        std::fs::remove_file(&agents).unwrap();
        std::fs::create_dir(&agents).unwrap();
        let output = output.output().unwrap();
        assert_eq!(output.status.code(), Some(4));
        assert_eq!(output.stdout, baseline.stdout);
        assert!(String::from_utf8_lossy(&output.stderr).contains("Setup incomplete"));
    }
}

#[test]
fn json_completion_results_contain_only_script_lines() {
    let root = tempfile::tempdir().unwrap();
    let baseline = success(
        command(root.path(), &["--quiet", "setup", "--completions", "bash"])
            .output()
            .unwrap(),
    );
    let baseline = String::from_utf8(baseline.stdout).unwrap();
    for version in ["1", "2"] {
        let output = success(
            command(
                root.path(),
                &[
                    "--json",
                    "--json-version",
                    version,
                    "setup",
                    "--completions",
                    "bash",
                ],
            )
            .output()
            .unwrap(),
        );
        assert!(output.stderr.is_empty());
        let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let script = document["result"]["lines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|line| line.as_str().unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        let expected = baseline
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(script, expected);
        assert!(
            document["diagnostics"]["stderr"]
                .to_string()
                .contains("All set")
        );
    }
}

#[cfg(unix)]
#[test]
fn mixed_setup_routes_child_notices_to_stderr_and_bash_can_source_the_result() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let bin = root.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let claude = bin.join("claude");
    std::fs::write(
        &claude,
        "#!/bin/sh\nprintf '%s\\n' 'Synthetic MCP registration notice'\n",
    )
    .unwrap();
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
    let output = success(
        command(
            root.path(),
            &[
                "setup",
                "--mcp",
                "--runtime",
                "claude-code",
                "--completions",
                "bash",
            ],
        )
        .env("PATH", &bin)
        .output()
        .unwrap(),
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("Synthetic MCP registration notice"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("Synthetic MCP registration notice"));
    let script = root.path().join("completion.bash");
    std::fs::write(&script, output.stdout).unwrap();
    let output = Command::new("bash")
        .args([
            "--noprofile",
            "--norc",
            "-c",
            "source \"$1\" && complete -p agit",
            "bash",
        ])
        .arg(script)
        .env_remove("BASH_ENV")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("-F _agit agit"));
}
