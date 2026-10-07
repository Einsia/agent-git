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
    let file = crate::rc::native_inbox::open_regular(&transcript)?;
    let size = file.metadata()?.len();
    ensure!(
        size <= crate::adapter::native_snapshot::MAX_CAPTURE_BYTES as u64,
        "native completion exceeds its capture budget"
    );
    let mut bytes = Vec::new();
    file.take(size).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 == size && bytes.last() == Some(&b'\n'),
        "native completion has an incomplete transcript record"
    );
    ensure!(
        registration.is_registered(),
        "native completion generation changed"
    );
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

pub(crate) fn turn_boundary(
    session: &str,
    transcript: &Path,
    turn: &str,
) -> crate::Result<Option<u64>> {
    ensure!(
        crate::rc::native_inbox::valid_id(turn),
        "native completion requires an exact turn UUID"
    );
    let transcript = transcript.canonicalize()?;
    let entries = match std::fs::read_dir(directory(session, &transcript)?) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let suffix = format!("-{turn}.json");
    let mut selected = None;
    for entry in entries {
        let entry = entry?;
        if !entry.file_name().to_string_lossy().ends_with(&suffix) {
            continue;
        }
        let receipt = read(&entry.path())?;
        ensure!(
            receipt.version == 1
                && receipt.session == session
                && receipt.transcript == transcript
                && receipt.turn == turn
                && receipt.bytes <= crate::adapter::native_snapshot::MAX_CAPTURE_BYTES as u64,
            "native completion receipt names another turn"
        );
        let file = crate::rc::native_inbox::open_regular(&transcript)?;
        let mut bytes = Vec::new();
        file.take(receipt.bytes).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 == receipt.bytes
                && hex::encode(Sha256::digest(&bytes)) == receipt.sha256,
            "native completion prefix changed"
        );
        ensure!(
            selected.is_none_or(|end| end == receipt.bytes),
            "native completion boundary is ambiguous"
        );
        selected = Some(receipt.bytes);
    }
    Ok(selected)
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
