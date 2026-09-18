//! Local filesystem locks identify daemon lifetimes independently of PID namespaces.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};

const LOCK_NAME: &str = "agitd.lock";

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct SocketIdentity {
    device: u64,
    inode: u64,
    changed_secs: i64,
    changed_nanos: i64,
}

impl SocketIdentity {
    fn read(path: &Path) -> crate::Result<Self> {
        Self::read_optional(path)?.with_context(|| format!("{} does not exist", path.display()))
    }

    fn read_optional(path: &Path) -> crate::Result<Option<Self>> {
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        ensure!(
            metadata.file_type().is_socket() && metadata.uid() == unsafe { libc::geteuid() },
            "{} is not a socket owned by this user",
            path.display()
        );
        Ok(Some(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            changed_secs: metadata.ctime(),
            changed_nanos: metadata.ctime_nsec(),
        }))
    }
}

#[derive(Serialize, Deserialize)]
struct Record {
    version: u32,
    socket: SocketIdentity,
}

/// The file is never unlinked: all starters must lock the same inode. Its descriptor is
/// close-on-exec, and remains held until the listener is closed, including before publication.
pub(super) struct Ownership {
    file: File,
    directory: PathBuf,
}

impl Ownership {
    /// A missing file is absence of evidence, not evidence that a legacy listener has exited.
    pub(super) fn acquire(directory: &Path, create: bool) -> crate::Result<Option<Self>> {
        let path = directory.join(LOCK_NAME);
        let file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create(create)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) if !create && error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(None);
            }
            Err(error) => return Err(error).context("cannot inspect daemon ownership lock"),
        };
        let owner = Self {
            file,
            directory: directory.to_owned(),
        };
        owner.validate()?;
        fs2::FileExt::try_lock_exclusive(&owner.file).context(
            "cannot acquire daemon ownership lock; an instance may be starting, running or stopping",
        )?;
        owner.validate()?;
        Ok(Some(owner))
    }

    fn validate(&self) -> crate::Result<()> {
        let descriptor = self.file.metadata()?;
        let named = std::fs::symlink_metadata(self.directory.join(LOCK_NAME))?;
        ensure!(
            descriptor.is_file()
                && named.is_file()
                && descriptor.uid() == unsafe { libc::geteuid() }
                && descriptor.nlink() == 1
                && descriptor.dev() == named.dev()
                && descriptor.ino() == named.ino(),
            "daemon ownership lock is not a stable regular file owned by this user"
        );
        Ok(())
    }

    /// A released lock proves exit only for the socket instance recorded under that lock.
    pub(super) fn verify_stale(&self, path: &Path) -> crate::Result<()> {
        self.validate()?;
        let mut file = &self.file;
        file.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        file.take(4097).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 4096, "daemon ownership record is too large");
        let record: Record = serde_json::from_slice(&bytes)
            .context("daemon ownership record is missing, incomplete or unrecognized")?;
        ensure!(record.version == 1, "unrecognized daemon ownership version");
        ensure!(
            record.socket == SocketIdentity::read(path)?,
            "daemon ownership record does not identify this socket"
        );
        Ok(())
    }

    pub(super) fn remove_stale(&self, path: &Path) -> crate::Result<()> {
        self.verify_stale(path)?;
        std::fs::remove_file(path)?;
        Ok(())
    }

    /// The operator must establish exit and exclude legacy starters: they do not hold this lock.
    /// Connection probes only check for contradictory evidence; they cannot establish exit.
    pub(super) fn recover_stopped(&self, path: &Path) -> crate::Result<bool> {
        self.validate()?;
        let paths = [path.to_owned(), path.with_extension("rpc")];
        let identities = paths
            .iter()
            .map(|path| SocketIdentity::read_optional(path))
            .collect::<crate::Result<Vec<_>>>()?;
        for (path, identity) in paths.iter().zip(&identities) {
            if identity.is_none() {
                continue;
            }
            match super::connect_within(path, super::CONNECT_TIMEOUT) {
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {}
                Ok(_) => anyhow::bail!(
                    "{} still accepts connections; stop its owner before recovery",
                    path.display()
                ),
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "cannot recover {}; socket state is uncertain",
                            path.display()
                        )
                    });
                }
            }
        }
        self.validate()?;
        for (path, identity) in paths.iter().zip(&identities) {
            ensure!(
                SocketIdentity::read_optional(path)? == *identity,
                "{} changed during recovery; preserve it and confirm all starters have exited",
                path.display()
            );
        }
        if identities[0].is_some() {
            std::fs::remove_file(path)?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Publication is diagnostic state; the lock already protects the bind/publication gap.
    pub(super) fn publish(&mut self, path: &Path) -> crate::Result<()> {
        self.validate()?;
        let record = Record {
            version: 1,
            socket: SocketIdentity::read(path)?,
        };
        self.file.seek(SeekFrom::Start(0))?;
        self.file.set_len(0)?;
        serde_json::to_writer(&mut self.file, &record)?;
        self.file.sync_all()?;

        let mut pid = tempfile::NamedTempFile::new_in(&self.directory)?;
        write!(pid, "{}", std::process::id())?;
        pid.persist(self.directory.join("agitd.pid"))?;
        Ok(())
    }
}
