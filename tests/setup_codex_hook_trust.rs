//! Codex runs a user hook only after the person has approved that exact command, and an
//! unapproved AgentGit hook saves nothing without any sign of it. Setup must therefore leave an
//! approved hook's command alone, and setup and doctor must repeat what Codex says about the hooks
//! it will not run.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

/// A Codex with the hooks feature whose app server answers `hooks/list` with the result in
/// `hooks-list.json` beside it, in the shape Codex's app-server protocol defines.
const FAKE_CODEX: &str = r#"#!/bin/sh
case "$1" in
features) echo "hooks stable true" ;;
app-server)
  while IFS= read -r line; do
    case "$line" in
    *'"id":1,'*) echo '{"id":1,"result":{"codexHome":"/unused"}}' ;;
    *'"id":2,'*) printf '{"id":2,"result":'; tr -d '\n' < "$(dirname "$0")/hooks-list.json"; echo '}' ;;
    esac
  done ;;
esac
"#;

fn executable(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn hook(event: &str, command: &str, enabled: bool, trust: &str) -> serde_json::Value {
    serde_json::json!({
        "key": format!("/h/.codex/hooks.json:{event}:0:0"),
        "eventName": event,
        "handlerType": "command",
        "command": command,
        "source": "user",
        "enabled": enabled,
        "isManaged": false,
        "currentHash": "sha256:0",
        "trustStatus": trust,
    })
}

fn agit(home: &Path, path: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_agit"))
        .args(args)
        .env_clear()
        .env("PATH", path)
        .env("HOME", home)
        .env("CODEX_HOME", home.join(".codex"))
        .env("CLAUDE_CONFIG_DIR", home.join(".claude"))
        .env("AGIT_HOME", home.join(".agit"))
        .env("AGIT_HUB_URL", "http://127.0.0.1:1")
        .env("AGIT_TELEMETRY_DISABLED", "1")
        .env("AGIT_SECRETS_KEYSTORE", "file")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("CI", "1")
        .env("NO_COLOR", "1")
        .current_dir(home)
        .output()
        .unwrap()
}

/// Hooks naming the `agit` a shell runs by name stay byte for byte when setup runs from another
/// copy of agit, so Codex keeps its approval of them; an implementation that points them at the
/// copy running setup changes the command Codex approved, and one that takes npx's cached copy
/// ahead of it on PATH for the person's `agit` does the same during an `npx` install. Codex's verdict on the AgentGit hooks
/// (one changed since approval, one disabled) reaches the person through setup and doctor, while
/// another tool's hook that Codex also lists is not reported as AgentGit's.
#[test]
fn approved_codex_hooks_survive_setup_and_unapproved_ones_are_reported() {
    // Outside the system temporary directory: an `agit` there is one a cleanup removes, and setup
    // never treats it as the person's own.
    let root = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let home = root.path();
    let local_bin = home.join(".local/bin");
    let person_agit = local_bin.join("agit");
    executable(&person_agit, "#!/bin/sh\nexit 0\n");
    let tools = home.join("tools");
    executable(&tools.join("codex"), FAKE_CODEX);
    // An installer run by `npx` puts the copy in npx's cache first on PATH.
    let npx_bin = home.join(".npm/_npx/0/node_modules/.bin");
    executable(&npx_bin.join("agit"), "#!/bin/sh\nexit 0\n");
    let path = format!(
        "{}:{}:{}:/usr/bin:/bin",
        npx_bin.display(),
        local_bin.display(),
        tools.display()
    );

    let ingest = format!("{} hooks ingest --runtime codex", person_agit.display());
    let settle = format!("{} hooks settle --runtime codex", person_agit.display());
    let hooks_file = home.join(".codex/hooks.json");
    let approved = serde_json::to_string_pretty(&serde_json::json!({
        "hooks": {
            "SessionStart": [{"hooks": [{"command": ingest, "type": "command"}]}],
            "Stop": [{"hooks": [{"command": settle, "type": "command"}]}],
        }
    }))
    .unwrap();
    std::fs::create_dir_all(hooks_file.parent().unwrap()).unwrap();
    std::fs::write(&hooks_file, &approved).unwrap();
    std::fs::write(
        tools.join("hooks-list.json"),
        serde_json::to_string(&serde_json::json!({"data": [{
            "cwd": home,
            "hooks": [
                hook("sessionStart", &ingest, true, "modified"),
                hook("stop", &settle, false, "trusted"),
                hook("userPromptSubmit", "/usr/bin/other-tool", true, "untrusted"),
            ],
            "warnings": [],
            "errors": [],
        }]}))
        .unwrap(),
    )
    .unwrap();

    let setup = agit(home, &path, &["setup", "--hooks", "--runtime", "codex"]);
    assert!(setup.status.success(), "{setup:?}");
    assert_eq!(std::fs::read_to_string(&hooks_file).unwrap(), approved);
    let said = String::from_utf8_lossy(&setup.stderr).into_owned()
        + &String::from_utf8_lossy(&setup.stdout);
    for expected in ["SessionStart", "Stop", "/hooks"] {
        assert!(said.contains(expected), "{expected}: {said}");
    }
    assert!(!said.contains("other-tool"), "{said}");

    let doctor = agit(home, &path, &["doctor"]);
    let said = String::from_utf8_lossy(&doctor.stdout).into_owned()
        + &String::from_utf8_lossy(&doctor.stderr);
    let row = said
        .lines()
        .find(|line| line.contains("codex hooks"))
        .unwrap_or_else(|| panic!("{said}"));
    for expected in ["SessionStart", "Stop", "/hooks"] {
        assert!(row.contains(expected), "{expected}: {row}");
    }
}
