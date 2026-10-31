//! The directory a native transcript records, read without loading the transcript.
//!
//! Adapters and the terminal interface share it, so it sits outside the `cli` feature: the Hub
//! links the adapters without a terminal.

/// Open only a regular native source; a special file must not block an advisory probe.
pub(crate) fn opening_file(path: &std::path::Path) -> Option<std::fs::File> {
    if !std::fs::symlink_metadata(path).ok()?.is_file() {
        return None;
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() {
        return None;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
        {
            return None;
        }
    }
    Some(file)
}

/// A record names its directory at top level (Claude Code) or in its payload (Codex). A relative
/// path names none: resolving it against the reader's own directory would attribute the session
/// to wherever agit happens to run.
pub(crate) fn record_cwd(record: &serde_json::Value) -> Option<String> {
    record
        .get("cwd")
        .or_else(|| record.get("payload")?.get("cwd"))
        .and_then(serde_json::Value::as_str)
        .filter(|cwd| std::path::Path::new(cwd).is_absolute())
        .map(str::to_owned)
}

/// A record is buffered whole only up to this size; a longer one is read past unparsed, so
/// memory stays bounded whatever an inline attachment weighs.
pub(crate) const CWD_RECORD_BYTES: usize = 1024 * 1024;
/// A transcript that never names an absolute directory is read no further than this.
const CWD_SCAN_BYTES: u64 = 16 * 1024 * 1024;

/// The directory named by the first complete record that carries an absolute `cwd`.
///
/// Discovery that selects sessions by directory reads this rather than an opening preview: a transcript
/// can open with records that name no directory followed by a prompt longer than the preview
/// window, and the preview then finds no directory in a session that has one. Records stream one
/// at a time and reading stops at the first directory, so the cost follows where that record
/// sits rather than how long the transcript has grown. A record too long to buffer is skipped,
/// since the records that follow it name the directory too. A record cut by the end of the file
/// or of the scan budget provides nothing.
pub(crate) fn recorded_cwd(path: &std::path::Path) -> Option<String> {
    use std::io::Read;
    let file = opening_file(path)?;
    let mut reader = std::io::BufReader::new(file.take(CWD_SCAN_BYTES));
    let mut record = Vec::new();
    while let Some(buffered) = next_record(&mut reader, &mut record).ok()? {
        if buffered
            && let Some(cwd) = serde_json::from_slice::<serde_json::Value>(&record)
                .ok()
                .as_ref()
                .and_then(record_cwd)
        {
            return Some(cwd);
        }
    }
    None
}

/// Consume the next newline-terminated record. `Some(true)` means `record` holds all of it,
/// `Some(false)` that it outgrew [`CWD_RECORD_BYTES`] and `record` holds nothing, and `None`
/// that the input ended; trailing bytes without a newline are a cut record and end the input.
fn next_record(
    reader: &mut impl std::io::BufRead,
    record: &mut Vec<u8>,
) -> std::io::Result<Option<bool>> {
    record.clear();
    let mut buffered = true;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(None);
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let chunk = &available[..newline.unwrap_or(available.len())];
        if buffered && record.len() + chunk.len() <= CWD_RECORD_BYTES {
            record.extend_from_slice(chunk);
        } else {
            buffered = false;
            record.clear();
        }
        let consumed = chunk.len() + usize::from(newline.is_some());
        reader.consume(consumed);
        if newline.is_some() {
            return Ok(Some(buffered));
        }
    }
}
