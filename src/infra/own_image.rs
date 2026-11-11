//! The executable a process starts more of itself from.
//!
//! A daemon outlives the install that started it: an upgrade renames a new binary over its path,
//! and npm replaces the package directory under it. A subprocess started from that path then runs
//! another build, whose arguments and replies this one cannot speak safely, or finds nothing there
//! at all. So every place agit starts itself goes through [`command`], which names the image this
//! process runs: a copy kept per build on this machine's own disk, which the daemon pins when it
//! starts ([`pin`]). A process that pinned nothing falls back, on Linux, to the kernel's link to
//! its image, which still opens after a rename on a local disk but not where the install
//! directory is a mount shared from a host; elsewhere to its own path, which is still the file it
//! was started from unless an install replaces it while it runs.
//!
//! Every child carries [`PARENT_BUILD`]. A child of another build refuses before it reads, writes
//! or sends anything ([`refuse_foreign_parent`]), so a fallback that still reached another build
//! fails the way a missing binary does, and the parent keeps the work for its retry.

use std::path::PathBuf;
use std::sync::OnceLock;

/// The variable that names the parent's build to a child.
pub const PARENT_BUILD: &str = "AGIT_RC_PARENT_BUILD";

/// The exit status of a child that refused a parent of another build.
pub const FOREIGN_PARENT_EXIT: i32 = 75;

/// This build's identity, as the parent passes it.
pub const BUILD: &str = env!("AGIT_BUILD_ID");

static PINNED: OnceLock<PathBuf> = OnceLock::new();

/// Start every later self-spawn from `image`, a copy of this build. Only the first pin counts.
pub fn pin(image: PathBuf) {
    let _ = PINNED.set(image);
}

/// The image this process runs, to start another of itself from.
pub fn own_image() -> std::io::Result<PathBuf> {
    if let Some(image) = PINNED.get() {
        return Ok(image.clone());
    }
    #[cfg(target_os = "linux")]
    {
        Ok(PathBuf::from(format!("/proc/{}/exe", std::process::id())))
    }
    #[cfg(not(target_os = "linux"))]
    {
        std::env::current_exe()
    }
}

/// A command that runs this build, carrying its identity to the child.
pub fn command() -> std::io::Result<std::process::Command> {
    let mut command = std::process::Command::new(own_image()?);
    command.env(PARENT_BUILD, BUILD);
    Ok(command)
}

/// Exit at once when the parent that started this process is another build. Called first in
/// `main`, before anything is read or written. The variable is removed either way, so whatever
/// this process starts by other means (git, a runtime, a hook) never inherits a claim about it.
pub fn refuse_foreign_parent() {
    let Some(parent) = std::env::var_os(PARENT_BUILD) else {
        return;
    };
    // SAFETY: called first in `main`, before any thread starts.
    unsafe {
        std::env::remove_var(PARENT_BUILD);
    }
    if parent != BUILD {
        eprintln!("agit: started by another agit build; nothing was done");
        std::process::exit(FOREIGN_PARENT_EXIT);
    }
}

#[cfg(test)]
mod tests {
    /// Once the daemon pins its copy, every self-spawn starts from it, on Linux too: the kernel's
    /// link to the running image doesn't start where the install directory is a mount shared
    /// from a host and an update renamed a new binary over it.
    #[test]
    fn a_pinned_copy_is_what_self_spawns_start_from() {
        let copy = tempfile::tempdir().unwrap().keep().join("agit");
        super::pin(copy.clone());
        assert_eq!(super::own_image().unwrap(), copy);
        assert_eq!(super::command().unwrap().get_program(), copy.as_os_str());
    }
}
