//! Codex keeps a cold rollout as `<name>.jsonl.zst` and restores `<name>.jsonl` before it appends.
//!
//! Readers here address the plain carrier. A compressed rollout is restored the way Codex restores
//! it for append: decoded beside the plain path, published without clobbering a writer that
//! restored it first, and only then is the compressed copy retired. Keeping the plain name keeps
//! every path the state database, pending archives and `history_base` offsets already name valid;
//! reading the frame in place would hand out a carrier that Codex deletes as soon as it resumes
//! the thread.

#[cfg(feature = "rc")]
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs::File;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};

const COMPRESSED_SUFFIX: &str = ".zst";
/// Header readers stop here. A `session_meta` record carries the base instructions and can be
/// far larger than a page of ordinary records.
pub(crate) const HEADER_LIMIT: u64 = 1024 * 1024;
// Codex's stale-temporary sweep removes `*.tmp`; a restoration in flight must not match it.
const RESTORE_PREFIX: &str = ".rollout-materialize-";

/// A caller that cannot wait found a cold carrier; its restoration continues in the background.
#[cfg(feature = "rc")]
#[derive(Debug, thiserror::Error)]
#[error("the conversation transcript is being restored from its compressed copy")]
pub(crate) struct Restoring;

struct Restorations {
    running: BTreeSet<PathBuf>,
    // The next caller receives a background failure instead of another busy answer, so a carrier
    // that cannot be decoded never turns into an endless restoring state.
    #[cfg(feature = "rc")]
    failed: BTreeMap<PathBuf, (io::ErrorKind, String)>,
}

static RESTORATIONS: Mutex<Restorations> = Mutex::new(Restorations {
    running: BTreeSet::new(),
    #[cfg(feature = "rc")]
    failed: BTreeMap::new(),
});
static RESTORED: Condvar = Condvar::new();

fn restorations() -> MutexGuard<'static, Restorations> {
    RESTORATIONS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One restoration per carrier: a concurrent reader waits for it instead of decoding again.
struct Turn(PathBuf);

impl Turn {
    fn take(plain: &Path) -> Self {
        let mut state = restorations();
        while state.running.contains(plain) {
            state = RESTORED.wait(state).unwrap_or_else(PoisonError::into_inner);
        }
        state.running.insert(plain.to_owned());
        Self(plain.to_owned())
    }
}

impl Drop for Turn {
    fn drop(&mut self) {
        restorations().running.remove(&self.0);
        RESTORED.notify_all();
    }
}

/// The turn [`begin_restore`] claimed for a background restoration. Its failure is recorded in
/// the same step that releases the turn: a caller that found the carrier neither restoring nor
/// failed in between would start a second attempt, and that attempt's failure would reach the
/// caller after the one the first failure was returned to.
#[cfg(feature = "rc")]
struct Background {
    plain: PathBuf,
    failure: Option<(io::ErrorKind, String)>,
}

#[cfg(feature = "rc")]
impl Drop for Background {
    fn drop(&mut self) {
        let mut state = restorations();
        if let Some(failure) = self.failure.take() {
            state.failed.insert(self.plain.clone(), failure);
        }
        state.running.remove(&self.plain);
        drop(state);
        RESTORED.notify_all();
    }
}

/// Removes restoration temporaries a crash left behind under a Codex home.
///
/// Only this module's own temporaries are removed, and only once their status has not changed
/// for `age`. A restoration in progress keeps changing its temporary's status until it is
/// published, so `age` only has to exceed the longest restoration.
#[cfg(feature = "rc")]
pub(crate) fn sweep_restorations(home: &Path, age: std::time::Duration) {
    for root in [home.join("sessions"), home.join("archived_sessions")] {
        for entry in walkdir::WalkDir::new(root).into_iter().flatten() {
            let stale = entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(RESTORE_PREFIX))
                && entry.file_type().is_file()
                && entry
                    .metadata()
                    .ok()
                    .and_then(|metadata| changed(&metadata))
                    .and_then(|changed| changed.elapsed().ok())
                    .is_some_and(|elapsed| elapsed > age);
            if stale {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// The last status change: restoring sets the temporary's modification time to the original's,
/// so that time says nothing about whether a restoration is still running.
#[cfg(feature = "rc")]
fn changed(metadata: &std::fs::Metadata) -> Option<std::time::SystemTime> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let seconds = u64::try_from(metadata.ctime()).ok()?;
        let nanos = u32::try_from(metadata.ctime_nsec()).ok()?;
        Some(std::time::UNIX_EPOCH + std::time::Duration::new(seconds, nanos))
    }
    #[cfg(not(unix))]
    {
        metadata.created().ok()
    }
}

/// The plain carrier for a plain or compressed rollout path.
pub(crate) fn plain_path(path: &Path) -> PathBuf {
    match path.file_name().and_then(|name| name.to_str()) {
        Some(name) if is_compressed_name(name) => {
            path.with_file_name(&name[..name.len() - COMPRESSED_SUFFIX.len()])
        }
        _ => path.to_owned(),
    }
}

pub(crate) fn is_compressed(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(is_compressed_name)
}

fn is_compressed_name(name: &str) -> bool {
    name.ends_with(".jsonl.zst")
}

fn compressed_path(plain: &Path) -> PathBuf {
    let mut name = plain.file_name().unwrap_or_default().to_os_string();
    name.push(COMPRESSED_SUFFIX);
    plain.with_file_name(name)
}

/// Only the compressed carrier is on disk.
#[cfg(feature = "rc")]
pub(crate) fn is_cold(plain: &Path) -> bool {
    std::fs::symlink_metadata(plain).is_err()
        && std::fs::symlink_metadata(compressed_path(plain))
            .is_ok_and(|metadata| metadata.file_type().is_file())
}

/// Starts restoring a cold carrier without waiting for it.
///
/// Returns true while the plain carrier is not yet readable. A caller holding a lock that other
/// conversations need reports [`Restoring`] then, so decoding a large rollout never stalls them.
/// The failure of a background attempt is returned to exactly one caller; the next call starts
/// over.
#[cfg(feature = "rc")]
pub(crate) fn begin_restore(path: &Path) -> io::Result<bool> {
    let plain = plain_path(path);
    let mut state = restorations();
    if let Some((kind, message)) = state.failed.remove(&plain) {
        return Err(io::Error::new(kind, message));
    }
    if !is_cold(&plain) {
        return Ok(false);
    }
    // The turn is claimed before the lock is released, so no other caller starts an attempt.
    if state.running.insert(plain.clone()) {
        drop(state);
        let claim = Background {
            plain,
            failure: None,
        };
        std::thread::Builder::new()
            .name("rollout-restore".into())
            .spawn(move || {
                let mut claim = claim;
                claim.failure = restore_held(&claim.plain)
                    .err()
                    .map(|error| (error.kind(), error.to_string()));
                drop(claim);
            })?;
    }
    Ok(true)
}

/// Returns the plain carrier, restoring it first when only its compressed form is on disk.
///
/// An existing plain entry always wins, as it does for Codex, so a live writer's file is never
/// replaced. A missing pair yields the plain path and leaves the caller's own not-found handling
/// in charge.
pub(crate) fn restore(path: &Path) -> io::Result<PathBuf> {
    let plain = plain_path(path);
    if std::fs::symlink_metadata(&plain).is_ok() {
        return Ok(plain);
    }
    let _turn = Turn::take(&plain);
    restore_held(&plain)
}

/// [`restore`] for a caller that holds the carrier's turn.
fn restore_held(plain: &Path) -> io::Result<PathBuf> {
    let plain = plain.to_owned();
    if std::fs::symlink_metadata(&plain).is_ok() {
        return Ok(plain);
    }
    let compressed = compressed_path(&plain);
    match std::fs::symlink_metadata(&compressed) {
        Ok(metadata) if metadata.file_type().is_file() => {}
        Ok(_) => return Ok(plain),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(plain),
        Err(error) => return Err(error),
    }
    let input = File::open(&compressed)?;
    let metadata = input.metadata()?;
    let directory = plain.parent().unwrap_or_else(|| Path::new("."));
    let mut output =
        tempfile::Builder::new()
            .prefix(RESTORE_PREFIX)
            .make_in(directory, |path| {
                // A temporary-file attribute would survive hard-link publication on Windows.
                let mut options = std::fs::OpenOptions::new();
                options.write(true).create_new(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o600);
                }
                options.open(path)
            })?;
    decode(&input, output.as_file_mut())?;
    output.as_file_mut().flush()?;
    output.as_file().set_permissions(metadata.permissions())?;
    // Restoration is not activity: file-time ordering must keep the thread where it was.
    output
        .as_file()
        .set_times(std::fs::FileTimes::new().set_modified(metadata.modified()?))?;
    output.as_file().sync_all()?;
    publish_noclobber(output, &plain)?;
    drop(input);
    match std::fs::remove_file(&compressed) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    Ok(plain)
}

/// The first record of a plain or compressed carrier, without restoring it.
///
/// A compressed carrier is decoded only as far as its first newline, so a listing or a lineage
/// walk costs the header, never the rollout. The line keeps its newline when it is complete;
/// a missing newline means the header is incomplete or longer than `limit`.
pub(crate) fn first_line(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    use io::Read;
    let file = File::open(path)?;
    let mut line = Vec::new();
    if !is_compressed(path) {
        io::BufReader::new(file.take(limit)).read_until(b'\n', &mut line)?;
        return Ok(line);
    }
    let mut input = io::BufReader::new(file);
    while (line.len() as u64) < limit && !line.ends_with(b"\n") {
        let frame =
            ruzstd::decoding::StreamingDecoder::new(&mut input).map_err(io::Error::other)?;
        io::BufReader::new(frame.take(limit - line.len() as u64)).read_until(b'\n', &mut line)?;
        if line.ends_with(b"\n") || input.fill_buf()?.is_empty() {
            break;
        }
    }
    Ok(line)
}

/// A rollout carrier as one listing entry: a compressed carrier counts only while its plain
/// name is absent, because the two are one rollout and the plain one is current.
pub(crate) fn is_carrier(path: &Path) -> bool {
    if is_compressed(path) {
        std::fs::symlink_metadata(plain_path(path))
            .is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
    } else {
        path.extension()
            .is_some_and(|extension| extension == "jsonl")
    }
}

/// Whether `next` is the segment Codex opened to continue the rollout carried by `current`.
///
/// Codex rolls a long thread over into a new carrier at a turn start. The new carrier's header
/// is paginated history whose base names the carrier before it by its segment id; nothing else
/// proves that a different file is the same conversation.
#[cfg(feature = "rc")]
pub(crate) fn continues(next: &Path, current: &Path) -> bool {
    let Some(segment) = super::codex::id_from_filename(current) else {
        return false;
    };
    first_line(next, HEADER_LIMIT)
        .ok()
        .and_then(|line| serde_json::from_slice::<serde_json::Value>(&line).ok())
        .is_some_and(|header| {
            header["type"] == "session_meta"
                && header["payload"]["history_mode"] == "paginated"
                && header["payload"]["history_base"]["thread_id"] == segment.as_str()
        })
}

/// The decoded bytes of a plain or compressed carrier, for inspection that must not restore it.
#[cfg(feature = "rc")]
pub(crate) fn read_decoded(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    use io::Read;
    let file = File::open(path)?;
    let mut bytes = Vec::new();
    if !is_compressed(path) {
        file.take(limit.saturating_add(1)).read_to_end(&mut bytes)?;
    } else {
        let mut input = io::BufReader::new(file);
        loop {
            let frame =
                ruzstd::decoding::StreamingDecoder::new(&mut input).map_err(io::Error::other)?;
            let left = limit.saturating_add(1) - bytes.len() as u64;
            frame.take(left).read_to_end(&mut bytes)?;
            if bytes.len() as u64 > limit || input.fill_buf()?.is_empty() {
                break;
            }
        }
    }
    if bytes.len() as u64 > limit {
        return Err(io::Error::other(
            "the native transcript exceeds the read limit",
        ));
    }
    Ok(bytes)
}

fn decode(input: &File, output: &mut File) -> io::Result<()> {
    let mut input = io::BufReader::new(input);
    let mut output = io::BufWriter::new(output);
    loop {
        let mut frame =
            ruzstd::decoding::StreamingDecoder::new(&mut input).map_err(io::Error::other)?;
        io::copy(&mut frame, &mut output)?;
        if input.fill_buf()?.is_empty() {
            break;
        }
    }
    output.flush()
}

fn publish_noclobber(temporary: tempfile::NamedTempFile, destination: &Path) -> io::Result<()> {
    match std::fs::hard_link(temporary.path(), destination) {
        Ok(()) => return Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => return Ok(()),
        Err(_) => {}
    }
    match temporary.persist_noclobber(destination) {
        Ok(_) => Ok(()),
        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error.error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compress(plain: &Path, text: &[u8]) -> PathBuf {
        let compressed = compressed_path(plain);
        let frame =
            ruzstd::encoding::compress_to_vec(text, ruzstd::encoding::CompressionLevel::Fastest);
        std::fs::write(&compressed, frame).unwrap();
        compressed
    }

    /// A caller under a shared lock must not decode inline: the first call only starts the work,
    /// and an undecodable carrier must surface its error instead of staying busy forever.
    #[test]
    fn background_restoration_finishes_and_failures_surface_once() {
        let directory = tempfile::tempdir().unwrap();
        let plain = directory.path().join("rollout-2026-10-01T00-00-00-b.jsonl");
        compress(&plain, b"{\"n\":1}\n");
        assert!(begin_restore(&plain).unwrap());
        while begin_restore(&plain).unwrap() {
            std::thread::yield_now();
        }
        assert_eq!(std::fs::read(&plain).unwrap(), b"{\"n\":1}\n");

        let broken = directory.path().join("rollout-2026-10-01T00-00-00-c.jsonl");
        std::fs::write(compressed_path(&broken), b"not a zstd frame").unwrap();
        let failure = loop {
            match begin_restore(&broken) {
                Ok(true) => std::thread::yield_now(),
                Ok(false) => panic!("an undecodable carrier cannot become readable"),
                Err(error) => break error,
            }
        };
        assert_ne!(failure.kind(), io::ErrorKind::NotFound);
        assert!(
            begin_restore(&broken).unwrap(),
            "the next caller starts over"
        );
    }

    /// A restoration that left both carriers, or wrote over a plain file Codex had already
    /// restored and appended to, would leave readers on stale bytes.
    #[test]
    fn restores_compressed_rollout_once_and_never_replaces_a_plain_writer() {
        let directory = tempfile::tempdir().unwrap();
        let plain = directory.path().join("rollout-2026-10-01T00-00-00-a.jsonl");
        let compressed = compress(&plain, b"{\"type\":\"session_meta\"}\n{\"n\":1}\n");

        assert_eq!(restore(&compressed).unwrap(), plain);
        assert_eq!(
            std::fs::read(&plain).unwrap(),
            b"{\"type\":\"session_meta\"}\n{\"n\":1}\n"
        );
        assert!(!compressed.exists());
        assert_eq!(
            std::fs::read_dir(directory.path()).unwrap().count(),
            1,
            "no restoration temporary may remain"
        );

        std::fs::write(&plain, b"appended\n").unwrap();
        compress(&plain, b"stale\n");
        assert_eq!(restore(&plain).unwrap(), plain);
        assert_eq!(std::fs::read(&plain).unwrap(), b"appended\n");
    }
}
