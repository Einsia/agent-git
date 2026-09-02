//! Repository-local reversible secret placeholders.
//!
//! The global vault says which literal values are secrets. This dictionary
//! gives every matching value a repository-scoped random handle and keeps the
//! reverse mapping beside the repository's Git metadata, never in its tree.

use super::{
    CURRENT_PROJECTION_VERSION, CURRENT_SCHEMA_VERSION, DecryptedRecord, KeyStore,
    MAX_REPOSITORY_SECRET_BYTES, Matcher, PlainRecord, RECORD_VERSION, RecordOrigin, SealedRecord,
    SelectedKeyStore, Unlocked, VaultStore, encode_padded, record_aad, seal, validate_name,
    validate_secret, write_vault,
};
use aho_corasick::{AhoCorasick, AhoCorasickBuilder, MatchKind};
use anyhow::{Context as _, bail};
use serde::Serialize;
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use zeroize::Zeroizing;

const DICTIONARY_RELATIVE_PATH: &str = "agit/secret-dictionary/vault.json";
const TOKEN_PREFIX: &str = "{{AGIT_SECRET_V1:";
const TOKEN_SUFFIX: &str = "}}";
const CANONICAL_TOKEN_LEN: usize = TOKEN_PREFIX.len() + 36 + 1 + 4 + 32 + TOKEN_SUFFIX.len();
const MAX_NEW_HEURISTIC_RECORDS: usize = 1_024;

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
pub struct RepositoryDictionary<K: KeyStore = SelectedKeyStore> {
    store: VaultStore<K>,
}

impl RepositoryDictionary<SelectedKeyStore> {
    /// Fails only on a keystore setting outside its domain; the key itself is touched lazily.
    pub fn open(repo_root: &Path) -> crate::Result<Self> {
        // One dictionary per repository: session-branch worktrees and the main checkout share it.
        Ok(Self::new(
            crate::domain::repo::common_git_dir(repo_root)
                .join(Path::new(DICTIONARY_RELATIVE_PATH)),
            SelectedKeyStore::from_config()?,
        ))
    }
}

impl<K: KeyStore> RepositoryDictionary<K> {
    pub fn new(path: PathBuf, keys: K) -> Self {
        Self {
            store: VaultStore::new(path, keys),
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
            // A finding larger than a reversible record cannot become a
            // placeholder, but that is a fact about *that* finding — it says
            // nothing about the registered secret three lines above it. Only
            // its own span stays in the clear; returning the whole accumulated
            // transcript instead would un-project everything a previous
            // settlement already protected and write those values into the next
            // Git object in the clear.
            //
            // Where those spans are is recomputed per string during protection,
            // from a bounded length test. Collecting the literals here instead
            // would have no budget to bound it: the candidate collector charges
            // its 1,024-record budget only for values it accepts, so refusing
            // the over-capacity ones excludes them from the very limit that
            // would have capped them.
            let candidates = {
                let existing: HashSet<&str> = records
                    .iter()
                    .map(|record| record.secret.as_str())
                    .collect();
                crate::domain::secrets::secret_candidates_jsonl(
                    text,
                    MAX_NEW_HEURISTIC_RECORDS,
                    |candidate| {
                        candidate.len() <= MAX_REPOSITORY_SECRET_BYTES
                            && !existing.contains(candidate)
                    },
                )
            };
            if candidates.truncated {
                bail!(
                    "more than {MAX_NEW_HEURISTIC_RECORDS} new heuristic secrets were found in one settlement; no repository dictionary update was written"
                );
            }
            let mut state = ProtectionState::from_records(
                &self.store,
                global,
                &candidates.values,
                ExistingRecordScope::All,
                unlocked,
                records,
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

    /// Protect arbitrary text (currently used for the generated commit subject).
    pub fn protect_text(&self, text: &str, global: &Matcher) -> crate::Result<ProtectionReport> {
        self.store.with_lock(|| {
            let mut state = ProtectionState::load(&self.store, global, &[])?;
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
            let mut out: Vec<_> = super::decrypt_records(&unlocked.file, &unlocked.dek)?
                .iter()
                .map(record_summary)
                .collect();
            out.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
            Ok(out)
        })
    }

    pub(crate) fn active_matcher(&self) -> crate::Result<Matcher> {
        self.store.with_lock(|| {
            if !self.store.path.exists() {
                return Ok(Matcher::empty());
            }
            let unlocked = self.store.unlock_existing()?;
            let generation = unlocked.file.generation;
            let records = super::decrypt_records(&unlocked.file, &unlocked.dek)?
                .into_iter()
                .filter(effective_protect)
                .collect();
            Matcher::build(generation, records)
        })
    }

    pub fn allow(&self, record_id: &str) -> crate::Result<RepositoryRecordSummary> {
        self.update_record(record_id, |record| {
            if !record.origins.contains(&RecordOrigin::Heuristic) {
                bail!("repository secret `{record_id}` was not discovered heuristically and cannot be allowlisted");
            }
            record.heuristic_disposition = super::HeuristicDisposition::Allow;
            Ok(())
        })
    }

    pub fn unallow(&self, record_id: &str) -> crate::Result<RepositoryRecordSummary> {
        self.update_record(record_id, |record| {
            if !record.origins.contains(&RecordOrigin::Heuristic) {
                bail!("repository secret `{record_id}` was not discovered heuristically");
            }
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
        validate_name(name)?;
        validate_secret(&secret, allow_short)?;
        self.store.with_lock(|| {
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
                return Ok(record_summary(record));
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
            let summary = record_summary(&record);
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
            Ok(record_summary(record))
        })
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
        Self::from_records(store, global, candidates, scope, unlocked, records)
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
                let allowed = spec.record_id.as_ref().is_some_and(|id| {
                    records.iter().any(|record| {
                        &record.id == id
                            && record.origins.contains(&RecordOrigin::Heuristic)
                            && record.heuristic_disposition == super::HeuristicDisposition::Allow
                            && !record.explicit_block
                            && !legacy_record(record)
                    })
                });
                if !allowed || spec.from_global {
                    spec.active = true;
                }
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
        for spec in specs.into_iter().filter(|spec| spec.active) {
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
                    .match_kind(MatchKind::LeftmostLongest)
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
                crate::domain::secrets::oversized_finding_spans(text, threshold)
            }
            _ => vec![],
        }
    }

    fn protect_segment(&mut self, text: &str, ac: &AhoCorasick) -> crate::Result<(String, usize)> {
        let mut out = String::with_capacity(text.len());
        let mut cursor = 0usize;
        let mut replacements = 0usize;
        for found in ac.find_iter(text.as_bytes()) {
            let pattern = found.pattern().as_usize();
            out.push_str(&text[cursor..found.start()]);
            let id = self.ensure_record(pattern, &text[found.start()..found.end()])?;
            let vault_id = &self
                .unlocked
                .as_ref()
                .expect("a matched pattern always initializes the dictionary")
                .file
                .vault_id;
            out.push_str(&token(vault_id, &id));
            cursor = found.end();
            replacements = replacements.saturating_add(1);
        }
        if replacements == 0 {
            return Ok((text.to_string(), 0));
        }
        out.push_str(&text[cursor..]);
        Ok((out, replacements))
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
    legacy_record(record)
        || record.explicit_block
        || (record.origins.contains(&RecordOrigin::Heuristic)
            && record.heuristic_disposition == super::HeuristicDisposition::Protect)
}

fn registered_protect(record: &DecryptedRecord) -> bool {
    effective_protect(record)
        && (legacy_record(record)
            || record.explicit_block
            || record.origins.contains(&RecordOrigin::Global)
            || record.origins.contains(&RecordOrigin::Explicit))
}

fn record_summary(record: &DecryptedRecord) -> RepositoryRecordSummary {
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
        effective_protect: effective_protect(record),
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

fn transform_jsonl(
    text: &str,
    mut transform: impl FnMut(&str) -> crate::Result<(String, usize)>,
) -> crate::Result<(String, usize)> {
    let mut out = String::with_capacity(text.len());
    let mut replacements = 0usize;
    for inclusive in text.split_inclusive('\n') {
        let (body, newline) = inclusive
            .strip_suffix('\n')
            .map(|s| (s.strip_suffix('\r').unwrap_or(s), true))
            .unwrap_or((inclusive, false));
        match serde_json::from_str::<Value>(body) {
            Ok(mut value) => {
                replacements =
                    replacements.saturating_add(transform_value(&mut value, &mut transform)?);
                out.push_str(&serde_json::to_string(&value)?);
            }
            Err(_) => {
                let (protected, count) = transform(body)?;
                out.push_str(&protected);
                replacements = replacements.saturating_add(count);
            }
        }
        if newline {
            out.push('\n');
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

pub(super) fn token_segments(text: &str) -> TokenSegments<'_> {
    TokenSegments { text, cursor: 0 }
}

/// Return the start of an opaque repository token that is complete and crosses
/// `cut`, or that is still a structurally valid prefix at the end of the
/// current stream buffer. The latter case is what keeps a chunk boundary from
/// exposing the token body to the registered-literal matcher.
pub(super) fn streaming_token_start(text: &str, cut: usize) -> Option<usize> {
    let mut earliest = token_segments(text)
        .take_while(|(start, _, _)| *start < cut)
        .find_map(|(start, end, _)| (end > cut).then_some(start));

    // Generated tokens have a fixed, bounded shape. Inspect only possible
    // starts close enough to the buffer end to remain an incomplete token, so
    // malformed input cannot make the stream buffer grow without bound.
    for (start, _) in text.rmatch_indices('{') {
        if text.len().saturating_sub(start) >= CANONICAL_TOKEN_LEN {
            break;
        }
        if start < cut && canonical_token_prefix(&text[start..]) {
            earliest = Some(earliest.map_or(start, |current| current.min(start)));
        }
    }
    earliest
}

fn canonical_token_prefix(fragment: &str) -> bool {
    if fragment.is_empty() || fragment.len() >= CANONICAL_TOKEN_LEN {
        return false;
    }
    fragment
        .bytes()
        .enumerate()
        .all(|(index, byte)| canonical_token_byte(index, byte))
}

fn canonical_token_byte(index: usize, byte: u8) -> bool {
    if index < TOKEN_PREFIX.len() {
        return TOKEN_PREFIX.as_bytes()[index] == byte;
    }

    let uuid_index = index - TOKEN_PREFIX.len();
    if uuid_index < 36 {
        return if matches!(uuid_index, 8 | 13 | 18 | 23) {
            byte == b'-'
        } else {
            byte.is_ascii_hexdigit()
        };
    }

    let after_uuid = uuid_index - 36;
    if after_uuid == 0 {
        return byte == b':';
    }
    if (1..5).contains(&after_uuid) {
        return b"sec_"[after_uuid - 1] == byte;
    }
    if (5..37).contains(&after_uuid) {
        return byte.is_ascii_hexdigit();
    }
    TOKEN_SUFFIX.as_bytes()[after_uuid - 37] == byte
}

pub(super) struct TokenSegments<'a> {
    text: &'a str,
    cursor: usize,
}

impl<'a> Iterator for TokenSegments<'a> {
    type Item = (usize, usize, &'a str);

    fn next(&mut self) -> Option<Self::Item> {
        while let Some(relative) = self.text[self.cursor..].find(TOKEN_PREFIX) {
            let start = self.cursor + relative;
            let content_start = start + TOKEN_PREFIX.len();
            let Some(close) = self.text[content_start..].find(TOKEN_SUFFIX) else {
                self.cursor = self.text.len();
                return None;
            };
            let end = content_start + close + TOKEN_SUFFIX.len();
            let body = &self.text[content_start..content_start + close];
            self.cursor = end;
            if valid_token_body(body) {
                return Some((start, end, &self.text[start..end]));
            }
        }
        None
    }
}

fn valid_token_body(body: &str) -> bool {
    let Some((vault, record)) = body.split_once(':') else {
        return false;
    };
    uuid::Uuid::parse_str(vault).is_ok()
        && record
            .strip_prefix("sec_")
            .is_some_and(|id| id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()))
}

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

    #[test]
    fn heuristic_cap_counts_only_candidates_not_already_in_the_dictionary() {
        let dir = tempfile::tempdir().unwrap();
        let dictionary = RepositoryDictionary::new(
            dir.path().join("dictionary/vault.json"),
            MemoryKeys::default(),
        );
        let tokens: Vec<_> = (0..=MAX_NEW_HEURISTIC_RECORDS)
            .map(|index| format!("ghp_R7kQ2mXv9LpZ4tNc8WjF3bHy6s{index:010X}"))
            .collect();
        let first = serde_json::to_string(&serde_json::json!({
            "tokens": &tokens[..MAX_NEW_HEURISTIC_RECORDS]
        }))
        .unwrap()
            + "\n";
        let first = dictionary.protect_jsonl(&first, &Matcher::empty()).unwrap();
        assert_eq!(first.new_heuristic_records, MAX_NEW_HEURISTIC_RECORDS);

        let cumulative =
            serde_json::to_string(&serde_json::json!({ "tokens": tokens })).unwrap() + "\n";
        let next = dictionary
            .protect_jsonl(&cumulative, &Matcher::empty())
            .unwrap();
        assert_eq!(next.new_heuristic_records, 1);
        assert_eq!(
            dictionary.review().unwrap().len(),
            MAX_NEW_HEURISTIC_RECORDS + 1
        );
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
    fn explicit_block_wins_over_an_allow_decision() {
        let dir = tempfile::tempdir().unwrap();
        let dictionary = RepositoryDictionary::new(
            dir.path().join("dictionary/vault.json"),
            MemoryKeys::default(),
        );
        let secret = "ghp_R7kQ2mXv9LpZ4tNc8WjF3bHy6sVd1aGe5uKr";
        let input = format!(r#"{{"message":"{secret}"}}"#) + "\n";
        let _ = dictionary.protect_jsonl(&input, &Matcher::empty()).unwrap();
        let id = dictionary.review().unwrap()[0].id.clone();
        dictionary.allow(&id).unwrap();

        let blocked = dictionary
            .block_add("must-protect", Zeroizing::new(secret.to_string()), false)
            .unwrap();
        assert_eq!(blocked.id, id);
        assert!(blocked.explicit_block);
        assert!(blocked.effective_protect);
        assert!(dictionary.active_matcher().unwrap().find(secret).len() == 1);

        let unblocked = dictionary.block_remove(&id).unwrap();
        assert!(!unblocked.explicit_block);
        assert!(!unblocked.effective_protect);
        assert!(dictionary.active_matcher().unwrap().find(secret).is_empty());
    }

    #[test]
    fn protocol_identity_fields_do_not_create_heuristic_records() {
        let dir = tempfile::tempdir().unwrap();
        let dictionary = RepositoryDictionary::new(
            dir.path().join("dictionary/vault.json"),
            MemoryKeys::default(),
        );
        let input = r#"{"session_id":"ghp_R7kQ2mXv9LpZ4tNc8WjF3bHy6sVd1aGe5uKr"}"#;
        let protected = dictionary.protect_jsonl(input, &Matcher::empty()).unwrap();
        assert_eq!(protected.replacements, 0);
        assert_eq!(protected.new_records, 0);
        assert!(dictionary.review().unwrap().is_empty());
    }
}
