//! Folders of Claude Code's memory, opened so that no link below the configured folder is followed.
//!
//! On unix every step works on a folder already opened without following a link (`openat` with
//! `O_NOFOLLOW`), so a path changed into a link meanwhile can't send a read or a write elsewhere.
//! Elsewhere a link is checked for before each step, not kept from appearing between them.

use std::ffi::OsStr;
use std::io;

/// What a name in a [`Dir`] is, seen without following a link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Dir,
    File,
    Link,
    Other,
}

#[cfg(unix)]
pub(super) use unix::Dir;

#[cfg(not(unix))]
pub(super) use paths::Dir;

/// The bytes of `name` in `dir`, at most `limit`: None when it holds more than that or isn't a
/// plain file.
pub(super) fn read_bounded(dir: &Dir, name: &OsStr, limit: u64) -> io::Result<Option<Vec<u8>>> {
    let Some(file) = dir.open_file(name)? else {
        return Ok(None);
    };
    let mut bytes = Vec::new();
    io::Read::read_to_end(&mut io::Read::take(file, limit + 1), &mut bytes)?;
    Ok((bytes.len() as u64 <= limit).then_some(bytes))
}

#[cfg(unix)]
mod unix {
    use super::Kind;
    use std::ffi::{CStr, CString, OsStr, OsString};
    use std::fs::File;
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::Path;

    /// A folder held open: every name below it is reached from this handle.
    pub(in crate::commands::memory) struct Dir(File);

    fn c(name: &OsStr) -> io::Result<CString> {
        CString::new(name.as_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
    }

    fn done(result: libc::c_int) -> io::Result<()> {
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    /// Clears `errno`, so a null from `readdir` can tell the end of a folder from an error.
    fn clear_errno() {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        unsafe {
            *libc::__errno_location() = 0;
        }
        #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
        unsafe {
            *libc::__error() = 0;
        }
    }

    impl Dir {
        /// The folder at `path`, with any link on the way followed: a folder the person chose.
        pub(in crate::commands::memory) fn open(path: &Path) -> io::Result<Self> {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY)
                .open(path)?;
            Ok(Self(file))
        }

        /// The folder `name` in this one; a link there is refused, not followed.
        pub(in crate::commands::memory) fn child(&self, name: &OsStr) -> io::Result<Self> {
            let name = c(name)?;
            let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
            let fd = unsafe { libc::openat(self.0.as_raw_fd(), name.as_ptr(), flags) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self(unsafe { File::from_raw_fd(fd) }))
        }

        /// What `name` is, without following it; None when nothing has that name.
        pub(in crate::commands::memory) fn kind(&self, name: &OsStr) -> io::Result<Option<Kind>> {
            Ok(self
                .stat(name)?
                .map(|stat| match stat.st_mode & libc::S_IFMT {
                    libc::S_IFDIR => Kind::Dir,
                    libc::S_IFREG => Kind::File,
                    libc::S_IFLNK => Kind::Link,
                    _ => Kind::Other,
                }))
        }

        /// The permission bits of `name`, not following it.
        pub(in crate::commands::memory) fn mode(&self, name: &OsStr) -> io::Result<u32> {
            let stat = self
                .stat(name)?
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
            // `mode_t` is 32 bits on Linux and 16 on macOS.
            #[allow(clippy::useless_conversion)]
            Ok(u32::from(stat.st_mode) & 0o7777)
        }

        fn stat(&self, name: &OsStr) -> io::Result<Option<libc::stat>> {
            let name = c(name)?;
            let mut stat: libc::stat = unsafe { std::mem::zeroed() };
            let flags = libc::AT_SYMLINK_NOFOLLOW;
            match done(unsafe {
                libc::fstatat(self.0.as_raw_fd(), name.as_ptr(), &mut stat, flags)
            }) {
                Ok(()) => Ok(Some(stat)),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(error),
            }
        }

        /// The names in this folder, `.` and `..` left out; None when there are more than `limit`.
        /// Listing stops there, and an error reading the folder is an error, never a short list.
        pub(in crate::commands::memory) fn names(
            &self,
            limit: usize,
        ) -> io::Result<Option<Vec<OsString>>> {
            let fd = unsafe { libc::dup(self.0.as_raw_fd()) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let stream = unsafe { libc::fdopendir(fd) };
            if stream.is_null() {
                let error = io::Error::last_os_error();
                unsafe { libc::close(fd) };
                return Err(error);
            }
            unsafe { libc::rewinddir(stream) };
            let mut names = Vec::new();
            let listed = loop {
                clear_errno();
                let entry = unsafe { libc::readdir(stream) };
                if entry.is_null() {
                    let error = io::Error::last_os_error();
                    break match error.raw_os_error() {
                        Some(0) => Ok(Some(names)),
                        _ => Err(error),
                    };
                }
                let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
                if name != b"." && name != b".." {
                    if names.len() == limit {
                        break Ok(None);
                    }
                    names.push(OsString::from_vec(name.to_vec()));
                }
            };
            unsafe { libc::closedir(stream) };
            listed
        }

        /// The folder `name` in this one for agit's own private files: made with 0700 if absent,
        /// opened without following a link, owned by this user, and left with no access for
        /// anyone else. Fails, changing nothing in it, when it is someone else's.
        pub(in crate::commands::memory) fn private_child(&self, name: &OsStr) -> io::Result<Self> {
            let cname = c(name)?;
            match done(unsafe { libc::mkdirat(self.0.as_raw_fd(), cname.as_ptr(), 0o700) }) {
                Err(error) if error.kind() != io::ErrorKind::AlreadyExists => return Err(error),
                _ => {}
            }
            let dir = self.child(name)?;
            let meta = dir.0.metadata()?;
            if std::os::unix::fs::MetadataExt::uid(&meta) != unsafe { libc::geteuid() } {
                return Err(io::Error::from(io::ErrorKind::PermissionDenied));
            }
            if std::os::unix::fs::MetadataExt::mode(&meta) & 0o077 != 0 {
                done(unsafe { libc::fchmod(dir.0.as_raw_fd(), 0o700) })?;
                if std::os::unix::fs::MetadataExt::mode(&dir.0.metadata()?) & 0o077 != 0 {
                    return Err(io::Error::from(io::ErrorKind::PermissionDenied));
                }
            }
            Ok(dir)
        }

        /// The plain file `name`, opened for reading without following a link; None when `name` is
        /// something else.
        pub(in crate::commands::memory) fn open_file(
            &self,
            name: &OsStr,
        ) -> io::Result<Option<File>> {
            let name = c(name)?;
            // Non-blocking, so a FIFO in its place doesn't wait for a writer; a plain file reads the
            // same either way.
            let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;
            let fd = unsafe { libc::openat(self.0.as_raw_fd(), name.as_ptr(), flags) };
            if fd < 0 {
                let error = io::Error::last_os_error();
                return match error.raw_os_error() {
                    Some(libc::ELOOP | libc::ENXIO) => Ok(None),
                    _ => Err(error),
                };
            }
            let file = unsafe { File::from_raw_fd(fd) };
            Ok(file.metadata()?.is_file().then_some(file))
        }

        /// A new file `name`, which must not exist yet, with `mode`.
        pub(in crate::commands::memory) fn create(
            &self,
            name: &OsStr,
            mode: u32,
        ) -> io::Result<File> {
            let name = c(name)?;
            let flags =
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC;
            let fd = unsafe {
                libc::openat(
                    self.0.as_raw_fd(),
                    name.as_ptr(),
                    flags,
                    mode as libc::c_uint,
                )
            };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let file = unsafe { File::from_raw_fd(fd) };
            file.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(mode))?;
            Ok(file)
        }

        /// Moves `from` here to `to` in `into`; an existing `to` is replaced, so callers pick a
        /// name nothing else uses.
        pub(in crate::commands::memory) fn rename(
            &self,
            from: &OsStr,
            into: &Self,
            to: &OsStr,
        ) -> io::Result<()> {
            let (from, to) = (c(from)?, c(to)?);
            done(unsafe {
                libc::renameat(
                    self.0.as_raw_fd(),
                    from.as_ptr(),
                    into.0.as_raw_fd(),
                    to.as_ptr(),
                )
            })
        }

        /// Gives the file `from` here the further name `to` in `into`; fails when `to` exists, and
        /// never follows `from` if it is a link.
        pub(in crate::commands::memory) fn link(
            &self,
            from: &OsStr,
            into: &Self,
            to: &OsStr,
        ) -> io::Result<()> {
            let (from, to) = (c(from)?, c(to)?);
            done(unsafe {
                libc::linkat(
                    self.0.as_raw_fd(),
                    from.as_ptr(),
                    into.0.as_raw_fd(),
                    to.as_ptr(),
                    0,
                )
            })
        }

        /// Removes the name `name` (a file's other names keep it).
        pub(in crate::commands::memory) fn remove(&self, name: &OsStr) -> io::Result<()> {
            let name = c(name)?;
            done(unsafe { libc::unlinkat(self.0.as_raw_fd(), name.as_ptr(), 0) })
        }
    }
}

#[cfg(not(unix))]
mod paths {
    use super::Kind;
    use std::ffi::{OsStr, OsString};
    use std::fs::File;
    use std::io;
    use std::path::{Path, PathBuf};

    /// A folder named by its path; each step checks for a link first.
    pub(in crate::commands::memory) struct Dir(PathBuf);

    impl Dir {
        pub(in crate::commands::memory) fn open(path: &Path) -> io::Result<Self> {
            if !std::fs::metadata(path)?.is_dir() {
                return Err(io::Error::from(io::ErrorKind::NotADirectory));
            }
            Ok(Self(path.to_path_buf()))
        }

        pub(in crate::commands::memory) fn child(&self, name: &OsStr) -> io::Result<Self> {
            match self.kind(name)? {
                Some(Kind::Dir) => Ok(Self(self.0.join(name))),
                None => Err(io::Error::from(io::ErrorKind::NotFound)),
                Some(_) => Err(io::Error::from(io::ErrorKind::NotADirectory)),
            }
        }

        pub(in crate::commands::memory) fn kind(&self, name: &OsStr) -> io::Result<Option<Kind>> {
            match std::fs::symlink_metadata(self.0.join(name)) {
                Ok(meta) if meta.file_type().is_symlink() => Ok(Some(Kind::Link)),
                Ok(meta) if meta.is_dir() => Ok(Some(Kind::Dir)),
                Ok(meta) if meta.is_file() => Ok(Some(Kind::File)),
                Ok(_) => Ok(Some(Kind::Other)),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(error),
            }
        }

        pub(in crate::commands::memory) fn mode(&self, _name: &OsStr) -> io::Result<u32> {
            Ok(0o600)
        }

        pub(in crate::commands::memory) fn names(
            &self,
            limit: usize,
        ) -> io::Result<Option<Vec<OsString>>> {
            let mut names = Vec::new();
            for entry in std::fs::read_dir(&self.0)? {
                if names.len() == limit {
                    return Ok(None);
                }
                names.push(entry?.file_name());
            }
            Ok(Some(names))
        }

        /// The folder `name` for agit's own private files. On Windows it is made, or checked, with
        /// an access list for this user alone and no reparse point on its path.
        pub(in crate::commands::memory) fn private_child(&self, name: &OsStr) -> io::Result<Self> {
            let path = self.0.join(name);
            #[cfg(windows)]
            crate::infra::windows_security::private_directory(&path)?;
            #[cfg(not(windows))]
            match std::fs::create_dir(&path) {
                Err(error) if error.kind() != io::ErrorKind::AlreadyExists => return Err(error),
                _ => {}
            }
            self.child(name)
        }

        pub(in crate::commands::memory) fn open_file(
            &self,
            name: &OsStr,
        ) -> io::Result<Option<File>> {
            if self.kind(name)? != Some(Kind::File) {
                return Ok(None);
            }
            File::open(self.0.join(name)).map(Some)
        }

        pub(in crate::commands::memory) fn create(
            &self,
            name: &OsStr,
            _mode: u32,
        ) -> io::Result<File> {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(self.0.join(name))
        }

        pub(in crate::commands::memory) fn rename(
            &self,
            from: &OsStr,
            into: &Self,
            to: &OsStr,
        ) -> io::Result<()> {
            std::fs::rename(self.0.join(from), into.0.join(to))
        }

        pub(in crate::commands::memory) fn link(
            &self,
            from: &OsStr,
            into: &Self,
            to: &OsStr,
        ) -> io::Result<()> {
            std::fs::hard_link(self.0.join(from), into.0.join(to))
        }

        pub(in crate::commands::memory) fn remove(&self, name: &OsStr) -> io::Result<()> {
            std::fs::remove_file(self.0.join(name))
        }
    }
}
