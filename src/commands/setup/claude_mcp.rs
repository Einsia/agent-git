//! Claude Code MCP registration.
//!
//! Claude Code keeps MCP servers in its global configuration file: user scope under the
//! top-level `mcpServers`, local scope under `projects.<folder>.mcpServers`. A local entry is
//! visible only in its own folder and takes precedence over the user entry there. agit registers
//! at user scope, because a local-only registration leaves every other folder without the agit
//! tools.
//!
//! The file is only read here; every change goes through the `claude mcp` commands, which own the
//! file's format and its concurrent writers.

use super::{SetupReport, ui};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const NAME: &str = "agit";

/// The `agit` entries of one Claude Code configuration file.
#[derive(Debug, Default)]
struct Registrations {
    user: Option<Value>,
    local: Vec<(PathBuf, Value)>,
}

/// What Claude Code loads as the agit MCP server.
pub(crate) enum Health {
    /// Registered at user scope. `overridden` lists existing folders whose own, different local
    /// entry wins there.
    User {
        overridden: Vec<PathBuf>,
    },
    /// Registered only for these folders.
    LocalOnly(Vec<PathBuf>),
    Missing,
    Unreadable(String),
}

/// The file Claude Code reads: a legacy `.config.json` in its configuration directory wins, and
/// otherwise `.claude.json` sits in `CLAUDE_CONFIG_DIR` or the home directory.
fn config_path() -> Option<PathBuf> {
    let configured = std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let home = crate::infra::config::user_home();
    let legacy = configured
        .clone()
        .or_else(|| home.as_ref().map(|home| home.join(".claude")))?
        .join(".config.json");
    if legacy.is_file() {
        return Some(legacy);
    }
    Some(configured.or(home)?.join(".claude.json"))
}

fn read(path: &Path) -> Result<Registrations, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Registrations::default());
        }
        Err(error) => return Err(format!("cannot read {}: {error}", ui::tilde(path))),
    };
    let doc = serde_json::from_str::<Value>(&text)
        .map_err(|_| format!("{} is not valid JSON", ui::tilde(path)))?;
    Ok(parse(&doc))
}

fn parse(doc: &Value) -> Registrations {
    Registrations {
        user: doc
            .get("mcpServers")
            .and_then(|servers| servers.get(NAME))
            .cloned(),
        local: doc
            .get("projects")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .filter_map(|(folder, project)| {
                let entry = project.get("mcpServers")?.get(NAME)?;
                Some((PathBuf::from(folder), entry.clone()))
            })
            .collect(),
    }
}

/// Two entries start the same server when every field agrees once Claude Code's defaults are
/// filled in: a stdio server with no arguments and no environment.
fn same_server(a: &Value, b: &Value) -> bool {
    fn normalized(entry: &Value) -> Option<serde_json::Map<String, Value>> {
        let mut entry = entry.as_object()?.clone();
        entry.entry("type").or_insert_with(|| "stdio".into());
        entry.entry("args").or_insert_with(|| Value::Array(vec![]));
        entry
            .entry("env")
            .or_insert_with(|| Value::Object(Default::default()));
        Some(entry)
    }
    matches!((normalized(a), normalized(b)), (Some(a), Some(b)) if a == b)
}

/// `claude mcp remove --scope local` acts on the project Claude Code derives from its working
/// directory: the enclosing git root, and for a linked worktree the main repository. Run from a
/// recorded folder, it addresses that folder's own entry only when the folder is a repository
/// root or lies outside every repository; anywhere else it could remove another project's entry.
fn addresses_itself(folder: &Path) -> bool {
    let git = folder.join(".git");
    if git.exists() {
        return git.is_dir();
    }
    folder
        .ancestors()
        .skip(1)
        .all(|ancestor| !ancestor.join(".git").exists())
}

/// A project's `.mcp.json` ranks between local and user scope, so removing a local entry there
/// would hand the folder to the project's definition rather than to the user one.
fn project_scope_defines(folder: &Path) -> bool {
    std::fs::read_to_string(folder.join(".mcp.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .is_some_and(|doc| {
            doc.get("mcpServers")
                .and_then(|servers| servers.get(NAME))
                .is_some()
        })
}

/// A local entry may be removed only when the user entry fully replaces it: removing it then
/// changes no folder's server, whoever created it. Any other local entry belongs to its folder's
/// owner and stays.
fn replaced_by_user(folder: &Path, entry: &Value, user: &Value) -> bool {
    same_server(entry, user) && addresses_itself(folder) && !project_scope_defines(folder)
}

/// Child notices follow agit's own informational output, so a completion script printed on
/// stdout stays clean.
fn claude(args: &[&str], folder: Option<&Path>) -> bool {
    let mut command = Command::new("claude");
    command.args(args).stdout(if ui::info_uses_stderr() {
        Stdio::from(std::io::stderr())
    } else {
        Stdio::inherit()
    });
    if let Some(folder) = folder {
        command.current_dir(folder);
    }
    command.status().is_ok_and(|status| status.success())
}

/// Run in a folder whose local entry overrides the user one, this leaves the user entry in force.
pub(crate) const REMOVE_LOCAL: &str = "claude mcp remove agit --scope local";

/// Register at user scope unless an entry is already there, then retire the local copies that
/// entry replaces. A failed `claude` leaves guidance instead of a failure: Claude Code may simply
/// not be installed.
pub(super) fn register(exe: &str) -> SetupReport {
    let path = config_path();
    let current = || path.as_deref().and_then(|path| read(path).ok());
    if current().is_some_and(|found| found.user.is_some()) {
        ui::info(format_args!(
            "  {} mcp(Claude Code, user scope) already in place",
            ui::dim("·")
        ));
    } else if claude(
        &["mcp", "add", "--scope", "user", NAME, "--", exe, "mcp"],
        None,
    ) {
        ui::info(format_args!(
            "  {} mcp → claude mcp add --scope user {NAME}",
            ui::ok(ui::theme::symbols().check)
        ));
    } else {
        ui::hint(&format!(
            "register MCP manually with `claude mcp add --scope user {NAME} -- {} mcp`",
            super::shell_word(exe)
        ));
        return SetupReport::item();
    }

    let Some(Registrations {
        user: Some(user),
        local,
    }) = current()
    else {
        return SetupReport::item();
    };
    // A folder that no longer exists overrides nothing until it is recreated.
    for (folder, entry) in local.iter().filter(|(folder, _)| folder.is_dir()) {
        if replaced_by_user(folder, entry, &user) {
            if claude(&["mcp", "remove", "--scope", "local", NAME], Some(folder)) {
                ui::info(format_args!(
                    "  {} mcp: removed the duplicate project-local registration in {}",
                    ui::ok(ui::theme::symbols().check),
                    ui::tilde(folder)
                ));
            }
        } else if !same_server(entry, &user) {
            ui::hint(&format!(
                "Claude Code uses a different project-local `{NAME}` MCP server in {}; to use the user-scope one there, run `{REMOVE_LOCAL}` in that folder",
                ui::tilde(folder)
            ));
        }
    }
    SetupReport::item()
}

/// The doctor's view of the registration, or None when Claude Code is neither installed nor
/// configured.
pub(crate) fn health() -> Option<Health> {
    let path = config_path()?;
    if !path.exists() && crate::adapter::which("claude").is_none() {
        return None;
    }
    let found = match read(&path) {
        Ok(found) => found,
        Err(problem) => return Some(Health::Unreadable(problem)),
    };
    Some(match found.user {
        Some(user) => Health::User {
            overridden: found
                .local
                .into_iter()
                .filter(|(folder, entry)| folder.is_dir() && !same_server(entry, &user))
                .map(|(folder, _)| folder)
                .collect(),
        },
        None if found.local.is_empty() => Health::Missing,
        None => Health::LocalOnly(found.local.into_iter().map(|(folder, _)| folder).collect()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Pins which local entries setup may remove. An implementation that compared only the
    /// command would delete a local entry carrying its own environment; one that skipped the
    /// folder check would run `claude mcp remove` from a worktree, which Claude Code maps to the
    /// main repository's entry.
    #[test]
    fn only_local_copies_the_user_entry_fully_replaces_are_removable() {
        let root = tempfile::tempdir().unwrap();
        let plain = root.path().join("plain");
        let repository = root.path().join("repository");
        let worktree = root.path().join("worktree");
        let nested = repository.join("nested");
        let team = root.path().join("team");
        for folder in [&plain, &repository.join(".git"), &worktree, &nested, &team] {
            std::fs::create_dir_all(folder).unwrap();
        }
        std::fs::write(worktree.join(".git"), "gitdir: elsewhere\n").unwrap();
        std::fs::write(
            team.join(".mcp.json"),
            r#"{"mcpServers":{"agit":{"command":"team-agit"}}}"#,
        )
        .unwrap();

        let user = json!({"type": "stdio", "command": "/bin/agit", "args": ["mcp"], "env": {}});
        let copy = json!({"command": "/bin/agit", "args": ["mcp"]});
        assert!(replaced_by_user(&plain, &copy, &user));
        assert!(replaced_by_user(&repository, &copy, &user));
        for folder in [&worktree, &nested, &team] {
            assert!(!replaced_by_user(folder, &copy, &user), "{folder:?}");
        }
        for own in [
            json!({"command": "/elsewhere/agit", "args": ["mcp"]}),
            json!({"command": "/bin/agit", "args": ["mcp"], "env": {"AGIT_HOME": "/x"}}),
            json!({"type": "http", "url": "http://127.0.0.1:1"}),
            json!("not an object"),
        ] {
            assert!(!replaced_by_user(&plain, &own, &user), "{own}");
        }
        assert!(!same_server(&json!("a"), &json!("a")));
    }
}
