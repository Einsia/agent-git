//! Repository-local reversible secret placeholders.
//!
//! The global vault says which literal values are secrets. This dictionary
//! gives every matching value a repository-scoped random handle and keeps the
//! reverse mapping beside the repository's Git metadata, never in its tree.

use super::{
    CURRENT_PROJECTION_VERSION, CURRENT_SCHEMA_VERSION, DecryptedRecord, KeyStore,
    MAX_REPOSITORY_SECRET_BYTES, Matcher, PlainRecord, RECORD_VERSION, RecordOrigin,
    RepositoryKeyStore, SealedRecord, Unlocked, VaultStore, encode_padded, record_aad, seal,
    write_vault,
};
use aho_corasick::{AhoCorasick, AhoCorasickBuilder, AhoCorasickKind, MatchKind};
use anyhow::{Context as _, bail};
use serde::Serialize;
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use zeroize::Zeroizing;

const DICTIONARY_RELATIVE_PATH: &str = "agit/secret-dictionary/vault.json";
use crate::domain::secrets::placeholder::{CANONICAL_TOKEN_LEN, TOKEN_PREFIX, TOKEN_SUFFIX};
const MAX_OVERLAPPING_MATCHES: usize = 64 * 1024;
const OVERLAPPING_MATCH_BATCH: usize = 4 * 1024;
#[derive(Clone, Copy)]
pub(crate) struct ReadonlyDictionaryLimits {
    pub vault_bytes: usize,
    pub records: usize,
    pub pattern_bytes: usize,
}

impl ReadonlyDictionaryLimits {
    #[cfg(any(feature = "cli", test))]
    pub const STATUS: Self = Self {
        vault_bytes: 256 * 1024,
        records: 128,
        pattern_bytes: 16 * 1024,
    };
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtectionReport {
    pub text: String,
    pub replacements: usize,
    pub new_records: usize,
    pub new_heuristic_records: usize,
    /// Heuristic findings too large for a reversible record. They are left
    /// byte-for-byte so the repo-wide push gate still rejects them; everything
    /// else in the same input is projected normally.
    pub intact: usize,
}

#[derive(Debug, thiserror::Error)]
#[error("read-only hydration exceeds its inspection budget")]
pub(crate) struct HydrationBudgetExceeded;

struct HydrationBudget {
    remaining: usize,
}

impl HydrationBudget {
    fn new(remaining: usize) -> Self {
        Self { remaining }
    }

    fn reserve_escaped(&mut self, bytes: usize) -> crate::Result<()> {
        // Every input byte covers its longest JSON escape; inserted secrets reserve separately.
        let bytes = bytes.checked_mul(6).ok_or(HydrationBudgetExceeded)?;
        self.remaining = self
            .remaining
            .checked_sub(bytes)
            .ok_or(HydrationBudgetExceeded)?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HydrationReport {
    pub text: String,
    pub replacements: usize,
    pub unresolved: usize,
}

/// Safe management-plane view. It intentionally has no plaintext, hash,
/// length, preview or decrypt/export companion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepositoryRecordSummary {
    pub id: String,
    pub name: String,
    pub origins: Vec<String>,
    pub heuristic_disposition: super::HeuristicDisposition,
    pub explicit_block: bool,
    pub effective_protect: bool,
    pub created_at: String,
    pub updated_at: String,
}

/// One encrypted dictionary per checkout. The path is beneath `.git`, so no
/// normal add/push/export path can accidentally publish it.
pub struct RepositoryDictionary<K: KeyStore = RepositoryKeyStore> {
    store: VaultStore<K>,
    repo_root: Option<PathBuf>,
}

impl RepositoryDictionary<RepositoryKeyStore> {
    /// A validated Git carrier selects its dictionary without rediscovering a worktree.
    #[cfg(feature = "cli")]
    pub(crate) fn open_at_git_dir(git_dir: &Path) -> crate::Result<Self> {
        Ok(Self::new(
            git_dir.join(Path::new(DICTIONARY_RELATIVE_PATH)),
            RepositoryKeyStore::new(git_dir.join("agit/secret-dictionary/keys")),
        ))
    }

    /// Repository keys use local files independently of the global registration keystore.
    pub fn open(repo_root: &Path) -> crate::Result<Self> {
        // One dictionary per repository: session-branch worktrees and the main checkout share it.
        let git_dir = crate::domain::repo::common_git_dir(repo_root);
        let mut dictionary = Self::new(
            git_dir.join(Path::new(DICTIONARY_RELATIVE_PATH)),
            RepositoryKeyStore::new(git_dir.join("agit/secret-dictionary/keys")),
        );
        dictionary.repo_root = Some(repo_root.to_owned());
        Ok(dictionary)
    }
}

impl<K: KeyStore> RepositoryDictionary<K> {
    pub fn new(path: PathBuf, keys: K) -> Self {
        Self {
            store: VaultStore::new(path, keys),
            repo_root: None,
        }
    }

    pub fn exists(&self) -> bool {
        self.store.path.exists()
    }

    /// Protect a JSONL transcript on decoded JSON strings, not on its escaped
    /// wire representation. Malformed/truncated lines fall back to literal text
    /// so the transformation remains fail-closed for readable bytes.
    pub fn protect_jsonl(&self, text: &str, global: &Matcher) -> crate::Result<ProtectionReport> {
        self.store.with_lock(|| {
            let (unlocked, records) = ProtectionState::read_records(&self.store)?;
            let allowlist = local_allowlist(&records, global)?;
            // A finding larger than a reversible record cannot become a
            // placeholder, but that is a fact about *that* finding — it says
            // nothing about the registered secret three lines above it. Only
            // its own span stays in the clear; returning the whole accumulated
            // transcript instead would un-project everything a previous
            // settlement already protected and write those values into the next
            // Git object in the clear.
            //
            // Where those spans are is recomputed per string during protection,
            // from a bounded length test, so the candidate list never has to
            // carry the over-capacity literals.
            let candidates = {
                let existing: HashSet<&str> = records
                    .iter()
                    .map(|record| record.secret.as_str())
                    .collect();
                crate::domain::secrets::secret_candidates_jsonl(text, |candidate| {
                    candidate.len() <= MAX_REPOSITORY_SECRET_BYTES
                        && !existing.contains(candidate)
                        && !allowlist.contains(candidate)
                })
            };
            if candidates.over_capacity {
                bail!(
                    "more than {} MiB of new heuristic secret values were found in one settlement; no repository dictionary update was written",
                    crate::domain::secrets::MAX_NEW_CANDIDATE_BYTES / (1024 * 1024)
                );
            }
            let mut state = ProtectionState::from_records(
                &self.store,
                global,
                &candidates.values,
                ExistingRecordScope::All,
                unlocked,
                records,
                allowlist,
            )?;
            state.oversized_threshold = Some(MAX_REPOSITORY_SECRET_BYTES);
            let (text, replacements) = transform_jsonl(text, |s| state.protect_string(s))?;
            let new_records = state.new_records;
            let new_heuristic_records = state.new_heuristic_records;
            let intact = state.intact_hits;
            state.persist()?;
            Ok(ProtectionReport {
                text,
                replacements,
                new_records,
                new_heuristic_records,
                intact,
            })
        })
    }

    /// The caller binds the native source to its validated session claim before using this boundary.
    /// A schema-owned session field is retained only when it equals that claim; credentials elsewhere
    /// keep their protection semantics even when they contain the same bytes.
    pub fn protect_native_jsonl(
        &self,
        text: &str,
        global: &Matcher,
        runtime: &str,
        native: &str,
    ) -> crate::Result<ProtectionReport> {
        self.protect_session_jsonl(
            text,
            global,
            runtime,
            native,
            self.repo_root.as_deref().unwrap_or(Path::new(".")),
        )
    }

    pub(crate) fn protect_session_jsonl(
        &self,
        text: &str,
        global: &Matcher,
        runtime: &str,
        native: &str,
        cwd: &Path,
    ) -> crate::Result<ProtectionReport> {
        use crate::domain::secrets::identity::{Evidence, RecordMask, native_session_pointers};
        let repo = self.repo_root.as_ref().map(crate::domain::repo::Repo::at);
        let mut evidence = repo.as_ref().map(|repo| Evidence::new(repo, cwd));
        if let Some(evidence) = &mut evidence {
            evidence.seed_native(runtime, native)?;
        }
        self.protect_with_masks(text, global, |value| {
            if let Some(evidence) = &mut evidence {
                evidence.record(runtime, native, value)
            } else {
                RecordMask(
                    native_session_pointers(runtime, native, value)
                        .into_iter()
                        .filter_map(|pointer| {
                            Some((
                                pointer.to_owned(),
                                0..value.pointer(pointer)?.as_str()?.len(),
                            ))
                        })
                        .collect(),
                )
            }
        })
    }

    pub(crate) fn protect_with_masks(
        &self,
        text: &str,
        global: &Matcher,
        mut mask_for: impl FnMut(&Value) -> crate::domain::secrets::identity::RecordMask,
    ) -> crate::Result<ProtectionReport> {
        let registered = global.merged(&self.registered_matcher()?)?;
        let mut hidden = std::collections::HashMap::<String, String>::new();
        let mut aliases = std::collections::HashMap::<String, String>::new();
        let mut input = String::new();
        for (chunk, value) in crate::domain::secrets::jsonl_chunks(text) {
            if let Some(mut value) = value {
                let mut mask = mask_for(&value);
                mask.0.retain(|(pointer, span)| {
                    value
                        .pointer(pointer)
                        .and_then(Value::as_str)
                        .is_some_and(|text| {
                            !registered
                                .find(text)
                                .iter()
                                .any(|hit| hit.start < span.end && span.start < hit.end)
                        })
                });
                mask.apply(&mut value, |identity| {
                    if let Some(token) = aliases.get(identity) {
                        return token.clone();
                    }
                    let token = format!(
                        "{{{{AGIT_SECRET_V1:{}:sec_{}}}}}",
                        uuid::Uuid::new_v4(),
                        uuid::Uuid::new_v4().simple()
                    );
                    hidden.insert(token.clone(), identity.to_owned());
                    aliases.insert(identity.to_owned(), token.clone());
                    token
                });
                input.push_str(&serde_json::to_string(&value)?);
                if chunk.ends_with('\n') {
                    input.push('\n');
                }
            } else {
                input.push_str(chunk);
            }
        }
        let mut report = self.protect_jsonl(&input, global)?;
        if !hidden.is_empty() {
            report.text = transform_jsonl(&report.text, |text| {
                let mut restored = String::with_capacity(text.len());
                let mut cursor = 0;
                for (start, end, token) in token_segments(text) {
                    if let Some(identity) = hidden.get(token) {
                        restored.push_str(&text[cursor..start]);
                        restored.push_str(identity);
                        cursor = end;
                    }
                }
                restored.push_str(&text[cursor..]);
                Ok((restored, 0))
            })?
            .0;
        }
        Ok(report)
    }

    /// Protect user-controlled observations without rewriting metadata schema or identity fields.
    pub fn protect_metadata(
        &self,
        metadata: &mut crate::domain::meta::Meta,
        global: &Matcher,
    ) -> crate::Result<ProtectionReport> {
        let fields = observation_fields(metadata);
        let report = self.protect_jsonl(&serde_json::to_string(&fields)?, global)?;
        let protected: Vec<String> = serde_json::from_str(&report.text)?;
        for (field, value) in fields.into_iter().zip(protected) {
            *field = value;
        }
        Ok(report)
    }

    /// Local comparison may recover observations without changing Git or the dictionary.
    /// Unknown placeholders remain opaque; callers must not treat them as known identities.
    pub fn hydrate_metadata_readonly(
        &self,
        metadata: &mut crate::domain::meta::Meta,
    ) -> crate::Result<usize> {
        let fields = observation_fields(metadata);
        let input = serde_json::to_string(&fields)?;
        if !input.contains(TOKEN_PREFIX) {
            return Ok(0);
        }
        let (report, _) = self.hydrate_pair_readonly(&input, "")?;
        let hydrated: Vec<String> = serde_json::from_str(&report.text)?;
        for (field, value) in fields.into_iter().zip(hydrated) {
            *field = value;
        }
        Ok(report.unresolved)
    }

    /// Protect a text carrier without interpreting its contents as a JSON record.
    pub fn protect_text(&self, text: &str, global: &Matcher) -> crate::Result<ProtectionReport> {
        self.store.with_lock(|| {
            let (unlocked, records) = ProtectionState::read_records(&self.store)?;
            let allowlist = local_allowlist(&records, global)?;
            let existing: HashSet<&str> = records
                .iter()
                .map(|record| record.secret.as_str())
                .collect();
            let literal = serde_json::to_string(text)?;
            let candidates =
                crate::domain::secrets::secret_candidates_jsonl(&literal, |candidate| {
                    candidate.len() <= MAX_REPOSITORY_SECRET_BYTES
                        && !existing.contains(candidate)
                        && !allowlist.contains(candidate)
                });
            if candidates.over_capacity {
                bail!(
                    "more than {} MiB of new heuristic secret values were found in one text carrier; no repository dictionary update was written",
                    crate::domain::secrets::MAX_NEW_CANDIDATE_BYTES / (1024 * 1024)
                );
            }
            let mut state = ProtectionState::from_records(
                &self.store,
                global,
                &candidates.values,
                ExistingRecordScope::All,
                unlocked,
                records,
                allowlist,
            )?;
            state.oversized_threshold = Some(MAX_REPOSITORY_SECRET_BYTES);
            let (text, replacements) = state.protect_string(text)?;
            let new_records = state.new_records;
            let new_heuristic_records = state.new_heuristic_records;
            let intact = state.intact_hits;
            state.persist()?;
            Ok(ProtectionReport {
                text,
                replacements,
                new_records,
                new_heuristic_records,
                intact,
            })
        })
    }

    /// Expand only repository tokens in literal text. JSON escapes inside the carrier stay data.
    pub fn hydrate_text(&self, text: &str) -> crate::Result<HydrationReport> {
        let encoded = serde_json::to_string(text)?;
        let (mut hydrated, _) = self.hydrate_pair_readonly(&encoded, "")?;
        hydrated.text = serde_json::from_str(&hydrated.text)?;
        Ok(hydrated)
    }

    /// For continuity checks after a secret was already assigned a repository
    /// key. This never learns a new global value and therefore cannot silently
    /// rewrite an already-settled prefix merely because the global rule list
    /// changed later.
    pub fn protect_existing_jsonl(&self, text: &str) -> crate::Result<ProtectionReport> {
        self.store.with_lock(|| {
            let mut state = ProtectionState::load(&self.store, &Matcher::empty(), &[])?;
            let (text, replacements) = transform_jsonl(text, |s| state.protect_string(s))?;
            state.persist()?;
            Ok(ProtectionReport {
                text,
                replacements,
                new_records: state.new_records,
                new_heuristic_records: state.new_heuristic_records,
                intact: state.intact_hits,
            })
        })
    }

    /// Comparison inputs share an immutable dictionary snapshot without learning records or
    /// creating locks, vaults, or keys. An unreadable existing dictionary is not an empty one.
    pub fn hydrate_pair_readonly(
        &self,
        committed: &str,
        live: &str,
    ) -> crate::Result<(HydrationReport, HydrationReport)> {
        self.with_readonly_hydrator(&[committed, live], None, None, |hydrate| {
            Ok((hydrate(committed)?, hydrate(live)?))
        })
    }

    /// Each input retains its own record boundaries and unresolved-token result.
    /// The expansion budget is shared before any replacement string is allocated.
    /// Dictionary admission requires a separate, explicit caller policy.
    pub(crate) fn hydrate_batch_readonly_bounded(
        &self,
        inputs: &[&str],
        max_output_bytes: usize,
    ) -> crate::Result<Vec<crate::Result<HydrationReport>>> {
        self.hydrate_batch_readonly_with_limits(inputs, max_output_bytes, None)
    }

    /// Dictionary admission is an explicit caller policy, separate from output expansion.
    pub(crate) fn hydrate_batch_readonly_with_limits(
        &self,
        inputs: &[&str],
        max_output_bytes: usize,
        dictionary_limits: Option<ReadonlyDictionaryLimits>,
    ) -> crate::Result<Vec<crate::Result<HydrationReport>>> {
        self.with_readonly_hydrator(
            inputs,
            Some(max_output_bytes),
            dictionary_limits,
            |hydrate| Ok(inputs.iter().copied().map(hydrate).collect()),
        )
    }

    fn with_readonly_hydrator<T>(
        &self,
        inputs: &[&str],
        max_output_bytes: Option<usize>,
        dictionary_limits: Option<ReadonlyDictionaryLimits>,
        consume: impl FnOnce(&mut dyn FnMut(&str) -> crate::Result<HydrationReport>) -> crate::Result<T>,
    ) -> crate::Result<T> {
        let mut budget = max_output_bytes.map(HydrationBudget::new);
        if let Some(budget) = &mut budget {
            for input in inputs {
                budget.reserve_escaped(input.len())?;
            }
        }
        let (unlocked, records) = match std::fs::symlink_metadata(&self.store.path) {
            Ok(_) => {
                let unlocked = if let Some(limits) = dictionary_limits {
                    self.unlock_readonly_bounded(limits)?
                } else {
                    self.store
                        .unlock_file(super::read_vault(&self.store.path)?, true)?
                };
                let records = super::decrypt_records(&unlocked.file, &unlocked.dek)?;
                (Some(unlocked), records)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (None, vec![]),
            Err(error) => return Err(error.into()),
        };
        let patterns: Vec<_> = unlocked
            .as_ref()
            .into_iter()
            .flat_map(|vault| {
                records
                    .iter()
                    .map(|record| token(&vault.file.vault_id, &record.id))
            })
            .collect();
        let known_tokens: HashSet<_> = patterns.iter().map(String::as_str).collect();
        let secrets: Vec<_> = records
            .iter()
            .map(|record| record.secret.as_str())
            .collect();
        let matcher = if patterns.is_empty() {
            None
        } else {
            let mut builder = AhoCorasickBuilder::new();
            builder.match_kind(MatchKind::LeftmostLongest);
            if dictionary_limits.is_some() {
                builder
                    .kind(Some(AhoCorasickKind::NoncontiguousNFA))
                    .dense_depth(0);
            }
            Some(
                builder
                    .build(patterns.iter().map(String::as_bytes))
                    .context("cannot build the repository secret hydrator")?,
            )
        };
        let mut hydrate = |text: &str| -> crate::Result<HydrationReport> {
            let mut unresolved = 0;
            let (text, replacements) = transform_jsonl(text, |value| {
                unresolved += token_segments(value)
                    .filter(|(_, _, token)| !known_tokens.contains(*token))
                    .count();
                Ok(match &matcher {
                    Some(matcher) => {
                        if let Some(budget) = &mut budget {
                            for found in matcher.find_iter(value.as_bytes()) {
                                budget
                                    .reserve_escaped(secrets[found.pattern().as_usize()].len())?;
                            }
                        }
                        replace_known_tokens(value, matcher, &secrets)
                    }
                    None => (value.to_owned(), 0),
                })
            })?;
            Ok(HydrationReport {
                text,
                replacements,
                unresolved,
            })
        };
        consume(&mut hydrate)
    }

    fn unlock_readonly_bounded(&self, limits: ReadonlyDictionaryLimits) -> crate::Result<Unlocked> {
        use crate::adapter::native_snapshot::{self, Limits, Unavailable};
        let cap = limits.vault_bytes;
        // The pinned reader bounds growth after stat and rejects special or substituted carriers.
        let bytes = native_snapshot::read_file_bytes(
            &self.store.path,
            Limits {
                bytes: cap,
                working_bytes: cap.checked_add(1).ok_or(HydrationBudgetExceeded)?,
                ..Limits::default()
            },
        )
        .map_err(|error| match error {
            Unavailable::BudgetExceeded => anyhow::Error::from(HydrationBudgetExceeded),
            other => anyhow::Error::from(other),
        })?;
        let file: super::VaultFile = serde_json::from_slice(&bytes)
            .context("the repository secret dictionary is malformed")?;
        if file.records.len() > limits.records {
            return Err(HydrationBudgetExceeded.into());
        }
        // Canonical identifiers bound AAD and matcher states before decryption or key lookup.
        anyhow::ensure!(
            file.vault_id.len() == 36 && uuid::Uuid::parse_str(&file.vault_id).is_ok(),
            "the repository secret dictionary has an invalid vault identity"
        );
        let mut encrypted = file.wrapped_dek.ciphertext.len();
        let mut patterns = 0usize;
        anyhow::ensure!(
            file.wrapped_dek.nonce.len() == 16 && encrypted == 64,
            "the repository secret dictionary has an invalid wrapped key"
        );
        for record in &file.records {
            anyhow::ensure!(
                record.id.strip_prefix("sec_").is_some_and(|id| {
                    id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit())
                }) && record.sealed.nonce.len() == 16,
                "the repository secret dictionary has an invalid record identity or nonce"
            );
            encrypted = encrypted
                .checked_add(record.sealed.ciphertext.len())
                .ok_or(HydrationBudgetExceeded)?;
            patterns = patterns
                .checked_add(CANONICAL_TOKEN_LEN)
                .ok_or(HydrationBudgetExceeded)?;
        }
        if encrypted > cap || patterns > limits.pattern_bytes {
            return Err(HydrationBudgetExceeded.into());
        }
        // Base64 ciphertext bounds the aggregate padded plaintext; the sparse NFA cannot
        // select a dense DFA whose states multiply the dictionary's pattern footprint.
        self.store.unlock_file(file, true)
    }

    /// Project only records that came from explicit/global registration (plus
    /// legacy records). Commit continuity uses this view to distinguish a
    /// retry-safe heuristic forward projection from a true rewrite caused by a
    /// later policy registration.
    pub fn protect_registered_jsonl(&self, text: &str) -> crate::Result<ProtectionReport> {
        self.store.with_lock(|| {
            let mut state = ProtectionState::load_with_scope(
                &self.store,
                &Matcher::empty(),
                &[],
                ExistingRecordScope::Registered,
            )?;
            let (text, replacements) = transform_jsonl(text, |s| state.protect_string(s))?;
            state.persist()?;
            Ok(ProtectionReport {
                text,
                replacements,
                new_records: state.new_records,
                new_heuristic_records: state.new_heuristic_records,
                intact: state.intact_hits,
            })
        })
    }

    pub fn review(&self) -> crate::Result<Vec<RepositoryRecordSummary>> {
        self.store.with_lock(|| {
            if !self.store.path.exists() {
                return Ok(vec![]);
            }
            let unlocked = self.store.unlock_existing()?;
            let allowlist = local_allowlist(&[], &Matcher::empty())?;
            let mut out: Vec<_> = super::decrypt_records(&unlocked.file, &unlocked.dek)?
                .iter()
                .map(|record| record_summary(record, &allowlist))
                .collect();
            out.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
            Ok(out)
        })
    }

    pub(crate) fn active_matcher(&self) -> crate::Result<Matcher> {
        self.matcher_for(false)
    }

    pub(crate) fn registered_matcher(&self) -> crate::Result<Matcher> {
        self.matcher_for(true)
    }

    fn matcher_for(&self, registered_only: bool) -> crate::Result<Matcher> {
        self.store.with_lock(|| {
            if !self.store.path.exists() {
                return Ok(
                    Matcher::empty().with_allowlist(local_allowlist(&[], &Matcher::empty())?)
                );
            }
            let unlocked = self.store.unlock_existing()?;
            let generation = unlocked.file.generation;
            let records = super::decrypt_records(&unlocked.file, &unlocked.dek)?;
            let allowlist = local_allowlist(&records, &Matcher::empty())?;
            let records = records
                .into_iter()
                .filter(|record| {
                    if allowlist.contains(record.secret.as_str())
                        || crate::domain::secrets::rules::preset_allows(&record.secret)
                    {
                        return false;
                    }
                    if registered_only {
                        registered_protect(record)
                    } else {
                        effective_protect(record)
                    }
                })
                .collect();
            Ok(Matcher::build(generation, records)?.with_allowlist(allowlist))
        })
    }

    pub fn allow(&self, record_id: &str) -> crate::Result<RepositoryRecordSummary> {
        self.update_record(record_id, |record| {
            record.heuristic_disposition = super::HeuristicDisposition::Allow;
            Ok(())
        })
    }

    pub fn unallow(&self, record_id: &str) -> crate::Result<RepositoryRecordSummary> {
        self.update_record(record_id, |record| {
            record.heuristic_disposition = super::HeuristicDisposition::Protect;
            Ok(())
        })
    }

    pub fn block_add(
        &self,
        name: &str,
        secret: Zeroizing<String>,
        allow_short: bool,
    ) -> crate::Result<RepositoryRecordSummary> {
        super::validate_registration(name, &secret, allow_short)?;
        self.store.with_lock(|| {
            let allowlist = local_allowlist(&[], &Matcher::empty())?;
            let created = !self.store.path.exists();
            let mut unlocked = if created {
                self.store.create_unlocked()?
            } else {
                self.store.unlock_existing()?
            };
            let mut records = super::decrypt_records(&unlocked.file, &unlocked.dek)?;
            if let Some(record) = records
                .iter_mut()
                .find(|record| record.secret.as_bytes() == secret.as_bytes())
            {
                if !record.origins.contains(&RecordOrigin::Explicit) {
                    record.origins.push(RecordOrigin::Explicit);
                }
                record.explicit_block = true;
                record.name = name.to_string();
                record.updated_at = chrono::Utc::now().to_rfc3339();
                reseal_record(&mut unlocked, record)?;
                bump_and_write(&self.store, &mut unlocked, created)?;
                return Ok(record_summary(record, &allowlist));
            }

            let id = format!("sec_{}", uuid::Uuid::now_v7().simple());
            let now = chrono::Utc::now().to_rfc3339();
            let record = DecryptedRecord {
                id,
                name: name.to_string(),
                secret,
                origins: vec![RecordOrigin::Explicit],
                heuristic_disposition: super::HeuristicDisposition::Protect,
                explicit_block: true,
                created_at: now.clone(),
                updated_at: now,
            };
            append_record(&mut unlocked, &record)?;
            let summary = record_summary(&record, &allowlist);
            bump_and_write(&self.store, &mut unlocked, created)?;
            Ok(summary)
        })
    }

    pub fn block_remove(&self, record_id: &str) -> crate::Result<RepositoryRecordSummary> {
        self.update_record(record_id, |record| {
            if legacy_record(record) {
                record.origins.push(RecordOrigin::Global);
            }
            record.explicit_block = false;
            record
                .origins
                .retain(|origin| *origin != RecordOrigin::Explicit);
            Ok(())
        })
    }

    fn update_record(
        &self,
        record_id: &str,
        update: impl FnOnce(&mut DecryptedRecord) -> crate::Result<()>,
    ) -> crate::Result<RepositoryRecordSummary> {
        self.store.with_lock(|| {
            let allowlist = local_allowlist(&[], &Matcher::empty())?;
            if !self.store.path.exists() {
                bail!("the repository secret dictionary has not been initialized");
            }
            let mut unlocked = self.store.unlock_existing()?;
            let mut records = super::decrypt_records(&unlocked.file, &unlocked.dek)?;
            let Some(record) = records.iter_mut().find(|record| record.id == record_id) else {
                bail!("no repository secret identified by `{record_id}`");
            };
            update(record)?;
            record.updated_at = chrono::Utc::now().to_rfc3339();
            reseal_record(&mut unlocked, record)?;
            bump_and_write(&self.store, &mut unlocked, false)?;
            Ok(record_summary(record, &allowlist))
        })
    }

    /// Validate envelopes before projecting native content; projected hashes describe the emitted bytes.
    pub fn protect_envelopes(
        &self,
        saved: &str,
        global: &Matcher,
    ) -> crate::Result<ProtectionReport> {
        use crate::domain::{storage, transcript};
        let envelopes = saved
            .split_inclusive('\n')
            .map(storage::parse_envelope_line)
            .collect::<crate::Result<Vec<_>>>()?;
        let masks = if let Some(root) = &self.repo_root {
            crate::domain::secrets::saved_content_masks(
                &crate::domain::repo::Repo::at(root),
                saved,
            )?
        } else {
            Vec::new()
        };
        let mut masks = masks.into_iter();
        let mut report =
            self.protect_with_masks(&transcript::unwrap_strict(saved)?, global, |_| {
                masks.next().unwrap_or_default()
            })?;
        let contents = report
            .text
            .lines()
            .map(serde_json::from_str::<Value>)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        anyhow::ensure!(
            contents.len() == envelopes.len(),
            "protection changed the native record count"
        );
        let mut text = String::new();
        for (mut envelope, content) in envelopes.into_iter().zip(contents) {
            envelope.content = content;
            envelope.object_hash = transcript::object_hash(&envelope.content);
            text.push_str(&storage::envelope_line(&envelope));
        }
        report.text = text;
        Ok(report)
    }

    /// Hydration changes only native content; provenance stays intact and hashes describe the new content.
    pub fn hydrate_envelopes(&self, saved: &str) -> crate::Result<HydrationReport> {
        self.hydrate_envelopes_with(saved, false)
    }

    /// Local presentation validates stored identities before restoring content without changing the vault.
    pub fn hydrate_envelopes_readonly(&self, saved: &str) -> crate::Result<HydrationReport> {
        self.hydrate_envelopes_with(saved, true)
    }

    fn hydrate_envelopes_with(
        &self,
        saved: &str,
        readonly: bool,
    ) -> crate::Result<HydrationReport> {
        use crate::domain::{storage, transcript};
        let envelopes = saved
            .split_inclusive('\n')
            .map(storage::parse_envelope_line)
            .collect::<crate::Result<Vec<_>>>()?;
        let raw = transcript::unwrap_strict(saved)?;
        let mut report = if readonly {
            self.hydrate_pair_readonly(&raw, "")?.0
        } else {
            self.hydrate_jsonl(&raw)?
        };
        let contents = report
            .text
            .lines()
            .map(serde_json::from_str::<Value>)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        anyhow::ensure!(
            contents.len() == envelopes.len(),
            "hydration changed the native record count"
        );
        let mut text = String::new();
        for (mut envelope, content) in envelopes.into_iter().zip(contents) {
            envelope.content = content;
            envelope.object_hash = transcript::object_hash(&envelope.content);
            text.push_str(&storage::envelope_line(&envelope));
        }
        report.text = text;
        Ok(report)
    }

    /// Restore known placeholders only while materializing a session into a
    /// local runtime. Unknown/foreign tokens remain visible and are counted.
    pub fn hydrate_jsonl(&self, text: &str) -> crate::Result<HydrationReport> {
        if !self.store.path.exists() {
            return Ok(HydrationReport {
                text: text.to_string(),
                replacements: 0,
                unresolved: count_token_prefixes(text),
            });
        }

        self.store.with_lock(|| {
            let unlocked = self.store.unlock_existing()?;
            let records = super::decrypt_records(&unlocked.file, &unlocked.dek)?;
            let vault_id = unlocked.file.vault_id;
            let patterns: Vec<String> = records
                .iter()
                .map(|record| token(&vault_id, &record.id))
                .collect();
            let secrets: Vec<&str> = records.iter().map(|r| r.secret.as_str()).collect();
            let ac = if patterns.is_empty() {
                None
            } else {
                Some(
                    AhoCorasickBuilder::new()
                        .match_kind(MatchKind::LeftmostLongest)
                        .build(patterns.iter().map(String::as_bytes))
                        .context("cannot build the repository secret hydrator")?,
                )
            };
            let (text, replacements) = transform_jsonl(text, |s| {
                Ok(match &ac {
                    Some(ac) => replace_known_tokens(s, ac, &secrets),
                    None => (s.to_string(), 0),
                })
            })?;
            Ok(HydrationReport {
                unresolved: count_token_prefixes(&text),
                text,
                replacements,
            })
        })
    }
}

#[derive(Clone)]
struct PatternSource {
    record_id: Option<String>,
    from_global: bool,
    from_heuristic: bool,
}

#[derive(Default)]
struct MergedSources {
    explicit_block: bool,
    heuristic: bool,
    global: bool,
    explicit: bool,
}

impl MergedSources {
    fn add(&mut self, record: &DecryptedRecord) {
        self.explicit_block |= record.explicit_block || legacy_record(record);
        self.heuristic |= record.origins.contains(&RecordOrigin::Heuristic);
        self.global |= record.origins.contains(&RecordOrigin::Global);
        self.explicit |= record.origins.contains(&RecordOrigin::Explicit);
    }
}

struct ProjectionBuffer {
    out: String,
    cursor: usize,
    replacements: usize,
}

struct PatternSpec {
    secret: Zeroizing<String>,
    record_id: Option<String>,
    from_global: bool,
    from_heuristic: bool,
    active: bool,
}

#[derive(Clone, Copy)]
enum ExistingRecordScope {
    All,
    Registered,
}

struct ProtectionState<'a, K: KeyStore> {
    store: &'a VaultStore<K>,
    unlocked: Option<Unlocked>,
    records: Vec<DecryptedRecord>,
    ac: Option<Arc<AhoCorasick>>,
    sources: Vec<PatternSource>,
    allowlist: HashSet<String>,
    dirty: bool,
    created: bool,
    new_records: usize,
    new_heuristic_records: usize,
    intact_hits: usize,
    /// Set only by the settlement path. `None` skips the extra scan entirely,
    /// which is what every continuity view wants.
    oversized_threshold: Option<usize>,
}

impl<'a, K: KeyStore> ProtectionState<'a, K> {
    fn load(
        store: &'a VaultStore<K>,
        global: &Matcher,
        candidates: &[Zeroizing<String>],
    ) -> crate::Result<Self> {
        Self::load_with_scope(store, global, candidates, ExistingRecordScope::All)
    }

    fn load_with_scope(
        store: &'a VaultStore<K>,
        global: &Matcher,
        candidates: &[Zeroizing<String>],
        scope: ExistingRecordScope,
    ) -> crate::Result<Self> {
        let (unlocked, records) = Self::read_records(store)?;
        let allowlist = local_allowlist(&records, global)?;
        Self::from_records(
            store, global, candidates, scope, unlocked, records, allowlist,
        )
    }

    fn read_records(
        store: &VaultStore<K>,
    ) -> crate::Result<(Option<Unlocked>, Vec<DecryptedRecord>)> {
        let (unlocked, records) = if store.path.exists() {
            let unlocked = store.unlock_existing()?;
            let records = super::decrypt_records(&unlocked.file, &unlocked.dek)?;
            (Some(unlocked), records)
        } else {
            (None, vec![])
        };
        Ok((unlocked, records))
    }

    fn from_records(
        store: &'a VaultStore<K>,
        global: &Matcher,
        candidates: &[Zeroizing<String>],
        scope: ExistingRecordScope,
        unlocked: Option<Unlocked>,
        records: Vec<DecryptedRecord>,
        allowlist: HashSet<String>,
    ) -> crate::Result<Self> {
        let mut specs: Vec<PatternSpec> =
            Vec::with_capacity(records.len() + global.rules() + candidates.len());
        for record in &records {
            specs.push(PatternSpec {
                secret: Zeroizing::new(record.secret.to_string()),
                record_id: Some(record.id.clone()),
                from_global: false,
                from_heuristic: false,
                active: match scope {
                    ExistingRecordScope::All => effective_protect(record),
                    ExistingRecordScope::Registered => registered_protect(record),
                },
            });
        }

        for (_, secret) in global.patterns() {
            if let Some(spec) = specs.iter_mut().find(|spec| spec.secret.as_str() == secret) {
                spec.from_global = true;
                spec.active = true;
            } else {
                specs.push(PatternSpec {
                    secret: Zeroizing::new(secret.to_string()),
                    record_id: None,
                    from_global: true,
                    from_heuristic: false,
                    active: true,
                });
            }
        }

        for candidate in candidates {
            if let Some(spec) = specs
                .iter_mut()
                .find(|spec| spec.secret.as_str() == candidate.as_str())
            {
                spec.from_heuristic = true;
                spec.active = true;
            } else {
                specs.push(PatternSpec {
                    secret: Zeroizing::new(candidate.to_string()),
                    record_id: None,
                    from_global: false,
                    from_heuristic: true,
                    active: true,
                });
            }
        }

        let mut patterns = Vec::new();
        let mut sources = Vec::new();
        for spec in specs.into_iter().filter(|spec| {
            spec.active
                && !allowlist.contains(spec.secret.as_str())
                && !crate::domain::secrets::rules::preset_allows(&spec.secret)
        }) {
            patterns.push(spec.secret);
            sources.push(PatternSource {
                record_id: spec.record_id,
                from_global: spec.from_global,
                from_heuristic: spec.from_heuristic,
            });
        }

        let ac = if patterns.is_empty() {
            None
        } else {
            Some(Arc::new(
                AhoCorasickBuilder::new()
                    .match_kind(MatchKind::Standard)
                    .build(patterns.iter().map(|p| p.as_bytes()))
                    .context("cannot build the repository secret protector")?,
            ))
        };

        Ok(Self {
            store,
            unlocked,
            records,
            ac,
            sources,
            allowlist,
            dirty: false,
            created: false,
            new_records: 0,
            new_heuristic_records: 0,
            intact_hits: 0,
            oversized_threshold: None,
        })
    }

    fn protect_string(&mut self, text: &str) -> crate::Result<(String, usize)> {
        // Two kinds of region are opaque to projection.
        //
        // A syntactically valid placeholder, because a user may register a
        // short value such as "AGIT" and it must not corrupt a key an earlier
        // settlement already wrote. And a heuristic finding too long to store
        // reversibly, because replacing an oversized PEM's short BEGIN header
        // alone would hide the larger finding from the push gate without
        // leaving any key to undo it with.
        //
        // The second kind is rare and the test for it is exact — a match longer
        // than the threshold cannot occur in a string that is not — so the
        // common path below keeps the original lazy walk over placeholders.
        //
        // Counting comes before every early return, including the one for an
        // empty matcher. A settlement whose only finding is over-capacity has
        // nothing to match against at all: no records yet, no global rules, and
        // the candidate itself was refused for its size. Leaving those bytes
        // alone is right; reporting nothing is not. `agit push` will reject
        // them, and the line `agit commit` prints is where the user gets to
        // hear that first.
        let oversized = self.oversized_spans(text);
        self.intact_hits = self.intact_hits.saturating_add(oversized.len());

        let Some(ac) = self.ac.clone() else {
            return Ok((text.to_string(), 0));
        };
        if oversized.is_empty() {
            return self.protect_between(text, &ac, token_segments(text).map(|(s, e, _)| (s, e)));
        }
        let mut opaque: Vec<(usize, usize)> =
            token_segments(text).map(|(s, e, _)| (s, e)).collect();
        opaque.extend(oversized);
        opaque.sort_unstable();
        // A placeholder can sit inside a long finding, and two rules can report
        // the same key. Overlapping spans would make the walk below copy bytes
        // twice, so fuse them into one region first.
        let mut fused: Vec<(usize, usize)> = Vec::with_capacity(opaque.len());
        for (start, end) in opaque {
            match fused.last_mut() {
                Some((_, last_end)) if start <= *last_end => *last_end = (*last_end).max(end),
                _ => fused.push((start, end)),
            }
        }
        self.protect_between(text, &ac, fused.into_iter())
    }

    /// Project everything outside `opaque`, copying each opaque region as it
    /// stands. Regions must be ordered and non-overlapping.
    fn protect_between(
        &mut self,
        text: &str,
        ac: &AhoCorasick,
        opaque: impl Iterator<Item = (usize, usize)>,
    ) -> crate::Result<(String, usize)> {
        let mut out = String::with_capacity(text.len());
        let mut replacements = 0usize;
        let mut cursor = 0usize;
        for (start, end) in opaque {
            if start > cursor {
                let (part, count) = self.protect_segment(&text[cursor..start], ac)?;
                out.push_str(&part);
                replacements = replacements.saturating_add(count);
            }
            out.push_str(&text[start..end]);
            cursor = end;
        }
        if cursor < text.len() {
            let (part, count) = self.protect_segment(&text[cursor..], ac)?;
            out.push_str(&part);
            replacements = replacements.saturating_add(count);
        }
        Ok((out, replacements))
    }

    /// Where this string carries a finding no record could reverse.
    ///
    /// The length guard is the bound: the scan runs only for a string that
    /// could actually contain such a match, which in a session transcript is
    /// almost never.
    fn oversized_spans(&self, text: &str) -> Vec<(usize, usize)> {
        match self.oversized_threshold {
            Some(threshold) if text.len() > threshold => {
                crate::domain::secrets::oversized_finding_spans(text, threshold, |value| {
                    !self.allowlist.contains(value)
                })
            }
            _ => vec![],
        }
    }

    fn protect_segment(&mut self, text: &str, ac: &AhoCorasick) -> crate::Result<(String, usize)> {
        let mut projection = ProjectionBuffer {
            out: String::with_capacity(text.len()),
            cursor: 0,
            replacements: 0,
        };
        let mut pending = Vec::with_capacity(OVERLAPPING_MATCH_BATCH);
        let mut max_end = 0usize;
        let mut pending_ordered = true;
        let mut blocked_until = 0usize;
        // The overlapping iterator advances by match end. Keep the suffix that can still be
        // reached by a pattern; a connected component is projected only after that suffix closes.
        for found in ac.find_overlapping_iter(text.as_bytes()) {
            let current = (found.start(), found.end(), found.pattern().as_usize());
            max_end = max_end.max(current.1);
            if current.0 < blocked_until {
                blocked_until = blocked_until.max(current.1);
            }
            if pending
                .last()
                .is_some_and(|last: &(usize, usize, usize)| current.0 < last.0)
            {
                pending_ordered = false;
            }
            pending.push(current);
            if pending.len() > MAX_OVERLAPPING_MATCHES {
                bail!(
                    "overlapping secret matches exceed the bounded protection limit; no dictionary update was written"
                );
            }
            let safe_end = max_end.saturating_sub(ac.max_pattern_len());
            if pending.len() >= OVERLAPPING_MATCH_BATCH && safe_end >= blocked_until {
                self.flush_components(
                    text,
                    &mut pending,
                    safe_end,
                    &mut pending_ordered,
                    &mut blocked_until,
                    &mut projection,
                )?;
            }
        }
        self.flush_components(
            text,
            &mut pending,
            usize::MAX,
            &mut pending_ordered,
            &mut blocked_until,
            &mut projection,
        )?;
        if projection.replacements == 0 {
            return Ok((text.to_string(), 0));
        }
        projection.out.push_str(&text[projection.cursor..]);
        Ok((projection.out, projection.replacements))
    }

    fn flush_components(
        &mut self,
        text: &str,
        pending: &mut Vec<(usize, usize, usize)>,
        safe_end: usize,
        ordered: &mut bool,
        blocked_until: &mut usize,
        projection: &mut ProjectionBuffer,
    ) -> crate::Result<()> {
        if pending.is_empty() {
            return Ok(());
        }
        if safe_end < *blocked_until {
            return Ok(());
        }
        if !*ordered {
            pending.sort_unstable_by_key(|(start, end, _)| (*start, *end));
            *ordered = true;
        }
        let mut ready = 0usize;
        let mut index = 0usize;
        while index < pending.len() {
            let (_, mut region_end, _) = pending[index];
            let component_start = index;
            index += 1;
            while index < pending.len() && pending[index].0 < region_end {
                region_end = region_end.max(pending[index].1);
                index += 1;
            }
            if region_end > safe_end {
                *blocked_until = region_end;
                break;
            }
            self.protect_component(text, &pending[component_start..index], projection)?;
            ready = index;
        }
        if ready > 0 {
            pending.drain(..ready);
            if pending.is_empty() {
                *ordered = true;
                *blocked_until = 0;
            }
        }
        Ok(())
    }

    fn protect_component(
        &mut self,
        text: &str,
        matches: &[(usize, usize, usize)],
        projection: &mut ProjectionBuffer,
    ) -> crate::Result<()> {
        let (region_start, mut region_end, first_pattern) = matches[0];
        let mut exact = Some(first_pattern);
        for &(start, end, pattern) in &matches[1..] {
            let extended = end > region_end;
            region_end = region_end.max(end);
            if extended {
                exact = (start == region_start).then_some(pattern);
            }
        }
        let mut sources = MergedSources::default();
        let mut seen_patterns = HashSet::new();
        let mut exact_id = None;
        for &(start, end, pattern) in matches {
            if !seen_patterns.insert(pattern) {
                continue;
            }
            let id = self.ensure_record(pattern, &text[start..end])?;
            if exact == Some(pattern) {
                exact_id = Some(id.clone());
            }
            if let Some(record) = self.records.iter().find(|record| record.id == id) {
                sources.add(record);
            }
        }
        let id =
            if region_end - region_start > MAX_REPOSITORY_SECRET_BYTES {
                self.intact_hits = self.intact_hits.saturating_add(1);
                None
            } else {
                Some(exact_id.unwrap_or(
                    self.ensure_region_record(&text[region_start..region_end], &sources)?,
                ))
            };
        projection
            .out
            .push_str(&text[projection.cursor..region_start]);
        let replaced = id.is_some();
        if let Some(id) = id {
            let vault_id = &self
                .unlocked
                .as_ref()
                .expect("a matched pattern always initializes the dictionary")
                .file
                .vault_id;
            projection.out.push_str(&token(vault_id, &id));
        } else {
            projection.out.push_str(&text[region_start..region_end]);
        }
        projection.cursor = region_end;
        if replaced {
            projection.replacements = projection.replacements.saturating_add(1);
        }
        Ok(())
    }

    fn ensure_region_record(
        &mut self,
        secret: &str,
        sources: &MergedSources,
    ) -> crate::Result<String> {
        if let Some(record) = self
            .records
            .iter()
            .find(|record| record.secret.as_str() == secret)
        {
            let id = record.id.clone();
            let index = self
                .records
                .iter()
                .position(|record| record.id == id)
                .expect("record was found above");
            let mut changed = false;
            {
                let record = &mut self.records[index];
                if sources.explicit_block && !record.explicit_block {
                    record.explicit_block = true;
                    changed = true;
                }
                for origin in [
                    sources.heuristic.then_some(RecordOrigin::Heuristic),
                    sources.global.then_some(RecordOrigin::Global),
                    sources.explicit.then_some(RecordOrigin::Explicit),
                ]
                .into_iter()
                .flatten()
                {
                    if !record.origins.contains(&origin) {
                        record.origins.push(origin);
                        changed = true;
                    }
                }
                if changed {
                    record.updated_at = chrono::Utc::now().to_rfc3339();
                }
            }
            if changed {
                let record = &self.records[index];
                reseal_record(
                    self.unlocked
                        .as_mut()
                        .expect("an existing record has an unlocked dictionary"),
                    record,
                )?;
                self.dirty = true;
            }
            return Ok(id);
        }
        if self.unlocked.is_none() {
            self.unlocked = Some(self.store.create_unlocked()?);
            self.created = true;
        }
        let id = format!("sec_{}", uuid::Uuid::now_v7().simple());
        let now = chrono::Utc::now().to_rfc3339();
        let record = DecryptedRecord {
            id: id.clone(),
            name: format!("repository-{id}"),
            secret: Zeroizing::new(secret.to_owned()),
            origins: [
                sources.heuristic.then_some(RecordOrigin::Heuristic),
                sources.global.then_some(RecordOrigin::Global),
                sources.explicit.then_some(RecordOrigin::Explicit),
            ]
            .into_iter()
            .flatten()
            .collect(),
            heuristic_disposition: super::HeuristicDisposition::Protect,
            explicit_block: sources.explicit_block,
            created_at: now.clone(),
            updated_at: now,
        };
        append_record(self.unlocked.as_mut().expect("initialized above"), &record)?;
        self.records.push(record);
        self.dirty = true;
        self.new_records += 1;
        self.new_heuristic_records += 1;
        Ok(id)
    }

    fn ensure_record(&mut self, pattern: usize, secret: &str) -> crate::Result<String> {
        let source = self.sources[pattern].clone();
        if let Some(id) = &source.record_id {
            let index = self
                .records
                .iter()
                .position(|record| &record.id == id)
                .expect("matcher record id belongs to the unlocked dictionary");
            let record = &mut self.records[index];
            let mut changed = false;
            if source.from_global {
                if !record.origins.contains(&RecordOrigin::Global) {
                    record.origins.push(RecordOrigin::Global);
                    changed = true;
                }
                changed |= !record.explicit_block;
                record.explicit_block = true;
            }
            if source.from_heuristic && !record.origins.contains(&RecordOrigin::Heuristic) {
                record.origins.push(RecordOrigin::Heuristic);
                changed = true;
            }
            if changed {
                record.updated_at = chrono::Utc::now().to_rfc3339();
                let unlocked = self
                    .unlocked
                    .as_mut()
                    .expect("an existing record has an unlocked dictionary");
                reseal_record(unlocked, record)?;
                self.dirty = true;
            }
            return Ok(id.clone());
        }

        // Two patterns are deduplicated while building the automaton, but keep
        // this equality check as the storage invariant's last line of defence.
        if let Some(record) = self.records.iter().find(|r| r.secret.as_str() == secret) {
            let id = record.id.clone();
            self.sources[pattern].record_id = Some(id.clone());
            return Ok(id);
        }

        if self.unlocked.is_none() {
            self.unlocked = Some(self.store.create_unlocked()?);
            self.created = true;
        }
        let id = format!("sec_{}", uuid::Uuid::now_v7().simple());
        let now = chrono::Utc::now().to_rfc3339();
        let mut origins = Vec::with_capacity(2);
        if source.from_global {
            origins.push(RecordOrigin::Global);
        }
        if source.from_heuristic {
            origins.push(RecordOrigin::Heuristic);
        }
        let record = DecryptedRecord {
            id: id.clone(),
            name: format!("repository-{id}"),
            secret: Zeroizing::new(secret.to_string()),
            origins,
            heuristic_disposition: super::HeuristicDisposition::Protect,
            explicit_block: source.from_global,
            created_at: now.clone(),
            updated_at: now,
        };
        append_record(self.unlocked.as_mut().expect("initialized above"), &record)?;
        self.records.push(record);
        self.sources[pattern].record_id = Some(id.clone());
        self.dirty = true;
        self.new_records += 1;
        if source.from_heuristic && !source.from_global {
            self.new_heuristic_records += 1;
        }
        Ok(id)
    }

    fn persist(&mut self) -> crate::Result<()> {
        if !self.dirty {
            return Ok(());
        }
        let unlocked = self
            .unlocked
            .as_mut()
            .expect("dirty dictionary is initialized");
        unlocked.file.generation = unlocked.file.generation.saturating_add(1);
        unlocked.file.schema_version = CURRENT_SCHEMA_VERSION;
        unlocked.file.projection_version = CURRENT_PROJECTION_VERSION;
        if let Err(error) = write_vault(&self.store.path, &unlocked.file) {
            if self.created {
                let _ = self.store.keys.delete(&unlocked.file.vault_id);
            }
            return Err(error);
        }
        self.dirty = false;
        Ok(())
    }
}

impl<K: KeyStore> Drop for ProtectionState<'_, K> {
    fn drop(&mut self) {
        // `create_unlocked` has already installed a KEK. If transformation
        // fails before the first atomic vault write, remove that orphan rather
        // than leaving an unreachable credential-store entry behind.
        if self.created
            && self.dirty
            && !self.store.path.exists()
            && let Some(unlocked) = &self.unlocked
        {
            let _ = self.store.keys.delete(&unlocked.file.vault_id);
        }
    }
}

fn legacy_record(record: &DecryptedRecord) -> bool {
    record.origins.is_empty()
}

fn effective_protect(record: &DecryptedRecord) -> bool {
    record.heuristic_disposition != super::HeuristicDisposition::Allow
        && !crate::domain::secrets::rules::preset_allows(&record.secret)
        && (legacy_record(record)
            || record.explicit_block
            || record.origins.contains(&RecordOrigin::Heuristic))
}

fn local_allowlist(
    records: &[DecryptedRecord],
    global: &Matcher,
) -> crate::Result<HashSet<String>> {
    let home = crate::infra::config::agit_home()?;
    let mut allowed = crate::domain::secrets::load_allowlist(&home);
    allowed.extend(global.allowed_values().map(str::to_owned));
    allowed.extend(
        records
            .iter()
            .filter(|record| record.heuristic_disposition == super::HeuristicDisposition::Allow)
            .map(|record| record.secret.to_string()),
    );
    Ok(allowed)
}

fn registered_protect(record: &DecryptedRecord) -> bool {
    effective_protect(record)
        && (legacy_record(record)
            || record.explicit_block
            || record.origins.contains(&RecordOrigin::Global)
            || record.origins.contains(&RecordOrigin::Explicit))
}

fn record_summary(
    record: &DecryptedRecord,
    allowlist: &HashSet<String>,
) -> RepositoryRecordSummary {
    let origins = if legacy_record(record) {
        vec!["legacy".to_string()]
    } else {
        record
            .origins
            .iter()
            .map(|origin| match origin {
                RecordOrigin::Heuristic => "heuristic",
                RecordOrigin::Global => "global",
                RecordOrigin::Explicit => "explicit",
            })
            .map(str::to_string)
            .collect()
    };
    RepositoryRecordSummary {
        id: record.id.clone(),
        name: record.name.clone(),
        origins,
        heuristic_disposition: record.heuristic_disposition,
        explicit_block: record.explicit_block || legacy_record(record),
        effective_protect: effective_protect(record) && !allowlist.contains(record.secret.as_str()),
        created_at: record.created_at.clone(),
        updated_at: record.updated_at.clone(),
    }
}

fn append_record(unlocked: &mut Unlocked, record: &DecryptedRecord) -> crate::Result<()> {
    let sealed = seal_record(unlocked, record)?;
    unlocked.file.records.push(sealed);
    Ok(())
}

fn reseal_record(unlocked: &mut Unlocked, record: &DecryptedRecord) -> crate::Result<()> {
    let replacement = seal_record(unlocked, record)?;
    let Some(slot) = unlocked
        .file
        .records
        .iter_mut()
        .find(|stored| stored.id == record.id)
    else {
        bail!(
            "repository dictionary record {} is missing from its envelope",
            record.id
        );
    };
    *slot = replacement;
    Ok(())
}

fn seal_record(unlocked: &Unlocked, record: &DecryptedRecord) -> crate::Result<SealedRecord> {
    let mut plain = PlainRecord {
        name: record.name.clone(),
        secret: record.secret.to_string(),
        origins: record.origins.clone(),
        heuristic_disposition: record.heuristic_disposition,
        explicit_block: record.explicit_block,
        created_at: record.created_at.clone(),
        updated_at: record.updated_at.clone(),
    };
    let encoded = encode_padded(&plain)?;
    zeroize::Zeroize::zeroize(&mut plain.secret);
    let aad = record_aad(&unlocked.file.vault_id, &record.id, RECORD_VERSION);
    Ok(SealedRecord {
        id: record.id.clone(),
        version: RECORD_VERSION,
        sealed: seal(&unlocked.dek, &encoded, &aad)?,
    })
}

fn bump_and_write<K: KeyStore>(
    store: &VaultStore<K>,
    unlocked: &mut Unlocked,
    created: bool,
) -> crate::Result<()> {
    unlocked.file.generation = unlocked.file.generation.saturating_add(1);
    unlocked.file.schema_version = CURRENT_SCHEMA_VERSION;
    unlocked.file.projection_version = CURRENT_PROJECTION_VERSION;
    if let Err(error) = write_vault(&store.path, &unlocked.file) {
        if created {
            let _ = store.keys.delete(&unlocked.file.vault_id);
        }
        return Err(error);
    }
    Ok(())
}

fn observation_fields(metadata: &mut crate::domain::meta::Meta) -> Vec<&mut String> {
    let mut fields = vec![&mut metadata.cwd];
    fields.extend(metadata.code.iter_mut());
    fields.extend(metadata.milestone.iter_mut());
    if let Some(state) = metadata.cwd_state.as_mut() {
        if !crate::domain::secrets::identity::empty_status_digest(state) {
            fields.extend(state.status_digest.iter_mut());
        }
        fields.extend(state.origin.iter_mut());
        fields.extend(state.branch.iter_mut());
    }
    fields
}

fn transform_jsonl(
    text: &str,
    mut transform: impl FnMut(&str) -> crate::Result<(String, usize)>,
) -> crate::Result<(String, usize)> {
    let mut out = String::with_capacity(text.len());
    let mut replacements = 0usize;
    for (chunk, value) in crate::domain::secrets::jsonl_chunks(text) {
        match value {
            Some(mut value) => {
                replacements =
                    replacements.saturating_add(transform_value(&mut value, &mut transform)?);
                out.push_str(&serde_json::to_string(&value)?);
                if chunk.ends_with('\n') {
                    out.push('\n');
                }
            }
            None => {
                let (protected, count) = transform(chunk.strip_suffix('\n').unwrap_or(chunk))?;
                out.push_str(&protected);
                if chunk.ends_with('\n') {
                    out.push('\n');
                }
                replacements = replacements.saturating_add(count);
            }
        }
    }
    Ok((out, replacements))
}

fn transform_value(
    value: &mut Value,
    transform: &mut impl FnMut(&str) -> crate::Result<(String, usize)>,
) -> crate::Result<usize> {
    match value {
        Value::String(text) => {
            let (next, count) = transform(text)?;
            *text = next;
            Ok(count)
        }
        Value::Array(values) => {
            let mut total = 0usize;
            for value in values {
                total = total.saturating_add(transform_value(value, transform)?);
            }
            Ok(total)
        }
        Value::Object(map) => {
            let old = std::mem::take(map);
            let mut total = 0usize;
            for (key, mut value) in old {
                let (key, key_count) = transform(&key)?;
                total = total.saturating_add(key_count);
                total = total.saturating_add(transform_value(&mut value, transform)?);
                if map.insert(key, value).is_some() {
                    anyhow::bail!("secret placeholder replacement produced a duplicate JSON key");
                }
            }
            Ok(total)
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => Ok(0),
    }
}

fn token(vault_id: &str, record_id: &str) -> String {
    format!("{TOKEN_PREFIX}{vault_id}:{record_id}{TOKEN_SUFFIX}")
}

pub(super) use crate::domain::secrets::placeholder::{streaming_token_start, token_segments};

fn replace_known_tokens(text: &str, ac: &AhoCorasick, secrets: &[&str]) -> (String, usize) {
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    let mut replacements = 0usize;
    for found in ac.find_iter(text.as_bytes()) {
        out.push_str(&text[cursor..found.start()]);
        out.push_str(secrets[found.pattern().as_usize()]);
        cursor = found.end();
        replacements = replacements.saturating_add(1);
    }
    if replacements == 0 {
        return (text.to_string(), 0);
    }
    out.push_str(&text[cursor..]);
    (out, replacements)
}

fn count_token_prefixes(text: &str) -> usize {
    text.match_indices(TOKEN_PREFIX).count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(Default)]
    struct MemoryKeys(Mutex<HashMap<String, Vec<u8>>>);

    impl KeyStore for MemoryKeys {
        fn get(&self, vault_id: &str) -> crate::Result<Zeroizing<Vec<u8>>> {
            self.0
                .lock()
                .unwrap()
                .get(vault_id)
                .cloned()
                .map(Zeroizing::new)
                .context("missing test key")
        }

        fn set(&self, vault_id: &str, key: &[u8]) -> crate::Result<()> {
            self.0
                .lock()
                .unwrap()
                .insert(vault_id.to_string(), key.to_vec());
            Ok(())
        }

        fn delete(&self, vault_id: &str) -> crate::Result<()> {
            self.0.lock().unwrap().remove(vault_id);
            Ok(())
        }
    }

    /// A rule matching a structural word must only project user-controlled observations.
    #[test]
    fn metadata_projection_preserves_schema_and_identity() {
        use crate::domain::meta::{self, Meta};

        let dir = tempfile::tempdir().unwrap();
        let dictionary =
            RepositoryDictionary::new(dir.path().join("vault.json"), MemoryKeys::default());
        let claim = format!("agit-{}", "a".repeat(40));
        let mut metadata = Meta::new(claim.clone(), "codex".into(), "/turn/project".into());
        metadata.turn = Some(1);
        metadata.milestone = Some("turn with \"quotes\"".into());
        let original = serde_json::to_value(&metadata).unwrap();
        let report = dictionary
            .protect_metadata(
                &mut metadata,
                &Matcher::for_test(&[("word", "turn"), ("escaped", "with \"quotes\"")]),
            )
            .unwrap();
        assert!(report.replacements > 0);
        meta::validate(&metadata).unwrap();
        assert_eq!(metadata.session, claim);
        assert_eq!(metadata.runtime, "codex");
        assert_eq!(metadata.kind, meta::Kind::Turn);
        assert_eq!(metadata.turn, Some(1));
        assert!(!metadata.cwd.contains("turn"));
        assert!(!metadata.milestone.as_ref().unwrap().contains("quotes"));
        let protected = metadata.clone();
        let vault_before = std::fs::read(dir.path().join("vault.json")).unwrap();
        assert_eq!(
            dictionary.hydrate_metadata_readonly(&mut metadata).unwrap(),
            0
        );
        assert_eq!(serde_json::to_value(metadata).unwrap(), original);
        assert_eq!(
            std::fs::read(dir.path().join("vault.json")).unwrap(),
            vault_before
        );
        let missing = dir.path().join("missing/vault.json");
        let unavailable = RepositoryDictionary::new(missing.clone(), MemoryKeys::default());
        let mut metadata = protected.clone();
        assert!(
            unavailable
                .hydrate_metadata_readonly(&mut metadata)
                .unwrap()
                > 0
        );
        assert_eq!(
            serde_json::to_value(metadata).unwrap(),
            serde_json::to_value(protected).unwrap()
        );
        assert!(!missing.parent().unwrap().exists());
    }

    #[test]
    fn short_heuristic_records_remain_reversible_after_reopening() {
        let dir = tempfile::tempdir().unwrap();
        let dictionary =
            RepositoryDictionary::new(dir.path().join("vault.json"), MemoryKeys::default());
        let input = "{\"message\":\"fixture: abc\"}\n";
        let protected = dictionary
            .store
            .with_lock(|| {
                let mut state = ProtectionState::load(
                    &dictionary.store,
                    &Matcher::empty(),
                    &[Zeroizing::new("abc".into())],
                )?;
                let (text, count) = transform_jsonl(input, |text| state.protect_string(text))?;
                assert_eq!(count, 1);
                state.persist()?;
                Ok(text)
            })
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&protected).unwrap();
        let message = value["message"].as_str().unwrap();
        let placeholders = token_segments(message).collect::<Vec<_>>();
        assert_eq!(placeholders.len(), 1);
        assert_eq!(message, format!("fixture: {}", placeholders[0].2));
        assert_eq!(dictionary.hydrate_jsonl(&protected).unwrap().text, input);
        assert_eq!(
            dictionary
                .protect_jsonl(input, &Matcher::empty())
                .unwrap()
                .text,
            protected
        );
    }

    #[test]
    fn bounded_readonly_batch_preserves_frames_and_preflights_secret_expansion() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dictionary/vault.json");
        let dictionary = RepositoryDictionary::new(path.clone(), MemoryKeys::default());
        let secret = format!("{}\n\"suffix", "private payload ".repeat(128));
        let raw = format!(
            "{}\n",
            serde_json::json!({"message": secret, "context": "public ".repeat(128)})
        );
        let protected = dictionary
            .protect_jsonl(&raw, &Matcher::for_test(&[("explicit", &secret)]))
            .unwrap();
        let repeated = format!("{}{}", protected.text, protected.text);
        let partial = protected.text.trim_end_matches('\n');
        let unknown = format!(
            "{{{{AGIT_SECRET_V1:00000000-0000-4000-8000-000000000001:sec_{}}}}}",
            "a".repeat(32)
        );
        let unknown = serde_json::json!({"message": unknown}).to_string();
        let inputs = [
            "",
            protected.text.as_str(),
            repeated.as_str(),
            partial,
            unknown.as_str(),
        ];
        let baseline = inputs.iter().map(|input| input.len() * 6).sum::<usize>();
        let before = std::fs::read(&path).unwrap();
        let keys = dictionary.store.keys.0.lock().unwrap().clone();
        let lock = path.parent().unwrap().join("vault.lock");
        std::fs::remove_file(&lock).unwrap();
        let exhausted = dictionary
            .hydrate_batch_readonly_bounded(&inputs, baseline)
            .unwrap();
        assert!(exhausted.iter().any(|report| {
            report
                .as_ref()
                .is_err_and(|error| error.downcast_ref::<HydrationBudgetExceeded>().is_some())
        }));
        let reports = dictionary
            .hydrate_batch_readonly_bounded(&inputs, baseline + secret.len() * 6 * 4)
            .unwrap()
            .into_iter()
            .collect::<crate::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(reports.len(), inputs.len());
        assert_eq!(reports[0].text, "");
        assert_eq!(reports[1].text, raw);
        assert_eq!(reports[2].text, format!("{raw}{raw}"));
        assert_eq!(reports[3].text, raw.trim_end_matches('\n'));
        assert_eq!(reports[4].unresolved, 1);
        assert_eq!(reports[2].text.lines().count(), 2);
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(*dictionary.store.keys.0.lock().unwrap(), keys);
        assert!(!lock.exists());
        assert_eq!(
            dictionary
                .hydrate_pair_readonly(&protected.text, &raw)
                .unwrap()
                .0
                .text,
            raw
        );
    }

    #[test]
    fn bounded_readonly_vault_admission_precedes_key_lookup_and_decryption() {
        struct NoKeyReads;
        impl KeyStore for NoKeyReads {
            fn get(&self, _: &str) -> crate::Result<Zeroizing<Vec<u8>>> {
                panic!("oversized dictionary admission must precede key access")
            }
            fn set(&self, _: &str, _: &[u8]) -> crate::Result<()> {
                panic!("read-only inspection cannot create keys")
            }
            fn delete(&self, _: &str) -> crate::Result<()> {
                panic!("read-only inspection cannot delete keys")
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dictionary/vault.json");
        let dictionary = RepositoryDictionary::new(path.clone(), MemoryKeys::default());
        let raw = "{\"message\":\"stored-secret\"}\n";
        let protected = dictionary
            .protect_jsonl(raw, &Matcher::for_test(&[("explicit", "stored-secret")]))
            .unwrap()
            .text;
        let original = std::fs::read(&path).unwrap();
        let keys = dictionary.store.keys.0.lock().unwrap().clone();
        let lock = path.parent().unwrap().join("vault.lock");
        std::fs::remove_file(&lock).unwrap();
        let denied = RepositoryDictionary::new(path.clone(), NoKeyReads);
        let mut oversized = original.clone();
        oversized.resize(ReadonlyDictionaryLimits::STATUS.vault_bytes + 1, b' ');
        let mut crowded: super::super::VaultFile = serde_json::from_slice(&original).unwrap();
        crowded.records.resize(
            ReadonlyDictionaryLimits::STATUS.records + 1,
            crowded.records[0].clone(),
        );
        let crowded = serde_json::to_vec(&crowded).unwrap();
        assert!(crowded.len() < ReadonlyDictionaryLimits::STATUS.vault_bytes);
        for bytes in [&oversized, &crowded] {
            std::fs::write(&path, bytes).unwrap();
            let error = denied
                .hydrate_batch_readonly_with_limits(
                    &[&protected, raw],
                    8 * 1024 * 1024,
                    Some(ReadonlyDictionaryLimits::STATUS),
                )
                .unwrap_err();
            assert!(error.downcast_ref::<HydrationBudgetExceeded>().is_some());
            assert_eq!(std::fs::read(&path).unwrap(), *bytes);
            assert!(!lock.exists());
        }
        // Whitespace padding is valid vault syntax for the unrestricted diff reader.
        std::fs::write(&path, &oversized).unwrap();
        assert_eq!(
            dictionary
                .hydrate_pair_readonly(&protected, raw)
                .unwrap()
                .0
                .text,
            raw
        );
        std::fs::write(&path, &original).unwrap();
        let reports = dictionary
            .hydrate_batch_readonly_with_limits(
                &[&protected, raw],
                8 * 1024 * 1024,
                Some(ReadonlyDictionaryLimits::STATUS),
            )
            .unwrap()
            .into_iter()
            .collect::<crate::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(reports[0].text, raw);
        assert_eq!(reports[1].text, raw);
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(*dictionary.store.keys.0.lock().unwrap(), keys);
        assert!(!lock.exists());
    }

    #[cfg(unix)]
    #[test]
    fn bounded_readonly_dictionary_refuses_symlinks_and_pipes_without_locks() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        let path = dir.path().join("vault.json");
        std::fs::write(&target, "private sentinel").unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        let dictionary = RepositoryDictionary::new(path.clone(), MemoryKeys::default());
        assert!(
            dictionary
                .hydrate_batch_readonly_with_limits(
                    &["{}\n"],
                    1024,
                    Some(ReadonlyDictionaryLimits::STATUS),
                )
                .is_err()
        );
        assert_eq!(std::fs::read_link(&path).unwrap(), target);
        std::fs::remove_file(&path).unwrap();
        let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: name is a live NUL-terminated path inside the owned fixture.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(
            dictionary
                .hydrate_batch_readonly_with_limits(
                    &["{}\n"],
                    1024,
                    Some(ReadonlyDictionaryLimits::STATUS),
                )
                .is_err()
        );
        use std::os::unix::fs::FileTypeExt;
        assert!(
            std::fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_fifo()
        );
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "private sentinel"
        );
        assert!(!dir.path().join("vault.lock").exists());
        assert!(dictionary.store.keys.0.lock().unwrap().is_empty());
    }

    #[test]
    fn bounded_batch_isolates_candidate_key_collisions_without_rereading_or_writing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dictionary/vault.json");
        let dictionary = RepositoryDictionary::new(path.clone(), MemoryKeys::default());
        let secret = "literal private key";
        let raw = format!("{}\n", serde_json::json!({"message":secret}));
        let protected = dictionary
            .protect_jsonl(&raw, &Matcher::for_test(&[("explicit", secret)]))
            .unwrap()
            .text;
        let value: Value = serde_json::from_str(&protected).unwrap();
        let placeholder = value["message"].as_str().unwrap();
        let collision = Value::Object(serde_json::Map::from_iter([
            (secret.to_owned(), Value::from(1)),
            (placeholder.to_owned(), Value::from(2)),
        ]))
        .to_string();
        let before = std::fs::read(&path).unwrap();
        let keys = dictionary.store.keys.0.lock().unwrap().clone();
        let lock = path.parent().unwrap().join("vault.lock");
        std::fs::remove_file(&lock).unwrap();
        let reports = dictionary
            .hydrate_batch_readonly_bounded(&[&protected, &collision, &protected], 1024 * 1024)
            .unwrap();
        assert_eq!(reports.len(), 3);
        assert_eq!(reports[0].as_ref().unwrap().text, raw);
        assert!(
            reports[1]
                .as_ref()
                .unwrap_err()
                .to_string()
                .contains("duplicate JSON key")
        );
        assert_eq!(reports[2].as_ref().unwrap().text, raw);
        assert!(
            dictionary
                .hydrate_pair_readonly(&collision, &protected)
                .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(*dictionary.store.keys.0.lock().unwrap(), keys);
        assert!(!lock.exists());
    }

    #[test]
    fn readonly_hydration_preserves_dictionary_keys_and_lock_absence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dictionary/vault.json");
        let dictionary = RepositoryDictionary::new(path.clone(), MemoryKeys::default());
        let input = "{\"message\":\"stored-secret\"}\n";
        let original = dictionary
            .protect_jsonl(input, &Matcher::for_test(&[("global", "stored-secret")]))
            .unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        let keys = dictionary.store.keys.0.lock().unwrap().clone();
        let lock = path.parent().unwrap().join("vault.lock");
        std::fs::remove_file(&lock).unwrap();

        let (committed, projected) = dictionary
            .hydrate_pair_readonly(&original.text, input)
            .unwrap();
        assert_eq!(projected.text, input);
        assert_eq!(committed.text, projected.text);
        assert_eq!(committed.unresolved, 0);
        assert_eq!(projected.unresolved, 0);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            modified
        );
        assert_eq!(*dictionary.store.keys.0.lock().unwrap(), keys);
        assert!(!lock.exists());

        dictionary.store.keys.0.lock().unwrap().clear();
        assert!(dictionary.hydrate_pair_readonly(input, input).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert!(!lock.exists());
        assert!(dictionary.store.keys.0.lock().unwrap().is_empty());
    }

    #[test]
    fn readonly_hydration_does_not_initialize_an_absent_dictionary() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("absent");
        let dictionary =
            RepositoryDictionary::new(parent.join("vault.json"), MemoryKeys::default());
        let input = "{\"message\":\"unregistered-content\"}\n";
        let result = dictionary.hydrate_pair_readonly(input, input).unwrap().0;
        assert_eq!(result.text, input);
        assert_eq!(result.replacements, 0);
        assert!(!parent.exists());
        assert!(dictionary.store.keys.0.lock().unwrap().is_empty());
    }

    #[test]
    fn readonly_hydration_counts_unknown_placeholders_without_creating_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent/vault.json");
        let dictionary = RepositoryDictionary::new(path.clone(), MemoryKeys::default());
        let unknown = token(
            "00000000-0000-4000-8000-000000000001",
            &format!("sec_{}", "a".repeat(32)),
        );
        let input = serde_json::json!({"content": unknown}).to_string();
        let (committed, live) = dictionary
            .hydrate_pair_readonly(&input, "{\"content\":\"literal\"}")
            .unwrap();
        assert_eq!(committed.unresolved, 1);
        assert_eq!(live.unresolved, 0);
        assert!(!path.parent().unwrap().exists());
        assert!(dictionary.store.keys.0.lock().unwrap().is_empty());
    }

    #[test]
    fn readonly_comparison_survives_overlapping_mapping_additions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault.json");
        let dictionary = RepositoryDictionary::new(path.clone(), MemoryKeys::default());
        let input = "{\"content\":\"stored-secret suffix\"}\n";
        let original = dictionary
            .protect_jsonl(input, &Matcher::for_test(&[("short", "stored-secret")]))
            .unwrap();
        dictionary
            .protect_jsonl(
                input,
                &Matcher::for_test(&[("long", "stored-secret suffix")]),
            )
            .unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let (committed, live) = dictionary
            .hydrate_pair_readonly(&original.text, input)
            .unwrap();
        assert_eq!(committed.text, live.text);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }

    #[cfg(unix)]
    #[test]
    fn readonly_hydration_rejects_a_dangling_dictionary_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault.json");
        let target = dir.path().join("missing-vault.json");
        std::os::unix::fs::symlink(&target, &path).unwrap();
        let dictionary = RepositoryDictionary::new(path.clone(), MemoryKeys::default());
        assert!(
            dictionary
                .hydrate_pair_readonly("{\"message\":\"content\"}\n", "")
                .is_err()
        );
        assert_eq!(std::fs::read_link(&path).unwrap(), target);
        assert!(!target.exists());
        assert!(!dir.path().join("vault.lock").exists());
        assert!(dictionary.store.keys.0.lock().unwrap().is_empty());
    }

    #[test]
    fn saved_hydration_preserves_provenance_and_recomputes_content_hashes() {
        use crate::domain::{storage, transcript};
        let dir = tempfile::tempdir().unwrap();
        let dictionary =
            RepositoryDictionary::new(dir.path().join("vault.json"), MemoryKeys::default());
        let secret = "synthetic hydration marker";
        let matcher = Matcher::for_test(&[("sec_global", secret)]);
        let raw = format!("{}\n", serde_json::json!({"message":secret}));
        let protected = dictionary.protect_jsonl(&raw, &matcher).unwrap();
        let claim = format!("agit-{}", "a".repeat(40));
        let saved = transcript::wrap_lines(&protected.text, "hermes", &claim);
        let hydrated = dictionary.hydrate_envelopes(&saved).unwrap();
        let envelope = storage::parse_envelope_line(&hydrated.text).unwrap();
        assert_eq!(envelope.source, "hermes");
        assert_eq!(envelope.session_id, claim);
        assert_eq!(envelope.content["message"], secret);
        assert_ne!(
            envelope.object_hash,
            storage::parse_envelope_line(&saved).unwrap().object_hash
        );
        assert_eq!(hydrated.replacements, 1);
    }

    #[test]
    fn semantic_json_roundtrip_handles_quotes_slashes_newlines_and_unicode() {
        let dir = tempfile::tempdir().unwrap();
        // A CJK fixture: the multi-byte scalars are the `unicode` half of this
        // round trip, and an ASCII secret leaves that half unexercised.
        let secret = "口令\"with\\slash\nand newline";
        let global = Matcher::for_test(&[("sec_global", secret)]);
        let dictionary = RepositoryDictionary::new(
            dir.path().join("dictionary/vault.json"),
            MemoryKeys::default(),
        );
        let input = serde_json::to_string(&serde_json::json!({
            "message": format!("before {secret} after")
        }))
        .unwrap()
            + "\n";

        let protected = dictionary.protect_jsonl(&input, &global).unwrap();
        assert_eq!(protected.replacements, 1);
        assert_eq!(protected.new_records, 1);
        assert!(!protected.text.contains(secret));
        assert!(protected.text.contains(TOKEN_PREFIX));

        let hydrated = dictionary.hydrate_jsonl(&protected.text).unwrap();
        assert_eq!(hydrated.replacements, 1);
        assert_eq!(hydrated.unresolved, 0);
        let value: Value = serde_json::from_str(hydrated.text.trim()).unwrap();
        assert_eq!(value["message"], format!("before {secret} after"));

        let vault = std::fs::read(dictionary.store.path.clone()).unwrap();
        assert!(!vault.windows(secret.len()).any(|w| w == secret.as_bytes()));
    }

    #[test]
    fn same_repository_reuses_a_key_and_unknown_tokens_survive() {
        let dir = tempfile::tempdir().unwrap();
        let global = Matcher::for_test(&[("sec_global", "blue horse battery")]);
        let dictionary = RepositoryDictionary::new(
            dir.path().join("dictionary/vault.json"),
            MemoryKeys::default(),
        );
        let protected = dictionary
            .protect_jsonl(
                "{\"a\":\"blue horse battery\",\"b\":\"blue horse battery\"}\n",
                &global,
            )
            .unwrap();
        assert_eq!(protected.new_records, 1);
        let tokens: Vec<_> = token_segments(&protected.text)
            .map(|(_, _, token)| token.to_string())
            .collect();
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0], tokens[1]);

        let foreign = "{{AGIT_SECRET_V1:00000000-0000-0000-0000-000000000000:sec_00000000000000000000000000000000}}";
        let hydrated = dictionary
            .hydrate_jsonl(&format!(
                "{{\"known\":\"{}\",\"foreign\":\"{foreign}\"}}\n",
                tokens[0]
            ))
            .unwrap();
        assert!(hydrated.text.contains("blue horse battery"));
        assert!(hydrated.text.contains(foreign));
        assert_eq!(hydrated.unresolved, 1);
    }

    #[test]
    fn dense_matches_are_streamed_and_existing_tokens_are_opaque() {
        let dir = tempfile::tempdir().unwrap();
        let global = Matcher::for_test(&[("sec_short", "AGIT")]);
        let dictionary = RepositoryDictionary::new(
            dir.path().join("dictionary/vault.json"),
            MemoryKeys::default(),
        );
        let input = format!("{{\"text\":\"{}\"}}\n", "AGIT".repeat(100_000));
        let first = dictionary.protect_jsonl(&input, &global).unwrap();
        assert_eq!(first.replacements, 100_000);
        let second = dictionary.protect_jsonl(&first.text, &global).unwrap();
        assert_eq!(second.replacements, 0);
        assert_eq!(second.text, first.text);
    }

    #[test]
    fn protected_history_keeps_continuity_and_repository_keys_are_unlinkable() {
        let one = tempfile::tempdir().unwrap();
        let two = tempfile::tempdir().unwrap();
        let global = Matcher::for_test(&[("sec_global", "blue horse battery")]);
        let first = RepositoryDictionary::new(
            one.path().join("dictionary/vault.json"),
            MemoryKeys::default(),
        );
        let second = RepositoryDictionary::new(
            two.path().join("dictionary/vault.json"),
            MemoryKeys::default(),
        );
        let v1 = "{\"message\":\"blue horse battery\"}\n";
        let v2 = format!("{v1}{{\"message\":\"next\"}}\n");

        let first_v1 = first.protect_jsonl(v1, &global).unwrap();
        let first_v2 = first.protect_jsonl(&v2, &global).unwrap();
        let second_v1 = second.protect_jsonl(v1, &global).unwrap();
        assert_ne!(
            first_v1.text, second_v1.text,
            "different repositories must not expose equality through deterministic keys"
        );

        let stored = crate::domain::transcript::wrap_lines(&first_v1.text, "codex", "session");
        assert_eq!(
            crate::domain::transcript::continuity(&stored, &first_v2.text),
            crate::domain::transcript::Continuity::Append
        );
        let hydrated = first.hydrate_jsonl(&first_v2.text).unwrap();
        assert_eq!(hydrated.text, v2);
    }

    #[test]
    fn heuristic_candidate_defaults_to_protect_and_allow_keeps_hydration() {
        let dir = tempfile::tempdir().unwrap();
        let dictionary = RepositoryDictionary::new(
            dir.path().join("dictionary/vault.json"),
            MemoryKeys::default(),
        );
        let secret = "ghp_R7kQ2mXv9LpZ4tNc8WjF3bHy6sVd1aGe5uKr";
        let input = format!(r#"{{"message":"before {secret} after"}}"#) + "\n";

        let first = dictionary.protect_jsonl(&input, &Matcher::empty()).unwrap();
        assert_eq!(first.new_heuristic_records, 1);
        assert_eq!(first.new_records, 1);
        assert!(!first.text.contains(secret));
        let records = dictionary.review().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].origins, vec!["heuristic"]);
        assert!(records[0].effective_protect);

        let allowed = dictionary.allow(&records[0].id).unwrap();
        assert_eq!(
            allowed.heuristic_disposition,
            crate::domain::secret_filter::HeuristicDisposition::Allow
        );
        assert!(!allowed.effective_protect);
        let after_allow = dictionary.protect_jsonl(&input, &Matcher::empty()).unwrap();
        assert_eq!(after_allow.replacements, 0);
        assert!(after_allow.text.contains(secret));

        let hydrated = dictionary.hydrate_jsonl(&first.text).unwrap();
        assert!(hydrated.text.contains(secret));
        assert_eq!(hydrated.unresolved, 0);
    }

    #[test]
    fn preset_allowances_filter_entropy_and_registered_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let dictionary = RepositoryDictionary::new(
            dir.path().join("dictionary/vault.json"),
            MemoryKeys::default(),
        );
        let sample = "abcdefghijklmnopqrstuvwxyz";
        let global = Matcher::for_test(&[("sec_sample", sample)]);
        let input = serde_json::json!({"token": sample, "text": sample}).to_string();
        assert!(
            crate::domain::secrets::scan_text_registered_with(&input, &HashSet::new(), &global)
                .is_empty()
        );
        let protected = dictionary.protect_jsonl(&input, &global).unwrap();
        assert_eq!(protected.replacements, 0);
        assert_eq!(protected.new_records, 0);
        assert!(dictionary.review().unwrap().is_empty());
        let blocked = dictionary
            .block_add("sample", Zeroizing::new(sample.into()), false)
            .unwrap();
        assert!(!blocked.effective_protect);
        assert!(!dictionary.review().unwrap()[0].effective_protect);
        assert_eq!(
            dictionary
                .protect_jsonl(&input, &global)
                .unwrap()
                .replacements,
            0
        );
    }

    #[test]
    fn boolean_substrings_do_not_waive_public_scan_or_reversible_protection() {
        let dir = tempfile::tempdir().unwrap();
        let dictionary = RepositoryDictionary::new(
            dir.path().join("dictionary/vault.json"),
            MemoryKeys::default(),
        );
        let random = "R7kQ2mXv9LpZ4tNc8WjF3bHy6sVd1aGe5uKr";
        let mut vendor = format!("ghp_{random}");
        vendor.replace_range(16..21, "false");
        let cases = [
            vendor,
            format!("true{random}"),
            format!("{random}false"),
            format!("{random}null"),
        ];
        for secret in cases {
            let input = serde_json::to_string(&serde_json::json!({
                "text": format!("https://reader:{secret}@example.test")
            }))
            .unwrap();
            assert!(!crate::domain::secrets::scan_text(&input, &HashSet::new()).is_empty());
            let protected = dictionary.protect_jsonl(&input, &Matcher::empty()).unwrap();
            assert!(!protected.text.contains(&secret));
            assert!(protected.text.contains(TOKEN_PREFIX));
            assert_eq!(
                dictionary.hydrate_jsonl(&protected.text).unwrap().text,
                input
            );
        }
    }

    #[test]
    fn complete_and_truncated_pem_regions_roundtrip_without_exposing_body_material() {
        let dir = tempfile::tempdir().unwrap();
        let dictionary = RepositoryDictionary::new(
            dir.path().join("dictionary/vault.json"),
            MemoryKeys::default(),
        );
        let body = "ghp_R7kQ2mXv9LpZ4tNc8WjF3bHy6sVd1aGe5uKr\nprivate body material";
        for footer in [
            "",
            "\n-----END RSA PRIVATE KEY-----",
            "\n-----END PUBLIC KEY-----",
        ] {
            let pem = format!("-----BEGIN RSA PRIVATE KEY-----\n{body}{footer}");
            let input = serde_json::to_string(&serde_json::json!({"text": pem})).unwrap();
            let protected = dictionary.protect_jsonl(&input, &Matcher::empty()).unwrap();
            assert_eq!(protected.intact, 0);
            assert!(!protected.text.contains("private body material"));
            assert!(!protected.text.contains("ghp_"));
            assert!(crate::domain::secrets::scan_text(&protected.text, &HashSet::new()).is_empty());
            assert_eq!(
                dictionary.hydrate_jsonl(&protected.text).unwrap().text,
                input
            );
            let repeated = dictionary.protect_jsonl(&input, &Matcher::empty()).unwrap();
            assert_eq!(repeated.new_records, 0);
            assert_eq!(repeated.text, protected.text);
            let (scrubbed, _) = crate::domain::secrets::scrub(&pem);
            assert!(!scrubbed.contains("private body material"));
            assert!(!scrubbed.contains("BEGIN"));
            let (scrubbed_json, _) = crate::domain::secrets::scrub(&input);
            assert!(serde_json::from_str::<Value>(&scrubbed_json).is_ok());
            assert!(!scrubbed_json.contains("private body material"));

            let plain = dictionary.protect_jsonl(&pem, &Matcher::empty()).unwrap();
            assert!(!plain.text.contains("private body material"));
            assert_eq!(dictionary.hydrate_jsonl(&plain.text).unwrap().text, pem);
            let mixed = format!("{pem}\n{{\"text\":\"ordinary prose\"}}\n");
            let protected = dictionary.protect_jsonl(&mixed, &Matcher::empty()).unwrap();
            assert!(
                protected
                    .text
                    .ends_with("\n{\"text\":\"ordinary prose\"}\n")
            );
            assert_eq!(
                dictionary.hydrate_jsonl(&protected.text).unwrap().text,
                mixed
            );
        }
    }

    #[test]
    fn overlapping_registered_prefix_projects_the_complete_pem_region() {
        let dir = tempfile::tempdir().unwrap();
        let dictionary = RepositoryDictionary::new(
            dir.path().join("dictionary/vault.json"),
            MemoryKeys::default(),
        );
        let pem = "prefix\n-----BEGIN RSA PRIVATE KEY-----\nprivate body material\n-----END RSA PRIVATE KEY-----";
        let global = Matcher::for_test(&[(
            "registered-prefix",
            "prefix\n-----BEGIN RSA PRIVATE KEY-----",
        )]);
        let text = dictionary.protect_text(pem, &global).unwrap();
        assert!(!text.text.contains("private body material"));
        assert!(crate::domain::secrets::scan_text(&text.text, &HashSet::new()).is_empty());
        assert_eq!(dictionary.hydrate_text(&text.text).unwrap().text, pem);

        let json = serde_json::to_string(&serde_json::json!({"text": pem})).unwrap();
        let protected = dictionary.protect_jsonl(&json, &global).unwrap();
        assert!(!protected.text.contains("private body material"));
        assert!(crate::domain::secrets::scan_text(&protected.text, &HashSet::new()).is_empty());
        assert_eq!(
            dictionary.hydrate_jsonl(&protected.text).unwrap().text,
            json
        );
    }

    #[test]
    fn an_oversized_merged_region_stays_intact_without_an_oversized_record() {
        let dir = tempfile::tempdir().unwrap();
        let dictionary = RepositoryDictionary::new(
            dir.path().join("dictionary/vault.json"),
            MemoryKeys::default(),
        );
        let input = "ab".repeat(MAX_REPOSITORY_SECRET_BYTES / 2 + 1);
        let global = Matcher::for_test(&[("short", "abab")]);

        let protected = dictionary.protect_text(&input, &global).unwrap();

        assert_eq!(protected.text, input);
        assert_eq!(protected.replacements, 0);
        assert_eq!(protected.intact, 1);
        let records = dictionary.review().unwrap();
        assert_eq!(records.len(), 1);
        assert!(dictionary.hydrate_text(&input).is_ok());
    }

    #[test]
    fn overlapping_global_records_keep_explicit_protection_after_the_rules_change() {
        let dir = tempfile::tempdir().unwrap();
        let dictionary = RepositoryDictionary::new(
            dir.path().join("dictionary/vault.json"),
            MemoryKeys::default(),
        );
        let global = Matcher::for_test(&[("left", "abcdefgh"), ("right", "efghijkl")]);

        let protected = dictionary.protect_text("abcdefghijkl", &global).unwrap();

        assert_eq!(protected.replacements, 1);
        assert_eq!(dictionary.review().unwrap().len(), 3);
        let after_rules_change = dictionary
            .protect_text("abcdefgh", &Matcher::empty())
            .unwrap();
        assert_eq!(after_rules_change.replacements, 1);
        assert!(!after_rules_change.text.contains("abcdefgh"));
    }

    #[test]
    fn overlapping_match_collection_has_a_bounded_resource_limit() {
        let dir = tempfile::tempdir().unwrap();
        let dictionary = RepositoryDictionary::new(
            dir.path().join("dictionary/vault.json"),
            MemoryKeys::default(),
        );
        let patterns: Vec<String> = (1..=125).map(|length| "ab".repeat(length)).collect();
        let ids: Vec<String> = (0..patterns.len()).map(|index| index.to_string()).collect();
        let pattern_refs: Vec<(&str, &str)> = patterns
            .iter()
            .enumerate()
            .map(|(index, pattern)| (ids[index].as_str(), pattern.as_str()))
            .collect();
        let global = Matcher::for_test(&pattern_refs);

        let error = dictionary
            .protect_text(&"ab".repeat(MAX_REPOSITORY_SECRET_BYTES / 2), &global)
            .expect_err("the overlap budget must reject a dense match set");

        assert!(
            error
                .to_string()
                .contains("overlapping secret matches exceed the bounded protection limit")
        );
        assert!(dictionary.review().unwrap().is_empty());
    }

    #[test]
    fn heuristic_forward_projection_is_retry_safe_and_content_classified() {
        let dir = tempfile::tempdir().unwrap();
        let dictionary = RepositoryDictionary::new(
            dir.path().join("dictionary/vault.json"),
            MemoryKeys::default(),
        );
        let secret = "ghp_R7kQ2mXv9LpZ4tNc8WjF3bHy6sVd1aGe5uKr";
        let input = format!(r#"{{"message":"before {secret} after"}}"#) + "\n";
        let committed = crate::domain::transcript::wrap_lines(&input, "codex", "session");

        let first = dictionary.protect_jsonl(&input, &Matcher::empty()).unwrap();
        assert_eq!(first.new_heuristic_records, 1);
        let retry = dictionary.protect_jsonl(&input, &Matcher::empty()).unwrap();
        assert_eq!(retry.new_records, 0, "the failed attempt already saved it");
        assert_eq!(retry.text, first.text);
        assert_eq!(
            crate::domain::transcript::continuity(&committed, &retry.text),
            crate::domain::transcript::Continuity::Diverged
        );

        let registered = dictionary.protect_registered_jsonl(&input).unwrap();
        assert_eq!(registered.text, input);
        assert_ne!(
            crate::domain::transcript::continuity(&committed, &registered.text),
            crate::domain::transcript::Continuity::Diverged,
            "a retry must still identify the settled-prefix difference as heuristic-only"
        );

        let promoted = Matcher::for_test(&[("sec_global", secret)]);
        dictionary.protect_jsonl(&input, &promoted).unwrap();
        let registered = dictionary.protect_registered_jsonl(&input).unwrap();
        assert_eq!(
            crate::domain::transcript::continuity(&committed, &registered.text),
            crate::domain::transcript::Continuity::Diverged,
            "global/explicit registration must still refuse a settled-prefix rewrite"
        );
    }

    /// The settled prefix normally *does* contain placeholders.
    ///
    /// This models the two predicates `settle_bytes` computes when the full
    /// projection diverges from the committed LOG. Judging «same session?» by
    /// comparing the committed envelopes against raw plaintext answers «no» for
    /// every branch that ever projected anything — and «already claimed by
    /// another session» is a permanent, unarguable refusal. The comparison has
    /// to happen on hydrated content, and «would a registered rule rewrite the
    /// settled prefix?» has to be asked of the prefix itself, where existing
    /// placeholders are opaque.
    #[test]
    fn a_settled_prefix_that_already_holds_a_placeholder_still_classifies() {
        use crate::domain::transcript::{Continuity, continuity, continuity_of_content};

        let dir = tempfile::tempdir().unwrap();
        let dictionary = RepositoryDictionary::new(
            dir.path().join("dictionary/vault.json"),
            MemoryKeys::default(),
        );
        let registered = "blue horse battery";
        let heuristic = "ghp_R7kQ2mXv9LpZ4tNc8WjF3bHy6sVd1aGe5uKr";
        let global = Matcher::for_test(&[("sec_memorable", registered)]);

        // Turn 1 settles a line carrying the registered value, so the committed
        // prefix holds a placeholder from here on.
        let line1 = format!(r#"{{"message":"deploy with {registered}"}}"#) + "\n";
        let settled1 = dictionary.protect_jsonl(&line1, &global).unwrap();
        assert_eq!(settled1.replacements, 1);

        // Turn 2 adds a heuristic value, which the user then allows, so it
        // settles in the clear next to the earlier placeholder.
        let line2 = format!(r#"{{"message":"token {heuristic}"}}"#) + "\n";
        let live2 = line1.clone() + &line2;
        dictionary.protect_jsonl(&live2, &global).unwrap();
        let heuristic_id = dictionary
            .review()
            .unwrap()
            .into_iter()
            .find(|record| record.origins.iter().any(|origin| origin == "heuristic"))
            .expect("the heuristic candidate earns a record")
            .id;
        dictionary.allow(&heuristic_id).unwrap();
        let settled2 = dictionary.protect_jsonl(&live2, &global).unwrap();
        assert!(settled2.text.contains(heuristic), "allow stops projection");
        let committed =
            crate::domain::transcript::wrap_lines(&settled2.text, "codex", "session-under-test");

        // The user changes their mind, and turn 3 appends another line.
        dictionary.unallow(&heuristic_id).unwrap();
        let live3 = live2.clone() + r#"{"message":"turn three"}"# + "\n";
        let protected_full = dictionary.protect_jsonl(&live3, &global).unwrap();
        assert_eq!(
            continuity(&committed, &protected_full.text),
            Continuity::Diverged,
            "precondition: the re-protected snapshot must differ from what is settled"
        );

        // 1. Same session? Decided on hydrated content — on both sides.
        let hydrated = dictionary.hydrate_jsonl(&committed).unwrap();
        assert_ne!(
            continuity_of_content(
                &hydrated.text,
                &dictionary.hydrate_jsonl(&live3).unwrap().text
            ),
            Continuity::Diverged,
            "hydrating the settled prefix must reveal it as this session's own history"
        );
        assert_eq!(
            continuity(&committed, &live3),
            Continuity::Diverged,
            "and the old raw-plaintext comparison is exactly what got this wrong"
        );

        // 2. Would a registered rule rewrite the settled prefix? Asked of the
        //    settled content, where the turn-1 placeholder is opaque and
        //    AgentGit's own envelope identities are out of the matching surface.
        let settled_content = crate::domain::transcript::unwrap_strict(&committed).unwrap();
        assert_eq!(
            dictionary
                .protect_registered_jsonl(&settled_content)
                .unwrap()
                .replacements,
            0,
            "no registered value is sitting in the clear in the settled prefix"
        );

        // A genuinely later registration must still be refused.
        let promoted = Matcher::for_test(&[("sec_memorable", registered), ("sec_late", heuristic)]);
        dictionary.protect_jsonl(&live3, &promoted).unwrap();
        assert!(
            dictionary
                .protect_registered_jsonl(&settled_content)
                .unwrap()
                .replacements
                > 0,
            "registering a value the settled prefix holds in the clear is a rewrite"
        );
    }

    /// A transcript may legitimately contain this repository's own placeholder.
    ///
    /// An agent that runs `agit show`, or reads back `session/log.jsonl`,
    /// records a real `{{AGIT_SECRET_V1:…}}` token as ordinary content;
    /// projection keeps it opaque, so it settles verbatim. Hydrating only the
    /// committed side then expands it while the live side keeps the token, and
    /// the session reads as a foreign one — the same wrong permanent refusal,
    /// from the opposite direction.
    #[test]
    fn a_placeholder_echoed_by_the_transcript_is_not_a_foreign_session() {
        use crate::domain::transcript::{Continuity, continuity_of_content};

        let dir = tempfile::tempdir().unwrap();
        let dictionary = RepositoryDictionary::new(
            dir.path().join("dictionary/vault.json"),
            MemoryKeys::default(),
        );
        let secret = "blue horse battery";
        let global = Matcher::for_test(&[("sec_memorable", secret)]);

        // Turn 1 projects the value and yields a real token for this vault.
        let line1 = format!(r#"{{"message":"deploy with {secret}"}}"#) + "\n";
        let settled1 = dictionary.protect_jsonl(&line1, &global).unwrap();
        let token = settled1
            .text
            .split_once(TOKEN_PREFIX)
            .and_then(|(_, rest)| rest.split_once(TOKEN_SUFFIX))
            .map(|(body, _)| format!("{TOKEN_PREFIX}{body}{TOKEN_SUFFIX}"))
            .expect("turn 1 must have produced a placeholder");

        // Turn 2: the agent reads its own settled log back, so the token itself
        // becomes content.
        let line2 = serde_json::to_string(&serde_json::json!({
            "message": format!("the log says {token}")
        }))
        .unwrap()
            + "\n";
        let live2 = line1.clone() + &line2;
        let settled2 = dictionary.protect_jsonl(&live2, &global).unwrap();
        assert!(
            settled2.text.contains(&token),
            "an already-valid token stays opaque through projection"
        );
        let committed =
            crate::domain::transcript::wrap_lines(&settled2.text, "codex", "session-under-test");

        // Turn 3 appends, so the outer check diverges and classification runs.
        let live3 = live2.clone() + r#"{"message":"turn three"}"# + "\n";

        assert_eq!(
            continuity_of_content(&dictionary.hydrate_jsonl(&committed).unwrap().text, &live3),
            Continuity::Diverged,
            "precondition: hydrating one side expands the echoed token and diverges"
        );
        assert_ne!(
            continuity_of_content(
                &dictionary.hydrate_jsonl(&committed).unwrap().text,
                &dictionary.hydrate_jsonl(&live3).unwrap().text,
            ),
            Continuity::Diverged,
            "hydrating both sides treats the echoed token identically"
        );
    }

    /// A settlement is never refused for the number of new heuristic values it carries: a long
    /// unsettled session full of identifiers must still settle in one pass, and a later pass
    /// registers only what the dictionary does not hold yet. An implementation that batches
    /// candidates against a fixed budget would refuse the first call or double-count the second.
    #[test]
    fn heuristic_records_are_unbounded_per_settlement_and_counted_once() {
        let dir = tempfile::tempdir().unwrap();
        let dictionary = RepositoryDictionary::new(
            dir.path().join("dictionary/vault.json"),
            MemoryKeys::default(),
        );
        let batch = 1_500;
        let tokens: Vec<_> = (0..=batch)
            .map(|index| format!("ghp_R7kQ2mXv9LpZ4tNc8WjF3bHy6s{index:010X}"))
            .collect();
        let first = serde_json::to_string(&serde_json::json!({ "tokens": &tokens[..batch] }))
            .unwrap()
            + "\n";
        let first = dictionary.protect_jsonl(&first, &Matcher::empty()).unwrap();
        assert_eq!(first.new_heuristic_records, batch);
        assert_eq!(first.replacements, batch);

        let cumulative =
            serde_json::to_string(&serde_json::json!({ "tokens": tokens })).unwrap() + "\n";
        let next = dictionary
            .protect_jsonl(&cumulative, &Matcher::empty())
            .unwrap();
        assert_eq!(next.new_heuristic_records, 1);
        assert_eq!(dictionary.review().unwrap().len(), batch + 1);
    }

    #[test]
    fn long_private_key_is_reversible_and_extreme_matches_remain_visible_to_push_gate() {
        let dir = tempfile::tempdir().unwrap();
        let dictionary = RepositoryDictionary::new(
            dir.path().join("dictionary/vault.json"),
            MemoryKeys::default(),
        );
        let pem = format!(
            "-----BEGIN RSA PRIVATE KEY-----\n{}\n-----END RSA PRIVATE KEY-----",
            "A".repeat(4 * 1024)
        );
        assert!(pem.len() > 2048);
        let input = serde_json::to_string(&serde_json::json!({ "message": pem })).unwrap() + "\n";
        let protected = dictionary.protect_jsonl(&input, &Matcher::empty()).unwrap();
        assert!(protected.replacements > 0);
        assert!(!protected.text.contains("BEGIN RSA PRIVATE KEY"));
        assert_eq!(
            dictionary.hydrate_jsonl(&protected.text).unwrap().text,
            input
        );

        let other_dir = tempfile::tempdir().unwrap();
        let fallback = RepositoryDictionary::new(
            other_dir.path().join("dictionary/vault.json"),
            MemoryKeys::default(),
        );
        let too_large = format!(
            "-----BEGIN RSA PRIVATE KEY-----\n{}\n-----END RSA PRIVATE KEY-----",
            "A".repeat(MAX_REPOSITORY_SECRET_BYTES + 1)
        );
        let input =
            serde_json::to_string(&serde_json::json!({ "message": too_large })).unwrap() + "\n";
        let protected = fallback.protect_jsonl(&input, &Matcher::empty()).unwrap();
        assert_eq!(protected.text, input);
        assert_eq!(protected.replacements, 0);
        assert!(
            crate::domain::secrets::scan_text(&input, &std::collections::HashSet::new())
                .iter()
                .any(|hit| hit.rule == "private-key"),
            "the unchanged full match must remain visible to the push scanner"
        );
        assert!(fallback.review().unwrap().is_empty());
    }

    /// An unstorable finding is a fact about that finding, not about the input.
    ///
    /// The whole point of the dictionary is that a value protected in one
    /// settlement stays protected in the next. Returning the accumulated
    /// transcript verbatim because a later turn happened to contain an
    /// oversized PEM would un-project everything earlier turns already
    /// protected — and write those values into the next Git object in the
    /// clear, which is the one outcome this feature exists to prevent.
    #[test]
    fn an_oversized_finding_only_keeps_its_own_span_in_the_clear() {
        let dir = tempfile::tempdir().unwrap();
        let dictionary = RepositoryDictionary::new(
            dir.path().join("dictionary/vault.json"),
            MemoryKeys::default(),
        );
        let heuristic = "ghp_R7kQ2mXv9LpZ4tNc8WjF3bHy6sVd1aGe5uKr";
        let registered = "blue horse battery";
        let global = Matcher::for_test(&[("sec_memorable", registered)]);

        // Turn 1 settles cleanly and gives both values a repository key.
        let first = format!(r#"{{"message":"{heuristic} / {registered}"}}"#) + "\n";
        let settled = dictionary.protect_jsonl(&first, &global).unwrap();
        assert_eq!(settled.intact, 0);
        assert!(!settled.text.contains(heuristic));
        assert!(!settled.text.contains(registered));

        // Turn 2 appends a PEM too large for a reversible record.
        let too_large = format!(
            "-----BEGIN RSA PRIVATE KEY-----\n{}\n-----END RSA PRIVATE KEY-----",
            "A".repeat(MAX_REPOSITORY_SECRET_BYTES + 1)
        );
        let second = first.clone()
            + &(serde_json::to_string(&serde_json::json!({ "message": too_large })).unwrap()
                + "\n");
        let protected = dictionary.protect_jsonl(&second, &global).unwrap();
        // The span survives in its JSONL-encoded form; comparing the decoded
        // value against wire bytes would fail on the newline escape alone.
        let wire = serde_json::to_string(&too_large).unwrap();
        let too_large_wire = &wire[1..wire.len() - 1];

        assert_eq!(
            protected.intact, 1,
            "the settlement must report the finding it could not reverse"
        );
        assert!(
            !protected.text.contains(heuristic),
            "an unstorable finding elsewhere in the input must not un-protect an earlier heuristic value"
        );
        assert!(
            !protected.text.contains(registered),
            "an unstorable finding elsewhere in the input must not un-protect a registered value"
        );
        assert!(
            protected.text.contains(too_large_wire),
            "the oversized match itself stays byte-for-byte so the push gate still rejects it"
        );
        assert!(
            crate::domain::secrets::scan_text(&protected.text, &HashSet::new())
                .iter()
                .any(|hit| hit.rule == "private-key"),
            "the unchanged full match must remain visible to the push scanner"
        );
        // The oversized value earns no record, so it cannot reach the dictionary
        // by another route and defeat `MAX_REPOSITORY_SECRET_BYTES`.
        assert_eq!(dictionary.review().unwrap().len(), 2);
        assert_eq!(
            dictionary.hydrate_jsonl(&protected.text).unwrap().text,
            second,
            "everything that was projected must still round-trip"
        );
    }

    /// The warning survives a settlement that has nothing to project.
    ///
    /// When the one finding is over-capacity, there is no pattern to build a
    /// matcher from — no records yet, no global rules, and the candidate was
    /// refused for its size — so projection has nothing to do and used to
    /// return before the finding was ever counted. The bytes were right and the
    /// report was silent, which is the worst combination available here: the
    /// value is in the clear, `agit push` is going to reject it, and commit is
    /// where the user should hear that.
    #[test]
    fn an_unprotectable_finding_is_reported_even_with_no_patterns_at_all() {
        let dir = tempfile::tempdir().unwrap();
        let dictionary = RepositoryDictionary::new(
            dir.path().join("dictionary/vault.json"),
            MemoryKeys::default(),
        );
        // An unbounded rule, so the whole run is one match with no shorter rule
        // overlapping inside it — nothing else can supply a pattern. The body
        // is generated rather than repeated because the rule carries an entropy
        // floor of 4, which a repeating string does not clear.
        let alphabet: Vec<u8> = (b'a'..=b'z')
            .chain(b'A'..=b'Z')
            .chain(b'0'..=b'9')
            .chain(*b"+/")
            .collect();
        let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
        let body: String = (0..70_000)
            .map(|_| {
                seed = seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                alphabet[(seed >> 33) as usize % alphabet.len()] as char
            })
            .collect();
        let token = format!("ops_eyJ{body}");
        assert!(token.len() > MAX_REPOSITORY_SECRET_BYTES);
        let input = serde_json::to_string(&serde_json::json!({ "message": token })).unwrap() + "\n";

        let protected = dictionary.protect_jsonl(&input, &Matcher::empty()).unwrap();

        assert_eq!(
            protected.intact, 1,
            "the finding must be reported even though nothing was projected"
        );
        assert_eq!(protected.replacements, 0);
        assert_eq!(
            protected.text, input,
            "and the bytes stay exactly as they were"
        );
        assert!(
            dictionary.review().unwrap().is_empty(),
            "it is over capacity, so it earns no record"
        );
        assert!(
            crate::domain::secrets::scan_text(&protected.text, &HashSet::new())
                .iter()
                .any(|hit| hit.rule == "1password-service-account-token"),
            "the push gate still sees it"
        );
    }

    /// Many distinct over-capacity findings must not accumulate.
    ///
    /// The candidate collector charges its 1,024-record budget only for values
    /// it accepts, so refusing the over-capacity ones excludes them from the
    /// very limit that would have capped them. Keeping their literals would
    /// therefore grow without any bound at all — and then clone them again into
    /// a pattern automaton, whose size follows total pattern length. This test
    /// says the settlement stays correct with many of them; the mechanism that
    /// makes it affordable is that nothing here is retained by value.
    #[test]
    fn many_oversized_findings_do_not_accumulate() {
        let dir = tempfile::tempdir().unwrap();
        let dictionary = RepositoryDictionary::new(
            dir.path().join("dictionary/vault.json"),
            MemoryKeys::default(),
        );
        let heuristic = "ghp_R7kQ2mXv9LpZ4tNc8WjF3bHy6sVd1aGe5uKr";
        // Distinct bodies, so nothing can be deduplicated away.
        let keys: Vec<String> = (0..8)
            .map(|i| {
                format!(
                    "-----BEGIN RSA PRIVATE KEY-----\n{}{i:04}\n-----END RSA PRIVATE KEY-----",
                    "A".repeat(MAX_REPOSITORY_SECRET_BYTES + 1)
                )
            })
            .collect();
        let mut input = format!(r#"{{"message":"token {heuristic}"}}"#) + "\n";
        for key in &keys {
            input +=
                &(serde_json::to_string(&serde_json::json!({ "message": key })).unwrap() + "\n");
        }

        let protected = dictionary.protect_jsonl(&input, &Matcher::empty()).unwrap();

        assert_eq!(protected.intact, keys.len());
        assert!(
            !protected.text.contains(heuristic),
            "the ordinary finding is still projected alongside all of them"
        );
        for key in &keys {
            let wire = serde_json::to_string(key).unwrap();
            assert!(
                protected.text.contains(&wire[1..wire.len() - 1]),
                "every over-capacity span stays byte-for-byte for the push gate"
            );
        }
        assert_eq!(
            dictionary.review().unwrap().len(),
            1,
            "none of them may earn a record"
        );
    }

    #[test]
    fn allow_wins_over_global_and_explicit_blocks_and_unallow_restores_them() {
        let dir = tempfile::tempdir().unwrap();
        let dictionary = RepositoryDictionary::new(
            dir.path().join("dictionary/vault.json"),
            MemoryKeys::default(),
        );
        let secret = "blue horse battery";
        let global = Matcher::for_test(&[("sec_global", secret)]);
        let input = format!(r#"{{"message":"{secret}"}}"#) + "\n";
        let protected = dictionary.protect_jsonl(&input, &global).unwrap();
        assert_eq!(dictionary.review().unwrap()[0].origins, vec!["global"]);
        let id = dictionary.review().unwrap()[0].id.clone();
        dictionary.allow(&id).unwrap();

        let blocked = dictionary
            .block_add("must-protect", Zeroizing::new(secret.to_string()), false)
            .unwrap();
        assert_eq!(blocked.id, id);
        assert!(blocked.explicit_block);
        assert!(!blocked.effective_protect);
        assert!(dictionary.active_matcher().unwrap().find(secret).is_empty());
        assert_eq!(
            dictionary.protect_jsonl(&input, &global).unwrap().text,
            input
        );
        let scan = global
            .merged(&dictionary.active_matcher().unwrap())
            .unwrap();
        assert!(
            crate::domain::secrets::scan_text_registered_with(&input, &HashSet::new(), &scan)
                .is_empty()
        );

        assert_eq!(
            dictionary.hydrate_jsonl(&protected.text).unwrap().text,
            input
        );
        dictionary.unallow(&id).unwrap();
        assert_eq!(dictionary.active_matcher().unwrap().find(secret).len(), 1);
        assert_eq!(
            dictionary
                .protect_jsonl(&input, &global)
                .unwrap()
                .replacements,
            1
        );
        dictionary.allow(&id).unwrap();

        let unblocked = dictionary.block_remove(&id).unwrap();
        assert!(!unblocked.explicit_block);
        assert!(!unblocked.effective_protect);
        assert!(dictionary.active_matcher().unwrap().find(secret).is_empty());
    }

    #[test]
    fn unverified_identity_field_names_do_not_waive_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let dictionary = RepositoryDictionary::new(
            dir.path().join("dictionary/vault.json"),
            MemoryKeys::default(),
        );
        let input = r#"{"session_id":"ghp_R7kQ2mXv9LpZ4tNc8WjF3bHy6sVd1aGe5uKr"}"#;
        let protected = dictionary.protect_jsonl(input, &Matcher::empty()).unwrap();
        assert_eq!(protected.replacements, 1);
        assert_eq!(protected.new_records, 1);
        assert!(!dictionary.review().unwrap().is_empty());
    }

    #[test]
    fn bare_values_and_user_controlled_keys_are_reversible_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let dictionary = RepositoryDictionary::new(
            dir.path().join("dictionary/vault.json"),
            MemoryKeys::default(),
        );
        let alpha = "Qz7mXv9LpZ4tNc8WjF3bHy6sVd1aGe5uKr2dF";
        let hex = "4f2a9c1e7b3d8056af12cd34ef56ab78901234cd";
        let key = "Kx7mQv2Lp9Nc8Wj3Fh6sVd1aGe5uRz4tYp8";
        let contextual = "aB3dE6gH9jK2mN5p";
        let input = serde_json::to_string(&serde_json::json!({
            "signature": {"credential": alpha},
            "token": hex,
            "password": contextual,
            key: "ordinary value"
        }))
        .unwrap();
        let protected = dictionary.protect_jsonl(&input, &Matcher::empty()).unwrap();
        assert!(protected.new_heuristic_records >= 4);
        assert!(!protected.text.contains(alpha));
        assert!(!protected.text.contains(hex));
        assert!(!protected.text.contains(key));
        assert!(!protected.text.contains(contextual));
        assert_eq!(
            dictionary.hydrate_jsonl(&protected.text).unwrap().text,
            input
        );
    }

    #[test]
    fn native_identity_evidence_does_not_waive_equal_credential_values() {
        let dir = tempfile::tempdir().unwrap();
        let dictionary =
            RepositoryDictionary::new(dir.path().join("vault.json"), MemoryKeys::default());
        let native = "3f6b1c2a-8d40-4e7b-9a15-2c0de4f8b731";
        let input = serde_json::json!({"type":"assistant", "sessionId":native,
            "message":{"role":"assistant", "content":"public"}, "token":native})
        .to_string();
        let report = dictionary
            .protect_native_jsonl(&input, &Matcher::empty(), "claude-code", native)
            .unwrap();
        let protected: Value = serde_json::from_str(&report.text).unwrap();
        assert_eq!(protected["sessionId"], native);
        assert_ne!(protected["token"], native);
        assert_eq!(
            serde_json::from_str::<Value>(&dictionary.hydrate_jsonl(&report.text).unwrap().text)
                .unwrap(),
            serde_json::from_str::<Value>(&input).unwrap()
        );
        assert_eq!(
            dictionary
                .protect_native_jsonl(&input, &Matcher::empty(), "claude-code", native)
                .unwrap()
                .text,
            report.text
        );
        let explicit = Matcher::for_test(&[("explicit", native)]);
        assert!(
            !dictionary
                .protect_native_jsonl(&input, &explicit, "claude-code", native)
                .unwrap()
                .text
                .contains(native)
        );
        let untrusted = serde_json::json!({"sessionId":native, "signature":native}).to_string();
        assert!(
            !dictionary
                .protect_native_jsonl(&untrusted, &Matcher::empty(), "claude-code", native)
                .unwrap()
                .text
                .contains(native)
        );
    }

    #[test]
    fn independent_entropy_alphabets_roundtrip_as_literal_carriers() {
        let dir = tempfile::tempdir().unwrap();
        let dictionary =
            RepositoryDictionary::new(dir.path().join("vault.json"), MemoryKeys::default());
        for value in [
            "Qz7mXv9LpZ4tNc8WjF3bHy6sVd1aGe5uKr2dF",
            "qLmRtNxPzSwTyVaXbUdZeFhJkCgHoIpQsMrNvLtPwZxY",
            "4f2a9c1e7b3d8056af12cd34ef56ab78901234cd",
            "Qz7mXv9L+pZ4tNc8/WjF3bHy6sVd1aGe5uKr2dF==",
            "Qz7mXv9L-pZ4tNc8_WjF3bHy6sVd1aGe5uKr2dF",
            "qL!2rM@5tN#8xP$3zR%6wS^9yT&4uV*7aX",
        ] {
            let first = dictionary.protect_text(value, &Matcher::empty()).unwrap();
            assert!(
                !first.text.contains(value),
                "a supported alphabet must be protected"
            );
            assert_eq!(dictionary.hydrate_text(&first.text).unwrap().text, value);
            let retry = dictionary.protect_text(value, &Matcher::empty()).unwrap();
            assert_eq!(retry.new_records, 0);
            assert_eq!(retry.text, first.text);
        }
    }
}
