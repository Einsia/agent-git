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
    blocks: Option<AhoCorasick>,
    block_sources: Vec<Source>,
    allows: Option<AhoCorasick>,
    inline_allow: bool,
    entropy: detector::Entropy,
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
        Ok(Self {
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
            entropy: detector::Entropy::Exclude,
        })
    }

    /// Bare entropy is replaced only after the user opts in; see `detector::Entropy`.
    pub fn with_entropy(mut self, entropy: detector::Entropy) -> Self {
        self.entropy = entropy;
        self
    }

    /// Whether the user allowed `value` as a whole; user allows override default heuristics.
    #[cfg(feature = "secret-vault")]
    pub(crate) fn allows(&self, value: &str) -> bool {
        self.allows
            .as_ref()
            .is_some_and(|allows| allows.is_match(value))
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
            self.entropy,
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
        // An allowed value is not replaced in part either: a heuristic hit that overlaps an
        // occurrence of an allowed literal is the user's allowed value, not a new credential.
        let allowed: Vec<(usize, usize)> = self
            .allows
            .as_ref()
            .map(|allows| {
                allows
                    .find_overlapping_iter(text)
                    .map(|hit| (hit.start(), hit.end()))
                    .collect()
            })
            .unwrap_or_default();
        findings.extend(
            heuristics
                .findings
                .into_iter()
                .filter(|hit| {
                    !allowed
                        .iter()
                        .any(|(start, end)| hit.start < *end && *start < hit.end)
                })
                .map(|hit| Finding {
                    start: hit.start,
                    end: hit.end,
                    source: Source::Default,
                    rule: hit.rule,
                }),
        );
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

    /// Automatic replacement covers provider credentials and values assigned to credential
    /// names, including password punctuation, but leaves an opaque value whose only evidence is
    /// entropy or the generic key rule unless the user opts in. A policy that kept heuristics on
    /// by default would replace the trace identifier and the runtime's beta list; one that tied
    /// assignment detection to entropy would leave the password; one whose assignment rule
    /// ignored code would replace the configuration lookup, the type annotations, the document
    /// tokens and a search result's file name; one that read every camel-shaped value as code
    /// would leave the shell passwords. AgentGit's own credentials and a bearer value outside a
    /// curl command are replaced, while a word or placeholder written after "Bearer" is not: a
    /// policy relying on gitleaks alone would publish the token an agent printed from its
    /// configuration. Published artifacts keep their checksums, embedded data URIs, links and
    /// build commands byte for byte: a policy that replaced any of them would publish files that
    /// no longer open or verify.
    #[test]
    fn default_tier_replaces_credentials_and_reports_entropy_only_on_request() {
        let token = "ghp_R7kQ2mXv9LpZ4tNc8WjF3bHy6sVd1aGe5uKr";
        let password = "c7Zq4xR2mNv8LpT3!wYb6Ks1Hd9Fg5Jt0";
        let opaque = "c7Zq4xR2mNv8LpT3wYb6Ks1Hd9Fg5Jt0";
        let posthog = "phx_R7kQ2mXv9LpZ4tNc8WjF3bHy6sVd1aGe5uKr2Tq";
        let pat = "agit_pat_6f3c9a1e8b2d4f70a5c1e9b3d7f2a4c8e1b5d9f3a7c2e6b0d4f8a1c5e9b3d7f2";
        let device = "agit_rc_0b4d8f2a6c1e5b9d3f7a2c6e0b4d8f1a5c9e3b7d2f6a0c4e8b1d5f9a3c7e2b6d";
        let code = "agit_su_Qk3vX9mT2pL7wR4nZ8cY1bH6sD0fJ5gE9aK2uM7tV3x";
        let bearer = "eW91LmNhbm5vdC5ndWVzcy50aGlzLjIwMjYtMTAtMjY.Zk9q3vR";
        let digest = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let font = "AAEAAAASAQAABAAgR0RFRrRCsIIAAjWsAAACYkdQT1N/KyYBAAI4EAAAMkxHU1VCQyNA0QACaVwAAAJ4T1MvMmq7WyEAAADUAAAAYGNtYXA=";
        let link = "https://fonts.gstatic.com/s/inter/v13/UcCO3FwrK3iLTeHuS_fvQtMwCp50KnMw2boKoduKmMEVuLyfAZ9hiA.woff2";
        let deploy = "npx wrangler pages deploy dist --project-name cloudflare-brief --commit-hash 9f86d081884c7d659a2feaa0c55ad015a3bf4f1b";
        let text = format!(
            "export GH_TOKEN={token}; password = \"{password}\"; trace {opaque}; \
             secret_key = settings.SECRET_KEY_BASE; api_key = $OPENAI_API_KEY; \
             token_endpoint = \"https://accounts.google.com/o/oauth2/token\"; \
             db_password = \"$c7Zq4xR2mNv8LpT3!wYb6Ks1Hd9Fg5Jt0\"; \
             passwd = 'c7Zq4xR2;mNv8LpT3!wYb6Ks1Hd9Fg5Jt0'; \
             password = \"/c7Zq4xR2mNv8LpT3!wYb6Ks1Hd9Fg5Jt0\"; \
             credential = \"~c7Zq4xR2mNv8LpT3!wYb6Ks1Hd9Fg5Jt0\"; \
             secret_file = /run/secrets/db_password; \
             \"betas\":[\"oauth-2025-04-20\",\"interleaved-thinking-2025-05-14\"]; \
             fn save(token: &SnapshotStore, cred: Option<credentials::HubSession>) {{}} \
             let secret = AccessTokenStore; \
             {{\"file_token\": \"Bf7kQ2mXv9LpZ4tNc8Wj\", \"nextPageToken\": \"Cg7kQ2mXv9LpZ4tNc8Wj\"}}; \
             docs/secrets.md:42:Rotate2026Keys; \
             export DB_PASSWORD=Qc7xnvKb8lwpZj6tru \n \
             export DB_PASSWORD=QcxnvKblwpZjtruMvx; ./start \n \
             load() {{ local DB_PASSWORD=RdxnvKblwpZjtruMvx; }} \n \
             readonly DB_PASSWORD=SexnvKblwpZjtruMvx \n \
             {{DB_PASSWORD: TfxnvKblwpZjtruMvx}} \n \
             const token = AuthenticationProvider \n \
             let mut secret = AuthenticationProvider ; \n \
             class Session {{ static token = AuthorizationProvider; }} \n \
             PostHog Key: {posthog}; \
             http_headers = {{ \"X-Agit\" = \"1\" }} \n Bearer {pat} \n \
             AGIT_DEVICE_TOKEN={device}; sign-up link ?code={code}&next=/ \n \
             Authorization: Bearer {bearer} \n \
             Bearer AuthenticationMiddlewareConfiguration \n \
             Authorization: Bearer YOUR_PERSONAL_ACCESS_TOKEN_HERE \n \
             lastPrompt: DB_PASSWORD=\"Pf3P7wexdB!60zEEZ…\" \n \
             {digest}  artifacts/app.zip \n \
             src: url(data:font/ttf;base64,{font}) format(\"truetype\"); \n \
             <link href=\"{link}\" rel=\"stylesheet\"> \n \
             {deploy} \n"
        );
        let span = |batch: &Batch, value: &str| {
            let start = text.find(value).unwrap();
            batch
                .findings
                .iter()
                .any(|hit| hit.start <= start && start + value.len() <= hit.end)
        };
        let default = Snapshot::compile(&[], &[], false).unwrap().scan(&text, 64);
        assert!(span(&default, token));
        assert!(span(&default, password));
        assert!(!span(&default, opaque));
        assert!(!span(&default, "settings.SECRET_KEY_BASE"));
        assert!(!span(&default, "$OPENAI_API_KEY"));
        assert!(!span(
            &default,
            "https://accounts.google.com/o/oauth2/token"
        ));
        assert!(span(&default, "$c7Zq4xR2mNv8LpT3!wYb6Ks1Hd9Fg5Jt0"));
        assert!(span(&default, "c7Zq4xR2;mNv8LpT3!wYb6Ks1Hd9Fg5Jt0"));
        assert!(span(&default, "/c7Zq4xR2mNv8LpT3!wYb6Ks1Hd9Fg5Jt0"));
        assert!(span(&default, "~c7Zq4xR2mNv8LpT3!wYb6Ks1Hd9Fg5Jt0"));
        assert!(!span(&default, "/run/secrets/db_password"));
        for code in [
            "interleaved-thinking-2025-05-14",
            "&SnapshotStore",
            ":HubSession",
            "AccessTokenStore",
            "AuthenticationProvider",
            "AuthorizationProvider",
            "Bf7kQ2mXv9LpZ4tNc8Wj",
            "Cg7kQ2mXv9LpZ4tNc8Wj",
            "42:Rotate2026Keys",
        ] {
            assert!(!span(&default, code), "{code} is not a credential");
        }
        assert!(span(&default, posthog));
        for credential in [pat, device, code, bearer] {
            assert!(span(&default, credential), "{credential} is a credential");
        }
        for word in [
            "AuthenticationMiddlewareConfiguration",
            "YOUR_PERSONAL_ACCESS_TOKEN_HERE",
        ] {
            assert!(!span(&default, word), "{word} is not a credential");
        }
        assert!(span(&default, "Qc7xnvKb8lwpZj6tru"));
        for shell in [
            "QcxnvKblwpZjtruMvx",
            "RdxnvKblwpZjtruMvx",
            "SexnvKblwpZjtruMvx",
            "TfxnvKblwpZjtruMvx",
        ] {
            assert!(span(&default, shell), "{shell} is a credential");
        }
        assert!(span(&default, "Pf3P7wexdB!60zEEZ…"));
        for artifact in [digest, font, link, deploy] {
            let start = text.find(artifact).unwrap();
            assert!(
                !default
                    .findings
                    .iter()
                    .any(|hit| hit.start < start + artifact.len() && hit.end > start),
                "{artifact} is published unchanged"
            );
        }
        let opted_in = Snapshot::compile(&[], &[], false)
            .unwrap()
            .with_entropy(detector::Entropy::Include)
            .scan(&text, 32);
        assert!(span(&opted_in, token));
        assert!(span(&opted_in, opaque));
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
