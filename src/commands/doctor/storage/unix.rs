//! Descriptor-relative enumeration never opens a payload or follows a path supplied by one.

use super::{Identity, Kind, Metadata};
use std::ffi::{CStr, CString, OsStr, OsString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path};

pub(super) struct Directory {
    handle: OwnedFd,
    stream: Option<Stream>,
    identity: Identity,
}

struct Stream(*mut libc::DIR);

impl Drop for Stream {
    fn drop(&mut self) {
        unsafe { libc::closedir(self.0) };
    }
}

fn name(value: &OsStr) -> io::Result<CString> {
    CString::new(value.as_bytes()).map_err(|_| io::ErrorKind::InvalidInput.into())
}

// These fields have different integer widths across Unix targets.
#[allow(clippy::unnecessary_cast)]
fn from_stat(stat: &libc::stat) -> Metadata {
    Metadata {
        identity: Identity(stat.st_dev as u64, stat.st_ino as u64),
        kind: match stat.st_mode & libc::S_IFMT {
            libc::S_IFREG => Kind::File,
            libc::S_IFDIR => Kind::Directory,
            libc::S_IFLNK => Kind::Link,
            _ => Kind::Other,
        },
        links: stat.st_nlink as u64,
        size: u64::try_from(stat.st_size).unwrap_or(0),
        allocated: u64::try_from(stat.st_blocks)
            .ok()
            .and_then(|n| n.checked_mul(512)),
    }
}

impl Directory {
    fn open(parent: i32, path: &OsStr) -> io::Result<(Self, Metadata)> {
        let path = name(path)?;
        let fd = unsafe {
            libc::openat(
                parent,
                path.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let handle = unsafe { OwnedFd::from_raw_fd(fd) };
        let mut stat = std::mem::MaybeUninit::uninit();
        if unsafe { libc::fstat(handle.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let metadata = from_stat(&unsafe { stat.assume_init() });
        let identity = metadata.identity;
        Ok((
            Self {
                handle,
                stream: None,
                identity,
            },
            metadata,
        ))
    }

    pub(super) fn root(root: &Path) -> io::Result<(Self, Metadata)> {
        let (mut directory, mut metadata) = Self::open(libc::AT_FDCWD, OsStr::new("/"))?;
        for component in root.components() {
            match component {
                Component::RootDir => {}
                Component::Normal(part) => {
                    (directory, metadata) = Self::open(directory.handle.as_raw_fd(), part)?;
                }
                _ => return Err(io::ErrorKind::InvalidInput.into()),
            }
        }
        Ok((directory, metadata))
    }

    pub(super) fn identity(&self) -> Identity {
        self.identity
    }

    pub(super) fn child(&self, child: &OsStr) -> io::Result<Self> {
        Ok(Self::open(self.handle.as_raw_fd(), child)?.0)
    }

    pub(super) fn metadata(&self, child: &OsStr) -> io::Result<Metadata> {
        let child = name(child)?;
        let mut stat = std::mem::MaybeUninit::uninit();
        if unsafe {
            libc::fstatat(
                self.handle.as_raw_fd(),
                child.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(from_stat(&unsafe { stat.assume_init() }))
    }

    pub(super) fn next_name(&mut self) -> io::Result<Option<OsString>> {
        if self.stream.is_none() {
            let fd = unsafe { libc::fcntl(self.handle.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let stream = unsafe { libc::fdopendir(fd) };
            if stream.is_null() {
                let error = io::Error::last_os_error();
                unsafe { libc::close(fd) };
                return Err(error);
            }
            self.stream = Some(Stream(stream));
        }
        let stream = self.stream.as_ref().unwrap();
        loop {
            #[cfg(target_os = "macos")]
            let errno = unsafe { libc::__error() };
            #[cfg(not(target_os = "macos"))]
            let errno = unsafe { libc::__errno_location() };
            unsafe { *errno = 0 };
            let entry = unsafe { libc::readdir(stream.0) };
            if entry.is_null() {
                return if unsafe { *errno } == 0 {
                    Ok(None)
                } else {
                    Err(io::Error::last_os_error())
                };
            }
            let bytes = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
            if bytes != b"." && bytes != b".." {
                return Ok(Some(OsString::from_vec(bytes.to_vec())));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Replacing the selected pathname cannot redirect an already pinned scan to outside data.
    #[test]
    fn a_replaced_root_does_not_redirect_enumeration() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().canonicalize().unwrap();
        let selected = root.join("selected");
        let outside = root.join("outside");
        std::fs::create_dir(&selected).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(selected.join("inside"), b"synthetic").unwrap();
        std::fs::write(outside.join("outside"), b"synthetic").unwrap();
        let (mut directory, _) = Directory::root(&selected).unwrap();
        std::fs::rename(&selected, root.join("moved")).unwrap();
        std::os::unix::fs::symlink(&outside, &selected).unwrap();
        assert_eq!(directory.next_name().unwrap().unwrap(), "inside");
        assert!(directory.next_name().unwrap().is_none());
        assert!(Directory::root(&selected).is_err());
    }
}
