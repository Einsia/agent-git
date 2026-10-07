//! Native Claude control binds to a process and session generation, never a transcript guess.

mod approval;
pub(crate) mod completion;
pub(crate) mod event_queue;
mod install;
mod transport;
pub(crate) use approval::wait_approval;
pub use install::install_plugin;
pub use transport::{Client, Endpoint, Snapshot, Update};

use anyhow::{Context, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

const RECORD_LIMIT: u64 = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Process {
    pub pid: u32,
    birth: Birth,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
struct Birth {
    boot: String,
    seconds: u64,
    fraction: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registration {
    pub version: u32,
    pub session: String,
    pub cwd: PathBuf,
    pub generation: String,
    pub process: Process,
    #[serde(default)]
    pub descriptor: PathBuf,
    revision: Birth,
    registry: PathBuf,
}

fn directory() -> crate::Result<PathBuf> {
    let directory = crate::infra::config::agit_home()?.join("native-claude");
    crate::infra::config::create_state_dir(&directory)?;
    let metadata = std::fs::symlink_metadata(&directory)?;
    ensure!(
        metadata.is_dir()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0,
        "native control directory must be private to its owner"
    );
    Ok(directory)
}

fn read_registration(path: &Path) -> crate::Result<Registration> {
    let file = super::native_inbox::open_regular(path)?;
    ensure!(
        file.metadata()?.uid() == unsafe { libc::geteuid() },
        "native control record has another owner"
    );
    let mut bytes = Vec::new();
    file.take(RECORD_LIMIT + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= RECORD_LIMIT,
        "native control record exceeds its size limit"
    );
    Ok(serde_json::from_slice(&bytes)?)
}

impl Registration {
    pub fn transcript_path(&self) -> crate::Result<PathBuf> {
        let path = self.transcript_target()?;
        ensure!(path.try_exists()?, "native transcript has not been written");
        Ok(path)
    }

    pub(crate) fn transcript_target(&self) -> crate::Result<PathBuf> {
        ensure!(
            self.is_registered(),
            "native registration is no longer current"
        );
        let live = super::claude_inbox::discover_in(&self.registry, &self.session)
            .context("native writer is no longer available")?;
        super::claude_inbox::live_transcript_target_in(
            &live,
            &self.cwd,
            &self
                .registry
                .parent()
                .context("native registry has no runtime home")?
                .join("projects"),
        )
    }
    /// Both the OS lifetime and the runtime's current session must still agree.
    pub fn is_current(&self) -> bool {
        self.version == 1
            && process(self.process.pid).is_some_and(|(process, _)| process == self.process)
            && super::claude_inbox::discover_in(&self.registry, &self.session).is_some_and(|live| {
                live.pid() == self.process.pid
                    && live.cwd().and_then(|cwd| cwd.canonicalize().ok()).as_ref()
                        == Some(&self.cwd)
            })
    }

    /// A delayed poll cannot re-enroll a generation superseded by a native transition.
    pub fn is_registered(&self) -> bool {
        directory()
            .ok()
            .and_then(|directory| {
                read_registration(&directory.join(format!("{}.json", self.process.pid))).ok()
            })
            .is_some_and(|current| current == *self && self.is_current())
    }
}

/// Only a child of the registered native writer can announce its current generation.
pub fn register(session: &str, generation: &str) -> crate::Result<Registration> {
    ensure!(
        super::native_inbox::valid_id(session) && super::native_inbox::valid_id(generation),
        "native control requires exact session and generation UUIDs"
    );
    let (child, parent_pid) =
        process(std::process::id()).context("cannot verify the registration process")?;
    let (parent, _) = process(parent_pid).context("cannot verify the native parent process")?;
    let registry = crate::adapter::claude_code::sessions_dir()?.canonicalize()?;
    let cwd = std::env::current_dir()?.canonicalize()?;
    let preferred = std::env::var_os("AGIT_NATIVE_CONTROL_DESCRIPTOR");
    let registration = Registration {
        version: 1,
        session: session.to_owned(),
        cwd,
        generation: generation.to_owned(),
        process: parent,
        revision: child.birth,
        descriptor: discover_descriptor(preferred.as_deref().map(Path::new))?,
        registry,
    };
    ensure!(
        registration.is_current(),
        "the caller is not the current native session writer"
    );
    publish(&directory()?, &registration)
}

pub fn descriptor_path() -> crate::Result<PathBuf> {
    Ok(super::rc_dir()?.join("claude-native.json"))
}

pub fn discover_descriptor(preferred: Option<&Path>) -> crate::Result<PathBuf> {
    transport::discover_descriptor(preferred)
}

fn publish(directory: &Path, registration: &Registration) -> crate::Result<Registration> {
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(directory.join(format!("{}.lock", registration.process.pid)))?;
    lock.lock_exclusive()?;
    let path = directory.join(format!("{}.json", registration.process.pid));
    match read_registration(&path) {
        Ok(current) => {
            if current.process == registration.process
                && current.session == registration.session
                && current.generation == registration.generation
                && current.cwd == registration.cwd
                && current.registry == registration.registry
                && current.descriptor == registration.descriptor
            {
                // Rediscovery preserves the generation's original lifetime fence.
                return Ok(current);
            }
            ensure!(
                current.process != registration.process || registration.revision > current.revision,
                "a newer native session registration is already present"
            );
        }
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) => {}
        Err(error) => return Err(error),
    }
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    serde_json::to_writer(&mut temporary, registration)?;
    temporary.flush()?;
    temporary.as_file().sync_all()?;
    temporary.persist(&path)?;
    super::native_inbox::sync_directory(directory)?;
    Ok(registration.clone())
}

#[cfg(target_os = "macos")]
fn process(pid: u32) -> Option<(Process, u32)> {
    let pid = i32::try_from(pid).ok().filter(|pid| *pid > 0)?;
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of_val(&info) as i32;
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    (read == size && info.pbi_uid == unsafe { libc::geteuid() }).then_some((
        Process {
            pid: pid as u32,
            birth: Birth {
                boot: String::new(),
                seconds: info.pbi_start_tvsec,
                fraction: info.pbi_start_tvusec,
            },
        },
        info.pbi_ppid,
    ))
}

#[cfg(target_os = "linux")]
fn process(pid: u32) -> Option<(Process, u32)> {
    if pid == 0 {
        return None;
    }
    let path = PathBuf::from(format!("/proc/{pid}"));
    if std::fs::metadata(&path).ok()?.uid() != unsafe { libc::geteuid() } {
        return None;
    }
    let stat = std::fs::read_to_string(path.join("stat")).ok()?;
    let fields: Vec<_> = stat.rsplit_once(") ")?.1.split_whitespace().collect();
    Some((
        Process {
            pid,
            birth: Birth {
                boot: std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
                    .ok()?
                    .trim()
                    .to_owned(),
                seconds: fields.get(19)?.parse().ok()?,
                fraction: 0,
            },
        },
        fields.get(1)?.parse().ok()?,
    ))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn process(_: u32) -> Option<(Process, u32)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delayed_registration_cannot_restore_a_prior_session_generation() {
        let directory = tempfile::tempdir().unwrap();
        let (process, _) = process(std::process::id()).unwrap();
        let mut current = Registration {
            version: 1,
            session: uuid::Uuid::new_v4().to_string(),
            cwd: directory.path().to_owned(),
            generation: uuid::Uuid::new_v4().to_string(),
            revision: process.birth.clone(),
            process,
            registry: directory.path().to_owned(),
            descriptor: directory.path().join("local.json"),
        };
        let first = current.clone();
        publish(directory.path(), &first).unwrap();
        let mut rediscovered = first.clone();
        rediscovered.revision.seconds += 1;
        assert_eq!(publish(directory.path(), &rediscovered).unwrap(), first);
        rediscovered.descriptor = directory.path().join("cloud.json");
        assert_eq!(
            publish(directory.path(), &rediscovered).unwrap(),
            rediscovered
        );
        assert!(publish(directory.path(), &first).is_err());
        current.revision.seconds += 2;
        current.generation = uuid::Uuid::new_v4().to_string();
        publish(directory.path(), &current).unwrap();
        assert!(
            publish(directory.path(), &first).is_err(),
            "late registration must not restore an old generation even when the session ID returns"
        );
        let path = directory
            .path()
            .join(format!("{}.json", current.process.pid));
        assert_eq!(read_registration(&path).unwrap(), current);
        assert!(
            !current.is_registered(),
            "a process without a native writer registration cannot become controllable"
        );
    }
}
