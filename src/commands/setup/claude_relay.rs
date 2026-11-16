//! Extend an existing Claude integration once, without reenabling hooks removed afterward.

use super::{HOOK_FILE_LIMIT, HOOKS, carries_hook, claude_settings, exe_str, hook_arguments};
use anyhow::{Context, ensure};
use fs2::FileExt;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::Path,
};

/// Ordinary CLI startup refreshes only an already-enabled local remote-control integration.
pub(super) fn refresh() {
    if std::env::var_os("AGIT_SKIP_SETUP").is_some()
        || !crate::rc::claude_inbox::remote_control_configured()
    {
        return;
    }
    let Some(settings) = claude_settings().filter(|path| path.is_file()) else {
        return;
    };
    let result = (|| {
        let profile = crate::infra::config::user_home()
            .context("the user home is unavailable")?
            .join(".claude");
        ensure!(
            settings == profile.join("settings.json") && fs::symlink_metadata(&profile)?.is_dir(),
            "only the standard user profile is migrated automatically"
        );
        migrate(&settings, &crate::infra::config::agit_home()?, &exe_str())
    })();
    match result {
        Ok(true) => crate::ui::info(format_args!(
            "Claude inbox hooks updated. Restart Claude Code to receive Workspace messages through its local relay."
        )),
        Ok(false) => {}
        Err(_) => crate::ui::warning(
            "Claude inbox hooks could not be updated. Run `agit setup --runtime claude-code --hooks`, then restart Claude Code; Workspace messages may wait for local review until then.",
        ),
    }
}

fn migrate(settings: &Path, home: &Path, exe: &str) -> crate::Result<bool> {
    let state = home.join("claude-relay-hooks");
    crate::infra::config::create_state_dir(&state)?;
    let key = hex::encode(Sha256::digest(settings.as_os_str().as_encoded_bytes()));
    let mut lock = crate::infra::config::state_file_options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(state.join(format!("{key}.session-start-v1")))?;
    if lock.try_lock_exclusive().is_err() {
        return Ok(false);
    }
    let mut receipt = String::new();
    std::io::Read::by_ref(&mut lock)
        .take(32)
        .read_to_string(&mut receipt)?;
    if receipt == "complete" {
        return Ok(false);
    }
    let metadata = fs::symlink_metadata(settings)?;
    ensure!(
        metadata.is_file() && metadata.len() <= HOOK_FILE_LIMIT as u64,
        "the hook file cannot be migrated automatically"
    );
    let original = fs::read_to_string(settings)?;
    let mut doc: serde_json::Value = serde_json::from_str(&original)?;
    if doc["disableAllHooks"] == true
        || !HOOKS
            .iter()
            .any(|(event, argv)| carries_hook(&doc, event, &hook_arguments(argv, None)))
    {
        return Ok(false);
    }
    let changed = append(&mut doc, exe)?;
    if changed {
        let edited = super::json_text::edit(&original, &doc, |copy| {
            let _ = append(copy, exe);
        })
        .context("the hook file cannot be extended without rewriting it")?;
        // The migration keeps its own before-image even when setup's original backup predates it.
        super::keep_copy(settings, &original, ".agit-inbox-backup", false)?;
        let parent = settings.parent().context("the hook file has no parent")?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        temporary
            .as_file()
            .set_permissions(metadata.permissions())?;
        temporary.write_all(edited.as_bytes())?;
        temporary.as_file().sync_all()?;
        ensure!(
            fs::read_to_string(settings)? == original,
            "the hook file changed while its extension was prepared"
        );
        temporary.persist(settings)?;
    }
    use std::io::{Seek, SeekFrom};
    lock.seek(SeekFrom::Start(0))?;
    lock.set_len(0)?;
    lock.write_all(b"complete")?;
    lock.sync_all()?;
    Ok(changed)
}

/// Only append the missing hook; existing groups, commands and options are the user's choices.
fn append(doc: &mut serde_json::Value, exe: &str) -> crate::Result<bool> {
    if carries_hook(doc, "SessionStart", "hooks relay") {
        return Ok(false);
    }
    let hooks = doc
        .get_mut("hooks")
        .and_then(serde_json::Value::as_object_mut)
        .context("the hook collection is not an object")?;
    let starts = hooks
        .entry("SessionStart")
        .or_insert_with(|| serde_json::json!([]))
        .as_array_mut()
        .context("SessionStart hooks are not an array")?;
    starts.push(serde_json::json!({"hooks": [{
        "type": "command",
        "command": super::hook_command(exe, &["hooks", "relay"], None),
        "async": true,
    }]}));
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An upgrade extends opted-in hooks once, keeps the exact before-image, and cannot undo
    /// a later opt-out. Malformed or unrelated profiles never acquire an integration.
    #[test]
    fn relay_migration_preserves_user_hooks_and_respects_later_removal() {
        let root = tempfile::tempdir().unwrap();
        let settings = root.path().join("settings.json");
        let original = "{\n  \"model\": \"opus\",\n  \"hooks\": {\"SessionStart\": [{\"matcher\": \"startup\", \"hooks\": [\n    {\"type\": \"command\", \"command\": \"/old/agit hooks ingest\", \"timeout\": 5},\n    {\"type\": \"command\", \"command\": \"echo user-hook\"}\n  ]}], \"Stop\": [{\"hooks\": [{\"type\": \"command\", \"command\": \"/old/agit commit --from-hook\"}]}]},\n  \"permissions\": {\"allow\": [\"Bash(ls:*)\"]}\n}\n";
        fs::write(&settings, original).unwrap();
        let home = root.path().join("agit");
        assert!(migrate(&settings, &home, "/new/agit").unwrap());
        assert_eq!(
            fs::read_to_string(root.path().join("settings.json.agit-inbox-backup")).unwrap(),
            original
        );
        let after = fs::read_to_string(&settings).unwrap();
        let mut doc: serde_json::Value = serde_json::from_str(&after).unwrap();
        let starts = doc["hooks"]["SessionStart"].as_array_mut().unwrap();
        let added = starts.pop().unwrap();
        assert_eq!(added["hooks"][0]["async"], true);
        assert!(
            added["hooks"][0]["command"]
                .as_str()
                .unwrap()
                .ends_with(super::super::RELAY_OUTPUT)
        );
        assert_eq!(
            doc,
            serde_json::from_str::<serde_json::Value>(original).unwrap()
        );
        assert!(after.contains("\"permissions\": {\"allow\": [\"Bash(ls:*)\"]}"));
        assert!(!migrate(&settings, &home, "/newer/agit").unwrap());
        assert_eq!(fs::read_to_string(&settings).unwrap(), after);
        fs::write(&settings, original).unwrap();
        assert!(!migrate(&settings, &home, "/newer/agit").unwrap());
        assert_eq!(fs::read_to_string(&settings).unwrap(), original);

        let blocked = root.path().join("blocked.json");
        fs::write(&blocked, original).unwrap();
        fs::write(
            root.path().join("blocked.json.agit-inbox-backup"),
            "an earlier backup",
        )
        .unwrap();
        assert!(migrate(&blocked, &home, "/new/agit").is_err());
        assert_eq!(fs::read_to_string(&blocked).unwrap(), original);

        for (name, text) in [
            (
                "disabled",
                "{\"disableAllHooks\":true,\"hooks\":{\"Stop\":[{\"hooks\":[{\"type\":\"command\",\"command\":\"agit hooks settle\"}]}]}}",
            ),
            (
                "unconfigured",
                "{\"hooks\":{\"SessionStart\":[{\"hooks\":[{\"type\":\"command\",\"command\":\"echo user-hook\"}]}]}}",
            ),
            ("invalid", "not json"),
        ] {
            let settings = root.path().join(format!("{name}.json"));
            fs::write(&settings, text).unwrap();
            assert!(!migrate(&settings, &home, "/new/agit").unwrap_or(false));
            assert_eq!(fs::read_to_string(&settings).unwrap(), text);
            assert!(
                !root
                    .path()
                    .join(format!("{name}.json.agit-inbox-backup"))
                    .exists()
            );
        }
    }
}
