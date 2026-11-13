use super::*;

/// Deciding "binary or not" uses the content, not the extension — extensions lie (a PNG stuffed
/// into a `.txt`, a `.log` that is gzip), no text format may contain a NUL, and a name the type
/// table does not know (`.env`, a language it lacks) says nothing either way. Getting it wrong
/// pushes a screenful of garbage into the previewer, renders an image as text, or offers no
/// preview of plain text. A sniff that demands the whole prefix decode cleanly calls text binary
/// when the prefix ends inside a character.
#[test]
fn binary_detection_does_not_trust_the_extension() {
    let dir = tempfile::tempdir().unwrap();

    let txt = dir.path().join("notes.txt");
    std::fs::write(&txt, "hello\nworld\n").unwrap();
    let r = read_preview(&txt, 0).unwrap();
    assert!(!r.is_binary);
    assert_eq!(r.text.as_deref(), Some("hello\nworld\n"));
    assert!(r.base64.is_none());

    // The extension says text; the content is binary.
    let liar = dir.path().join("liar.txt");
    std::fs::write(&liar, [0x89, b'P', b'N', b'G', 0x00, 0x1a]).unwrap();
    let r = read_preview(&liar, 0).unwrap();
    assert!(r.is_binary, "content with a NUL must be treated as binary");
    assert!(r.base64.is_some() && r.text.is_none());

    // The table does not know the name; the content is text.
    let env = dir.path().join("robotwin2-261113b.env");
    std::fs::write(&env, "API_URL=http://localhost\n").unwrap();
    let r = read_preview(&env, 0).unwrap();
    assert!(!r.is_binary);
    assert_eq!(r.mime, "text/plain");
    assert_eq!(r.text.as_deref(), Some("API_URL=http://localhost\n"));

    // CJK fixture (multi-byte boundary): the sniffed prefix ends inside `预`.
    let cut = dir.path().join("cut.unknown");
    let mut body = "a".repeat(TEXT_SNIFF_BYTES - 1);
    body.push('预');
    std::fs::write(&cut, &body).unwrap();
    let r = read_preview(&cut, 0).unwrap();
    assert!(
        !r.is_binary,
        "a character cut by the sniffed prefix is still text"
    );

    // Neither the name nor the content says text; a file that opens with the tail of a
    // character is not text either, though a later page may start there.
    for (name, bytes) in [
        ("blob.dat", &[0xff, 0xfe, b'A', b'B'][..]),
        ("tail.dat", &[0x80, 0x80, b'A', b'B'][..]),
    ] {
        let blob = dir.path().join(name);
        std::fs::write(&blob, bytes).unwrap();
        let r = read_preview(&blob, 0).unwrap();
        assert!(r.is_binary, "{name}");
        assert_eq!(r.mime, "application/octet-stream", "{name}");
    }
}

/// A single `read()` handing back less than asked for is legal (interrupted by a signal, NFS,
/// special files). The preview must fill the window: taking one short read for end of file gives
/// the user an "opening" whose second half is missing with no sign of it, and when the cut lands
/// inside a multi-byte character lossy decoding grows a replacement character at the end.
#[test]
fn a_short_read_does_not_silently_truncate_the_preview() {
    /// A reader that gives up one small bite per `read` — the kernel's legal short read.
    struct Stutter<'a> {
        body: &'a [u8],
        at: usize,
        bite: usize,
    }
    impl std::io::Read for Stutter<'_> {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            let n = self.bite.min(self.body.len() - self.at).min(out.len());
            out[..n].copy_from_slice(&self.body[self.at..self.at + n]);
            self.at += n;
            Ok(n)
        }
    }

    // CJK fixture (multi-byte boundary): `"预览"` spans several bytes in UTF-8 and `bite` is
    // shorter than the whole body, so this reader is forced to short-read.
    let body = "预览".as_bytes();
    let mut reader = Stutter {
        body,
        at: 0,
        bite: 3,
    };
    let got = read_up_to(&mut reader, body.len()).unwrap();
    assert_eq!(
        got, body,
        "a short read must keep filling the window, not stand for end of file"
    );
    assert_eq!(String::from_utf8_lossy(&got), "预览");
}

/// A text chunk owns a character by which nominal window holds that character's leading byte.
/// The first chunk completes its trailing character, the next skips the continuation bytes it
/// starts with; the caller keeps stepping by a fixed cap plus offset, and the chunks concatenate
/// with no U+FFFD, nothing repeated and nothing dropped.
#[test]
fn utf8_split_at_the_preview_cap_roundtrips_across_fixed_offsets() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("split.txt");
    let mut body = "x".repeat(FILE_PREVIEW_CAP as usize - 1);
    // CJK fixture (multi-byte boundary): `界` straddles the cap, so the split lands mid-scalar.
    body.push_str("界tail");
    std::fs::write(&path, &body).unwrap();

    let first = read_preview(&path, 0).unwrap();
    let second = read_preview(&path, FILE_PREVIEW_CAP).unwrap();
    let first = first.text.expect("text preview");
    let second = second.text.expect("text preview");

    assert!(
        first.ends_with('界'),
        "the first chunk must finish its scalar"
    );
    assert_eq!(
        second, "tail",
        "the next chunk must skip prior continuations"
    );
    assert!(!first.contains('\u{fffd}') && !second.contains('\u{fffd}'));
    assert_eq!(format!("{first}{second}"), body);
}

#[test]
fn a_window_containing_only_the_previous_scalar_tail_is_empty() {
    // Offset points at the last byte of `界`; the one-byte nominal window
    // owns no leading byte, so that character belongs wholly to the prior
    // chunk rather than becoming a replacement character here.
    let bytes = &"界".as_bytes()[2..];
    assert_eq!(utf8_preview_window(bytes, 1), b"");
}

#[test]
fn a_huge_file_is_truncated_rather_than_refused() {
    let dir = tempfile::tempdir().unwrap();
    let big = dir.path().join("huge.log");
    let chunk = "x".repeat(1024);
    let mut body = String::new();
    for _ in 0..(FILE_PREVIEW_CAP / 1024 + 8) {
        body.push_str(&chunk);
    }
    std::fs::write(&big, &body).unwrap();

    let r = read_preview(&big, 0).unwrap();
    assert!(
        r.truncated,
        "going over the cap must be flagged; otherwise the reader thinks they saw everything"
    );
    assert_eq!(r.size, body.len() as u64);
    assert!(r.text.unwrap().len() as u64 <= FILE_PREVIEW_CAP);
}

/// A folder is said to be a folder, in a field a page can test, under the code pages that
/// predate the field already handle. A page that can only read the message treats a link to a
/// folder as a failed read.
#[test]
fn a_directory_is_rejected_with_a_next_step() {
    let dir = tempfile::tempdir().unwrap();
    let e = read_preview(dir.path(), 0).unwrap_err();
    assert!(e.is(ErrorCode::MalformedFrame));
    let data = e.data.unwrap();
    assert_eq!(data["kind"], "directory");
    assert!(data["hint"].as_str().unwrap().contains("readDirectory"));
}

#[test]
fn previewable_kinds_get_the_mime_the_browser_needs() {
    let p = std::path::Path::new("/x/a.png");
    assert_eq!(mime_of(p), "image/png");
    assert_eq!(
        mime_of(std::path::Path::new("/x/doc.pdf")),
        "application/pdf"
    );
    assert_eq!(mime_of(std::path::Path::new("/x/s.mp3")), "audio/mpeg");
    // SVG is text but its MIME does not start with text/ — without a case for it, it reads as
    // binary.
    assert!(is_texty(&mime_of(std::path::Path::new("/x/i.svg"))));
    assert_eq!(mime_of(std::path::Path::new("/x/Makefile")), "text/plain");
    assert_eq!(mime_of(std::path::Path::new("/x/prices.csv")), "text/csv");
}
