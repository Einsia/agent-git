//! Low-entropy secrets the user registers explicitly.
//!
//! The gitleaks rules answer "does this text look like a credential"; this answers "does it
//! contain, verbatim, a value the user already registered". The two cannot be merged into one
//! heuristic: a passphrase a human can remember may well be an ordinary sentence.
//!
//! Disk holds AES-256-GCM ciphertext only; the KEK goes to the operating-system keyring, and the
//! random DEK sits in the vault wrapped by the KEK. Unlocking builds one Aho–Corasick; a scan
//! does not walk the input once per rule.

use aes_gcm::aead::{Aead, KeyInit, OsRng, Payload, rand_core::RngCore};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use aho_corasick::{AhoCorasick, AhoCorasickBuilder, MatchKind};
use anyhow::{Context, bail};
use base64::Engine as _;
use fs2::FileExt as _;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use zeroize::{Zeroize, Zeroizing};

mod repository;

pub use repository::{
    HydrationReport, ProtectionReport, RepositoryDictionary, RepositoryRecordSummary,
};

const VAULT_VERSION: u32 = 1;
const RECORD_VERSION: u32 = 1;
const KEY_VERSION: u32 = 1;
const KEYRING_SERVICE: &str = "com.einsia.agent-git.secret-filter";
const MIN_SECRET_BYTES: usize = 4;
const DEFAULT_MIN_SECRET_BYTES: usize = 8;
const MAX_SECRET_BYTES: usize = 512;
pub(super) const MAX_REPOSITORY_SECRET_BYTES: usize = 64 * 1024;
const MAX_NAME_BYTES: usize = 128;
const PADDING_BUCKETS: &[usize] = &[
    128, 256, 512, 1024, 2048, 4096, 8192, 16_384, 32_768, 65_536, 131_072,
];
const PLACEHOLDER: &str = "[redacted:registered-secret]";
const CURRENT_SCHEMA_VERSION: u32 = 2;
const CURRENT_PROJECTION_VERSION: u32 = 1;

fn legacy_schema_version() -> u32 {
    1
}

fn legacy_projection_version() -> u32 {
    0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct VaultFile {
    version: u32,
    key_version: u32,
    #[serde(default = "legacy_schema_version")]
    schema_version: u32,
    #[serde(default = "legacy_projection_version")]
    projection_version: u32,
    vault_id: String,
    generation: u64,
    wrapped_dek: Sealed,
    records: Vec<SealedRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SealedRecord {
    id: String,
    version: u32,
    sealed: Sealed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Sealed {
    nonce: String,
    ciphertext: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RecordOrigin {
    Heuristic,
    Global,
    Explicit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum HeuristicDisposition {
    #[default]
    Protect,
    Allow,
}

#[derive(Debug, Serialize, Deserialize)]
struct PlainRecord {
    name: String,
    secret: String,
    #[serde(default)]
    origins: Vec<RecordOrigin>,
    #[serde(default)]
    heuristic_disposition: HeuristicDisposition,
    #[serde(default)]
    explicit_block: bool,
    created_at: String,
    updated_at: String,
}

struct DecryptedRecord {
    id: String,
    name: String,
    secret: Zeroizing<String>,
    origins: Vec<RecordOrigin>,
    heuristic_disposition: HeuristicDisposition,
    explicit_block: bool,
    created_at: String,
    updated_at: String,
}

/// A management-plane summary that carries no secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecordSummary {
    pub id: String,
    pub name: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VaultStatus {
    pub initialized: bool,
    pub generation: u64,
    pub rules: usize,
}

/// A runtime hit. Only an opaque id and a byte range into the original text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredMatch {
    pub id: String,
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterReport {
    pub text: String,
    pub matches: usize,
    pub ids: Vec<String>,
}

/// The KEK backend. Tests use an in-memory implementation, production the OS keyring; the vault
/// logic is not written twice.
pub trait KeyStore: Send + Sync {
    fn get(&self, vault_id: &str) -> crate::Result<Zeroizing<Vec<u8>>>;
    fn set(&self, vault_id: &str, key: &[u8]) -> crate::Result<()>;
    fn delete(&self, vault_id: &str) -> crate::Result<()>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct OsKeyStore;

impl OsKeyStore {
    fn entry(vault_id: &str) -> crate::Result<keyring::Entry> {
        keyring::Entry::new(KEYRING_SERVICE, vault_id)
            .context("cannot open the operating-system credential store")
    }
}

impl KeyStore for OsKeyStore {
    fn get(&self, vault_id: &str) -> crate::Result<Zeroizing<Vec<u8>>> {
        let key = Self::entry(vault_id)?.get_secret().with_context(|| {
            format!(
                "the secret-filter vault exists, but its OS keyring key `{vault_id}` is unavailable"
            )
        })?;
        validate_key(&key, "KEK")?;
        Ok(Zeroizing::new(key))
    }

    fn set(&self, vault_id: &str, key: &[u8]) -> crate::Result<()> {
        validate_key(key, "KEK")?;
        Self::entry(vault_id)?
            .set_secret(key)
            .context("cannot save the secret-filter KEK in the operating-system credential store")
    }

    fn delete(&self, vault_id: &str) -> crate::Result<()> {
        Self::entry(vault_id)?.delete_credential().context(
            "cannot delete the secret-filter KEK from the operating-system credential store",
        )
    }
}

/// The vault entry point. Every read and write happens under the same lock file, so two CLIs
/// cannot overwrite each other's records.
pub struct VaultStore<K: KeyStore = OsKeyStore> {
    path: PathBuf,
    keys: K,
}

impl VaultStore<OsKeyStore> {
    pub fn open_default() -> crate::Result<Self> {
        Ok(Self::new(
            crate::infra::config::secret_filter_vault_path()?,
            OsKeyStore,
        ))
    }
}

impl<K: KeyStore> VaultStore<K> {
    pub fn new(path: PathBuf, keys: K) -> Self {
        Self { path, keys }
    }

    pub fn add(
        &self,
        name: &str,
        secret: Zeroizing<String>,
        allow_short: bool,
    ) -> crate::Result<RecordSummary> {
        validate_name(name)?;
        validate_secret(&secret, allow_short)?;
        self.with_lock(|| {
            let created = !self.path.exists();
            let mut unlocked = if created {
                self.create_unlocked()?
            } else {
                self.unlock_existing()?
            };
            let records = decrypt_records(&unlocked.file, &unlocked.dek)?;
            if records.iter().any(|r| r.name == name) {
                bail!("a registered secret named `{name}` already exists");
            }
            if records
                .iter()
                .any(|r| r.secret.as_bytes() == secret.as_bytes())
            {
                bail!("that exact secret is already registered under another name");
            }

            let now = chrono::Utc::now().to_rfc3339();
            let id = format!("sec_{}", uuid::Uuid::now_v7().simple());
            let mut plain = PlainRecord {
                name: name.to_string(),
                secret: secret.to_string(),
                origins: vec![],
                heuristic_disposition: HeuristicDisposition::Protect,
                explicit_block: false,
                created_at: now.clone(),
                updated_at: now.clone(),
            };
            let encoded = encode_padded(&plain);
            // Zeroizing owns the `secret` parameter; the temporary copy made for serialization
            // is wiped at the earliest point too.
            plain.secret.zeroize();
            let encoded = encoded?;
            let aad = record_aad(&unlocked.file.vault_id, &id, RECORD_VERSION);
            let sealed = seal(&unlocked.dek, &encoded, &aad)?;
            unlocked.file.records.push(SealedRecord {
                id: id.clone(),
                version: RECORD_VERSION,
                sealed,
            });
            unlocked.file.generation += 1;
            if let Err(e) = write_vault(&self.path, &unlocked.file) {
                if created {
                    let _ = self.keys.delete(&unlocked.file.vault_id);
                }
                return Err(e);
            }
            Ok(RecordSummary {
                id,
                name: name.to_string(),
                created_at: now.clone(),
                updated_at: now,
            })
        })
    }

    pub fn list(&self) -> crate::Result<Vec<RecordSummary>> {
        self.with_lock(|| {
            if !self.path.exists() {
                return Ok(vec![]);
            }
            let unlocked = self.unlock_existing()?;
            let mut out: Vec<RecordSummary> = decrypt_records(&unlocked.file, &unlocked.dek)?
                .into_iter()
                .map(|r| RecordSummary {
                    id: r.id,
                    name: r.name,
                    created_at: r.created_at,
                    updated_at: r.updated_at,
                })
                .collect();
            out.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
            Ok(out)
        })
    }

    pub fn remove(&self, id_or_name: &str) -> crate::Result<RecordSummary> {
        self.with_lock(|| {
            if !self.path.exists() {
                bail!("the secret-filter vault has not been initialized");
            }
            let mut unlocked = self.unlock_existing()?;
            let records = decrypt_records(&unlocked.file, &unlocked.dek)?;
            let Some(found) = records
                .iter()
                .find(|r| r.id == id_or_name || r.name == id_or_name)
            else {
                bail!("no registered secret named or identified by `{id_or_name}`");
            };
            let summary = RecordSummary {
                id: found.id.clone(),
                name: found.name.clone(),
                created_at: found.created_at.clone(),
                updated_at: found.updated_at.clone(),
            };
            unlocked.file.records.retain(|r| r.id != found.id);
            unlocked.file.generation += 1;
            write_vault(&self.path, &unlocked.file)?;
            Ok(summary)
        })
    }

    pub fn status(&self) -> crate::Result<VaultStatus> {
        self.with_lock(|| {
            if !self.path.exists() {
                return Ok(VaultStatus {
                    initialized: false,
                    generation: 0,
                    rules: 0,
                });
            }
            let unlocked = self.unlock_existing()?;
            // Authenticate every record: status must not call a partly corrupt vault healthy.
            let records = decrypt_records(&unlocked.file, &unlocked.dek)?;
            Ok(VaultStatus {
                initialized: true,
                generation: unlocked.file.generation,
                rules: records.len(),
            })
        })
    }

    /// Unlock and build one immutable matcher. A missing vault is an empty set, not an error.
    pub fn matcher(&self) -> crate::Result<Matcher> {
        self.with_lock(|| {
            if !self.path.exists() {
                return Ok(Matcher::empty());
            }
            let unlocked = self.unlock_existing()?;
            let records = decrypt_records(&unlocked.file, &unlocked.dek)?;
            Matcher::build(unlocked.file.generation, records)
        })
    }

    fn create_unlocked(&self) -> crate::Result<Unlocked> {
        let vault_id = uuid::Uuid::new_v4().to_string();
        let mut kek = Zeroizing::new(vec![0u8; 32]);
        let mut dek = Zeroizing::new(vec![0u8; 32]);
        OsRng.fill_bytes(&mut kek);
        OsRng.fill_bytes(&mut dek);
        self.keys.set(&vault_id, &kek)?;
        let wrapped_dek = seal(
            &kek,
            &dek,
            &vault_aad(&vault_id, VAULT_VERSION, KEY_VERSION),
        )?;
        Ok(Unlocked {
            file: VaultFile {
                version: VAULT_VERSION,
                key_version: KEY_VERSION,
                schema_version: CURRENT_SCHEMA_VERSION,
                projection_version: CURRENT_PROJECTION_VERSION,
                vault_id,
                generation: 0,
                wrapped_dek,
                records: vec![],
            },
            dek,
        })
    }

    fn unlock_existing(&self) -> crate::Result<Unlocked> {
        let file = read_vault(&self.path)?;
        if file.version != VAULT_VERSION {
            bail!(
                "unsupported secret-filter vault version {} (this build supports {VAULT_VERSION})",
                file.version
            );
        }
        if file.key_version != KEY_VERSION {
            bail!(
                "unsupported secret-filter key version {} (this build supports {KEY_VERSION})",
                file.key_version
            );
        }
        if file.schema_version > CURRENT_SCHEMA_VERSION {
            bail!(
                "unsupported secret-filter schema version {} (this build supports through {CURRENT_SCHEMA_VERSION})",
                file.schema_version
            );
        }
        if file.projection_version > CURRENT_PROJECTION_VERSION {
            bail!(
                "unsupported secret-filter projection version {} (this build supports through {CURRENT_PROJECTION_VERSION})",
                file.projection_version
            );
        }
        let kek = self.keys.get(&file.vault_id)?;
        let dek = open(
            &kek,
            &file.wrapped_dek,
            &vault_aad(&file.vault_id, file.version, file.key_version),
        )?;
        validate_key(&dek, "DEK")?;
        Ok(Unlocked { file, dek })
    }

    fn with_lock<T>(&self, f: impl FnOnce() -> crate::Result<T>) -> crate::Result<T> {
        let dir = self
            .path
            .parent()
            .context("the secret-filter vault path has no parent directory")?;
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        set_private_dir(dir)?;
        let lock_path = dir.join("vault.lock");
        let lock = std::fs::OpenOptions::new()
            .create(true)
            // An inode taken only to carry the file lock; its content is never read or written.
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .with_context(|| format!("cannot open {}", lock_path.display()))?;
        set_private_file(&lock_path)?;
        lock.lock_exclusive()
            .with_context(|| format!("cannot lock {}", lock_path.display()))?;
        let result = f();
        let unlock = fs2::FileExt::unlock(&lock)
            .with_context(|| format!("cannot unlock {}", lock_path.display()));
        match (result, unlock) {
            (Err(e), _) => Err(e),
            (Ok(_), Err(e)) => Err(e),
            (Ok(v), Ok(())) => Ok(v),
        }
    }
}

struct Unlocked {
    file: VaultFile,
    dek: Zeroizing<Vec<u8>>,
}

#[derive(Clone)]
pub struct Matcher {
    inner: Arc<MatcherInner>,
}

struct MatcherInner {
    generation: u64,
    ac: Option<AhoCorasick>,
    ids: Vec<String>,
    // Aho-Corasick owns an opaque copy of every pattern. Keeping an explicitly
    // zeroizing copy does not widen the runtime trust boundary (the automaton is
    // already recoverable), and lets the repository dictionary build one
    // combined matcher without decrypting the global vault a second time.
    patterns: Vec<Zeroizing<String>>,
    max_pattern_len: usize,
}

impl std::fmt::Debug for Matcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Matcher")
            .field("generation", &self.inner.generation)
            .field("rules", &self.inner.ids.len())
            .field("max_pattern_len", &self.inner.max_pattern_len)
            .finish_non_exhaustive()
    }
}

impl Matcher {
    pub fn empty() -> Self {
        Self {
            inner: Arc::new(MatcherInner {
                generation: 0,
                ac: None,
                ids: vec![],
                patterns: vec![],
                max_pattern_len: 0,
            }),
        }
    }

    fn build(generation: u64, records: Vec<DecryptedRecord>) -> crate::Result<Self> {
        if records.is_empty() {
            let mut empty = Self::empty();
            Arc::get_mut(&mut empty.inner)
                .expect("new Arc is unique")
                .generation = generation;
            return Ok(empty);
        }
        let patterns: Vec<Zeroizing<String>> = records
            .iter()
            .map(|r| Zeroizing::new(r.secret.to_string()))
            .collect();
        let max_pattern_len = patterns
            .iter()
            .map(|pattern| pattern.len())
            .max()
            .unwrap_or(0);
        let ac = AhoCorasickBuilder::new()
            .match_kind(MatchKind::LeftmostLongest)
            .build(patterns.iter().map(|p| p.as_bytes()))
            .context("cannot build the registered-secret matcher")?;
        let ids = records.into_iter().map(|r| r.id).collect();
        Ok(Self {
            inner: Arc::new(MatcherInner {
                generation,
                ac: Some(ac),
                ids,
                patterns,
                max_pattern_len,
            }),
        })
    }

    #[cfg(test)]
    pub(crate) fn for_test(patterns: &[(&str, &str)]) -> Self {
        let records = patterns
            .iter()
            .map(|(id, secret)| DecryptedRecord {
                id: (*id).to_string(),
                name: (*id).to_string(),
                secret: Zeroizing::new((*secret).to_string()),
                origins: vec![],
                heuristic_disposition: HeuristicDisposition::Protect,
                explicit_block: false,
                created_at: "test".to_string(),
                updated_at: "test".to_string(),
            })
            .collect();
        Self::build(1, records).expect("test patterns are valid")
    }

    pub fn generation(&self) -> u64 {
        self.inner.generation
    }

    pub fn rules(&self) -> usize {
        self.inner.ids.len()
    }

    pub fn max_pattern_len(&self) -> usize {
        self.inner.max_pattern_len
    }

    fn patterns(&self) -> impl Iterator<Item = (&str, &str)> {
        self.inner
            .ids
            .iter()
            .map(String::as_str)
            .zip(self.inner.patterns.iter().map(|p| p.as_str()))
    }

    pub(crate) fn merged(&self, other: &Self) -> crate::Result<Self> {
        let mut records = Vec::with_capacity(self.rules() + other.rules());
        for (id, secret) in self.patterns().chain(other.patterns()) {
            if records
                .iter()
                .any(|record: &DecryptedRecord| record.secret.as_str() == secret)
            {
                continue;
            }
            records.push(DecryptedRecord {
                id: id.to_string(),
                name: id.to_string(),
                secret: Zeroizing::new(secret.to_string()),
                origins: vec![],
                heuristic_disposition: HeuristicDisposition::Protect,
                explicit_block: false,
                created_at: "runtime".to_string(),
                updated_at: "runtime".to_string(),
            });
        }
        Self::build(self.generation().max(other.generation()), records)
    }

    pub fn find(&self, text: &str) -> Vec<RegisteredMatch> {
        self.find_capped(text, usize::MAX).0
    }

    /// Return the earliest registered match or opaque repository token that
    /// straddles `cut`. A trailing token prefix is also held until it either
    /// completes or becomes structurally invalid.
    pub fn crossing_start(&self, text: &str, cut: usize) -> Option<usize> {
        let mut earliest = repository::streaming_token_start(text, cut);
        if let Some(ac) = &self.inner.ac {
            for found in ac.find_iter(text.as_bytes()) {
                if found.start() >= cut {
                    break;
                }
                if found.end() > cut {
                    earliest =
                        Some(earliest.map_or(found.start(), |current| current.min(found.start())));
                    break;
                }
            }
        }
        earliest
    }

    /// `truncated` is true only when a `cap + 1`-th match actually exists.
    pub fn find_capped(&self, text: &str, cap: usize) -> (Vec<RegisteredMatch>, bool) {
        if self.inner.ac.is_none() {
            return (vec![], false);
        }
        let mut out = Vec::with_capacity(cap.min(16));
        let mut truncated = false;
        let mut cursor = 0usize;
        for (start, end, _) in repository::token_segments(text) {
            self.find_segment_capped(&text[cursor..start], cursor, cap, &mut out, &mut truncated);
            if truncated {
                return (out, true);
            }
            cursor = end;
        }
        self.find_segment_capped(&text[cursor..], cursor, cap, &mut out, &mut truncated);
        (out, truncated)
    }

    fn find_segment_capped(
        &self,
        text: &str,
        base: usize,
        cap: usize,
        out: &mut Vec<RegisteredMatch>,
        truncated: &mut bool,
    ) {
        let Some(ac) = &self.inner.ac else { return };
        for found in ac.find_iter(text.as_bytes()) {
            if out.len() == cap {
                *truncated = true;
                return;
            }
            out.push(RegisteredMatch {
                id: self.inner.ids[found.pattern().as_usize()].clone(),
                start: base + found.start(),
                end: base + found.end(),
            });
        }
    }

    pub fn scrub(&self, text: &str) -> FilterReport {
        let Some(ac) = &self.inner.ac else {
            return FilterReport {
                text: text.to_string(),
                matches: 0,
                ids: vec![],
            };
        };

        // Build the output while the automaton advances. A four-byte rule may
        // match millions of times in a large transcript; collecting every span
        // (and cloning its record id) before replacing is an avoidable OOM.
        let mut out = String::with_capacity(text.len());
        let mut cursor = 0usize;
        let mut matches = 0usize;
        let mut seen = HashSet::new();
        let mut ids = Vec::new();
        for (start, end, opaque) in repository::token_segments(text) {
            scrub_registered_segment(
                ac,
                &self.inner.ids,
                &text[cursor..start],
                &mut out,
                &mut matches,
                &mut seen,
                &mut ids,
            );
            out.push_str(opaque);
            cursor = end;
        }
        scrub_registered_segment(
            ac,
            &self.inner.ids,
            &text[cursor..],
            &mut out,
            &mut matches,
            &mut seen,
            &mut ids,
        );
        if matches == 0 {
            return FilterReport {
                text: text.to_string(),
                matches: 0,
                ids,
            };
        }
        FilterReport {
            text: out,
            matches,
            ids,
        }
    }
}

fn scrub_registered_segment<'a>(
    ac: &AhoCorasick,
    registered_ids: &'a [String],
    text: &str,
    out: &mut String,
    matches: &mut usize,
    seen: &mut HashSet<&'a str>,
    ids: &mut Vec<String>,
) {
    let mut cursor = 0usize;
    for found in ac.find_iter(text.as_bytes()) {
        out.push_str(&text[cursor..found.start()]);
        out.push_str(PLACEHOLDER);
        cursor = found.end();
        *matches = matches.saturating_add(1);
        let id = &registered_ids[found.pattern().as_usize()];
        if seen.insert(id.as_str()) {
            ids.push(id.clone());
        }
    }
    out.push_str(&text[cursor..]);
}

impl Default for Matcher {
    fn default() -> Self {
        Self::empty()
    }
}

/// The immutable matcher pointer every live session shares. A reload builds the replacement in
/// full first, then swaps it in one step.
#[derive(Clone, Default)]
pub struct MatcherHandle {
    current: Arc<RwLock<Matcher>>,
}

impl MatcherHandle {
    pub fn new(matcher: Matcher) -> Self {
        Self {
            current: Arc::new(RwLock::new(matcher)),
        }
    }

    pub fn load_default() -> crate::Result<Self> {
        Ok(Self::new(VaultStore::open_default()?.matcher()?))
    }

    pub fn snapshot(&self) -> Matcher {
        self.current
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn replace(&self, matcher: Matcher) {
        *self
            .current
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = matcher;
    }

    pub fn reload_default(&self) -> crate::Result<VaultStatus> {
        let store = VaultStore::open_default()?;
        let matcher = store.matcher()?;
        let status = VaultStatus {
            initialized: store.path.exists(),
            generation: matcher.generation(),
            rules: matcher.rules(),
        };
        self.replace(matcher);
        Ok(status)
    }
}

fn validate_name(name: &str) -> crate::Result<()> {
    if name.trim().is_empty() {
        bail!("the registered secret name cannot be empty");
    }
    if name.len() > MAX_NAME_BYTES {
        bail!("the registered secret name is longer than {MAX_NAME_BYTES} UTF-8 bytes");
    }
    Ok(())
}

fn validate_secret(secret: &str, allow_short: bool) -> crate::Result<()> {
    validate_secret_with_limit(secret, allow_short, MAX_SECRET_BYTES)
}

fn validate_stored_secret(secret: &str, allow_short: bool) -> crate::Result<()> {
    validate_secret_with_limit(secret, allow_short, MAX_REPOSITORY_SECRET_BYTES)
}

fn validate_secret_with_limit(
    secret: &str,
    allow_short: bool,
    max_bytes: usize,
) -> crate::Result<()> {
    let n = secret.len();
    if n < MIN_SECRET_BYTES {
        bail!("registered secrets must be at least {MIN_SECRET_BYTES} UTF-8 bytes");
    }
    if n < DEFAULT_MIN_SECRET_BYTES && !allow_short {
        bail!(
            "registered secrets shorter than {DEFAULT_MIN_SECRET_BYTES} UTF-8 bytes require --allow-short"
        );
    }
    if n > max_bytes {
        bail!("registered secrets cannot exceed {max_bytes} UTF-8 bytes");
    }
    Ok(())
}

fn validate_key(key: &[u8], label: &str) -> crate::Result<()> {
    if key.len() != 32 {
        bail!(
            "the secret-filter {label} has {} bytes; expected 32",
            key.len()
        );
    }
    Ok(())
}

fn vault_aad(vault_id: &str, version: u32, key_version: u32) -> Vec<u8> {
    format!("agent-git/secret-filter/vault/{version}/{key_version}/{vault_id}").into_bytes()
}

fn record_aad(vault_id: &str, record_id: &str, version: u32) -> Vec<u8> {
    format!("agent-git/secret-filter/record/{version}/{vault_id}/{record_id}").into_bytes()
}

fn seal(key: &[u8], plaintext: &[u8], aad: &[u8]) -> crate::Result<Sealed> {
    validate_key(key, "encryption key")?;
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let mut nonce = [0u8; 12];
    OsRng.fill_bytes(&mut nonce);
    let ciphertext = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| anyhow::anyhow!("secret-filter encryption failed"))?;
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    Ok(Sealed {
        nonce: b64.encode(nonce),
        ciphertext: b64.encode(ciphertext),
    })
}

fn open(key: &[u8], sealed: &Sealed, aad: &[u8]) -> crate::Result<Zeroizing<Vec<u8>>> {
    validate_key(key, "decryption key")?;
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let nonce = b64
        .decode(&sealed.nonce)
        .context("the secret-filter nonce is not valid base64url")?;
    if nonce.len() != 12 {
        bail!(
            "the secret-filter nonce has {} bytes; expected 12",
            nonce.len()
        );
    }
    let ciphertext = b64
        .decode(&sealed.ciphertext)
        .context("the secret-filter ciphertext is not valid base64url")?;
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let plaintext = cipher
        .decrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: &ciphertext,
                aad,
            },
        )
        .map_err(|_| {
            anyhow::anyhow!("secret-filter authentication failed; refusing a partial scan")
        })?;
    Ok(Zeroizing::new(plaintext))
}

fn encode_padded(record: &PlainRecord) -> crate::Result<Zeroizing<Vec<u8>>> {
    let json = Zeroizing::new(serde_json::to_vec(record)?);
    let need = json
        .len()
        .checked_add(4)
        .context("secret-filter record length overflow")?;
    let bucket = PADDING_BUCKETS
        .iter()
        .copied()
        .find(|n| *n >= need)
        .context("the registered secret metadata is too large for the largest storage bucket")?;
    let mut out = Zeroizing::new(vec![0u8; bucket]);
    out[..4].copy_from_slice(&(json.len() as u32).to_be_bytes());
    out[4..4 + json.len()].copy_from_slice(&json);
    OsRng.fill_bytes(&mut out[4 + json.len()..]);
    Ok(out)
}

fn decode_padded(plaintext: &[u8]) -> crate::Result<PlainRecord> {
    if plaintext.len() < 4 {
        bail!("decrypted secret-filter record is shorter than its length prefix");
    }
    let len = u32::from_be_bytes(plaintext[..4].try_into().expect("four bytes")) as usize;
    let end = 4usize
        .checked_add(len)
        .context("secret-filter record length overflow")?;
    if end > plaintext.len() {
        bail!("decrypted secret-filter record length exceeds its padded payload");
    }
    serde_json::from_slice(&plaintext[4..end])
        .context("decrypted secret-filter record is malformed")
}

fn decrypt_records(file: &VaultFile, dek: &[u8]) -> crate::Result<Vec<DecryptedRecord>> {
    let mut out = Vec::with_capacity(file.records.len());
    for stored in &file.records {
        if stored.version != RECORD_VERSION {
            bail!(
                "unsupported secret-filter record version {} for {}",
                stored.version,
                stored.id
            );
        }
        let aad = record_aad(&file.vault_id, &stored.id, stored.version);
        let plaintext = open(dek, &stored.sealed, &aad)
            .with_context(|| format!("cannot authenticate registered secret {}", stored.id))?;
        let mut plain = decode_padded(&plaintext)
            .with_context(|| format!("cannot decode registered secret {}", stored.id))?;
        validate_name(&plain.name)?;
        validate_stored_secret(&plain.secret, true)?;
        let secret = Zeroizing::new(std::mem::take(&mut plain.secret));
        out.push(DecryptedRecord {
            id: stored.id.clone(),
            name: plain.name,
            secret,
            origins: plain.origins,
            heuristic_disposition: plain.heuristic_disposition,
            explicit_block: plain.explicit_block,
            created_at: plain.created_at,
            updated_at: plain.updated_at,
        });
    }
    Ok(out)
}

fn read_vault(path: &Path) -> crate::Result<VaultFile> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("cannot read secret-filter vault {}", path.display()))?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("secret-filter vault {} is malformed", path.display()))
}

fn write_vault(path: &Path, file: &VaultFile) -> crate::Result<()> {
    let dir = path
        .parent()
        .context("the secret-filter vault path has no parent directory")?;
    let mut temp = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("cannot create a temporary vault in {}", dir.display()))?;
    serde_json::to_writer_pretty(&mut temp, file)?;
    temp.write_all(b"\n")?;
    temp.flush()?;
    temp.as_file().sync_all()?;
    set_private_file(temp.path())?;
    temp.persist(path)
        .map_err(|e| e.error)
        .with_context(|| format!("cannot atomically replace {}", path.display()))?;
    set_private_file(path)?;
    Ok(())
}

#[cfg(unix)]
fn set_private_dir(path: &Path) -> crate::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("cannot chmod 0700 on {}", path.display()))
}

#[cfg(not(unix))]
fn set_private_dir(_path: &Path) -> crate::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file(path: &Path) -> crate::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("cannot chmod 0600 on {}", path.display()))
}

#[cfg(not(unix))]
fn set_private_file(_path: &Path) -> crate::Result<()> {
    Ok(())
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

    /// A registered literal can be too low-entropy for any heuristic rule to fire; text bound
    /// for history must be scanned against it as well.
    #[test]
    fn a_registered_low_entropy_value_is_a_hit_for_text_scans() {
        let dir = tempfile::tempdir().unwrap();
        let vault = store(&dir, MemoryKeys::default());
        vault
            .add(
                "router",
                Zeroizing::new("acme-router-pass".to_string()),
                true,
            )
            .unwrap();
        let matcher = vault.matcher().unwrap();
        let allowlist = std::collections::HashSet::new();
        let text = "the wifi password is acme-router-pass, ask ops\n";
        assert!(
            crate::domain::secrets::scan_text(text, &allowlist).is_empty(),
            "no heuristic rule matches a low-entropy word"
        );
        let hits = crate::domain::secrets::scan_text_registered_with(text, &allowlist, &matcher);
        assert!(
            hits.iter().any(|h| h.rule == "registered-secret"),
            "{hits:?}"
        );
    }

    fn store(dir: &tempfile::TempDir, keys: MemoryKeys) -> VaultStore<MemoryKeys> {
        VaultStore::new(dir.path().join("vault.json"), keys)
    }

    #[test]
    fn encrypted_vault_roundtrips_and_matcher_is_literal() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir, MemoryKeys::default());
        let secret = "hello.*world";
        let added = store
            .add("memorable", Zeroizing::new(secret.into()), false)
            .unwrap();

        let disk = std::fs::read_to_string(dir.path().join("vault.json")).unwrap();
        assert!(!disk.contains(secret), "the vault must not hold plaintext");
        assert!(
            !disk.contains("memorable"),
            "the name is inside the encrypted payload too"
        );

        let matcher = store.matcher().unwrap();
        assert!(
            matcher
                .find("x hello.*world y")
                .iter()
                .any(|m| m.id == added.id)
        );
        assert!(
            matcher.find("x hello-ANY-world y").is_empty(),
            "a literal, not a regex"
        );
        let redacted = matcher.scrub("x hello.*world y");
        assert_eq!(redacted.text, format!("x {PLACEHOLDER} y"));
        assert_eq!(redacted.ids, vec![added.id]);
    }

    #[test]
    fn same_plaintext_gets_randomized_ciphertext() {
        let key = [7u8; 32];
        let a = seal(&key, b"same", b"aad").unwrap();
        let b = seal(&key, b"same", b"aad").unwrap();
        assert_ne!(a.nonce, b.nonce);
        assert_ne!(a.ciphertext, b.ciphertext);
        assert_eq!(&*open(&key, &a, b"aad").unwrap(), b"same");
    }

    #[test]
    fn aad_or_ciphertext_tampering_fails_closed() {
        let key = [9u8; 32];
        let sealed = seal(&key, b"secret", b"record-a").unwrap();
        assert!(open(&key, &sealed, b"record-b").is_err());

        let mut bad_nonce = sealed.clone();
        corrupt_base64url(&mut bad_nonce.nonce);
        assert!(open(&key, &bad_nonce, b"record-a").is_err());

        let mut bad_ciphertext = sealed;
        corrupt_base64url(&mut bad_ciphertext.ciphertext);
        assert!(open(&key, &bad_ciphertext, b"record-a").is_err());
    }

    fn corrupt_base64url(value: &mut String) {
        let replacement = if value.starts_with('A') { "B" } else { "A" };
        value.replace_range(0..1, replacement);
    }

    #[test]
    fn duplicate_name_and_secret_are_rejected_and_remove_rebuilds() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir, MemoryKeys::default());
        let one = store
            .add("one", Zeroizing::new("blue horse battery".into()), false)
            .unwrap();
        assert!(
            store
                .add("one", Zeroizing::new("another long value".into()), false)
                .is_err()
        );
        assert!(
            store
                .add("two", Zeroizing::new("blue horse battery".into()), false)
                .is_err()
        );
        assert_eq!(store.list().unwrap().len(), 1);
        assert_eq!(store.remove(&one.id).unwrap().name, "one");
        assert!(
            store
                .matcher()
                .unwrap()
                .find("blue horse battery")
                .is_empty()
        );
    }

    #[test]
    fn short_values_need_an_explicit_override() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir, MemoryKeys::default());
        assert!(
            store
                .add("tiny", Zeroizing::new("abc".into()), true)
                .is_err()
        );
        assert!(
            store
                .add("short", Zeroizing::new("abcd".into()), false)
                .is_err()
        );
        store
            .add("short", Zeroizing::new("abcd".into()), true)
            .unwrap();
    }

    #[test]
    fn corrupted_record_makes_status_and_matcher_fail() {
        let dir = tempfile::tempdir().unwrap();
        let keys = MemoryKeys::default();
        let store = store(&dir, keys);
        store
            .add("one", Zeroizing::new("correct horse battery".into()), false)
            .unwrap();
        let path = dir.path().join("vault.json");
        let mut file: VaultFile = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        corrupt_base64url(&mut file.records[0].sealed.ciphertext);
        write_vault(&path, &file).unwrap();
        assert!(store.status().is_err());
        assert!(store.matcher().is_err());
    }

    #[test]
    fn padded_payload_hides_exact_length_bucket() {
        let a = PlainRecord {
            name: "a".into(),
            secret: "12345678".into(),
            origins: vec![],
            heuristic_disposition: HeuristicDisposition::Protect,
            explicit_block: false,
            created_at: "t".into(),
            updated_at: "t".into(),
        };
        let b = PlainRecord {
            name: "b".into(),
            secret: "1234567890123456".into(),
            origins: vec![],
            heuristic_disposition: HeuristicDisposition::Protect,
            explicit_block: false,
            created_at: "t".into(),
            updated_at: "t".into(),
        };
        assert_eq!(
            encode_padded(&a).unwrap().len(),
            encode_padded(&b).unwrap().len()
        );
    }

    #[test]
    fn streaming_redactor_catches_a_match_split_across_deltas() {
        let matcher = Matcher::for_test(&[("sec_test", "correct horse battery")]);
        let handle = MatcherHandle::new(matcher);
        let redactor = crate::domain::redact::Redactor::with_registered(
            crate::domain::redact::Persona::default(),
            handle,
        );
        let mut stream = redactor.stream();
        let first = stream.push("before correct horse ");
        let second = stream.push("battery after");
        let last = stream.flush();
        let text = format!("{}{}{}", first.text, second.text, last.text);
        assert_eq!(text, format!("before {PLACEHOLDER} after"));
        let ids = [first, second, last]
            .into_iter()
            .flat_map(|report| report.registered_ids)
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["sec_test"]);
    }

    #[test]
    fn matcher_handle_replacement_is_visible_to_existing_redactors() {
        let handle = MatcherHandle::default();
        let redactor = crate::domain::redact::Redactor::with_registered(
            crate::domain::redact::Persona::default(),
            handle.clone(),
        );
        assert_eq!(redactor.scrub("blue horse battery").secrets, 0);
        handle.replace(Matcher::for_test(&[("sec_new", "blue horse battery")]));
        let report = redactor.scrub("blue horse battery");
        assert_eq!(report.text, PLACEHOLDER);
        assert_eq!(report.registered_ids, vec!["sec_new"]);
    }

    #[test]
    fn missing_keyring_key_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir, MemoryKeys::default());
        store
            .add(
                "memorable",
                Zeroizing::new("correct horse battery".into()),
                false,
            )
            .unwrap();
        let file = read_vault(&dir.path().join("vault.json")).unwrap();
        store.keys.delete(&file.vault_id).unwrap();
        assert!(store.status().is_err());
        assert!(store.matcher().is_err());
    }

    #[test]
    fn unicode_and_overlapping_rules_have_deterministic_leftmost_longest_semantics() {
        // CJK segmentation fixtures: overlapping multi-byte literals pin leftmost-longest
        // semantics and char-boundary offsets; ASCII literals make the test vacuous.
        let matcher = Matcher::for_test(&[
            ("short", "可记忆"),
            ("long", "可记忆口令"),
            ("other", "蓝马电池"),
        ]);
        let text = "开头可记忆口令，中间蓝马电池，结尾可记忆口令";
        let found = matcher.find(text);
        assert_eq!(
            found.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["long", "other", "long"]
        );
        for m in found {
            assert!(text.is_char_boundary(m.start));
            assert!(text.is_char_boundary(m.end));
        }
    }

    #[test]
    fn dense_matches_are_capped_without_changing_the_verdict() {
        let matcher = Matcher::for_test(&[("dense", "blue horse battery")]);
        let text = "blue horse battery ".repeat(10_000);
        let (found, truncated) = matcher.find_capped(&text, 20);
        assert_eq!(found.len(), 20);
        assert!(truncated);
    }
}
