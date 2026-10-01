//! Common file projection for privacy-aware publication paths.
//!
//! The projection is the boundary shared by commit, export, share, and push integrations. It
//! decides scope first, preserves the original bytes only for the caller's encrypted payload,
//! then checks the rewritten public content for secrets before returning it.

use crate::domain::privacy::{CandidateAction, PrivacyPolicy, ReplacementRule};
use crate::domain::privacy_paths::{PathAliasStore, PathProjection};
use crate::domain::redact::Redactor;
use anyhow::{Context, Result};
use regex::Regex;
use std::path::Path;

/// A projected candidate ready for public serialization and private-layer sealing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileProjection {
    pub path: PathProjection,
    pub public_bytes: Option<Vec<u8>>,
    /// Original bytes for an encrypted private payload. This is never serialized by this module.
    pub private_bytes: Option<Vec<u8>>,
    pub replacements: usize,
    pub secret_matches: usize,
}

/// Project one file through the repository policy and content protection pipeline.
pub fn project_file(
    policy: &PrivacyPolicy,
    aliases: &mut PathAliasStore,
    path: &Path,
    branch: Option<&str>,
    bytes: &[u8],
) -> Result<FileProjection> {
    let path_projection = aliases.project_file(policy, path, branch)?;
    if path_projection.action != CandidateAction::Allowed {
        return Ok(FileProjection {
            path: path_projection,
            public_bytes: None,
            private_bytes: None,
            replacements: 0,
            secret_matches: 0,
        });
    }
    let redactor = Redactor::try_this_machine()?;
    project_allowed(path_projection, policy, path, bytes, &redactor)
}

#[cfg(test)]
fn project_file_with_redactor(
    policy: &PrivacyPolicy,
    aliases: &mut PathAliasStore,
    path: &Path,
    branch: Option<&str>,
    bytes: &[u8],
    redactor: &Redactor,
) -> Result<FileProjection> {
    let path_projection = aliases.project_file(policy, path, branch)?;
    anyhow::ensure!(
        path_projection.action != CandidateAction::Review,
        "privacy projection cannot publish a review-only candidate"
    );
    if path_projection.action != CandidateAction::Allowed {
        return Ok(FileProjection {
            path: path_projection,
            public_bytes: None,
            private_bytes: None,
            replacements: 0,
            secret_matches: 0,
        });
    }
    project_allowed(path_projection, policy, path, bytes, redactor)
}

fn project_allowed(
    path_projection: PathProjection,
    policy: &PrivacyPolicy,
    path: &Path,
    bytes: &[u8],
    redactor: &Redactor,
) -> Result<FileProjection> {
    let original = bytes.to_vec();
    let text = match std::str::from_utf8(bytes) {
        Ok(text) => text,
        Err(_) => {
            return Ok(FileProjection {
                path: PathProjection {
                    action: CandidateAction::Review,
                    logical_path: path_projection.logical_path,
                    reason: Some("file content is not UTF-8 and needs attachment review".into()),
                },
                public_bytes: None,
                private_bytes: None,
                replacements: 0,
                secret_matches: 0,
            });
        }
    };
    let logical_path = path_projection.logical_path.as_deref().unwrap_or_default();
    let aliased = rewrite_path(text, path, logical_path);
    let protected = redactor.try_scrub(&aliased)?;
    let protected_path = protected.text;
    let (rewritten, replacements) = apply_replacements(&protected_path, &policy.replacements)?;
    // A replacement may introduce a secret or a local identity even when its input is safe.
    let public = redactor.try_scrub(&rewritten)?;
    Ok(FileProjection {
        path: path_projection,
        public_bytes: Some(public.text.into_bytes()),
        private_bytes: Some(original),
        replacements,
        secret_matches: protected.secrets + public.secrets,
    })
}

fn rewrite_path(text: &str, path: &Path, logical_path: &str) -> String {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    let raw = absolute.to_string_lossy();
    let normalized = raw.replace('\\', "/");
    text.replace(raw.as_ref(), logical_path)
        .replace(&normalized, logical_path)
}

fn apply_replacements(text: &str, rules: &[ReplacementRule]) -> Result<(String, usize)> {
    let mut output = text.to_owned();
    let mut count = 0;
    for rule in rules {
        if rule.regex {
            let regex = Regex::new(&rule.pattern)
                .with_context(|| format!("invalid privacy replacement regex `{}`", rule.pattern))?;
            count += regex.find_iter(&output).count();
            output = regex
                .replace_all(&output, rule.replacement.as_str())
                .into_owned();
        } else {
            count += output.matches(&rule.pattern).count();
            output = output.replace(&rule.pattern, &rule.replacement);
        }
    }
    Ok((output, count))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::privacy::ReplacementRule;
    use std::fs;

    #[test]
    fn excluded_file_has_no_public_or_private_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("notes.txt");
        fs::write(&path, "private\n").unwrap();
        let policy = PrivacyPolicy {
            workspace: Some(temp.path().to_path_buf()),
            ..PrivacyPolicy::default()
        };
        let mut aliases = PathAliasStore::default();
        let output = project_file_with_redactor(
            &policy,
            &mut aliases,
            &path,
            None,
            b"private\n",
            &Redactor::new(Default::default()),
        )
        .unwrap();
        assert_eq!(output.path.action, CandidateAction::Excluded);
        assert!(output.public_bytes.is_none());
        assert!(output.private_bytes.is_none());
        assert_eq!(
            output.path.logical_path.as_deref(),
            Some("<private-file-1>")
        );
    }

    #[test]
    fn allowed_text_keeps_original_private_bytes_and_applies_rules() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("src/main.rs");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let original = format!("fn main() {{}} // {}\n", path.display());
        fs::write(&path, &original).unwrap();
        let policy = PrivacyPolicy {
            workspace: Some(temp.path().to_path_buf()),
            replacements: vec![ReplacementRule {
                pattern: "main".into(),
                replacement: "entry".into(),
                regex: false,
            }],
            ..PrivacyPolicy::default()
        };
        let mut aliases = PathAliasStore::default();
        let output = project_file_with_redactor(
            &policy,
            &mut aliases,
            &path,
            None,
            original.as_bytes(),
            &Redactor::new(Default::default()),
        )
        .unwrap();
        assert_eq!(output.path.action, CandidateAction::Allowed);
        assert_eq!(output.private_bytes.as_deref(), Some(original.as_bytes()));
        let public = String::from_utf8(output.public_bytes.unwrap()).unwrap();
        assert!(public.contains("<workspace>/src/entry.rs"));
        assert!(public.contains("entry"));
        assert!(!public.contains(temp.path().to_string_lossy().as_ref()));
        assert!(output.replacements > 0);
    }

    #[test]
    fn binary_content_is_review_only_and_never_returns_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let path = workspace.join("src/blob.bin");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let policy = PrivacyPolicy {
            workspace: Some(workspace),
            include: vec!["**/*".into()],
            ..PrivacyPolicy::default()
        };
        let mut aliases = PathAliasStore::default();
        let output = project_file_with_redactor(
            &policy,
            &mut aliases,
            &path,
            None,
            &[0, 159, 146, 150],
            &Redactor::new(Default::default()),
        )
        .unwrap();
        assert_eq!(
            output.path.action,
            CandidateAction::Review,
            "{:?}",
            output.path
        );
        assert!(output.public_bytes.is_none());
        assert!(output.private_bytes.is_none());
    }
}
