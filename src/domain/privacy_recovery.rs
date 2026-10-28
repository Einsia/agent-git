//! Device-local recovered bytes are bound to an immutable encrypted publication.

use super::{
    link::PrivacyRecovery,
    meta,
    privacy_envelope::{MAX_ENVELOPE_BYTES, MAX_PRIVATE_BYTES, PrivacyEnvelope, digest_bytes},
    privacy_layer::{EVIDENCE_FIELD, EVIDENCE_PREFIX, EvidenceReference},
    repo::Repo,
    storage, transcript,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{collections::BTreeMap, fs, path::Path};
use zeroize::Zeroizing;

const MANIFEST: &str = "recovery.json";
/// Everything the cache holds is restored from one private layer, bounded by the envelope's
/// private payload: the snapshot's events, once each, are no larger than the layer, and their
/// sequence files no larger than the events. A smaller budget refuses to recover an envelope that
/// was published.
const MAX_CACHE_BYTES: usize = 2 * MAX_PRIVATE_BYTES;

/// The publication envelope `commit` carries, read from local objects only, or `None` when the
/// commit carries none. It is read up to [`MAX_ENVELOPE_BYTES`], the size an envelope may have:
/// this is the only way back to a published original, so a smaller read loses it.
pub(crate) fn committed_envelope(repo: &Repo, commit: &str) -> Result<Option<Vec<u8>>> {
    super::session_privacy::validate_oid(commit)?;
    let repo = repo.clone().local_objects_only();
    let object = format!("{commit}:privacy/envelope.json");
    let entry = repo.git_bytes_result(&[
        "ls-tree",
        "-z",
        "--full-name",
        commit,
        "--",
        "privacy/envelope.json",
    ])?;
    if entry.is_empty() {
        return Ok(None);
    }
    ensure!(
        entry.starts_with(b"100644 blob "),
        "privacy envelope must be a regular file"
    );
    let size: usize = repo.git(&["cat-file", "-s", &object])?.trim().parse()?;
    ensure!(
        size <= MAX_ENVELOPE_BYTES,
        "privacy envelope exceeds the size limit"
    );
    let output = repo.inspection_output(&["cat-file", "blob", &object], MAX_ENVELOPE_BYTES)?;
    ensure!(
        output.status.success() && output.stderr.is_empty(),
        "privacy envelope is unavailable"
    );
    Ok(Some(output.stdout))
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    publication_digest: String,
    files: BTreeMap<String, String>,
}

pub struct RecoveredSnapshot {
    pub metadata: meta::Meta,
    pub log: Zeroizing<String>,
    pub view: Zeroizing<String>,
    binding: PrivacyRecovery,
}

impl RecoveredSnapshot {
    /// Capture the complete cache before using any of it. A malformed cache cannot silently
    /// select the public transcript and discard private evidence on the next settlement.
    pub fn load(repo: &Repo, commit: &str) -> Result<Option<Self>> {
        ensure!(
            matches!(commit.len(), 40 | 64) && commit.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "recovery requires an immutable commit"
        );
        let Some(envelope) = committed_envelope(repo, commit)? else {
            return Ok(None);
        };
        let repo = repo.clone().local_objects_only();
        PrivacyEnvelope::parse(&envelope)?;
        let common = repo.common_dir()?.canonicalize()?;
        let parent = common.join("agit/privacy-recovery");
        let root = parent.join(commit);
        for directory in [common.join("agit"), parent, root.clone()] {
            let metadata = match fs::symlink_metadata(directory) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error.into()),
            };
            ensure!(
                metadata.file_type().is_dir(),
                "recovery directory is not a regular directory"
            );
        }
        let bytes = storage::read_bytes_capped(&root.join(MANIFEST), MAX_ENVELOPE_BYTES)?;
        let manifest: Manifest =
            serde_json::from_slice(&bytes).context("invalid private recovery manifest")?;
        ensure!(
            manifest.version == 1 && manifest.publication_digest == digest_bytes(&envelope),
            "recovery data belongs to another publication"
        );
        let files = read_files(&root)?;
        ensure!(
            file_digests(&files) == manifest.files,
            "private recovery data changed; unlock a fresh copy before continuing"
        );
        let metadata: meta::Meta = serde_json::from_slice(
            files
                .get(meta::FILE)
                .context("recovery metadata is missing")?,
        )?;
        meta::validate(&metadata)?;
        ensure!(
            metadata.layout == meta::LayoutVersion::CURRENT
                && metadata.runtime_instances.is_empty()
                && metadata.baseline_bytes.is_none()
                && metadata.cwd_state.is_none()
                && !metadata.cwd_is_agent_repository,
            "recovery metadata carries source-device authority"
        );
        let log = materialize(&files, meta::LOG_FILE)?;
        let view = materialize(&files, meta::VIEW_FILE)?;
        storage::snapshot_files(&log, &view)?;
        Ok(Some(Self {
            metadata,
            log,
            view,
            binding: PrivacyRecovery {
                commit: commit.to_owned(),
                manifest_digest: digest_bytes(&bytes),
            },
        }))
    }

    pub fn load_bound(repo: &Repo, binding: &PrivacyRecovery, tip: &str) -> Result<Self> {
        if binding.commit != tip {
            super::archive_history::verify_materialized_chain(repo, &binding.commit, tip)?;
        }
        let snapshot = Self::load(repo, &binding.commit)?.context(
            "private recovery data is missing; unlock the original snapshot before continuing",
        )?;
        ensure!(
            snapshot.binding == *binding,
            "private recovery data changed after materialization"
        );
        Ok(snapshot)
    }

    pub fn binding(&self) -> PrivacyRecovery {
        self.binding.clone()
    }

    pub fn verify(&self, repo: &Repo) -> Result<()> {
        Self::load_bound(repo, &self.binding, &self.binding.commit)?;
        Ok(())
    }

    /// Settlement of a legacy quoted baseline must retain the context that runtime continued.
    pub fn legacy_evidence_view(&self) -> Result<Zeroizing<String>> {
        use transcript::recovery::{CLOSING_TEXT, GENERATED_ORIGIN};
        let mut output = String::new();
        for record in storage::parse_envelopes(&self.view)? {
            let evidence = json!({"runtime":record.source,"session":record.session_id,"record":record.content});
            let text = format!("{EVIDENCE_PREFIX}{evidence}");
            let message = json!({"agit":GENERATED_ORIGIN,"type":"user","message":{"role":"user","content":text},
                EVIDENCE_FIELD: EvidenceReference::from_record(&record)});
            output.push_str(&transcript::wrap_lines(
                &format!("{message}\n"),
                "claude-code",
                &self.metadata.session,
            ));
        }
        if !output.is_empty() {
            let message = json!({"agit":GENERATED_ORIGIN,"type":"assistant","message":{"role":"assistant","content":CLOSING_TEXT}});
            output.push_str(&transcript::wrap_lines(
                &format!("{message}\n"),
                "claude-code",
                &self.metadata.session,
            ));
        }
        Ok(Zeroizing::new(output))
    }
}

pub fn write_manifest(root: &Path, publication: &[u8]) -> Result<()> {
    let files = read_files(root)?;
    let manifest = Manifest {
        version: 1,
        publication_digest: digest_bytes(publication),
        files: file_digests(&files),
    };
    fs::write(root.join(MANIFEST), serde_json::to_vec(&manifest)?)?;
    Ok(())
}

fn read_files(root: &Path) -> Result<BTreeMap<String, Zeroizing<Vec<u8>>>> {
    let mut files = BTreeMap::new();
    let mut remaining = MAX_CACHE_BYTES;
    for entry in walkdir::WalkDir::new(root).follow_links(false) {
        let entry = entry?;
        if entry.file_type().is_dir() {
            continue;
        }
        ensure!(
            entry.file_type().is_file(),
            "private recovery contains a non-regular file"
        );
        let path = entry
            .path()
            .strip_prefix(root)?
            .to_str()
            .context("invalid recovery filename")?
            .replace('\\', "/");
        if path == MANIFEST {
            continue;
        }
        ensure!(files.len() < 65_536, "private recovery has too many files");
        let bytes = Zeroizing::new(storage::read_bytes_capped(entry.path(), remaining)?);
        remaining -= bytes.len();
        files.insert(path, bytes);
    }
    Ok(files)
}

fn file_digests(files: &BTreeMap<String, Zeroizing<Vec<u8>>>) -> BTreeMap<String, String> {
    files
        .iter()
        .map(|(name, bytes)| (name.clone(), digest_bytes(bytes)))
        .collect()
}

fn materialize(
    files: &BTreeMap<String, Zeroizing<Vec<u8>>>,
    name: &str,
) -> Result<Zeroizing<String>> {
    let sequence = std::str::from_utf8(
        files
            .get(name)
            .context("private recovery sequence is missing")?,
    )?;
    let mut output = Zeroizing::new(String::new());
    for id in storage::parse_sequence(sequence)? {
        let path = meta::event_path(&id)?;
        let line = std::str::from_utf8(
            files
                .get(&path)
                .context("private recovery event is missing")?,
        )?;
        ensure!(
            storage::event_id(line)? == id,
            "private recovery event identity changed"
        );
        ensure!(
            output.len() + line.len() <= MAX_CACHE_BYTES,
            "private recovery sequence exceeds its budget"
        );
        output.push_str(line);
    }
    Ok(output)
}
