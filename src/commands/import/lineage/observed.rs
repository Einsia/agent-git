use crate::adapter::{Adapter, codex, native_snapshot};
use crate::domain::{native_archive, privacy::projector, transcript};
use native_snapshot::{Limits, Source, Unavailable};
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Read};

const RECORD_BYTES: usize = 8 * 1024 * 1024;
const PREFIX_BYTES: usize = 16 * 1024 * 1024;
const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

#[derive(PartialEq, Eq)]
pub(super) struct Observed {
    pub(super) prefix: Vec<u8>,
    pub(super) suffix_hashes: Vec<String>,
    digest: Option<[u8; 32]>,
}

impl Observed {
    pub(super) fn read(source: &Source, adapter: &dyn Adapter) -> crate::Result<Self> {
        if source.runtime == "codex"
            && std::fs::symlink_metadata(&source.path)?.len()
                > native_snapshot::MAX_NATIVE_BYTES as u64
        {
            return Self::stream(source, PREFIX_BYTES);
        }
        let snapshot = adapter.snapshot_native_readonly(source, Limits::default())?;
        anyhow::ensure!(
            snapshot.source == *source,
            "the native provider returned another source"
        );
        Ok(Self {
            prefix: snapshot.bytes,
            suffix_hashes: Vec::new(),
            digest: None,
        })
    }

    fn stream(source: &Source, prefix_limit: usize) -> crate::Result<Self> {
        let cap = native_snapshot::MAX_CAPTURE_BYTES;
        let (file, opened) = native_snapshot::open_unchanged(&source.path, |len| {
            if len > cap {
                Err(Unavailable::BudgetExceeded)
            } else {
                Ok(())
            }
        })
        .map_err(|error| {
            if error == Unavailable::BudgetExceeded {
                anyhow::Error::new(StreamGap::Bytes)
            } else {
                anyhow::Error::new(error)
            }
        })?;
        let mut reader = BufReader::new(&file);
        let mut digest = Sha256::new();
        let mut prefix = Vec::new();
        let mut suffix_hashes = Vec::new();
        let mut line = Vec::new();
        let mut total = 0;
        let mut records = 0;
        loop {
            line.clear();
            let count = (&mut reader)
                .take(RECORD_BYTES as u64 + 1)
                .read_until(b'\n', &mut line)
                .map_err(|_| Unavailable::Read)?;
            if count == 0 {
                break;
            }
            total += count;
            records += 1;
            if count > RECORD_BYTES {
                return Err(StreamGap::RecordBytes.into());
            }
            if total > cap {
                return Err(StreamGap::Bytes.into());
            }
            if records > native_snapshot::MAX_NATIVE_RECORDS {
                return Err(StreamGap::Records.into());
            }
            if line.last() != Some(&b'\n') {
                return Err(Unavailable::Incomplete.into());
            }
            if records == 1 && !codex::standalone_header(source, &line) {
                return Err(Unavailable::Incomplete.into());
            }
            digest.update(&line);
            if suffix_hashes.is_empty() && prefix.len() + count <= prefix_limit {
                prefix.extend_from_slice(&line);
            } else {
                let checked = native_archive::capture(&line, 0, EMPTY_SHA256)
                    .map_err(|_| Unavailable::Incomplete)?;
                let value: serde_json::Value =
                    serde_json::from_str(checked.records).map_err(|_| Unavailable::Incomplete)?;
                if value.get("type").and_then(|kind| kind.as_str()) == Some("session_meta") {
                    return Err(Unavailable::Incomplete.into());
                }
                if projector::tokens(&value.to_string()).next().is_some() {
                    return Err(StreamGap::SecretMapping.into());
                }
                suffix_hashes.push(transcript::object_hash(&value));
            }
        }
        if records == 0 {
            return Err(Unavailable::Incomplete.into());
        }
        native_archive::capture(&prefix, 0, EMPTY_SHA256).map_err(|_| Unavailable::Incomplete)?;
        native_snapshot::finish_unchanged(&file, &source.path, &opened, total)?;
        Ok(Self {
            prefix,
            suffix_hashes,
            digest: Some(digest.finalize().into()),
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub(super) enum StreamGap {
    #[error("native_stream_bytes")]
    Bytes,
    #[error("native_stream_record_bytes")]
    RecordBytes,
    #[error("native_stream_records")]
    Records,
    #[error("secret_mapping")]
    SecretMapping,
}

impl StreamGap {
    pub(super) fn reason(&self) -> &'static str {
        match self {
            Self::Bytes => "native_stream_bytes",
            Self::RecordBytes => "native_stream_record_bytes",
            Self::Records => "native_stream_records",
            Self::SecretMapping => "secret_mapping",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_streamed_suffix_matches_record_identity_but_freezes_every_source_byte() {
        let directory = tempfile::tempdir().unwrap();
        let id = "aaaaaaaa-0000-4000-8000-000000000001";
        let source = Source {
            runtime: "codex",
            session_id: id.into(),
            path: directory.path().join(format!("rollout-{id}.jsonl")),
            database: false,
        };
        let header = format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\"}}}}\n");
        let write =
            |suffix: &str| std::fs::write(&source.path, format!("{header}{suffix}")).unwrap();
        write("{\"value\":1}\n");
        let first = Observed::stream(&source, header.len()).unwrap();
        assert_eq!(first.prefix, header.as_bytes());
        assert_eq!(
            first.suffix_hashes,
            transcript::live_hashes("{\"value\":1}\n")
        );
        write("{\"value\": 1}\n");
        let changed = Observed::stream(&source, header.len()).unwrap();
        assert_eq!(first.suffix_hashes, changed.suffix_hashes);
        assert_ne!(first.digest, changed.digest);
        write("{\"value\":1}\n{\"tail\":2}");
        assert!(Observed::stream(&source, header.len()).is_err());
    }
}
