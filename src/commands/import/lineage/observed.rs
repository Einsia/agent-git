use crate::adapter::{Adapter, codex, native_snapshot};
use crate::domain::{native_archive, privacy::projector, transcript};
use native_snapshot::{BudgetHit, Limit, Limits, Source, Unavailable};
use sha2::{Digest, Sha256};

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
        let snapshot = match adapter.snapshot_native_readonly(source, Limits::default()) {
            Err(Unavailable::Limit(BudgetHit {
                limit: Limit::Bytes | Limit::WorkingBytes | Limit::Records,
                ..
            })) if source.runtime == "codex" => return Self::stream(source, PREFIX_BYTES),
            result => result?,
        };
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
        let mut digest = Sha256::new();
        let mut prefix = Vec::new();
        let mut suffix_hashes = Vec::new();
        let mut candidate_hash_bytes =
            crate::domain::import_lineage::Limits::default().snapshot_bytes;
        codex::lineage_stream::visit(source, |line| {
            digest.update(line);
            if suffix_hashes.is_empty() && prefix.len() + line.len() <= prefix_limit {
                prefix.extend_from_slice(line);
            } else {
                let checked = native_archive::capture(line, 0, EMPTY_SHA256)
                    .map_err(|_| Unavailable::Incomplete)?;
                let value: serde_json::Value =
                    serde_json::from_str(checked.records).map_err(|_| Unavailable::Incomplete)?;
                if value.get("type").and_then(|kind| kind.as_str()) == Some("session_meta") {
                    return Err(Unavailable::Incomplete.into());
                }
                if projector::tokens(&value.to_string()).next().is_some() {
                    return Err(StreamGap::SecretMapping.into());
                }
                if candidate_hash_bytes != 0 {
                    let hash = transcript::object_hash(&value);
                    // Every verified stored envelope contains its content hash literally.
                    // No admissible candidate can reach records past this comparison window.
                    if hash.len() <= candidate_hash_bytes {
                        candidate_hash_bytes -= hash.len();
                        suffix_hashes.push(hash);
                    } else {
                        candidate_hash_bytes = 0;
                    }
                }
            }
            Ok(())
        })?;
        native_archive::capture(&prefix, 0, EMPTY_SHA256).map_err(|_| Unavailable::Incomplete)?;
        Ok(Self {
            prefix,
            suffix_hashes,
            digest: Some(digest.finalize().into()),
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub(super) enum StreamGap {
    #[error("secret_mapping")]
    SecretMapping,
}

impl StreamGap {
    pub(super) fn reason(&self) -> &'static str {
        match self {
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
