//! The native plugin is executable integration code; its managed files contain no session data.

use anyhow::{Context, ensure};
use fs2::FileExt;
use std::{io::Write, path::Path};

const MANIFEST: &str = include_str!("../harness/claude_native/plugin/.claude-plugin/plugin.json");
const CONTROL: &str = include_str!("../harness/claude_native/plugin/hooks/control.js");
const DEFAULT_CLI: &str = "const DEFAULT_AGIT_CLI = \"agit\";";

pub fn install_plugin() -> crate::Result<()> {
    let projects = crate::adapter::claude_code::projects_dir()?;
    let profile = projects.parent().context("Claude profile is unavailable")?;
    install_at(profile, &std::env::current_exe()?)
}

fn install_at(profile: &Path, executable: &Path) -> crate::Result<()> {
    let skills = profile.join("skills");
    std::fs::create_dir_all(&skills)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(skills.join(".agit-native-control.lock"))?;
    lock.lock_exclusive()?;
    let destination = skills.join("agit-native-control");
    let staging;
    let directory = if destination.try_exists()? {
        ensure!(
            !std::fs::symlink_metadata(&destination)?
                .file_type()
                .is_symlink(),
            "native Claude plugin directory is a symlink"
        );
        let manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(
            destination.join(".claude-plugin/plugin.json"),
        )?)?;
        ensure!(
            manifest["name"] == "agit-native-control" && manifest["author"]["name"] == "AgentGit",
            "another plugin owns the native Claude integration directory"
        );
        destination.as_path()
    } else {
        staging = tempfile::Builder::new()
            .prefix(".agit-native-control-")
            .tempdir_in(&skills)?;
        staging.path()
    };
    let command = serde_json::to_string(
        executable
            .to_str()
            .context("Agit executable path is not Unicode")?,
    )?;
    let control = CONTROL.replace(DEFAULT_CLI, &format!("const DEFAULT_AGIT_CLI = {command};"));
    for (name, body) in [
        (".claude-plugin/plugin.json", MANIFEST),
        ("hooks/control.js", control.as_str()),
        (
            "hooks/register.js",
            include_str!("../harness/claude_native/plugin/hooks/register.js"),
        ),
        (
            "hooks/hooks.json",
            include_str!("../harness/claude_native/plugin/hooks/hooks.json"),
        ),
    ] {
        let path = directory.join(name);
        let parent = path.parent().expect("bundle files have a parent");
        std::fs::create_dir_all(parent)?;
        ensure!(
            !std::fs::symlink_metadata(parent)?.file_type().is_symlink(),
            "native Claude plugin component directory is a symlink"
        );
        if std::fs::read_to_string(&path).ok().as_deref() == Some(body) {
            continue;
        }
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        file.write_all(body.as_bytes())?;
        file.as_file().sync_all()?;
        file.persist(&path)?;
        super::super::native_inbox::sync_directory(parent)?;
    }
    if directory != destination {
        // Discovery must never observe an incomplete initial plugin bundle.
        std::fs::rename(directory, &destination)?;
        super::super::native_inbox::sync_directory(&skills)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installation_uses_the_selected_profile_and_preserves_unowned_files() {
        let profile = tempfile::tempdir().unwrap();
        let executable = Path::new("/a path/agit\"custom");
        install_at(profile.path(), executable).unwrap();
        let directory = profile.path().join("skills/agit-native-control");
        let control = directory.join("hooks/control.js");
        assert!(
            std::fs::read_to_string(&control)
                .unwrap()
                .contains(&format!(
                    "const DEFAULT_AGIT_CLI = {};",
                    serde_json::to_string(executable.to_str().unwrap()).unwrap()
                ))
        );
        let extra = directory.join("notes.txt");
        std::fs::write(&extra, "user-owned notes").unwrap();
        install_at(profile.path(), executable).unwrap();
        assert_eq!(std::fs::read_to_string(extra).unwrap(), "user-owned notes");
        let before = std::fs::read(&control).unwrap();
        std::fs::write(
            directory.join(".claude-plugin/plugin.json"),
            "{\"name\":\"other\"}",
        )
        .unwrap();
        assert!(install_at(profile.path(), Path::new("/another/agit")).is_err());
        assert_eq!(std::fs::read(control).unwrap(), before);
    }
}
