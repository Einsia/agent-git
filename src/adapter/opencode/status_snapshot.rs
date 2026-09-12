//! Status never opens the source through SQLite or creates a coordination file.
//! Pinned read-only handles freeze cooperating SQLite writers while an in-memory image is copied.

use crate::adapter::native_snapshot::{Limits, Result, Unavailable};
use rusqlite::{Connection, DatabaseName, config::DbConfig, hooks, limits::Limit};
use std::fs::{File, Metadata, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub(crate) const MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const SQLITE_HEAP_BYTES: i64 = 32 * 1024 * 1024;
const MAX_VM_STEPS: usize = 2_000_000;
const READ_TIMEOUT: Duration = Duration::from_secs(5);
const PENDING_BYTE: u64 = 0x4000_0000;
const SHARED_FIRST: u64 = PENDING_BYTE + 2;
const SHARED_SIZE: u64 = 510;
const WAL_MUTATION_LOCKS: u64 = 120;

fn database_error(error: rusqlite::Error) -> Unavailable {
    match error.sqlite_error_code() {
        Some(
            rusqlite::ErrorCode::OutOfMemory
            | rusqlite::ErrorCode::TooBig
            | rusqlite::ErrorCode::OperationInterrupted,
        ) => Unavailable::BudgetExceeded,
        _ => Unavailable::Database,
    }
}
fn clock(deadline: Instant) -> Result<()> {
    if Instant::now() >= deadline {
        Err(Unavailable::BudgetExceeded)
    } else {
        Ok(())
    }
}
fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// The worker must retain one descriptor per inode: closing an alias drops POSIX process locks.
struct Carrier {
    path: PathBuf,
    file: File,
    before: Metadata,
}
impl Carrier {
    fn open(path: &Path) -> Result<Option<Self>> {
        let before = match std::fs::symlink_metadata(path) {
            Ok(value) => value,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(Unavailable::Read),
        };
        if !path.is_absolute() || !regular(&before) {
            return Err(Unavailable::Read);
        }
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.custom_flags(
                windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT,
            );
        }
        let file = options.open(path).map_err(|_| Unavailable::Read)?;
        let opened = file.metadata().map_err(|_| Unavailable::Read)?;
        if !same(&before, &opened) {
            return Err(Unavailable::Changed);
        }
        let result = Self {
            path: path.to_owned(),
            file,
            before: opened,
        };
        result.verify()?;
        Ok(Some(result))
    }
    fn verify(&self) -> Result<()> {
        let now = self.file.metadata().map_err(|_| Unavailable::Read)?;
        let path = std::fs::symlink_metadata(&self.path).map_err(|_| Unavailable::Changed)?;
        if !same(&self.before, &now) || !same(&self.before, &path) {
            return Err(Unavailable::Changed);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            use windows_sys::Win32::Storage::FileSystem::{
                FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
            };
            // Windows byte locks belong to their handle; this attribute-only handle cannot release them.
            let current = OpenOptions::new()
                .access_mode(FILE_READ_ATTRIBUTES)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
                .open(&self.path)
                .map_err(|_| Unavailable::Changed)?;
            if windows_identity(&self.file)? != windows_identity(&current)? {
                return Err(Unavailable::Changed);
            }
        }
        Ok(())
    }
}
fn regular(meta: &Metadata) -> bool {
    let valid = meta.file_type().is_file();
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        valid
            && meta.file_attributes()
                & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
                == 0
    }
    #[cfg(not(windows))]
    {
        valid
    }
}
fn same(a: &Metadata, b: &Metadata) -> bool {
    let valid = regular(a)
        && regular(b)
        && a.len() == b.len()
        && a.modified()
            .ok()
            .zip(b.modified().ok())
            .is_some_and(|(a, b)| a == b);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        valid
            && a.dev() == b.dev()
            && a.ino() == b.ino()
            && a.ctime() == b.ctime()
            && a.ctime_nsec() == b.ctime_nsec()
    }
    #[cfg(not(unix))]
    {
        valid
            && a.created()
                .ok()
                .zip(b.created().ok())
                .is_some_and(|(a, b)| a == b)
    }
}
#[cfg(windows)]
fn windows_identity(file: &File) -> Result<(u32, u32, u32)> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(Unavailable::Read);
    }
    Ok((
        info.dwVolumeSerialNumber,
        info.nFileIndexHigh,
        info.nFileIndexLow,
    ))
}

struct ReadLock<'a> {
    file: &'a File,
    start: u64,
    len: u64,
}
impl<'a> ReadLock<'a> {
    fn take(file: &'a File, start: u64, len: u64) -> Result<Self> {
        lock(file, start, len, false)?;
        Ok(Self { file, start, len })
    }
}
impl Drop for ReadLock<'_> {
    fn drop(&mut self) {
        let _ = lock(self.file, self.start, self.len, true);
    }
}
#[cfg(unix)]
fn lock(file: &File, start: u64, len: u64, release: bool) -> Result<()> {
    use std::os::fd::AsRawFd;
    let mut range: libc::flock = unsafe { std::mem::zeroed() };
    range.l_type = if release {
        libc::F_UNLCK
    } else {
        libc::F_RDLCK
    } as _;
    range.l_whence = libc::SEEK_SET as _;
    range.l_start = start as _;
    range.l_len = len as _;
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &range) } != 0 {
        return Err(Unavailable::Changed);
    }
    Ok(())
}
#[cfg(windows)]
fn lock(file: &File, start: u64, len: u64, release: bool) -> Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::{
        Storage::FileSystem::{LOCKFILE_FAIL_IMMEDIATELY, LockFileEx, UnlockFileEx},
        System::IO::OVERLAPPED,
    };
    let mut position: OVERLAPPED = unsafe { std::mem::zeroed() };
    position.Anonymous.Anonymous = windows_sys::Win32::System::IO::OVERLAPPED_0_0 {
        Offset: start as u32,
        OffsetHigh: (start >> 32) as u32,
    };
    let ok = unsafe {
        if release {
            UnlockFileEx(
                file.as_raw_handle(),
                0,
                len as u32,
                (len >> 32) as u32,
                &mut position,
            )
        } else {
            LockFileEx(
                file.as_raw_handle(),
                LOCKFILE_FAIL_IMMEDIATELY,
                0,
                len as u32,
                (len >> 32) as u32,
                &mut position,
            )
        }
    };
    if ok == 0 {
        return Err(Unavailable::Changed);
    }
    Ok(())
}
#[cfg(not(any(unix, windows)))]
fn lock(_: &File, _: u64, _: u64, _: bool) -> Result<()> {
    Err(Unavailable::Unsupported)
}

/// An orphan SHM must remain eligible for SQLite's first-opener reset.
fn live_dms(file: &File) -> Result<ReadLock<'_>> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let mut range: libc::flock = unsafe { std::mem::zeroed() };
        range.l_type = libc::F_WRLCK as _;
        range.l_whence = libc::SEEK_SET as _;
        range.l_start = 128;
        range.l_len = 1;
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETLK, &mut range) } != 0
            || i64::from(range.l_type) != i64::from(libc::F_RDLCK)
        {
            return Err(Unavailable::Changed);
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::{
            Foundation::{ERROR_LOCK_VIOLATION, GetLastError},
            Storage::FileSystem::{
                LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx, UnlockFileEx,
            },
            System::IO::OVERLAPPED,
        };
        let mut position: OVERLAPPED = unsafe { std::mem::zeroed() };
        position.Anonymous.Anonymous = windows_sys::Win32::System::IO::OVERLAPPED_0_0 {
            Offset: 128,
            OffsetHigh: 0,
        };
        let obtained = unsafe {
            LockFileEx(
                file.as_raw_handle(),
                LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
                0,
                1,
                0,
                &mut position,
            )
        };
        if obtained != 0 {
            unsafe {
                UnlockFileEx(file.as_raw_handle(), 0, 1, 0, &mut position);
            }
            return Err(Unavailable::Changed);
        }
        if unsafe { GetLastError() } != ERROR_LOCK_VIOLATION {
            return Err(Unavailable::Read);
        }
    }
    ReadLock::take(file, 128, 1)
}

fn absent(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        _ => Err(Unavailable::Changed),
    }
}
fn be(bytes: &[u8]) -> u32 {
    u32::from_be_bytes(bytes[..4].try_into().expect("fixed-width integer"))
}
fn native(bytes: &[u8]) -> u32 {
    u32::from_ne_bytes(bytes[..4].try_into().expect("fixed-width integer"))
}
fn checksum(bytes: &[u8], big: bool, mut sum: [u32; 2]) -> [u32; 2] {
    for pair in bytes.chunks_exact(8) {
        let word = |b: &[u8]| {
            if big {
                be(b)
            } else {
                u32::from_le_bytes(b[..4].try_into().expect("fixed-width integer"))
            }
        };
        sum[0] = sum[0].wrapping_add(word(pair)).wrapping_add(sum[1]);
        sum[1] = sum[1].wrapping_add(word(&pair[4..])).wrapping_add(sum[0]);
    }
    sum
}

struct Frontier {
    page: usize,
    big_checksum: bool,
    frames: usize,
    pages: usize,
    salt: [u8; 8],
    checksum: [u32; 2],
}
fn frontier(shm: &[u8]) -> Result<Frontier> {
    if shm.len() < 96
        || shm[..48] != shm[48..96]
        || native(shm) != 3_007_000
        || shm[12] != 1
        || shm[13] > 1
        || checksum(&shm[..40], cfg!(target_endian = "big"), [0; 2])
            != [native(&shm[40..]), native(&shm[44..])]
    {
        return Err(Unavailable::Database);
    }
    let page = u16::from_ne_bytes([shm[14], shm[15]]);
    Ok(Frontier {
        page: if page == 1 { 65_536 } else { page as usize },
        big_checksum: shm[13] != 0,
        frames: native(&shm[16..]) as usize,
        pages: native(&shm[20..]) as usize,
        salt: shm[32..40].try_into().expect("fixed-width salt"),
        checksum: [native(&shm[24..]), native(&shm[28..])],
    })
}

fn image(
    mut db: Vec<u8>,
    wal: &[u8],
    frontier: Option<&Frontier>,
    deadline: Instant,
) -> Result<Vec<u8>> {
    if db.len() < 100
        || &db[..16] != b"SQLite format 3\0"
        || !matches!((db[18], db[19]), (1, 1) | (2, 2))
    {
        return Err(Unavailable::Database);
    }
    let encoded = u16::from_be_bytes([db[16], db[17]]);
    let page = if encoded == 1 {
        65_536
    } else {
        encoded as usize
    };
    if !(512..=65_536).contains(&page) || !page.is_power_of_two() || !db.len().is_multiple_of(page)
    {
        return Err(Unavailable::Database);
    }
    let base_pages = db.len() / page;
    let mut committed_pages = base_pages;
    if !wal.is_empty() {
        if db[18] != 2
            || wal.len() < 32
            || !matches!(be(wal), 0x377f0682 | 0x377f0683)
            || be(&wal[4..]) != 3_007_000
            || be(&wal[8..]) as usize != page
        {
            return Err(Unavailable::Database);
        }
        let big = be(wal) & 1 != 0;
        if frontier.is_some_and(|f| f.page != page || f.big_checksum != big) {
            return Err(Unavailable::Database);
        }
        let mut sum = checksum(&wal[..24], big, [0; 2]);
        if sum != [be(&wal[24..]), be(&wal[28..])] {
            return Err(Unavailable::Database);
        }
        let stride = page + 24;
        let available = (wal.len() - 32) / stride;
        let expected = frontier.map(|f| f.frames);
        if expected.is_some_and(|n| n > available) {
            return Err(Unavailable::Incomplete);
        }
        let mut frames = Vec::new();
        let mut committed = 0;
        let mut committed_sum = sum;
        for n in 0..available {
            clock(deadline)?;
            let frame = &wal[32 + n * stride..32 + (n + 1) * stride];
            let mut next = checksum(&frame[..8], big, sum);
            next = checksum(&frame[24..], big, next);
            if frame[8..16] != wal[16..24] || next != [be(&frame[16..]), be(&frame[20..])] {
                if expected.is_some_and(|end| n < end) {
                    return Err(Unavailable::Database);
                } else {
                    break;
                }
            }
            sum = next;
            let pgno = be(frame) as usize;
            if pgno == 0 || pgno > MAX_IMAGE_BYTES / page {
                return Err(Unavailable::BudgetExceeded);
            }
            frames.push((pgno, 32 + n * stride + 24));
            if be(&frame[4..]) > 0 {
                committed = frames.len();
                committed_sum = sum;
                committed_pages = be(&frame[4..]) as usize;
            }
        }
        if let Some(frontier) = frontier
            && (frontier.salt != wal[16..24]
                || frontier.frames != committed
                || (committed > 0
                    && (frontier.pages != committed_pages || frontier.checksum != committed_sum)))
        {
            return Err(Unavailable::Database);
        }
        if committed_pages == 0 || committed_pages > MAX_IMAGE_BYTES / page {
            return Err(Unavailable::BudgetExceeded);
        }
        let length = committed_pages * page;
        db.try_reserve_exact(length.saturating_sub(db.len()))
            .map_err(|_| Unavailable::BudgetExceeded)?;
        db.resize(length, 0);
        let mut filled = vec![false; committed_pages.saturating_sub(base_pages)];
        for &(pgno, offset) in &frames[..committed] {
            if pgno > committed_pages {
                continue;
            }
            if pgno > base_pages {
                filled[pgno - base_pages - 1] = true;
            }
            db[(pgno - 1) * page..pgno * page].copy_from_slice(&wal[offset..offset + page]);
        }
        if filled.iter().any(|filled| !filled) {
            return Err(Unavailable::Incomplete);
        }
    } else if frontier.is_some_and(|f| f.frames != 0) {
        return Err(Unavailable::Incomplete);
    }
    if be(&db[24..]) != be(&db[92..]) || be(&db[28..]) as usize != committed_pages {
        return Err(Unavailable::Database);
    }
    // Only the owned image leaves WAL mode; SQLite never receives a source pathname.
    db[18] = 1;
    db[19] = 1;
    Ok(db)
}

fn acquire(path: &Path, deadline: Instant) -> Result<Vec<u8>> {
    acquire_with(path, deadline, || {})
}

fn acquire_with(path: &Path, deadline: Instant, after_read: impl FnOnce()) -> Result<Vec<u8>> {
    let db = Carrier::open(path)?.ok_or(Unavailable::NotFound)?;
    // Holding the pending byte only while acquiring the shared range avoids blocking new readers.
    let pending = ReadLock::take(&db.file, PENDING_BYTE, 1)?;
    let shared = ReadLock::take(&db.file, SHARED_FIRST, SHARED_SIZE)?;
    drop(pending);
    let shm_path = sidecar(path, "-shm");
    let wal_path = sidecar(path, "-wal");
    let journal = sidecar(path, "-journal");
    absent(&journal)?;
    let shm = Carrier::open(&shm_path)?;
    let mutation = shm
        .as_ref()
        .map(|shm| ReadLock::take(&shm.file, WAL_MUTATION_LOCKS, 3))
        .transpose()?;
    // Prove another DMS holder only after writes/recovery/checkpoints are excluded.
    let dms = shm.as_ref().map(|shm| live_dms(&shm.file)).transpose()?;
    // Neither a checkpoint nor recovery may change the database while its pages are copied.
    // If SHM is absent, the shared DB lock blocks exclusive-mode writers and final absence is rechecked.
    let wal = Carrier::open(&wal_path)?;
    // A recoverable WAL prefix does not prove the live published frontier without its trusted index.
    if wal.is_some() && shm.is_none() {
        return Err(Unavailable::Changed);
    }
    let mut allowance = MAX_IMAGE_BYTES;
    // Read locks borrow the handles, so copying uses separate immutable handle references.
    let db_before = db.file.metadata().map_err(|_| Unavailable::Read)?;
    let shm_header = if let Some(shm) = &shm {
        if shm.before.len() < 96 || shm.before.len() > MAX_IMAGE_BYTES as u64 {
            return Err(Unavailable::Database);
        }
        let mut bytes = [0; 96];
        (&shm.file)
            .read_exact(&mut bytes)
            .map_err(|_| Unavailable::Read)?;
        Some(bytes)
    } else {
        None
    };
    let end = shm_header
        .as_ref()
        .map(|bytes| frontier(bytes))
        .transpose()?;
    // Do not close or clone a source handle while its process-scoped lock is held.
    let read = |file: &File, len: u64, allowance: &mut usize| -> Result<Vec<u8>> {
        clock(deadline)?;
        let len = usize::try_from(len).map_err(|_| Unavailable::BudgetExceeded)?;
        *allowance = allowance
            .checked_sub(len)
            .ok_or(Unavailable::BudgetExceeded)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(len)
            .map_err(|_| Unavailable::BudgetExceeded)?;
        file.take(len as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| Unavailable::Read)?;
        if bytes.len() != len {
            return Err(Unavailable::Changed);
        }
        Ok(bytes)
    };
    let db_bytes = read(&db.file, db_before.len(), &mut allowance)?;
    let wal_bytes = wal
        .as_ref()
        .map(|wal| read(&wal.file, wal.before.len(), &mut allowance))
        .transpose()?
        .unwrap_or_default();
    after_read();
    verify_bytes(&db, &db_bytes, deadline)?;
    if let Some(wal) = &wal {
        verify_bytes(wal, &wal_bytes, deadline)?;
    }
    if !same(
        &db_before,
        &db.file.metadata().map_err(|_| Unavailable::Read)?,
    ) {
        return Err(Unavailable::Changed);
    }
    db.verify()?;
    if let Some(wal) = &wal {
        wal.verify()?;
    } else {
        absent(&wal_path)?;
    }
    if let Some(shm) = &shm {
        let mut handle = &shm.file;
        handle
            .seek(SeekFrom::Start(0))
            .map_err(|_| Unavailable::Read)?;
        let mut final_header = [0; 96];
        handle
            .read_exact(&mut final_header)
            .map_err(|_| Unavailable::Changed)?;
        if Some(final_header) != shm_header {
            return Err(Unavailable::Changed);
        }
        shm.verify()?;
    } else {
        absent(&shm_path)?;
    }
    absent(&journal)?;
    let result = image(db_bytes, &wal_bytes, end.as_ref(), deadline);
    drop(mutation);
    drop(dms);
    drop(shared);
    result
}

fn verify_bytes(carrier: &Carrier, bytes: &[u8], deadline: Instant) -> Result<()> {
    let mut handle = &carrier.file;
    handle
        .seek(SeekFrom::Start(0))
        .map_err(|_| Unavailable::Read)?;
    let mut buffer = [0; 8192];
    for expected in bytes.chunks(buffer.len()) {
        clock(deadline)?;
        handle
            .read_exact(&mut buffer[..expected.len()])
            .map_err(|_| Unavailable::Changed)?;
        if buffer[..expected.len()] != *expected {
            return Err(Unavailable::Changed);
        }
    }
    carrier.verify()
}

/// Called only in the isolated status worker, whose parent owns the hard I/O deadline.
pub(crate) fn read(id: &str, limits: Limits) -> Result<Vec<u8>> {
    super::super::native_snapshot::validate_id(id, limits)?;
    let deadline = Instant::now() + READ_TIMEOUT;
    let path = super::db_path().ok_or(Unavailable::NotFound)?;
    let bytes = acquire(&path, deadline)?;
    query(bytes, id, limits, deadline)
}
fn query(bytes: Vec<u8>, id: &str, limits: Limits, deadline: Instant) -> Result<Vec<u8>> {
    let mut db = Connection::open_in_memory().map_err(database_error)?;
    db.set_limit(Limit::SQLITE_LIMIT_LENGTH, limits.bytes as i32);
    db.set_limit(Limit::SQLITE_LIMIT_SQL_LENGTH, 16 * 1024);
    db.set_limit(Limit::SQLITE_LIMIT_COLUMN, 64);
    db.set_limit(Limit::SQLITE_LIMIT_EXPR_DEPTH, 64);
    db.set_limit(Limit::SQLITE_LIMIT_VDBE_OP, 25_000);
    db.set_limit(Limit::SQLITE_LIMIT_VARIABLE_NUMBER, 16);
    db.set_limit(Limit::SQLITE_LIMIT_ATTACHED, 0);
    db.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)
        .map_err(database_error)?;
    db.set_db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA, false)
        .map_err(database_error)?;
    db.execute_batch("PRAGMA temp_store=MEMORY; PRAGMA cache_size=-1024; PRAGMA mmap_size=0;")
        .map_err(database_error)?;
    let pointer = std::ptr::NonNull::new(
        unsafe { rusqlite::ffi::sqlite3_malloc64(bytes.len() as u64) }.cast::<u8>(),
    )
    .ok_or(Unavailable::BudgetExceeded)?;
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer.as_ptr(), bytes.len());
    }
    let owned = unsafe { rusqlite::serialize::OwnedData::from_raw_nonnull(pointer, bytes.len()) };
    drop(bytes);
    db.deserialize(DatabaseName::Main, owned, true)
        .map_err(database_error)?;
    let mut work = 0usize;
    let exhausted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let progress_exhausted = exhausted.clone();
    db.progress_handler(
        1000,
        Some(move || {
            work += 1000;
            let stop = work > MAX_VM_STEPS || Instant::now() >= deadline;
            progress_exhausted.store(stop, std::sync::atomic::Ordering::Relaxed);
            stop
        }),
    );
    db.execute_batch("PRAGMA query_only=ON;")
        .map_err(database_error)?;
    db.authorizer(Some(|context: hooks::AuthContext<'_>| {
        match context.action {
            hooks::AuthAction::Select => hooks::Authorization::Allow,
            hooks::AuthAction::Read { table_name, .. }
                if context.database_name == Some("main")
                    && matches!(table_name, "session" | "message" | "part" | "sqlite_master") =>
            {
                hooks::Authorization::Allow
            }
            _ => hooks::Authorization::Deny,
        }
    }));
    {
        let mut statement=db.prepare("SELECT name, type, rootpage FROM sqlite_schema WHERE name IN ('session','message','part')").map_err(database_error)?;
        let mut rows = statement.query([]).map_err(database_error)?;
        let mut names = std::collections::BTreeSet::new();
        while let Some(row) = rows.next().map_err(database_error)? {
            let name: String = row.get(0).map_err(database_error)?;
            let kind: String = row.get(1).map_err(database_error)?;
            let root: i64 = row.get(2).map_err(database_error)?;
            if kind != "table" || root <= 0 || !names.insert(name) {
                return Err(Unavailable::Database);
            }
        }
        if names.len() != 3 {
            return Err(Unavailable::Database);
        }
    }
    let result = super::native_snapshot::materialize(&db, id, limits);
    clock(deadline)?;
    if exhausted.load(std::sync::atomic::Ordering::Relaxed) {
        return Err(Unavailable::BudgetExceeded);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn fixture() -> (tempfile::TempDir, PathBuf, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().canonicalize().unwrap().join("opencode.db");
        let db = Connection::open(&path).unwrap();
        db.execute_batch("PRAGMA journal_mode=WAL;
            CREATE TABLE session(id TEXT PRIMARY KEY, project_id TEXT, parent_id TEXT, directory TEXT, time_created INTEGER, version TEXT);
            CREATE TABLE message(id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT);
            CREATE TABLE part(id TEXT PRIMARY KEY, session_id TEXT, message_id TEXT, time_created INTEGER, data TEXT);
            INSERT INTO session VALUES('ses_selected','global',NULL,'/fixture',1,'1.18.13');
            INSERT INTO message VALUES('msg_user','ses_selected',2,'{\"role\":\"user\"}');
            INSERT INTO part VALUES('prt_text','ses_selected','msg_user',3,'{\"type\":\"text\",\"text\":\"original\"}');
            PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
        (dir, path, db)
    }
    fn files(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        std::fs::read_dir(dir.canonicalize().unwrap())
            .unwrap()
            .map(|entry| {
                let path = entry.unwrap().path();
                let bytes = std::fs::read(&path).unwrap();
                (path, bytes)
            })
            .collect()
    }
    fn until() -> Instant {
        Instant::now() + READ_TIMEOUT
    }
    fn materialized(bytes: Vec<u8>) -> String {
        String::from_utf8(query(bytes, "ses_selected", Limits::default(), until()).unwrap())
            .unwrap()
    }

    #[test]
    fn database_alone_is_copied_to_memory_without_sidecars_or_source_writes() {
        let (dir, path, db) = fixture();
        drop(db);
        assert!(!sidecar(&path, "-wal").exists() && !sidecar(&path, "-shm").exists());
        let before = files(dir.path());
        let text = materialized(acquire(&path, until()).unwrap());
        assert!(text.contains("original") && text.contains("ses_selected"));
        assert_eq!(files(dir.path()), before);
        assert_eq!(
            acquire(&path, Instant::now()),
            Err(Unavailable::BudgetExceeded)
        );
        assert_eq!(files(dir.path()), before);
    }

    #[test]
    fn wal_commit_replay_rejects_stale_index_torn_frames_and_missing_pages() {
        let (dir, path, db) = fixture();
        db.execute(
            "UPDATE part SET data='{\"type\":\"text\",\"text\":\"first revision\"}'",
            [],
        )
        .unwrap();
        let old_index = std::fs::read(sidecar(&path, "-shm")).unwrap();
        db.execute(
            "UPDATE part SET data='{\"type\":\"text\",\"text\":\"second revision\"}'",
            [],
        )
        .unwrap();
        let before = files(dir.path());
        let base = std::fs::read(&path).unwrap();
        let wal = std::fs::read(sidecar(&path, "-wal")).unwrap();
        let index = std::fs::read(sidecar(&path, "-shm")).unwrap();
        let current = frontier(&index).unwrap();
        let text = materialized(image(base.clone(), &wal, Some(&current), until()).unwrap());
        assert!(text.contains("second revision") && !text.contains("first revision"));
        assert!(
            image(
                base.clone(),
                &wal,
                Some(&frontier(&old_index).unwrap()),
                until()
            )
            .is_err()
        );
        assert!(image(base.clone(), &wal[..wal.len() - 1], Some(&current), until()).is_err());
        let mut corrupt = wal.clone();
        corrupt[40] ^= 1;
        assert!(image(base.clone(), &corrupt, Some(&current), until()).is_err());
        let mut reset = index.clone();
        reset[48] = 0;
        assert!(frontier(&reset).is_err());
        let mut missing = base;
        missing.truncate(4096);
        assert!(image(missing, &wal, Some(&current), until()).is_err());
        assert_eq!(files(dir.path()), before);
    }

    #[test]
    fn orphaned_wal_index_cannot_claim_a_live_recovery_boundary() {
        let (dir, path, db) = fixture();
        db.execute(
            "UPDATE part SET data='{\"type\":\"text\",\"text\":\"committed\"}'",
            [],
        )
        .unwrap();
        let before = files(dir.path());
        drop(db);
        for (path, bytes) in &before {
            std::fs::write(path, bytes).unwrap();
        }
        assert_eq!(files(dir.path()), before);
        // No external DMS holder survives the connection's close, even though both index copies are valid.
        assert_eq!(acquire(&path, until()), Err(Unavailable::Changed));
        assert_eq!(files(dir.path()), before);
    }

    #[test]
    fn wal_without_a_trusted_index_requires_recovery_elsewhere() {
        let (dir, path, db) = fixture();
        db.execute(
            "UPDATE part SET data='{\"type\":\"text\",\"text\":\"committed\"}'",
            [],
        )
        .unwrap();
        let original = files(dir.path());
        let wal_path = sidecar(&path, "-wal");
        assert!(original[&wal_path].len() > 32);
        drop(db);
        for wal in [original[&wal_path].clone(), Vec::new()] {
            std::fs::write(&path, &original[&path]).unwrap();
            std::fs::write(&wal_path, wal).unwrap();
            assert!(!sidecar(&path, "-shm").exists());
            let before = files(dir.path());
            assert_eq!(acquire(&path, until()), Err(Unavailable::Changed));
            assert_eq!(files(dir.path()), before);
        }
    }

    #[test]
    fn unsafe_or_changed_carriers_cannot_establish_empty_activity() {
        let (dir, path, db) = fixture();
        drop(db);
        let original = files(dir.path());
        std::fs::write(sidecar(&path, "-journal"), b"rollback evidence").unwrap();
        let journal = files(dir.path());
        assert!(acquire(&path, until()).is_err());
        assert_eq!(files(dir.path()), journal);
        std::fs::remove_file(sidecar(&path, "-journal")).unwrap();
        assert_eq!(
            acquire_with(&path, until(), || {
                std::fs::write(sidecar(&path, "-shm"), b"new carrier").unwrap();
            }),
            Err(Unavailable::Changed)
        );
        std::fs::remove_file(sidecar(&path, "-shm")).unwrap();
        let bytes = original[&path].clone();
        assert_eq!(
            acquire_with(&path, until(), || {
                let mut changed = bytes.clone();
                changed[60] ^= 1;
                std::fs::write(&path, changed).unwrap();
            }),
            Err(Unavailable::Changed)
        );
        std::fs::write(&path, &bytes).unwrap();
        let oversized = OpenOptions::new().write(true).open(&path).unwrap();
        oversized.set_len(MAX_IMAGE_BYTES as u64 + 1).unwrap();
        drop(oversized);
        assert_eq!(acquire(&path, until()), Err(Unavailable::BudgetExceeded));
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(files(dir.path()), original);
    }

    #[test]
    fn in_memory_queries_retain_record_limits_and_reject_views() {
        let (_dir, path, db) = fixture();
        drop(db);
        let bytes = acquire(&path, until()).unwrap();
        for limits in [
            Limits {
                records: 1,
                ..Limits::default()
            },
            Limits {
                bytes: 8,
                ..Limits::default()
            },
            Limits {
                working_bytes: 1,
                ..Limits::default()
            },
        ] {
            assert!(query(bytes.clone(), "ses_selected", limits, until()).is_err());
        }
        assert_eq!(
            query(bytes, "ses_missing", Limits::default(), until()),
            Err(Unavailable::NotFound)
        );
        let db = Connection::open(&path).unwrap();
        db.execute_batch("DROP TABLE part; CREATE VIEW part AS SELECT 1 AS id;")
            .unwrap();
        drop(db);
        assert!(
            query(
                acquire(&path, until()).unwrap(),
                "ses_selected",
                Limits::default(),
                until()
            )
            .is_err()
        );
    }
}
