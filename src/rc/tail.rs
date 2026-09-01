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
    /// Byte offset we've consumed up to.
    offset: u64,
    /// Physical line number of the next line (0-based).
    lineno: u64,
    /// A trailing partial line (the harness writes a line in more than one
    /// syscall); held back until the newline arrives.
    pending: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TailedLine {
    pub lineno: u64,
    pub text: String,
}

impl Tailer {
    /// Start at the end of the file (only new content) or the beginning.
    pub fn new(path: impl Into<PathBuf>, from_start: bool) -> Tailer {
        let path = path.into();
        let (offset, lineno) = if from_start {
            (0, 0)
        } else {
            count_to_end(&path).unwrap_or_default()
        };
        Tailer {
            path,
            offset,
            lineno,
            pending: String::new(),
        }
    }

    /// Start at a known byte offset that is also a known line boundary.
    ///
    /// For replaying only the tail of a long transcript: `Tailer::new(.., true)`
    /// would read every line of the file into one `Vec` on the first poll just
    /// to throw the front away, which costs memory proportional to the whole
    /// file. Seeking straight to the window keeps that bounded.
    ///
    /// `offset` must be the first byte of line `lineno`, or the line numbers
    /// this tailer reports will not match the file.
    pub fn at(path: impl Into<PathBuf>, offset: u64, lineno: u64) -> Tailer {
        Tailer {
            path: path.into(),
            offset,
            lineno,
            pending: String::new(),
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

    /// Read whatever has been appended since the last call.
    ///
    /// Truncation (file shrank — replaced or rotated) resets to the start, since
    /// the old coordinates no longer mean anything.
    pub fn poll(&mut self) -> std::io::Result<Vec<TailedLine>> {
        let meta = match std::fs::metadata(&self.path) {
            Ok(m) => m,
            Err(_) => return Ok(vec![]), // not created yet; try again next tick
        };
        let len = meta.len();
        if len < self.offset {
            self.offset = 0;
            self.lineno = 0;
            self.pending.clear();
        }
        if len == self.offset {
            return Ok(vec![]);
        }
        let mut f = std::fs::File::open(&self.path)?;
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

/// (byte length, line count) of an existing file.
fn count_to_end(path: &Path) -> Option<(u64, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    let f = std::fs::File::open(path).ok()?;
    let lines = BufReader::new(f).lines().count() as u64;
    Some((meta.len(), lines))
}

#[cfg(test)]
mod tests {

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
