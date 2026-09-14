use super::*;
use std::io::Write as _;

#[test]
fn replacement_between_window_selection_and_watch_start_invalidates_the_window() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("source.jsonl");
    let replacement = dir.path().join("replacement.jsonl");
    std::fs::write(&path, "old\ntail\n").unwrap();
    let (offset, line, _, _, source) = tail_window(&path, 1);
    assert!(offset > 0);
    std::fs::write(&replacement, "replacement\n").unwrap();
    std::fs::rename(&replacement, &path).unwrap();
    let mut tailer = crate::rc::tail::Tailer::at(&path, offset, line, source, 1);
    assert_eq!(
        tailer.poll().unwrap(),
        vec![crate::rc::tail::TailedLine {
            lineno: 0,
            text: "replacement".into(),
        }]
    );
}

/// An **absurdly large** transcript does not let the scan grow without bound.
///
/// Exact line numbers count from the head of the file, and this function runs synchronously
/// inside `dispatch` holding the daemon's global lock: the cost grows linearly with the size of
/// the transcript and has no bound of its own — one pathologically large transcript stalls every
/// RPC on this machine, the event pump with them. Past the cap only the last stretch is scanned.
#[test]
fn a_transcript_far_past_the_cap_is_only_scanned_from_the_cap_onwards() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("huge.jsonl");
    {
        let mut f = std::fs::File::create(&p).unwrap();
        // Fixed line width, so line count times width just clears the `SCAN_CAP` in `tail_window`.
        let line = format!("{}\n", "x".repeat(1023));
        for _ in 0..(33 * 1024) {
            f.write_all(line.as_bytes()).unwrap();
        }
    }
    let (start_off, from_line, total, absolute, source) = tail_window(&p, 400);
    // Only lines after the cut are counted, so the total is far below the file's 33792 lines.
    assert!(
        total < 33 * 1024,
        "past the cap the count must not reach the whole file: {total}"
    );
    assert!(
        !absolute,
        "relative line numbers past the cap must be stated in the protocol, not switched silently"
    );
    assert!(
        from_line + 400 <= total + 1,
        "the window is still about the last 400 lines"
    );
    // The offset lands past the cut, aligned to the start of a line.
    assert!(start_off >= 32 * 1024 * 1024, "{start_off}");
    let mut t = crate::rc::tail::Tailer::at(&p, start_off, from_line, source, 400);
    let lines = t.poll().expect("tail");
    assert_eq!(lines.len(), 400);
    assert!(lines.iter().all(|l| l.text.len() == 1023));
}

/// Only the tail is replayed, but the numbers reported must be **absolute** — `item.completed.line`
/// is the physical line number in the file, sharing coordinates with `agit show`. Counting it
/// wrong points the web interface at a different line.
#[test]
fn the_window_starts_at_the_last_n_lines_and_counts_them_absolutely() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("t.jsonl");
    let mut f = std::fs::File::create(&p).unwrap();
    for i in 0..1000 {
        writeln!(f, "{{\"n\":{i}}}").unwrap();
    }
    drop(f);

    let (offset, from_line, total, absolute, source) = tail_window(&p, 400);
    assert!(
        absolute,
        "1000 lines sit well under the cap, so line numbers are physical"
    );
    assert_eq!(total, 1000);
    assert_eq!(from_line, 600, "the last 400 lines start at line 600");

    // Reading from this offset, the first line is line 600 and the line numbers line up.
    let mut t = crate::rc::tail::Tailer::at(&p, offset, from_line, source, 400);
    let got = t.poll().unwrap();
    assert_eq!(got.len(), 400);
    assert_eq!(got[0].lineno, 600);
    assert_eq!(got[0].text, "{\"n\":600}");
    assert_eq!(got[399].lineno, 999);
}

/// A file shorter than the window is served from the start, never a negative or empty window.
#[test]
fn a_short_transcript_is_replayed_whole() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("t.jsonl");
    std::fs::write(&p, "a\nb\nc\n").unwrap();
    let (offset, from_line, total, absolute, _source) = tail_window(&p, 400);
    assert_eq!((offset, from_line, total), (0, 0, 3));
    assert!(absolute, "with no cap the line numbers are physical");
}

/// A file that does not exist yields a neutral zero rather than a panic — the transcript may
/// have been cleared this very moment.
#[test]
fn a_missing_transcript_is_not_a_panic() {
    assert_eq!(
        tail_window(std::path::Path::new("/nope/none"), 400),
        (0, 0, 0, true, None)
    );
}

#[test]
fn replaced_watch_reselects_a_bounded_window_before_emitting_history() {
    for poll_before_replace in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.jsonl");
        let replacement = dir.path().join("replacement.jsonl");
        let original = "old\n".repeat(1000);
        std::fs::write(&path, &original).unwrap();
        let (offset, line, _, _, source) = tail_window(&path, 400);
        let mut tailer = crate::rc::tail::Tailer::at(&path, offset, line, source, 400);
        if poll_before_replace {
            assert_eq!(tailer.poll().unwrap().len(), 400);
        }
        let replaced = "new\n".repeat(1000);
        assert_eq!(original.len(), replaced.len());
        std::fs::write(&replacement, replaced).unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        let lines = tailer.poll().unwrap();
        assert_eq!(lines.len(), 400);
        assert_eq!(lines[0].lineno, 600);
        assert_eq!(lines.last().unwrap().lineno, 999);
        assert!(lines.iter().all(|line| line.text == "new"));
        assert!(tailer.poll().unwrap().is_empty());
        writeln!(
            std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap(),
            "next"
        )
        .unwrap();
        let appended = tailer.poll().unwrap();
        assert_eq!(appended.len(), 1);
        assert_eq!(appended[0].lineno, 1000);
        assert_eq!(appended[0].text, "next");
    }
}

#[test]
fn truncated_watch_reselects_a_bounded_window_without_buffered_fragments() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("source.jsonl");
    std::fs::write(&path, format!("{}partial", "old\n".repeat(2000))).unwrap();
    let (offset, line, _, _, source) = tail_window(&path, 400);
    let mut tailer = crate::rc::tail::Tailer::at(&path, offset, line, source, 400);
    tailer.poll().unwrap();
    std::fs::write(&path, "new\n".repeat(1000)).unwrap();
    let lines = tailer.poll().unwrap();
    assert_eq!(lines.len(), 400);
    assert_eq!(lines[0].lineno, 600);
    assert!(lines.iter().all(|line| line.text == "new"));
}

#[test]
fn a_watch_without_an_initial_source_bounds_the_first_available_history() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("source.jsonl");
    let (offset, line, _, _, source) = tail_window(&path, 400);
    let mut tailer = crate::rc::tail::Tailer::at(&path, offset, line, source, 400);
    std::fs::write(&path, "new\n".repeat(1000)).unwrap();
    let lines = tailer.poll().unwrap();
    assert_eq!(lines.len(), 400);
    assert_eq!(lines[0].lineno, 600);
}

#[test]
fn replacement_past_the_scan_cap_keeps_replay_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("source.jsonl");
    let replacement = dir.path().join("replacement.jsonl");
    std::fs::write(&path, "old\n").unwrap();
    let (offset, line, _, _, source) = tail_window(&path, 400);
    let mut tailer = crate::rc::tail::Tailer::at(&path, offset, line, source, 400);
    assert_eq!(tailer.poll().unwrap().len(), 1);
    {
        let mut file = std::io::BufWriter::new(std::fs::File::create(&replacement).unwrap());
        let record = format!("{}\n", "x".repeat(1023));
        for _ in 0..33 * 1024 {
            file.write_all(record.as_bytes()).unwrap();
        }
    }
    std::fs::rename(&replacement, &path).unwrap();
    let lines = tailer.poll().unwrap();
    assert_eq!(lines.len(), 400);
    assert!(lines.iter().all(|line| line.text.len() == 1023));
    assert!(tailer.poll().unwrap().is_empty());
}
