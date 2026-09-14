//! Native integrations translate lifecycle events into the common capture protocol.

use super::{SetupReport, ui, write_if_changed};
use anyhow::{Context, ensure};
use serde_json::{Value, json};
use std::{
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

fn report(label: &str, result: crate::Result<()>) -> SetupReport {
    match result {
        Ok(()) => {
            ui::info(format_args!(
                "  {} {label}",
                ui::ok(ui::theme::symbols().check)
            ));
            SetupReport::item()
        }
        Err(error) => {
            ui::warning(&format!("cannot configure {label}: {error:#}"));
            SetupReport::failure()
        }
    }
}

pub(super) fn hermes(exe: &str, kind: &str) -> SetupReport {
    let result = (|| {
        let python = crate::adapter::hermes::python()
            .context("install Hermes before configuring its integration")?;
        let mut child = Command::new(python)
            .args(["-c", include_str!("hermes.py")])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        child
            .stdin
            .take()
            .context("Hermes configuration stdin is unavailable")?
            .write_all(serde_json::to_string(&json!({"exe":exe,"kind":kind}))?.as_bytes())?;
        let output = child.wait_with_output()?;
        ensure!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    })();
    let result = report(&format!("Hermes {kind}"), result);
    if kind == "hooks" && result.succeeded() {
        ui::info(format_args!(
            "  Hermes asks you to trust these commands when it first loads the hooks."
        ));
    }
    result
}

pub(super) fn workbuddy_mcp(exe: &str) -> SetupReport {
    let Ok(root) = crate::adapter::workbuddy::home() else {
        return SetupReport::failure();
    };
    let path = [
        root.join(".mcp.json"),
        root.join("mcp.json"),
        root.join(".codebuddy.json"),
    ]
    .into_iter()
    .find(|path| path.exists())
    .unwrap_or_else(|| root.join(".mcp.json"));
    super::upsert_json_mcp(
        &path,
        "mcpServers",
        "agit",
        json!({"command":exe,"args":["mcp"]}),
    )
}

pub(super) fn openclaw(exe: &str) -> SetupReport {
    let result = (|| {
        let root = crate::adapter::openclaw::home()?;
        openclaw_at(&root, exe)
    })();
    report("OpenClaw lifecycle plugin", result)
}

fn openclaw_at(root: &Path, exe: &str) -> crate::Result<()> {
    let path = root.join("openclaw.json");
    let mut config: Value = match std::fs::read_to_string(&path) {
        Ok(raw) => {
            serde_json::from_str(&raw).context("OpenClaw configuration is not valid JSON")?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        config.is_object(),
        "OpenClaw configuration must be an object"
    );
    let plugins = config
        .as_object_mut()
        .unwrap()
        .entry("plugins")
        .or_insert_with(|| json!({}));
    ensure!(plugins.is_object(), "OpenClaw plugins must be an object");
    let plugins = plugins.as_object_mut().unwrap();
    // An absent or empty native allowlist permits discovery; adding one would disable other plugins.
    if let Some(allow) = plugins.get_mut("allow") {
        ensure!(
            allow.is_array(),
            "OpenClaw plugin allowlist must be an array"
        );
        let allow = allow.as_array_mut().unwrap();
        if !allow.is_empty() && !allow.iter().any(|id| id == "agit") {
            allow.push(json!("agit"));
        }
    }
    let entries = plugins.entry("entries").or_insert_with(|| json!({}));
    ensure!(
        entries.is_object(),
        "OpenClaw plugin entries must be an object"
    );
    entries["agit"] = json!({"enabled":true,"hooks":{"allowConversationAccess":true,"allowPromptInjection":true},"config":{"command":exe}});
    let directory = root.join("extensions/agit");
    if directory.exists() {
        let package: Value =
            serde_json::from_str(&std::fs::read_to_string(directory.join("package.json"))?)?;
        ensure!(
            package["name"] == "@einsia/agentgit-openclaw",
            "another plugin owns the OpenClaw agit directory"
        );
    }
    let files = [
        ("package.json", json!({"name":"@einsia/agentgit-openclaw","version":env!("CARGO_PKG_VERSION"),"type":"module","openclaw":{"extensions":["./index.mjs"]}}).to_string()),
        ("openclaw.plugin.json", json!({"id":"agit","name":"AgentGit","configSchema":{"type":"object","additionalProperties":false,"required":["command"],"properties":{"command":{"type":"string","minLength":1}}}}).to_string()),
        ("index.mjs", include_str!("openclaw.mjs").into()),
    ];
    for (name, body) in files {
        ensure!(
            write_if_changed(&directory.join(name), &body, "OpenClaw AgentGit plugin").succeeded(),
            "OpenClaw plugin file was not written"
        );
    }
    ensure!(
        write_if_changed(
            &path,
            &format!("{}\n", serde_json::to_string_pretty(&config)?),
            "OpenClaw plugin configuration"
        )
        .succeeded(),
        "OpenClaw plugin configuration was not written"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn plugin_setup_keeps_unrestricted_native_discovery_unrestricted() {
        for plugins in [
            json!({"entries":{"existing":{"enabled":true}}}),
            json!({"allow":[]}),
        ] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("openclaw.json");
            std::fs::write(&path, json!({"plugins":plugins}).to_string()).unwrap();
            openclaw_at(root.path(), "/synthetic/agit").unwrap();
            let config: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
            assert_eq!(config["plugins"].get("allow"), plugins.get("allow"));
            assert_eq!(config["plugins"]["entries"]["agit"]["enabled"], true);
        }
    }

    #[test]
    fn plugin_setup_preserves_unrelated_config_and_rejects_foreign_ownership() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("openclaw.json");
        std::fs::write(&path,r#"{"models":{"opaque":"unchanged"},"plugins":{"allow":["existing"],"entries":{"existing":{"enabled":true}}}}"#).unwrap();
        openclaw_at(root.path(), "/workspace with spaces/agit").unwrap();
        let before = std::fs::read(&path).unwrap();
        openclaw_at(root.path(), "/workspace with spaces/agit").unwrap();
        assert_eq!(before, std::fs::read(&path).unwrap());
        let config: Value = serde_json::from_slice(&before).unwrap();
        assert_eq!(config["models"]["opaque"], "unchanged");
        assert_eq!(config["plugins"]["allow"], json!(["existing", "agit"]));
        std::fs::write(
            root.path().join("extensions/agit/package.json"),
            r#"{"name":"foreign"}"#,
        )
        .unwrap();
        assert!(openclaw_at(root.path(), "/changed").is_err());
        assert_eq!(before, std::fs::read(&path).unwrap());
    }
}
