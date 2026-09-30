//! Reuse compiled pattern prefixes only after the caller applies current secret policy.
use aho_corasick::{
    AhoCorasick, AhoCorasickBuilder, FindOverlappingIter, Match, MatchKind, PatternID,
};
use anyhow::Context as _;
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    iter::Peekable,
    sync::{Arc, Mutex, OnceLock},
};

const CACHE_BYTES: usize = 256 * 1024 * 1024;
const CACHE_ENTRIES: usize = 4;
const MERGE_TAIL_BYTES: usize = 64 * 1024;
const MAX_SEGMENTS: usize = 8;

#[derive(Clone)]
struct Segment {
    start: usize,
    end: usize,
    pattern_bytes: usize,
    automaton: Arc<AhoCorasick>,
}

pub(super) struct Compiled {
    segments: Vec<Segment>,
}

impl Compiled {
    pub(super) fn max_pattern_len(&self) -> usize {
        self.segments
            .iter()
            .map(|segment| segment.automaton.max_pattern_len())
            .max()
            .unwrap_or(0)
    }

    fn memory_usage(&self) -> usize {
        self.segments
            .iter()
            .map(|segment| segment.automaton.memory_usage())
            .sum()
    }

    pub(super) fn find_overlapping_iter<'a, 'h>(&'a self, text: &'h [u8]) -> Overlapping<'a, 'h> {
        Overlapping(
            self.segments
                .iter()
                .map(|segment| {
                    (
                        segment.start,
                        segment.automaton.find_overlapping_iter(text).peekable(),
                    )
                })
                .collect(),
        )
    }

    #[cfg(test)]
    fn find(&self, text: &str) -> Option<Match> {
        self.find_overlapping_iter(text.as_bytes()).next()
    }
}

pub(super) struct Overlapping<'a, 'h>(Vec<(usize, Peekable<FindOverlappingIter<'a, 'h>>)>);

impl Iterator for Overlapping<'_, '_> {
    type Item = Match;
    fn next(&mut self) -> Option<Match> {
        // Match ends order the component frontier; pattern IDs always address the current table.
        let index = self
            .0
            .iter_mut()
            .enumerate()
            .filter_map(|(index, (offset, iter))| {
                let found = iter.peek()?;
                Some((index, (found.end(), found.pattern().as_usize() + *offset)))
            })
            .min_by_key(|(_, key)| *key)?
            .0;
        let (offset, iter) = &mut self.0[index];
        let found = iter.next()?;
        Some(Match::new(
            PatternID::must(found.pattern().as_usize() + *offset),
            found.start()..found.end(),
        ))
    }
}

struct Entry {
    key: [u8; 32],
    patterns: Vec<[u8; 32]>,
    matcher: Arc<Compiled>,
}
static CACHE: OnceLock<Mutex<VecDeque<Entry>>> = OnceLock::new();

pub(super) fn compile(patterns: &[&str], stable_prefix: usize) -> crate::Result<Arc<Compiled>> {
    anyhow::ensure!(
        stable_prefix <= patterns.len(),
        "invalid secret matcher prefix"
    );
    let fingerprints: Vec<[u8; 32]> = patterns
        .iter()
        .map(|pattern| Sha256::digest(pattern.as_bytes()).into())
        .collect();
    let mut digest = Sha256::new();
    digest.update(b"agit-secret-protector-segments-v1");
    for fingerprint in &fingerprints {
        digest.update(fingerprint);
    }
    let key: [u8; 32] = digest.finalize().into();
    let cache = CACHE.get_or_init(Default::default);
    let mut segments = {
        let mut entries = cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(index) = entries.iter().position(|entry| entry.key == key) {
            let entry = entries.remove(index).expect("the matched entry exists");
            let result = entry.matcher.clone();
            entries.push_back(entry);
            return Ok(result);
        }
        entries
            .iter()
            .map(|entry| {
                let common = entry
                    .patterns
                    .iter()
                    .zip(&fingerprints)
                    .take_while(|(a, b)| a == b)
                    .count();
                let segments: Vec<_> = entry
                    .matcher
                    .segments
                    .iter()
                    .take_while(|segment| segment.end <= common)
                    .cloned()
                    .collect();
                (segments.last().map_or(0, |segment| segment.end), segments)
            })
            .max_by_key(|(prefix, _)| *prefix)
            .map(|(_, segments)| segments)
            .unwrap_or_default()
    };
    // Coalesce small tails without rebuilding the immutable prefix on each appended finding.
    while segments
        .last()
        .is_some_and(|segment| segment.pattern_bytes <= MERGE_TAIL_BYTES)
        || segments.len() >= MAX_SEGMENTS - 1
    {
        segments.pop();
    }
    let mut start = segments.last().map_or(0, |segment| segment.end);
    for end in [stable_prefix, patterns.len()] {
        if end <= start {
            continue;
        }
        let automaton = Arc::new(
            AhoCorasickBuilder::new()
                .match_kind(MatchKind::Standard)
                .build(
                    patterns[start..end]
                        .iter()
                        .map(|pattern| pattern.as_bytes()),
                )
                .context("cannot build the repository secret protector")?,
        );
        segments.push(Segment {
            start,
            end,
            pattern_bytes: patterns[start..end]
                .iter()
                .map(|pattern| pattern.len())
                .sum(),
            automaton,
        });
        start = end;
    }
    let compiled = Arc::new(Compiled { segments });
    let bytes = compiled.memory_usage() + fingerprints.len() * std::mem::size_of::<[u8; 32]>();
    if bytes <= CACHE_BYTES {
        let mut entries = cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        entries.retain(|entry| entry.key != key);
        while entries.len() >= CACHE_ENTRIES
            || entries
                .iter()
                .map(|entry| {
                    entry.matcher.memory_usage()
                        + entry.patterns.len() * std::mem::size_of::<[u8; 32]>()
                })
                .sum::<usize>()
                + bytes
                > CACHE_BYTES
        {
            entries.pop_front();
        }
        entries.push_back(Entry {
            key,
            patterns: fingerprints,
            matcher: compiled.clone(),
        });
    }
    Ok(compiled)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn policy_changes_and_pattern_order_cannot_reuse_stale_match_ids() {
        let first = compile(&["cache-policy-alpha", "cache-policy-beta"], 2).unwrap();
        let same = compile(&["cache-policy-alpha", "cache-policy-beta"], 2).unwrap();
        assert!(Arc::ptr_eq(&first, &same));
        let reversed = compile(&["cache-policy-beta", "cache-policy-alpha"], 2).unwrap();
        assert_eq!(
            reversed
                .find("cache-policy-alpha")
                .unwrap()
                .pattern()
                .as_usize(),
            1
        );
        let removed = compile(&["cache-policy-alpha"], 1).unwrap();
        assert!(removed.find("cache-policy-beta").is_none());
        let boundary = compile(&["cache-policy-al", "phacache-policy-beta"], 2).unwrap();
        assert!(boundary.find("cache-policy-alpha").is_some());
        assert!(boundary.find("cache-policy-beta").is_none());
    }

    #[test]
    fn appended_patterns_reuse_prefix_and_preserve_cross_segment_overlaps() {
        let padding: Vec<_> = (0..2048)
            .map(|index| format!("prefix-padding-{index:08x}-padding-padding"))
            .collect();
        let mut base: Vec<_> = padding.iter().map(String::as_str).collect();
        base.extend(["abcd", "bc"]);
        let prefix = compile(&base, base.len()).unwrap();
        let mut patterns = base.clone();
        patterns.extend(["cdef", "defg"]);
        let appended = compile(&patterns, base.len()).unwrap();
        assert!(Arc::ptr_eq(
            &prefix.segments[0].automaton,
            &appended.segments[0].automaton
        ));
        let whole = AhoCorasickBuilder::new()
            .match_kind(MatchKind::Standard)
            .build(&patterns)
            .unwrap();
        let text = b"abcdefg abcdefg";
        let tuple = |found: Match| (found.start(), found.end(), found.pattern().as_usize());
        let mut actual: Vec<_> = appended.find_overlapping_iter(text).map(tuple).collect();
        let mut expected: Vec<_> = whole.find_overlapping_iter(text).map(tuple).collect();
        assert!(actual.windows(2).all(|pair| pair[0].1 <= pair[1].1));
        actual.sort_unstable();
        expected.sort_unstable();
        assert_eq!(actual, expected);
        let mut changed_patterns: Vec<_> = padding.iter().map(String::as_str).collect();
        changed_patterns.extend(["bc", "cdef", "defg"]);
        let changed = compile(&changed_patterns, padding.len() + 1).unwrap();
        assert!(
            !changed
                .find_overlapping_iter(b"abcd")
                .any(|found| found.start() == 0)
        );
        assert_eq!(
            changed.find("bc").unwrap().pattern().as_usize(),
            padding.len()
        );
    }
}
