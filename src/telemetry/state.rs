//! Preferences and the upload gate are shared by every CLI process on this installation.

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

pub const NOTICE_VERSION: u32 = 1;
pub const ENABLED_NOTICE: &str = "Usage statistics are enabled and linked to your account when signed in. Run `agit telemetry disable` to turn them off.";
pub const DISCLOSURE: &str = "Help improve AgentGit by sharing CLI usage statistics.\n\nIncludes command and subcommand names, supported options, feature outcomes, performance, and basic environment information. When signed in, statistics are linked to your AgentGit account ID and analyzed using PostHog.\n\nExcludes repository names, paths, content, command text, and free-text arguments.\nYou can turn this off at any time with `agit telemetry disable`.\nDetails: https://github.com/Einsia/agent-git/blob/main/docs/telemetry.md\n";

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Preference {
    #[default]
    Unset,
    Enabled,
    Disabled,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionSource {
    SetupPrompt,
    SetupYes,
    SetupNoninteractive,
    CreateAgitYes,
    FirstInvocation,
    ExplicitEnable,
    ExplicitDisable,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcquisitionAccount {
    pub account_id: Option<String>,
    pub event_id: uuid::Uuid,
    pub saved_at: chrono::DateTime<chrono::Utc>,
    pub ci: bool,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Preferences {
    pub preference: Preference,
    pub generation: u64,
    pub notice_version: u32,
    pub decision_source: Option<DecisionSource>,
    pub decision_at: Option<String>,
    pub device_id: Option<uuid::Uuid>,
    pub channel: String,
    pub acquisition_id: Option<uuid::Uuid>,
    pub acquisition_route: Option<String>,
    pub install_reported: bool,
    pub acquisition_completed: bool,
    pub first_acquisition_account: Option<AcquisitionAccount>,
    pub acquisition_reported: bool,
}

pub fn directory() -> Result<PathBuf> {
    Ok(crate::infra::config::agit_home()?.join("telemetry"))
}

pub(crate) fn read_json<T: serde::de::DeserializeOwned>(
    path: &Path,
    limit: u64,
) -> Result<Option<T>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        metadata.is_file() && metadata.len() <= limit,
        "invalid telemetry state file"
    );
    let mut body = Vec::new();
    File::open(path)?.take(limit + 1).read_to_end(&mut body)?;
    ensure!(
        body.len() as u64 <= limit,
        "telemetry state exceeds its limit"
    );
    Ok(Some(serde_json::from_slice(&body)?))
}

pub(crate) fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let body = serde_json::to_vec(value)?;
    #[cfg(windows)]
    crate::infra::windows_security::write_private_file(path, &body)?;
    #[cfg(not(windows))]
    {
        use std::io::Write;
        let parent = path.parent().context("missing telemetry directory")?;
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        file.write_all(&body)?;
        file.persist(path)
            .map_err(|_| anyhow::anyhow!("cannot persist telemetry state"))?;
    }
    Ok(())
}

pub(crate) fn gate(dir: &Path, wait: bool) -> Result<File> {
    std::fs::create_dir_all(dir)?;
    ensure!(
        std::fs::symlink_metadata(dir)?.is_dir(),
        "invalid telemetry directory"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(dir.join("gate.lock"))?;
    if wait {
        fs2::FileExt::lock_exclusive(&file)?;
    } else {
        fs2::FileExt::try_lock_exclusive(&file)?;
    }
    Ok(file)
}

pub(crate) fn read_at(dir: &Path) -> Result<Preferences> {
    Ok(read_json(&dir.join("preferences.json"), 8192)?.unwrap_or_default())
}

pub fn read() -> Result<Preferences> {
    read_at(&directory()?)
}

pub fn override_reason() -> Option<&'static str> {
    [
        "AGIT_TELEMETRY_DISABLED",
        "DO_NOT_TRACK",
        "AGIT_TELEMETRY_DEFER",
    ]
    .into_iter()
    .find(|name| {
        std::env::var_os(name).is_some_and(|v| {
            !matches!(
                v.to_str().map(str::to_ascii_lowercase).as_deref(),
                Some("0" | "false" | "")
            )
        })
    })
}

pub fn enabled(preferences: &Preferences) -> bool {
    preferences.preference == Preference::Enabled && override_reason().is_none()
}

pub fn positive(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true"))
}

fn choose_at(
    dir: &Path,
    preference: Preference,
    source: DecisionSource,
    only_unset: bool,
) -> Result<Preferences> {
    let _guard = gate(dir, true)?;
    let mut current = match read_at(dir) {
        Ok(current) => current,
        Err(_) if preference == Preference::Disabled => Preferences::default(),
        Err(error) => return Err(error),
    };
    if (only_unset && current.preference != Preference::Unset)
        || (current.preference == preference && preference != Preference::Disabled)
    {
        return Ok(current);
    }
    let preserve_pending =
        current.preference == Preference::Unset && preference == Preference::Enabled;
    // The barrier covers both persistence and queue removal, so a worker cannot admit a stale batch.
    current.generation = current
        .generation
        .checked_add(1)
        .context("telemetry generation exhausted")?;
    current.preference = preference;
    current.notice_version = NOTICE_VERSION;
    current.decision_source = Some(source);
    current.decision_at = Some(chrono::Utc::now().to_rfc3339());
    current.device_id = (preference == Preference::Enabled).then(uuid::Uuid::new_v4);
    current.acquisition_id = None;
    current.acquisition_route = None;
    current.install_reported = false;
    current.acquisition_completed = false;
    current.first_acquisition_account = None;
    current.acquisition_reported = false;
    current.channel = match std::env::var("AGIT_INSTALL_CHANNEL").as_deref() {
        Ok("create_agit") => "create_agit",
        Ok("npm_global") => "npm_global",
        Ok("source") => "source",
        Ok("archive") => "archive",
        _ => "unknown",
    }
    .into();
    write_json(&dir.join("preferences.json"), &current)?;
    for name in ["queue.json", "activity.json", "pending-install.json"] {
        if name == "pending-install.json" && preserve_pending {
            continue;
        }
        match std::fs::remove_file(dir.join(name)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(current)
}

pub fn choose(
    preference: Preference,
    source: DecisionSource,
    only_unset: bool,
) -> Result<Preferences> {
    choose_at(&directory()?, preference, source, only_unset)
}

pub fn onboarding() -> Result<()> {
    if override_reason().is_some() {
        return Ok(());
    }
    let current = read()?;
    if current.preference != Preference::Unset {
        crate::ui::progress(if current.preference == Preference::Enabled {
            ENABLED_NOTICE
        } else {
            "Usage statistics are disabled."
        });
        return Ok(());
    }
    eprintln!("{DISCLOSURE}");
    let (answer, source) = if positive("AGIT_INSTALLER_YES") {
        (Some(true), DecisionSource::CreateAgitYes)
    } else if positive("AGIT_YES") {
        (Some(true), DecisionSource::SetupYes)
    } else if crate::ui::prompt::interactive() && !crate::commands::json::requested() {
        (
            crate::ui::prompt::confirm("Enable usage statistics?", true)?,
            DecisionSource::SetupPrompt,
        )
    } else {
        (Some(true), DecisionSource::SetupNoninteractive)
    };
    if let Some(answer) = answer {
        let saved = choose(
            if answer {
                Preference::Enabled
            } else {
                Preference::Disabled
            },
            source,
            true,
        )?;
        eprintln!(
            "{}",
            if saved.preference == Preference::Enabled {
                ENABLED_NOTICE
            } else {
                "Usage statistics are disabled."
            }
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reinstall_preserves_refusal_and_explicit_enable_rotates_identifiers() {
        let dir = tempfile::tempdir().unwrap();
        let first = choose_at(
            dir.path(),
            Preference::Enabled,
            DecisionSource::SetupYes,
            true,
        )
        .unwrap();
        std::fs::write(dir.path().join("queue.json"), b"[]").unwrap();
        let disabled = choose_at(
            dir.path(),
            Preference::Disabled,
            DecisionSource::ExplicitDisable,
            false,
        )
        .unwrap();
        assert!(disabled.generation > first.generation);
        assert!(!dir.path().join("queue.json").exists());
        assert!(disabled.device_id.is_none());
        assert_eq!(
            choose_at(
                dir.path(),
                Preference::Enabled,
                DecisionSource::CreateAgitYes,
                true
            )
            .unwrap()
            .preference,
            Preference::Disabled
        );
        let enabled = choose_at(
            dir.path(),
            Preference::Enabled,
            DecisionSource::ExplicitEnable,
            false,
        )
        .unwrap();
        assert_ne!(enabled.device_id, first.device_id);
    }
    #[test]
    fn corrupt_preferences_fail_closed_but_can_be_disabled() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("preferences.json"), b"not json").unwrap();
        assert!(read_at(dir.path()).is_err());
        assert!(
            choose_at(
                dir.path(),
                Preference::Enabled,
                DecisionSource::SetupYes,
                true
            )
            .is_err()
        );
        assert_eq!(
            choose_at(
                dir.path(),
                Preference::Disabled,
                DecisionSource::ExplicitDisable,
                false
            )
            .unwrap()
            .preference,
            Preference::Disabled
        );
    }
}
