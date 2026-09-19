//! Advisory native writer observation; thread/resume still acquires the actual writer lock.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

pub(crate) fn writer_active(thread_id: &str) -> Option<bool> {
    let thread_id = uuid::Uuid::parse_str(thread_id).ok()?;
    let home = super::codex::codex_home().ok()?;
    writer_active_in(&home, &thread_id.to_string()).ok()
}

fn open_existing(path: &Path) -> io::Result<File> {
    OpenOptions::new().read(true).write(true).open(path)
}

fn writer_active_in(home: &Path, thread_id: &str) -> io::Result<bool> {
    let directory = home.join("thread-writer-locks");
    // Native creation and cleanup hold the coordination lock exclusively. Observation
    // must not race with unlinking, create lock files, or wait behind native startup.
    let coordination = open_existing(&directory.join(".coordination.lock"))?;
    fs2::FileExt::try_lock_shared(&coordination)?;
    let writer = match open_existing(&directory.join(format!("{thread_id}.lock"))) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    match fs2::FileExt::try_lock_shared(&writer) {
        Ok(()) => Ok(false),
        Err(error) if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => {
            Ok(true)
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observation_distinguishes_live_released_and_unknown_native_ownership() {
        let home = tempfile::tempdir().unwrap();
        let directory = home.path().join("thread-writer-locks");
        assert!(writer_active_in(home.path(), "native").is_err());
        assert!(!directory.exists());
        std::fs::create_dir(&directory).unwrap();
        let coordination = File::create(directory.join(".coordination.lock")).unwrap();
        let path = directory.join("native.lock");
        let writer = File::create(&path).unwrap();
        fs2::FileExt::lock_exclusive(&writer).unwrap();
        assert!(writer_active_in(home.path(), "native").unwrap());
        drop(writer);
        assert!(!writer_active_in(home.path(), "native").unwrap());
        assert!(
            path.exists(),
            "observation must not remove native lock files"
        );
        std::fs::remove_file(&path).unwrap();
        assert!(!writer_active_in(home.path(), "native").unwrap());
        assert!(
            !path.exists(),
            "observation must not create native lock files"
        );
        fs2::FileExt::lock_exclusive(&coordination).unwrap();
        assert!(writer_active_in(home.path(), "native").is_err());
    }
}
