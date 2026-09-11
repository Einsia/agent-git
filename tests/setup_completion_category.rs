//! Invalid completion shells are argument errors before any requested integration is installed.

use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn inventory(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    walkdir::WalkDir::new(root)
        .into_iter()
        .map(|entry| {
            let entry = entry.unwrap();
            assert!(!entry.file_type().is_symlink());
            let bytes = entry
                .file_type()
                .is_file()
                .then(|| fs::read(entry.path()).unwrap());
            (entry.path().strip_prefix(root).unwrap().to_owned(), bytes)
        })
        .collect()
}

#[test]
fn invalid_completion_shells_refuse_before_startup_or_integration_writes() {
    for mode in ["human", "quiet", "json1", "json2"] {
        for mixed in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let home = root.path().join("home");
            let work = root.path().join("work");
            for directory in [home.join(".claude"), work.clone(), root.path().join("bin")] {
                fs::create_dir_all(directory).unwrap();
            }
            fs::write(home.join(".claude/settings.json"), "{\"synthetic\":true}\n").unwrap();
            fs::write(work.join("AGENTS.md"), "Synthetic project instructions.\n").unwrap();
            let before = inventory(root.path());
            let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
            command.env_clear();
            for name in ["SystemRoot", "WINDIR", "ComSpec", "PATHEXT"] {
                if let Some(value) = std::env::var_os(name) {
                    command.env(name, value);
                }
            }
            command
                .env("PATH", root.path().join("bin"))
                .env("HOME", &home)
                .env("USERPROFILE", &home)
                .env("CODEX_HOME", home.join(".codex"))
                .env("XDG_CONFIG_HOME", home.join(".config"))
                .env("AGIT_HOME", root.path().join("agit"))
                .env("AGIT_HUB_URL", "http://127.0.0.1:1")
                .env("CI", "1")
                .env("NO_COLOR", "1")
                .current_dir(&work)
                .stdin(Stdio::null());
            if mode == "quiet" {
                command.arg("--quiet");
            }
            if let Some(version) = mode.strip_prefix("json") {
                command.args(["--json", "--json-version", version]);
            }
            command.arg("setup");
            if mixed {
                command.args([
                    "--runtime",
                    "claude-code",
                    "--hooks",
                    "--skill",
                    "--mcp",
                    "--agents-md",
                ]);
            }
            let output = command
                .args(["--completions", "unsupported-shell"])
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(2), "{mode}/{mixed}: {output:?}");
            let error = if let Some(version) = mode.strip_prefix("json") {
                assert!(output.stderr.is_empty(), "{output:?}");
                let value: Value = serde_json::from_slice(&output.stdout).unwrap();
                assert_eq!(value["schema_version"], version.parse::<u64>().unwrap());
                assert_eq!(value["command"], "setup");
                assert_eq!(value["exit_code"], 2);
                assert_eq!(value["ok"], false);
                value["diagnostics"]["stderr"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|entry| entry["message"].as_str().unwrap())
                    .collect::<Vec<_>>()
                    .join("\n")
            } else {
                assert!(output.stdout.is_empty(), "{output:?}");
                String::from_utf8(output.stderr).unwrap()
            };
            assert!(error.contains("unsupported-shell"), "{error}");
            assert!(error.contains("--completions"), "{error}");
            assert!(!error.contains("Setup incomplete"), "{error}");
            assert_eq!(
                inventory(root.path()),
                before,
                "{mode}/{mixed}: files changed"
            );
            assert!(!root.path().join("agit").exists(), "startup must not run");
        }
    }
}

#[test]
fn supported_completion_shells_keep_the_existing_argument_contract() {
    for shell in ["bash", "zsh", "fish"] {
        let matches = agit::commands::cli_def()
            .try_get_matches_from(["agit", "setup", "--completions", shell])
            .unwrap();
        let setup = matches.subcommand_matches("setup").unwrap();
        assert_eq!(setup.get_one::<String>("completions").unwrap(), shell);
    }
}
