//! Immutable policy snapshots combine defaults and user decisions without storage dependencies.

use super::detector;
use aho_corasick::{AhoCorasick, MatchKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Default,
    GlobalUser,
    RepositoryUser,
}

pub struct Literal<'a> {
    pub value: &'a str,
    pub source: Source,
}

pub struct Snapshot {
    #[cfg(feature = "secret-vault")]
    pub(crate) fingerprint: String,
    blocks: Option<AhoCorasick>,
    block_sources: Vec<Source>,
    allows: Option<AhoCorasick>,
    inline_allow: bool,
    pub max_literal_bytes: usize,
}

#[derive(Debug, Clone)]
pub struct Finding {
    pub start: usize,
    pub end: usize,
    pub source: Source,
    pub rule: &'static str,
}

pub struct Batch {
    pub findings: Vec<Finding>,
    pub complete: bool,
}

impl Snapshot {
    pub fn compile(
        blocks: &[Literal<'_>],
        allows: &[&str],
        inline_allow: bool,
    ) -> crate::Result<Self> {
        let blocks: Vec<_> = blocks
            .iter()
            .filter(|rule| !rule.value.is_empty())
            .collect();
        let allows: Vec<_> = allows
            .iter()
            .copied()
            .filter(|value| !value.is_empty())
            .collect();
        let block_sources = blocks.iter().map(|rule| rule.source).collect();
        #[cfg(feature = "secret-vault")]
        use sha2::{Digest, Sha256};
        #[cfg(feature = "secret-vault")]
        let fingerprint = hex::encode(Sha256::digest(serde_json::to_vec(&(
            blocks
                .iter()
                .map(|rule| (rule.value, rule.source as u8))
                .collect::<Vec<_>>(),
            &allows,
            inline_allow,
        ))?));
        Ok(Self {
            #[cfg(feature = "secret-vault")]
            fingerprint,
            max_literal_bytes: blocks
                .iter()
                .map(|rule| rule.value.len())
                .max()
                .unwrap_or(0),
            blocks: if blocks.is_empty() {
                None
            } else {
                Some(
                    AhoCorasick::builder()
                        .kind(Some(aho_corasick::AhoCorasickKind::NoncontiguousNFA))
                        .dense_depth(0)
                        .match_kind(MatchKind::LeftmostLongest)
                        .build(blocks.iter().map(|rule| rule.value))?,
                )
            },
            block_sources,
            allows: if allows.is_empty() {
                None
            } else {
                Some(AhoCorasick::new(allows)?)
            },
            inline_allow,
        })
    }

    #[cfg(feature = "secret-vault")]
    pub(crate) fn explicitly_blocks(&self, text: &str) -> bool {
        self.blocks
            .as_ref()
            .is_some_and(|blocks| blocks.is_match(text))
    }

    pub(crate) fn explicit_findings(&self, text: &str, limit: usize) -> Batch {
        let mut findings = Vec::new();
        let mut complete = limit > 0;
        if let Some(blocks) = &self.blocks {
            for hit in blocks.find_iter(text) {
                if findings.len() == limit {
                    complete = false;
                    break;
                }
                findings.push(Finding {
                    start: hit.start(),
                    end: hit.end(),
                    source: self.block_sources[hit.pattern().as_usize()],
                    rule: "explicit-block",
                });
            }
        }
        Batch { findings, complete }
    }

    pub fn scan(&self, text: &str, limit: usize) -> Batch {
        let Batch {
            mut findings,
            mut complete,
        } = self.explicit_findings(text, limit);
        let lines = (self.inline_allow && text.contains("agit:allow-secret"))
            .then(|| detector::Lines::new(text));
        let heuristics = detector::scan_filtered(
            text,
            limit.saturating_sub(findings.len()),
            |value, start| {
                !self
                    .allows
                    .as_ref()
                    .is_some_and(|allows| allows.is_match(value))
                    && !lines
                        .as_ref()
                        .is_some_and(|lines| lines.text_at(start).contains("agit:allow-secret"))
            },
        );
        complete &= heuristics.complete;
        findings.extend(heuristics.findings.into_iter().map(|hit| Finding {
            start: hit.start,
            end: hit.end,
            source: Source::Default,
            rule: hit.rule,
        }));
        findings.sort_by_key(|hit| (hit.start, std::cmp::Reverse(hit.end)));
        let mut merged: Vec<Finding> = Vec::with_capacity(findings.len());
        for finding in findings {
            if let Some(previous) = merged
                .last_mut()
                .filter(|previous| finding.start < previous.end)
            {
                previous.end = previous.end.max(finding.end);
                if finding.source != Source::Default {
                    previous.source = finding.source;
                    previous.rule = finding.rule;
                }
            } else {
                merged.push(finding);
            }
        }
        Batch {
            findings: merged,
            complete,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_blocks_override_both_user_allows_and_inline_exceptions() {
        let secret = "AKIA2E7YQXK4NMZ5VJ3T";
        let text = format!("{secret} low-entropy agit:allow-secret");
        let policy = Snapshot::compile(
            &[
                Literal {
                    value: secret,
                    source: Source::GlobalUser,
                },
                Literal {
                    value: "low-entropy",
                    source: Source::RepositoryUser,
                },
            ],
            &[secret, "low-entropy"],
            true,
        )
        .unwrap();
        let batch = policy.scan(&text, 32);
        assert!(batch.complete);
        assert_eq!(batch.findings.len(), 2);
        assert_eq!(batch.findings[0].source, Source::GlobalUser);
        assert_eq!(batch.findings[1].source, Source::RepositoryUser);
        let allowed = Snapshot::compile(&[], &["2E7YQXK4"], false).unwrap();
        assert!(allowed.scan(secret, 32).findings.is_empty());
    }

    #[test]
    fn overlapping_blocks_and_heuristics_form_one_reversible_range() {
        let secret = "AKIA2E7YQXK4NMZ5VJ3T";
        let policy = Snapshot::compile(
            &[Literal {
                value: "2E7YQX",
                source: Source::RepositoryUser,
            }],
            &[],
            false,
        )
        .unwrap();
        let batch = policy.scan(secret, 32);
        assert!(batch.complete);
        assert_eq!(batch.findings.len(), 1);
        assert_eq!(
            &secret[batch.findings[0].start..batch.findings[0].end],
            secret
        );
        assert_eq!(batch.findings[0].source, Source::RepositoryUser);
    }
}
