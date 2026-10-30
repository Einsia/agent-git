//! Native completion receipts describe immutable prefixes without copying conversation content.

use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Receipt {
    version: u32,
    session: String,
    generation: String,
    turn: String,
    reason: String,
    transcript: PathBuf,
    pub(crate) bytes: u64,
    sha256: String,
}

fn directory(session: &str, transcript: &Path) -> crate::Result<PathBuf> {
    let key = serde_json::to_vec(&(session, transcript))?;
    Ok(super::directory()?
        .join("completions")
        .join(hex::encode(Sha256::digest(key))))
}

#[cfg(test)]
pub(crate) fn receipt_directory(session: &str, transcript: &Path) -> PathBuf {
    directory(session, &transcript.canonicalize().unwrap()).unwrap()
}

fn read(path: &Path) -> crate::Result<Receipt> {
    let file = crate::rc::native_inbox::open_regular(path)?;
    ensure!(
        file.metadata()?.len() <= super::RECORD_LIMIT,
        "native completion receipt is too large"
    );
    Ok(serde_json::from_reader(file.take(super::RECORD_LIMIT))?)
}

/// The hook waits for this child before allowing another native turn to start.
pub(crate) fn record(
    session: &str,
    generation: &str,
    turn: &str,
    reason: &str,
) -> crate::Result<Receipt> {
    ensure!(
        crate::rc::native_inbox::valid_id(turn),
        "native completion requires an exact turn UUID"
    );
    ensure!(reason == "aborted", "invalid native completion reason");
    let registration = super::register(session, generation)?;
    let transcript = registration.transcript_path()?.canonicalize()?;
    write(session, generation, turn, transcript, None, || {
        ensure!(
            registration.is_registered(),
            "native completion generation changed"
        );
        Ok(())
    })
}

/// Where a turn that just ended stops in its transcript, as the completed-only cut will see it.
pub(crate) struct TurnEnd {
    pub(crate) at: u64,
    /// Conversation follows the last boundary, so no cut reaches `at` yet.
    pub(crate) open: bool,
}

/// A turn that ended on its own stops at its own completion; the bookkeeping Claude appends after
/// it belongs to the next snapshot, and a job that counted it would wait for a cut no commit makes.
/// A turn still awaiting its reply is open through the whole prefix, so an earlier completion
/// cannot acknowledge its prompt. A `stopped` turn, one its harness stopped before completing,
/// gets an end receipt there: Claude marks such a stop only with a tool denial or synthetic text,
/// which also occur in turns that go on, so no transcript rule can close it, and a conversation
/// that is never continued would never settle it.
pub(crate) fn turn_end(
    session: &str,
    transcript: &Path,
    consumed: u64,
    stopped: Option<(&str, &str)>,
) -> crate::Result<TurnEnd> {
    let transcript = transcript.canonicalize()?;
    let mut prefix = read_prefix(&transcript, consumed)?;
    // A record still being written belongs to a later read.
    prefix.truncate(
        prefix
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |end| end + 1),
    );
    let closed = boundaries_in(
        &directory(session, &transcript)?,
        session,
        &transcript,
        &prefix,
    )?
    .last()
    .copied()
    .unwrap_or(0);
    let text = std::str::from_utf8(&prefix).context("native transcript is not UTF-8")?;
    if !crate::adapter::claude_code::awaits_reply(&text[closed..]) {
        let own = crate::adapter::claude_code::completed_turn_boundary(text).unwrap_or(0);
        return Ok(TurnEnd {
            at: closed.max(own) as u64,
            open: false,
        });
    }
    let Some((generation, turn)) = stopped else {
        return Ok(TurnEnd {
            at: prefix.len() as u64,
            open: true,
        });
    };
    ensure!(
        crate::rc::native_inbox::valid_id(turn),
        "native completion requires an exact turn UUID"
    );
    let receipt = write(
        session,
        generation,
        turn,
        transcript,
        Some(prefix.len() as u64),
        || Ok(()),
    )?;
    Ok(TurnEnd {
        at: receipt.bytes,
        open: false,
    })
}

fn read_prefix(transcript: &Path, end: u64) -> crate::Result<Vec<u8>> {
    ensure!(
        end <= crate::adapter::native_snapshot::MAX_CAPTURE_BYTES as u64,
        "native completion exceeds its capture budget"
    );
    let mut bytes = Vec::new();
    crate::rc::native_inbox::open_regular(transcript)?
        .take(end)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 == end,
        "native transcript is shorter than what was read"
    );
    Ok(bytes)
}

/// Receipts are written once per turn: a replay returns the saved boundary, never a later one.
fn write(
    session: &str,
    generation: &str,
    turn: &str,
    transcript: PathBuf,
    end: Option<u64>,
    current: impl FnOnce() -> crate::Result<()>,
) -> crate::Result<Receipt> {
    let reason = "aborted";
    let directory = directory(session, &transcript)?;
    crate::infra::config::create_state_dir(&directory)?;
    let path = directory.join(format!("{generation}-{turn}.json"));
    if path.exists() {
        let saved = read(&path)?;
        ensure!(
            saved.session == session
                && saved.generation == generation
                && saved.turn == turn
                && saved.reason == reason
                && saved.transcript == transcript,
            "native completion replay changed its identity"
        );
        return Ok(saved);
    }
    let size = match end {
        Some(end) => end,
        None => std::fs::metadata(&transcript)?.len(),
    };
    let bytes = read_prefix(&transcript, size)?;
    ensure!(
        bytes.last() == Some(&b'\n'),
        "native completion has an incomplete transcript record"
    );
    current()?;
    let receipt = Receipt {
        version: 1,
        session: session.into(),
        generation: generation.into(),
        turn: turn.into(),
        reason: reason.into(),
        transcript,
        bytes: size,
        sha256: hex::encode(Sha256::digest(&bytes)),
    };
    let mut temporary = tempfile::NamedTempFile::new_in(&directory)?;
    serde_json::to_writer(&mut temporary, &receipt)?;
    temporary.flush()?;
    temporary.as_file().sync_all()?;
    // A repeated callback must never move the original turn boundary into a later turn.
    match temporary.persist_noclobber(&path) {
        Ok(_) => {}
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            return read(&path);
        }
        Err(error) => return Err(error.error.into()),
    }
    crate::rc::native_inbox::sync_directory(&directory)?;
    Ok(receipt)
}

pub(crate) fn boundaries(
    session: &str,
    transcript: &Path,
    bytes: &[u8],
) -> crate::Result<Vec<usize>> {
    let transcript = transcript.canonicalize()?;
    let directory = directory(session, &transcript)?;
    boundaries_in(&directory, session, &transcript, bytes)
}

/// The receipts recorded for `transcript`, by name: a receipt can end a turn without the
/// transcript changing, so a reader that skips an unchanged transcript compares these too.
pub(crate) fn receipt_names(session: &str, transcript: &Path) -> Vec<std::ffi::OsString> {
    let Some(directory) = transcript
        .canonicalize()
        .ok()
        .and_then(|transcript| directory(session, &transcript).ok())
    else {
        return vec![];
    };
    let mut names: Vec<_> = std::fs::read_dir(directory)
        .into_iter()
        .flatten()
        .filter_map(|entry| Some(entry.ok()?.file_name()))
        .filter(|name| {
            Path::new(name)
                .extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect();
    names.sort();
    names
}

fn boundaries_in(
    directory: &Path,
    session: &str,
    transcript: &Path,
    bytes: &[u8],
) -> crate::Result<Vec<usize>> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(error) => return Err(error.into()),
    };
    let mut receipts = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        let receipt = read(&path)?;
        ensure!(
            receipt.version == 1
                && receipt.session == session
                && receipt.transcript == transcript
                && receipt.reason == "aborted",
            "native completion receipt names another transcript"
        );
        if receipt.bytes <= bytes.len() as u64 {
            receipts.push(receipt);
        }
    }
    receipts.sort_by_key(|receipt| receipt.bytes);
    let mut digest = Sha256::new();
    let mut previous = 0;
    let mut boundaries = Vec::new();
    for receipt in receipts {
        let end =
            usize::try_from(receipt.bytes).context("native completion offset is out of range")?;
        ensure!(
            end > 0 && bytes.get(end - 1) == Some(&b'\n'),
            "native completion does not end at a record boundary"
        );
        digest.update(&bytes[previous..end]);
        previous = end;
        ensure!(
            hex::encode(digest.clone().finalize()) == receipt.sha256,
            "native completion prefix changed"
        );
        if boundaries.last() != Some(&end) {
            boundaries.push(end);
        }
    }
    Ok(boundaries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_authorizes_only_its_unchanged_native_prefix() {
        let directory = tempfile::tempdir().unwrap();
        let transcript = directory.path().join("native.jsonl");
        let stopped = b"{\"type\":\"user\"}\n";
        let mut grown = stopped.to_vec();
        grown.extend_from_slice(b"{\"type\":\"assistant\"}\n");
        let receipt = Receipt {
            version: 1,
            session: "session".into(),
            generation: "generation".into(),
            turn: "turn".into(),
            reason: "aborted".into(),
            transcript: transcript.clone(),
            bytes: stopped.len() as u64,
            sha256: hex::encode(Sha256::digest(stopped)),
        };
        std::fs::write(
            directory.path().join("receipt.json"),
            serde_json::to_vec(&receipt).unwrap(),
        )
        .unwrap();
        assert_eq!(
            boundaries_in(directory.path(), "session", &transcript, &grown).unwrap(),
            vec![stopped.len()]
        );
        grown[2] = b'x';
        assert!(boundaries_in(directory.path(), "session", &transcript, &grown).is_err());
        assert!(boundaries_in(directory.path(), "another-session", &transcript, stopped).is_err());
    }
}
