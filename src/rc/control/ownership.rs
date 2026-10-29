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
        Ok(Some(Self::of(&metadata)))
    }

    fn of(metadata: &std::fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            changed_secs: metadata.ctime(),
            changed_nanos: metadata.ctime_nsec(),
        }
    }
}

impl SocketIdentity {
    /// Whether `current` is the socket this record was taken from.
    ///
    /// A device number is not stable across mounts: macOS renumbers volumes at boot, so a
    /// recorded device never matches again after a restart and the stale socket of a dead daemon
    /// could never be reclaimed. The socket lives beside the lock, so it must be on the lock's
    /// current device; inode and change time then identify the instance on that device.
    fn identifies(&self, current: &Self, lock_device: u64) -> bool {
        current.device == lock_device
            && current.inode == self.inode
            && current.changed_secs == self.changed_secs
            && current.changed_nanos == self.changed_nanos
    }
}

#[derive(Serialize, Deserialize)]
struct Record {
    version: u32,
    socket: SocketIdentity,
}

/// What shows that a released lock's socket has no listener left.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Proof {
    /// The lock recorded this socket.
    Recorded,
    /// The lock recorded another socket, and the process that last published here is gone: the
    /// home was copied or restored to where every file has a new identity.
    Relocated,
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

    /// A released lock proves exit for the socket instance recorded under that lock.
    ///
    /// A home copied or restored elsewhere keeps its record while the socket file gets a new
    /// identity, or becomes a regular file, so the record can never name it again. Such a file is
    /// reclaimed only with evidence that no listener holds it: nothing can be bound to a regular
    /// file, and on Linux a socket bound in this network namespace is listed in `/proc/net/unix`
    /// as soon as `bind` returns. A refused connection is no such evidence, since a listener with
    /// a full queue refuses too, and neither is `agitd.pid`: a daemon that does not take this lock
    /// writes it only after binding, so until then it names the previous daemon. Where no such
    /// evidence exists, a socket the record does not name stays uncertain. The process
    /// `agitd.pid` names must also be gone, or be this one.
    pub(super) fn verify_stale(&self, path: &Path) -> crate::Result<Proof> {
        self.validate()?;
        let mut file = &self.file;
        file.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        file.take(4097).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 4096, "daemon ownership record is too large");
        let record: Record = serde_json::from_slice(&bytes)
            .context("daemon ownership record is missing, incomplete or unrecognized")?;
        ensure!(record.version == 1, "unrecognized daemon ownership version");
        let metadata = std::fs::symlink_metadata(path)?;
        ensure!(
            (metadata.file_type().is_socket() || metadata.is_file())
                && metadata.uid() == unsafe { libc::geteuid() },
            "{} is not a socket owned by this user",
            path.display()
        );
        if metadata.file_type().is_socket()
            && record
                .socket
                .identifies(&SocketIdentity::of(&metadata), self.file.metadata()?.dev())
        {
            return Ok(Proof::Recorded);
        }
        ensure!(
            metadata.is_file() || bound_to(&metadata) == Some(false),
            "daemon ownership record does not identify {}, and nothing shows that no listener is bound to it",
            path.display()
        );
        let pid = self.published_pid()?;
        ensure!(
            pid == std::process::id() || process_is_gone(pid),
            "daemon ownership record does not identify this socket, and agitd.pid names running process {pid}"
        );
        Ok(Proof::Relocated)
    }

    fn published_pid(&self) -> crate::Result<u32> {
        let text = std::fs::read_to_string(self.directory.join("agitd.pid")).context(
            "daemon ownership record does not identify this socket, and agitd.pid is missing",
        )?;
        let pid: u32 = text
            .trim()
            .parse()
            .context("agitd.pid does not hold a process id")?;
        ensure!(pid > 0, "agitd.pid does not hold a process id");
        Ok(pid)
    }

    pub(super) fn remove_stale(&self, path: &Path) -> crate::Result<Proof> {
        let proof = self.verify_stale(path)?;
        std::fs::remove_file(path)?;
        Ok(proof)
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

/// Whether a socket in this network namespace is bound to the file `socket` describes, asked of the
/// kernel by the file's inode (sock_diag's `UNIX_DIAG_VFS`). The kernel keeps the path a socket was
/// bound under even after the file or a directory above it is renamed, so a path names a bound
/// socket's file only until then; the inode names it for as long as it is bound. The kernel
/// reports a truncated inode number, and the device it reports can differ from the one `stat`
/// gives on a layered filesystem, so the device is not compared: another file that matches can
/// only keep a socket that could have been reclaimed. `None` where the kernel cannot be asked,
/// which proves nothing.
#[cfg(target_os = "linux")]
fn bound_to(socket: &std::fs::Metadata) -> Option<bool> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    const SOCK_DIAG_BY_FAMILY: u16 = 20;
    const UDIAG_SHOW_VFS: u32 = 2;
    const UNIX_DIAG_VFS: u16 = 1;
    const HEADER: usize = std::mem::size_of::<libc::nlmsghdr>();
    // struct unix_diag_msg: family, type, state, pad, inode, cookie.
    const MESSAGE: usize = 16;
    #[repr(C)]
    struct Request {
        header: libc::nlmsghdr,
        // struct unix_diag_req
        family: u8,
        protocol: u8,
        pad: u16,
        states: u32,
        inode: u32,
        show: u32,
        cookie: [u32; 2],
    }
    let wanted = socket.ino() as u32;
    let raw = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_DGRAM | libc::SOCK_CLOEXEC,
            libc::NETLINK_SOCK_DIAG,
        )
    };
    if raw < 0 {
        return None;
    }
    let netlink = unsafe { OwnedFd::from_raw_fd(raw) };
    let request = Request {
        header: libc::nlmsghdr {
            nlmsg_len: std::mem::size_of::<Request>() as u32,
            nlmsg_type: SOCK_DIAG_BY_FAMILY,
            nlmsg_flags: (libc::NLM_F_REQUEST | libc::NLM_F_DUMP) as u16,
            nlmsg_seq: 1,
            nlmsg_pid: 0,
        },
        family: libc::AF_UNIX as u8,
        protocol: 0,
        pad: 0,
        states: u32::MAX,
        inode: 0,
        show: UDIAG_SHOW_VFS,
        cookie: [0; 2],
    };
    let sent = unsafe {
        libc::send(
            netlink.as_raw_fd(),
            (&request as *const Request).cast(),
            std::mem::size_of::<Request>(),
            0,
        )
    };
    if usize::try_from(sent).ok() != Some(std::mem::size_of::<Request>()) {
        return None;
    }
    let u16_at =
        |bytes: &[u8], at: usize| Some(u16::from_ne_bytes(bytes.get(at..at + 2)?.try_into().ok()?));
    let u32_at =
        |bytes: &[u8], at: usize| Some(u32::from_ne_bytes(bytes.get(at..at + 4)?.try_into().ok()?));
    let mut buffer = vec![0u8; 64 * 1024];
    let mut bound = false;
    loop {
        let read = unsafe {
            libc::recv(
                netlink.as_raw_fd(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                0,
            )
        };
        if read < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        let chunk = buffer.get(..usize::try_from(read).ok().filter(|read| *read > 0)?)?;
        let mut at = 0;
        while at < chunk.len() {
            let length = u32_at(chunk, at)? as usize;
            let kind = u16_at(chunk, at + 4)?;
            let message = chunk.get(at..at.checked_add(length)?)?;
            if length < HEADER {
                return None;
            }
            match i32::from(kind) {
                libc::NLMSG_DONE => return Some(bound),
                libc::NLMSG_ERROR => return None,
                _ if kind == SOCK_DIAG_BY_FAMILY => {
                    let mut attribute = HEADER + MESSAGE;
                    while attribute + 4 <= message.len() {
                        let size = u16_at(message, attribute)? as usize;
                        if size < 4 {
                            return None;
                        }
                        // struct unix_diag_vfs: the inode, then the device.
                        if u16_at(message, attribute + 2)? == UNIX_DIAG_VFS
                            && u32_at(message, attribute + 4)? == wanted
                        {
                            bound = true;
                        }
                        attribute += size.next_multiple_of(4);
                    }
                }
                _ => {}
            }
            at += length.next_multiple_of(4);
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn bound_to(_socket: &std::fs::Metadata) -> Option<bool> {
    None
}

/// Whether no process has this id in this PID namespace. Permission errors mean one exists.
fn process_is_gone(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    pid > 0
        && unsafe { libc::kill(pid, 0) } == -1
        && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A listener stays bound to its socket file after the directory above it is renamed, as when
    /// a home is moved while its daemon runs; the kernel still lists the socket under the old path.
    /// Matching listed paths to the file finds nothing bound there and reclaims a live listener.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_listener_stays_bound_to_its_socket_after_its_directory_is_renamed() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("before")).unwrap();
        let _listener =
            std::os::unix::net::UnixListener::bind(tmp.path().join("before/control.sock")).unwrap();
        std::fs::rename(tmp.path().join("before"), tmp.path().join("after")).unwrap();
        let socket = std::fs::symlink_metadata(tmp.path().join("after/control.sock")).unwrap();
        assert_eq!(bound_to(&socket), Some(true));
    }

    /// After a reboot the volume's device number changes while the socket file is unchanged; a
    /// record that compared devices would leave the dead daemon's socket unrecoverable. Any other
    /// difference is still a different socket.
    #[test]
    fn a_renumbered_volume_still_identifies_the_recorded_socket() {
        let recorded = SocketIdentity {
            device: 1,
            inode: 7,
            changed_secs: 10,
            changed_nanos: 20,
        };
        let after_reboot = SocketIdentity {
            device: 2,
            ..recorded
        };
        assert!(recorded.identifies(&after_reboot, 2));
        assert!(
            !recorded.identifies(&after_reboot, 3),
            "socket must sit beside the lock"
        );
        for other in [
            SocketIdentity {
                inode: 8,
                ..after_reboot
            },
            SocketIdentity {
                changed_secs: 11,
                ..after_reboot
            },
            SocketIdentity {
                changed_nanos: 21,
                ..after_reboot
            },
        ] {
            assert!(!recorded.identifies(&other, 2));
        }
    }
}
