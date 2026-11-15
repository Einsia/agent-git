//! Native integrations translate lifecycle events into the common capture protocol.

use super::{SetupReport, ui, write_if_changed};
use anyhow::{Context, ensure};
use serde_json::{Value, json};
use std::{io::Write, path::Path, process::Stdio};

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

#[cfg(unix)]
pub(super) fn claude() -> SetupReport {
    report(
        "Claude native shared control",
        crate::rc::native_claude::install_plugin(),
    )
}

pub(super) fn hermes(exe: &str, kind: &str) -> SetupReport {
    let result = (|| {
        let mut child = crate::adapter::hermes::python_command(include_str!("hermes.py"))?
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

/// Gemini CLI reads MCP servers from the same user settings its hooks live in.
pub(super) fn gemini_mcp(exe: &str) -> SetupReport {
    let Ok(root) = crate::adapter::gemini_cli::home() else {
        return SetupReport::failure();
    };
    super::upsert_json_mcp(
        &root.join("settings.json"),
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

/// The first line of agit's OpenCode plugin; a file that does not start with it is not agit's.
const OPENCODE_MARKER: &str = "// agentgit-opencode-plugin ";

/// Where agit's OpenCode plugin goes: one file in OpenCode's global plugin folder, which OpenCode
/// loads in every folder. OpenCode reads its configuration from `$XDG_CONFIG_HOME/opencode` when
/// that is an absolute path and from `~/.config/opencode` otherwise; a plugin written anywhere
/// else is never loaded, and every turn end it should report goes unnoticed.
pub(crate) fn opencode_plugin_path() -> Option<std::path::PathBuf> {
    opencode_plugin_path_in(
        std::env::var_os("XDG_CONFIG_HOME").map(std::path::PathBuf::from),
        crate::infra::config::user_home(),
    )
}

fn opencode_plugin_path_in(
    xdg_config: Option<std::path::PathBuf>,
    home: Option<std::path::PathBuf>,
) -> Option<std::path::PathBuf> {
    xdg_config
        .filter(|path| path.is_absolute())
        .or_else(|| home.map(|home| home.join(".config")))
        .map(|config| config.join("opencode/plugin/agit.js"))
}

/// agit's OpenCode plugin for the agit at `exe`.
fn opencode_plugin(exe: &str) -> String {
    include_str!("opencode.mjs")
        .replace("__AGIT_PLUGIN_VERSION__", env!("CARGO_PKG_VERSION"))
        .replace(
            "__AGIT_EXE__",
            &exe.replace('\\', "\\\\").replace('"', "\\\""),
        )
}

/// What is at `path`, where agit's OpenCode plugin goes.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum OpencodePlugin {
    Absent,
    /// agit's plugin, the one this agit writes.
    Current,
    /// agit's plugin from another version or for an agit at another path.
    Older,
    /// A file there that agit did not write.
    Foreign,
}

pub(crate) fn opencode_plugin_state(path: &Path, exe: &str) -> OpencodePlugin {
    match std::fs::read_to_string(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => OpencodePlugin::Absent,
        Ok(text) if text == opencode_plugin(exe) => OpencodePlugin::Current,
        Ok(text) if text.starts_with(OPENCODE_MARKER) => OpencodePlugin::Older,
        _ => OpencodePlugin::Foreign,
    }
}

/// Writes agit's OpenCode plugin, named in setup's output. A file there that agit did not write
/// stays as it is and setup says so; nothing else in OpenCode's plugin folder is touched.
pub(super) fn opencode(exe: &str) -> SetupReport {
    let Some(path) = opencode_plugin_path() else {
        ui::warning("cannot install the OpenCode plugin: HOME is not set");
        return SetupReport::failure();
    };
    opencode_at(&path, exe)
}

fn opencode_at(path: &Path, exe: &str) -> SetupReport {
    if opencode_plugin_state(path, exe) == OpencodePlugin::Foreign {
        ui::warning(&format!(
            "cannot install the OpenCode plugin: {} is a file agit did not write, so it was left as it is",
            path.display()
        ));
        return SetupReport::failure();
    }
    let result = super::write_if_changed(path, &opencode_plugin(exe), "OpenCode plugin");
    if result.succeeded() {
        ui::info(format_args!(
            "  {} OpenCode plugin: {}",
            ui::ok(ui::theme::symbols().check),
            path.display()
        ));
    }
    result
}

/// Removes agit's OpenCode plugin, and only it: a file there that agit did not write stays.
pub(crate) fn remove_opencode(path: &Path) -> std::io::Result<bool> {
    match std::fs::read_to_string(path) {
        Ok(text) if text.starts_with(OPENCODE_MARKER) => std::fs::remove_file(path).map(|()| true),
        Ok(_) => Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// The first line of agit's Pi extension; a file that does not start with it is not agit's.
const PI_MARKER: &str = "// agentgit-pi-extension ";

/// Where agit's Pi extension goes: a folder of its own among Pi's global extensions,
/// `<agent dir>/extensions/agit/index.js`, which Pi loads in every folder without asking.
pub(crate) fn pi_extension_path() -> Option<std::path::PathBuf> {
    crate::adapter::pi::home()
        .ok()
        .map(|home| home.join("extensions/agit/index.js"))
}

/// agit's Pi extension for the agit at `exe`.
fn pi_extension(exe: &str) -> String {
    include_str!("pi.mjs")
        .replace("__AGIT_PLUGIN_VERSION__", env!("CARGO_PKG_VERSION"))
        .replace(
            "__AGIT_EXE__",
            &exe.replace('\\', "\\\\").replace('"', "\\\""),
        )
}

/// Writes agit's Pi extension, named in setup's output. Its folder holds only that file: a folder
/// there holding anything else, or a file agit did not write, stays as it is and setup says so,
/// since Pi would load what is there in place of agit's. Nothing else among Pi's extensions is
/// touched.
pub(super) fn pi(exe: &str) -> SetupReport {
    let Some(path) = pi_extension_path() else {
        ui::warning("cannot install the Pi extension: HOME is not set");
        return SetupReport::failure();
    };
    pi_at(&path, exe)
}

fn pi_at(path: &Path, exe: &str) -> SetupReport {
    let ours = match std::fs::read_to_string(path) {
        Ok(text) => text.starts_with(PI_MARKER),
        Err(error) => error.kind() == std::io::ErrorKind::NotFound,
    };
    let alone = path
        .parent()
        .is_none_or(|folder| match std::fs::read_dir(folder) {
            Ok(entries) => entries
                .filter_map(Result::ok)
                .all(|entry| entry.file_name() == "index.js"),
            Err(_) => true,
        });
    if !ours || !alone {
        ui::warning(&format!(
            "cannot install the Pi extension: {} holds files agit did not write, so it was left as it is",
            path.parent().unwrap_or(path).display()
        ));
        return SetupReport::failure();
    }
    let result = write_if_changed(path, &pi_extension(exe), "Pi extension");
    if result.succeeded() {
        ui::info(format_args!(
            "  {} Pi extension: {}",
            ui::ok(ui::theme::symbols().check),
            path.display()
        ));
    }
    result
}

/// Removes agit's Pi extension, and its folder once that is empty: a file there that agit did not
/// write stays.
pub(crate) fn remove_pi(path: &Path) -> std::io::Result<bool> {
    match std::fs::read_to_string(path) {
        Ok(text) if text.starts_with(PI_MARKER) => {
            std::fs::remove_file(path)?;
            if let Some(folder) = path.parent() {
                // A folder that still holds something is not agit's to remove.
                let _ = std::fs::remove_dir(folder);
            }
            Ok(true)
        }
        Ok(_) => Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
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

    /// Setup owns one file in OpenCode's plugin folder, recognized by its first line. It writes
    /// and replaces only that file, leaves a file of the same name that it did not write as it
    /// is, and removal deletes only its own. The plugin it writes runs this agit, starts it
    /// detached on `session.idle` alone, and hands it nothing but the conversation's id and
    /// folder. The folder is the one OpenCode loads plugins from: under an absolute
    /// `$XDG_CONFIG_HOME`, else `~/.config`. A setup that overwrote by name would destroy the
    /// person's own plugin; one that passed more would hand agit the conversation through a path
    /// no privacy check reads; one that wrote under the home folder while OpenCode reads another
    /// would install a plugin that never runs.
    #[test]
    fn setup_owns_only_its_own_opencode_plugin_file() {
        // Absolute on this platform: a Unix-style path is relative on Windows.
        let paths = tempfile::tempdir().unwrap();
        let home = paths.path().join("home");
        let config = paths.path().join("config");
        assert_eq!(
            opencode_plugin_path_in(Some(config.clone()), Some(home.clone())),
            Some(config.join("opencode/plugin/agit.js"))
        );
        assert_eq!(
            opencode_plugin_path_in(Some("relative/config".into()), Some(home.clone())),
            Some(home.join(".config/opencode/plugin/agit.js"))
        );
        assert_eq!(
            opencode_plugin_path_in(None, Some(home.clone())),
            Some(home.join(".config/opencode/plugin/agit.js"))
        );
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("plugin").join("agit.js");
        assert_eq!(
            opencode_plugin_state(&path, "/a/agit"),
            OpencodePlugin::Absent
        );
        assert!(opencode_at(&path, "/a/agit").succeeded());
        assert_eq!(
            opencode_plugin_state(&path, "/a/agit"),
            OpencodePlugin::Current
        );
        assert_eq!(
            opencode_plugin_state(&path, "/b/agit"),
            OpencodePlugin::Older
        );
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.starts_with(OPENCODE_MARKER));
        assert!(written.contains("const AGIT = \"/a/agit\";"));
        assert!(written.contains("event?.type !== \"session.idle\""));
        assert!(written.contains("detached: true"));
        assert!(written.contains("JSON.stringify({ session_id: session, cwd: directory })"));
        assert!(!written.contains("__AGIT_"));
        assert!(opencode_at(&path, "/b/agit").succeeded());
        assert_eq!(
            opencode_plugin_state(&path, "/b/agit"),
            OpencodePlugin::Current
        );
        assert!(remove_opencode(&path).unwrap());
        assert!(!path.exists());

        let theirs = "export default async () => ({});\n";
        std::fs::write(&path, theirs).unwrap();
        let neighbour = path.with_file_name("theirs.js");
        std::fs::write(&neighbour, theirs).unwrap();
        assert_eq!(
            opencode_plugin_state(&path, "/a/agit"),
            OpencodePlugin::Foreign
        );
        assert!(!opencode_at(&path, "/a/agit").succeeded());
        assert!(!remove_opencode(&path).unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), theirs);
        assert_eq!(std::fs::read_to_string(&neighbour).unwrap(), theirs);
    }

    /// Setup writes agit's Pi extension as the only file of its own folder among Pi's
    /// extensions, replaces only that file, and removal deletes it and the folder it leaves empty.
    /// A file there that agit did not write, or any other file in that folder, stays as it is and
    /// setup refuses: Pi would load the folder's own entry in place of agit's. Other extensions
    /// beside the folder are never touched. The extension runs this agit, starts it detached on
    /// `agent_settled` and `session_shutdown`, and hands it only the session's id, file and folder
    /// and which of the two it was.
    /// A setup that overwrote by name would destroy the person's own extension, and one that
    /// passed more would hand agit the conversation through a path no privacy check reads.
    #[test]
    fn setup_owns_only_its_own_pi_extension_folder() {
        let root = tempfile::tempdir().unwrap();
        let extensions = root.path().join("extensions");
        let path = extensions.join("agit/index.js");
        let theirs = "export default function (pi) {}\n";
        std::fs::create_dir_all(&extensions).unwrap();
        let neighbour = extensions.join("theirs.js");
        std::fs::write(&neighbour, theirs).unwrap();

        assert!(pi_at(&path, "/a/agit").succeeded());
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.starts_with(PI_MARKER));
        assert!(written.contains("const AGIT = \"/a/agit\";"));
        assert!(written.contains("pi.on(\"agent_settled\""));
        assert!(written.contains("pi.on(\"session_shutdown\""));
        assert!(written.contains("detached: true"));
        assert!(written.contains(
            "session_id: session,\n        transcript_path: file,\n        cwd: ctx.cwd,"
        ));
        assert!(!written.contains("__AGIT_"));
        assert!(pi_at(&path, "/b/agit").succeeded());
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("const AGIT = \"/b/agit\";")
        );
        assert!(remove_pi(&path).unwrap());
        assert!(!path.exists() && !path.parent().unwrap().exists());

        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, theirs).unwrap();
        assert!(!pi_at(&path, "/a/agit").succeeded());
        assert!(!remove_pi(&path).unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), theirs);
        std::fs::remove_file(&path).unwrap();
        let manifest = path.with_file_name("package.json");
        std::fs::write(&manifest, "{}").unwrap();
        assert!(!pi_at(&path, "/a/agit").succeeded());
        assert!(!path.exists());
        assert_eq!(std::fs::read_to_string(&neighbour).unwrap(), theirs);
    }
}
