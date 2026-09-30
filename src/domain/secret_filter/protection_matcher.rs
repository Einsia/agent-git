//! Reuse compiled automata only after the caller applies current secret policy.
use aho_corasick::{AhoCorasick, AhoCorasickBuilder, MatchKind};
use anyhow::Context as _;
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, OnceLock},
};

const CACHE_BYTES: usize = 256 * 1024 * 1024;
const CACHE_ENTRIES: usize = 4;
type Entry = ([u8; 32], Arc<AhoCorasick>);
static CACHE: OnceLock<Mutex<VecDeque<Entry>>> = OnceLock::new();

pub(super) fn compile(patterns: &[&str]) -> crate::Result<Arc<AhoCorasick>> {
    // Order and boundaries determine pattern IDs, which select each replacement's source.
    let mut digest = Sha256::new();
    digest.update(b"agit-secret-protector-standard-v1");
    for pattern in patterns {
        digest.update((pattern.len() as u64).to_le_bytes());
        digest.update(pattern.as_bytes());
    }
    let key: [u8; 32] = digest.finalize().into();
    let cache = CACHE.get_or_init(Default::default);
    {
        let mut entries = cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(index) = entries.iter().position(|(candidate, _)| candidate == &key) {
            let entry = entries.remove(index).expect("the matched entry exists");
            let result = entry.1.clone();
            entries.push_back(entry);
            return Ok(result);
        }
    }
    // Compilation does not hold the cache lock; independent repositories may prepare in parallel.
    let compiled = Arc::new(
        AhoCorasickBuilder::new()
            .match_kind(MatchKind::Standard)
            .build(patterns.iter().map(|pattern| pattern.as_bytes()))
            .context("cannot build the repository secret protector")?,
    );
    if compiled.memory_usage() <= CACHE_BYTES {
        let mut entries = cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        entries.retain(|(candidate, _)| candidate != &key);
        while entries.len() >= CACHE_ENTRIES
            || entries
                .iter()
                .map(|(_, value)| value.memory_usage())
                .sum::<usize>()
                + compiled.memory_usage()
                > CACHE_BYTES
        {
            entries.pop_front();
        }
        entries.push_back((key, compiled.clone()));
    }
    Ok(compiled)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn policy_changes_and_pattern_order_cannot_reuse_stale_match_ids() {
        let first = compile(&["cache-policy-alpha", "cache-policy-beta"]).unwrap();
        let same = compile(&["cache-policy-alpha", "cache-policy-beta"]).unwrap();
        assert!(Arc::ptr_eq(&first, &same));
        let reversed = compile(&["cache-policy-beta", "cache-policy-alpha"]).unwrap();
        assert_eq!(
            reversed
                .find("cache-policy-alpha")
                .unwrap()
                .pattern()
                .as_usize(),
            1
        );
        let removed = compile(&["cache-policy-alpha"]).unwrap();
        assert!(removed.find("cache-policy-beta").is_none());
        let boundary = compile(&["cache-policy-al", "phacache-policy-beta"]).unwrap();
        assert!(boundary.find("cache-policy-alpha").is_some());
        assert!(boundary.find("cache-policy-beta").is_none());
    }
}
