//! Reverse mappings are immutable. Policy changes do not destroy recovery information.

use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use zeroize::{Zeroize, Zeroizing};

const TOKEN_PREFIX: &str = "{{AGIT_SECRET_V2:";
const FRAGMENT_BYTES: usize = 32 * 1024;
const MAX_RECORD_BYTES: usize = 8 * 1024 * 1024;
const MAX_PARTS: usize = MAX_RECORD_BYTES / FRAGMENT_BYTES + 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    Heuristic,
    GlobalUser,
    RepositoryUser,
    Legacy,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Record {
    pub token: String,
    pub original: String,
    pub origins: BTreeSet<Origin>,
}

impl Drop for Record {
    fn drop(&mut self) {
        self.original.zeroize();
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fragment {
    pub token: String,
    pub index: usize,
    pub total: usize,
    pub value: String,
    pub origins: BTreeSet<Origin>,
}

impl Drop for Fragment {
    fn drop(&mut self) {
        self.value.zeroize();
    }
}

impl Record {
    pub fn new(dictionary: &str, original: &str, origin: Origin) -> crate::Result<Self> {
        uuid::Uuid::parse_str(dictionary).context("invalid dictionary identity")?;
        ensure!(
            !original.is_empty() && original.len() <= MAX_RECORD_BYTES,
            "privacy record exceeds its byte budget"
        );
        Ok(Self {
            token: format!("{TOKEN_PREFIX}{dictionary}:{}}}}}", uuid::Uuid::new_v4()),
            original: original.to_owned(),
            origins: BTreeSet::from([origin]),
        })
    }

    pub fn fragments(&self) -> Vec<Fragment> {
        let mut chunks = Vec::new();
        let mut start = 0;
        while start < self.original.len() {
            let mut end = (start + FRAGMENT_BYTES).min(self.original.len());
            while !self.original.is_char_boundary(end) {
                end -= 1;
            }
            chunks.push(&self.original[start..end]);
            start = end;
        }
        let total = chunks.len();
        chunks
            .into_iter()
            .enumerate()
            .map(|(index, value)| Fragment {
                token: self.token.clone(),
                index,
                total,
                value: value.to_owned(),
                origins: self.origins.clone(),
            })
            .collect()
    }
}

pub fn token_identity(token: &str) -> Option<(&str, &str)> {
    let body = token
        .strip_prefix(TOKEN_PREFIX)
        .or_else(|| token.strip_prefix("{{AGIT_SECRET_V1:"))?
        .strip_suffix("}}")?;
    let (dictionary, record) = body.split_once(':')?;
    uuid::Uuid::parse_str(dictionary).ok()?;
    if token.starts_with(TOKEN_PREFIX) {
        uuid::Uuid::parse_str(record).ok()?;
    } else {
        let id = record.strip_prefix("sec_")?;
        if id.len() != 32 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
    }
    Some((dictionary, record))
}

struct Assembly {
    pieces: Vec<Option<Zeroizing<String>>>,
    origins: BTreeSet<Origin>,
    bytes: usize,
}

#[derive(Default)]
pub struct Dictionary {
    records: HashMap<String, Record>,
    by_value: HashMap<String, String>,
    pending: HashMap<String, Assembly>,
    pending_bytes: usize,
    mapped_bytes: usize,
}

impl Drop for Dictionary {
    fn drop(&mut self) {
        for (mut value, _) in self.by_value.drain() {
            value.zeroize();
        }
    }
}

impl Dictionary {
    pub fn get(&self, token: &str) -> Option<&Record> {
        self.records.get(token)
    }

    pub fn for_value(&self, value: &str) -> Option<&Record> {
        self.by_value
            .get(value)
            .and_then(|token| self.records.get(token))
    }

    pub fn records(&self) -> impl Iterator<Item = &Record> {
        self.records.values()
    }

    pub fn accept(&mut self, record: Record) -> crate::Result<()> {
        ensure!(
            token_identity(&record.token).is_some(),
            "invalid privacy placeholder"
        );
        ensure!(
            !record.original.is_empty() && record.original.len() <= MAX_RECORD_BYTES,
            "invalid privacy record size"
        );
        if let Some(existing) = self.records.get_mut(&record.token) {
            ensure!(
                existing.original == record.original,
                "conflicting privacy mapping"
            );
            existing.origins.extend(&record.origins);
        } else {
            ensure!(
                self.records.len() < 65536
                    && self.mapped_bytes + record.original.len() <= 64 * 1024 * 1024,
                "privacy dictionary exceeds its memory budget"
            );
            self.mapped_bytes += record.original.len();
            self.by_value
                .entry(record.original.clone())
                .or_insert_with(|| record.token.clone());
            self.records.insert(record.token.clone(), record);
        }
        Ok(())
    }

    /// A token is resolvable only after every authenticated fragment is available.
    pub fn accept_fragment(&mut self, part: Fragment) -> crate::Result<bool> {
        ensure!(
            token_identity(&part.token).is_some(),
            "invalid privacy placeholder"
        );
        ensure!(
            part.total > 0 && part.total <= MAX_PARTS && part.index < part.total,
            "invalid privacy fragment position"
        );
        ensure!(
            !part.value.is_empty() && part.value.len() <= FRAGMENT_BYTES,
            "invalid privacy fragment size"
        );
        if let Some(record) = self.records.get_mut(&part.token) {
            let mut start = 0;
            let mut count = 0;
            let mut matches = false;
            while start < record.original.len() {
                let mut end = (start + FRAGMENT_BYTES).min(record.original.len());
                while !record.original.is_char_boundary(end) {
                    end -= 1;
                }
                if count == part.index {
                    matches = record.original[start..end] == part.value;
                }
                count += 1;
                start = end;
            }
            ensure!(
                count == part.total && matches,
                "conflicting privacy fragment"
            );
            record.origins.extend(&part.origins);
            return Ok(true);
        }
        ensure!(
            self.pending.contains_key(&part.token) || self.pending.len() < 1024,
            "privacy assembly exceeds its record budget"
        );
        let assembly = self
            .pending
            .entry(part.token.clone())
            .or_insert_with(|| Assembly {
                pieces: (0..part.total).map(|_| None).collect(),
                origins: BTreeSet::new(),
                bytes: 0,
            });
        ensure!(
            assembly.pieces.len() == part.total,
            "conflicting privacy fragment count"
        );
        if let Some(previous) = &assembly.pieces[part.index] {
            ensure!(
                previous.as_str() == part.value,
                "conflicting privacy fragment"
            );
        } else {
            ensure!(
                assembly.bytes + part.value.len() <= MAX_RECORD_BYTES,
                "privacy record exceeds its byte budget"
            );
            ensure!(
                self.pending_bytes + part.value.len() <= 16 * 1024 * 1024,
                "privacy assembly exceeds its memory budget"
            );
            self.pending_bytes += part.value.len();
            assembly.bytes += part.value.len();
            assembly.pieces[part.index] = Some(Zeroizing::new(part.value.clone()));
        }
        assembly.origins.extend(&part.origins);
        if assembly.pieces.iter().any(Option::is_none) {
            return Ok(false);
        }
        let mut original = String::with_capacity(assembly.bytes);
        for value in assembly.pieces.iter().flatten() {
            original.push_str(value);
        }
        let record = Record {
            token: part.token.clone(),
            original,
            origins: assembly.origins.clone(),
        };
        self.accept(record)?;
        if let Some(assembly) = self.pending.remove(&part.token) {
            self.pending_bytes -= assembly.bytes;
        }
        Ok(true)
    }
}
