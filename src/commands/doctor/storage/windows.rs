//! Directory guards deny replacement and reparse writes during pathname-based enumeration.

use super::{Identity, Kind, Metadata};
use std::ffi::{OsStr, OsString};
use std::fs::{File, ReadDir};
use std::io;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Component, Path, PathBuf, Prefix};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
    FILE_SHARE_READ, FILE_TYPE_DISK, GetDriveTypeW, GetFileInformationByHandle, GetFileType,
    GetVolumeNameForVolumeMountPointW,
};
use windows_sys::Win32::System::WindowsProgramming::{
    DRIVE_CDROM, DRIVE_FIXED, DRIVE_RAMDISK, DRIVE_REMOVABLE,
};

pub(super) struct Directory {
    path: PathBuf,
    _handle: File,
    _ancestors: Vec<File>,
    entries: Option<ReadDir>,
    identity: Identity,
}

fn open(path: &Path) -> io::Result<(File, Metadata)> {
    let file = File::options()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    let mut info = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), info.as_mut_ptr()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let info = unsafe { info.assume_init() };
    let kind = if unsafe { GetFileType(file.as_raw_handle()) } != FILE_TYPE_DISK {
        Kind::Other
    } else if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        Kind::Link
    } else if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
        Kind::Directory
    } else {
        Kind::File
    };
    Ok((
        file,
        Metadata {
            identity: Identity(
                info.dwVolumeSerialNumber as u64,
                (info.nFileIndexHigh as u64) << 32 | info.nFileIndexLow as u64,
            ),
            kind,
            links: info.nNumberOfLinks as u64,
            size: (info.nFileSizeHigh as u64) << 32 | info.nFileSizeLow as u64,
            allocated: None,
        },
    ))
}

fn is_local_volume(path: &[u16]) -> bool {
    matches!(
        unsafe { GetDriveTypeW(path.as_ptr()) },
        DRIVE_FIXED | DRIVE_REMOVABLE | DRIVE_CDROM | DRIVE_RAMDISK
    )
}

// A drive letter can be remapped while its old directory handles remain open. Bind enumeration
// to the local volume's GUID, so keeping ancestors open also pins the paths used for children.
fn local_volume(drive: u8) -> io::Result<PathBuf> {
    let mount = [u16::from(drive), u16::from(b':'), u16::from(b'\\'), 0];
    if !is_local_volume(&mount) {
        return Err(io::ErrorKind::Unsupported.into());
    }
    let mut volume = [0u16; 64];
    if unsafe {
        GetVolumeNameForVolumeMountPointW(mount.as_ptr(), volume.as_mut_ptr(), volume.len() as u32)
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if !is_local_volume(&volume) {
        return Err(io::ErrorKind::Unsupported.into());
    }
    let end = volume
        .iter()
        .position(|c| *c == 0)
        .ok_or(io::ErrorKind::InvalidData)?;
    Ok(PathBuf::from(OsString::from_wide(&volume[..end])))
}

impl Directory {
    fn at(path: PathBuf, ancestors: Vec<File>) -> io::Result<(Self, Metadata)> {
        let (handle, metadata) = open(&path)?;
        if metadata.kind != Kind::Directory {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let identity = metadata.identity;
        Ok((
            Self {
                path,
                _handle: handle,
                _ancestors: ancestors,
                entries: None,
                identity,
            },
            metadata,
        ))
    }

    pub(super) fn root(root: &Path) -> io::Result<(Self, Metadata)> {
        let mut components = root.components();
        let Some(Component::Prefix(prefix)) = components.next() else {
            return Err(io::ErrorKind::InvalidInput.into());
        };
        let (Prefix::Disk(drive) | Prefix::VerbatimDisk(drive)) = prefix.kind() else {
            return Err(io::ErrorKind::InvalidInput.into());
        };
        if components.next() != Some(Component::RootDir) {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let mut path = local_volume(drive)?;
        let mut ancestors = Vec::new();
        for component in components {
            match component {
                Component::Normal(part) => {
                    let (handle, metadata) = open(&path)?;
                    if metadata.kind != Kind::Directory {
                        return Err(io::ErrorKind::InvalidInput.into());
                    }
                    ancestors.push(handle);
                    path.push(part);
                }
                _ => return Err(io::ErrorKind::InvalidInput.into()),
            }
        }
        Self::at(path, ancestors)
    }

    pub(super) fn identity(&self) -> Identity {
        self.identity
    }

    pub(super) fn child(&self, name: &OsStr) -> io::Result<Self> {
        Ok(Self::at(self.path.join(name), Vec::new())?.0)
    }

    pub(super) fn metadata(&self, name: &OsStr) -> io::Result<Metadata> {
        Ok(open(&self.path.join(name))?.1)
    }

    pub(super) fn next_name(&mut self) -> io::Result<Option<OsString>> {
        if self.entries.is_none() {
            self.entries = Some(std::fs::read_dir(&self.path)?);
        }
        self.entries
            .as_mut()
            .unwrap()
            .next()
            .map(|entry| entry.map(|entry| entry.file_name()))
            .transpose()
    }
}
