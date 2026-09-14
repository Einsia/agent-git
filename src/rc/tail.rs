//! Transcript tailer.
//!
//! `agitd` does not invent its own record of what happened: it tails the file
//! the harness is already writing, because that file is exactly what
//! `agit commit` will snapshot. If the live view came from stdout and the
//! committed history came from the file, the two could disagree — and "we
//! talked about it on the web but `agit log` doesn't have it" is the one crack
//! this product cannot have.
//!
//! Polling, not inotify. The harness appends every few hundred ms at most, a
//! 100 ms poll is imperceptible, and polling has no platform matrix, no
//! descriptor limits, and behaves identically when the file is replaced (which
//! `agit resume`'s slow path does — it materializes a *new* file and the
//! session's transcript path changes underneath us).
//!
//! Line numbers are physical and 0-based, matching `adapter::Event::line`:
//! blank and unparseable lines still consume a number, so the coordinate can be
//! used to seek back into the file.

use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

pub struct Tailer {
    path: PathBuf,
    /// Coordinates and buffered fragments belong to the open source, not its pathname.
    source: Option<same_file::Handle>,
    #[cfg(unix)]
    source_identity: Option<(u64, u64)>,
    /// Byte offset we've consumed up to.
    offset: u64,
    /// Physical line number of the next line (0-based).
    lineno: u64,
    /// A trailing partial line (the harness writes a line in more than one
    /// syscall); held back until the newline arrives.
    pending: String,
    /// A watched source restarts from a bounded replay window when its coordinates expire.
    replay_window: Option<u64>,
    codex_header: Option<super::codex_history::Header>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TailedLine {
    pub lineno: u64,
    pub text: String,
}

#[derive(Default)]
pub(crate) struct CodexBatch {
    pub mode: super::codex_history::HistoryMode,
    pub lines: Vec<TailedLine>,
}

impl Tailer {
    /// Start at the end of the file (only new content) or the beginning.
    pub fn new(path: impl Into<PathBuf>, from_start: bool) -> Tailer {
        let path = path.into();
        let mut source = std::fs::File::open(&path)
            .and_then(same_file::Handle::from_file)
            .ok();
        let (offset, lineno) = if from_start {
            (0, 0)
        } else {
            source
                .as_mut()
                .and_then(|handle| count_to_end(handle.as_file_mut()).ok())
                .unwrap_or_default()
        };
        Tailer {
            path,
            #[cfg(unix)]
            source_identity: source.as_ref().and_then(|handle| {
                handle
                    .as_file()
                    .metadata()
                    .ok()
                    .map(|metadata| file_identity(&metadata))
            }),
            source,
            offset,
            lineno,
            pending: String::new(),
            replay_window: None,
            codex_header: None,
        }
    }

    /// Start at a known byte offset that is also a known line boundary.
    ///
    /// For replaying only the tail of a long transcript: `Tailer::new(.., true)`
    /// would read every line of the file into one `Vec` on the first poll just
    /// to throw the front away, which costs memory proportional to the whole
    /// file. Seeking straight to the window keeps that bounded.
    ///
    /// The open source must be the file used to calculate the coordinates; reopening
    /// its pathname can bind the window to a replacement file.
    pub fn at(
        path: impl Into<PathBuf>,
        offset: u64,
        lineno: u64,
        source: Option<same_file::Handle>,
        replay_lines: u64,
    ) -> Tailer {
        let (offset, lineno) = if source.is_some() {
            (offset, lineno)
        } else {
            (0, 0)
        };
        Tailer {
            path: path.into(),
            #[cfg(unix)]
            source_identity: source.as_ref().and_then(|handle| {
                handle
                    .as_file()
                    .metadata()
                    .ok()
                    .map(|metadata| file_identity(&metadata))
            }),
            source,
            offset,
            lineno,
            pending: String::new(),
            replay_window: Some(replay_lines),
            codex_header: None,
        }
    }

    /// How far this file has been read, in bytes.
    ///
    /// **This is the only reliable coordinate for "is there anything new".** A line count is
    /// not: while the transcript writes its last record in pieces (the harness spends more than
    /// one syscall on a line), `poll()` leaves those bytes in `pending` and returns an empty
    /// list — by line count that reads as "nothing moved", while the file is growing. The
    /// settlement's quiet test reading the wrong coordinate declares the turn over early and
    /// cuts the unfinished record outside the commit.
    ///
    /// `pending.len()` is not added: those bytes are **already counted in `offset`** (`poll`
    /// does `offset += n` first, then pushes the incomplete tail into `pending`), so adding
    /// them again double-counts.
    pub fn consumed(&self) -> u64 {
        self.offset
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Follow the file to a different path (slow-path resume mints a new file).
    pub fn retarget(&mut self, path: impl Into<PathBuf>, from_start: bool) {
        *self = Tailer::new(path, from_start);
    }

    /// History mode and records are read through the same retained file handle.
    pub(crate) fn poll_codex(&mut self) -> std::io::Result<CodexBatch> {
        let lines = self.poll()?;
        if self.codex_header.is_none()
            || matches!(
                self.codex_header,
                Some(super::codex_history::Header::Pending)
            )
        {
            self.codex_header = self
                .source
                .as_mut()
                .map(|source| super::codex_history::read_header(source.as_file_mut()));
        }
        Ok(CodexBatch {
            mode: self
                .codex_header
                .map(super::codex_history::Header::mode)
                .unwrap_or_default(),
            lines,
        })
    }

    /// Read whatever has been appended since the last call.
    ///
    /// Replacement or truncation resets the coordinates and pending fragment together.
    /// A replacement can be longer than the source whose coordinates were consumed.
    pub fn poll(&mut self) -> std::io::Result<Vec<TailedLine>> {
        // The held source keeps its identity valid while an unchanged path skips reopening.
        // Changed paths are opened and identified before their bytes are consumed.
        #[cfg(unix)]
        if let Some(identity) = self.source_identity
            && std::fs::metadata(&self.path).is_ok_and(|metadata| {
                metadata.len() == self.offset && file_identity(&metadata) == identity
            })
        {
            return Ok(vec![]);
        }
        let file = match std::fs::File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(error) => return Err(error),
        };
        let mut source = same_file::Handle::from_file(file)?;
        let metadata = source.as_file().metadata()?;
        let len = metadata.len();
        #[cfg(unix)]
        {
            self.source_identity = Some(file_identity(&metadata));
        }
        let replaced = self
            .source
            .as_ref()
            .is_some_and(|previous| previous != &source);
        if replaced || len < self.offset || self.source.is_none() {
            let (offset, line) = self.replay_window.map_or((0, 0), |want| {
                let (offset, line, _, _) = window_start(source.as_file_mut(), want);
                (offset, line)
            });
            self.offset = offset;
            self.lineno = line;
            self.pending.clear();
            self.codex_header = None;
        }
        let source = self.source.insert(source);
        if len == self.offset {
            return Ok(vec![]);
        }
        let f = source.as_file_mut();
        f.seek(SeekFrom::Start(self.offset))?;
        let mut reader = BufReader::new(f);
        let mut out = vec![];
        loop {
            let mut buf = String::new();
            let n = reader.read_line(&mut buf)?;
            if n == 0 {
                break;
            }
            self.offset += n as u64;
            if buf.ends_with('\n') {
                let mut line = std::mem::take(&mut self.pending);
                line.push_str(buf.trim_end_matches(['\n', '\r']));
                out.push(TailedLine {
                    lineno: self.lineno,
                    text: line,
                });
                self.lineno += 1;
            } else {
                // Partial trailing line — the harness wrote a line in more than
                // one syscall. Hold the bytes and keep the offset advanced: the
                // next poll appends the rest and emits the line whole. (Rewinding
                // the offset *and* buffering would count these bytes twice.)
                self.pending.push_str(&buf);
                break;
            }
        }
        Ok(out)
    }
}

/// Select a bounded replay window on the same open file the tailer will consume.
pub(super) fn window_start(f: &mut std::fs::File, want: u64) -> (u64, u64, u64, bool) {
    let want = want.max(1) as usize;

    // A bounded scan prevents large transcripts from monopolizing the daemon.
    const SCAN_CAP: u64 = 32 * 1024 * 1024;
    let mut base_off: u64 = 0;
    if let Ok(meta) = f.metadata()
        && meta.len() > SCAN_CAP
    {
        base_off = meta.len() - SCAN_CAP;
        if f.seek(SeekFrom::Start(base_off)).is_ok() {
            // Align to the next newline; never start in the middle of a line.
            let mut skip = Vec::with_capacity(8192);
            let mut r = BufReader::new(&mut *f);
            if let Ok(n) = r.read_until(b'\n', &mut skip) {
                base_off += n as u64;
            }
            let _ = f.seek(SeekFrom::Start(base_off));
        } else {
            base_off = 0;
        }
    }
    let mut starts: std::collections::VecDeque<(u64, u64)> = std::collections::VecDeque::new();
    let mut reader = BufReader::new(f);
    let mut offset: u64 = base_off;
    let mut lineno: u64 = 0;
    let mut buf = Vec::with_capacity(8192);
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if starts.len() == want {
                    starts.pop_front();
                }
                starts.push_back((offset, lineno));
                offset += n as u64;
                lineno += 1;
            }
        }
    }
    // An empty capped window must stay at the aligned boundary, never rewind to the head.
    let (start_off, start_line) = starts.front().copied().unwrap_or((base_off, 0));
    (start_off, start_line, lineno, base_off == 0)
}

#[cfg(unix)]
fn file_identity(metadata: &std::fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (metadata.dev(), metadata.ino())
}

/// (byte length, line count) of an existing file.
fn count_to_end(file: &mut std::fs::File) -> std::io::Result<(u64, u64)> {
    let mut reader = BufReader::new(file);
    let mut bytes = 0;
    let mut lines = 0;
    let mut unterminated = false;
    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            return Ok((bytes, lines + u64::from(unterminated)));
        }
        bytes += buffer.len() as u64;
        lines += buffer.iter().filter(|&&byte| byte == b'\n').count() as u64;
        unterminated = buffer.last() != Some(&b'\n');
        let consumed = buffer.len();
        reader.consume(consumed);
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn replacement_restarts_coordinates_even_when_the_source_does_not_shrink() {
        for contents in ["new\n", "new\nmore\n"] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("source.jsonl");
            let replacement = dir.path().join("replacement.jsonl");
            std::fs::write(&path, "old\n").unwrap();
            let mut tailer = Tailer::new(&path, true);
            assert_eq!(tailer.poll().unwrap()[0].text, "old");
            std::fs::write(&replacement, contents).unwrap();
            std::fs::rename(&replacement, &path).unwrap();

            let expected: Vec<_> = contents
                .lines()
                .enumerate()
                .map(|(line, text)| TailedLine {
                    lineno: line as u64,
                    text: text.into(),
                })
                .collect();
            assert_eq!(tailer.poll().unwrap(), expected);
            assert!(tailer.poll().unwrap().is_empty());
        }
    }

    #[test]
    fn replacement_discards_the_previous_sources_partial_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.jsonl");
        let replacement = dir.path().join("replacement.jsonl");
        std::fs::write(&path, "old-part").unwrap();
        let mut tailer = Tailer::new(&path, true);
        assert!(tailer.poll().unwrap().is_empty());
        std::fs::write(&replacement, "replacement-complete\n").unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        assert_eq!(
            tailer.poll().unwrap(),
            vec![TailedLine {
                lineno: 0,
                text: "replacement-complete".into(),
            }]
        );
    }

    #[test]
    fn replacement_after_seeking_to_end_does_not_reuse_the_old_offset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.jsonl");
        let replacement = dir.path().join("replacement.jsonl");
        std::fs::write(&path, "old\n").unwrap();
        let mut tailer = Tailer::new(&path, false);
        std::fs::write(&replacement, "replacement\n").unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        assert_eq!(
            tailer.poll().unwrap(),
            vec![TailedLine {
                lineno: 0,
                text: "replacement".into(),
            }]
        );
    }

    /// A record being written in pieces: the bytes grow, but not one whole line can be handed
    /// back yet.
    ///
    /// The position **must** advance with them — the settlement's quiet test is what tells
    /// "written" from "being written" apart. A caller that returns early on the empty list
    /// never reads the position at all, and a long record written slowly is judged quiet and
    /// cut outside the commit.
    #[test]
    fn a_half_written_record_still_moves_the_position() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("t.jsonl");
        std::fs::write(&path, b"").expect("create");
        let mut t = Tailer::new(path.clone(), true);

        assert!(t.poll().expect("poll").is_empty());
        assert_eq!(t.consumed(), 0);

        // The harness wrote half a record, with no newline.
        std::fs::write(&path, b"{\"partial\":").expect("write");
        let lines = t.poll().expect("poll");
        assert!(lines.is_empty(), "no whole line yet");
        assert_eq!(
            t.consumed(),
            11,
            "the byte position must advance or the turn is judged quiet"
        );

        // Another chunk, still no newline.
        std::fs::write(&path, b"{\"partial\":\"more").expect("write");
        assert!(t.poll().expect("poll").is_empty());
        assert_eq!(t.consumed(), 16);

        // The closing newline arrives; the whole line comes out at once.
        std::fs::write(&path, b"{\"partial\":\"more\"}\n").expect("write");
        let lines = t.poll().expect("poll");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "{\"partial\":\"more\"}");
        // The position does not double-count: the earlier bytes are already in it.
        assert_eq!(t.consumed(), 19);
    }
    use super::*;
    use std::io::Write;

    #[test]
    fn reads_only_appended_lines_and_numbers_them_physically() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("t.jsonl");
        std::fs::write(&p, "a\nb\n").unwrap();

        let mut t = Tailer::new(&p, false);
        assert!(
            t.poll().unwrap().is_empty(),
            "starting at the end sees nothing"
        );

        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        writeln!(f, "c").unwrap();
        writeln!(f).unwrap(); // blank line still consumes a number
        writeln!(f, "e").unwrap();
        let got = t.poll().unwrap();
        assert_eq!(
            got,
            vec![
                TailedLine {
                    lineno: 2,
                    text: "c".into()
                },
                TailedLine {
                    lineno: 3,
                    text: String::new()
                },
                TailedLine {
                    lineno: 4,
                    text: "e".into()
                },
            ]
        );
    }

    #[test]
    fn a_half_written_line_is_held_back_until_its_newline_arrives() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("t.jsonl");
        std::fs::write(&p, "").unwrap();
        let mut t = Tailer::new(&p, true);

        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        write!(f, "{{\"partial\":").unwrap();
        f.flush().unwrap();
        assert!(
            t.poll().unwrap().is_empty(),
            "no newline yet — must not emit a broken line"
        );

        writeln!(f, "true}}").unwrap();
        f.flush().unwrap();
        assert_eq!(
            t.poll().unwrap(),
            vec![TailedLine {
                lineno: 0,
                text: "{\"partial\":true}".into()
            }]
        );
    }

    #[test]
    fn truncation_restarts_from_the_beginning() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("t.jsonl");
        std::fs::write(&p, "one\ntwo\n").unwrap();
        let mut t = Tailer::new(&p, false);
        std::fs::write(&p, "x\n").unwrap();
        assert_eq!(
            t.poll().unwrap(),
            vec![TailedLine {
                lineno: 0,
                text: "x".into()
            }]
        );
    }
}
