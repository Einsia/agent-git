//! Claude Code's agit MCP registration must reach every folder: setup asks Claude Code for user
//! scope, retires only the project-local copies that entry fully replaces, and doctor names what
//! still keeps a folder from it.
#![cfg(unix)]

use serde_json::json;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// Records every call with its working directory and, like Claude Code, writes the user entry
/// into the configuration file when asked to add one.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
printf '%s|%s\n' "$(pwd -P)" "$*" >> "$FAKE_CLAUDE_LOG"
case "$*" in "mcp add --scope user "*) cp "$FAKE_CLAUDE_AFTER" "$HOME/.claude.json" ;; esac
"#;

struct Lab {
    root: PathBuf,
    _dir: tempfile::TempDir,
}

impl Lab {
    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    fn agit(&self, args: &[&str]) -> Output {
        let path = format!(
            "{}:{}",
            self.path("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        Command::new(env!("CARGO_BIN_EXE_agit"))
            .args(args)
            .env_clear()
            .env("PATH", path)
            .env("HOME", self.path("home"))
            .env("AGIT_HOME", self.path("agit"))
            .env("AGIT_HUB_URL", "http://127.0.0.1:1")
            .env("AGIT_SECRETS_KEYSTORE", "file")
            .env("CI", "1")
            .env("NO_COLOR", "1")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", self.path("absent-gitconfig"))
            .env("FAKE_CLAUDE_LOG", self.path("claude.log"))
            .env("FAKE_CLAUDE_AFTER", self.path("after.json"))
            .current_dir(self.path("work"))
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn calls(&self) -> Vec<String> {
        fs::read_to_string(self.path("claude.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn doctor_row(lab: &Lab) -> String {
    let output = text(&lab.agit(&["doctor"]));
    output
        .lines()
        .find(|line| line.contains("mcp claude-code"))
        .unwrap_or_else(|| panic!("no Claude Code MCP row:\n{output}"))
        .to_owned()
}

/// A setup that registers without `--scope user` fails the argument check; one that removes
/// every local `agit` entry deletes the folder-owned one that carries its own environment; one
/// that adds whenever a call succeeds registers twice on the second run. A doctor that read
/// project entries as user scope would call the local-only state healthy.
#[test]
fn registration_reaches_every_folder_and_keeps_folder_owned_entries() {
    let dir = tempfile::tempdir().unwrap();
    let lab = Lab {
        root: dir.path().canonicalize().unwrap(),
        _dir: dir,
    };
    for name in ["bin", "home", "work", "copy", "owned"] {
        fs::create_dir_all(lab.path(name)).unwrap();
    }
    let claude = lab.path("bin/claude");
    fs::write(&claude, FAKE_CLAUDE).unwrap();
    fs::set_permissions(&claude, fs::Permissions::from_mode(0o755)).unwrap();

    let server = json!({"type": "stdio", "command": "/synthetic/agit", "args": ["mcp"], "env": {}});
    let (copy, owned, gone) = (lab.path("copy"), lab.path("owned"), lab.path("gone"));
    let key = |path: &Path| path.to_str().unwrap().to_owned();
    let projects = json!({
        key(&copy): {"mcpServers": {"agit": server}},
        key(&owned): {"mcpServers": {"agit": {
            "command": "/synthetic/agit", "args": ["mcp"], "env": {"AGIT_HOME": "/synthetic"}
        }}},
        key(&gone): {"mcpServers": {"agit": {"command": "/removed/agit", "args": ["mcp"]}}},
    });
    fs::write(
        lab.path("home/.claude.json"),
        json!({"projects": projects}).to_string(),
    )
    .unwrap();
    fs::write(
        lab.path("after.json"),
        json!({"mcpServers": {"agit": server}, "projects": projects}).to_string(),
    )
    .unwrap();

    let row = doctor_row(&lab);
    assert!(row.contains("registered only for"), "{row}");
    assert!(
        row.contains("agit setup --runtime claude-code --mcp"),
        "{row}"
    );

    let setup = lab.agit(&["setup", "--mcp", "--runtime", "claude-code"]);
    assert!(setup.status.success(), "{}", text(&setup));
    let calls = lab.calls();
    let work = lab.path("work");
    let (folder, add) = calls[0].split_once('|').unwrap();
    assert_eq!(Path::new(folder), work);
    assert!(add.starts_with("mcp add --scope user agit -- "), "{add}");
    assert!(add.ends_with(" mcp"), "{add}");
    assert_eq!(
        calls[1..],
        [format!("{}|mcp remove --scope local agit", copy.display())]
    );
    let output = text(&setup);
    assert!(output.contains(&owned.display().to_string()), "{output}");
    assert!(
        output.contains("claude mcp remove agit --scope local"),
        "{output}"
    );

    let row = doctor_row(&lab);
    assert!(row.contains("overrides it in"), "{row}");
    assert!(row.contains(&owned.display().to_string()), "{row}");
    for unaffected in [&copy, &gone] {
        assert!(!row.contains(&unaffected.display().to_string()), "{row}");
    }

    let again = lab.agit(&["setup", "--mcp", "--runtime", "claude-code"]);
    assert!(again.status.success(), "{}", text(&again));
    assert_eq!(
        lab.calls()
            .iter()
            .filter(|call| call.contains("|mcp add "))
            .count(),
        1
    );
}
