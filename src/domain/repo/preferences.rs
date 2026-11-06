//! Device-local repository preferences never travel with conversation history.

use super::Repo;
use anyhow::{Result, bail};

/// What this computer records about a destination's automatic upload, as the checkout reads it;
/// builds without the record read `Unset`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(feature = "secret-vault"), allow(dead_code))]
enum Recorded {
    Off,
    Unset,
    Unreadable,
}

impl Recorded {
    fn holds_off(self) -> bool {
        self != Self::Unset
    }
}

const AUTO_PUSH_KEY: &str = "agit.autoPush";
/// Set on a device-local repository when the person turned its folder off with `agit off`.
const UPLOADS_OFF_KEY: &str = "agit.uploadsOff";

impl Repo {
    /// Only the repository's local config can override the user's automatic publishing choice.
    /// Includes and inherited Git configuration cannot grant upload consent.
    pub fn auto_push_override(&self) -> Result<Option<bool>> {
        let (status, output, error) =
            self.git_status(&["config", "--local", "--no-includes", "--get", AUTO_PUSH_KEY])?;
        match status {
            Some(0) => match output.trim() {
                "true" => Ok(Some(true)),
                "false" => Ok(Some(false)),
                _ => bail!(
                    "invalid repository push.auto preference; set it to true or false with agit config"
                ),
            },
            Some(1) => Ok(None),
            _ => bail!(
                "could not read repository push.auto preference: {}",
                error.trim()
            ),
        }
    }

    pub fn set_auto_push(&self, value: Option<bool>) -> Result<()> {
        if let Some(value) = value {
            self.git(&[
                "config",
                "--local",
                "--replace-all",
                AUTO_PUSH_KEY,
                if value { "true" } else { "false" },
            ])?;
        } else {
            let (status, _, error) =
                self.git_status(&["config", "--local", "--unset-all", AUTO_PUSH_KEY])?;
            if !matches!(status, Some(0 | 5)) {
                bail!(
                    "could not unset repository push.auto preference: {}",
                    error.trim()
                );
            }
        }
        Ok(())
    }

    /// Whether automatic publication is explicitly off for this repository on this computer: its
    /// own preference is false, or it has none and the user's is false. Only a request that names
    /// the setting lifts it. An unset preference is a choice not yet made, which enrolment may
    /// make.
    pub fn auto_push_explicitly_off(&self) -> Result<bool> {
        let recorded = self.destination_recorded();
        if recorded == Recorded::Off {
            return Ok(true);
        }
        Ok(match self.auto_push_override()? {
            Some(value) => !value,
            None if recorded.holds_off() => true,
            None => crate::infra::config::get_global("push.auto")?.as_deref() == Some("false"),
        })
    }

    /// What this computer records about this destination's automatic upload, when this is a
    /// destination's checkout (`<repos>/<owner>/<name>`), and `Unset` otherwise.
    ///
    /// The record holds an off only while the person's latest choice for the destination is that
    /// off: every choice that turns it on writes the checkout's own setting and then drops the
    /// record. So a recorded off outranks the checkout's setting, which can then only be one a
    /// refused command left behind and could not take back. A record that can't be read speaks
    /// only for a checkout without a setting of its own.
    fn destination_recorded(&self) -> Recorded {
        #[cfg(feature = "secret-vault")]
        {
            let Ok(repos) = crate::infra::config::repos_dir() else {
                return Recorded::Unset;
            };
            let canonical = |path: &std::path::Path| path.canonicalize().ok();
            let relative = match self.root().strip_prefix(&repos) {
                Ok(relative) => relative.to_path_buf(),
                Err(_) => match (canonical(self.root()), canonical(&repos)) {
                    (Some(root), Some(repos)) => match root.strip_prefix(&repos) {
                        Ok(relative) => relative.to_path_buf(),
                        Err(_) => return Recorded::Unset,
                    },
                    _ => return Recorded::Unset,
                },
            };
            let mut parts = relative.components();
            match (parts.next(), parts.next(), parts.next()) {
                (Some(owner), Some(name), None) => {
                    use super::automatic_upload_off::Recorded as Record;
                    match super::automatic_upload_off::recorded(
                        &owner.as_os_str().to_string_lossy(),
                        &name.as_os_str().to_string_lossy(),
                    ) {
                        Record::Off => Recorded::Off,
                        Record::Unset => Recorded::Unset,
                        Record::Unreadable => Recorded::Unreadable,
                    }
                }
                _ => Recorded::Unset,
            }
        }
        #[cfg(not(feature = "secret-vault"))]
        Recorded::Unset
    }

    /// Whether the person turned this repository's folder off (`agit off`): nothing re-enables
    /// its automatic uploads, a later bind of the folder included, until they turn it on again.
    pub fn uploads_turned_off(&self) -> Result<bool> {
        let (status, output, error) = self.git_status(&[
            "config",
            "--local",
            "--no-includes",
            "--get",
            UPLOADS_OFF_KEY,
        ])?;
        match status {
            Some(0) => Ok(output.trim() != "false"),
            Some(1) => Ok(false),
            _ => bail!("could not read whether uploads are off: {}", error.trim()),
        }
    }

    pub fn turn_uploads_off(&self, off: bool) -> Result<()> {
        if off {
            self.git(&[
                "config",
                "--local",
                "--replace-all",
                UPLOADS_OFF_KEY,
                "true",
            ])?;
            return self.set_auto_push(Some(false));
        }
        let (status, _, error) =
            self.git_status(&["config", "--local", "--unset-all", UPLOADS_OFF_KEY])?;
        if !matches!(status, Some(0 | 5)) {
            bail!("could not turn uploads back on: {}", error.trim());
        }
        Ok(())
    }

    pub fn auto_push_enabled(&self) -> Result<bool> {
        let recorded = self.destination_recorded();
        if recorded == Recorded::Off {
            return Ok(false);
        }
        match self.auto_push_override()? {
            Some(value) => Ok(value),
            None if recorded.holds_off() => Ok(false),
            None => crate::infra::config::auto_push_default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_preference_is_local_and_unset_restores_inheritance() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(dir.path()).unwrap();
        assert_eq!(repo.auto_push_override().unwrap(), None);
        for value in [true, false] {
            repo.set_auto_push(Some(value)).unwrap();
            assert_eq!(repo.auto_push_override().unwrap(), Some(value));
            assert_eq!(repo.auto_push_enabled().unwrap(), value);
            assert!(repo.git(&["status", "--porcelain"]).unwrap().is_empty());
        }
        repo.set_auto_push(None).unwrap();
        repo.set_auto_push(None).unwrap();
        assert_eq!(repo.auto_push_override().unwrap(), None);
        repo.git(&["config", "--local", AUTO_PUSH_KEY, "invalid"])
            .unwrap();
        assert!(repo.auto_push_override().is_err());
    }
}
