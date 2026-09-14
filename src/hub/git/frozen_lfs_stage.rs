//! Private payload copies are returned only after their completed bytes match the pointers.
//! Availability, consent, deterministic scanning and publication remain caller responsibilities.

use crate::domain::lfs::Pointer;
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

const EMPTY_OID: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LfsStagingFailure {
    #[error("invalid or conflicting LFS staging pointers")]
    Pointers,
    #[error("LFS staging exceeds its object or byte budget")]
    Budget,
    #[error("LFS staging requires a fresh empty owned directory")]
    Destination,
    #[error("LFS staging directory overlaps the original cache")]
    Overlap,
    #[error("selected LFS payload is unavailable or unsupported")]
    Source,
    #[error("cannot write private LFS staging data")]
    Write,
    #[error("private LFS payload does not match its declared size and digest")]
    Integrity,
}

/// Failure retains the supplied directory, including any partial private copies.
/// Callers can recover ownership instead of deleting a rejected nonempty directory.
pub struct LfsStagingError {
    failure: LfsStagingFailure,
    directory: TempDir,
}

impl LfsStagingError {
    pub fn failure(&self) -> LfsStagingFailure {
        self.failure
    }

    pub fn into_directory(self) -> TempDir {
        self.directory
    }
}

impl std::fmt::Debug for LfsStagingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.failure, f)
    }
}

impl std::fmt::Display for LfsStagingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.failure, f)
    }
}

impl std::error::Error for LfsStagingError {}

/// Owns verified private copies, not a scan or consent receipt.
/// Keep this owner alive while inspecting or uploading its storage directory.
pub struct StagedLfsPayloads {
    directory: TempDir,
    pointers: Vec<Pointer>,
}

impl std::fmt::Debug for StagedLfsPayloads {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StagedLfsPayloads")
            .field("objects", &self.pointers.len())
            .finish_non_exhaustive()
    }
}

impl StagedLfsPayloads {
    /// The caller creates and exclusively owns `directory` before this call, outside the
    /// captured source cache. The caller selects pointers according to its inspection and
    /// upload policy; staging does not query remote availability.
    /// No source file is opened until every pointer and the aggregate budget are checked.
    /// Errors return directory ownership; dropping that error attempts ordinary temp cleanup.
    pub fn stage(
        source_objects: &Path,
        missing: &[Pointer],
        byte_budget: u64,
        directory: TempDir,
    ) -> Result<Self, LfsStagingError> {
        match stage_into(source_objects, missing, byte_budget, directory.path()) {
            Ok(pointers) => Ok(Self {
                directory,
                pointers,
            }),
            Err(failure) => Err(LfsStagingError { failure, directory }),
        }
    }

    /// Git LFS storage root; payloads live below its `objects` directory.
    /// Execution must retain this owner and avoid mutating files after inspection.
    pub(super) fn storage(&self) -> PathBuf {
        self.directory.path().join("storage")
    }

    pub fn pointers(&self) -> &[Pointer] {
        &self.pointers
    }

    /// Only the selected identity and size can open a staged payload for inspection.
    pub fn open_payload(&self, pointer: &Pointer) -> Result<impl Read + '_, LfsStagingFailure> {
        let index = self
            .pointers
            .binary_search_by(|selected| selected.oid.cmp(&pointer.oid))
            .map_err(|_| LfsStagingFailure::Pointers)?;
        if self.pointers[index].size != pointer.size {
            return Err(LfsStagingFailure::Pointers);
        }
        open_source(&object_path(&self.storage().join("objects"), pointer))
    }
}

fn stage_into(
    source_objects: &Path,
    missing: &[Pointer],
    byte_budget: u64,
    directory: &Path,
) -> Result<Vec<Pointer>, LfsStagingFailure> {
    let pointers = selected_pointers(missing, byte_budget)?;
    let destination = directory
        .canonicalize()
        .map_err(|_| LfsStagingFailure::Destination)?;
    let metadata = directory
        .symlink_metadata()
        .map_err(|_| LfsStagingFailure::Destination)?;
    if !metadata.is_dir() || metadata.is_symlink() {
        return Err(LfsStagingFailure::Destination);
    }
    if directory
        .read_dir()
        .map_err(|_| LfsStagingFailure::Destination)?
        .next()
        .transpose()
        .map_err(|_| LfsStagingFailure::Destination)?
        .is_some()
    {
        return Err(LfsStagingFailure::Destination);
    }
    let (source, exists) = source_outside_directory(source_objects, &destination)?;
    if !exists && pointers.iter().any(|pointer| pointer.size != 0) {
        return Err(LfsStagingFailure::Source);
    }
    let storage = destination.join("storage");
    create_private_directory(&storage)?;
    let objects = storage.join("objects");
    std::fs::create_dir(&objects).map_err(|_| LfsStagingFailure::Write)?;
    for pointer in &pointers {
        let output = object_path(&objects, pointer);
        std::fs::create_dir_all(output.parent().ok_or(LfsStagingFailure::Write)?)
            .map_err(|_| LfsStagingFailure::Write)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut output = options.open(output).map_err(|_| LfsStagingFailure::Write)?;
        if pointer.size == 0 {
            // Git LFS can represent the empty object without a physical cache entry.
            copy_verified(&mut std::io::empty(), &mut output, pointer)?;
        } else {
            let mut input = open_source(&object_path(&source, pointer))?;
            copy_verified(&mut input, &mut output, pointer)?;
        }
    }
    Ok(pointers)
}

/// Check the chosen parent before creating a new owned staging directory beneath it.
pub(super) fn source_outside_directory(
    source_objects: &Path,
    directory: &Path,
) -> Result<(PathBuf, bool), LfsStagingFailure> {
    let destination = directory
        .canonicalize()
        .map_err(|_| LfsStagingFailure::Destination)?;
    if !source_objects.is_absolute() {
        return Err(LfsStagingFailure::Source);
    }
    let (source, exists) = cache_boundary(source_objects)?;
    let source_boundary = crate::domain::repo::inspection_git_path_spelling(source.clone());
    let destination_boundary =
        crate::domain::repo::inspection_git_path_spelling(destination.clone());
    if source_boundary.starts_with(&destination_boundary)
        || destination_boundary.starts_with(&source_boundary)
    {
        return Err(LfsStagingFailure::Overlap);
    }
    Ok((source, exists))
}

fn cache_boundary(path: &Path) -> Result<(PathBuf, bool), LfsStagingFailure> {
    // Trailing separators make metadata follow a link instead of inspecting the link itself.
    let mut existing = path.components().as_path();
    let mut suffix = Vec::new();
    loop {
        match existing.canonicalize() {
            Ok(mut resolved) => {
                let exists = suffix.is_empty();
                for component in suffix.into_iter().rev() {
                    resolved.push(component);
                }
                return Ok((resolved, exists));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // Resolve existing links before appending a missing cache suffix.
                // A dangling link is not an absent component and cannot become a suffix.
                match existing.symlink_metadata() {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    _ => return Err(LfsStagingFailure::Source),
                }
                if suffix.len() >= 256 {
                    return Err(LfsStagingFailure::Source);
                }
                suffix.push(existing.file_name().ok_or(LfsStagingFailure::Source)?);
                existing = existing.parent().ok_or(LfsStagingFailure::Source)?;
            }
            Err(_) => return Err(LfsStagingFailure::Source),
        }
    }
}

fn selected_pointers(
    missing: &[Pointer],
    byte_budget: u64,
) -> Result<Vec<Pointer>, LfsStagingFailure> {
    // Bound caller input as well as the distinct object set before allocation or file access.
    if missing.len() > 10_000 {
        return Err(LfsStagingFailure::Budget);
    }
    let mut selected = BTreeMap::new();
    let mut bytes = 0_u64;
    for pointer in missing {
        pointer
            .validate()
            .map_err(|_| LfsStagingFailure::Pointers)?;
        if (pointer.size == 0) != (pointer.oid == EMPTY_OID) {
            return Err(LfsStagingFailure::Pointers);
        }
        if let Some(size) = selected.get(&pointer.oid) {
            if *size != pointer.size {
                return Err(LfsStagingFailure::Pointers);
            }
        } else {
            bytes = bytes
                .checked_add(pointer.size)
                .filter(|bytes| *bytes <= byte_budget)
                .ok_or(LfsStagingFailure::Budget)?;
            selected.insert(pointer.oid.clone(), pointer.size);
        }
    }
    Ok(selected
        .into_iter()
        .map(|(oid, size)| Pointer { oid, size })
        .collect())
}

fn object_path(objects: &Path, pointer: &Pointer) -> PathBuf {
    objects
        .join(&pointer.oid[..2])
        .join(&pointer.oid[2..4])
        .join(&pointer.oid)
}

fn create_private_directory(path: &Path) -> Result<(), LfsStagingFailure> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(path)
            .map_err(|_| LfsStagingFailure::Write)
    }
    #[cfg(windows)]
    {
        crate::infra::windows_security::private_directory(path)
            .map_err(|_| LfsStagingFailure::Write)
    }
}

fn open_source(path: &Path) -> Result<File, LfsStagingFailure> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path).map_err(|_| LfsStagingFailure::Source)?;
    let metadata = file.metadata().map_err(|_| LfsStagingFailure::Source)?;
    if !metadata.is_file() || metadata.is_symlink() {
        return Err(LfsStagingFailure::Source);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        if metadata.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
        {
            return Err(LfsStagingFailure::Source);
        }
    }
    Ok(file)
}

fn copy_verified(
    input: &mut impl Read,
    output: &mut File,
    pointer: &Pointer,
) -> Result<(), LfsStagingFailure> {
    let copied = std::io::copy(&mut input.by_ref().take(pointer.size), output)
        .map_err(|_| LfsStagingFailure::Write)?;
    if copied != pointer.size
        || input
            .read(&mut [0_u8; 1])
            .map_err(|_| LfsStagingFailure::Source)?
            != 0
    {
        return Err(LfsStagingFailure::Integrity);
    }
    output.rewind().map_err(|_| LfsStagingFailure::Write)?;
    pointer
        .verify(output)
        .map_err(|_| LfsStagingFailure::Integrity)
}

#[cfg(test)]
#[path = "frozen_lfs_stage_tests.rs"]
mod tests;
