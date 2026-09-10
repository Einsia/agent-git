//! Explicit native sources can be inspected without installing a runtime export or a claim.

use std::io::Read;
use std::path::{Path, PathBuf};

pub const MAX_NATIVE_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_NATIVE_RECORDS: usize = 262_144;
pub const MAX_WORKING_BYTES: usize = 256 * 1024 * 1024;
pub const MAX_LOOKUP_ENTRIES: usize = 262_144;

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub bytes: usize,
    pub records: usize,
    /// Admission covers owned read buffers, not SQLite coordination or later native validation.
    pub working_bytes: usize,
    pub lookup_entries: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            bytes: MAX_NATIVE_BYTES,
            records: MAX_NATIVE_RECORDS,
            working_bytes: MAX_WORKING_BYTES,
            lookup_entries: MAX_LOOKUP_ENTRIES,
        }
    }
}

/// Missing or incomplete evidence never establishes an empty native history.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum Unavailable {
    #[error("the explicit native session was not found")]
    NotFound,
    #[error("the explicit native session identifies multiple sources")]
    Ambiguous,
    #[error("this runtime has no read-only native snapshot provider")]
    Unsupported,
    #[error("the native inspection budget was exhausted")]
    BudgetExceeded,
    #[error("the native source could not be read completely")]
    Read,
    #[error("the native database schema or row is unavailable")]
    Database,
    #[error("the native source contains an incomplete record")]
    Incomplete,
    #[error("the native source changed while it was being inspected")]
    Changed,
}

pub type Result<T> = std::result::Result<T, Unavailable>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Source {
    pub runtime: &'static str,
    pub session_id: String,
    pub path: PathBuf,
    pub(crate) database: bool,
}

/// A complete byte read is not proof that its records establish native lineage.
#[derive(Debug)]
pub struct Snapshot {
    pub source: Source,
    pub bytes: Vec<u8>,
    pub records: usize,
}

pub(crate) struct Budget {
    remaining: usize,
}

impl Budget {
    pub(crate) fn new(bytes: usize) -> Self {
        Self { remaining: bytes }
    }

    pub(crate) fn reserve(&mut self, bytes: usize) -> Result<()> {
        self.remaining = self
            .remaining
            .checked_sub(bytes)
            .ok_or(Unavailable::BudgetExceeded)?;
        Ok(())
    }
}

pub(crate) fn validate_id(id: &str, limits: Limits) -> Result<()> {
    if id.is_empty() || id.contains(['\0', '/', '\\']) {
        return Err(Unavailable::NotFound);
    }
    if id.len() > limits.bytes.min(limits.working_bytes) {
        return Err(Unavailable::BudgetExceeded);
    }
    Ok(())
}

/// Lookup accepts a full native identity, never a prefix or a current-session inference.
pub(crate) fn lookup_files(runtime: &'static str, id: &str, limits: Limits) -> Result<Source> {
    validate_id(id, limits)?;
    let root = match runtime {
        "claude-code" => super::claude_code::projects_dir(),
        "codex" => {
            if let Some(path) = super::codex_index::native_path_readonly(id, limits)? {
                return file_source(runtime, id, path);
            }
            super::codex::sessions_root()
        }
        _ => return Err(Unavailable::Unsupported),
    }
    .map_err(|_| Unavailable::Read)?;
    lookup_files_at(runtime, id, &root, limits)
}

fn file_source(runtime: &'static str, id: &str, path: PathBuf) -> Result<Source> {
    let metadata = std::fs::symlink_metadata(&path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            Unavailable::NotFound
        } else {
            Unavailable::Read
        }
    })?;
    if !metadata.file_type().is_file() {
        return Err(Unavailable::Read);
    }
    Ok(Source {
        runtime,
        session_id: id.to_owned(),
        path,
        database: false,
    })
}

fn lookup_files_at(runtime: &'static str, id: &str, root: &Path, limits: Limits) -> Result<Source> {
    match std::fs::symlink_metadata(root) {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(Unavailable::NotFound);
        }
        _ => return Err(Unavailable::Read),
    }
    let mut selected = None;
    for (visited, entry) in walkdir::WalkDir::new(root).into_iter().enumerate() {
        if visited >= limits.lookup_entries {
            return Err(Unavailable::BudgetExceeded);
        }
        let entry = entry.map_err(|_| Unavailable::Read)?;
        let path = entry.path();
        let matches = path
            .extension()
            .is_some_and(|extension| extension == "jsonl")
            && match runtime {
                "claude-code" => path.file_stem().is_some_and(|stem| stem == id),
                "codex" => super::codex::id_from_filename(path).as_deref() == Some(id),
                _ => false,
            };
        if matches {
            if selected.is_some() {
                return Err(Unavailable::Ambiguous);
            }
            selected = Some(file_source(runtime, id, path.to_owned())?);
        }
    }
    selected.ok_or(Unavailable::NotFound)
}

pub(crate) fn finish(source: Source, bytes: Vec<u8>, limits: Limits) -> Result<Snapshot> {
    let records = bytes.iter().filter(|byte| **byte == b'\n').count();
    if bytes.len() > limits.bytes || records > limits.records {
        return Err(Unavailable::BudgetExceeded);
    }
    if !bytes.is_empty() && bytes.last() != Some(&b'\n') {
        return Err(Unavailable::Incomplete);
    }
    Ok(Snapshot {
        source,
        bytes,
        records,
    })
}

pub(crate) fn read_file(source: &Source, limits: Limits) -> Result<Snapshot> {
    if source.database {
        return Err(Unavailable::Unsupported);
    }
    finish(
        source.clone(),
        read_file_bytes(&source.path, limits)?,
        limits,
    )
}

/// Byte inspection pins a regular carrier; record completeness belongs to the caller.
pub(crate) fn read_file_bytes(path: &Path, limits: Limits) -> Result<Vec<u8>> {
    // Windows timestamps can survive replacement; keep the initial file identity alive.
    #[cfg(windows)]
    let initial = open_attributes(path).map_err(|_| Unavailable::Read)?;
    #[cfg(windows)]
    let before = initial.metadata().map_err(|_| Unavailable::Read)?;
    #[cfg(not(windows))]
    let before = std::fs::symlink_metadata(path).map_err(|_| Unavailable::Read)?;
    if !before.file_type().is_file() {
        return Err(Unavailable::Read);
    }
    #[cfg(windows)]
    let initial_identity = file_identity(&initial)?;
    let cap = limits
        .bytes
        .checked_add(1)
        .ok_or(Unavailable::BudgetExceeded)?;
    let capacity = usize::try_from(before.len()).unwrap_or(usize::MAX).min(cap);
    if capacity > limits.working_bytes {
        return Err(Unavailable::BudgetExceeded);
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // A substituted pipe must not wait before the opened-file check can reject it.
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    #[cfg(test)]
    read_observation::notify(read_observation::Stage::BeforeOpen);
    let file = options.open(path).map_err(|_| Unavailable::Read)?;
    let opened = file.metadata().map_err(|_| Unavailable::Read)?;
    if !same_file(&before, &opened) {
        return Err(Unavailable::Changed);
    }
    #[cfg(windows)]
    if file_identity(&file)? != initial_identity {
        return Err(Unavailable::Changed);
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(capacity)
        .map_err(|_| Unavailable::BudgetExceeded)?;
    let allowed = cap.min(limits.working_bytes);
    let mut reader = (&file).take(u64::try_from(allowed).map_err(|_| Unavailable::BudgetExceeded)?);
    let mut chunk = [0; 8192];
    loop {
        let read = reader.read(&mut chunk).map_err(|_| Unavailable::Read)?;
        if read == 0 {
            break;
        }
        let next = bytes
            .len()
            .checked_add(read)
            .ok_or(Unavailable::BudgetExceeded)?;
        if next > limits.bytes || next > limits.working_bytes {
            return Err(Unavailable::BudgetExceeded);
        }
        if next > bytes.capacity() {
            bytes
                .try_reserve_exact(next - bytes.len())
                .map_err(|_| Unavailable::BudgetExceeded)?;
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
    #[cfg(test)]
    read_observation::notify(read_observation::Stage::BeforeVerify);
    let after = file.metadata().map_err(|_| Unavailable::Read)?;
    let current = std::fs::symlink_metadata(path).map_err(|_| Unavailable::Changed)?;
    if u64::try_from(bytes.len()).ok() != Some(opened.len())
        || !same_file(&opened, &after)
        || !same_file(&opened, &current)
        || !is_current_file(&file, path)?
    {
        return Err(Unavailable::Changed);
    }
    Ok(bytes)
}

#[cfg(test)]
mod read_observation {
    use std::cell::RefCell;

    #[derive(Clone, Copy, PartialEq, Eq)]
    pub(super) enum Stage {
        BeforeOpen,
        BeforeVerify,
    }

    type Observer = Box<dyn FnMut(Stage)>;
    thread_local! {
        static OBSERVER: RefCell<Option<Observer>> = const { RefCell::new(None) };
    }

    pub(super) fn notify(stage: Stage) {
        OBSERVER.with_borrow_mut(|observer| {
            if let Some(observer) = observer {
                observer(stage);
            }
        });
    }

    pub(super) fn during<T>(observer: impl FnMut(Stage) + 'static, run: impl FnOnce() -> T) -> T {
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                OBSERVER.with_borrow_mut(|observer| *observer = None);
            }
        }
        OBSERVER.with_borrow_mut(|slot| {
            assert!(slot.is_none(), "native read observations must not nest");
            *slot = Some(Box::new(observer));
        });
        let _reset = Reset;
        run()
    }
}

#[cfg(windows)]
fn open_attributes(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
    };
    std::fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

#[cfg(windows)]
fn file_identity(file: &std::fs::File) -> Result<(u32, u32, u32)> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT, GetFileInformationByHandle,
    };
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut information) } == 0
        || information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
    {
        return Err(Unavailable::Read);
    }
    Ok((
        information.dwVolumeSerialNumber,
        information.nFileIndexHigh,
        information.nFileIndexLow,
    ))
}

#[cfg(windows)]
fn is_current_file(file: &std::fs::File, path: &Path) -> Result<bool> {
    let current = open_attributes(path).map_err(|_| Unavailable::Changed)?;
    Ok(file_identity(file)? == file_identity(&current)?)
}

#[cfg(not(windows))]
fn is_current_file(file: &std::fs::File, path: &Path) -> Result<bool> {
    Ok(same_file(
        &file.metadata().map_err(|_| Unavailable::Read)?,
        &std::fs::symlink_metadata(path).map_err(|_| Unavailable::Changed)?,
    ))
}

fn same_file(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    let same = left.file_type().is_file()
        && right.file_type().is_file()
        && left.len() == right.len()
        && left
            .modified()
            .ok()
            .zip(right.modified().ok())
            .is_some_and(|(a, b)| a == b);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        same && left.dev() == right.dev()
            && left.ino() == right.ino()
            && left.ctime() == right.ctime()
            && left.ctime_nsec() == right.ctime_nsec()
    }
    #[cfg(not(unix))]
    {
        same && left
            .created()
            .ok()
            .zip(right.created().ok())
            .is_some_and(|(a, b)| a == b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_bytes_preserve_an_incomplete_tail_without_weakening_snapshots() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("native.jsonl");
        let bytes = b"{\"type\":\"complete\"}\n{\"type\":";
        std::fs::write(&path, bytes).unwrap();
        assert_eq!(read_file_bytes(&path, Limits::default()).unwrap(), bytes);
        let source = Source {
            runtime: "codex",
            session_id: "native".into(),
            path: path.clone(),
            database: false,
        };
        assert_eq!(
            read_file(&source, Limits::default()).unwrap_err(),
            Unavailable::Incomplete
        );
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }

    #[test]
    fn substituted_regular_carriers_are_refused_at_both_read_boundaries() {
        use read_observation::Stage;
        for stage in [Stage::BeforeOpen, Stage::BeforeVerify] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("native.jsonl");
            let moved = root.path().join("retained.jsonl");
            std::fs::write(&path, b"{}\n").unwrap();
            let replacement = path.clone();
            let retained = moved.clone();
            let result = read_observation::during(
                move |current| {
                    if current == stage {
                        std::fs::rename(&replacement, &retained).unwrap();
                        std::fs::write(&replacement, b"{}\n").unwrap();
                        let modified = std::fs::metadata(&retained).unwrap().modified().unwrap();
                        std::fs::OpenOptions::new()
                            .write(true)
                            .open(&replacement)
                            .unwrap()
                            .set_times(std::fs::FileTimes::new().set_modified(modified))
                            .unwrap();
                    }
                },
                || read_file_bytes(&path, Limits::default()),
            );
            assert_eq!(result.unwrap_err(), Unavailable::Changed);
            assert_eq!(std::fs::read(path).unwrap(), b"{}\n");
            assert_eq!(std::fs::read(moved).unwrap(), b"{}\n");
        }
    }

    #[cfg(windows)]
    #[test]
    fn matching_windows_timestamps_cannot_rebind_the_native_carrier() {
        use read_observation::Stage;
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle, SetFileTime,
        };

        for stage in [Stage::BeforeOpen, Stage::BeforeVerify] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("native.jsonl");
            let moved = root.path().join("retained.jsonl");
            std::fs::write(&path, b"{}\n").unwrap();
            let replacement = path.clone();
            let retained = moved.clone();
            let result = read_observation::during(
                move |current| {
                    if current == stage {
                        std::fs::rename(&replacement, &retained).unwrap();
                        std::fs::write(&replacement, b"{}\n").unwrap();
                        let original = std::fs::File::open(&retained).unwrap();
                        let substitute = std::fs::OpenOptions::new()
                            .write(true)
                            .open(&replacement)
                            .unwrap();
                        let mut information = BY_HANDLE_FILE_INFORMATION::default();
                        assert_ne!(
                            unsafe {
                                GetFileInformationByHandle(
                                    original.as_raw_handle(),
                                    &mut information,
                                )
                            },
                            0
                        );
                        assert_ne!(
                            unsafe {
                                SetFileTime(
                                    substitute.as_raw_handle(),
                                    &information.ftCreationTime,
                                    std::ptr::null(),
                                    &information.ftLastWriteTime,
                                )
                            },
                            0
                        );
                        drop(substitute);
                        let substitute = std::fs::File::open(&replacement).unwrap();
                        assert!(same_file(
                            &original.metadata().unwrap(),
                            &substitute.metadata().unwrap()
                        ));
                        assert_ne!(
                            file_identity(&original).unwrap(),
                            file_identity(&substitute).unwrap()
                        );
                    }
                },
                || read_file_bytes(&path, Limits::default()),
            );
            assert_eq!(result.unwrap_err(), Unavailable::Changed);
            assert_eq!(std::fs::read(path).unwrap(), b"{}\n");
            assert_eq!(std::fs::read(moved).unwrap(), b"{}\n");
        }
    }

    #[cfg(unix)]
    #[test]
    fn substituted_symlinks_are_refused_before_open_and_after_read() {
        use read_observation::Stage;
        for stage in [Stage::BeforeOpen, Stage::BeforeVerify] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("native.jsonl");
            let target = root.path().join("foreign.jsonl");
            std::fs::write(&path, b"{}\n").unwrap();
            std::fs::write(&target, b"{}\n").unwrap();
            let replacement = path.clone();
            let foreign = target.clone();
            let result = read_observation::during(
                move |current| {
                    if current == stage {
                        std::fs::remove_file(&replacement).unwrap();
                        std::os::unix::fs::symlink(&foreign, &replacement).unwrap();
                    }
                },
                || read_file_bytes(&path, Limits::default()),
            );
            assert!(matches!(
                result,
                Err(Unavailable::Read | Unavailable::Changed)
            ));
            assert!(
                std::fs::symlink_metadata(path)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            assert_eq!(std::fs::read(target).unwrap(), b"{}\n");
        }
    }

    #[cfg(unix)]
    #[test]
    fn substituted_fifo_cannot_block_before_the_carrier_check() {
        use std::os::unix::ffi::OsStrExt;
        const CHILD: &str = "AGIT_TEST_NATIVE_FIFO_CHILD";
        if let Some(path) = std::env::var_os(CHILD) {
            let path = std::path::PathBuf::from(path);
            let replacement = path.clone();
            let result = read_observation::during(
                move |stage| {
                    if stage == read_observation::Stage::BeforeOpen {
                        std::fs::remove_file(&replacement).unwrap();
                        let name =
                            std::ffi::CString::new(replacement.as_os_str().as_bytes()).unwrap();
                        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
                    }
                },
                || read_file_bytes(&path, Limits::default()),
            );
            assert_eq!(result.unwrap_err(), Unavailable::Changed);
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("native.jsonl");
        std::fs::write(&path, b"{}\n").unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "adapter::native_snapshot::tests::substituted_fifo_cannot_block_before_the_carrier_check", "--nocapture"])
            .env(CHILD, &path).spawn().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "FIFO carrier refusal child failed");
                break;
            }
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("FIFO substitution blocked native carrier inspection");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        use std::os::unix::fs::FileTypeExt;
        assert!(
            std::fs::symlink_metadata(path)
                .unwrap()
                .file_type()
                .is_fifo()
        );
    }

    #[test]
    fn selected_files_are_bounded_complete_and_unchanged() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("native.jsonl");
        let original = b"{\"type\":\"unknown\",\"payload\":\"kept\"}\n";
        std::fs::write(&path, original).unwrap();
        let source =
            lookup_files_at("claude-code", "native", root.path(), Limits::default()).unwrap();
        let exact = Limits {
            bytes: original.len(),
            ..Limits::default()
        };
        let snapshot = read_file(&source, exact).unwrap();
        assert_eq!(snapshot.bytes, original);
        assert_eq!(snapshot.records, 1);
        assert_eq!(std::fs::read(&path).unwrap(), original);
        for limits in [
            Limits {
                bytes: original.len() - 1,
                ..exact
            },
            Limits {
                records: 0,
                ..exact
            },
            Limits {
                working_bytes: original.len() - 1,
                ..exact
            },
        ] {
            assert_eq!(
                read_file(&source, limits).unwrap_err(),
                Unavailable::BudgetExceeded
            );
        }
        std::fs::write(&path, b"{\"type\":\"pending\"}").unwrap();
        assert_eq!(
            read_file(&source, Limits::default()).unwrap_err(),
            Unavailable::Incomplete
        );
        std::fs::write(&path, b"{\"type\":\"a\",\"type\":\"b\"}\n").unwrap();
        assert_eq!(
            read_file(&source, Limits::default()).unwrap().bytes,
            b"{\"type\":\"a\",\"type\":\"b\"}\n"
        );
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[test]
    fn a_replaced_path_does_not_identify_the_opened_native_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("native.jsonl");
        std::fs::write(&path, b"{}\n").unwrap();
        let held = std::fs::File::open(&path).unwrap();
        assert!(is_current_file(&held, &path).unwrap());
        std::fs::rename(&path, directory.path().join("retained.jsonl")).unwrap();
        std::fs::write(&path, b"{}\n").unwrap();
        assert!(!is_current_file(&held, &path).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn a_link_to_another_native_file_is_not_a_regular_snapshot_source() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("other.jsonl");
        let selected = directory.path().join("selected.jsonl");
        std::fs::write(&target, b"{}\n").unwrap();
        std::os::unix::fs::symlink(&target, &selected).unwrap();
        let source = Source {
            runtime: "claude-code",
            session_id: "selected".into(),
            path: selected,
            database: false,
        };
        assert_eq!(
            read_file(&source, Limits::default()).unwrap_err(),
            Unavailable::Read
        );
        assert_eq!(std::fs::read(target).unwrap(), b"{}\n");
    }

    #[test]
    fn lookup_does_not_read_transcripts_or_choose_duplicate_identities() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("first")).unwrap();
        let path = root.path().join("first/native.jsonl");
        std::fs::write(&path, b"invalid transcript").unwrap();
        assert_eq!(
            lookup_files_at("claude-code", "native", root.path(), Limits::default())
                .unwrap()
                .path,
            path
        );
        assert_eq!(
            lookup_files_at("claude-code", "nativ", root.path(), Limits::default()).unwrap_err(),
            Unavailable::NotFound
        );
        assert_eq!(
            lookup_files_at(
                "claude-code",
                "native",
                root.path(),
                Limits {
                    lookup_entries: 1,
                    ..Limits::default()
                }
            )
            .unwrap_err(),
            Unavailable::BudgetExceeded
        );
        std::fs::create_dir(root.path().join("second")).unwrap();
        std::fs::write(root.path().join("second/native.jsonl"), b"different").unwrap();
        assert_eq!(
            lookup_files_at("claude-code", "native", root.path(), Limits::default()).unwrap_err(),
            Unavailable::Ambiguous
        );
        assert_eq!(std::fs::read(path).unwrap(), b"invalid transcript");
    }
}
