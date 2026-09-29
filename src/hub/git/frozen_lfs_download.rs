//! Historical payload recovery uses isolated Git configuration and the source repository fence.

use super::FrozenPublication;
use crate::domain::lfs::Pointer;
use anyhow::{Context, Result, ensure};
use std::fs::File;
use std::io::{Seek, Write};
use std::process::Stdio;

impl FrozenPublication {
    pub(super) fn download_lfs_payload(&self, pointer: &Pointer) -> Result<File> {
        pointer.validate()?;
        let execution = self.lfs_execution(&self.directory.path().join("recovered-lfs"))?;
        let environment = || self.transport.environment_in(Some(&execution), true);
        let mut version = execution.command();
        version.args(["lfs", "version"]).envs(environment()?);
        let version = crate::domain::repo::bounded_inspection_output(version, super::CONFIG_LIMIT)?;
        ensure!(
            version.status.success(),
            "Git LFS is required to recover historical payloads"
        );
        crate::domain::lfs::local::validate_client_version(std::str::from_utf8(&version.stdout)?)?;
        if self
            .transport
            .client
            .as_ref()
            .is_some_and(crate::hub::Client::access_expired)
        {
            self.transport.refresh()?;
        }
        let bytes = format!(
            "version {}\noid sha256:{}\nsize {}\n",
            crate::domain::lfs::VERSION,
            pointer.oid,
            pointer.size
        );
        for attempt in 0..2 {
            let temporary = tempfile::NamedTempFile::new_in(self.directory.path())?;
            let mut child = execution
                .command()
                .args(["lfs", "smudge", "--", &pointer.oid])
                .envs(environment()?)
                .env("GIT_LFS_SKIP_SMUDGE", "0")
                .stdin(Stdio::piped())
                .stdout(temporary.reopen()?)
                .stderr(Stdio::piped())
                .spawn()
                .context("failed to start historical LFS download")?;
            let written = child
                .stdin
                .take()
                .context("LFS input is unavailable")?
                .write_all(bytes.as_bytes());
            let output = child
                .wait_with_output()
                .context("historical LFS download did not finish")?;
            written?;
            if !output.status.success() {
                let error = String::from_utf8_lossy(&output.stderr);
                if attempt == 0
                    && super::super::looks_like_auth_failure(&error)
                    && self.transport.refresh()?
                {
                    continue;
                }
                anyhow::bail!("historical LFS download failed: {}", error.trim());
            }
            let mut file = temporary.into_file();
            file.rewind()?;
            return Ok(file);
        }
        anyhow::bail!("historical LFS authentication could not be renewed")
    }
}
