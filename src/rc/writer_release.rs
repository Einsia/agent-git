//! A supervised harness that owned its process records the transcript it leaves behind.
//!
//! The mark is written only after that process tree is reaped, so while the transcript still
//! matches it, the latest write came from a writer that no longer exists. A recency guard that
//! waits for an unknown writer to go quiet has nothing to wait for then. Any later write, by
//! anyone, changes the transcript and voids the mark.

use anyhow::Context;
use serde::{Deserialize, Serialize};
use sha2::Digest as _;
use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Mark {
    len: u64,
    modified_secs: u64,
    modified_nanos: u32,
}

impl Mark {
    fn of(metadata: &std::fs::Metadata) -> Option<Self> {
        let modified = metadata
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?;
        Some(Self {
            len: metadata.len(),
            modified_secs: modified.as_secs(),
            modified_nanos: modified.subsec_nanos(),
        })
    }
}

fn path(runtime: &str, thread_id: &str) -> crate::Result<PathBuf> {
    let mut digest = sha2::Sha256::new();
    digest.update(runtime.as_bytes());
    digest.update([0]);
    digest.update(thread_id.as_bytes());
    Ok(super::rc_dir()?
        .join("released")
        .join(format!("{}.json", hex::encode(digest.finalize()))))
}

/// Record `transcript` as its supervised writer left it. The caller has reaped that writer.
pub(crate) fn record(runtime: &str, thread_id: &str, transcript: &Path) -> crate::Result<()> {
    let mark = Mark::of(&std::fs::metadata(transcript)?)
        .context("transcript modification time is unavailable")?;
    let path = path(runtime, thread_id)?;
    let directory = path.parent().context("release mark has no directory")?;
    crate::infra::config::create_state_dir(directory)?;
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    serde_json::to_writer(&mut file, &mark)?;
    file.persist(&path)?;
    Ok(())
}

/// Whether the transcript described by `metadata` is exactly as a reaped supervised writer left it.
pub(crate) fn unchanged(runtime: &str, thread_id: &str, metadata: &std::fs::Metadata) -> bool {
    let Some(current) = Mark::of(metadata) else {
        return false;
    };
    path(runtime, thread_id)
        .ok()
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice::<Mark>(&bytes).ok())
        .is_some_and(|mark| mark == current)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    /// The mark lifts the recency guard only for the exact transcript its writer left. A mark
    /// keyed by conversation alone would let a terminal that resumed it afterwards be taken
    /// over while it writes.
    #[test]
    fn a_release_mark_matches_only_the_transcript_its_writer_left() {
        let root = tempfile::tempdir().unwrap();
        crate::rc::with_agit_home(root.path(), || {
            let transcript = root.path().join("native.jsonl");
            std::fs::write(&transcript, "{}\n").unwrap();
            let unchanged = || {
                super::unchanged(
                    "claude-code",
                    "native",
                    &std::fs::metadata(&transcript).unwrap(),
                )
            };
            assert!(!unchanged(), "no supervised writer was recorded");
            super::record("claude-code", "native", &transcript).unwrap();
            assert!(unchanged());
            assert!(
                !super::unchanged("codex", "native", &std::fs::metadata(&transcript).unwrap()),
                "another runtime's conversation is a different transcript"
            );
            std::fs::OpenOptions::new()
                .append(true)
                .open(&transcript)
                .unwrap()
                .write_all(b"{}\n")
                .unwrap();
            assert!(!unchanged(), "a later write voids the mark");
        });
    }
}
