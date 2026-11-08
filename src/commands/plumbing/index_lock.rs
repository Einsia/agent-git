//! Git's index lock, found in a checkout whose transaction journal is still present: whether
//! anything can still be writing that index. The answer rests on the process that ran the
//! transaction, recorded next to its journal, and on the other processes that reach the checkout.

use std::path::Path;

/// The process that runs a checkout transaction. A record next to the transaction's journal, bound
/// to that journal's bytes, names it before the transaction starts any Git, and every Git the
/// transaction starts runs in its process group, so a later recovery can tell when neither it nor
/// any of those Gits is left.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Runner {
    pub(super) pid: u32,
    /// When `pid` started, as [`crate::rc::native_claude::started_ms`] reads it: the same pid with
    /// another start time is another process.
    pub(super) started_ms: i64,
    pub(super) process_group: i32,
}

impl Runner {
    /// This process, when its start time can be read. Without one no runner is recorded, and a
    /// lock its transaction leaves is one only a person removes.
    pub(super) fn current() -> Option<Self> {
        #[cfg(unix)]
        {
            let pid = std::process::id();
            Some(Self {
                pid,
                started_ms: crate::rc::native_claude::started_ms(pid)?,
                // SAFETY: getpgrp has no preconditions and cannot fail.
                process_group: unsafe { libc::getpgrp() },
            })
        }
        #[cfg(not(unix))]
        {
            None
        }
    }

    /// Neither this process nor any process of its group is left. A pid the system gave to
    /// another process since counts as gone; one whose start can't be read does not.
    #[cfg(unix)]
    fn gone(&self) -> bool {
        let replaced = !crate::commands::upload_outcome::alive(self.pid)
            || crate::rc::native_claude::started_ms(self.pid)
                .is_some_and(|started| started != self.started_ms);
        replaced && group_empty(self.process_group)
    }
}

/// Who may hold an index lock left in a checkout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(unix), allow(dead_code))]
pub(super) enum Holder {
    /// There is no lock.
    Absent,
    /// Nothing can still be writing the index: the transaction's runner and every process of its
    /// group are gone, no other process of this user reaches the checkout (see [`reach`]), and the
    /// lock is older than that check. Recovery removes it.
    Nobody,
    /// The runner, a process of its group, or another process reaching the checkout is still
    /// running, or the lock is newer than the check.
    Running,
    /// Whether anything still writes the index can't be shown: no runner is recorded for the
    /// journal, the processes of this user can't all be read, or this platform is not one agit
    /// reads them on. Only a person removes such a lock.
    Unknown,
}

/// Who may hold `lock`, the index lock of the checkout at `worktree` whose private Git directory
/// is `git_dir`, given the runner recorded for its transaction's journal. Only reads: removing the
/// lock is the caller's, which holds the checkout's transaction lock so that no agit transaction
/// starts there meanwhile.
pub(super) fn holder(
    lock: &Path,
    runner: Option<&Runner>,
    worktree: &Path,
    git_dir: &Path,
) -> Holder {
    match std::fs::symlink_metadata(lock) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Holder::Absent,
        Ok(metadata) if metadata.is_file() => {}
        _ => return Holder::Unknown,
    }
    #[cfg(unix)]
    {
        let Some(runner) = runner else {
            return Holder::Unknown;
        };
        // A lock written from this moment on is not the one the checks below are about.
        let checked = std::time::SystemTime::now();
        if !runner.gone() {
            return Holder::Running;
        }
        match reached(&Places::of(worktree, git_dir)) {
            None => return Holder::Unknown,
            Some(true) => return Holder::Running,
            Some(false) => {}
        }
        match std::fs::symlink_metadata(lock) {
            Ok(metadata) => match metadata.modified() {
                Ok(modified) if metadata.is_file() && modified < checked => Holder::Nobody,
                Ok(_) if metadata.is_file() => Holder::Running,
                _ => Holder::Unknown,
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Holder::Absent,
            Err(_) => Holder::Unknown,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (runner, worktree, git_dir);
        Holder::Unknown
    }
}

/// No process is left in group `group`. Group ids up to 1 never name a recorded group: 0 is the
/// caller's own and -1 every process, so neither is ever reported empty.
#[cfg(unix)]
fn group_empty(group: i32) -> bool {
    // SAFETY: signal 0 only checks that the group exists and may be signalled.
    group > 1
        && unsafe { libc::kill(-group, 0) } == -1
        && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

/// The places from which a process may write the checkout's index: its worktree and its private
/// Git directory. Each is kept as given and with every link resolved, since a process may name
/// either form.
#[cfg(any(target_os = "linux", target_os = "macos"))]
struct Places {
    worktree: [std::path::PathBuf; 2],
    git_dir: [std::path::PathBuf; 2],
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Places {
    fn of(worktree: &Path, git_dir: &Path) -> Self {
        let forms = |path: &Path| {
            let given = normalize(path);
            let resolved = path.canonicalize().unwrap_or_else(|_| given.clone());
            [given, resolved]
        };
        Self {
            worktree: forms(worktree),
            git_dir: forms(git_dir),
        }
    }

    fn in_git_dir(&self, path: &Path) -> bool {
        self.git_dir.iter().any(|place| path.starts_with(place))
    }

    fn in_checkout(&self, path: &Path) -> bool {
        self.in_git_dir(path) || self.worktree.iter().any(|place| path.starts_with(place))
    }
}

/// The environment variables through which a Git finds the checkout it writes.
#[cfg(any(target_os = "linux", target_os = "macos"))]
const GIT_LOCATIONS: [&str; 4] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
];

/// Why a part of a process could not be read.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unread {
    /// The process ended while it was read.
    Ended,
    /// The process still runs, and the part can't be read.
    Refused,
}

/// One process of this user as read: each part, or why it could not be read. The values stay in
/// this process and are never told to anyone: reading them only decides [`reach`].
#[cfg(any(target_os = "linux", target_os = "macos"))]
struct Reading {
    cwd: Result<std::path::PathBuf, Unread>,
    /// The paths of the files it has open.
    open: Result<Vec<std::path::PathBuf>, Unread>,
    arguments: Result<Vec<std::ffi::OsString>, Unread>,
    /// The values of the [`GIT_LOCATIONS`] variables in its environment.
    git_locations: Result<Vec<std::ffi::OsString>, Unread>,
}

/// Whether one process may write the checkout's index.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reach {
    /// It can't: it works elsewhere, or it ended.
    Outside,
    /// It may.
    Inside,
    /// Part of it can't be read, so it can't be shown not to.
    Unknown,
}

/// Whether a process may write the checkout's index, from [`Reading`] it. It may when its working
/// directory is in the checkout, when it has a file open in the Git directory (as a Git holding
/// the index lock does), or when an argument or a [`GIT_LOCATIONS`] value names a place in the
/// checkout (as a Git started elsewhere with `--git-dir` does, also while it holds no file open).
/// An argument names the path it is, and an `--option=value` one also the path its value is;
/// each is resolved against the process's working directory and compared lexically. A process
/// that ended while it was read works nowhere, whatever else was read of it.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn reach(places: &Places, reading: &Reading) -> Reach {
    let failures = [
        reading.cwd.as_ref().err(),
        reading.open.as_ref().err(),
        reading.arguments.as_ref().err(),
        reading.git_locations.as_ref().err(),
    ];
    if failures.contains(&Some(&Unread::Ended)) {
        return Reach::Outside;
    }
    let cwd = reading.cwd.as_deref().ok();
    let names = |path: &Path| resolve(cwd, path).is_some_and(|path| places.in_checkout(&path));
    let inside = cwd.is_some_and(|cwd| places.in_checkout(cwd))
        || reading.open.as_ref().is_ok_and(|open| {
            open.iter()
                .any(|path| places.in_git_dir(path) && !agit_own(path))
        })
        || reading.arguments.as_ref().is_ok_and(|arguments| {
            arguments
                .iter()
                .flat_map(|argument| argument_paths(argument.as_os_str()))
                .any(&names)
        })
        || reading
            .git_locations
            .as_ref()
            .is_ok_and(|values| values.iter().any(|value| names(Path::new(value))));
    if inside {
        Reach::Inside
    } else if failures.contains(&Some(&Unread::Refused)) {
        Reach::Unknown
    } else {
        Reach::Outside
    }
}

/// Whether `path` is one of agit's own checkout files (`agit-checkout-*`), which no Git opens. A
/// process has one open while recovery holds the checkout's transaction lock only as a child
/// forked meanwhile, which keeps that descriptor until it starts its own program.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn agit_own(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("agit-checkout-"))
}

/// The paths `argument` may name: itself, and the value of an `--option=value` argument.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn argument_paths(argument: &std::ffi::OsStr) -> impl Iterator<Item = &Path> {
    use std::os::unix::ffi::OsStrExt as _;
    let bytes = argument.as_bytes();
    let value = bytes
        .first()
        .filter(|first| **first == b'-')
        .and_then(|_| bytes.iter().position(|byte| *byte == b'='))
        .map(|at| Path::new(std::ffi::OsStr::from_bytes(&bytes[at + 1..])));
    std::iter::once(Path::new(argument)).chain(value)
}

/// `path` resolved against `cwd` when it is relative, and normalized; `None` for a relative path
/// with no working directory to resolve it against.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn resolve(cwd: Option<&Path>, path: &Path) -> Option<std::path::PathBuf> {
    if path.is_absolute() {
        Some(normalize(path))
    } else {
        cwd.map(|cwd| normalize(&cwd.join(path)))
    }
}

/// `path` with its `.` and `..` components applied lexically, without following any link.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn normalize(path: &Path) -> std::path::PathBuf {
    let mut normal = std::path::PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normal.pop();
            }
            component => normal.push(component.as_os_str()),
        }
    }
    normal
}

/// The [`GIT_LOCATIONS`] values among `environment`'s `NAME=value` entries.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn git_locations<'a>(environment: impl Iterator<Item = &'a [u8]>) -> Vec<std::ffi::OsString> {
    use std::os::unix::ffi::OsStrExt as _;
    environment
        .filter_map(|entry| {
            let at = entry.iter().position(|byte| *byte == b'=')?;
            GIT_LOCATIONS
                .iter()
                .any(|name| &entry[..at] == name.as_bytes())
                .then(|| std::ffi::OsStr::from_bytes(&entry[at + 1..]).to_owned())
        })
        .collect()
}

/// Whether a process of this user other than this one may write the checkout's index (see
/// [`reach`]), `None` when the processes can't be listed or one of them can't be read.
#[cfg(target_os = "linux")]
fn reached(places: &Places) -> Option<bool> {
    use std::os::unix::fs::MetadataExt as _;
    let me = std::process::id();
    // SAFETY: geteuid has no preconditions and cannot fail.
    let user = unsafe { libc::geteuid() };
    let mut unreadable = false;
    for entry in std::fs::read_dir("/proc").ok()? {
        let entry = entry.ok()?;
        if entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
            .is_none_or(|pid| pid == me)
        {
            continue;
        }
        let process = entry.path();
        match std::fs::metadata(&process) {
            Ok(metadata) if metadata.uid() == user => {}
            Ok(_) => continue,
            Err(error) if unread(&error) == Unread::Ended => continue,
            Err(_) => {
                unreadable = true;
                continue;
            }
        }
        match reach(places, &read_process(&process)) {
            Reach::Inside => return Some(true),
            Reach::Unknown => unreadable = true,
            Reach::Outside => {}
        }
    }
    (!unreadable).then_some(false)
}

/// What a failed read under `/proc/<pid>` says: a process that is gone, or still a zombie, gives
/// ENOENT or ESRCH.
#[cfg(target_os = "linux")]
fn unread(error: &std::io::Error) -> Unread {
    match error.raw_os_error() {
        Some(libc::ENOENT | libc::ESRCH) => Unread::Ended,
        _ => Unread::Refused,
    }
}

/// The process whose `/proc` directory is `process`, as read.
#[cfg(target_os = "linux")]
fn read_process(process: &Path) -> Reading {
    use std::os::unix::ffi::OsStrExt as _;
    let entries = |bytes: &[u8]| -> Vec<std::ffi::OsString> {
        bytes
            .split(|byte| *byte == 0)
            .filter(|entry| !entry.is_empty())
            .map(|entry| std::ffi::OsStr::from_bytes(entry).to_owned())
            .collect()
    };
    let open = (|| -> Result<Vec<std::path::PathBuf>, Unread> {
        let mut paths = Vec::new();
        for entry in std::fs::read_dir(process.join("fd")).map_err(|error| unread(&error))? {
            let entry = entry.map_err(|error| unread(&error))?;
            match std::fs::read_link(entry.path()) {
                Ok(target) => paths.push(target),
                // A descriptor closed since it was listed holds nothing.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(unread(&error)),
            }
        }
        Ok(paths)
    })();
    Reading {
        cwd: std::fs::read_link(process.join("cwd")).map_err(|error| unread(&error)),
        open,
        arguments: std::fs::read(process.join("cmdline"))
            .map(|bytes| entries(&bytes))
            .map_err(|error| unread(&error)),
        git_locations: std::fs::read(process.join("environ"))
            .map(|mut bytes| {
                let locations = git_locations(bytes.split(|byte| *byte == 0));
                bytes.fill(0);
                locations
            })
            .map_err(|error| unread(&error)),
    }
}

/// Whether a process of this user other than this one may write the checkout's index (see
/// [`reach`]), `None` when the processes can't be listed or one of them can't be read.
#[cfg(target_os = "macos")]
fn reached(places: &Places) -> Option<bool> {
    let me = i32::try_from(std::process::id()).ok()?;
    // SAFETY: a null buffer asks only for the number of processes.
    let listed = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    let listed = usize::try_from(listed).ok().filter(|count| *count > 0)?;
    // Room for processes started since; a full buffer may have left some out.
    let mut pids = vec![0i32; listed + listed / 4 + 64];
    let bytes = i32::try_from(pids.len() * std::mem::size_of::<i32>()).ok()?;
    // SAFETY: the buffer holds `bytes` bytes.
    let count = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
    let count = usize::try_from(count)
        .ok()
        .filter(|count| *count > 0 && *count < pids.len())?;
    pids.truncate(count);
    // SAFETY: geteuid has no preconditions and cannot fail.
    let user = unsafe { libc::geteuid() };
    let mut unreadable = false;
    for pid in pids {
        // Only a process shown to be a live one of this user's is read.
        if pid <= 0 || pid == me || !live_and_own(pid, user) {
            continue;
        }
        let reading = read_process(pid, user);
        match reach(places, &reading) {
            Reach::Inside => {
                #[cfg(test)]
                note_inside(pid, me, places, &reading);
                return Some(true);
            }
            Reach::Unknown => unreadable = true,
            Reach::Outside => {}
        }
    }
    (!unreadable).then_some(false)
}

/// Notes, for [`take_refusals`], by which part a process was found in the checkout and whether it
/// is a child of this one. No name, path or value of the process is kept.
#[cfg(all(test, target_os = "macos"))]
fn note_inside(pid: i32, me: i32, places: &Places, reading: &Reading) {
    // SAFETY: the struct is plain data, and the call writes at most its size.
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
    let child = read == size && i32::try_from(info.pbi_ppid).is_ok_and(|parent| parent == me);
    let part = if reading
        .cwd
        .as_deref()
        .is_ok_and(|cwd| places.in_checkout(cwd))
    {
        "inside by cwd"
    } else if reading.open.as_ref().is_ok_and(|open| {
        open.iter()
            .any(|path| places.in_git_dir(path) && !agit_own(path))
    }) {
        "inside by an open file"
    } else {
        "inside by an argument or GIT_* value"
    };
    REFUSED.with(|refused| {
        let mut refused = refused.borrow_mut();
        refused.push((if child { "a child" } else { "not a child" }, None));
        refused.push((part, None));
    });
}

#[cfg(all(test, target_os = "macos"))]
thread_local! {
    /// The parts of live processes of this user that the scans on this thread could not read,
    /// each with the error number it gave, and how a process was found in the checkout. A test that expects a lock cleared says from these why
    /// it stayed.
    static REFUSED: std::cell::RefCell<Vec<(&'static str, Option<i32>)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// [`REFUSED`] since the last call, counted by part and error number; empty on Linux.
#[cfg(test)]
pub(super) fn take_refusals() -> String {
    #[cfg(target_os = "macos")]
    {
        let mut counted = std::collections::BTreeMap::<String, usize>::new();
        for (part, error) in REFUSED.with(|refused| std::mem::take(&mut *refused.borrow_mut())) {
            *counted
                .entry(format!("{part} errno {error:?}"))
                .or_default() += 1;
        }
        format!("{counted:?}")
    }
    #[cfg(not(target_os = "macos"))]
    {
        String::new()
    }
}

/// `pid` is a process of `user` that has not ended.
#[cfg(target_os = "macos")]
fn live_and_own(pid: i32, user: libc::uid_t) -> bool {
    // SAFETY: the struct is plain data, and the call writes at most its size.
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
    read == size && info.pbi_uid == user && info.pbi_status != libc::SZOMB
}

/// How long a process's arguments are read again while KERN_PROCARGS2 says it is between images.
#[cfg(target_os = "macos")]
const ARGUMENTS_SETTLE: std::time::Duration = std::time::Duration::from_millis(50);

/// `struct vnode_fdinfowithpath` of `<sys/proc_info.h>`, which the libc crate leaves out, and its
/// `proc_pidfdinfo` flavor.
#[cfg(target_os = "macos")]
#[repr(C)]
struct VnodeFdInfoWithPath {
    /// `struct proc_fileinfo`, which nothing here reads.
    _file: [u64; 3],
    path: libc::vnode_info_path,
}

#[cfg(target_os = "macos")]
const PROC_PIDFDVNODEPATHINFO: libc::c_int = 2;

/// A path as the kernel reports it in a `vnode_info_path`.
#[cfg(target_os = "macos")]
fn reported_path(path: &[[libc::c_char; 32]; 32]) -> std::path::PathBuf {
    use std::os::unix::ffi::OsStrExt as _;
    let bytes: Vec<u8> = path
        .iter()
        .flatten()
        .take_while(|byte| **byte != 0)
        .map(|byte| *byte as u8)
        .collect();
    std::path::PathBuf::from(std::ffi::OsStr::from_bytes(&bytes))
}

/// The live process `pid` of `user`, as read.
#[cfg(target_os = "macos")]
fn read_process(pid: i32, user: libc::uid_t) -> Reading {
    // A read fails with ESRCH for a process that ended, and KERN_PROCARGS2 with EINVAL for one
    // that just exited; whatever the failure, the process ended when it is no longer a live one of
    // this user's, and refuses when it still is.
    let failed = |_part: &'static str| {
        let _error = std::io::Error::last_os_error().raw_os_error();
        if live_and_own(pid, user) {
            #[cfg(test)]
            REFUSED.with(|refused| refused.borrow_mut().push((_part, _error)));
            Unread::Refused
        } else {
            Unread::Ended
        }
    };
    let cwd = {
        // SAFETY: the struct is plain data, and the call writes at most its size.
        let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of_val(&info) as i32;
        let read = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDVNODEPATHINFO,
                0,
                (&mut info as *mut libc::proc_vnodepathinfo).cast(),
                size,
            )
        };
        if read == size {
            Ok(reported_path(&info.pvi_cdir.vip_path))
        } else {
            Err(failed("cwd"))
        }
    };
    let open = (|| -> Result<Vec<std::path::PathBuf>, Unread> {
        let entry = std::mem::size_of::<libc::proc_fdinfo>();
        // SAFETY: a null buffer asks only for the size of the descriptor table.
        let needed =
            unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
        if needed <= 0 {
            return Err(failed("descriptors"));
        }
        // Room for descriptors opened since; a reply that fills it may have been cut short.
        let room = needed as usize / entry + 32;
        let mut table = vec![
            libc::proc_fdinfo {
                proc_fd: 0,
                proc_fdtype: 0,
            };
            room
        ];
        let capacity = libc::c_int::try_from(room * entry).map_err(|_| Unread::Refused)?;
        // SAFETY: the table holds `capacity` bytes.
        let written = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDLISTFDS,
                0,
                table.as_mut_ptr().cast(),
                capacity,
            )
        };
        if written <= 0 {
            return Err(failed("descriptors"));
        }
        if written >= capacity {
            return Err(Unread::Refused);
        }
        table.truncate(written as usize / entry);
        let mut paths = Vec::new();
        for descriptor in table
            .iter()
            .filter(|descriptor| descriptor.proc_fdtype == libc::PROX_FDTYPE_VNODE as u32)
        {
            // SAFETY: the struct is plain data, and the call writes at most its size.
            let mut info: VnodeFdInfoWithPath = unsafe { std::mem::zeroed() };
            let size = std::mem::size_of_val(&info) as i32;
            let read = unsafe {
                libc::proc_pidfdinfo(
                    pid,
                    descriptor.proc_fd,
                    PROC_PIDFDVNODEPATHINFO,
                    (&mut info as *mut VnodeFdInfoWithPath).cast(),
                    size,
                )
            };
            if read == size {
                paths.push(reported_path(&info.path.vip_path));
                continue;
            }
            // A descriptor closed since it was listed holds nothing. macOS refuses (EPERM) the
            // path of a descriptor in a process it protects, such as a sandboxed system agent of
            // this user's; that process still counts by its working directory and arguments, so
            // a refusal there does not make every lock one only a person removes.
            match std::io::Error::last_os_error().raw_os_error() {
                Some(libc::EBADF | libc::EPERM) => {}
                _ => return Err(failed("descriptor path")),
            }
        }
        Ok(paths)
    })();
    // KERN_PROCARGS2 also fails, with EINVAL or EIO, for a live process in the middle of an exec
    // or an exit; such a failure is read again for a moment before it counts, or else every scan
    // that meets a process starting or ending says the lock can't be judged.
    let settled = std::time::Instant::now() + ARGUMENTS_SETTLE;
    let read = loop {
        match process_arguments(pid) {
            Some(read) => break Ok(read),
            None => {
                let transient = matches!(
                    std::io::Error::last_os_error().raw_os_error(),
                    Some(libc::EINVAL | libc::EIO)
                );
                if transient && std::time::Instant::now() < settled && live_and_own(pid, user) {
                    std::thread::yield_now();
                    continue;
                }
                break Err(failed("arguments"));
            }
        }
    };
    let (arguments, git_locations) = match read {
        Ok((arguments, git_locations)) => (Ok(arguments), Ok(git_locations)),
        Err(unread) => (Err(unread), Err(unread)),
    };
    Reading {
        cwd,
        open,
        arguments,
        git_locations,
    }
}

/// The arguments of `pid` and the [`GIT_LOCATIONS`] values in its environment, from
/// KERN_PROCARGS2: the argument count, the executable's path and its padding, the arguments, then
/// the environment. `None` when the call fails or its reply can't be parsed. macOS leaves out the
/// environment of its protected system binaries, so for those only the arguments count; Git's
/// environment is shown.
#[cfg(target_os = "macos")]
fn process_arguments(pid: i32) -> Option<(Vec<std::ffi::OsString>, Vec<std::ffi::OsString>)> {
    use std::os::unix::ffi::OsStrExt as _;
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut length = 0;
    // SAFETY: a null buffer asks only for the size of the reply.
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            std::ptr::null_mut(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return None;
    }
    let mut bytes = vec![0u8; length];
    // SAFETY: the buffer holds `length` bytes.
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            bytes.as_mut_ptr().cast(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return None;
    }
    bytes.truncate(length);
    let parsed = (|| -> Option<(Vec<std::ffi::OsString>, Vec<std::ffi::OsString>)> {
        let count = i32::from_ne_bytes(bytes.get(..4)?.try_into().ok()?);
        let count = usize::try_from(count).ok()?;
        let mut rest = &bytes[4..];
        rest = &rest[rest.iter().position(|byte| *byte == 0)? + 1..];
        while rest.first() == Some(&0) {
            rest = &rest[1..];
        }
        let mut arguments = Vec::with_capacity(count.min(4096));
        for _ in 0..count {
            let end = rest.iter().position(|byte| *byte == 0)?;
            arguments.push(std::ffi::OsStr::from_bytes(&rest[..end]).to_owned());
            rest = &rest[end + 1..];
        }
        Some((arguments, git_locations(rest.split(|byte| *byte == 0))))
    })();
    bytes.fill(0);
    parsed
}

/// On a platform agit doesn't read processes on, nothing can be shown.
#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
struct Places;

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
impl Places {
    fn of(_worktree: &Path, _git_dir: &Path) -> Self {
        Self
    }
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn reached(_places: &Places) -> Option<bool> {
    None
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;

    /// One process's reading decides alone whether it may write the index. A part that shows the
    /// process ended while it was read means it works nowhere, even after another part placed it
    /// in the checkout; a part a live process of this user refuses leaves it unknown; and an
    /// `--option=value` argument relative to its working directory names that place; and agit's
    /// own checkout file open in the Git directory doesn't. An implementation that counted a
    /// process ending mid-scan as unreadable would never clear a lock on a busy machine; one that
    /// skipped a refusing live process would take a lock that process may hold; one that read
    /// arguments only as absolute paths would miss a Git started next to the checkout; one that
    /// counted agit's own transaction lock would find recovery's lock in any child forked while
    /// recovery holds it, and keep the index lock for no Git.
    #[test]
    fn one_process_reading_decides_whether_it_may_write_the_index() {
        let places = Places::of(
            Path::new("/store/checkout"),
            Path::new("/store/checkout/.git"),
        );
        let elsewhere = || Reading {
            cwd: Ok("/".into()),
            open: Ok(vec!["/dev/null".into()]),
            arguments: Ok(vec!["cat".into()]),
            git_locations: Ok(vec![]),
        };
        assert_eq!(reach(&places, &elsewhere()), Reach::Outside);
        let ended = Reading {
            cwd: Ok("/store/checkout".into()),
            open: Err(Unread::Ended),
            ..elsewhere()
        };
        assert_eq!(reach(&places, &ended), Reach::Outside);
        let refused = Reading {
            git_locations: Err(Unread::Refused),
            ..elsewhere()
        };
        assert_eq!(reach(&places, &refused), Reach::Unknown);
        let beside = Reading {
            cwd: Ok("/store/elsewhere".into()),
            arguments: Ok(vec![
                "git".into(),
                "--git-dir=../checkout/./.git".into(),
                "update-index".into(),
            ]),
            ..elsewhere()
        };
        assert_eq!(reach(&places, &beside), Reach::Inside);
        let forked = Reading {
            open: Ok(vec![
                "/store/checkout/.git/agit-checkout-transaction.lock".into(),
            ]),
            ..elsewhere()
        };
        assert_eq!(reach(&places, &forked), Reach::Outside);
        let holding = Reading {
            open: Ok(vec!["/store/checkout/.git/index.lock".into()]),
            ..elsewhere()
        };
        assert_eq!(reach(&places, &holding), Reach::Inside);
    }
}
