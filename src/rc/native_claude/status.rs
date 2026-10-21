//! Whether a terminal Claude Code session on this machine can be controlled through the plugin.
//!
//! Shared control needs the plugin loaded inside the terminal's Claude process. When it is not,
//! the conversation is only readable from elsewhere, and the reason returned here is what a
//! client shows instead of an unexplained read-only view.

use std::path::Path;

/// The oldest Claude Code that loads the plugin's hooks from the skills directory.
const MINIMUM_VERSION: (u64, u64, u64) = (2, 1, 287);

/// The stable reason shared control cannot work on this machine, or `None` when it can.
pub(crate) fn unavailable() -> Option<&'static str> {
    let Some(program) = crate::rc::harness::claude_code::program::resolve() else {
        return Some("claude_unavailable");
    };
    if version(&program).is_none_or(|version| version < MINIMUM_VERSION) {
        return Some("claude_version_unsupported");
    }
    let profile = crate::adapter::claude_code::projects_dir().ok()?;
    let profile = profile.parent()?;
    unavailable_in(profile, &managed_settings())
}

fn unavailable_in(profile: &Path, managed: &[std::path::PathBuf]) -> Option<&'static str> {
    if !profile
        .join("skills/agit-native-control/.claude-plugin/plugin.json")
        .is_file()
    {
        return Some("plugin_not_installed");
    }
    let disables = |path: &Path| {
        std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .is_some_and(|settings| {
                [
                    "disableAllHooks",
                    "allowManagedHooksOnly",
                    "allowManagedModsOnly",
                ]
                .iter()
                .any(|key| settings[key] == true)
            })
    };
    if disables(&profile.join("settings.json")) || managed.iter().any(|path| disables(path)) {
        return Some("hooks_disabled");
    }
    None
}

fn managed_settings() -> Vec<std::path::PathBuf> {
    [
        "/Library/Application Support/ClaudeCode/managed-settings.json",
        "/etc/claude-code/managed-settings.json",
    ]
    .map(std::path::PathBuf::from)
    .to_vec()
}

/// The version a program reports, cached by its path and modification time.
fn version(program: &crate::rc::harness::claude_code::program::Program) -> Option<(u64, u64, u64)> {
    use std::sync::Mutex;
    type Version = (u64, u64, u64);
    type Key = (std::path::PathBuf, Option<std::time::SystemTime>);
    static CACHE: Mutex<Option<(Key, Option<Version>)>> = Mutex::new(None);
    let key = (
        program.path.clone(),
        std::fs::metadata(&program.path)
            .and_then(|metadata| metadata.modified())
            .ok(),
    );
    if let Some((cached, version)) = CACHE.lock().ok()?.as_ref()
        && *cached == key
    {
        return *version;
    }
    let version = crate::rc::harness::claude_code::version_of(program)
        .as_deref()
        .and_then(parse_version);
    if let Ok(mut cache) = CACHE.lock() {
        *cache = Some((key, version));
    }
    version
}

/// `claude --version` prints `2.1.299 (Claude Code)`.
fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    let mut parts = text.split_whitespace().next()?.split('.');
    let mut number = || parts.next()?.parse::<u64>().ok();
    Some((number()?, number()?, number()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each condition that keeps the plugin out of a terminal session has its own reason, so a
    /// client can say why a live conversation is read-only instead of offering control that
    /// fails.
    #[test]
    fn shared_control_reports_why_the_plugin_cannot_load() {
        assert_eq!(parse_version("2.1.299 (Claude Code)"), Some((2, 1, 299)));
        assert!(parse_version("2.1.286").unwrap() < MINIMUM_VERSION);

        let profile = tempfile::tempdir().unwrap();
        assert_eq!(
            unavailable_in(profile.path(), &[]),
            Some("plugin_not_installed")
        );
        let manifest = profile
            .path()
            .join("skills/agit-native-control/.claude-plugin/plugin.json");
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        std::fs::write(&manifest, "{}").unwrap();
        assert_eq!(unavailable_in(profile.path(), &[]), None);

        let managed = profile.path().join("managed-settings.json");
        std::fs::write(&managed, r#"{"allowManagedHooksOnly":true}"#).unwrap();
        assert_eq!(
            unavailable_in(profile.path(), &[managed]),
            Some("hooks_disabled")
        );
        std::fs::write(
            profile.path().join("settings.json"),
            r#"{"disableAllHooks":true}"#,
        )
        .unwrap();
        assert_eq!(unavailable_in(profile.path(), &[]), Some("hooks_disabled"));
    }
}
